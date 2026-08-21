use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Read one regular file without allowing its contents to grow memory use
/// beyond `max_bytes`.  On Unix, `O_NOFOLLOW` also closes the
/// check-then-open symlink race rather than relying only on path metadata.
pub fn read_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing to read non-regular file: {}", path.display());
    }
    if metadata.len() > max_bytes as u64 {
        bail!("file exceeds {max_bytes} byte limit: {}", path.display());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("could not inspect opened file {}", path.display()))?;
    if !opened.is_file() {
        bail!("refusing to read non-regular file: {}", path.display());
    }
    if opened.len() > max_bytes as u64 {
        bail!("file exceeds {max_bytes} byte limit: {}", path.display());
    }

    // Do not trust metadata alone: a concurrently-written file can grow
    // after fstat. Reading one sentinel byte lets us reject that case without
    // allocating the unbounded remainder.
    let take_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(max_bytes));
    file.take(take_limit).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        bail!(
            "file exceeds {max_bytes} byte limit while reading: {}",
            path.display()
        );
    }
    Ok(bytes)
}

pub fn read_string_bounded(path: &Path, max_bytes: usize) -> Result<String> {
    String::from_utf8(read_bounded(path, max_bytes)?)
        .with_context(|| format!("file is not valid UTF-8: {}", path.display()))
}

/// Ensure that `path` is a real directory.
///
/// Missing components are created with `mode`, but permissions on directories
/// that already existed are deliberately preserved.  This is the appropriate
/// operation for external parents such as `/etc/frr` and `/var/lock`.
pub fn ensure_dir(path: &Path, mode: u32) -> Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("{} is not a real directory", cursor.display());
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor.clone());
                let Some(parent) = cursor.parent() else {
                    bail!("{} has no existing directory ancestor", path.display());
                };
                cursor = parent.to_path_buf();
            }
            Err(error) => return Err(error.into()),
        }
    }

    for directory in missing.into_iter().rev() {
        fs::create_dir(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        set_mode(&directory, mode)?;
        sync_dir(&directory)?;
        if let Some(parent) = directory.parent() {
            sync_dir(parent)?;
        }
    }
    reject_symlink(path)?;
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    sync_dir(path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Create or validate a private directory and enforce its mode even when it
/// already existed.  Callers must use this only for directories they own.
pub fn ensure_private_dir(path: &Path, mode: u32) -> Result<()> {
    ensure_dir(path, mode)?;
    set_mode(path, mode)?;
    sync_dir(path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Confirm an external directory without creating it or changing its mode.
pub fn confirm_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("required parent directory is missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{} is not a real directory", path.display());
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<bool> {
    let parent = path
        .parent()
        .context("atomic target has no parent directory")?;
    confirm_dir(parent)?;
    reject_symlink(path)?;
    cleanup_atomic_temps(path)?;

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                bail!("refusing to replace non-regular file: {}", path.display());
            }
            if metadata.len() == bytes.len() as u64 && read_bounded(path, bytes.len())? == bytes {
                set_mode(path, mode)?;
                sync_file(path)?;
                sync_dir(parent)?;
                return Ok(false);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let nonce = random_nonce();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("atomic target has a non-UTF-8 filename")?;
    let temporary = parent.join(format!(".{name}.meduza-{nonce}"));
    reject_existing(&temporary)?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(mode);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_mode(&temporary, mode)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&temporary, path)
        .with_context(|| format!("could not publish {}", path.display()))?;
    sync_dir(parent)?;
    Ok(true)
}

/// Remove only interrupted temporary siblings created by `atomic_write` for
/// this exact target. Unknown objects and symlinks fail closed.
pub fn cleanup_atomic_temps(path: &Path) -> Result<usize> {
    let parent = path.parent().context("atomic target has no parent")?;
    confirm_dir(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("atomic target has a non-UTF-8 filename")?;
    let prefix = format!(".{name}.meduza-");
    let mut removed = 0;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let candidate = entry.file_name();
        let Some(candidate) = candidate.to_str() else {
            continue;
        };
        let Some(nonce) = candidate.strip_prefix(&prefix) else {
            continue;
        };
        if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "invalid atomic temporary object: {}",
                entry.path().display()
            );
        }
        fs::remove_file(entry.path())?;
        removed += 1;
    }
    if removed > 0 {
        sync_dir(parent)?;
    }
    Ok(removed)
}

pub fn atomic_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<bool> {
    let bytes = serde_json::to_vec(value)?;
    atomic_write(path, &bytes, 0o600)
}

pub fn atomic_json_bounded<T: serde::Serialize>(
    path: &Path,
    value: &T,
    max_bytes: usize,
) -> Result<bool> {
    let mut output = BoundedBuffer::new(max_bytes);
    serde_json::to_writer(&mut output, value).with_context(|| {
        format!(
            "could not serialize state within {max_bytes} byte limit: {}",
            path.display()
        )
    })?;
    atomic_write(path, &output.bytes, 0o600)
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BoundedBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if buffer.len() > remaining {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "serialized state exceeds configured byte limit",
            ));
        }
        self.bytes
            .try_reserve_exact(buffer.len())
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!("could not reserve state buffer: {error}"),
                )
            })?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn durable_remove(path: &Path) -> Result<bool> {
    let Some(parent) = path.parent() else {
        bail!("path has no parent")
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if parent.is_dir() {
                sync_dir(parent)?;
            }
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing to unlink non-regular file: {}", path.display());
    }
    fs::remove_file(path)?;
    sync_dir(parent)?;
    Ok(true)
}

pub fn durable_rename(source: &Path, target: &Path) -> Result<()> {
    reject_symlink(source)?;
    reject_existing(target)?;
    let parent = source.parent().context("source has no parent")?;
    if target.parent() != Some(parent) {
        bail!("durable rename must stay in one directory");
    }
    fs::rename(source, target)?;
    sync_dir(parent)
}

pub fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing symlink path: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to replace unexpected path: {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let file =
            File::open(path).with_context(|| format!("could not open {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("could not fsync {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn sync_file(path: &Path) -> Result<()> {
    // Windows requires a writable handle for FlushFileBuffers; Linux accepts
    // this as well.  The caller has just enforced the file's requested mode.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("could not fsync {}", path.display()))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

pub fn random_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn rooted(root: Option<&Path>, absolute: &str) -> PathBuf {
    match root {
        Some(root) => root.join(absolute.trim_start_matches('/')),
        None => PathBuf::from(absolute),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_is_atomic_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("private");
        ensure_private_dir(&parent, 0o700).unwrap();
        let path = parent.join("state.json");
        assert!(atomic_write(&path, b"one", 0o600).unwrap());
        assert!(!atomic_write(&path, b"one", 0o600).unwrap());
        assert!(atomic_write(&path, b"two", 0o600).unwrap());
        assert_eq!(fs::read(path).unwrap(), b"two");
    }

    #[test]
    fn interrupted_atomic_temporary_is_cleaned_without_touching_foreign_files() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("private");
        ensure_private_dir(&parent, 0o700).unwrap();
        let target = parent.join("state.json");
        let interrupted = parent.join(format!(".state.json.meduza-{}", "a".repeat(32)));
        let foreign = parent.join(".state.json.meduza-not-a-nonce");
        fs::write(&interrupted, b"partial").unwrap();
        fs::write(&foreign, b"operator").unwrap();

        assert_eq!(cleanup_atomic_temps(&target).unwrap(), 1);
        assert!(!interrupted.exists());
        assert_eq!(fs::read(foreign).unwrap(), b"operator");
    }

    #[test]
    fn bounded_read_rejects_oversized_files_without_returning_a_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::write(&path, b"12345").unwrap();

        assert_eq!(read_bounded(&path, 5).unwrap(), b"12345");
        assert!(read_bounded(&path, 4).is_err());
    }

    #[test]
    fn bounded_json_rejects_oversized_serialization_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("private");
        ensure_private_dir(&parent, 0o700).unwrap();
        let path = parent.join("state.json");

        assert!(atomic_json_bounded(&path, &"12345", 6).is_err());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_does_not_follow_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::write(&target, b"secret").unwrap();
        symlink(&target, &link).unwrap();

        assert!(read_bounded(&link, 64).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_external_parent_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("external");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&parent.join("state"), b"value", 0o600).unwrap();

        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_enforces_mode_without_changing_ancestor() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let private = temp.path().join("one/two");

        ensure_private_dir(&private, 0o700).unwrap();

        assert_eq!(
            fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
