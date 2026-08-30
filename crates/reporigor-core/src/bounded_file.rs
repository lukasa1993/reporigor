use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::path_io::CoreIoResult;
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
    if !path_exists(path)? {
        return Ok(None);
    }
    let canonical_path = resolve_contained_path(root, path)?;
    validate_regular_path(path, &canonical_path)?;
    Ok(Some(canonical_path))
}

fn path_exists(path: &Path) -> Result<bool, CoreError> {
    Ok(optional_symlink_metadata(path)?.is_some())
}

pub(crate) fn optional_symlink_metadata(path: &Path) -> Result<Option<fs::Metadata>, CoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn resolve_contained_path(root: &Path, path: &Path) -> Result<PathBuf, CoreError> {
    let canonical_root = canonical_directory(root)?;
    let canonical_path = path.canonicalize().for_read_path(path)?;
    ensure_path_within(path, &canonical_path, &canonical_root)?;
    Ok(canonical_path)
}

fn ensure_path_within(path: &Path, canonical_path: &Path, canonical_root: &Path) -> Result<(), CoreError> {
    if !canonical_path.starts_with(canonical_root) {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: format!(
                "resolved path {} escapes project root {}",
                canonical_path.display(),
                canonical_root.display()
            ),
        });
    }
    Ok(())
}

fn validate_regular_path(path: &Path, canonical_path: &Path) -> Result<(), CoreError> {
    let metadata = fs::metadata(canonical_path).for_read_path(path)?;
    reject_non_regular(path, &metadata)
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
    let canonical_path = path.canonicalize().for_read_path(path)?;
    read_bounded_utf8_file_at(&canonical_path, path, max_bytes, None)
}

/// Resolve a directory path to its canonical form and reject non-directories.
///
/// # Errors
///
/// Returns a typed read or invalid-root error when resolution fails.
pub fn canonical_directory(root: &Path) -> Result<PathBuf, CoreError> {
    let canonical = root.canonicalize().for_read_path(root)?;
    let metadata = fs::metadata(&canonical).for_read_path(root)?;
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
    let expected_identity =
        inspect_file_identity(FileIdentitySource::Path(canonical_path), display_path, max_bytes)?;
    let file = open_for_bounded_read(canonical_path, anchor_root).for_read_path(display_path)?;
    let opened_identity = inspect_file_identity(FileIdentitySource::Opened(&file), display_path, max_bytes)?;
    ensure_unchanged_identity(display_path, &expected_identity, &opened_identity)?;
    read_bounded_contents(file, display_path, max_bytes)
}

#[derive(Clone, Copy)]
enum FileIdentitySource<'a> {
    Path(&'a Path),
    Opened(&'a File),
}

fn inspect_file_identity(
    source: FileIdentitySource<'_>,
    display_path: &Path,
    max_bytes: u64,
) -> Result<FileIdentity, CoreError> {
    source.inspect(display_path, max_bytes)
}

impl FileIdentitySource<'_> {
    fn inspect(self, display_path: &Path, max_bytes: u64) -> Result<FileIdentity, CoreError> {
        let metadata = self.metadata().for_read_path(display_path)?;
        validate_bounded_metadata(display_path, &metadata, max_bytes)?;
        #[cfg(unix)]
        let identity = self.identity(display_path, &metadata);
        #[cfg(not(unix))]
        let identity = self.identity(display_path, &metadata)?;
        Ok(identity)
    }

    fn metadata(self) -> std::io::Result<fs::Metadata> {
        match self {
            Self::Path(path) => fs::metadata(path),
            Self::Opened(file) => file.metadata(),
        }
    }

    #[cfg(unix)]
    fn identity(self, display_path: &Path, metadata: &fs::Metadata) -> FileIdentity {
        let _ = (self, display_path);
        file_identity(metadata)
    }

    #[cfg(not(unix))]
    fn identity(self, display_path: &Path, _metadata: &fs::Metadata) -> Result<FileIdentity, CoreError> {
        match self {
            Self::Path(path) => same_file::Handle::from_path(path).for_read_path(display_path),
            Self::Opened(file) => {
                let clone = file.try_clone().for_read_path(display_path)?;
                same_file::Handle::from_file(clone).for_read_path(display_path)
            }
        }
    }
}

fn validate_bounded_metadata(path: &Path, metadata: &fs::Metadata, max_bytes: u64) -> Result<(), CoreError> {
    reject_non_regular(path, metadata)?;
    reject_oversized(path, metadata.len(), max_bytes)
}

fn ensure_unchanged_identity(
    display_path: &Path,
    expected_identity: &FileIdentity,
    opened_identity: &FileIdentity,
) -> Result<(), CoreError> {
    if expected_identity != opened_identity {
        return Err(CoreError::UnsafePath {
            path: display_path.display().to_string(),
            message: "file changed while it was being opened".to_string(),
        });
    }
    Ok(())
}

fn read_bounded_contents(file: File, display_path: &Path, max_bytes: u64) -> Result<String, CoreError> {
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .for_read_path(display_path)?;
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
    let canonical_root = root.canonicalize()?;
    let relative = anchored_relative_path(&canonical_root, canonical_path)?;
    let components = normal_path_components(&relative)?;
    reject_empty_metadata_path(&components)?;
    open_component_chain(&canonical_root, &components)
}

#[cfg(unix)]
fn anchored_relative_path(canonical_root: &Path, canonical_path: &Path) -> std::io::Result<PathBuf> {
    canonical_path
        .strip_prefix(canonical_root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            std::io::Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{} is outside anchored root {}",
                    canonical_path.display(),
                    canonical_root.display()
                ),
            )
        })
}

#[cfg(unix)]
fn normal_path_components(relative: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    relative
        .components()
        .map(|component| normal_path_component(component, relative))
        .collect()
}

#[cfg(unix)]
fn normal_path_component(
    component: std::path::Component<'_>,
    relative: &Path,
) -> std::io::Result<std::ffi::OsString> {
    match component {
        std::path::Component::Normal(name) => Ok(name.to_os_string()),
        _ => Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsafe relative path component in {}", relative.display()),
        )),
    }
}

#[cfg(unix)]
fn reject_empty_metadata_path(components: &[std::ffi::OsString]) -> std::io::Result<()> {
    if components.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "repository metadata path resolves to the project root",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_component_chain(root: &Path, components: &[std::ffi::OsString]) -> std::io::Result<File> {
    use rustix::fs::{open, openat, Mode, OFlags};

    let directory_flags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mut descriptor = open(root, directory_flags, Mode::empty())?;
    for (index, component) in components.iter().enumerate() {
        let flags = if index + 1 == components.len() {
            file_flags
        } else {
            directory_flags
        };
        descriptor = openat(&descriptor, component, flags, Mode::empty())?;
    }
    Ok(File::from(descriptor))
}

#[cfg(unix)]
type FileIdentity = (u64, u64);

#[cfg(not(unix))]
type FileIdentity = same_file::Handle;

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

    #[cfg(unix)]
    fn escaping_symlink_fixture(name: &str, contents: &str) -> (TempDir, TempDir, PathBuf) {
        use std::os::unix::fs::symlink;

        let (root, outside, target) = crate::path_io::external_test_file(name, contents);
        let link = root.path().join(name);
        symlink(&target, &link).unwrap_or_else(|error| panic!("symlink: {error}"));
        (root, outside, link)
    }

    #[test]
    fn sparse_oversized_file_is_rejected_before_reading() {
        let (root, path) = crate::path_io::sparse_test_file("package.json", PROJECT_METADATA_MAX_BYTES + 1);

        crate::path_io::assert_core_error(
            read_bounded_utf8_file_within(root.path(), &path, PROJECT_METADATA_MAX_BYTES),
            crate::path_io::ExpectedCoreError::FileTooLarge,
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let (root, _outside, link) = escaping_symlink_fixture("package.json", "{}\n");

        crate::path_io::assert_core_error(
            read_bounded_utf8_file_within(root.path(), &link, PROJECT_METADATA_MAX_BYTES),
            crate::path_io::ExpectedCoreError::UnsafePath,
        );
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
