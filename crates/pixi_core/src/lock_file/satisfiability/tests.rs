// Top-level orchestration tests for satisfiability verification. They walk
// through the full pipeline against fixture workspaces and snapshot the
// resulting diagnostic. Lives at `lock_file::satisfiability::tests` so the
// existing snapshot files under `satisfiability/snapshots/` keep matching
// the generated module-path key. Per-module unit tests live next to the
// code they exercise.

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Component, PathBuf},
    sync::Arc,
};

use dashmap::DashMap;
use insta::Settings;
use itertools::Itertools;
use miette::{Diagnostic, IntoDiagnostic, NarratableReportHandler};
use once_cell::sync::OnceCell;
use pep440_rs::{Operator, Version, VersionSpecifiers};
use pixi_build_backend_passthrough::PassthroughBackend;
use pixi_build_frontend::BackendOverride;
use pixi_command_dispatcher::{CacheDirs, CommandDispatcherError};
use pixi_manifest::FeaturesExt;
use pixi_manifest::HasWorkspaceManifest;
use pixi_manifest::PixiPlatformName;
use pixi_record::LockFileResolver;
use pixi_uv_context::UvResolutionContext;
use rattler_lock::LockFile;
use rstest::rstest;
use std::str::FromStr;
use thiserror::Error;
use tracing_test::traced_test;

use super::{
    EnvironmentUnsat, PlatformUnsat, SatisfiabilityMode, SolveGroupUnsat,
    VerifySatisfiabilityContext, pypi_metadata, verify_environment_satisfiability_with_mode,
    verify_platform_satisfiability, verify_solve_group_satisfiability,
};
use crate::{
    Workspace,
    lock_file::outdated::{BuildCacheKey, PypiEnvironmentBuildCache},
};

#[derive(Error, Debug, Diagnostic)]
enum LockfileUnsat {
    #[error("environment '{0}' is missing")]
    EnvironmentMissing(String),

    #[error("environment '{0}' is in the lock-file but no longer exists in the project")]
    EnvironmentRemoved(String),

    #[error("environment '{0}' does not satisfy the requirements of the project")]
    Environment(String, #[source] EnvironmentUnsat),

    #[error(
        "environment '{0}' does not satisfy the requirements of the project for platform '{1}'"
    )]
    PlatformUnsat(String, PixiPlatformName, #[source] PlatformUnsat),

    #[error(
        "solve group '{0}' does not satisfy the requirements of the project for platform '{1}'"
    )]
    SolveGroupUnsat(String, PixiPlatformName, #[source] SolveGroupUnsat),

    #[error("failed to build the lock file resolver: {0}")]
    ResolverBuild(String),
}

async fn verify_lock_file_satisfiability(
    project: &Workspace,
    lock_file: &LockFile,
    backend_override: Option<BackendOverride>,
) -> Result<(), LockfileUnsat> {
    verify_lock_file_satisfiability_with_mode(
        project,
        lock_file,
        backend_override,
        SatisfiabilityMode::Exact,
    )
    .await
}

async fn verify_lock_file_satisfiability_with_mode(
    project: &Workspace,
    lock_file: &LockFile,
    backend_override: Option<BackendOverride>,
    satisfiability: SatisfiabilityMode,
) -> Result<(), LockfileUnsat> {
    // Ensure the rayon thread pool is initialized before any code path
    // that might trigger implicit rayon initialization (e.g. uv's
    // DistributionDatabase). Without this, concurrent tests can race
    // and trigger a GlobalPoolAlreadyInitialized panic.
    uv_configuration::initialize_rayon_once();

    // Mirror production's load path (`Workspace::load_lock_file`): align the
    // lockfile's platform names to the manifest by identity, so short on-disk
    // aliases like `p1` resolve to the workspace platform names the rest of
    // this check matches against.
    let aligned_lock_file = crate::lock_file::platform_rename::align_platform_names(
        lock_file.clone(),
        project.workspace_manifest(),
        project.root(),
    );
    let lock_file = &aligned_lock_file;

    let mut individual_verified_envs = HashMap::new();

    let temp_pixi_dir = tempfile::tempdir().unwrap();
    let command_dispatcher = {
        let command_dispatcher = project
            .command_dispatcher_builder()
            .unwrap()
            .with_cache_dirs(CacheDirs::new(
                pixi_path::AbsPathBuf::new(temp_pixi_dir.path())
                    .expect("tempdir path should be absolute")
                    .into_assume_dir(),
            ));
        let command_dispatcher = if let Some(backend_override) = backend_override {
            command_dispatcher.with_backend_overrides(backend_override)
        } else {
            command_dispatcher
        };
        command_dispatcher.finish()
    };

    // Create UV context lazily for building dynamic metadata
    let uv_context: OnceCell<UvResolutionContext> = OnceCell::new();

    // Create build caches for sharing between satisfiability and resolution
    let build_caches: DashMap<BuildCacheKey, Arc<PypiEnvironmentBuildCache>> = DashMap::new();

    // Create static metadata cache for sharing across platforms
    let static_metadata_cache: DashMap<PathBuf, pypi_metadata::LocalPackageMetadata> =
        DashMap::new();

    let resolver = LockFileResolver::build(lock_file, project.root())
        .map_err(|err| LockfileUnsat::ResolverBuild(err.to_string()))?;

    // Verify that the lock-file does not contain environments that no longer
    // exist in the project.
    for (name, _) in lock_file.environments() {
        if project.environment(name).is_none() {
            return Err(LockfileUnsat::EnvironmentRemoved(name.to_string()));
        }
    }

    // Verify individual environment satisfiability
    for env in project.environments() {
        let locked_env = lock_file
            .environment(env.name().as_str())
            .ok_or_else(|| LockfileUnsat::EnvironmentMissing(env.name().to_string()))?;
        verify_environment_satisfiability_with_mode(&env, locked_env, satisfiability)
            .map_err(|e| LockfileUnsat::Environment(env.name().to_string(), e))?;

        for platform in env.platforms() {
            let ctx = VerifySatisfiabilityContext {
                environment: &env,
                command_dispatcher: command_dispatcher.clone(),
                platform: platform.clone(),
                project_root: project.root(),
                uv_context: &uv_context,
                config: project.config(),
                project_env_vars: project.env_vars().clone(),
                build_caches: &build_caches,
                static_metadata_cache: &static_metadata_cache,
                resolver: &resolver,
                satisfiability,
            };
            let (verified_env, _locked_pypi) = verify_platform_satisfiability(&ctx, locked_env)
                .await
                .map_err(|e| match e {
                    CommandDispatcherError::Failed(e) => {
                        LockfileUnsat::PlatformUnsat(env.name().to_string(), platform.clone(), *e)
                    }
                    CommandDispatcherError::Cancelled => {
                        panic!("operation was cancelled which should never happen here")
                    }
                })?;

            individual_verified_envs.insert((env.name(), platform), verified_env);
        }
    }

    // Verify the solve group requirements
    for solve_group in project.solve_groups() {
        for platform in solve_group.platforms() {
            verify_solve_group_satisfiability(solve_group.environments().filter_map(|env| {
                individual_verified_envs.remove(&(env.name(), platform.clone()))
            }))
            .map_err(|e| {
                LockfileUnsat::SolveGroupUnsat(solve_group.name().to_string(), platform.clone(), e)
            })?;
        }
    }

    // Verify environments not part of a solve group
    for ((env_name, platform), verified_env) in individual_verified_envs.into_iter() {
        verify_solve_group_satisfiability([verified_env])
            .map_err(|e| match e {
                SolveGroupUnsat::CondaPackageShouldBePypi { name } => {
                    PlatformUnsat::CondaPackageShouldBePypi { name }
                }
            })
            .map_err(|e| LockfileUnsat::PlatformUnsat(env_name.to_string(), platform, e))?;
    }

    Ok(())
}

#[rstest]
#[tokio::test]
#[traced_test]
async fn test_good_satisfiability(
    #[files("../../tests/data/satisfiability/*/pixi.toml")] manifest_path: PathBuf,
) {
    // TODO: skip this test on windows
    // Until we can figure out how to handle unix file paths with pep508_rs url
    // parsing correctly
    if manifest_path
        .components()
        .contains(&Component::Normal(OsStr::new("absolute-paths")))
        && cfg!(windows)
    {
        return;
    }

    let project = Workspace::from_path(&manifest_path).unwrap();
    let lock_file = LockFile::from_path(&project.lock_file_path()).unwrap();
    match verify_lock_file_satisfiability(
        &project,
        &lock_file,
        Some(BackendOverride::from_memory(
            PassthroughBackend::instantiator(),
        )),
    )
    .await
    .into_diagnostic()
    {
        Ok(()) => {}
        Err(e) => panic!("{e:?}"),
    }
}

#[rstest]
#[tokio::test]
#[traced_test]
async fn test_failing_satisfiability(
    #[files("../../tests/data/non-satisfiability/*/pixi.toml")] manifest_path: PathBuf,
) {
    let report_handler = NarratableReportHandler::new().with_cause_chain();

    let project = Workspace::from_path(&manifest_path).unwrap();
    let lock_file = LockFile::from_path(&project.lock_file_path()).unwrap();
    let err = verify_lock_file_satisfiability(
        &project,
        &lock_file,
        Some(BackendOverride::from_memory(
            PassthroughBackend::instantiator(),
        )),
    )
    .await
    .expect_err("expected failing satisfiability");

    let name = manifest_path
        .parent()
        .unwrap()
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap();

    let mut s = String::new();
    report_handler.render_report(&mut s, &err).unwrap();

    let mut settings = Settings::clone_current();
    settings.set_snapshot_suffix(name);
    settings.bind(|| {
        // run snapshot test here
        insta::assert_snapshot!(s);
    });
}

fn non_satisfiability_fixture(name: &str) -> (Workspace, LockFile) {
    fixture_with_manifest("non-satisfiability", name, None)
}

fn fixture_with_manifest(
    category: &str,
    name: &str,
    manifest: Option<&str>,
) -> (Workspace, LockFile) {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/data")
        .join(category)
        .join(name);
    let manifest_path = fixture_path.join("pixi.toml");
    let project = if let Some(manifest) = manifest {
        Workspace::from_str(&manifest_path, manifest).unwrap()
    } else {
        Workspace::from_path(&manifest_path).unwrap()
    };
    let lock_file = LockFile::from_path(&fixture_path.join("pixi.lock")).unwrap();
    (project, lock_file)
}

fn passthrough_backend() -> Option<BackendOverride> {
    Some(BackendOverride::from_memory(
        PassthroughBackend::instantiator(),
    ))
}

#[derive(Debug, Clone, Copy)]
enum ExpectedExactPolicyFailure {
    SolveStrategy,
    ChannelPriority,
    ExcludeNewer,
    PypiPrerelease,
    NoBuild,
    AdditionalPlatforms,
}

#[rstest]
#[case("mismatch-solve-strategy", ExpectedExactPolicyFailure::SolveStrategy)]
#[case(
    "mismatch-channel-priority",
    ExpectedExactPolicyFailure::ChannelPriority
)]
#[case("mismatch-exclude-newer", ExpectedExactPolicyFailure::ExcludeNewer)]
#[case(
    "mismatch-pypi-prerelease-mode",
    ExpectedExactPolicyFailure::PypiPrerelease
)]
#[case("non-binary-no-build", ExpectedExactPolicyFailure::NoBuild)]
#[case("too-many-platforms", ExpectedExactPolicyFailure::AdditionalPlatforms)]
fn sufficient_environment_satisfiability_accepts_resolution_policy_differences(
    #[case] fixture: &str,
    #[case] expected: ExpectedExactPolicyFailure,
) {
    let (project, lock_file) = non_satisfiability_fixture(fixture);
    let environment = project.default_environment();
    let locked_environment = lock_file.environment("default").unwrap();

    let exact_error = verify_environment_satisfiability_with_mode(
        &environment,
        locked_environment,
        SatisfiabilityMode::Exact,
    )
    .expect_err("the fixture must differ under exact lock-file semantics");
    let matches_expected_variant = matches!(
        (expected, &exact_error),
        (
            ExpectedExactPolicyFailure::SolveStrategy,
            EnvironmentUnsat::SolveStrategyMismatch { .. }
        ) | (
            ExpectedExactPolicyFailure::ChannelPriority,
            EnvironmentUnsat::ChannelPriorityMismatch { .. }
        ) | (
            ExpectedExactPolicyFailure::ExcludeNewer,
            EnvironmentUnsat::ExcludeNewerMismatch(_)
        ) | (
            ExpectedExactPolicyFailure::PypiPrerelease,
            EnvironmentUnsat::PypiPrereleaseModeMismatch { .. }
        ) | (
            ExpectedExactPolicyFailure::NoBuild,
            EnvironmentUnsat::NoBuildWithNonBinaryPackages(_)
        ) | (
            ExpectedExactPolicyFailure::AdditionalPlatforms,
            EnvironmentUnsat::AdditionalPlatformsInLockFile(_)
        )
    );
    assert!(
        matches_expected_variant,
        "{fixture} did not isolate the intended exact-policy failure: {exact_error:?}"
    );

    verify_environment_satisfiability_with_mode(
        &environment,
        locked_environment,
        SatisfiabilityMode::Sufficient,
    )
    .unwrap_or_else(|error| panic!("{fixture} should remain a sufficient solution: {error:?}"));
}

#[tokio::test]
async fn sufficient_satisfiability_does_not_require_pypi_index_provenance() {
    let fixture = "pypi-index-mismatch";
    let (project, lock_file) = non_satisfiability_fixture(fixture);

    let exact_error = verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Exact,
    )
    .await
    .expect_err("the fixture must differ under exact lock-file semantics");
    assert!(
        matches!(
            exact_error,
            LockfileUnsat::PlatformUnsat(_, _, PlatformUnsat::LockedPyPIIndexMismatch { .. })
        ),
        "the fixture did not isolate the intended index mismatch: {exact_error:?}"
    );

    verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Sufficient,
    )
    .await
    .unwrap_or_else(|error| panic!("{fixture} should remain a sufficient solution: {error:?}"));
}

#[derive(Debug, Clone, Copy)]
enum ExpectedSufficientFailure {
    MatchSpec,
    SourceSpec,
    PlatformDefinition,
    WheelTags,
}

#[rstest]
#[case("mismatched-spec", ExpectedSufficientFailure::MatchSpec)]
#[case("missing-dependency", ExpectedSufficientFailure::MatchSpec)]
#[case("mismatched-source-spec", ExpectedSufficientFailure::SourceSpec)]
#[case(
    "changed-platform-subdir",
    ExpectedSufficientFailure::PlatformDefinition
)]
#[case(
    "changed-platform-virtual-package",
    ExpectedSufficientFailure::PlatformDefinition
)]
#[case("wheels-with-wrong-tags", ExpectedSufficientFailure::WheelTags)]
#[tokio::test]
async fn sufficient_satisfiability_rejects_real_incompatibilities(
    #[case] fixture: &str,
    #[case] expected: ExpectedSufficientFailure,
) {
    let (project, lock_file) = non_satisfiability_fixture(fixture);
    let error = verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Sufficient,
    )
    .await
    .expect_err("a real incompatibility must not be accepted as sufficient");

    let matches_expected_variant = matches!(
        (expected, &error),
        (
            ExpectedSufficientFailure::MatchSpec,
            LockfileUnsat::PlatformUnsat(_, _, PlatformUnsat::UnsatisfiableMatchSpec(_, _))
        ) | (
            ExpectedSufficientFailure::SourceSpec,
            LockfileUnsat::PlatformUnsat(_, _, PlatformUnsat::SourcePackageMismatch(_, _))
        ) | (
            ExpectedSufficientFailure::PlatformDefinition,
            LockfileUnsat::Environment(_, EnvironmentUnsat::PlatformDefinitionChanged(_))
        ) | (
            ExpectedSufficientFailure::WheelTags,
            LockfileUnsat::PlatformUnsat(_, _, PlatformUnsat::PypiWheelTagsMismatch { .. })
        )
    );
    assert!(
        matches_expected_variant,
        "{fixture} failed with the wrong incompatibility: {error:?}"
    );
}

#[test]
fn sufficient_environment_satisfiability_accepts_changed_channels() {
    let manifest = r#"
[workspace]
channels = ["https://example.invalid/channel"]
name = "changed-channel"
platforms = ["linux-64"]
"#;
    let (project, lock_file) =
        fixture_with_manifest("satisfiability", "wheel-with-correct-tags", Some(manifest));
    let environment = project.default_environment();
    let locked_environment = lock_file.environment("default").unwrap();

    assert!(matches!(
        verify_environment_satisfiability_with_mode(
            &environment,
            locked_environment,
            SatisfiabilityMode::Exact,
        ),
        Err(EnvironmentUnsat::ChannelsMismatch)
    ));
    verify_environment_satisfiability_with_mode(
        &environment,
        locked_environment,
        SatisfiabilityMode::Sufficient,
    )
    .expect("channel policy must not invalidate a compatible installed solution");
}

#[test]
fn sufficient_environment_satisfiability_accepts_changed_pypi_indexes() {
    let manifest = r#"
[workspace]
channels = ["conda-forge"]
name = "changed-index"
platforms = ["win-64"]

[workspace.pypi-options]
index-url = "https://different.example.com/simple"

[dependencies]
python = "3.12.*"

[pypi-dependencies]
my-dep = ">=1.0"
"#;
    let (project, lock_file) =
        fixture_with_manifest("satisfiability", "pypi-index-match", Some(manifest));
    let environment = project.default_environment();
    let locked_environment = lock_file.environment("default").unwrap();

    assert!(matches!(
        verify_environment_satisfiability_with_mode(
            &environment,
            locked_environment,
            SatisfiabilityMode::Exact,
        ),
        Err(EnvironmentUnsat::IndexesMismatch(_))
    ));
    verify_environment_satisfiability_with_mode(
        &environment,
        locked_environment,
        SatisfiabilityMode::Sufficient,
    )
    .expect("index policy must not invalidate a compatible installed solution");
}

#[tokio::test]
async fn sufficient_satisfiability_accepts_a_fully_surplus_resolution() {
    let manifest = r#"
[workspace]
channels = ["conda-forge"]
name = "empty-script"
platforms = ["linux-64"]
"#;
    let (project, lock_file) = fixture_with_manifest(
        "non-satisfiability",
        "wheels-with-wrong-tags",
        Some(manifest),
    );

    let exact_error = verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Exact,
    )
    .await
    .expect_err("exact validation must reject surplus packages");
    assert!(matches!(
        exact_error,
        LockfileUnsat::PlatformUnsat(_, _, PlatformUnsat::TooManyPypiPackages(_))
    ));

    verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Sufficient,
    )
    .await
    .expect("a fully surplus installed resolution remains sufficient");
}

#[tokio::test]
async fn sufficient_satisfiability_ignores_tags_on_a_surplus_wheel() {
    let manifest = r#"
[workspace]
channels = ["conda-forge"]
name = "reduced-script"
platforms = ["linux-64"]

[dependencies]
python = ">=3.14.2,<3.15"

[pypi-dependencies]
cffi = "==2.0.0"

[system-requirements]
libc = "2.17"
"#;
    let (project, lock_file) = fixture_with_manifest(
        "non-satisfiability",
        "wheels-with-wrong-tags",
        Some(manifest),
    );

    let exact_error = verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Exact,
    )
    .await
    .expect_err("exact validation must inspect the surplus wheel");
    assert!(matches!(
        exact_error,
        LockfileUnsat::Environment(_, EnvironmentUnsat::PypiWheelTagsMismatch { .. })
    ));

    verify_lock_file_satisfiability_with_mode(
        &project,
        &lock_file,
        passthrough_backend(),
        SatisfiabilityMode::Sufficient,
    )
    .await
    .expect("wheel tags on an unreachable surplus package are irrelevant");
}

#[test]
fn test_version_specifiers_logic() {
    let version = Version::from_str("1.19").unwrap();
    let version_specifiers = VersionSpecifiers::from_str("<2.0, >=1.16").unwrap();
    assert!(version_specifiers.contains(&version));
    // VersionSpecifiers derefs into a list of specifiers
    assert_eq!(
        version_specifiers
            .iter()
            .position(|specifier| *specifier.operator() == Operator::LessThan),
        Some(1)
    );
}
