use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::CoreError;

/// Maximum size accepted for repository-controlled manifests and configuration.
pub const PROJECT_METADATA_MAX_BYTES: u64 = 1024 * 1024;

/// Read an optional UTF-8 regular file after proving that it remains within
/// `root` and does not exceed `max_bytes`.
///
/// A missing file is returned as `None`. Existing directories, devices, and
/// symlinks that resolve outside the canonical project root are rejected.
///
/// # Errors
///
/// Returns a typed error when either path cannot be inspected, the target is
/// unsafe or too large, reading fails, or the contents are not valid UTF-8.
pub fn read_optional_bounded_utf8_file_within(
    root: &Path,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<String>, CoreError> {
    resolve_optional_regular_file_within(root, path)?.map_or(Ok(None), |canonical_path| {
        read_bounded_utf8_file_at(&canonical_path, path, max_bytes, Some(root)).map(Some)
    })
}

/// Resolve an optional repository-controlled path after proving it is a
/// regular file contained by the canonical project root.
///
/// # Errors
///
/// Returns a typed error when either path cannot be inspected, the target is
/// not a regular file, or the resolved target escapes `root`.
pub fn resolve_optional_regular_file_within(root: &Path, path: &Path) -> Result<Option<PathBuf>, CoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CoreError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    }
    let canonical_root = canonical_root(root)?;
    let canonical_path = path.canonicalize().map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: format!(
                "resolved path {} escapes project root {}",
                canonical_path.display(),
                canonical_root.display()
            ),
        });
    }
    let metadata = fs::metadata(&canonical_path).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: "expected a regular file".to_string(),
        });
    }
    Ok(Some(canonical_path))
}

/// Read a UTF-8 regular file after proving that it remains within `root` and
/// does not exceed `max_bytes`.
///
/// # Errors
///
/// Returns a typed error when either path cannot be inspected, the target is
/// unsafe or too large, reading fails, or the contents are not valid UTF-8.
pub fn read_bounded_utf8_file_within(root: &Path, path: &Path, max_bytes: u64) -> Result<String, CoreError> {
    let canonical_path =
        resolve_optional_regular_file_within(root, path)?.ok_or_else(|| CoreError::Read {
            path: path.display().to_string(),
            source: std::io::Error::new(ErrorKind::NotFound, "file does not exist"),
        })?;
    read_bounded_utf8_file_at(&canonical_path, path, max_bytes, Some(root))
}

/// Read a bounded UTF-8 regular file selected explicitly by the caller.
///
/// Unlike [`read_bounded_utf8_file_within`], this permits a path outside a
/// project root. It is intended for explicit user-selected inputs, not files
/// discovered from an untrusted repository.
///
/// # Errors
///
/// Returns a typed error when the path cannot be resolved, is not a regular
/// file, is too large, cannot be read, or is not valid UTF-8.
pub fn read_bounded_utf8_file(path: &Path, max_bytes: u64) -> Result<String, CoreError> {
    let canonical_path = path.canonicalize().map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    read_bounded_utf8_file_at(&canonical_path, path, max_bytes, None)
}

fn canonical_root(root: &Path) -> Result<PathBuf, CoreError> {
    let canonical = root.canonicalize().map_err(|source| CoreError::Read {
        path: root.display().to_string(),
        source,
    })?;
    let metadata = fs::metadata(&canonical).map_err(|source| CoreError::Read {
        path: root.display().to_string(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(CoreError::InvalidRoot {
            path: root.display().to_string(),
            message: "not a directory".to_string(),
        });
    }
    Ok(canonical)
}

fn read_bounded_utf8_file_at(
    canonical_path: &Path,
    display_path: &Path,
    max_bytes: u64,
    anchor_root: Option<&Path>,
) -> Result<String, CoreError> {
    let path_metadata = fs::metadata(canonical_path).map_err(|source| CoreError::Read {
        path: display_path.display().to_string(),
        source,
    })?;
    reject_non_regular(display_path, &path_metadata)?;
    reject_oversized(display_path, path_metadata.len(), max_bytes)?;
    #[cfg(unix)]
    let expected_identity = file_identity(&path_metadata);
    #[cfg(not(unix))]
    let expected_identity =
        same_file::Handle::from_path(canonical_path).map_err(|source| CoreError::Read {
            path: display_path.display().to_string(),
            source,
        })?;

    let file = open_for_bounded_read(canonical_path, anchor_root).map_err(|source| CoreError::Read {
        path: display_path.display().to_string(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| CoreError::Read {
        path: display_path.display().to_string(),
        source,
    })?;
    reject_non_regular(display_path, &metadata)?;
    reject_oversized(display_path, metadata.len(), max_bytes)?;
    #[cfg(unix)]
    let opened_identity = file_identity(&metadata);
    #[cfg(not(unix))]
    let opened_identity =
        same_file::Handle::from_file(file.try_clone().map_err(|source| CoreError::Read {
            path: display_path.display().to_string(),
            source,
        })?)
        .map_err(|source| CoreError::Read {
            path: display_path.display().to_string(),
            source,
        })?;
    if expected_identity != opened_identity {
        return Err(CoreError::UnsafePath {
            path: display_path.display().to_string(),
            message: "file changed while it was being opened".to_string(),
        });
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| CoreError::Read {
            path: display_path.display().to_string(),
            source,
        })?;
    let observed_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    reject_oversized(display_path, observed_size, max_bytes)?;
    String::from_utf8(bytes).map_err(|error| CoreError::Parse {
        path: display_path.display().to_string(),
        message: format!("file is not valid UTF-8: {error}"),
    })
}

fn open_for_bounded_read(path: &Path, anchor_root: Option<&Path>) -> std::io::Result<File> {
    #[cfg(unix)]
    if let Some(root) = anchor_root {
        return open_beneath(root, path);
    }
    #[cfg(not(unix))]
    let _ = anchor_root;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

#[cfg(unix)]
fn open_beneath(root: &Path, canonical_path: &Path) -> std::io::Result<File> {
    use std::path::Component;

    use rustix::fs::{open, openat, Mode, OFlags};

    let canonical_root = root.canonicalize()?;
    let relative = canonical_path.strip_prefix(&canonical_root).map_err(|_| {
        std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{} is outside anchored root {}",
                canonical_path.display(),
                canonical_root.display()
            ),
        )
    })?;
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("unsafe relative path component in {}", relative.display()),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "repository metadata path resolves to the project root",
        ));
    }

    let directory_flags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mut descriptor = open(&canonical_root, directory_flags, Mode::empty())?;
    for (index, component) in components.iter().enumerate() {
        let flags = if index + 1 == components.len() {
            file_flags
        } else {
            directory_flags
        };
        descriptor = openat(&descriptor, *component, flags, Mode::empty())?;
    }
    Ok(File::from(descriptor))
}

#[cfg(unix)]
type FileIdentity = (u64, u64);

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

fn reject_non_regular(path: &Path, metadata: &fs::Metadata) -> Result<(), CoreError> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: "expected a regular file".to_string(),
        })
    }
}

fn reject_oversized(path: &Path, size: u64, max_bytes: u64) -> Result<(), CoreError> {
    if size > max_bytes {
        Err(CoreError::FileTooLarge {
            path: path.display().to_string(),
            size,
            max_bytes,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn sparse_oversized_file_is_rejected_before_reading() {
        let root = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
        let path = root.path().join("package.json");
        let file = File::create(&path).unwrap_or_else(|error| panic!("manifest: {error}"));
        file.set_len(PROJECT_METADATA_MAX_BYTES + 1)
            .unwrap_or_else(|error| panic!("sparse length: {error}"));

        let Err(error) = read_bounded_utf8_file_within(root.path(), &path, PROJECT_METADATA_MAX_BYTES) else {
            panic!("oversized fixture must be rejected");
        };
        assert!(matches!(error, CoreError::FileTooLarge { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap_or_else(|error| panic!("root: {error}"));
        let outside = TempDir::new().unwrap_or_else(|error| panic!("outside: {error}"));
        let target = outside.path().join("package.json");
        fs::write(&target, "{}\n").unwrap_or_else(|error| panic!("target: {error}"));
        let link = root.path().join("package.json");
        symlink(&target, &link).unwrap_or_else(|error| panic!("symlink: {error}"));

        let Err(error) = read_bounded_utf8_file_within(root.path(), &link, PROJECT_METADATA_MAX_BYTES) else {
            panic!("escaping symlink must be rejected");
        };
        assert!(matches!(error, CoreError::UnsafePath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_regular_file_inside_root_is_opened_beneath_the_anchor() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap_or_else(|error| panic!("root: {error}"));
        let metadata = root.path().join("metadata");
        fs::create_dir(&metadata).unwrap_or_else(|error| panic!("metadata directory: {error}"));
        let target = metadata.join("package.json");
        fs::write(&target, "{\"name\":\"inside\"}\n").unwrap_or_else(|error| panic!("target: {error}"));
        let link = root.path().join("package.json");
        symlink(&target, &link).unwrap_or_else(|error| panic!("symlink: {error}"));

        let contents = read_bounded_utf8_file_within(root.path(), &link, PROJECT_METADATA_MAX_BYTES)
            .unwrap_or_else(|error| panic!("contained symlink: {error}"));
        assert!(contents.contains("inside"));
    }
}
