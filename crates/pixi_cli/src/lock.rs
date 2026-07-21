use std::path::PathBuf;

use clap::Parser;
use miette::{Context, IntoDiagnostic};
use pixi_config::Config;
use pixi_core::{
    Workspace, WorkspaceLocator,
    environment::LockFileUsage,
    lock_file::{LockFileDerivedData, UpdateLockFileOptions},
};
use pixi_diff::{LockFileDiff, LockFileJsonDiff};
use pixi_manifest::script::ScriptManifest;

use crate::cli_config::NoInstallConfig;
use crate::cli_config::WorkspaceConfig;

/// Solve environment and update the lock file without installing the
/// environments.
#[derive(Debug, Parser)]
#[clap(arg_required_else_help = false)]
pub struct Args {
    #[clap(flatten)]
    pub config_source: pixi_config::ConfigSourceCli,

    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    /// Lock a Python script using its inline PEP 723 metadata.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["manifest_path", "workspace"]
    )]
    pub script: Option<PathBuf>,

    #[clap(flatten)]
    pub config: pixi_config::ConfigCli,

    #[clap(flatten)]
    pub no_install_config: NoInstallConfig,

    /// Output the changes in JSON format.
    #[clap(long)]
    pub json: bool,

    /// Check if any changes have been made to the lock file.
    /// If yes, exit with a non-zero code.
    #[clap(long)]
    pub check: bool,

    ///Compute the lock file without writing to disk.
    /// Implies --no-install
    #[clap(long)]
    pub dry_run: bool,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let mut workspace = if let Some(path) = &args.script {
        let script = ScriptManifest::from_path(path)?.ok_or_else(|| {
            miette::miette!(
                help = format!(
                    "Initialize it with `pixi init --script {}`.",
                    path.display()
                ),
                "{} does not contain a PEP 723 metadata block",
                path.display()
            )
        })?;
        let root = script
            .path()
            .parent()
            .expect("an absolute script path always has a parent");
        let config = Config::load_with(root, &args.config_source.source())
            .merge_config(args.config.clone().into());
        let script_workspace = Workspace::from_script(script, config)?;
        for warning in script_workspace.warnings {
            tracing::warn!("{warning}");
        }
        script_workspace.value
    } else {
        WorkspaceLocator::for_cli()
            .with_global_config_source(args.config_source.source())
            .with_search_start(args.workspace_config.workspace_locator_start())
            .locate()?
            .with_cli_config(args.config.clone())
    };

    // Apply backend override if provided (primarily for testing)
    if let Some(backend_override) = args.workspace_config.backend_override.clone() {
        workspace = workspace.with_backend_override(backend_override);
    }

    // Update the lock file, and extract it from the derived data to drop additional resources
    // created for the solve.
    // Use the silent version here since update_lock_file() will display the warning.
    let original_lock_file = workspace.load_lock_file().await?.into_lock_file_or_empty();
    let progress = pixi_reporters::TopLevelProgress::from_global();
    let (LockFileDerivedData { lock_file, .. }, lock_updated) = workspace
        .update_lock_file(
            Some(progress),
            UpdateLockFileOptions {
                lock_file_usage: if args.dry_run {
                    LockFileUsage::DryRun
                } else {
                    LockFileUsage::Update
                },
                no_install: args.no_install_config.no_install || args.dry_run,
                upgrade_lock_file_format: true,
                max_concurrent_solves: workspace.config().max_concurrent_solves(),
            },
        )
        .await?;

    // Determine the diff between the old and new lock file.
    let diff = LockFileDiff::from_lock_files(&original_lock_file, &lock_file);

    // Format as json?
    if args.json {
        let diff = LockFileDiff::from_lock_files(&original_lock_file, &lock_file);
        let json_diff = LockFileJsonDiff::new(Some(workspace.named_environments()), diff);
        let json = serde_json::to_string_pretty(&json_diff).expect("failed to convert to json");
        println!("{json}");
    } else if args.dry_run {
        if lock_updated {
            eprintln!(
                "{}Dry-run: lock file would be updated (not written to disk)",
                console::style(console::Emoji("i ", "i ")).blue()
            );
            diff.print()
                .into_diagnostic()
                .context("failed to print lock file diff")?;
        } else {
            eprintln!(
                "{}Dry-run: lock file would not change",
                console::style(console::Emoji("i ", "i ")).blue()
            );
        }
    } else if lock_updated {
        eprintln!(
            "{}Updated lock file",
            console::style(console::Emoji("✔ ", "")).green()
        );
        diff.print()
            .into_diagnostic()
            .context("failed to print lock file diff")?;
    } else {
        eprintln!(
            "{}Lock-file was already up-to-date",
            console::style(console::Emoji("✔ ", "")).green()
        );
    }

    if args.check && lock_updated {
        miette::bail!("lock file not up-to-date with the workspace");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_mode_is_explicit() {
        let args = Args::try_parse_from(["lock", "--script", "example.py"]).unwrap();
        assert_eq!(args.script, Some(PathBuf::from("example.py")));

        assert!(Args::try_parse_from(["lock", "example.py"]).is_err());
    }

    #[test]
    fn script_mode_rejects_workspace_selection() {
        assert!(
            Args::try_parse_from([
                "lock",
                "--script",
                "example.py",
                "--manifest-path",
                "pixi.toml",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from(["lock", "--script", "example.py", "--workspace", "dev"]).is_err()
        );
    }
}
