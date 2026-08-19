use fs_err::tokio as tokio_fs;
use std::path::Path;

/// Build a [`tempfile::NamedTempFile`] in the same directory as `path`, using
/// the original filename as the prefix so the temp file is easily identifiable
/// (e.g. `.pixi.toml.XXXXXX`).
///
/// On Unix, `_perms` is forwarded to [`tempfile::Builder::permissions`] so the
/// temp file is created with the correct mode from the start.
fn temp_file_for(
    path: &Path,
    _perms: Option<std::fs::Permissions>,
) -> std::io::Result<tempfile::NamedTempFile> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;

    let prefix = format!(
        ".{}.",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp")
    );

    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // tempfile defaults regular files to 0o600. For a new destination we
        // want the same umask-derived mode as File::create (0o666 & umask),
        // while rewrites start no more permissively than the existing file.
        builder.permissions(
            _perms
                .clone()
                .unwrap_or_else(|| std::fs::Permissions::from_mode(0o666)),
        );
    }
    let temp_file = builder.tempfile_in(dir)?;
    #[cfg(unix)]
    if let Some(perms) = _perms {
        // `open(2)` still applies the current umask to Builder's requested
        // mode. Restore an existing destination's exact mode before the
        // temporary inode can be published by rename.
        fs_err::set_permissions(temp_file.path(), perms)?;
    }
    Ok(temp_file)
}

/// Return the permissions of an existing file at `path`, or `None` if the file
/// does not exist.  On non-Unix platforms always returns `None`.
///
/// Read *before* the temp file is created so the correct mode can be passed to
/// [`tempfile::Builder::permissions`] at construction time.
#[cfg(unix)]
fn original_permissions(path: &Path) -> std::io::Result<Option<std::fs::Permissions>> {
    match fs_err::metadata(path) {
        Ok(m) => Ok(Some(m.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn original_permissions(_path: &Path) -> std::io::Result<Option<std::fs::Permissions>> {
    Ok(None)
}

/// Atomically write contents to a file by first writing to a temporary file and
/// then renaming it to the target path.
///
/// This ensures that the target file is never left in a partially-written state.
/// If the write fails (e.g., due to disk full), the original file remains
/// untouched.
///
/// On Unix the permissions of the existing file are preserved across the
/// rewrite.  The correct mode is set via [`tempfile::Builder::permissions`] at
/// temp-file creation time so the file never exists with the wrong permissions.
pub async fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let perms = original_permissions(path)?;

    let temp_file = match temp_file_for(path, perms) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                path = %path.display(),
                "cannot create temp file in parent directory; falling back to direct write. \
                Write will not be atomic."
            );
            return tokio_fs::write(path, contents.as_ref()).await;
        }
        Err(e) => return Err(e),
    };

    let temp_path = temp_file.into_temp_path();
    tokio_fs::write(&temp_path, contents.as_ref()).await?;

    temp_path.persist(path).map_err(|e| e.error)?;

    Ok(())
}

/// Atomically replace `path`, returning an error instead of falling back to a
/// truncating write when its parent cannot host a temporary file.
pub async fn atomic_write_strict(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let perms = original_permissions(path)?;
    let temp_file = temp_file_for(path, perms)?;
    let temp_path = temp_file.into_temp_path();
    tokio_fs::write(&temp_path, contents.as_ref()).await?;
    temp_path
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

/// Synchronous version of [`atomic_write`].
pub fn atomic_write_sync(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let perms = original_permissions(path)?;

    let mut temp_file = match temp_file_for(path, perms) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                path = %path.display(),
                "cannot create temp file in parent directory; falling back to direct write. \
                Write will not be atomic."
            );
            return fs_err::write(path, contents.as_ref());
        }
        Err(e) => return Err(e),
    };
    std::io::Write::write_all(&mut temp_file, contents.as_ref())?;

    temp_file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Synchronous strict variant of [`atomic_write_strict`].
pub fn atomic_write_sync_strict(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let perms = original_permissions(path)?;
    let mut temp_file = temp_file_for(path, perms)?;
    std::io::Write::write_all(&mut temp_file, contents.as_ref())?;
    temp_file
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temp_file_created_in_same_dir_when_writable() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");

        let temp = temp_file_for(&target, None).unwrap();

        assert_eq!(temp.path().parent().unwrap(), dir.path());
    }

    #[test]
    fn test_temp_file_has_correct_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");

        let temp = temp_file_for(&target, None).unwrap();
        let name = temp.path().file_name().unwrap().to_str().unwrap();

        assert!(
            name.starts_with(".pixi.toml."),
            "expected prefix `.pixi.toml.`, got `{name}`"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_atomic_write_new_file_uses_regular_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("reference");
        let target = dir.path().join("pixi.lock");
        std::fs::File::create(&reference).unwrap();

        atomic_write_sync_strict(&target, b"version: 1\n").unwrap();

        assert_eq!(
            fs_err::metadata(&target).unwrap().permissions().mode() & 0o777,
            fs_err::metadata(&reference).unwrap().permissions().mode() & 0o777,
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_atomic_write_preserves_permissions_under_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD_PATH: &str = "PIXI_ATOMIC_WRITE_UMASK_TARGET";
        if let Some(target) = std::env::var_os(CHILD_PATH) {
            atomic_write_sync_strict(Path::new(&target), b"updated\n").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.lock");
        fs_err::write(&target, b"original\n").unwrap();
        fs_err::set_permissions(&target, std::fs::Permissions::from_mode(0o664)).unwrap();

        let status = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "umask 077; exec \"$1\" --exact atomic_write::tests::test_atomic_write_preserves_permissions_under_restrictive_umask --nocapture",
                "sh",
            ])
            .arg(std::env::current_exe().unwrap())
            .env(CHILD_PATH, &target)
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(
            fs_err::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o664,
        );
    }

    /// Integration test: when the parent directory is read-only, `atomic_write`
    /// should fall back to a direct write and the file contents must be correct.
    ///
    /// Note: on Unix, a read-only directory still allows writing to existing
    /// files within it (controlled by the file's own permissions), so the
    /// fallback `tokio_fs::write` succeeds even though rename cannot.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_atomic_write_falls_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");
        let contents = b"[project]\nname = \"test\"";

        tokio_fs::write(&target, b"").await.unwrap();
        tokio_fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
            .await
            .unwrap();

        atomic_write(&target, contents).await.unwrap();

        let written = tokio_fs::read(&target).await.unwrap();
        assert_eq!(written, contents);

        // Reset permissions for clean up
        tokio_fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn test_atomic_write_sync_falls_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");
        let contents = b"[project]\nname = \"test\"";

        fs_err::write(&target, b"").unwrap();
        fs_err::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        atomic_write_sync(&target, contents).unwrap();

        let written = fs_err::read(&target).unwrap();
        assert_eq!(written, contents);

        // Reset permissions for clean up
        fs_err::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `atomic_write` must not change the mode of an existing file.
    /// This is the regression test for https://github.com/prefix-dev/pixi/issues/6295 —
    /// `project version set` was silently downgrading pixi.toml from 0644 → 0600.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_atomic_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");
        let original = b"[workspace]\nversion = \"1.0.0\"\n";
        let updated = b"[workspace]\nversion = \"1.2.3\"\n";

        // Create file with explicit 0o644 permissions (world-readable).
        tokio_fs::write(&target, original).await.unwrap();
        tokio_fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();

        atomic_write(&target, updated).await.unwrap();

        let mode = tokio_fs::metadata(&target)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o644,
            "atomic_write must not change file permissions (got {mode:#o})"
        );
        assert_eq!(tokio_fs::read(&target).await.unwrap(), updated);
    }

    /// Same regression test for the synchronous path.
    #[test]
    #[cfg(unix)]
    fn test_atomic_write_sync_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");
        let original = b"[workspace]\nversion = \"1.0.0\"\n";
        let updated = b"[workspace]\nversion = \"1.2.3\"\n";

        fs_err::write(&target, original).unwrap();
        fs_err::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write_sync(&target, updated).unwrap();

        let mode = fs_err::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "atomic_write_sync must not change file permissions (got {mode:#o})"
        );
        assert_eq!(fs_err::read(&target).unwrap(), updated);
    }

    /// Verify that non-standard permissions (e.g. 0o600) on existing files are
    /// also faithfully preserved — atomic_write must not normalise them to 0644.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_atomic_write_preserves_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pixi.toml");

        tokio_fs::write(&target, b"original\n").await.unwrap();
        tokio_fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();

        atomic_write(&target, b"updated\n").await.unwrap();

        let mode = tokio_fs::metadata(&target)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "atomic_write must preserve 0o600 when that was the original mode (got {mode:#o})"
        );
    }
}
