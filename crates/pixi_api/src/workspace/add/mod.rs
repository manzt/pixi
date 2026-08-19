use std::future::Future;

use indexmap::IndexMap;
use miette::{Diagnostic, IntoDiagnostic};
use pixi_core::{
    environment::{LockFileUsage, sanity_check_workspace},
    workspace::{PypiDeps, SkippedPackage, UpdateDeps, WorkspaceMut},
};
use pixi_manifest::{
    DependencyOverwriteBehavior, FeatureName, HasWorkspaceManifest, KnownPreviewFlag, SpecType,
};
use pixi_spec::{GitSpec, SourceSpec, Subdirectory};
use rattler_conda_types::{MatchSpec, PackageName};
use thiserror::Error;

use crate::workspace::platforms::resolve_platforms;

mod options;

pub use options::{DependencyOptions, GitOptions};

#[derive(Debug, Diagnostic, Error)]
#[error("dependency update cancelled")]
pub struct DependencyUpdateCancelled;

async fn await_script_dependency_update<T>(
    is_script: bool,
    future: impl Future<Output = T>,
) -> Option<T> {
    if is_script {
        tokio::select! {
            result = future => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        }
    } else {
        Some(future.await)
    }
}

async fn save_dependency_edit(
    workspace: WorkspaceMut,
    is_script: bool,
    lock_file_usage: LockFileUsage,
) -> Result<(), std::io::Error> {
    if is_script && lock_file_usage == LockFileUsage::DryRun {
        workspace.save_and_clear_script_resolution().await?;
    } else {
        workspace.save().await?;
    }
    Ok(())
}

pub async fn add_conda_dep(
    mut workspace: WorkspaceMut,
    specs: IndexMap<PackageName, MatchSpec>,
    spec_type: SpecType,
    dep_options: DependencyOptions,
    git_options: GitOptions,
) -> miette::Result<(Option<UpdateDeps>, Vec<SkippedPackage>)> {
    sanity_check_workspace(workspace.workspace()).await?;

    // Resolve the requested platforms, accepting bare subdirs as subdir
    // platforms, and add any that the workspace does not yet declare.
    let workspace_platforms = workspace
        .workspace()
        .workspace_manifest()
        .workspace
        .platforms
        .clone();
    let pixi_platforms = resolve_platforms(&workspace_platforms, &dep_options.platforms)?;
    workspace
        .manifest()
        .add_platforms(pixi_platforms.iter(), &FeatureName::Default)?;

    let mut match_specs = IndexMap::default();
    let mut source_specs = IndexMap::default();

    // if user passed some git configuration
    // we will use it to create pixi source specs
    let passed_specs: IndexMap<PackageName, (MatchSpec, SpecType)> = specs
        .into_iter()
        .map(|(name, spec)| (name, (spec, spec_type)))
        .collect();

    if let Some(git) = &git_options.git {
        if !workspace
            .manifest()
            .workspace
            .preview()
            .is_enabled(KnownPreviewFlag::PixiBuild)
        {
            return Err(miette::miette!(
                help = "Run `pixi workspace preview add pixi-build` to enable the preview flag",
                "conda source dependencies are not allowed without enabling the 'pixi-build' preview flag"
            ));
        }

        let subdirectory = git_options
            .subdir
            .clone()
            .map(Subdirectory::try_from)
            .transpose()
            .into_diagnostic()?
            .unwrap_or_default();
        source_specs = passed_specs
            .iter()
            .map(|(name, (_spec, spec_type))| {
                let git_spec = GitSpec::new(
                    git.clone(),
                    Some(git_options.reference.clone()),
                    subdirectory.clone(),
                );
                (name.clone(), (SourceSpec::from(git_spec), *spec_type))
            })
            .collect();
    } else {
        match_specs = passed_specs;
    }

    // TODO: add dry_run logic to add
    let dry_run = false;

    let targets = workspace.target_selectors_for_platforms(&dep_options.platforms);
    let is_script = workspace.workspace().is_script();
    let update_result = await_script_dependency_update(
        is_script,
        Box::pin(workspace.update_dependencies(
            match_specs,
            IndexMap::default(),
            source_specs,
            dep_options.no_install,
            &dep_options.lock_file_usage,
            &dep_options.feature,
            &targets,
            false,
            dry_run,
            DependencyOverwriteBehavior::OverwriteIfExplicit,
        )),
    )
    .await;
    let Some(update_result) = update_result else {
        workspace.revert().await.into_diagnostic()?;
        return Err(DependencyUpdateCancelled.into());
    };
    let (update_deps, skipped) = match update_result {
        Ok(result) => {
            // Write the updated manifest
            save_dependency_edit(workspace, is_script, dep_options.lock_file_usage)
                .await
                .into_diagnostic()?;
            result
        }
        Err(e) => {
            workspace.revert().await.into_diagnostic()?;
            return Err(e);
        }
    };

    Ok((update_deps, skipped))
}

pub async fn add_pypi_dep(
    mut workspace: WorkspaceMut,
    pypi_deps: PypiDeps,
    editable: bool,
    options: DependencyOptions,
) -> miette::Result<(Option<UpdateDeps>, Vec<SkippedPackage>)> {
    sanity_check_workspace(workspace.workspace()).await?;

    // Resolve the requested platforms, accepting bare subdirs as subdir
    // platforms, and add any that the workspace does not yet declare.
    let workspace_platforms = workspace
        .workspace()
        .workspace_manifest()
        .workspace
        .platforms
        .clone();
    let pixi_platforms = resolve_platforms(&workspace_platforms, &options.platforms)?;
    workspace
        .manifest()
        .add_platforms(pixi_platforms.iter(), &FeatureName::Default)?;

    // TODO: add dry_run logic to add
    let dry_run = false;

    let targets = workspace.target_selectors_for_platforms(&options.platforms);
    let is_script = workspace.workspace().is_script();
    let update_result = await_script_dependency_update(
        is_script,
        Box::pin(workspace.update_dependencies(
            IndexMap::default(),
            pypi_deps,
            IndexMap::default(),
            options.no_install,
            &options.lock_file_usage,
            &options.feature,
            &targets,
            editable,
            dry_run,
            DependencyOverwriteBehavior::OverwriteIfExplicit,
        )),
    )
    .await;
    let Some(update_result) = update_result else {
        workspace.revert().await.into_diagnostic()?;
        return Err(DependencyUpdateCancelled.into());
    };
    let (update_deps, skipped) = match update_result {
        Ok(result) => {
            // Write the updated manifest
            save_dependency_edit(workspace, is_script, options.lock_file_usage)
                .await
                .into_diagnostic()?;
            result
        }
        Err(e) => {
            workspace.revert().await.into_diagnostic()?;
            return Err(e);
        }
    };

    Ok((update_deps, skipped))
}
