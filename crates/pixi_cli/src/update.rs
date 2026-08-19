use std::{cmp::Ordering, collections::HashSet};

use clap::Parser;
use fancy_display::FancyDisplay;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic, MietteDiagnostic};
use pixi_config::ConfigCli;
use pixi_consts::consts;
use pixi_core::WorkspaceLocator;
use pixi_core::{
    Workspace,
    environment::InstallFilter,
    lock_file::{
        LockFileDerivedData, LockedPackageKind, ReinstallPackages, UpdateContext, UpdateMode,
        filter_lock_file,
    },
    workspace::{ScriptResolutionStateGuard, script_resolutions_equal},
};
use pixi_diff::{LockFileDiff, LockFileJsonDiff};
use pixi_manifest::{EnvironmentName, PixiPlatformName};
use rattler_lock::LockFile;

use crate::cli_config::ScriptWorkspaceConfig;

/// The `update` command checks if there are newer versions of the dependencies and updates the `pixi.lock` file and environments accordingly.
///
/// It will only update the lock file if the dependencies in the manifest file are still compatible with the new versions.
#[derive(Parser, Debug, Default)]
pub struct Args {
    #[clap(flatten)]
    pub config_source: pixi_config::ConfigSourceCli,

    #[clap(flatten)]
    pub config: ConfigCli,

    #[clap(flatten)]
    pub project_config: ScriptWorkspaceConfig,

    /// Don't install the (solve) environments needed for pypi-dependencies
    /// solving.
    #[arg(long, env = "PIXI_NO_INSTALL")]
    pub no_install: bool,

    /// Don't actually write the lock file or update any environment.
    #[clap(short = 'n', long)]
    pub dry_run: bool,

    #[clap(flatten)]
    pub specs: UpdateSpecsArgs,

    /// Output the changes in JSON format.
    #[clap(long)]
    pub json: bool,
}

#[derive(Parser, Debug, Default)]
pub struct UpdateSpecsArgs {
    /// The packages to update, space separated.
    /// If no packages are provided, all packages will be updated.
    pub packages: Option<Vec<String>>,

    /// The environments to update. If none is specified, all environments are
    /// updated.
    #[clap(long = "environment", short = 'e', value_name = "ENVIRONMENT")]
    pub environments: Option<Vec<EnvironmentName>>,

    /// The platforms to update. If none is specified, all platforms are
    /// updated. Accepts a workspace platform name; a bare conda subdir
    /// (e.g. `linux-64`) is also accepted so users don't have to declare
    /// a platform before targeting it.
    #[clap(long = "platform", short = 'p', value_name = "PLATFORM")]
    pub platforms: Option<Vec<PixiPlatformName>>,
}

/// A distilled version of `UpdateSpecsArgs`.
/// TODO: In the future if we want to add `--recursive` this data structure could
///     be used to store information about recursive packages.
struct UpdateSpecs {
    packages: Option<HashSet<String>>,
    environments: Option<HashSet<EnvironmentName>>,
    platforms: Option<HashSet<PixiPlatformName>>,
}

impl From<UpdateSpecsArgs> for UpdateSpecs {
    fn from(args: UpdateSpecsArgs) -> Self {
        Self {
            packages: args.packages.map(|args| args.into_iter().collect()),
            environments: args.environments.map(|args| args.into_iter().collect()),
            platforms: args.platforms.map(|args| args.into_iter().collect()),
        }
    }
}

impl UpdateSpecs {
    /// Returns true if the update is scoped to part of the lock file.
    fn is_selective(&self) -> bool {
        self.packages.is_some() || self.platforms.is_some()
    }

    /// Returns true if the package should be relaxed according to the user
    /// input.
    fn should_relax(
        &self,
        environment_name: &EnvironmentName,
        platform: &PixiPlatformName,
        package_name: &str,
    ) -> bool {
        // Check if the platform is in the list of platforms to update.
        if let Some(platforms) = &self.platforms
            && !platforms.contains(platform)
        {
            return false;
        }

        // Check if the environment is in the list of environments to update.
        if let Some(environments) = &self.environments
            && !environments.contains(environment_name)
        {
            return false;
        }

        // Check if the package is in the list of packages to update.
        if let Some(packages) = &self.packages
            && !packages.contains(package_name)
        {
            return false;
        }

        tracing::debug!(
            "relaxing package: {}, env={}, platform={}",
            package_name,
            environment_name.fancy_display(),
            consts::PLATFORM_STYLE.apply_to(platform),
        );

        true
    }
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let workspace = WorkspaceLocator::for_cli()
        .with_global_config_source(args.config_source.source())
        .with_search_start(args.project_config.workspace_locator_start())
        .locate()?
        .with_cli_config(args.config);

    // Like uv, script resolution uses a disposable environment to inspect
    // dynamic PyPI metadata. A failed or losing optimistic solve must never
    // mutate the script's normal cached prefix before its resolution commits.
    let script_solve_workspace = if workspace.is_script() {
        let temp_dir = tempfile::tempdir()
            .into_diagnostic()
            .context("failed to create a temporary script solve environment")?;
        let solve_workspace = workspace
            .clone()
            .with_script_pixi_dir(temp_dir.path().join("environment"));
        Some((temp_dir, solve_workspace))
    } else {
        None
    };

    let specs = UpdateSpecs::from(args.specs);

    if workspace.is_script() && specs.environments.is_some() {
        miette::bail!(
            help = "A PEP 723 script has one implicit default environment.",
            "`pixi update --script` does not support --environment"
        );
    }

    // If the user specified an environment name, check to see if it exists.
    if let Some(env) = &specs.environments {
        for env in env {
            if workspace.environment(env).is_none() {
                miette::bail!(
                    "could not find an environment named {}",
                    env.fancy_display()
                )
            }
        }
    }

    let (loaded_lock_file, updated_lock_file) = if workspace.is_script() {
        let solve_workspace = script_solve_workspace
            .as_ref()
            .map(|(_, workspace)| workspace)
            .expect("script workspaces always have a disposable solve workspace");
        update_script(
            &workspace,
            solve_workspace,
            &specs,
            args.no_install,
            args.dry_run,
        )
        .await?
    } else {
        let loaded_lock_file = workspace
            .load_lock_file()
            .await?
            .into_lock_file_or_empty_with_warning();
        let updated_lock_file =
            resolve_update(&workspace, &loaded_lock_file, &specs, args.no_install).await?;
        if !args.dry_run {
            updated_lock_file.write_to_disk()?;
        }
        (loaded_lock_file, updated_lock_file)
    };

    let lock_file = updated_lock_file.into_lock_file();

    // Determine the diff between the old and new lock file.
    let diff = LockFileDiff::from_lock_files(&loaded_lock_file, &lock_file);

    // Format as json?
    if args.json {
        let diff = LockFileDiff::from_lock_files(&loaded_lock_file, &lock_file);
        let json_diff = LockFileJsonDiff::new(Some(workspace.named_environments()), diff);
        let json = serde_json::to_string_pretty(&json_diff).expect("failed to convert to json");
        println!("{json}");
    } else if diff.is_empty() {
        eprintln!(
            "{}Lock-file was already up-to-date",
            console::style(console::Emoji("✔ ", "")).green()
        );
    } else {
        diff.print()
            .into_diagnostic()
            .context("failed to print lock file diff")?;
    }

    Ok(())
}

const MAX_SCRIPT_UPDATE_ATTEMPTS: usize = 3;

/// Update a script without holding the external state lock across resolution
/// or installation. Both phases can run activation scripts, which may invoke
/// pixi recursively for the same script.
async fn update_script<'p>(
    workspace: &'p Workspace,
    solve_workspace: &Workspace,
    specs: &UpdateSpecs,
    no_install: bool,
    dry_run: bool,
) -> miette::Result<(LockFile, LockFileDerivedData<'p>)> {
    for attempt in 1..=MAX_SCRIPT_UPDATE_ATTEMPTS {
        let has_sidecar = has_script_lock_file(workspace);
        let initial_guard = acquire_script_resolution_state(
            workspace,
            !has_sidecar && (!dry_run || specs.is_selective()),
        )
        .await?;
        let baseline =
            load_script_resolution(workspace, has_sidecar, initial_guard.as_ref()).await?;
        workspace.ensure_script_metadata_unchanged().await?;
        let sidecar_state = if has_sidecar {
            Some(workspace.script_lock_file_state().await?)
        } else {
            None
        };
        drop(initial_guard);

        if !has_sidecar && baseline.is_none() && specs.is_selective() {
            miette::bail!(
                help = "Run an unfiltered `pixi update --script <path>` first to cache a complete resolution.",
                "cannot selectively update a script without a prior cached resolution"
            );
        }

        let loaded_lock_file = baseline.clone().unwrap_or_default();
        let solved_lock_file =
            resolve_update(solve_workspace, &loaded_lock_file, specs, no_install).await?;
        let updated_lock_file = rebind_script_lock_file(workspace, solved_lock_file)?;

        if dry_run {
            workspace.ensure_script_metadata_unchanged().await?;
            return Ok((loaded_lock_file, updated_lock_file));
        }

        // Reacquire the lock and publish only if the authority and baseline
        // stayed unchanged while resolution ran. Publication always requires
        // coordination: atomic replacement prevents partial sidecar writes,
        // but cannot by itself prevent concurrent writers from losing updates.
        let publish_guard = acquire_script_resolution_state(workspace, true).await?;
        workspace.ensure_script_metadata_unchanged().await?;
        let current_has_sidecar = has_script_lock_file(workspace);
        let current =
            load_script_resolution(workspace, current_has_sidecar, publish_guard.as_ref()).await?;
        let sidecar_is_current = match sidecar_state.as_ref() {
            Some(expected) if current_has_sidecar => {
                workspace
                    .script_lock_file_state_is_current(expected)
                    .await?
            }
            Some(_) => false,
            None => !current_has_sidecar,
        };
        if !sidecar_is_current || !script_resolutions_equal(baseline.as_ref(), current.as_ref())? {
            drop(publish_guard);
            if attempt == MAX_SCRIPT_UPDATE_ATTEMPTS {
                miette::bail!(
                    "the script resolution changed repeatedly while updating; please retry"
                );
            }
            tracing::debug!(attempt, "script resolution changed; retrying update");
            continue;
        }

        if has_sidecar {
            updated_lock_file.write_to_disk()?;
            if !no_install
                && let Some(guard) = publish_guard.as_ref()
                && let Err(error) = guard.store(updated_lock_file.as_lock_file()).await
            {
                tracing::warn!(
                    %error,
                    "failed to shadow the script resolution before synchronization"
                );
            }
        } else {
            publish_guard
                .as_ref()
                .expect("lockless script publication requires a state guard")
                .store(updated_lock_file.as_lock_file())
                .await?;
        }
        drop(publish_guard);

        if no_install {
            return Ok((loaded_lock_file, updated_lock_file));
        }

        // Prefix work can execute user activation scripts, so it must remain
        // outside the resolution-state guard. Once synchronization finishes,
        // verify that this exact candidate is still authoritative. If another
        // updater published meanwhile, resolve again from its newer baseline
        // and converge the prefix before reporting success.
        updated_lock_file
            .prefix(
                &workspace.default_environment(),
                UpdateMode::Revalidate,
                &ReinstallPackages::default(),
                &InstallFilter::default(),
            )
            .await?;

        let reconcile_guard = acquire_script_resolution_state(workspace, !has_sidecar).await?;
        workspace.ensure_script_metadata_unchanged().await?;
        let current_has_sidecar = has_script_lock_file(workspace);
        let current =
            load_script_resolution(workspace, current_has_sidecar, reconcile_guard.as_ref())
                .await?;
        let candidate_is_current = current_has_sidecar == has_sidecar
            && script_resolutions_equal(Some(updated_lock_file.as_lock_file()), current.as_ref())?;

        let sidecar_became_hidden = has_sidecar
            && !current_has_sidecar
            && script_resolutions_equal(Some(updated_lock_file.as_lock_file()), current.as_ref())?;

        if candidate_is_current || sidecar_became_hidden {
            // A sidecar is authoritative while it exists, but the hidden state
            // should still describe the prefix we just installed. If the user
            // later removes the sidecar, lockless execution then starts from
            // the installed solution instead of resurrecting stale cache data.
            if has_sidecar
                && let Some(guard) = reconcile_guard.as_ref()
                && let Err(error) = guard.store(updated_lock_file.as_lock_file()).await
            {
                tracing::warn!(
                    %error,
                    "failed to refresh the cached script resolution; the sidecar lock remains authoritative"
                );
            }
            drop(reconcile_guard);
            return Ok((loaded_lock_file, updated_lock_file));
        }

        drop(reconcile_guard);
        if attempt == MAX_SCRIPT_UPDATE_ATTEMPTS {
            workspace.reconcile_script_prefix_to_authority().await?;
            miette::bail!("the script resolution changed repeatedly while updating; please retry");
        }
        tracing::debug!(
            attempt,
            "script resolution changed during installation; retrying update"
        );
    }

    unreachable!("the bounded update loop always returns or errors")
}

async fn resolve_update<'p>(
    workspace: &'p Workspace,
    loaded_lock_file: &LockFile,
    specs: &UpdateSpecs,
    no_install: bool,
) -> miette::Result<LockFileDerivedData<'p>> {
    // If the user specified a package name, check to see if it is even locked.
    if let Some(packages) = &specs.packages {
        for package in packages {
            ensure_package_exists(loaded_lock_file, package, specs)?
        }
    }

    // Unlock dependencies in the lock file that we want to update.
    let relaxed_lock_file = unlock_packages(workspace, loaded_lock_file, specs);

    // Update the packages in the lock file.
    let progress = pixi_reporters::TopLevelProgress::from_global();
    let dispatcher = progress
        .clone()
        .register_with(workspace.command_dispatcher_builder()?)
        .finish();
    UpdateContext::builder(workspace, dispatcher)?
        .with_lock_file(relaxed_lock_file)
        .with_no_install(no_install)
        .with_update_targets(specs.packages.clone())
        .finish()
        .await?
        .update()
        .await
}

fn rebind_script_lock_file<'p>(
    workspace: &'p Workspace,
    lock_file: LockFileDerivedData<'_>,
) -> miette::Result<LockFileDerivedData<'p>> {
    let progress = pixi_reporters::TopLevelProgress::from_global();
    let dispatcher = progress
        .register_with(workspace.command_dispatcher_builder()?)
        .finish();
    Ok(lock_file.rebind_workspace(workspace, dispatcher))
}

async fn acquire_script_resolution_state(
    workspace: &Workspace,
    required: bool,
) -> miette::Result<Option<ScriptResolutionStateGuard>> {
    match workspace.acquire_script_resolution_state().await {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if required => Err(miette::Report::new(error)),
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to coordinate the cached script environment; continuing without cached resolution state"
            );
            Ok(None)
        }
    }
}

fn has_script_lock_file(workspace: &Workspace) -> bool {
    workspace
        .persistent_lock_file_path()
        .is_some_and(|path| path.is_file())
}

async fn load_script_resolution(
    workspace: &Workspace,
    has_sidecar: bool,
    guard: Option<&ScriptResolutionStateGuard>,
) -> miette::Result<Option<LockFile>> {
    if has_sidecar {
        Ok(Some(
            workspace
                .load_lock_file()
                .await?
                .into_lock_file_or_empty_with_warning(),
        ))
    } else {
        Ok(match guard {
            Some(guard) => guard.load(workspace).await,
            None => workspace.load_script_resolution_state().await,
        })
    }
}

/// Ensures the existence of the specified package
///
/// # Returns
///
/// Returns `miette::Result` with a descriptive error message
/// if the package does not exist.
fn ensure_package_exists(
    lock_file: &LockFile,
    package_name: &str,
    specs: &UpdateSpecs,
) -> miette::Result<()> {
    let environments = lock_file
        .environments()
        .filter_map(|(name, env)| {
            if let Some(envs) = &specs.environments
                && !envs.contains(name)
            {
                return None;
            }
            Some(env)
        })
        .collect_vec();

    let similar_names = environments
        .iter()
        .flat_map(|env| env.packages_by_platform())
        .filter_map(|(lock_p, packages)| {
            let name = PixiPlatformName::try_from(lock_p.name().as_str()).ok()?;
            if let Some(platforms) = &specs.platforms
                && !platforms.contains(&name)
            {
                return None;
            }
            Some(packages)
        })
        .flatten()
        .map(|p| p.name().to_string())
        .unique()
        .filter_map(|name| {
            let distance = strsim::jaro(package_name, &name);
            if distance > 0.6 {
                Some((name, distance))
            } else {
                None
            }
        })
        .sorted_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal))
        .take(5)
        .map(|(name, _)| name)
        .collect_vec();

    if similar_names.first().map(String::as_str) == Some(package_name) {
        return Ok(());
    }

    let message = format!("could not find a package named '{package_name}'");

    Err(MietteDiagnostic {
        message,
        code: None,
        severity: None,
        help: if !similar_names.is_empty() {
            Some(format!(
                "did you mean '{}'?",
                similar_names.iter().format("', '")
            ))
        } else {
            None
        },
        url: None,
        labels: None,
    }
    .into())
}

/// Constructs a new lock file where some of the constraints have been removed.
///
/// The same predicate runs against top-level entries and against the
/// transitive `build_packages` / `host_packages` of every kept source record,
/// so stale copies of an update target inside a source record's host or build
/// closure are stripped together with the top-level entry. Without that
/// strip, `pixi update <pkg>` would update only the top-level entry and leave
/// source packages building against the old version.
fn unlock_packages(project: &Workspace, lock_file: &LockFile, specs: &UpdateSpecs) -> LockFile {
    filter_lock_file(project, lock_file, |env, platform, package| {
        let name = match package {
            LockedPackageKind::Conda(name) => name.as_normalized(),
            LockedPackageKind::Pypi(name) => name.as_ref(),
        };
        !specs.should_relax(env.name(), platform, name)
    })
}
