use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    convert::identity,
    ffi::OsString,
    io::Read,
    string::String,
    sync::Arc,
};

#[cfg(unix)]
use std::io::IsTerminal;

use clap::Parser;
use deno_task_shell::KillSignal;
use dialoguer::theme::ColorfulTheme;
use fancy_display::FancyDisplay;
use indicatif::ProgressDrawTarget;
use itertools::Itertools;
use miette::{Context, Diagnostic, IntoDiagnostic};
use pixi_config::{ConfigCli, ConfigCliActivation};
use pixi_core::{
    Workspace, WorkspaceLocator,
    environment::{InstallFilter, LockFileUsage, sanity_check_workspace},
    lock_file::{
        LockFileDerivedData, LockFileInput, ReinstallPackages, SatisfiabilityMode,
        UpdateLockFileOptions, UpdateMode,
    },
    workspace::{
        Environment, ScriptResolutionStateGuard,
        errors::UnsupportedPlatformError,
        script_resolutions_equal,
        virtual_packages::{
            EnvironmentRunnability, classify_environment_runnability,
            verify_current_platform_can_run_environment,
        },
    },
};
use pixi_manifest::{HasWorkspaceManifest, PixiPlatformName, TaskName, WithWarnings};
use pixi_progress::global_multi_progress;
use pixi_task::{
    AmbiguousTask, CanSkip, ExecutableTask, FailedToParseShellScript, InvalidWorkingDirectory,
    PreferExecutable, SearchEnvironments, TaskAndEnvironment, TaskGraph, get_task_env,
};
use rattler_conda_types::Platform;
use rattler_lock::LockFile;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::Level;

use crate::cli_config::{
    LockAndInstallConfig, ScriptWorkspaceConfig, script_lock_file_usage,
    transient_script_lock_file_usage,
};
use crate::process_exit;
use crate::run_script::{
    RunScriptInput, STDIN_SCRIPT_COMMAND, StdinScriptCommand, prepare_remote_script,
    prepare_stdin_script, transient_script_cache_key,
};
use crate::shared::install_platform::resolve_install_platform;

/// Runs task in the pixi environment.
///
/// This command is used to run tasks in the pixi environment.
/// It will activate the environment and run the task in the environment.
/// It is using the deno_task_shell to run the task.
///
/// `pixi run` will also update the lock file and install the environment if it
/// is required.
#[derive(Parser, Debug, Default)]
#[clap(trailing_var_arg = true, disable_help_flag = true)]
pub struct Args {
    #[clap(flatten)]
    pub config_source: pixi_config::ConfigSourceCli,

    /// The pixi task or a task shell command you want to run in the workspace's
    /// environment, which can be an executable in the environment's PATH.
    pub task: Vec<String>,

    /// Execute the command as an executable without resolving Pixi tasks.
    ///
    /// Useful when a task name and an executable have the same name.
    #[arg(long = "executable", short = 'x')]
    pub executable: bool,

    #[clap(flatten)]
    pub workspace_config: ScriptWorkspaceConfig,

    #[clap(flatten)]
    pub lock_and_install_config: LockAndInstallConfig,

    #[clap(flatten)]
    pub config: ConfigCli,

    #[clap(flatten)]
    pub activation_config: ConfigCliActivation,

    /// The environment to run the task in.
    #[arg(long, short)]
    pub environment: Option<String>,

    /// Install and run in the environment for the given platform; a warning is
    /// printed when it doesn't run on this machine. Accepts a workspace
    /// platform name; a bare conda subdir (e.g. `linux-64`) is also accepted.
    #[arg(long, short)]
    pub platform: Option<PixiPlatformName>,

    /// Use a clean environment to run the task
    ///
    /// Using this flag will ignore your current shell environment and use bare
    /// minimum environment to activate the pixi environment in.
    #[arg(long)]
    pub clean_env: bool,

    /// Don't run the dependencies of the task ('depends-on' field in the task
    /// definition)
    #[arg(long)]
    pub skip_deps: bool,

    /// Enable template rendering for the command arguments.
    ///
    /// By default, arguments passed to `pixi run` on the command line are not
    /// processed by the template engine. Use this flag to enable rendering
    /// of template variables like `{{ pixi.platform }}`.
    #[arg(long)]
    pub templated: bool,

    /// Run the task in dry-run mode (only print the command that would run)
    #[clap(short = 'n', long)]
    pub dry_run: bool,

    #[clap(long, action = clap::ArgAction::HelpLong)]
    pub help: Option<bool>,

    #[clap(short, action = clap::ArgAction::HelpShort)]
    pub h: Option<bool>,
}

impl Args {
    fn validate_script_options(&self) -> miette::Result<()> {
        if self.workspace_config.script.is_none() {
            return Ok(());
        }

        let mut unsupported = Vec::new();
        if self.environment.is_some() {
            unsupported.push("--environment");
        }
        if self.skip_deps {
            unsupported.push("--skip-deps");
        }

        if unsupported.is_empty() {
            Ok(())
        } else {
            Err(miette::miette!(
                help = "A PEP 723 script has one implicit default run environment and no Pixi task graph.",
                "`pixi run --script` does not support {}",
                unsupported.join(", ")
            ))
        }
    }
}

/// CLI entry point for `pixi run`
/// When running the sigints are ignored and child can react to them. As it
/// pleases.
pub async fn execute(mut args: Args) -> miette::Result<()> {
    args.validate_script_options()?;

    // Following statements don't spawn any progress bar, so set
    // progress draw target to hidden. Otherwise output may be
    // incorrect.
    let not_hidden = !global_multi_progress().is_hidden();
    global_multi_progress().set_draw_target(ProgressDrawTarget::hidden());

    let cli_config = args
        .activation_config
        .merge_config(args.config.clone().into());

    let is_script = args.workspace_config.script.is_some();
    let script_input = args
        .workspace_config
        .script
        .as_deref()
        .map(RunScriptInput::classify);
    let requested_lock_file_usage = args.lock_and_install_config.lock_file_usage()?;
    let global_config_source = args.config_source.source();
    let mut transient_lock_file_usage = None;
    let mut _remote_script_file = None;
    let mut stdin_script_command = None;
    let workspace = match script_input {
        Some(RunScriptInput::Remote(url)) => {
            transient_lock_file_usage =
                Some(transient_script_lock_file_usage(requested_lock_file_usage)?);
            let root = std::env::current_dir().into_diagnostic()?;
            let config = pixi_config::Config::load_with(&root, &global_config_source)
                .merge_config(cli_config);
            let prepared = prepare_remote_script(url, &config, &root).await?;
            let cache_key = transient_script_cache_key(
                b"remote",
                prepared.original_url.as_str().as_bytes(),
                &root,
            );
            let WithWarnings {
                value: workspace,
                warnings,
            } = Workspace::from_transient_script(
                prepared.manifest,
                config,
                root,
                prepared.file.path().to_owned(),
                &prepared.cache_name,
                &cache_key,
            )?;
            for warning in warnings {
                tracing::warn!("{warning}");
            }
            _remote_script_file = Some(prepared.file);
            workspace
        }
        Some(RunScriptInput::Stdin) => {
            transient_lock_file_usage =
                Some(transient_script_lock_file_usage(requested_lock_file_usage)?);
            let root = std::env::current_dir().into_diagnostic()?;
            let config = pixi_config::Config::load_with(&root, &global_config_source)
                .merge_config(cli_config);
            let mut contents = Vec::new();
            std::io::stdin()
                .read_to_end(&mut contents)
                .into_diagnostic()?;
            let prepared = prepare_stdin_script(contents, &root)?;
            let cache_key = transient_script_cache_key(
                b"stdin",
                prepared.manifest.metadata().as_bytes(),
                &root,
            );
            let WithWarnings {
                value: workspace,
                warnings,
            } = Workspace::from_transient_script(
                prepared.manifest,
                config,
                root,
                "<stdin>".into(),
                "stdin",
                &cache_key,
            )?;
            for warning in warnings {
                tracing::warn!("{warning}");
            }
            stdin_script_command = Some(prepared.command);
            workspace
        }
        Some(RunScriptInput::Local(path)) => WorkspaceLocator::for_cli()
            .with_global_config_source(global_config_source)
            .with_search_start(pixi_core::workspace::DiscoveryStart::Script(path))
            .with_cli_config(cli_config)
            .locate()?,
        None => WorkspaceLocator::for_cli()
            .with_global_config_source(global_config_source)
            .with_search_start(args.workspace_config.workspace_locator_start())
            .with_cli_config(cli_config)
            .locate()?,
    };

    // Resolve script candidates in a disposable prefix. Dynamic PyPI metadata
    // may execute build backends or activation while solving, and a candidate
    // must not mutate the real cached prefix before it wins publication.
    let script_solve_workspace = if is_script {
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

    let stdin_display_args = if stdin_script_command.is_some() {
        Some(args.task.clone())
    } else {
        None
    };
    if stdin_script_command.is_some() {
        args.task.insert(0, STDIN_SCRIPT_COMMAND.to_owned());
        args.executable = true;
    } else if is_script {
        let script_path = workspace.workspace.provenance.path.clone();
        let script_path = script_path.into_os_string().into_string().map_err(|_| {
            miette::miette!("the script path must contain only valid UTF-8 characters")
        })?;

        args.task.insert(0, script_path);
        args.task.insert(0, "python".to_owned());
        args.executable = true;
    }

    // Extract the passed in environment name.
    let environment = if is_script {
        workspace.default_environment()
    } else {
        workspace.environment_from_name_or_env_var(args.environment.clone())?
    };

    // Find the environment to run the task in, if any were specified.
    let explicit_environment =
        if is_script || (args.environment.is_none() && environment.is_default()) {
            None
        } else {
            Some(environment.clone())
        };

    // Print all available tasks if no task is provided
    if args.task.is_empty() {
        command_not_found(&workspace, explicit_environment);
        return Ok(());
    }

    // We expect progress bar to be used afterwards, so set draw
    // target to the original one.
    if not_hidden {
        global_multi_progress().set_draw_target(ProgressDrawTarget::stderr_with_hz(20));
    }
    // Sanity check of prefix location
    sanity_check_workspace(&workspace).await?;

    // `--platform` pins which declared platform the environment is installed
    // and activated for. Without it we auto-upgrade to the platform the
    // environment was last installed for (so users need not repeat
    // `--platform`), falling back to the host-aware best match when the
    // environment isn't installed yet.
    let user_platform = resolve_install_platform(&workspace, args.platform.as_ref())?;
    let run_platform = user_platform
        .clone()
        .or_else(|| environment.installed_resolved_platform_name());
    let best_declared_platform = environment.named_or_best_declared_platform(run_platform.as_ref());

    // A `--platform` the environment doesn't list is a membership error. With
    // no platform requested, defer to the install path's minimum fallback.
    if args.lock_and_install_config.allow_installs()
        && best_declared_platform.is_none()
        && let Some(name) = user_platform.as_ref()
    {
        return Err(miette::miette!(
            "platform '{}' is not part of environment '{}'",
            name,
            environment.name(),
        ));
    }

    if args.lock_and_install_config.allow_installs() {
        environment.emit_emulation_warning();
    }

    // Top-level progress, kept here so we can clear it between phases.
    let progress = pixi_reporters::TopLevelProgress::from_global();

    // Ensure that the lock file is up-to-date.
    let has_script_lock_file = is_script
        && workspace
            .persistent_lock_file_path()
            .is_some_and(|path| path.is_file());
    let lock_file_usage = match transient_lock_file_usage {
        Some(lock_file_usage) => lock_file_usage,
        None => script_lock_file_usage(requested_lock_file_usage, is_script, has_script_lock_file)?,
    };
    let mut lock_file = if is_script {
        prepare_script_environment(
            &workspace,
            script_solve_workspace
                .as_ref()
                .map(|(_, workspace)| workspace)
                .expect("script workspaces always have a disposable solve workspace"),
            ScriptRunOptions {
                progress: progress.clone(),
                requested_lock_file_usage,
                transient: transient_lock_file_usage.is_some(),
                no_install: args.lock_and_install_config.no_install(),
                dry_run: args.dry_run,
                user_platform: user_platform.as_ref(),
            },
        )
        .await?
    } else {
        workspace
            .update_lock_file(
                Some(progress.clone()),
                UpdateLockFileOptions {
                    lock_file_usage,
                    no_install: args.lock_and_install_config.no_install(),
                    max_concurrent_solves: workspace.config().max_concurrent_solves(),
                    ..Default::default()
                },
            )
            .await?
            .0
    };

    // Only an explicit `--platform` pins the global target; the implicit
    // auto-upgrade is resolved per-environment in the loop below, since a
    // global pin broke sibling environments with a different platform.
    lock_file.target_platform = user_platform.clone();

    // Spawn a task that listens for ctrl+c and resets the cursor.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            reset_cursor();
        }
    });

    // Construct a task graph from the input arguments.
    // Pin the search only to an explicit `--platform`; otherwise each
    // environment resolves its own platform, so foreign-platform tasks are found.
    let search_platform = if args.lock_and_install_config.allow_installs() {
        user_platform.as_ref().and_then(|name| {
            (&workspace)
                .workspace_manifest()
                .workspace
                .platform_by_name(name)
        })
    } else {
        None
    };
    let search_environment =
        SearchEnvironments::from_opt_env(&workspace, explicit_environment.clone(), search_platform)
            .with_disambiguate_fn(disambiguate_task_interactive);

    let task_graph = TaskGraph::from_cmd_args(
        &workspace,
        &search_environment,
        args.task,
        args.skip_deps,
        if args.executable {
            PreferExecutable::Always
        } else {
            PreferExecutable::TaskFirst
        },
        args.templated,
    )?;
    tracing::debug!("Task graph: {}", task_graph);

    // Print dry-run message if dry-run mode is enabled
    if args.dry_run {
        pixi_progress::println!(
            "{}{}\n\n",
            console::Emoji("🌵 ", ""),
            console::style("Dry-run mode enabled - no tasks will be executed.")
                .yellow()
                .bold()
        );
    }

    // Traverse the task graph in topological order and execute each individual
    // task.
    let mut task_idx = 0;
    let mut task_envs = HashMap::new();
    let signal = KillSignal::default();
    // make sure that child processes are killed when pixi stops
    let _drop_guard = signal.clone().drop_guard();

    let init_cwd = std::env::current_dir().ok();
    for task_id in task_graph.topological_order() {
        let executable_task =
            ExecutableTask::from_task_graph(&task_graph, task_id, init_cwd.clone());

        // If the task is not executable (e.g. an alias), we skip it. This ensures we
        // don't instantiate a prefix for an alias.
        if !executable_task.task().is_executable() {
            continue;
        }

        // Classify how this machine runs the task's environment. A `--platform`
        // override means the user vouches for the machine, so skip the check.
        let runnability =
            (args.lock_and_install_config.allow_installs() && user_platform.is_none()).then(|| {
                classify_environment_runnability(
                    &executable_task.run_environment,
                    Some(lock_file.as_lock_file()),
                )
            });

        // Fail before announcing a task whose environment can't run here at
        // all; by-accident environments proceed and `--platform` overrides.
        if runnability == Some(EnvironmentRunnability::Unsupported) {
            return Err(
                match verify_current_platform_can_run_environment(
                    &executable_task.run_environment,
                    Some(lock_file.as_lock_file()),
                ) {
                    Err(err) => err.into(),
                    Ok(()) => executable_task
                        .run_environment
                        .unsupported_platform_error()
                        .into(),
                },
            );
        }

        // Showing which command is being run if the level and type allows it.
        if tracing::enabled!(Level::WARN)
            && (!executable_task.task().is_custom() || stdin_script_command.is_some())
        {
            if task_idx > 0 {
                // Add a newline between task outputs
                pixi_progress::println!();
            }

            let display_command = if let Some(forwarded_args) = &stdin_display_args {
                if forwarded_args.is_empty() {
                    "python -c <stdin>".to_owned()
                } else {
                    format!("python -c <stdin> {}", forwarded_args.iter().format(" "))
                }
            } else {
                executable_task.display_command().to_string()
            };

            pixi_progress::println!(
                "{}{}{}{}{}{}{}",
                console::Emoji("✨ ", ""),
                console::style("Pixi task (").bold(),
                console::style(executable_task.name().unwrap_or("unnamed"))
                    .green()
                    .bold(),
                // Only print environment if multiple environments are available
                if workspace.environments().len() > 1 {
                    format!(
                        " in {}",
                        executable_task.run_environment.name().fancy_display()
                    )
                } else {
                    "".to_string()
                },
                console::style("): ").bold(),
                display_command,
                if let Some(description) = executable_task.task().description() {
                    console::style(format!(": ({description})")).yellow()
                } else {
                    console::style("".to_string()).yellow()
                }
            );
        }

        // on dry-run mode, we just print the command and skip the execution
        if args.dry_run {
            task_idx += 1;
            continue;
        }

        // check task cache
        let task_cache = match executable_task
            .can_skip(lock_file.as_lock_file())
            .await
            .into_diagnostic()?
        {
            CanSkip::No(cache) => cache,
            CanSkip::Yes => {
                let args_text = if !executable_task.args().is_empty() {
                    format!(
                        " with args {}",
                        console::style(executable_task.args()).bold()
                    )
                } else {
                    String::new()
                };

                pixi_progress::println!(
                    "Task '{}'{args_text} can be skipped (cache hit) 🚀",
                    console::style(executable_task.name().unwrap_or("")).bold()
                );
                task_idx += 1;
                continue;
            }
        };

        // If we don't have a command environment yet, we need to compute it. We lazily
        // compute the task environment because we only need the environment if
        // a task is actually executed.
        let task_env: &_ = match task_envs.entry(executable_task.run_environment.clone()) {
            Entry::Occupied(env) => env.into_mut(),
            Entry::Vacant(entry) => {
                // Report the platform per environment: a bare `pixi run` may
                // span environments that declare different platforms.
                tracing::info!(
                    "Running tasks in environment '{}' assuming platform '{}'",
                    executable_task.run_environment.name().fancy_display(),
                    executable_task.platform.name(),
                );

                // A dependency-less environment installs nothing that could
                // require a virtual package the machine lacks, so skip the
                // prefix build and platform validation -- its tasks run
                // anywhere, relying only on the host environment.
                if !is_script
                    && args.lock_and_install_config.allow_installs()
                    && runnability != Some(EnvironmentRunnability::NoDependencies)
                {
                    // No `--platform`: pin to the platform this environment was
                    // last installed for, not a sibling's bare subdir.
                    if user_platform.is_none() {
                        lock_file.target_platform = executable_task
                            .run_environment
                            .installed_resolved_platform_name();
                    }

                    // Ensure there is a valid prefix
                    lock_file
                        .prefix(
                            &executable_task.run_environment,
                            UpdateMode::QuickValidate,
                            &ReinstallPackages::default(),
                            &pixi_core::environment::InstallFilter::default(),
                        )
                        .await?;

                    // Validate that the auto-detected machine (or explicit
                    // `--platform`) can run what was installed, comparing
                    // against the resolved/minimum platforms in conda-meta/pixi.
                    pixi_core::workspace::virtual_packages::verify_run_platform(
                        &executable_task.run_environment,
                        user_platform.as_ref(),
                    )?;
                }

                // Clear the current progress reports.
                progress.on_clear();

                // Clear caches based on the filesystem. The tasks might change files on disk.
                lock_file.command_dispatcher.clear_filesystem_caches().await;

                let command_env = get_task_env(
                    &executable_task.run_environment,
                    &executable_task.platform,
                    args.clean_env || executable_task.task().clean_env(),
                    Some(lock_file.as_lock_file()),
                    workspace.config().force_activate(),
                    workspace.config().experimental_activation_cache_usage(),
                )
                .await?;
                entry.insert(command_env)
            }
        };

        let task_env = task_env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();

        // Execute the task itself within the command environment. If one of the tasks
        // failed with a non-zero exit code, we exit this parent process with
        // the same code.
        match execute_task(
            &executable_task,
            &task_env,
            signal.clone(),
            stdin_script_command.as_ref(),
        )
        .await
        {
            Ok(_) => {
                task_idx += 1;
            }
            Err(TaskExecutionError::NonZeroExitCode(code)) => {
                if code == 127 {
                    command_not_found(&workspace, explicit_environment.clone());
                }
                process_exit::exit_with_code(code);
            }
            Err(err) => return Err(err.into()),
        }

        // Compute post-run hash, warn on missing globs, and update the cache
        let post_hash = executable_task
            .compute_post_run_hash(lock_file.as_lock_file(), task_cache)
            .await
            .into_diagnostic()?;
        if let Some(ref hash) = post_hash {
            executable_task.warn_on_missing_globs(hash);
        }
        executable_task
            .save_cache(post_hash)
            .await
            .into_diagnostic()?;
    }

    Ok(())
}

/// Called when a command was not found.
fn command_not_found<'p>(workspace: &'p Workspace, explicit_environment: Option<Environment<'p>>) {
    let available_tasks: HashSet<TaskName> =
        if let Some(explicit_environment) = explicit_environment {
            explicit_environment.get_filtered_tasks()
        } else {
            workspace
                .environments()
                .into_iter()
                .flat_map(|env| env.get_filtered_tasks())
                .collect()
        };

    if !available_tasks.is_empty() {
        pixi_progress::println!(
            "\nAvailable tasks:\n{}",
            available_tasks
                .into_iter()
                .sorted()
                .format_with("\n", |name, f| {
                    f(&format_args!("\t{}", name.fancy_display().bold()))
                })
        );
    }

    // Point at the missing platform only when it is genuinely what blocks the
    // run. An environment that installs nothing, or whose packages the machine
    // already satisfies, runs here regardless of the declared platforms, so
    // suggesting `platform add` would send the user after the wrong problem.
    if workspace.environments().iter().all(|env| {
        classify_environment_runnability(env, None) == EnvironmentRunnability::Unsupported
    }) {
        pixi_progress::println!(
            "\nHelp: This platform ({}) is not supported. Please run the following command to add this platform to the workspace:\n\n\tpixi workspace platform add {}",
            Platform::current(),
            Platform::current()
        );
    }
}

const MAX_SCRIPT_RUN_ATTEMPTS: usize = 3;

struct ScriptRunOptions<'platform> {
    progress: Arc<pixi_reporters::TopLevelProgress>,
    requested_lock_file_usage: LockFileUsage,
    transient: bool,
    no_install: bool,
    dry_run: bool,
    user_platform: Option<&'platform PixiPlatformName>,
}

/// Resolve, publish, and synchronize a script environment without holding its
/// publication guard across activation-capable solve or prefix work.
async fn prepare_script_environment<'p>(
    workspace: &'p Workspace,
    solve_workspace: &Workspace,
    options: ScriptRunOptions<'_>,
) -> miette::Result<LockFileDerivedData<'p>> {
    let ScriptRunOptions {
        progress,
        requested_lock_file_usage,
        transient,
        no_install,
        dry_run,
        user_platform,
    } = options;
    for attempt in 1..=MAX_SCRIPT_RUN_ATTEMPTS {
        let has_sidecar = workspace
            .persistent_lock_file_path()
            .is_some_and(|path| path.is_file());
        let lock_file_usage = if transient {
            transient_script_lock_file_usage(requested_lock_file_usage)?
        } else {
            script_lock_file_usage(requested_lock_file_usage, true, has_sidecar)?
        };
        let initial_guard = acquire_script_resolution_state(workspace).await;
        workspace.ensure_script_metadata_unchanged().await?;
        let sidecar_state = if has_sidecar {
            Some(workspace.script_lock_file_state().await?)
        } else {
            None
        };
        let baseline = load_script_run_resolution(
            workspace,
            has_sidecar,
            initial_guard.as_ref(),
            dry_run,
            lock_file_usage,
        )
        .await?;
        drop(initial_guard);

        let satisfiability = if has_sidecar {
            SatisfiabilityMode::Exact
        } else {
            SatisfiabilityMode::Sufficient
        };
        let (solved, mut updated) = solve_workspace
            .update_lock_file(
                Some(progress.clone()),
                UpdateLockFileOptions {
                    lock_file_usage,
                    no_install,
                    max_concurrent_solves: workspace.config().max_concurrent_solves(),
                    lock_file_input: LockFileInput::Ephemeral {
                        lock_file: baseline.clone().unwrap_or_default(),
                        satisfiability,
                    },
                    ..Default::default()
                },
            )
            .await?;
        let mut candidate = rebind_script_lock_file(workspace, solved)?;
        candidate.target_platform = user_platform.cloned();

        // uv applies permissive requirement checks to the installed
        // environment itself. Our reusable metadata is separate from the
        // prefix, so only take the same fast path when the completed-install
        // marker proves this exact resolution is present. Otherwise validate
        // the cached resolution exactly under current acquisition policy before
        // using it to repair the prefix.
        if !has_sidecar
            && satisfiability == SatisfiabilityMode::Sufficient
            && !updated
            && !dry_run
            && !no_install
            && !candidate.prefix_is_up_to_date(&workspace.default_environment())?
        {
            let (solved, exact_updated) = solve_workspace
                .update_lock_file(
                    Some(progress.clone()),
                    UpdateLockFileOptions {
                        lock_file_usage,
                        no_install,
                        max_concurrent_solves: workspace.config().max_concurrent_solves(),
                        lock_file_input: LockFileInput::Ephemeral {
                            lock_file: baseline.clone().unwrap_or_default(),
                            satisfiability: SatisfiabilityMode::Exact,
                        },
                        ..Default::default()
                    },
                )
                .await?;
            candidate = rebind_script_lock_file(workspace, solved)?;
            candidate.target_platform = user_platform.cloned();
            updated = exact_updated;
        }

        if dry_run {
            workspace.ensure_script_metadata_unchanged().await?;
            return Ok(candidate);
        }

        // Publish only if the metadata, authority kind, and baseline remained
        // unchanged while resolution ran.
        let publish_guard = acquire_script_resolution_state(workspace).await;
        workspace.ensure_script_metadata_unchanged().await?;
        let current_has_sidecar = workspace
            .persistent_lock_file_path()
            .is_some_and(|path| path.is_file());
        let current = load_script_run_resolution(
            workspace,
            current_has_sidecar,
            publish_guard.as_ref(),
            false,
            lock_file_usage,
        )
        .await?;
        let sidecar_is_current = match sidecar_state.as_ref() {
            Some(expected) if current_has_sidecar => {
                workspace
                    .script_lock_file_state_is_current(expected)
                    .await?
            }
            Some(_) => false,
            None => !current_has_sidecar,
        };
        let baseline_is_current = script_resolutions_equal(baseline.as_ref(), current.as_ref())?;

        if !sidecar_is_current || (publish_guard.is_some() && !baseline_is_current) {
            drop(publish_guard);
            if attempt == MAX_SCRIPT_RUN_ATTEMPTS {
                return Err(pixi_core::workspace::ScriptResolutionConflictError.into());
            }
            tracing::debug!(
                attempt,
                "script resolution changed; retrying run preparation"
            );
            continue;
        }

        let installing = !no_install;
        if has_sidecar {
            if updated {
                candidate.write_to_disk()?;
            }
            if installing
                && let Some(guard) = publish_guard.as_ref()
                && let Err(error) = guard.store(candidate.as_lock_file()).await
            {
                tracing::warn!(
                    %error,
                    "failed to shadow the script resolution before synchronization"
                );
            }
        } else if installing
            && let Some(guard) = publish_guard.as_ref()
            && let Err(error) = guard.store(candidate.as_lock_file()).await
        {
            tracing::warn!(
                %error,
                "failed to cache the script resolution; the environment remains usable"
            );
        }
        drop(publish_guard);

        if !installing {
            return Ok(candidate);
        }

        let environment = workspace.default_environment();
        let runnability = user_platform.is_none().then(|| {
            classify_environment_runnability(&environment, Some(candidate.as_lock_file()))
        });
        if runnability == Some(EnvironmentRunnability::Unsupported) {
            return Err(
                match verify_current_platform_can_run_environment(
                    &environment,
                    Some(candidate.as_lock_file()),
                ) {
                    Err(error) => error.into(),
                    Ok(()) => environment.unsupported_platform_error().into(),
                },
            );
        }

        if runnability != Some(EnvironmentRunnability::NoDependencies) {
            if user_platform.is_none() {
                candidate.target_platform = environment.installed_resolved_platform_name();
            }
            candidate
                .prefix(
                    &environment,
                    UpdateMode::QuickValidate,
                    &ReinstallPackages::default(),
                    &InstallFilter::default(),
                )
                .await?;
            pixi_core::workspace::virtual_packages::verify_run_platform(
                &environment,
                user_platform,
            )?;
        }

        let reconcile_guard = acquire_script_resolution_state(workspace).await;
        workspace.ensure_script_metadata_unchanged().await?;
        let current_has_sidecar = workspace
            .persistent_lock_file_path()
            .is_some_and(|path| path.is_file());
        let current = load_script_run_resolution(
            workspace,
            current_has_sidecar,
            reconcile_guard.as_ref(),
            false,
            lock_file_usage,
        )
        .await?;
        let candidate_is_current = current_has_sidecar == has_sidecar
            && script_resolutions_equal(Some(candidate.as_lock_file()), current.as_ref())?;
        let sidecar_became_hidden = has_sidecar
            && !current_has_sidecar
            && script_resolutions_equal(Some(candidate.as_lock_file()), current.as_ref())?;

        let uncoordinated_lockless_run =
            !has_sidecar && !current_has_sidecar && reconcile_guard.is_none();
        if candidate_is_current || sidecar_became_hidden || uncoordinated_lockless_run {
            if has_sidecar
                && let Some(guard) = reconcile_guard.as_ref()
                && let Err(error) = guard.store(candidate.as_lock_file()).await
            {
                tracing::warn!(
                    %error,
                    "failed to refresh the cached script resolution; the sidecar lock remains authoritative"
                );
            }
            return Ok(candidate);
        }

        drop(reconcile_guard);
        if attempt == MAX_SCRIPT_RUN_ATTEMPTS {
            workspace.reconcile_script_prefix_to_authority().await?;
            return Err(pixi_core::workspace::ScriptResolutionConflictError.into());
        }
        tracing::debug!(
            attempt,
            "script resolution changed during installation; retrying run preparation"
        );
    }

    unreachable!("the bounded script run loop always returns")
}

async fn acquire_script_resolution_state(
    workspace: &Workspace,
) -> Option<ScriptResolutionStateGuard> {
    match workspace.acquire_script_resolution_state().await {
        Ok(guard) => Some(guard),
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to coordinate the cached script environment; continuing without cached resolution state"
            );
            None
        }
    }
}

async fn load_script_run_resolution(
    workspace: &Workspace,
    has_sidecar: bool,
    guard: Option<&ScriptResolutionStateGuard>,
    allow_unlocked_hidden_read: bool,
    lock_file_usage: LockFileUsage,
) -> miette::Result<Option<LockFile>> {
    if has_sidecar {
        let loaded = workspace.load_lock_file().await?;
        if matches!(
            lock_file_usage,
            LockFileUsage::Locked | LockFileUsage::Frozen
        ) {
            Ok(Some(loaded.into_lock_file()?))
        } else {
            Ok(Some(loaded.into_lock_file_or_empty_with_warning()))
        }
    } else if let Some(guard) = guard {
        Ok(guard.load(workspace).await)
    } else if allow_unlocked_hidden_read {
        Ok(workspace.load_script_resolution_state().await)
    } else {
        Ok(None)
    }
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

#[derive(Debug, Error, Diagnostic)]
enum TaskExecutionError {
    #[error("the script exited with a non-zero exit code {0}")]
    NonZeroExitCode(i32),

    #[error(transparent)]
    #[diagnostic(transparent)]
    FailedToParseShellScript(#[from] FailedToParseShellScript),

    #[error(transparent)]
    InvalidWorkingDirectory(#[from] InvalidWorkingDirectory),

    #[error(transparent)]
    UnsupportedPlatformError(#[from] UnsupportedPlatformError),
}

/// Called to execute a single command.
///
/// This function is called from [`execute`].
async fn execute_task(
    task: &ExecutableTask<'_>,
    command_env: &HashMap<OsString, OsString>,
    kill_signal: KillSignal,
    stdin_script: Option<&StdinScriptCommand>,
) -> Result<(), TaskExecutionError> {
    let Some(script) = task.as_deno_script()? else {
        return Ok(());
    };
    let cwd = task.working_directory()?;
    let custom_commands = stdin_script
        .map(|command| HashMap::from([(STDIN_SCRIPT_COMMAND.to_owned(), command.shell_command())]))
        .unwrap_or_default();
    let execute_future = deno_task_shell::execute(
        script,
        command_env.clone(),
        cwd,
        custom_commands,
        kill_signal.clone(),
    );

    // Execute the process and forward signals.
    let status_code = run_future_forwarding_signals(kill_signal, execute_future).await;
    if status_code != 0 {
        return Err(TaskExecutionError::NonZeroExitCode(status_code));
    }

    Ok(())
}

/// Called to disambiguate between environments to run a task in.
fn disambiguate_task_interactive<'p>(
    problem: &AmbiguousTask<'p>,
) -> Option<TaskAndEnvironment<'p>> {
    // If any of the candidate tasks declares a `default-environment` that
    // corresponds to one of the candidate environments, prefer that
    // environment automatically.
    if let Some(idx) = problem.environments.iter().position(|(env, task)| {
        if let Some(default_env_name) = task.default_environment() {
            default_env_name == env.name()
        } else {
            false
        }
    }) {
        return Some(problem.environments[idx].clone());
    }

    let environment_names = problem
        .environments
        .iter()
        .map(|(env, _)| env.name())
        .collect_vec();
    let theme = ColorfulTheme {
        active_item_style: console::Style::new().for_stderr().magenta(),
        ..ColorfulTheme::default()
    };

    dialoguer::Select::with_theme(&theme)
        .with_prompt(format!(
            "The task '{}' {}can be run in multiple environments.\n\nPlease select an environment to run the task in:",
            problem.task_name.fancy_display(),
            if let Some(dependency) = &problem.depended_on_by {
                format!("(depended on by '{}') ", dependency.0.fancy_display())
            } else {
                String::new()
            }
        ))
        .report(false)
        .items(&environment_names)
        .default(0)
        .interact_opt()
        .map_or(None, identity)
        .map(|idx| problem.environments[idx].clone())
}

/// `dialoguer` doesn't clean up your term if it's aborted via e.g. `SIGINT` or
/// other exceptions: <https://github.com/console-rs/dialoguer/issues/188>.
///
/// `dialoguer`, as a library, doesn't want to mess with signal handlers,
/// but we, as an application, are free to mess with signal handlers if we feel
/// like it, since we own the process.
/// This function was taken from <https://github.com/dnjstrom/git-select-branch/blob/16c454624354040bc32d7943b9cb2e715a5dab92/src/main.rs#L119>.
fn reset_cursor() {
    let term = console::Term::stdout();
    let _ = term.show_cursor();
}

// /// Exit the process with the appropriate exit code for a SIGINT.
// fn exit_process_on_sigint() {
//     // https://learn.microsoft.com/en-us/cpp/c-runtime-library/signal-constants
//     #[cfg(target_os = "windows")]
//     std::process::exit(3);
//
//     // POSIX compliant OSs: 128 + SIGINT (2)
//     #[cfg(not(target_os = "windows"))]
//     std::process::exit(130);
// }

/// Runs a task future forwarding any signals received to the process.
///
/// Signal listeners and ctrl+c listening will be setup.
pub async fn run_future_forwarding_signals<TOutput>(
    #[cfg_attr(windows, allow(unused_variables))] kill_signal: KillSignal,
    future: impl std::future::Future<Output = TOutput>,
) -> TOutput {
    fn spawn_future_with_cancellation(
        future: impl std::future::Future<Output = ()> + 'static,
        token: CancellationToken,
    ) {
        tokio::task::spawn_local(async move {
            tokio::select! {
              _ = future => {}
              _ = token.cancelled() => {}
            }
        });
    }

    let token = CancellationToken::new();
    let _token_drop_guard = token.clone().drop_guard();
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async move {
            #[cfg(windows)]
            spawn_future_with_cancellation(listen_ctrl_c_windows(), token.clone());

            #[cfg(unix)]
            spawn_future_with_cancellation(listen_and_forward_all_signals(kill_signal), token);

            future.await
        })
        .await
}

#[cfg(windows)]
async fn listen_ctrl_c_windows() {
    // On windows, ctrl+c is sent to the process group, so the signal would
    // have already been sent to the child process. We still want to listen
    // for ctrl+c here to keep the process alive when receiving it, but no
    // need to forward the signal because it's already been sent.
    while let Ok(()) = tokio::signal::ctrl_c().await {}
}

/// Listens to all incoming signals and forwards all of them, except
/// some cases.
///
/// Note that we don't handle `SIGINT` correctly, if the subprocess changes
/// its PGID, then the system won't forward CTRL+C automatically.
/// However, we should do that to ensure consistent behaviour.
///
/// To resolve this we should patch `deno_task_shell` to return PID
/// from which we could get PGID and do things right.
///
/// Resulting approach should mimic
/// <https://github.com/astral-sh/uv/blob/9d17dfa3537312b928f94479f632891f918c4760/crates/uv/src/child.rs#L156C21-L168C77>
#[cfg(unix)]
async fn listen_and_forward_all_signals(kill_signal: KillSignal) {
    use futures::FutureExt;

    use pixi_core::signals::SIGNALS;

    // listen and forward every signal we support
    let mut futures = Vec::with_capacity(SIGNALS.len());
    let is_interactive = std::io::stdin().is_terminal();
    for signo in SIGNALS.iter().copied() {
        if signo == libc::SIGKILL
            || signo == libc::SIGSTOP
            || (signo == libc::SIGINT && is_interactive)
        {
            continue; // skip, can't listen to these
        }

        let kill_signal = kill_signal.clone();
        futures.push(
            async move {
                let Ok(mut stream) = tokio::signal::unix::signal(signo.into()) else {
                    return;
                };
                let signal_kind = signo.into();
                while let Some(()) = stream.recv().await {
                    kill_signal.send(signal_kind);
                }
            }
            .boxed_local(),
        )
    }
    futures::future::join_all(futures).await;
}
