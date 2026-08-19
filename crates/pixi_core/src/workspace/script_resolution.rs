//! Cached resolution state for PEP 723 script workspaces.
//!
//! This is deliberately separate from a script's adjacent lock file. It is
//! disposable cache metadata used to avoid resolving an already-satisfying
//! ephemeral environment on every invocation.

use std::path::{Path, PathBuf};

use async_fd_lock::LockWrite;
use miette::{Diagnostic, IntoDiagnostic, WrapErr};
use pixi_manifest::{HasWorkspaceManifest, script::ScriptManifest};
use rattler_digest::{Sha256, digest::Digest};
use rattler_lock::LockFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::Workspace;
use crate::{
    environment::InstallFilter,
    lock_file::{LockFileDerivedData, ReinstallPackages, UpdateMode},
};

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "script-resolution-v1.json";

fn script_resolution_lock_path(identity: &Path) -> std::io::Result<PathBuf> {
    let digest = Sha256::digest(identity.as_os_str().as_encoded_bytes());
    Ok(script_resolution_lock_root()?
        .join(format!("pixi-script-{}.lock", &hex::encode(digest)[..16])))
}

fn script_resolution_lock_root() -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        Ok(system_user_home()?.join(".pixi").join("script-locks"))
    }
    #[cfg(not(unix))]
    {
        dirs::data_local_dir()
            .map(|root| root.join("pixi").join("script-locks"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "could not determine a stable per-user script lock directory",
                )
            })
    }
}

#[cfg(unix)]
fn system_user_home() -> std::io::Result<PathBuf> {
    use std::{
        ffi::{CStr, OsString},
        mem::MaybeUninit,
        os::unix::ffi::OsStringExt,
        ptr,
    };

    // SAFETY: `geteuid` and `sysconf` have no pointer preconditions.
    let effective_user = unsafe { libc::geteuid() };
    let suggested_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if suggested_size > 0 {
        suggested_size as usize
    } else {
        1024
    };

    loop {
        let mut passwd = MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        // SAFETY: all pointers refer to live writable storage for the duration
        // of the call, and `buffer` has the supplied length.
        let status = unsafe {
            libc::getpwuid_r(
                effective_user,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE {
            buffer_size = buffer_size.saturating_mul(2);
            continue;
        }
        if status != 0 {
            return Err(std::io::Error::from_raw_os_error(status));
        }
        if result.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine the effective user's home directory",
            ));
        }

        // SAFETY: a successful `getpwuid_r` initialized `passwd`, and `pw_dir`
        // points into `buffer` until the end of this loop iteration.
        let passwd = unsafe { passwd.assume_init() };
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
        if home.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the effective user's home directory is empty",
            ));
        }
        return Ok(PathBuf::from(OsString::from_vec(home.to_vec())));
    }
}

fn prepare_script_resolution_lock_root(lock_path: &Path) -> std::io::Result<()> {
    let root = lock_path
        .parent()
        .expect("a script resolution lock always has a parent");

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        fs_err::create_dir_all(
            root.parent()
                .expect("the account-local lock root always has a parent"),
        )?;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }

        let metadata = fs_err::symlink_metadata(root)?;
        // SAFETY: `geteuid` has no preconditions and does not dereference pointers.
        let effective_user = unsafe { libc::geteuid() };
        if !metadata.is_dir() || metadata.uid() != effective_user {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the script resolution lock directory is not owned by the current user",
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            fs_err::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    #[cfg(not(unix))]
    fs_err::create_dir_all(root)?;

    Ok(())
}

fn physical_script_file_name(path: &Path) -> std::io::Result<std::ffi::OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs_err::symlink_metadata(path)?;
        let parent = path
            .parent()
            .expect("a persistent script path always has a parent");
        let requested_name = path
            .file_name()
            .expect("a persistent script path always has a file name");
        let mut candidates = Vec::new();
        for entry in fs_err::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name() == requested_name {
                return Ok(entry.file_name());
            }
            let entry_metadata = fs_err::symlink_metadata(entry.path())?;
            if entry_metadata.dev() == metadata.dev() && entry_metadata.ino() == metadata.ino() {
                candidates.push(entry.file_name());
            }
        }
        select_script_file_name(requested_name, candidates)
    }

    #[cfg(windows)]
    {
        let requested_name = path
            .file_name()
            .expect("a persistent script path always has a file name");
        let parent = path
            .parent()
            .expect("a persistent script path always has a parent");
        let mut candidates = Vec::new();
        for entry in fs_err::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name() == requested_name {
                return Ok(entry.file_name());
            } else if windows_file_names_equal(&entry.file_name(), requested_name)? {
                candidates.push(entry.file_name());
            }
        }
        select_script_file_name(requested_name, candidates)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let metadata = fs_err::symlink_metadata(path)?;
        let resolved = if metadata.file_type().is_symlink() {
            path.to_owned()
        } else {
            dunce::canonicalize(path)?
        };
        resolved.file_name().map(ToOwned::to_owned).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a persistent script path must have a file name",
            )
        })
    }
}

fn select_script_file_name(
    requested: &std::ffi::OsStr,
    candidates: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::io::Result<std::ffi::OsString> {
    let mut fallback = None;
    let mut ambiguous = false;
    for candidate in candidates {
        if candidate == requested {
            return Ok(candidate);
        }
        if fallback.replace(candidate).is_some() {
            ambiguous = true;
        }
    }
    if ambiguous {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the script path has multiple physical directory-entry aliases",
        ));
    }
    fallback.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not find the script's physical directory entry",
        )
    })
}

#[cfg(windows)]
fn windows_file_names_equal(
    left: &std::ffi::OsStr,
    right: &std::ffi::OsStr,
) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    let left = left.encode_wide().collect::<Vec<_>>();
    let right = right.encode_wide().collect::<Vec<_>>();
    let left_len = i32::try_from(left.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the script file name is too long for Windows ordinal comparison",
        )
    })?;
    let right_len = i32::try_from(right.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the script file name is too long for Windows ordinal comparison",
        )
    })?;
    // SAFETY: both pointers refer to live UTF-16 buffers with the supplied
    // lengths for the duration of the call.
    Ok(
        unsafe { CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) }
            == CSTR_EQUAL,
    )
}

fn try_script_resolution_identity(
    workspace: &Workspace,
) -> Result<PathBuf, ScriptResolutionStateLockError> {
    if let Some(sidecar_path) = workspace.persistent_lock_file_path() {
        let script_path = workspace
            .script_manifest()
            .expect("persistent script workspaces retain their manifest")
            .path();
        let parent = script_path
            .parent()
            .expect("a persistent script path always has a parent");
        let canonical_parent =
            dunce::canonicalize(parent).map_err(|source| ScriptResolutionStateLockError {
                path: sidecar_path.clone(),
                source,
            })?;
        let mut sidecar_name = physical_script_file_name(script_path).map_err(|source| {
            ScriptResolutionStateLockError {
                path: sidecar_path.clone(),
                source,
            }
        })?;
        sidecar_name.push(".pixi.lock");
        Ok(canonical_parent.join(sidecar_name))
    } else {
        Ok(workspace.pixi_dir())
    }
}

#[cfg(test)]
fn script_resolution_identity(workspace: &Workspace) -> PathBuf {
    try_script_resolution_identity(workspace).unwrap()
}

#[derive(Serialize, Deserialize)]
struct StoredScriptResolution {
    version: u32,
    lock_file: String,
}

/// Failure to acquire the cross-process script resolution lock.
#[derive(Debug, Error, Diagnostic)]
#[error("failed to lock the cached script environment at `{path}`")]
pub struct ScriptResolutionStateLockError {
    path: PathBuf,
    #[source]
    source: std::io::Error,
}

/// A script manifest or its authoritative resolution changed while an
/// optimistic operation was in progress.
#[derive(Debug, Error, Diagnostic)]
#[error("the script environment changed while it was being updated")]
#[diagnostic(help("Retry the command against the updated script environment."))]
pub struct ScriptResolutionConflictError;

/// The exact adjacent lock-file state observed before an optimistic solve.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptLockFileState(Option<Vec<u8>>);

impl ScriptLockFileState {
    pub(crate) fn from_contents(contents: Vec<u8>) -> Self {
        Self(Some(contents))
    }

    pub(crate) fn is_absent(&self) -> bool {
        self.0.is_none()
    }
}

/// The exact hidden resolution-state file observed before a script mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptResolutionFileState(Option<Vec<u8>>);

/// Compare script resolutions by their stable rendered lock-file form.
pub fn script_resolutions_equal(
    left: Option<&LockFile>,
    right: Option<&LockFile>,
) -> miette::Result<bool> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => Ok(left
            .render_to_string()
            .into_diagnostic()
            .wrap_err("failed to serialize the cached script resolution")?
            == right
                .render_to_string()
                .into_diagnostic()
                .wrap_err("failed to serialize the cached script resolution")?),
        _ => Ok(false),
    }
}

/// Exclusive access to a script's resolution publication state.
///
/// The guard coordinates hidden-state and adjacent-lock-file transactions for
/// the same persistent script identity. Prefix mutation has its own locking
/// and callers must not assume this guard serializes environment installation.
pub struct ScriptResolutionStateGuard {
    state_path: PathBuf,
    _guard: async_fd_lock::RwLockWriteGuard<tokio::fs::File>,
}

impl Workspace {
    /// Synchronize a script prefix to whichever sidecar or hidden resolution is
    /// authoritative, retrying if authority changes during installation.
    ///
    /// This is the recovery path for an optimistic writer that installed a
    /// candidate and then discovered a different winner. Prefix work remains
    /// outside the publication guard because activation and build backends may
    /// invoke Pixi recursively.
    pub async fn reconcile_script_prefix_to_authority(&self) -> miette::Result<()> {
        const MAX_ATTEMPTS: usize = 3;

        for _ in 0..MAX_ATTEMPTS {
            let guard = self.acquire_script_resolution_state().await?;
            self.ensure_script_metadata_unchanged().await?;
            let has_sidecar = self
                .persistent_lock_file_path()
                .is_some_and(|path| path.is_file());
            let authority = if has_sidecar {
                self.load_lock_file()
                    .await?
                    .into_lock_file_or_empty_with_warning()
            } else {
                guard
                    .load(self)
                    .await
                    .ok_or(ScriptResolutionConflictError)?
            };
            drop(guard);

            let progress = pixi_reporters::TopLevelProgress::from_global();
            let dispatcher = progress
                .register_with(self.command_dispatcher_builder()?)
                .finish();
            let mut derived = LockFileDerivedData::from_input_lock_file(
                self,
                authority.clone(),
                dispatcher.package_cache().clone(),
                dispatcher,
                pixi_glob::GlobHashCache::default(),
            );
            let environment = self.default_environment();
            derived.target_platform = environment.installed_resolved_platform_name();
            derived
                .prefix(
                    &environment,
                    UpdateMode::Revalidate,
                    &ReinstallPackages::default(),
                    &InstallFilter::default(),
                )
                .await?;

            let guard = self.acquire_script_resolution_state().await?;
            self.ensure_script_metadata_unchanged().await?;
            let current_has_sidecar = self
                .persistent_lock_file_path()
                .is_some_and(|path| path.is_file());
            let current = if current_has_sidecar {
                Some(
                    self.load_lock_file()
                        .await?
                        .into_lock_file_or_empty_with_warning(),
                )
            } else {
                guard.load(self).await
            };
            if current_has_sidecar == has_sidecar
                && script_resolutions_equal(Some(&authority), current.as_ref())?
            {
                return Ok(());
            }
        }

        Err(ScriptResolutionConflictError.into())
    }

    /// Exclusively lock this script workspace's resolution publication state.
    pub async fn acquire_script_resolution_state(
        &self,
    ) -> Result<ScriptResolutionStateGuard, ScriptResolutionStateLockError> {
        let pixi_dir = self.pixi_dir();
        // Local script workspaces can share an adjacent lock file even when
        // their configured exec-cache roots differ. Coordinate them by that
        // persistent identity while keeping hidden state in the configured
        // cache location. Transient scripts have no adjacent lock and retain
        // their cache directory as their identity.
        let identity = try_script_resolution_identity(self)?;
        // Keep the coordination inode outside the disposable environment tree.
        // `pixi clean cache --exec` may remove that tree while another process
        // is using it; a stable lock path still serializes the next invocation.
        let lock_path = script_resolution_lock_path(&identity).map_err(|source| {
            ScriptResolutionStateLockError {
                path: identity.clone(),
                source,
            }
        })?;
        prepare_script_resolution_lock_root(&lock_path).map_err(|source| {
            ScriptResolutionStateLockError {
                path: lock_path.clone(),
                source,
            }
        })?;
        let lock_file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .await
            .map_err(|source| ScriptResolutionStateLockError {
                path: lock_path.clone(),
                source,
            })?;
        let guard =
            lock_file
                .lock_write()
                .await
                .map_err(|error| ScriptResolutionStateLockError {
                    path: lock_path,
                    source: error.error,
                })?;

        Ok(ScriptResolutionStateGuard {
            state_path: pixi_dir.join(STATE_FILE),
            _guard: guard,
        })
    }

    /// Load hidden script resolution state without acquiring the publication
    /// guard. The state file is atomically replaced, so read-only callers
    /// always observe a complete old or new value.
    pub async fn load_script_resolution_state(&self) -> Option<LockFile> {
        load_script_resolution(self.pixi_dir().join(STATE_FILE), self).await
    }

    /// Capture the current adjacent script lock file, including its absence.
    pub async fn script_lock_file_state(&self) -> miette::Result<ScriptLockFileState> {
        let path = self
            .persistent_lock_file_path()
            .expect("only persistent script workspaces have adjacent lock files");
        match tokio::fs::read(&path).await {
            Ok(contents) => Ok(ScriptLockFileState(Some(contents))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ScriptLockFileState(None))
            }
            Err(error) => Err(error)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read script lock file `{}`", path.display())),
        }
    }

    /// Reject publication if the adjacent lock file changed since `expected`.
    pub async fn ensure_script_lock_file_state(
        &self,
        expected: &ScriptLockFileState,
    ) -> miette::Result<()> {
        if !self.script_lock_file_state_is_current(expected).await? {
            return Err(ScriptResolutionConflictError.into());
        }
        Ok(())
    }

    /// Restore an adjacent script lock file to an earlier exact state.
    pub async fn restore_script_lock_file_state(
        &self,
        state: &ScriptLockFileState,
    ) -> miette::Result<()> {
        if self.script_lock_file_state_is_current(state).await? {
            return Ok(());
        }
        let path = self
            .persistent_lock_file_path()
            .expect("only persistent script workspaces have adjacent lock files");
        match &state.0 {
            Some(contents) => {
                pixi_utils::atomic_write::atomic_write_strict(&path, contents)
                    .await
                    .into_diagnostic()
                    .wrap_err_with(|| {
                        format!("failed to restore script lock file `{}`", path.display())
                    })?;
            }
            None => match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).into_diagnostic().wrap_err_with(|| {
                        format!("failed to remove script lock file `{}`", path.display())
                    });
                }
            },
        }
        Ok(())
    }

    /// Return whether the adjacent script lock still matches `expected`.
    pub async fn script_lock_file_state_is_current(
        &self,
        expected: &ScriptLockFileState,
    ) -> miette::Result<bool> {
        Ok(&self.script_lock_file_state().await? == expected)
    }

    /// Reject work parsed from stale PEP 723 environment metadata.
    ///
    /// The fingerprint ignores Python source, TOML formatting, comments, and
    /// unrelated tool settings, none of which can change the resolved environment.
    pub async fn ensure_script_metadata_unchanged(&self) -> miette::Result<()> {
        if self.persistent_lock_file_path().is_none() {
            return Ok(());
        }
        let expected = self
            .script_manifest()
            .expect("script workspaces retain their parsed manifest")
            .environment_metadata_fingerprint()?;
        let path = &self.workspace.provenance.path;
        let contents = tokio::fs::read(path)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to read script `{}`", path.display()))?;
        let current = ScriptManifest::from_source(path, &contents)?
            .ok_or_else(|| miette::miette!("script no longer contains a PEP 723 metadata block"))?
            .environment_metadata_fingerprint()?;
        if current != expected {
            return Err(ScriptResolutionConflictError.into());
        }
        Ok(())
    }
}

impl ScriptResolutionStateGuard {
    /// Load the cached resolution. Any missing, corrupt, or unsupported state
    /// is a cache miss; normal resolution remains responsible for user-facing
    /// errors.
    pub async fn load(&self, workspace: &Workspace) -> Option<LockFile> {
        load_script_resolution(self.state_path.clone(), workspace).await
    }

    /// Atomically replace the cached resolution while holding the state lock.
    pub async fn store(&self, lock_file: &LockFile) -> miette::Result<()> {
        let lock_file = lock_file
            .render_to_string()
            .into_diagnostic()
            .wrap_err("failed to serialize the cached script resolution")?;
        let contents = serde_json::to_vec(&StoredScriptResolution {
            version: STATE_VERSION,
            lock_file,
        })
        .into_diagnostic()
        .wrap_err("failed to serialize the cached script resolution state")?;

        if let Some(parent) = self.state_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!(
                        "failed to create the cached script environment directory `{}`",
                        parent.display()
                    )
                })?;
        }

        pixi_utils::atomic_write::atomic_write_strict(&self.state_path, contents)
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!(
                    "failed to write cached script resolution to `{}`",
                    self.state_path.display()
                )
            })
    }

    /// Capture the exact hidden state, including absence or corrupt contents.
    pub async fn file_state(&self) -> miette::Result<ScriptResolutionFileState> {
        match tokio::fs::read(&self.state_path).await {
            Ok(contents) => Ok(ScriptResolutionFileState(Some(contents))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ScriptResolutionFileState(None))
            }
            Err(error) => Err(error)
                .into_diagnostic()
                .wrap_err("failed to read cached script resolution state"),
        }
    }

    /// Restore a previously captured hidden state while holding the guard.
    pub async fn restore_file_state(
        &self,
        state: &ScriptResolutionFileState,
    ) -> miette::Result<()> {
        if &self.file_state().await? == state {
            return Ok(());
        }
        match &state.0 {
            Some(contents) => {
                if let Some(parent) = self.state_path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .into_diagnostic()
                        .wrap_err("failed to restore cached script resolution state")?;
                }
                pixi_utils::atomic_write::atomic_write_strict(&self.state_path, contents)
                    .await
                    .into_diagnostic()
                    .wrap_err("failed to restore cached script resolution state")?;
            }
            None => match tokio::fs::remove_file(&self.state_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .into_diagnostic()
                        .wrap_err("failed to clear cached script resolution state");
                }
            },
        }
        Ok(())
    }

    /// Remove hidden resolution state so the next run must resolve again.
    pub async fn clear(&self) -> miette::Result<()> {
        self.restore_file_state(&ScriptResolutionFileState(None))
            .await
    }
}

async fn load_script_resolution(state_path: PathBuf, workspace: &Workspace) -> Option<LockFile> {
    let contents = match tokio::fs::read_to_string(&state_path).await {
        Ok(contents) => contents,
        Err(error) => {
            tracing::debug!(
                path = %state_path.display(),
                %error,
                "cached script resolution is unavailable"
            );
            return None;
        }
    };
    let stored: StoredScriptResolution = match serde_json::from_str(&contents) {
        Ok(stored) => stored,
        Err(error) => {
            tracing::debug!(
                path = %state_path.display(),
                %error,
                "cached script resolution is invalid"
            );
            return None;
        }
    };
    if stored.version != STATE_VERSION {
        tracing::debug!(
            path = %state_path.display(),
            version = stored.version,
            "cached script resolution has an unsupported version"
        );
        return None;
    }

    let lock_file =
        match LockFile::from_str_with_base_directory(&stored.lock_file, Some(workspace.root())) {
            Ok(lock_file) => lock_file,
            Err(error) => {
                tracing::debug!(
                    path = %state_path.display(),
                    %error,
                    "cached script resolution contains an invalid lock file"
                );
                return None;
            }
        };

    Some(crate::lock_file::align_platform_names(
        lock_file,
        workspace.workspace_manifest(),
        workspace.root(),
    ))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use pixi_config::{CacheConfig, Config};
    use pixi_manifest::{EnvironmentName, script::ScriptManifest};
    use rattler_conda_types::NamedChannelOrUrl;

    use super::*;

    fn script_workspace(root: &std::path::Path, cache: &std::path::Path) -> Workspace {
        let path = root.join("example.py");
        fs_err::write(
            &path,
            r#"# /// script
# dependencies = []
# ///
print("hello")
"#,
        )
        .unwrap();
        let script = ScriptManifest::from_path(path).unwrap().unwrap();
        Workspace::from_script(
            script,
            Config {
                default_channels: vec![NamedChannelOrUrl::Name("testing".into())],
                cache: CacheConfig {
                    exec_environments: Some(cache.to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap()
        .value
    }

    #[cfg(windows)]
    #[test]
    fn windows_script_identity_uses_ordinal_file_name_case() {
        assert!(
            windows_file_names_equal(std::ffi::OsStr::new("σ.py"), std::ffi::OsStr::new("ς.py"))
                .unwrap()
        );
    }

    #[test]
    fn physical_script_name_prefers_exact_matches_independent_of_entry_order() {
        let requested = std::ffi::OsStr::new("requested.py");
        for candidates in [
            vec!["alias.py", "requested.py"],
            vec!["requested.py", "alias.py"],
            vec!["first.py", "second.py", "requested.py"],
        ] {
            let selected = select_script_file_name(
                requested,
                candidates.into_iter().map(std::ffi::OsString::from),
            )
            .unwrap();
            assert_eq!(selected, requested);
        }

        let error = select_script_file_name(
            requested,
            ["first.py", "second.py"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn state_round_trips_and_invalid_state_is_a_cache_miss() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = script_workspace(root.path(), cache.path());
        let lock_path =
            script_resolution_lock_path(&script_resolution_identity(&workspace)).unwrap();
        let guard = workspace.acquire_script_resolution_state().await.unwrap();

        assert!(guard.load(&workspace).await.is_none());
        let absent_state = guard.file_state().await.unwrap();

        let lock_file = LockFile::default();
        guard.store(&lock_file).await.unwrap();
        let stored_state = guard.file_state().await.unwrap();
        let restored = guard.load(&workspace).await.unwrap();
        assert_eq!(
            restored.render_to_string().unwrap(),
            lock_file.render_to_string().unwrap()
        );
        let unlocked = tokio::time::timeout(
            Duration::from_secs(1),
            workspace.load_script_resolution_state(),
        )
        .await
        .expect("an atomic read should not wait for the publication guard")
        .unwrap();
        assert_eq!(
            unlocked.render_to_string().unwrap(),
            lock_file.render_to_string().unwrap()
        );

        guard.clear().await.unwrap();
        assert!(guard.load(&workspace).await.is_none());
        guard.restore_file_state(&stored_state).await.unwrap();
        assert!(guard.load(&workspace).await.is_some());
        guard.restore_file_state(&absent_state).await.unwrap();
        assert!(guard.load(&workspace).await.is_none());
        guard.restore_file_state(&stored_state).await.unwrap();

        let invalid_states = [
            "not json".to_owned(),
            serde_json::json!({
                "version": STATE_VERSION + 1,
                "lock_file": lock_file.render_to_string().unwrap(),
            })
            .to_string(),
            serde_json::json!({
                "version": STATE_VERSION,
                "lock_file": "not a lock file",
            })
            .to_string(),
        ];
        for contents in invalid_states {
            fs_err::write(&guard.state_path, contents).unwrap();
            assert!(guard.load(&workspace).await.is_none());
        }

        drop(guard);
        fs_err::remove_file(lock_path).unwrap();
    }

    #[tokio::test]
    async fn lockless_save_rejects_a_new_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = script_workspace(root.path(), cache.path());
        let script_path = workspace.workspace.provenance.path.clone();
        let source_before = fs_err::read(&script_path).unwrap();
        let sidecar = workspace.persistent_lock_file_path().unwrap();
        fs_err::write(&sidecar, "concurrent sidecar").unwrap();

        let error = workspace
            .modify()
            .unwrap()
            .save_and_clear_script_resolution()
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the script environment changed while it was being updated")
        );
        assert_eq!(fs_err::read(script_path).unwrap(), source_before);
        assert_eq!(fs_err::read(sidecar).unwrap(), b"concurrent sidecar");
    }

    #[tokio::test]
    async fn state_lock_survives_environment_cache_removal() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = Arc::new(script_workspace(root.path(), cache.path()));
        let lock_path =
            script_resolution_lock_path(&script_resolution_identity(&workspace)).unwrap();
        let first = workspace.acquire_script_resolution_state().await.unwrap();

        fs_err::create_dir_all(workspace.pixi_dir()).unwrap();
        fs_err::remove_dir_all(workspace.pixi_dir()).unwrap();

        let second_workspace = workspace.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut second = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            second_workspace.acquire_script_resolution_state().await
        });
        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second)
                .await
                .is_err(),
            "a second writer acquired the script state while the first still held it"
        );

        drop(first);
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the second writer remained blocked after the first released the lock")
            .unwrap()
            .unwrap();

        fs_err::remove_file(lock_path).unwrap();
    }

    #[tokio::test]
    async fn local_script_lock_is_shared_across_cache_roots() {
        let root = tempfile::tempdir().unwrap();
        let first_cache = tempfile::tempdir().unwrap();
        let second_cache = tempfile::tempdir().unwrap();
        let first = script_workspace(root.path(), first_cache.path());
        let second = Arc::new(script_workspace(root.path(), second_cache.path()));
        assert_ne!(first.pixi_dir(), second.pixi_dir());

        let lock_path = script_resolution_lock_path(&script_resolution_identity(&first)).unwrap();
        let first_guard = first.acquire_script_resolution_state().await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut waiter = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            second.acquire_script_resolution_state().await
        });
        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "different exec-cache roots bypassed the local script lock"
        );

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the second cache root remained blocked after publication")
            .unwrap()
            .unwrap();
        fs_err::remove_file(lock_path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_script_lock_is_shared_across_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        fs_err::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let first_cache = tempfile::tempdir().unwrap();
        let second_cache = tempfile::tempdir().unwrap();
        let first = script_workspace(&real, first_cache.path());
        let second = Arc::new(script_workspace(&alias, second_cache.path()));

        let lock_path = script_resolution_lock_path(&script_resolution_identity(&first)).unwrap();
        let first_guard = first.acquire_script_resolution_state().await.unwrap();
        let mut waiter =
            tokio::spawn(async move { second.acquire_script_resolution_state().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "a directory symlink bypassed the physical script lock"
        );

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the aliased script remained blocked after publication")
            .unwrap()
            .unwrap();
        fs_err::remove_file(lock_path).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn local_script_lock_is_shared_across_case_aliases() {
        let root = tempfile::tempdir().unwrap();
        let first_cache = tempfile::tempdir().unwrap();
        let first = script_workspace(root.path(), first_cache.path());
        let alias = root.path().join("EXAMPLE.PY");
        if fs_err::symlink_metadata(&alias).is_err() {
            // This filesystem is case-sensitive, so there is no alias to test.
            return;
        }

        let second_cache = tempfile::tempdir().unwrap();
        let script = ScriptManifest::from_path(alias).unwrap().unwrap();
        let second = Arc::new(
            Workspace::from_script(
                script,
                Config {
                    default_channels: vec![NamedChannelOrUrl::Name("testing".into())],
                    cache: CacheConfig {
                        exec_environments: Some(second_cache.path().to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap()
            .value,
        );
        assert_eq!(
            script_resolution_identity(&first),
            script_resolution_identity(&second)
        );

        let lock_path = script_resolution_lock_path(&script_resolution_identity(&first)).unwrap();
        let first_guard = first.acquire_script_resolution_state().await.unwrap();
        let mut waiter =
            tokio::spawn(async move { second.acquire_script_resolution_state().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "a case alias bypassed the physical script lock"
        );

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the case-aliased script remained blocked after publication")
            .unwrap()
            .unwrap();
        fs_err::remove_file(lock_path).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn local_script_lock_is_stable_across_hard_link_replacement() {
        let root = tempfile::tempdir().unwrap();
        let original_cache = tempfile::tempdir().unwrap();
        let original = script_workspace(root.path(), original_cache.path());
        let original_path = original.script_manifest().unwrap().path();
        let alias = root.path().join("alias.py");
        fs_err::hard_link(original_path, &alias).unwrap();
        let source = fs_err::read_to_string(original_path).unwrap();

        let make_workspace = |cache: &std::path::Path| {
            let script = ScriptManifest::from_path(&alias).unwrap().unwrap();
            Workspace::from_script(
                script,
                Config {
                    default_channels: vec![NamedChannelOrUrl::Name("testing".into())],
                    cache: CacheConfig {
                        exec_environments: Some(cache.to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap()
            .value
        };

        let first_cache = tempfile::tempdir().unwrap();
        let first = make_workspace(first_cache.path());
        let identity = script_resolution_identity(&first);
        assert_eq!(
            identity.file_name().unwrap(),
            std::ffi::OsStr::new("alias.py.pixi.lock")
        );
        let lock_path = script_resolution_lock_path(&identity).unwrap();
        let first_guard = first.acquire_script_resolution_state().await.unwrap();

        pixi_utils::atomic_write::atomic_write_sync_strict(&alias, source).unwrap();
        let second_cache = tempfile::tempdir().unwrap();
        let second = Arc::new(make_workspace(second_cache.path()));
        assert_eq!(identity, script_resolution_identity(&second));
        let mut waiter =
            tokio::spawn(async move { second.acquire_script_resolution_state().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "replacing a hard-link alias changed the script lock identity"
        );

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the replaced hard-link alias remained blocked after publication")
            .unwrap()
            .unwrap();
        fs_err::remove_file(lock_path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_script_lock_is_stable_when_a_file_symlink_is_retargeted() {
        let root = tempfile::tempdir().unwrap();
        let first_target = root.path().join("first.py");
        let second_target = root.path().join("second.py");
        let link = root.path().join("example.py");
        let source = "# /// script\n# dependencies = []\n# ///\nprint('hello')\n";
        fs_err::write(&first_target, source).unwrap();
        fs_err::write(&second_target, source).unwrap();
        std::os::unix::fs::symlink(&first_target, &link).unwrap();

        let make_workspace = |cache: &std::path::Path| {
            let script = ScriptManifest::from_path(&link).unwrap().unwrap();
            Workspace::from_script(
                script,
                Config {
                    default_channels: vec![NamedChannelOrUrl::Name("testing".into())],
                    cache: CacheConfig {
                        exec_environments: Some(cache.to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap()
            .value
        };

        let first_cache = tempfile::tempdir().unwrap();
        let first = make_workspace(first_cache.path());
        let lock_path = script_resolution_lock_path(&script_resolution_identity(&first)).unwrap();
        let first_guard = first.acquire_script_resolution_state().await.unwrap();

        fs_err::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second_target, &link).unwrap();
        let second_cache = tempfile::tempdir().unwrap();
        let second = Arc::new(make_workspace(second_cache.path()));
        let mut waiter =
            tokio::spawn(async move { second.acquire_script_resolution_state().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "retargeting a script symlink changed the sidecar coordination identity"
        );

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the retargeted script remained blocked after publication")
            .unwrap()
            .unwrap();
        fs_err::remove_file(lock_path).unwrap();
    }

    #[test]
    fn redirecting_a_script_workspace_resets_activation_state() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = script_workspace(root.path(), cache.path());
        let original_env_vars = workspace
            .env_vars
            .get(&EnvironmentName::default())
            .expect("script workspaces have a default environment");
        let redirected = workspace
            .clone()
            .with_script_pixi_dir(root.path().join("disposable"));
        let redirected_env_vars = redirected
            .env_vars
            .get(&EnvironmentName::default())
            .expect("redirected scripts retain their default environment");

        assert!(!Arc::ptr_eq(
            original_env_vars.clean(),
            redirected_env_vars.clean()
        ));
        assert!(!Arc::ptr_eq(
            original_env_vars.pixi_only(),
            redirected_env_vars.pixi_only()
        ));
        assert!(!Arc::ptr_eq(
            original_env_vars.full(),
            redirected_env_vars.full()
        ));
    }
}
