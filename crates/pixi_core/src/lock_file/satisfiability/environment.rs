use std::{collections::HashSet, path::Path, str::FromStr};

use crate::lock_file::records_by_name::HasNameVersion;
use itertools::Itertools;
use pixi_install_pypi::UnresolvedPypiRecord;
use pixi_manifest::{
    FeaturesExt, HasWorkspaceManifest, PixiPlatform, PixiPlatformName, pypi::pypi_options::NoBuild,
};
use pixi_pypi_spec::PixiPypiSource;
use pypi_modifiers::Tags;
use rattler_conda_types::{ChannelUrl, NamedChannelOrUrl};
use rattler_lock::{LockedPackage, PypiIndexes, UrlOrPath};
use url::Url;
use uv_distribution_filename::{DistExtension, ExtensionError, SourceDistExtension, WheelFilename};

use super::errors::{
    EnvironmentUnsat, IndexesMismatch, PlatformDefinitionChanged, verify_exclude_newer,
};
use super::platform::resolve_lock_platform_for;
use crate::workspace::{Environment, grouped_environment::GroupedEnvironment};

/// Verifies that all the requirements of the specified `environment` can be
/// satisfied with the packages present in the lock file.
///
/// This function returns a [`EnvironmentUnsat`] error if a verification issue
/// occurred. The [`EnvironmentUnsat`] error should contain enough information
/// for the user and developer to figure out what went wrong.
pub fn verify_environment_satisfiability(
    environment: &Environment<'_>,
    locked_environment: rattler_lock::Environment<'_>,
) -> Result<(), EnvironmentUnsat> {
    verify_environment_satisfiability_with_mode(
        environment,
        locked_environment,
        SatisfiabilityMode::Exact,
    )
}

/// Controls whether a resolved environment must reproduce the current
/// resolution inputs exactly or merely satisfy the current manifest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SatisfiabilityMode {
    /// Treat resolution-policy changes as making the lock file out of date.
    #[default]
    Exact,
    /// Reuse an installed solution while it remains compatible with the
    /// manifest, even if policy changes could produce a different solution.
    Sufficient,
}

pub(crate) fn verify_environment_satisfiability_with_mode(
    environment: &Environment<'_>,
    locked_environment: rattler_lock::Environment<'_>,
    mode: SatisfiabilityMode,
) -> Result<(), EnvironmentUnsat> {
    let grouped_env = GroupedEnvironment::from(environment.clone());

    // Check if the channels in the lock file match our current configuration. Note
    // that the order matters here. If channels are added in a different order,
    // the solver might return a different result.
    let config = environment.channel_config();
    if mode == SatisfiabilityMode::Exact {
        let channels: Vec<ChannelUrl> = grouped_env
            .channels()
            .into_iter()
            .map(|channel| channel.clone().into_base_url(&config))
            .try_collect()?;

        let locked_channels: Vec<ChannelUrl> = locked_environment
            .channels()
            .iter()
            .map(|c| {
                NamedChannelOrUrl::from_str(&c.url)
                    .unwrap_or_else(|_err| NamedChannelOrUrl::Name(c.url.clone()))
                    .into_base_url(&config)
            })
            .try_collect()?;

        // Check if channels match or were only extended (appended).
        // If locked_channels is a prefix of channels, only lower-priority channels were added,
        // which doesn't affect existing package selections due to channel priority semantics.
        if channels.starts_with(&locked_channels) {
            if channels.len() > locked_channels.len() {
                // Channels were extended - lock file needs update but packages are still valid
                return Err(EnvironmentUnsat::ChannelsExtended);
            }
            // Exact match - channels are identical, no error
        } else {
            // Channels were removed, reordered, or prepended - need full re-solve
            return Err(EnvironmentUnsat::ChannelsMismatch);
        }
    }

    let platforms = environment.platforms();
    let locked_platform_data: Vec<rattler_lock::PlatformData> = locked_environment
        .platforms()
        .map(|p| rattler_lock::PlatformData {
            name: p.name().clone(),
            subdir: p.subdir(),
            virtual_packages: p.virtual_packages().to_vec(),
        })
        .collect();
    // Subdirs the env actually targets. A lockfile platform whose subdir is
    // covered by some env platform is treated as the same target -- this is
    // the case for old lockfiles whose bare-subdir names no longer appear in
    // workspace.platforms after the `[system-requirements]` migration.
    let env_subdirs: HashSet<rattler_conda_types::Platform> = platforms
        .iter()
        .filter_map(|name| {
            environment
                .workspace_manifest()
                .workspace
                .platform_by_name(name)
                .map(|p| p.subdir())
        })
        .collect();
    let additional_platforms: HashSet<PixiPlatformName> = locked_platform_data
        .iter()
        .filter_map(|lp| {
            // A foreign/hand-edited name that isn't a valid pixi platform name
            // can't match a workspace platform; skip it rather than panicking.
            let name = PixiPlatformName::try_from(lp.name.as_str()).ok()?;
            if platforms.contains(&name) || env_subdirs.contains(&lp.subdir) {
                None
            } else {
                Some(name)
            }
        })
        .collect();
    if mode == SatisfiabilityMode::Exact && !additional_platforms.is_empty() {
        return Err(EnvironmentUnsat::AdditionalPlatformsInLockFile(
            additional_platforms,
        ));
    }

    // For every platform that the workspace and the lockfile share by name,
    // make sure their `subdir` and declared virtual-package set still agree.
    // Without this check, `pixi workspace platform edit ... --subdir X` or
    // `--cuda V` would update the manifest but leave the lockfile silently
    // stale: the satisfiability layer would say "fine, same name" and the
    // outdated-envs machinery would short-circuit without re-solving.
    let workspace_manifest = environment.workspace_manifest();
    for locked in &locked_platform_data {
        let Ok(name) = PixiPlatformName::try_from(locked.name.as_str()) else {
            continue;
        };
        if !platforms.contains(&name) {
            continue;
        }
        let Some(workspace_platform) = workspace_manifest.workspace.platform_by_name(&name) else {
            continue;
        };
        // Compare only the user-customised virtual packages. The subdir
        // defaults (`__unix`, `__linux`, `__glibc`, `__win`, `__osx`,
        // `__archspec`) are materialised into the workspace platform at
        // parse/edit time, but they are pixi's baseline assumption rather
        // than user intent. Filtering them on both sides keeps lock files
        // produced before the defaults-materialisation change satisfying,
        // and keeps the comparison focused on what the user actually
        // changed (e.g. adding/removing `__cuda` or pinning `__glibc` to
        // a non-default version).
        let workspace_subdir = workspace_platform.subdir();
        let expected_vps: Vec<String> = workspace_platform
            .declared_virtual_packages()
            .iter()
            .filter(|gvp| !pixi_manifest::platform::is_subdir_default(gvp, workspace_subdir))
            .map(|vp| vp.to_string())
            .collect();
        let locked_vps: Vec<String> = locked
            .virtual_packages
            .iter()
            .filter(|raw| {
                pixi_manifest::platform::parse_locked_virtual_package(raw)
                    .map(|gvp| !pixi_manifest::platform::is_subdir_default(&gvp, workspace_subdir))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        let same_subdir = workspace_subdir == locked.subdir;
        // Compare VPs as multisets: lockfile ordering is not part of the
        // platform's identity for satisfiability purposes.
        let same_vps = {
            let mut a = expected_vps.clone();
            let mut b = locked_vps.clone();
            a.sort();
            b.sort();
            a == b
        };
        if !same_subdir || !same_vps {
            return Err(EnvironmentUnsat::PlatformDefinitionChanged(
                PlatformDefinitionChanged {
                    name,
                    expected_subdir: workspace_subdir,
                    found_subdir: locked.subdir,
                    expected_virtual_packages: expected_vps,
                    found_virtual_packages: locked_vps,
                },
            ));
        }
    }

    // Do some more checks if we have pypi dependencies
    // 1. Check if the PyPI indexes are present and match
    // 2. Check if we have a no-build option set, that we only have binary packages,
    //    or an editable source
    // 3. Check that wheel tags still are possible with current system requirements
    let pypi_dependencies = environment.pypi_dependencies(None);
    if !pypi_dependencies.is_empty() {
        let group_pypi_options = grouped_env.pypi_options();
        let indexes = rattler_lock::PypiIndexes::from(group_pypi_options.clone());

        // Index and build policy affect which solution would be selected, but
        // do not invalidate a compatible installed solution in sufficient mode.
        if mode == SatisfiabilityMode::Exact {
            verify_pypi_indexes(locked_environment, indexes)?;

            let no_build_check = PypiNoBuildCheck::new(group_pypi_options.no_build.as_ref());
            // Exact validation checks every locked artifact. Build wheel tags
            // for each named Pixi platform rather than keying them by Conda
            // subdir: rich platforms can share a subdir while selecting
            // different Python ABI/system tags.
            for platform_name in &platforms {
                let Some(pixi_platform) = environment
                    .workspace_manifest()
                    .workspace
                    .platform_by_name(platform_name)
                else {
                    continue;
                };
                let Some(lock_platform) =
                    resolve_lock_platform_for(locked_environment.lock_file(), pixi_platform)
                else {
                    continue;
                };
                let Some(package_it) = locked_environment.pypi_packages(lock_platform) else {
                    continue;
                };
                let pypi_wheel_tags_check =
                    PypiWheelTagsCheck::new(pixi_platform, &locked_environment);
                for package_data in package_it {
                    let record = UnresolvedPypiRecord::from(package_data.clone());
                    let pypi_source = pypi_dependencies
                        .get(record.name())
                        .and_then(|specs| specs.last())
                        .map(|spec| &spec.source);
                    no_build_check.check(&record, pypi_source)?;
                    pypi_wheel_tags_check.check(&record)?;
                }
            }
        }
    }

    if mode == SatisfiabilityMode::Exact {
        // Verify solver options.
        let expected_solve_strategy = environment.solve_strategy().into();
        if locked_environment.solve_options().strategy != expected_solve_strategy {
            return Err(EnvironmentUnsat::SolveStrategyMismatch {
                locked_strategy: locked_environment.solve_options().strategy,
                expected_strategy: expected_solve_strategy,
            });
        }

        let expected_channel_priority = environment
            .channel_priority()
            .unwrap_or_default()
            .unwrap_or_default()
            .into();
        if locked_environment.solve_options().channel_priority != expected_channel_priority {
            return Err(EnvironmentUnsat::ChannelPriorityMismatch {
                locked_priority: locked_environment.solve_options().channel_priority,
                expected_priority: expected_channel_priority,
            });
        }

        let locked_prerelease_mode = locked_environment
            .solve_options()
            .pypi_prerelease_mode
            .into();
        let expected_prerelease_mode = grouped_env
            .pypi_options()
            .prerelease_mode
            .unwrap_or_default();
        if locked_prerelease_mode != expected_prerelease_mode {
            return Err(EnvironmentUnsat::PypiPrereleaseModeMismatch {
                locked_mode: locked_prerelease_mode,
                expected_mode: expected_prerelease_mode,
            });
        }

        let resolved_exclude_newer = environment.exclude_newer_config_resolved(&config)?;

        let exclude_newer = resolved_exclude_newer
            .as_ref()
            .cloned()
            .map(rattler_solve::ExcludeNewer::from);

        if let Err(err) = verify_exclude_newer(exclude_newer.as_ref(), &locked_environment) {
            return Err(EnvironmentUnsat::ExcludeNewerMismatch(err));
        }
    }

    Ok(())
}

pub(super) struct PypiWheelTagsCheck {
    wheel_tags: Option<Tags>,
}

impl PypiWheelTagsCheck {
    pub(super) fn new(
        pixi_platform: &PixiPlatform,
        locked_environment: &rattler_lock::Environment<'_>,
    ) -> Self {
        let wheel_tags = resolve_lock_platform_for(locked_environment.lock_file(), pixi_platform)
            .and_then(|lock_platform| locked_environment.packages(lock_platform))
            .into_iter()
            .flatten()
            .filter_map(|package| match package {
                LockedPackage::Conda(rattler_lock::CondaPackageData::Binary(package)) => {
                    Some(package)
                }
                _ => None,
            })
            .find(|package| pypi_modifiers::pypi_tags::is_python_record(&package.package_record))
            .and_then(|package| {
                pypi_modifiers::pypi_tags::get_pypi_tags(pixi_platform, &package.package_record)
                    .ok()
            });

        PypiWheelTagsCheck { wheel_tags }
    }

    pub fn check(&self, package_data: &UnresolvedPypiRecord) -> Result<(), EnvironmentUnsat> {
        if let Some(wheel) = self.incompatible_wheel(package_data.as_package_data()) {
            Err(EnvironmentUnsat::PypiWheelTagsMismatch { wheel })
        } else {
            Ok(())
        }
    }

    pub(super) fn incompatible_wheel(
        &self,
        package_data: &rattler_lock::PypiPackageData,
    ) -> Option<String> {
        let package_file_name = package_data.location().file_name()?;
        let platform_tags = self.wheel_tags.as_ref()?;
        let Ok(wheel) = WheelFilename::from_str(package_file_name) else {
            return None;
        };
        if !wheel.is_compatible(platform_tags) {
            Some(wheel.name.to_string())
        } else {
            None
        }
    }
}

// Check if we are disallowing all source packages or only a subset
#[derive(Eq, PartialEq)]
enum Check {
    All,
    Packages(HashSet<pep508_rs::PackageName>),
}

pub struct PypiNoBuildCheck {
    check: Option<Check>,
}

impl PypiNoBuildCheck {
    pub fn new(no_build: Option<&NoBuild>) -> Self {
        let check = match no_build {
            // Ok, so we are allowed to build any source package
            Some(NoBuild::None) | None => None,
            // We are not allowed to build any source package
            Some(NoBuild::All) => Some(Check::All),
            // We are not allowed to build a subset of source packages
            Some(NoBuild::Packages(hash_set)) => {
                let packages = hash_set
                    .iter()
                    .filter_map(|name| pep508_rs::PackageName::new(name.to_string()).ok())
                    .collect();
                Some(Check::Packages(packages))
            }
        };

        Self { check }
    }

    pub fn check(
        &self,
        package_data: &UnresolvedPypiRecord,
        source: Option<&PixiPypiSource>,
    ) -> Result<(), EnvironmentUnsat> {
        let package_data = package_data.as_package_data();
        let Some(check) = &self.check else {
            return Ok(());
        };

        // Determine if we do not accept non-wheels for all packages or only for a
        // subset Check all the currently locked packages if we are making any
        // violations
        // Small helper function to get the dist extension from a url
        fn pypi_dist_extension_from_url(url: &Url) -> Result<DistExtension, ExtensionError> {
            // Take the file name from the url
            let path = url
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or_default();
            // Convert the path to a dist extension
            DistExtension::from_path(Path::new(path))
        }

        let extension = match &**package_data.location() {
            // Get the extension from the url
            UrlOrPath::Url(url) => {
                if url.scheme().starts_with("git+") {
                    // Just choose some source extension, does not really matter, cause it is
                    // actually a directory, this is just for the check
                    Ok(DistExtension::Source(SourceDistExtension::TarGz))
                } else {
                    pypi_dist_extension_from_url(url)
                }
            }
            UrlOrPath::Path(path) => {
                // Editables are allowed with no-build
                // Check this before is_dir() because the path may be relative
                // and not resolve correctly from the current working directory
                let is_editable = source
                    .map(|source| match source {
                        PixiPypiSource::Path { path: _, editable } => editable.unwrap_or_default(),
                        _ => false,
                    })
                    .unwrap_or_default();
                if is_editable {
                    return Ok(());
                }
                let path = Path::new(path.as_str());
                if path.is_dir() {
                    // Non-editable source packages might not be allowed
                    Ok(DistExtension::Source(SourceDistExtension::TarGz))
                } else {
                    // Could be a reference to a wheel or sdist
                    DistExtension::from_path(path)
                }
            }
        }?;

        match extension {
            // Wheels are fine
            DistExtension::Wheel => Ok(()),
            // Check if we have a source package that we are not allowed to build
            // it could be that we are only disallowing for certain source packages
            DistExtension::Source(_) => match check {
                Check::All => Err(EnvironmentUnsat::NoBuildWithNonBinaryPackages(
                    package_data.name().to_string(),
                )),
                Check::Packages(hash_set) => {
                    if hash_set.contains(package_data.name()) {
                        Err(EnvironmentUnsat::NoBuildWithNonBinaryPackages(
                            package_data.name().to_string(),
                        ))
                    } else {
                        Ok(())
                    }
                }
            },
        }
    }
}

fn verify_pypi_indexes(
    locked_environment: rattler_lock::Environment<'_>,
    indexes: PypiIndexes,
) -> Result<(), EnvironmentUnsat> {
    match locked_environment.pypi_indexes() {
        None => {
            // Mismatch when there should be an index but there is not
            if locked_environment
                .lock_file()
                .version()
                .should_pypi_indexes_be_present()
                && locked_environment
                    .pypi_packages_by_platform()
                    .any(|(_platform, mut packages)| packages.next().is_some())
            {
                return Err(IndexesMismatch {
                    current: indexes,
                    previous: None,
                }
                .into());
            }
        }
        Some(locked_indexes) => {
            if locked_indexes != &indexes {
                return Err(IndexesMismatch {
                    current: indexes,
                    previous: Some(locked_indexes.clone()),
                }
                .into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use pixi_manifest::{PixiPlatform, PixiPlatformName};
    use rattler_conda_types::{
        PackageName, PackageRecord, Platform, Version, package::DistArchiveIdentifier,
    };
    use rattler_lock::{
        CondaBinaryData, CondaPackageData, LockFile, PlatformData, PlatformName, SolveOptions,
        UrlOrPath, Verbatim,
    };
    use url::Url;

    use super::PypiWheelTagsCheck;
    use crate::lock_file::tests::make_wheel_package_with;

    fn python_package(version: &str) -> CondaPackageData {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked("python"),
            Version::from_str(version).unwrap(),
            "h0".to_string(),
        );
        record.subdir = "linux-64".to_string();
        let file_name = format!("python-{version}-h0.conda");
        CondaPackageData::Binary(Box::new(CondaBinaryData {
            package_record: record,
            location: UrlOrPath::Url(
                Url::parse(&format!("https://example.invalid/{file_name}")).unwrap(),
            ),
            file_name: DistArchiveIdentifier::try_from_filename(&file_name).unwrap(),
            channel: None,
        }))
    }

    #[test]
    fn wheel_tags_are_computed_per_named_platform() {
        let old = PixiPlatform::new(
            PixiPlatformName::try_from("linux-old-python").unwrap(),
            Platform::Linux64,
            vec![],
        )
        .unwrap();
        let new = PixiPlatform::new(
            PixiPlatformName::try_from("linux-new-python").unwrap(),
            Platform::Linux64,
            vec![],
        )
        .unwrap();
        let mut builder = LockFile::builder()
            .with_platforms(vec![
                PlatformData {
                    name: PlatformName::try_from(old.name().as_str()).unwrap(),
                    subdir: Platform::Linux64,
                    virtual_packages: vec![],
                },
                PlatformData {
                    name: PlatformName::try_from(new.name().as_str()).unwrap(),
                    subdir: Platform::Linux64,
                    virtual_packages: vec![],
                },
            ])
            .unwrap();
        builder.set_channels("default", Vec::<rattler_lock::Channel>::new());
        builder.set_options("default", SolveOptions::default());
        builder
            .add_conda_package("default", old.name().as_str(), python_package("3.9.0"))
            .unwrap();
        builder
            .add_conda_package("default", new.name().as_str(), python_package("3.12.0"))
            .unwrap();
        let lock = builder.finish();
        let environment = lock.environment("default").unwrap();
        let wheel = pixi_install_pypi::UnresolvedPypiRecord::from(make_wheel_package_with(
            "demo",
            "1.0.0",
            Verbatim::new(UrlOrPath::Url(
                Url::parse(
                    "https://example.invalid/demo-1.0.0-cp312-cp312-manylinux_2_17_x86_64.whl",
                )
                .unwrap(),
            )),
            None,
            None,
            vec![],
            None,
        ));

        assert!(
            PypiWheelTagsCheck::new(&old, &environment)
                .check(&wheel)
                .is_err()
        );
        assert!(
            PypiWheelTagsCheck::new(&new, &environment)
                .check(&wheel)
                .is_ok()
        );
    }
}
