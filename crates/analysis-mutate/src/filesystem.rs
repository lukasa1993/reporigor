use std::fs::{self, File, FileTimes, Metadata, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fs2::FileExt;
use reporigor_core::MutationCandidate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{MutationError, RecoveryAction};

/// Application-specific suffix used below the operating system's persistent
/// per-user state directory.
pub const STATE_DIRECTORY: &str = "reporigor/mutation";
/// Override for the parent beneath which the dedicated state base is created.
pub const STATE_DIRECTORY_ENV: &str = "REPORIGOR_MUTATION_STATE_DIR";
pub const ACTIVE_JOURNAL: &str = "active.json";
pub const ACTIVE_RUN: &str = "active-run.json";
pub const RUN_LOCK: &str = "run.lock";

const JOURNAL_SCHEMA_VERSION: u8 = 1;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ACTIVE_RUN_BYTES: u64 = 64 * 1024;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
const MAX_EXTENDED_ATTRIBUTE_NAME_BYTES: usize = 64 * 1024;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
const MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES: usize = 1024 * 1024;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
const MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES: usize = 4 * 1024 * 1024;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
const MAX_EXTENDED_ATTRIBUTES: usize = 256;
const EXCLUSIVE_LOCK_WAIT: Duration = Duration::from_secs(3);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
/// Hard ceiling for source bytes held by executable mutation and recovery.
pub const MAX_MUTATION_SOURCE_BYTES: usize = 32 * 1024 * 1024;

const fn default_max_mutation_source_bytes() -> usize {
    MAX_MUTATION_SOURCE_BYTES
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalRecord {
    schema_version: u8,
    file: String,
    original_base64: String,
    original_sha256: String,
    mutated_sha256: String,
    #[serde(default = "default_max_mutation_source_bytes")]
    max_source_bytes: usize,
    original_permissions: StoredPermissions,
}

#[derive(Debug, Serialize, Deserialize)]
struct ActiveRunRecord {
    schema_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_base64: Option<String>,
    root_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_identity: Option<RootIdentity>,
    file: String,
    original_sha256: String,
    mutated_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RootIdentity {
    kind: String,
    primary: u64,
    secondary: u64,
}

/// Read-only description of crash-recovery state relevant to a project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMutation {
    pub root: PathBuf,
    pub journal: PathBuf,
    pub active_pointer: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPermissions {
    readonly: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accessed: Option<StoredFileTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified: Option<StoredFileTime>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extended_attributes: Vec<StoredExtendedAttribute>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct StoredFileTime {
    before_epoch: bool,
    seconds: u64,
    nanoseconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredExtendedAttribute {
    name_base64: String,
    value_base64: String,
}

impl StoredFileTime {
    fn capture(value: SystemTime) -> Self {
        match value.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self {
                before_epoch: false,
                seconds: duration.as_secs(),
                nanoseconds: duration.subsec_nanos(),
            },
            Err(error) => Self {
                before_epoch: true,
                seconds: error.duration().as_secs(),
                nanoseconds: error.duration().subsec_nanos(),
            },
        }
    }

    fn materialize(self) -> Option<SystemTime> {
        if self.nanoseconds >= 1_000_000_000 {
            return None;
        }
        let duration = Duration::new(self.seconds, self.nanoseconds);
        if self.before_epoch {
            UNIX_EPOCH.checked_sub(duration)
        } else {
            UNIX_EPOCH.checked_add(duration)
        }
    }
}

impl StoredPermissions {
    #[allow(clippy::unnecessary_wraps)]
    fn capture(path: &Path, file: &File, metadata: &Metadata) -> Result<Self, MutationError> {
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        let _ = (path, file);
        #[cfg(unix)]
        let unix_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(metadata.permissions().mode())
        };
        #[cfg(not(unix))]
        let unix_mode = None;
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        let extended_attributes = capture_extended_attributes(path, file)?;
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        let extended_attributes = Vec::new();
        Ok(Self {
            readonly: metadata.permissions().readonly(),
            unix_mode,
            accessed: metadata.accessed().ok().map(StoredFileTime::capture),
            modified: metadata.modified().ok().map(StoredFileTime::capture),
            extended_attributes,
        })
    }

    fn materialize(&self, fallback: Permissions) -> Permissions {
        #[cfg(unix)]
        if let Some(mode) = self.unix_mode {
            use std::os::unix::fs::PermissionsExt;
            return Permissions::from_mode(mode);
        }
        let mut permissions = fallback;
        permissions.set_readonly(self.readonly);
        permissions
    }

    fn apply(&self, file: &File, path: &Path, fallback: Permissions) -> Result<(), MutationError> {
        file.set_permissions(self.materialize(fallback))
            .map_err(|source| MutationError::io("set replacement permissions", path, source))?;
        let mut times = FileTimes::new();
        let mut has_times = false;
        if let Some(accessed) = self.accessed {
            let accessed =
                accessed
                    .materialize()
                    .ok_or_else(|| MutationError::UnsupportedSourceMetadata {
                        path: path.to_path_buf(),
                        message: "stored access time is outside the platform time range".into(),
                    })?;
            times = times.set_accessed(accessed);
            has_times = true;
        }
        if let Some(modified) = self.modified {
            let modified =
                modified
                    .materialize()
                    .ok_or_else(|| MutationError::UnsupportedSourceMetadata {
                        path: path.to_path_buf(),
                        message: "stored modification time is outside the platform time range".into(),
                    })?;
            times = times.set_modified(modified);
            has_times = true;
        }
        if has_times {
            file.set_times(times)
                .map_err(|source| MutationError::io("set replacement timestamps", path, source))?;
        }
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        apply_extended_attributes(path, file, &self.extended_attributes)?;
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        if !self.extended_attributes.is_empty() {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: "stored extended attributes cannot be restored on this platform".into(),
            });
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn capture_extended_attributes(
    path: &Path,
    file: &File,
) -> Result<Vec<StoredExtendedAttribute>, MutationError> {
    let names = list_extended_attribute_names(path, file)?;
    let mut stored = Vec::with_capacity(names.len());
    let mut retained_bytes = 0_usize;
    for name in names {
        let name = std::ffi::CString::new(name).map_err(|_| MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: "extended-attribute name contains an embedded NUL byte".into(),
        })?;
        let mut value = vec![0_u8; MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES];
        let length = match rustix::fs::fgetxattr(file, name.as_c_str(), &mut value) {
            Ok(length) => length,
            Err(error) if error == rustix::io::Errno::RANGE => {
                return Err(MutationError::UnsupportedSourceMetadata {
                    path: path.to_path_buf(),
                    message: format!(
                        "extended attribute {:?} exceeds {MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES} bytes",
                        name.as_c_str()
                    ),
                });
            }
            Err(error) => {
                return Err(MutationError::io(
                    "read mutation source extended attribute",
                    path,
                    std::io::Error::from_raw_os_error(error.raw_os_error()),
                ));
            }
        };
        value.truncate(length);
        retained_bytes = retained_bytes
            .checked_add(name.as_bytes().len())
            .and_then(|total| total.checked_add(value.len()))
            .ok_or_else(|| MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: "extended-attribute size overflowed".into(),
            })?;
        if retained_bytes > MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "extended attributes exceed {MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES} retained bytes"
                ),
            });
        }
        stored.push(StoredExtendedAttribute {
            name_base64: BASE64.encode(name.as_bytes()),
            value_base64: BASE64.encode(value),
        });
    }
    Ok(stored)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn list_extended_attribute_names(path: &Path, file: &File) -> Result<Vec<Vec<u8>>, MutationError> {
    let mut query = [0_u8; 0];
    let required = match rustix::fs::flistxattr(file, &mut query) {
        Ok(required) => required,
        Err(error) if error == rustix::io::Errno::NOTSUP => return Ok(Vec::new()),
        Err(error) => {
            return Err(MutationError::io(
                "list mutation source extended attributes",
                path,
                std::io::Error::from_raw_os_error(error.raw_os_error()),
            ));
        }
    };
    if required > MAX_EXTENDED_ATTRIBUTE_NAME_BYTES {
        return Err(MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: format!(
                "extended-attribute names require {required} bytes, exceeding {MAX_EXTENDED_ATTRIBUTE_NAME_BYTES}"
            ),
        });
    }
    if required == 0 {
        return Ok(Vec::new());
    }
    let mut encoded = vec![0_u8; required];
    let actual = rustix::fs::flistxattr(file, &mut encoded).map_err(|error| {
        MutationError::io(
            "list mutation source extended attributes",
            path,
            std::io::Error::from_raw_os_error(error.raw_os_error()),
        )
    })?;
    if actual > required {
        return Err(MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: "extended attributes changed while they were being captured".into(),
        });
    }
    encoded.truncate(actual);
    if encoded.last() != Some(&0) {
        return Err(MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: "extended-attribute name list is malformed".into(),
        });
    }
    let names = encoded
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if names.len() > MAX_EXTENDED_ATTRIBUTES {
        return Err(MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: format!(
                "file has {} extended attributes, exceeding {MAX_EXTENDED_ATTRIBUTES}",
                names.len()
            ),
        });
    }
    Ok(names)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn apply_extended_attributes(
    path: &Path,
    file: &File,
    stored: &[StoredExtendedAttribute],
) -> Result<(), MutationError> {
    let current = list_extended_attribute_names(path, file)?;
    let mut decoded = Vec::with_capacity(stored.len());
    let mut retained_bytes = 0_usize;
    for attribute in stored {
        if attribute.name_base64.len() > MAX_EXTENDED_ATTRIBUTE_NAME_BYTES.saturating_mul(2)
            || attribute.value_base64.len() > MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES.saturating_mul(2)
        {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: "encoded extended attribute exceeds its recovery limit".into(),
            });
        }
        let name = BASE64.decode(&attribute.name_base64).map_err(|error| {
            MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: format!("invalid encoded extended-attribute name: {error}"),
            }
        })?;
        let value = BASE64.decode(&attribute.value_base64).map_err(|error| {
            MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: format!("invalid encoded extended-attribute value: {error}"),
            }
        })?;
        if name.is_empty()
            || name.len() > MAX_EXTENDED_ATTRIBUTE_NAME_BYTES
            || value.len() > MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES
        {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: "decoded extended attribute exceeds its recovery limit".into(),
            });
        }
        retained_bytes = retained_bytes
            .checked_add(name.len())
            .and_then(|total| total.checked_add(value.len()))
            .ok_or_else(|| MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: "extended-attribute size overflowed".into(),
            })?;
        if retained_bytes > MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES || decoded.len() >= MAX_EXTENDED_ATTRIBUTES {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: "stored extended attributes exceed their recovery limits".into(),
            });
        }
        let name = std::ffi::CString::new(name).map_err(|_| MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: "extended-attribute name contains an embedded NUL byte".into(),
        })?;
        decoded.push((name, value));
    }
    for (name, value) in &decoded {
        rustix::fs::fsetxattr(file, name.as_c_str(), value, rustix::fs::XattrFlags::empty()).map_err(
            |error| {
                MutationError::io(
                    "restore mutation source extended attribute",
                    path,
                    std::io::Error::from_raw_os_error(error.raw_os_error()),
                )
            },
        )?;
    }
    for name in current {
        if decoded.iter().any(|(stored, _)| stored.as_bytes() == name) {
            continue;
        }
        let name = std::ffi::CString::new(name).map_err(|_| MutationError::UnsupportedSourceMetadata {
            path: path.to_path_buf(),
            message: "extended-attribute name contains an embedded NUL byte".into(),
        })?;
        rustix::fs::fremovexattr(file, name.as_c_str()).map_err(|error| {
            MutationError::io(
                "remove replacement-only extended attribute",
                path,
                std::io::Error::from_raw_os_error(error.raw_os_error()),
            )
        })?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApplyMutationError {
    #[error("invalid mutation candidate: {0}")]
    Invalid(String),
    #[error(transparent)]
    Fatal(#[from] MutationError),
}

pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf, MutationError> {
    let canonical = root.canonicalize().map_err(|source| MutationError::InvalidRoot {
        path: root.to_path_buf(),
        message: source.to_string(),
    })?;
    let metadata = fs::metadata(&canonical)
        .map_err(|source| MutationError::io("inspect project root", canonical.clone(), source))?;
    if !metadata.is_dir() {
        return Err(MutationError::InvalidRoot {
            path: canonical,
            message: "root is not a directory".into(),
        });
    }
    Ok(canonical)
}

fn ensure_directory(path: &Path) -> Result<(), MutationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(MutationError::UnsafePath {
                    path: path.to_path_buf(),
                    message: "state directory is a symbolic link".into(),
                });
            }
            if !metadata.is_dir() {
                return Err(MutationError::UnsafePath {
                    path: path.to_path_buf(),
                    message: "state path is not a directory".into(),
                });
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path)
                .map_err(|source| MutationError::io("create state directory", path, source))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|source| MutationError::io("inspect state directory", path, source))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(MutationError::UnsafePath {
                    path: path.to_path_buf(),
                    message: "new state directory was replaced with an unsafe path".into(),
                });
            }
        }
        Err(source) => return Err(MutationError::io("inspect state directory", path, source)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, Permissions::from_mode(0o700)).map_err(|source| {
            MutationError::io("restrict mutation state directory permissions", path, source)
        })?;
    }
    Ok(())
}

fn configured_state_base() -> Result<PathBuf, MutationError> {
    if let Some(override_path) = std::env::var_os(STATE_DIRECTORY_ENV) {
        if override_path.is_empty() {
            return Err(MutationError::State(format!(
                "{STATE_DIRECTORY_ENV} must not be empty"
            )));
        }
        return Ok(state_base_below_override(Path::new(&override_path)));
    }

    #[cfg(windows)]
    let platform_base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    #[cfg(target_os = "macos")]
    let platform_base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));

    #[cfg(all(unix, not(target_os = "macos")))]
    let platform_base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/state"))
        });

    #[cfg(not(any(unix, windows)))]
    let platform_base: Option<PathBuf> = None;

    platform_base
        .map(|base| base.join(STATE_DIRECTORY))
        .ok_or_else(|| {
            MutationError::State(format!(
                "cannot locate a persistent per-user state directory; set {STATE_DIRECTORY_ENV} to an absolute path"
            ))
        })
}

fn state_base_below_override(parent: &Path) -> PathBuf {
    parent.join(STATE_DIRECTORY)
}

fn validate_absolute_state_path(path: &Path) -> Result<(), MutationError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(MutationError::UnsafePath {
            path: path.to_path_buf(),
            message: format!(
                "mutation state must use an absolute normalized path (configure {STATE_DIRECTORY_ENV})"
            ),
        });
    }
    Ok(())
}

/// Resolve an absolute path through its nearest existing ancestor without
/// creating it. This lets us reject an override that would resolve inside the
/// analyzed project before writing any state there.
fn prospective_canonical_path(path: &Path) -> Result<PathBuf, MutationError> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| MutationError::UnsafePath {
                    path: path.to_path_buf(),
                    message: "mutation state has no existing ancestor".into(),
                })?;
                missing.push(name.to_os_string());
                if !existing.pop() {
                    return Err(MutationError::UnsafePath {
                        path: path.to_path_buf(),
                        message: "mutation state has no existing ancestor".into(),
                    });
                }
            }
            Err(source) => {
                return Err(MutationError::io(
                    "inspect mutation state ancestor",
                    &existing,
                    source,
                ));
            }
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|source| MutationError::io("canonicalize mutation state ancestor", &existing, source))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn project_state_key(root: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"reporigor-mutation-root-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        digest.update(root.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for word in root.as_os_str().encode_wide() {
            digest.update(word.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    digest.update(root.as_os_str().to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
}

fn state_directory_path(root: &Path, base: &Path) -> Result<PathBuf, MutationError> {
    validate_absolute_state_path(base)?;
    let resolved_base = prospective_canonical_path(base)?;
    let state = resolved_base.join(project_state_key(root));
    if state.starts_with(root) {
        return Err(MutationError::UnsafePath {
            path: state,
            message: "mutation execution state must be outside the analyzed project".into(),
        });
    }
    Ok(state)
}

/// Return the stable per-project mutation state path without creating it.
///
/// # Errors
///
/// Returns an error if the root is invalid or the configured persistent state
/// parent is unavailable, relative, non-normalized, or resolves inside the
/// analyzed project.
pub fn mutation_state_directory(root: &Path) -> Result<PathBuf, MutationError> {
    let root = canonical_root(root)?;
    let base = configured_state_base()?;
    state_directory_path(&root, &base)
}

fn pending_journal_in_state(state: &Path) -> Result<Option<PathBuf>, MutationError> {
    match fs::symlink_metadata(state) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(MutationError::UnsafePath {
                path: state.to_path_buf(),
                message: "mutation state is not a regular directory".into(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(MutationError::io("inspect mutation state", state, source)),
    }
    let journal = journal_path(state);
    ensure_regular_nonsymlink(&journal, "active journal")?;
    match fs::metadata(&journal) {
        Ok(_) => Ok(Some(journal)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MutationError::io("inspect active journal", journal, source)),
    }
}

/// Locate a pending recovery journal without creating state or changing source.
///
/// The presence of a journal is reported even if its contents are corrupt, so
/// callers never analyze a possibly mutated source merely because recovery
/// validation would fail.
///
/// # Errors
///
/// Returns an error if the root or configured state path is unsafe or cannot
/// be inspected.
pub fn pending_mutation_journal(root: &Path) -> Result<Option<PendingMutation>, MutationError> {
    let root = canonical_root(root)?;
    let base = configured_state_base()?;
    let state = state_directory_path(&root, &base)?;
    pending_mutation_in_state(&root, &state)
}

pub(crate) fn pending_mutation_locked(
    root: &Path,
    state: &Path,
) -> Result<Option<PendingMutation>, MutationError> {
    pending_mutation_in_state(root, state)
}

fn pending_mutation_in_state(root: &Path, state: &Path) -> Result<Option<PendingMutation>, MutationError> {
    let base = state_base(state)?;
    if let Some((pointer, record)) = read_active_run(base)? {
        let active_root = validated_active_root(&pointer, &record)?;
        // A global pointer means some source may still contain a mutant. It
        // blocks every shared analysis session until recovery, including when
        // a crashed checkout has since moved to a non-overlapping path.
        return Ok(Some(PendingMutation {
            journal: base.join(&record.root_key).join(ACTIVE_JOURNAL),
            root: active_root,
            active_pointer: Some(pointer),
        }));
    }
    Ok(pending_journal_in_state(state)?.map(|journal| PendingMutation {
        root: root.to_path_buf(),
        journal,
        active_pointer: None,
    }))
}

fn prepare_state_directory_in(root: &Path, base: &Path) -> Result<PathBuf, MutationError> {
    let prospective = state_directory_path(root, base)?;
    fs::create_dir_all(base)
        .map_err(|source| MutationError::io("create mutation state base", base, source))?;
    let metadata = fs::symlink_metadata(base)
        .map_err(|source| MutationError::io("inspect mutation state base", base, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MutationError::UnsafePath {
            path: base.to_path_buf(),
            message: "mutation state base is not a regular directory".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(base, Permissions::from_mode(0o700))
            .map_err(|source| MutationError::io("restrict mutation state base permissions", base, source))?;
    }
    let canonical_base = base
        .canonicalize()
        .map_err(|source| MutationError::io("canonicalize mutation state base", base, source))?;
    let state = canonical_base.join(project_state_key(root));
    if state != prospective || state.starts_with(root) {
        return Err(MutationError::UnsafePath {
            path: state,
            message:
                "mutation state base changed while it was prepared or resolves inside the analyzed project"
                    .into(),
        });
    }
    ensure_directory(&state)?;
    Ok(state)
}

fn prepare_state_directory(root: &Path) -> Result<PathBuf, MutationError> {
    let base = configured_state_base()?;
    prepare_state_directory_in(root, &base)
}

fn ensure_regular_nonsymlink(path: &Path, purpose: &'static str) -> Result<(), MutationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MutationError::UnsafePath {
            path: path.to_path_buf(),
            message: format!("{purpose} is a symbolic link"),
        }),
        Ok(metadata) if !metadata.is_file() => Err(MutationError::UnsafePath {
            path: path.to_path_buf(),
            message: format!("{purpose} is not a regular file"),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MutationError::io("inspect state file", path, source)),
    }
}

#[derive(Debug)]
pub(crate) struct RunLockGuard {
    file: File,
    state: PathBuf,
}

impl RunLockGuard {
    pub(crate) fn acquire(root: &Path) -> Result<Self, MutationError> {
        let state = prepare_state_directory(root)?;
        Self::acquire_in_state(state, true, true)
    }

    pub(crate) fn acquire_shared(root: &Path) -> Result<Self, MutationError> {
        let state = prepare_state_directory(root)?;
        Self::acquire_in_state(state, false, false)
    }

    #[cfg(test)]
    fn acquire_with_base(root: &Path, base: &Path) -> Result<Self, MutationError> {
        let state = prepare_state_directory_in(root, base)?;
        Self::acquire_in_state(state, true, false)
    }

    #[cfg(test)]
    fn acquire_shared_with_base(root: &Path, base: &Path) -> Result<Self, MutationError> {
        let state = prepare_state_directory_in(root, base)?;
        Self::acquire_in_state(state, false, false)
    }

    #[cfg(test)]
    fn acquire_waiting_with_base(root: &Path, base: &Path) -> Result<Self, MutationError> {
        let state = prepare_state_directory_in(root, base)?;
        Self::acquire_in_state(state, true, true)
    }

    fn acquire_in_state(
        state: PathBuf,
        exclusive: bool,
        wait_for_readers: bool,
    ) -> Result<Self, MutationError> {
        let base = state.parent().ok_or_else(|| MutationError::UnsafePath {
            path: state.clone(),
            message: "mutation state has no global lock directory".into(),
        })?;
        // One lock for the entire state base deliberately serializes roots that
        // overlap (for example, a workspace and one nested package).
        let path = base.join(RUN_LOCK);
        ensure_regular_nonsymlink(&path, "run lock")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| MutationError::io("open run lock", &path, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|source| MutationError::io("restrict run lock permissions", &path, source))?;
        }
        let deadline = wait_for_readers.then(|| Instant::now() + EXCLUSIVE_LOCK_WAIT);
        loop {
            let lock_result = if exclusive {
                FileExt::try_lock_exclusive(&file)
            } else {
                FileExt::try_lock_shared(&file)
            };
            match lock_result {
                Ok(()) => return Ok(Self { file, state }),
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        && deadline.is_some_and(|deadline| Instant::now() < deadline) =>
                {
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    return Err(MutationError::AlreadyRunning { path });
                }
                Err(source) => return Err(MutationError::io("acquire run lock", path, source)),
            }
        }
    }

    pub(crate) fn state(&self) -> &Path {
        &self.state
    }
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn is_control_path(path: &Path) -> bool {
    let names = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>();
    names.iter().any(|name| {
        [".git", ".hg", ".svn", ".reporigor"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    }) || names
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case("target") && pair[1].eq_ignore_ascii_case("reporigor"))
}

fn relative_source_path(file: &str) -> Result<PathBuf, MutationError> {
    let relative = Path::new(file);
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(MutationError::UnsafePath {
            path: relative.to_path_buf(),
            message: "source path must be non-empty and relative".into(),
        });
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(MutationError::UnsafePath {
            path: relative.to_path_buf(),
            message: "source path contains a root, parent, prefix, or current-directory component".into(),
        });
    }
    if is_control_path(relative) {
        return Err(MutationError::UnsafePath {
            path: relative.to_path_buf(),
            message: "version-control and reporigor control paths are not valid mutation targets".into(),
        });
    }
    Ok(relative.to_path_buf())
}

pub(crate) fn resolve_source_path(
    root: &Path,
    file: &str,
    allow_missing_file: bool,
) -> Result<PathBuf, MutationError> {
    let relative = relative_source_path(file)?;
    if is_control_path(root) {
        return Err(MutationError::UnsafePath {
            path: root.to_path_buf(),
            message: "a version-control or reporigor control directory cannot be a mutation root".into(),
        });
    }
    let component_count = relative.components().count();
    let mut cursor = root.to_path_buf();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(name) = component else {
            unreachable!("relative_source_path accepts only normal components");
        };
        cursor.push(name);
        let final_component = index + 1 == component_count;
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(MutationError::UnsafePath {
                        path: cursor,
                        message: "symbolic links are not valid mutation targets".into(),
                    });
                }
                if final_component && !metadata.is_file() {
                    return Err(MutationError::UnsafePath {
                        path: cursor,
                        message: "mutation target is not a regular file".into(),
                    });
                }
                if !final_component && !metadata.is_dir() {
                    return Err(MutationError::UnsafePath {
                        path: cursor,
                        message: "a mutation path parent is not a directory".into(),
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound && final_component && allow_missing_file => {
                // Recovery may recreate a source file deleted by an interrupted command.
            }
            Err(source) => {
                return Err(MutationError::io("inspect mutation source", cursor, source));
            }
        }
    }

    let parent = cursor.parent().ok_or_else(|| MutationError::UnsafePath {
        path: cursor.clone(),
        message: "mutation target has no parent directory".into(),
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|source| MutationError::io("canonicalize mutation source parent", parent, source))?;
    if !canonical_parent.starts_with(root) {
        return Err(MutationError::UnsafePath {
            path: cursor,
            message: "mutation source escapes the project root".into(),
        });
    }
    if cursor.exists() {
        let canonical_target = cursor
            .canonicalize()
            .map_err(|source| MutationError::io("canonicalize mutation source", &cursor, source))?;
        if !canonical_target.starts_with(root) {
            return Err(MutationError::UnsafePath {
                path: cursor,
                message: "canonical mutation source escapes the project root".into(),
            });
        }
    }
    Ok(cursor)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_bounded_source(
    path: &Path,
    max_source_bytes: usize,
) -> Result<(Metadata, Vec<u8>, StoredPermissions), MutationError> {
    let max_source_bytes_u64 = u64::try_from(max_source_bytes).unwrap_or(u64::MAX);
    let file = File::open(path).map_err(|source| MutationError::io("open mutation source", path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| MutationError::io("inspect opened mutation source", path, source))?;
    ensure_supported_source_metadata(path, &file, &metadata)?;
    let permissions = StoredPermissions::capture(path, &file, &metadata)?;
    if metadata.len() > max_source_bytes_u64 {
        return Err(MutationError::SourceTooLarge {
            path: path.to_path_buf(),
            actual_bytes: metadata.len(),
            max_source_bytes: max_source_bytes_u64,
        });
    }
    let capacity = usize::try_from(metadata.len()).unwrap_or(max_source_bytes);
    let read_limit = max_source_bytes_u64.saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity.min(max_source_bytes));
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|source| MutationError::io("read mutation source", path, source))?;
    if bytes.len() > max_source_bytes {
        return Err(MutationError::SourceTooLarge {
            path: path.to_path_buf(),
            actual_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max_source_bytes: max_source_bytes_u64,
        });
    }
    Ok((metadata, bytes, permissions))
}

fn ensure_supported_source_metadata(
    path: &Path,
    file: &File,
    metadata: &Metadata,
) -> Result<(), MutationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "file has {} hard links; atomic replacement would silently split the linked inode",
                    metadata.nlink()
                ),
            });
        }
    }
    #[cfg(windows)]
    {
        let links = winapi_util::file::information(file)
            .map_err(|source| MutationError::io("inspect mutation source links", path, source))?
            .number_of_links();
        if links != 1 {
            return Err(MutationError::UnsupportedSourceMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "file has {links} hard links; atomic replacement would silently split the linked inode"
                ),
            });
        }
    }
    #[cfg(not(windows))]
    let _ = file;
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn sync_parent(path: &Path) -> Result<(), MutationError> {
    #[cfg(unix)]
    {
        let parent = path.parent().ok_or_else(|| MutationError::UnsafePath {
            path: path.to_path_buf(),
            message: "atomic replacement target has no parent".into(),
        })?;
        let directory = File::open(parent)
            .map_err(|source| MutationError::io("open directory for synchronization", parent, source))?;
        directory
            .sync_all()
            .map_err(|source| MutationError::io("synchronize directory", parent, source))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&StoredPermissions>,
) -> Result<(), MutationError> {
    let parent = path.parent().ok_or_else(|| MutationError::UnsafePath {
        path: path.to_path_buf(),
        message: "atomic replacement target has no parent".into(),
    })?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|source| MutationError::io("create atomic replacement", parent, source))?;
    temporary
        .write_all(bytes)
        .map_err(|source| MutationError::io("write atomic replacement", temporary.path(), source))?;
    if let Some(value) = permissions {
        let fallback = temporary
            .as_file()
            .metadata()
            .map_err(|source| MutationError::io("inspect replacement permissions", temporary.path(), source))?
            .permissions();
        value.apply(temporary.as_file(), temporary.path(), fallback)?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(Permissions::from_mode(0o600))
                .map_err(|source| {
                    MutationError::io(
                        "restrict atomic replacement permissions",
                        temporary.path(),
                        source,
                    )
                })?;
        }
    }
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| MutationError::io("synchronize atomic replacement", temporary.path(), source))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| MutationError::io("synchronize replacement metadata", temporary.path(), source))?;
    temporary
        .persist(path)
        .map_err(|error| MutationError::io("persist atomic replacement", path, error.error))?;
    sync_parent(path)
}

fn journal_path(state: &Path) -> PathBuf {
    state.join(ACTIVE_JOURNAL)
}

fn write_journal_with_limit(
    state: &Path,
    record: &JournalRecord,
    max_journal_bytes: u64,
) -> Result<PathBuf, MutationError> {
    let path = journal_path(state);
    ensure_regular_nonsymlink(&path, "active journal")?;
    if path.exists() {
        return Err(MutationError::State(format!(
            "refusing to replace existing active mutation journal {}",
            path.display()
        )));
    }
    let encoded =
        serde_json::to_vec_pretty(record).map_err(|error| MutationError::State(error.to_string()))?;
    let encoded_len = u64::try_from(encoded.len())
        .map_err(|_| MutationError::State("encoded mutation journal length does not fit u64".into()))?;
    if encoded_len > max_journal_bytes {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!(
                "encoded journal is {encoded_len} bytes, exceeding the safe {max_journal_bytes}-byte limit"
            ),
        });
    }
    atomic_replace(&path, &encoded, None)?;
    Ok(path)
}

fn remove_journal(path: &Path) -> Result<(), MutationError> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MutationError::io("remove active journal", path, source)),
    }
}

fn read_journal(state: &Path) -> Result<Option<(PathBuf, JournalRecord)>, MutationError> {
    let path = journal_path(state);
    ensure_regular_nonsymlink(&path, "active journal")?;
    let metadata = match fs::metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(MutationError::io("inspect active journal", &path, source)),
    };
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!("journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        });
    }
    let file = File::open(&path).map_err(|source| MutationError::io("open active journal", &path, source))?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len().min(MAX_JOURNAL_BYTES)).unwrap_or(usize::MAX));
    file.take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MutationError::io("read active journal", &path, source))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JOURNAL_BYTES {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!("journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        });
    }
    let record =
        serde_json::from_slice::<JournalRecord>(&bytes).map_err(|error| MutationError::InvalidJournal {
            path: path.clone(),
            message: error.to_string(),
        })?;
    if record.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!("unsupported schema version {}", record.schema_version),
        });
    }
    Ok(Some((path, record)))
}

fn state_base(state: &Path) -> Result<&Path, MutationError> {
    state.parent().ok_or_else(|| MutationError::UnsafePath {
        path: state.to_path_buf(),
        message: "per-project mutation state has no global state base".into(),
    })
}

fn active_run_path(base: &Path) -> PathBuf {
    base.join(ACTIVE_RUN)
}

fn write_active_run(root: &Path, state: &Path, journal: &JournalRecord) -> Result<PathBuf, MutationError> {
    let path = active_run_path(state_base(state)?);
    ensure_regular_nonsymlink(&path, "global active mutation pointer")?;
    if path.exists() {
        return Err(MutationError::State(format!(
            "refusing to replace existing active mutation pointer {}",
            path.display()
        )));
    }
    let (root_encoding, root_base64) = encode_active_root(root);
    let record = ActiveRunRecord {
        schema_version: JOURNAL_SCHEMA_VERSION,
        root: None,
        root_encoding: Some(root_encoding),
        root_base64: Some(root_base64),
        root_key: project_state_key(root),
        root_identity: root_identity(root)?,
        file: journal.file.clone(),
        original_sha256: journal.original_sha256.clone(),
        mutated_sha256: journal.mutated_sha256.clone(),
    };
    let encoded =
        serde_json::to_vec_pretty(&record).map_err(|error| MutationError::State(error.to_string()))?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_ACTIVE_RUN_BYTES {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!("active mutation pointer exceeds {MAX_ACTIVE_RUN_BYTES} bytes"),
        });
    }
    atomic_replace(&path, &encoded, None)?;
    Ok(path)
}

#[allow(clippy::unnecessary_wraps)]
fn root_identity(root: &Path) -> Result<Option<RootIdentity>, MutationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(root)
            .map_err(|source| MutationError::io("inspect project root identity", root, source))?;
        Ok(Some(RootIdentity {
            kind: "unix-device-inode".into(),
            primary: metadata.dev(),
            secondary: metadata.ino(),
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(None)
    }
}

fn read_active_run(base: &Path) -> Result<Option<(PathBuf, ActiveRunRecord)>, MutationError> {
    let path = active_run_path(base);
    ensure_regular_nonsymlink(&path, "global active mutation pointer")?;
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MutationError::io(
                "inspect active mutation pointer",
                &path,
                source,
            ))
        }
    };
    if metadata.len() > MAX_ACTIVE_RUN_BYTES {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!("active mutation pointer exceeds {MAX_ACTIVE_RUN_BYTES} bytes"),
        });
    }
    let file = File::open(&path)
        .map_err(|source| MutationError::io("open active mutation pointer", &path, source))?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len().min(MAX_ACTIVE_RUN_BYTES)).unwrap_or(usize::MAX));
    file.take(MAX_ACTIVE_RUN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MutationError::io("read active mutation pointer", &path, source))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ACTIVE_RUN_BYTES {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!("active mutation pointer exceeds {MAX_ACTIVE_RUN_BYTES} bytes"),
        });
    }
    let record =
        serde_json::from_slice::<ActiveRunRecord>(&bytes).map_err(|error| MutationError::InvalidJournal {
            path: path.clone(),
            message: error.to_string(),
        })?;
    if record.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(MutationError::InvalidJournal {
            path,
            message: format!(
                "unsupported active pointer schema version {}",
                record.schema_version
            ),
        });
    }
    let root = decode_active_root(&record).map_err(|message| MutationError::InvalidJournal {
        path: path.clone(),
        message,
    })?;
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        || record.root_key != project_state_key(&root)
        || relative_source_path(&record.file).is_err()
        || !is_sha256(&record.original_sha256)
        || !is_sha256(&record.mutated_sha256)
    {
        return Err(MutationError::InvalidJournal {
            path,
            message: "active mutation pointer contains invalid root, source, or checksum fields".into(),
        });
    }
    Ok(Some((path, record)))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remove_active_run(path: &Path) -> Result<(), MutationError> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MutationError::io("remove active mutation pointer", path, source)),
    }
}

fn encode_active_root(root: &Path) -> (String, String) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        ("unix-bytes".into(), BASE64.encode(root.as_os_str().as_bytes()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut bytes = Vec::new();
        for word in root.as_os_str().encode_wide() {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        ("windows-utf16le".into(), BASE64.encode(bytes))
    }
    #[cfg(not(any(unix, windows)))]
    {
        (
            "utf8".into(),
            BASE64.encode(root.as_os_str().to_string_lossy().as_bytes()),
        )
    }
}

fn decode_active_root(record: &ActiveRunRecord) -> Result<PathBuf, String> {
    if let (Some(encoding), Some(encoded)) = (&record.root_encoding, &record.root_base64) {
        if record.root.is_some() {
            return Err("active mutation pointer contains ambiguous root encodings".into());
        }
        let bytes = BASE64
            .decode(encoded)
            .map_err(|error| format!("invalid encoded mutation root: {error}"))?;
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            if encoding != "unix-bytes" {
                return Err(format!("unsupported mutation root encoding {encoding}"));
            }
            return Ok(PathBuf::from(OsString::from_vec(bytes)));
        }
        #[cfg(windows)]
        {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            if encoding != "windows-utf16le" || bytes.len() % 2 != 0 {
                return Err(format!("invalid mutation root encoding {encoding}"));
            }
            let words = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>();
            return Ok(PathBuf::from(OsString::from_wide(&words)));
        }
        #[cfg(not(any(unix, windows)))]
        {
            if encoding != "utf8" {
                return Err(format!("unsupported mutation root encoding {encoding}"));
            }
            return String::from_utf8(bytes)
                .map(PathBuf::from)
                .map_err(|error| format!("invalid UTF-8 mutation root: {error}"));
        }
    }
    match (&record.root, &record.root_encoding, &record.root_base64) {
        (Some(root), None, None) => Ok(PathBuf::from(root)),
        _ => Err("active mutation pointer is missing a complete root encoding".into()),
    }
}

fn validated_active_root(pointer: &Path, record: &ActiveRunRecord) -> Result<PathBuf, MutationError> {
    let recorded_root = decode_active_root(record).map_err(|message| MutationError::InvalidJournal {
        path: pointer.to_path_buf(),
        message,
    })?;
    let canonical = canonical_root(&recorded_root).map_err(|error| MutationError::InvalidJournal {
        path: pointer.to_path_buf(),
        message: format!("recorded mutation root is unavailable or unsafe: {error}"),
    })?;
    if canonical != recorded_root || record.root_key != project_state_key(&canonical) {
        return Err(MutationError::InvalidJournal {
            path: pointer.to_path_buf(),
            message: "recorded mutation root no longer resolves to its canonical identity".into(),
        });
    }
    if let Some(expected) = &record.root_identity {
        if root_identity(&canonical)?.as_ref() != Some(expected) {
            return Err(MutationError::InvalidJournal {
                path: pointer.to_path_buf(),
                message: "recorded mutation root path now resolves to a different filesystem identity".into(),
            });
        }
    }
    Ok(canonical)
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn existing_recovery_target(root: &Path, file: &str, journal: &Path) -> Result<PathBuf, MutationError> {
    let relative = relative_source_path(file)?;
    let unresolved = root.join(&relative);
    match fs::symlink_metadata(&unresolved) {
        Ok(_) => resolve_source_path(root, file, false),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(MutationError::RecoveryConflict {
            path: unresolved,
            journal: journal.to_path_buf(),
        }),
        Err(source) => Err(MutationError::io(
            "inspect mutation source before recovery",
            unresolved,
            source,
        )),
    }
}

fn read_current_for_recovery(
    path: &Path,
    journal: &Path,
    max_source_bytes: usize,
) -> Result<Vec<u8>, MutationError> {
    match read_bounded_source(path, max_source_bytes) {
        Ok((_, bytes, _permissions)) => Ok(bytes),
        Err(MutationError::SourceTooLarge { .. } | MutationError::UnsupportedSourceMetadata { .. }) => {
            Err(MutationError::RecoveryConflict {
                path: path.to_path_buf(),
                journal: journal.to_path_buf(),
            })
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn recover_active_locked(
    requested_root: &Path,
    requested_state: &Path,
) -> Result<RecoveryAction, MutationError> {
    let base = state_base(requested_state)?;
    if let Some((pointer, active)) = read_active_run(base)? {
        let recorded_root = active_root_for_recovery(base, &pointer, &active, requested_root)?;
        if !roots_overlap(requested_root, &recorded_root) {
            return Err(MutationError::PendingMutationRoot {
                active_root: recorded_root,
                requested_root: requested_root.to_path_buf(),
            });
        }
        let active_state = base.join(&active.root_key);
        validate_pointed_state(base, &active_state)?;
        let active_action =
            recover_project_journal(&recorded_root, &active_state, Some((&pointer, &active)))?;
        if active_state == requested_state {
            return Ok(active_action);
        }
        let requested_action = recover_project_journal(requested_root, requested_state, None)?;
        return Ok(combine_recovery_actions(active_action, requested_action));
    }
    recover_project_journal(requested_root, requested_state, None)
}

fn active_root_for_recovery(
    base: &Path,
    pointer: &Path,
    active: &ActiveRunRecord,
    requested_root: &Path,
) -> Result<PathBuf, MutationError> {
    match validated_active_root(pointer, active) {
        Ok(root) => Ok(root),
        Err(validation_error) => {
            let recorded = decode_active_root(active).map_err(|message| MutationError::InvalidJournal {
                path: pointer.to_path_buf(),
                message,
            })?;
            if recorded.exists() {
                return Err(validation_error);
            }
            if let Some(expected) = &active.root_identity {
                if root_identity(requested_root)?.as_ref() != Some(expected) {
                    return Err(MutationError::InvalidJournal {
                        path: pointer.to_path_buf(),
                        message: format!(
                            "recorded mutation root {} moved or disappeared, but {} has a different filesystem identity",
                            recorded.display(),
                            requested_root.display()
                        ),
                    });
                }
            }
            let journal = base.join(&active.root_key).join(ACTIVE_JOURNAL);
            let target = existing_recovery_target(requested_root, &active.file, &journal)?;
            let current = read_current_for_recovery(&target, &journal, MAX_MUTATION_SOURCE_BYTES)?;
            let current_hash = sha256(&current);
            if current_hash == active.original_sha256 || current_hash == active.mutated_sha256 {
                Ok(requested_root.to_path_buf())
            } else {
                Err(MutationError::InvalidJournal {
                    path: pointer.to_path_buf(),
                    message: format!(
                        "recorded mutation root {} is unavailable and {} does not contain the recorded source content",
                        recorded.display(),
                        requested_root.display()
                    ),
                })
            }
        }
    }
}

fn validate_pointed_state(base: &Path, state: &Path) -> Result<(), MutationError> {
    if state.parent() != Some(base) {
        return Err(MutationError::UnsafePath {
            path: state.to_path_buf(),
            message: "pointed mutation state is not an immediate child of the global state base".into(),
        });
    }
    match fs::symlink_metadata(state) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(MutationError::UnsafePath {
                path: state.to_path_buf(),
                message: "pointed mutation state is not a regular directory".into(),
            })
        }
        Ok(_) => {
            let canonical = state
                .canonicalize()
                .map_err(|source| MutationError::io("canonicalize pointed mutation state", state, source))?;
            if canonical != state {
                return Err(MutationError::UnsafePath {
                    path: state.to_path_buf(),
                    message: "pointed mutation state changed canonical identity".into(),
                });
            }
            Ok(())
        }
        // A pointer without a project directory is handled conservatively by
        // pointer-only recovery; it may only be cleared when source is proven
        // to match the recorded original checksum.
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MutationError::io("inspect pointed mutation state", state, source)),
    }
}

const fn combine_recovery_actions(left: RecoveryAction, right: RecoveryAction) -> RecoveryAction {
    match (left, right) {
        (RecoveryAction::Restored, _) | (_, RecoveryAction::Restored) => RecoveryAction::Restored,
        (RecoveryAction::AlreadyClean, _) | (_, RecoveryAction::AlreadyClean) => RecoveryAction::AlreadyClean,
        _ => RecoveryAction::None,
    }
}

fn recover_project_journal(
    root: &Path,
    state: &Path,
    active: Option<(&Path, &ActiveRunRecord)>,
) -> Result<RecoveryAction, MutationError> {
    let Some((journal, record)) = read_journal(state)? else {
        return match active {
            Some((pointer, active)) => recover_pointer_without_journal(root, state, pointer, active),
            None => Ok(RecoveryAction::None),
        };
    };
    if let Some((pointer, active)) = active {
        if active.file != record.file
            || active.original_sha256 != record.original_sha256
            || active.mutated_sha256 != record.mutated_sha256
            || journal != state.join(ACTIVE_JOURNAL)
        {
            return Err(MutationError::InvalidJournal {
                path: pointer.to_path_buf(),
                message: "global active pointer does not match its project recovery journal".into(),
            });
        }
    }
    let original = BASE64
        .decode(&record.original_base64)
        .map_err(|error| MutationError::InvalidJournal {
            path: journal.clone(),
            message: format!("invalid original content: {error}"),
        })?;
    if record.max_source_bytes == 0
        || record.max_source_bytes > MAX_MUTATION_SOURCE_BYTES
        || original.len() > record.max_source_bytes
    {
        return Err(MutationError::InvalidJournal {
            path: journal,
            message: format!(
                "original content or recorded limit exceeds the {MAX_MUTATION_SOURCE_BYTES}-byte recovery ceiling"
            ),
        });
    }
    if sha256(&original) != record.original_sha256 {
        return Err(MutationError::InvalidJournal {
            path: journal,
            message: "original content checksum does not match".into(),
        });
    }
    let target = existing_recovery_target(root, &record.file, &journal)?;
    let current = read_current_for_recovery(&target, &journal, record.max_source_bytes)?;
    if sha256(&current) == record.original_sha256 {
        if let Some((pointer, _)) = active {
            remove_active_run(pointer)?;
        }
        remove_journal(&journal)?;
        return Ok(RecoveryAction::AlreadyClean);
    }
    if sha256(&current) != record.mutated_sha256 {
        return Err(MutationError::RecoveryConflict {
            path: target,
            journal,
        });
    }
    atomic_replace(&target, &original, Some(&record.original_permissions))?;
    if let Some((pointer, _)) = active {
        remove_active_run(pointer)?;
    }
    remove_journal(&journal)?;
    Ok(RecoveryAction::Restored)
}

fn recover_pointer_without_journal(
    root: &Path,
    state: &Path,
    pointer: &Path,
    active: &ActiveRunRecord,
) -> Result<RecoveryAction, MutationError> {
    let journal = journal_path(state);
    let target = existing_recovery_target(root, &active.file, &journal)?;
    let current = read_current_for_recovery(&target, &journal, MAX_MUTATION_SOURCE_BYTES)?;
    if sha256(&current) == active.original_sha256 {
        remove_active_run(pointer)?;
        return Ok(RecoveryAction::AlreadyClean);
    }
    let message = if sha256(&current) == active.mutated_sha256 {
        "source still matches the active mutant and the original bytes are unavailable"
    } else {
        "source differs from both recorded checksums and the original bytes are unavailable"
    };
    Err(MutationError::MissingRecoveryJournal {
        path: target,
        pointer: pointer.to_path_buf(),
        journal,
        message: message.into(),
    })
}

#[derive(Debug)]
pub(crate) struct SourceRestoreGuard {
    root: PathBuf,
    file: String,
    original: Vec<u8>,
    original_sha256: String,
    mutated_sha256: String,
    max_source_bytes: usize,
    permissions: StoredPermissions,
    journal: PathBuf,
    active_pointer: PathBuf,
    restored: bool,
}

impl SourceRestoreGuard {
    #[cfg(test)]
    pub(crate) fn apply(
        root: &Path,
        state: &Path,
        mutation: &MutationCandidate,
    ) -> Result<Self, ApplyMutationError> {
        Self::apply_bounded(root, state, mutation, MAX_MUTATION_SOURCE_BYTES)
    }

    pub(crate) fn apply_bounded(
        root: &Path,
        state: &Path,
        mutation: &MutationCandidate,
        max_source_bytes: usize,
    ) -> Result<Self, ApplyMutationError> {
        Self::apply_with_limits(root, state, mutation, max_source_bytes, MAX_JOURNAL_BYTES)
    }

    #[cfg(test)]
    fn apply_with_journal_limit(
        root: &Path,
        state: &Path,
        mutation: &MutationCandidate,
        max_journal_bytes: u64,
    ) -> Result<Self, ApplyMutationError> {
        Self::apply_with_limits(
            root,
            state,
            mutation,
            MAX_MUTATION_SOURCE_BYTES,
            max_journal_bytes,
        )
    }

    fn apply_with_limits(
        root: &Path,
        state: &Path,
        mutation: &MutationCandidate,
        max_source_bytes: usize,
        max_journal_bytes: u64,
    ) -> Result<Self, ApplyMutationError> {
        let max_source_bytes = max_source_bytes.min(MAX_MUTATION_SOURCE_BYTES);
        if max_source_bytes == 0 {
            return Err(ApplyMutationError::Invalid(
                "executable mutation source limit must be greater than zero".into(),
            ));
        }
        let path = resolve_source_path(root, &mutation.file, false)?;
        let (_metadata, original, permissions) = read_bounded_source(&path, max_source_bytes)?;
        if mutation.start_byte > mutation.end_byte || mutation.end_byte > original.len() {
            return Err(ApplyMutationError::Invalid(format!(
                "byte range {}..{} is outside a {}-byte source file",
                mutation.start_byte,
                mutation.end_byte,
                original.len()
            )));
        }
        if original[mutation.start_byte..mutation.end_byte] != *mutation.original.as_bytes() {
            return Err(ApplyMutationError::Invalid(
                "candidate original text no longer matches the source bytes".into(),
            ));
        }
        if mutation.original == mutation.replacement {
            return Err(ApplyMutationError::Invalid(
                "mutation replacement is identical to the original text".into(),
            ));
        }
        let removed = mutation.end_byte - mutation.start_byte;
        let mutated_len = original
            .len()
            .checked_sub(removed)
            .and_then(|remaining| remaining.checked_add(mutation.replacement.len()))
            .ok_or_else(|| {
                ApplyMutationError::Invalid("candidate replacement size overflows addressable memory".into())
            })?;
        if mutated_len > max_source_bytes || mutated_len > MAX_MUTATION_SOURCE_BYTES {
            return Err(ApplyMutationError::Invalid(format!(
                "mutated source would be {mutated_len} bytes, exceeding the executable mutation limit of {} bytes",
                max_source_bytes.min(MAX_MUTATION_SOURCE_BYTES)
            )));
        }
        let mut mutated = Vec::with_capacity(mutated_len);
        mutated.extend_from_slice(&original[..mutation.start_byte]);
        mutated.extend_from_slice(mutation.replacement.as_bytes());
        mutated.extend_from_slice(&original[mutation.end_byte..]);

        let original_sha256 = sha256(&original);
        let mutated_sha256 = sha256(&mutated);
        let record = JournalRecord {
            schema_version: JOURNAL_SCHEMA_VERSION,
            file: mutation.file.clone(),
            original_base64: BASE64.encode(&original),
            original_sha256: original_sha256.clone(),
            mutated_sha256: mutated_sha256.clone(),
            max_source_bytes,
            original_permissions: permissions.clone(),
        };
        let journal = write_journal_with_limit(state, &record, max_journal_bytes)?;
        let active_pointer = match write_active_run(root, state, &record) {
            Ok(path) => path,
            Err(error) => {
                // Pointer persistence can fail after its atomic rename has
                // committed. Retaining the already-synced journal is the only
                // crash-safe choice; recovery will prove the source is still
                // original and clear both artifacts when appropriate.
                return Err(ApplyMutationError::Fatal(error));
            }
        };
        if let Err(error) = atomic_replace(&path, &mutated, Some(&permissions)) {
            let _ = recover_active_locked(root, state);
            return Err(ApplyMutationError::Fatal(error));
        }
        Ok(Self {
            root: root.to_path_buf(),
            file: mutation.file.clone(),
            original,
            original_sha256,
            mutated_sha256,
            max_source_bytes,
            permissions,
            journal,
            active_pointer,
            restored: false,
        })
    }

    pub(crate) fn restore(&mut self) -> Result<(), MutationError> {
        if self.restored {
            return Ok(());
        }
        let path = existing_recovery_target(&self.root, &self.file, &self.journal)?;
        let current = read_current_for_recovery(&path, &self.journal, self.max_source_bytes)?;
        let current_sha256 = sha256(&current);
        if current_sha256 == self.original_sha256 {
            remove_active_run(&self.active_pointer)?;
            remove_journal(&self.journal)?;
            self.restored = true;
            return Ok(());
        }
        if current_sha256 != self.mutated_sha256 {
            return Err(MutationError::RecoveryConflict {
                path,
                journal: self.journal.clone(),
            });
        }
        atomic_replace(&path, &self.original, Some(&self.permissions))?;
        remove_active_run(&self.active_pointer)?;
        remove_journal(&self.journal)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for SourceRestoreGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use std::mem;

    use reporigor_core::{Language, MutationCandidate};
    use tempfile::tempdir;

    use super::*;

    fn candidate(file: &str) -> MutationCandidate {
        MutationCandidate {
            id: 1,
            language: Language::Rust,
            file: file.into(),
            line: 1,
            column: 1,
            original: "true".into(),
            replacement: "false".into(),
            start_byte: 0,
            end_byte: 4,
        }
    }

    #[test]
    fn source_guard_atomically_restores_original_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;

        {
            let _guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
            assert_eq!(fs::read(&path)?, b"false\n");
            assert!(journal_path(&state).is_file());
        }

        assert_eq!(fs::read(&path)?, b"true\n");
        assert!(!journal_path(&state).exists());
        let temporary_files = fs::read_dir(directory.path())?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
        Ok(())
    }

    #[test]
    fn interrupted_mutation_is_recovered_from_journal() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);

        assert_eq!(fs::read(&path)?, b"false\n");
        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        assert_eq!(fs::read(&path)?, b"true\n");
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_follows_a_renamed_checkout_with_the_same_root_identity(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let parent = tempdir()?;
        let state_base = tempdir()?;
        let original_path = parent.path().join("original");
        let moved_path = parent.path().join("moved");
        fs::create_dir(&original_path)?;
        fs::write(original_path.join("sample.rs"), b"true\n")?;
        let original_root = canonical_root(&original_path)?;
        let original_state = prepare_state_directory_in(&original_root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&original_root, &original_state, &candidate("sample.rs"))?;
        mem::forget(guard);
        fs::rename(&original_path, &moved_path)?;
        let moved_root = canonical_root(&moved_path)?;
        let moved_state = prepare_state_directory_in(&moved_root, state_base.path())?;

        assert_eq!(
            recover_active_locked(&moved_root, &moved_state)?,
            RecoveryAction::Restored
        );
        assert_eq!(fs::read(moved_path.join("sample.rs"))?, b"true\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_different_checkout_recreated_at_the_same_path(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let parent = tempdir()?;
        let state_base = tempdir()?;
        let checkout_path = parent.path().join("checkout");
        let displaced_path = parent.path().join("displaced");
        fs::create_dir(&checkout_path)?;
        fs::write(checkout_path.join("sample.rs"), b"true\n")?;
        let original_root = canonical_root(&checkout_path)?;
        let state = prepare_state_directory_in(&original_root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&original_root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);
        fs::rename(&checkout_path, &displaced_path)?;
        fs::create_dir(&checkout_path)?;
        fs::write(checkout_path.join("sample.rs"), b"false\n")?;
        let replacement_root = canonical_root(&checkout_path)?;

        assert!(matches!(
            recover_active_locked(&replacement_root, &state),
            Err(MutationError::InvalidJournal { .. })
        ));
        assert_eq!(fs::read(checkout_path.join("sample.rs"))?, b"false\n");
        Ok(())
    }

    #[test]
    fn pending_journal_detection_is_read_only_and_tracks_recovery() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let root = canonical_root(directory.path())?;
        let absent_state = state_base.path().join("absent");
        assert!(pending_journal_in_state(&absent_state)?.is_none());
        assert!(!absent_state.exists(), "detection must not create state");

        let path = root.join("sample.rs");
        fs::write(&path, b"true\n")?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        assert!(pending_journal_in_state(&state)?.is_none());
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);

        assert_eq!(pending_journal_in_state(&state)?, Some(journal_path(&state)));
        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        assert!(pending_journal_in_state(&state)?.is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_restores_original_file_mode() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        fs::set_permissions(&path, Permissions::from_mode(0o744))?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);
        fs::set_permissions(&path, Permissions::from_mode(0o600))?;

        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o744);
        assert_eq!(fs::read(&path)?, b"true\n");
        Ok(())
    }

    #[test]
    fn recovery_restores_original_file_timestamps() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let original_modified = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(&path)?
            .set_times(FileTimes::new().set_modified(original_modified))?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);
        File::options()
            .write(true)
            .open(&path)?
            .set_times(FileTimes::new().set_modified(SystemTime::now()))?;

        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        assert_eq!(fs::metadata(&path)?.modified()?, original_modified);
        assert_eq!(fs::read(&path)?, b"true\n");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn recovery_restores_extended_attributes() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let file = File::open(&path)?;
        #[cfg(target_os = "linux")]
        let attribute = "user.reporigor-test";
        #[cfg(target_vendor = "apple")]
        let attribute = "com.reporigor.test";
        match rustix::fs::fsetxattr(&file, attribute, b"retained", rustix::fs::XattrFlags::empty()) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::NOTSUP => return Ok(()),
            Err(error) => {
                return Err(std::io::Error::from_raw_os_error(error.raw_os_error()).into());
            }
        }
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);
        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        let file = File::open(&path)?;
        let mut value = vec![0_u8; 32];
        let length = rustix::fs::fgetxattr(&file, attribute, &mut value)
            .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
        value.truncate(length);
        assert_eq!(value, b"retained");
        assert_eq!(fs::read(&path)?, b"true\n");
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn recovery_does_not_overwrite_unrecognized_content() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);
        fs::write(&path, b"manual edit\n")?;

        assert!(matches!(
            recover_active_locked(&root, &state),
            Err(MutationError::RecoveryConflict { .. })
        ));
        assert_eq!(fs::read(&path)?, b"manual edit\n");
        assert!(journal_path(&state).is_file());
        Ok(())
    }

    #[test]
    fn normal_guard_refuses_to_overwrite_an_independent_edit() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let mut guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        fs::write(&path, b"independent edit\n")?;

        let error = match guard.restore() {
            Ok(()) => {
                return Err(std::io::Error::other(
                    "independent source edit was overwritten instead of conflicting",
                )
                .into());
            }
            Err(error) => error,
        };
        assert!(matches!(&error, MutationError::RecoveryConflict { .. }));
        assert!(error.to_string().contains("source was left unchanged"));
        drop(guard);

        assert_eq!(fs::read(&path)?, b"independent edit\n");
        assert!(journal_path(&state).is_file(), "recovery data must be retained");

        fs::write(&path, b"false\n")?;
        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        assert_eq!(fs::read(&path)?, b"true\n");
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn oversized_journal_is_rejected_before_source_replacement() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;

        let result = SourceRestoreGuard::apply_with_journal_limit(&root, &state, &candidate("sample.rs"), 64);
        assert!(matches!(
            result,
            Err(ApplyMutationError::Fatal(MutationError::InvalidJournal { .. }))
        ));
        assert_eq!(fs::read(&path)?, b"true\n");
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn sparse_huge_source_is_rejected_before_journaling_or_allocation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(1024 * 1024 * 1024)?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;

        let result = SourceRestoreGuard::apply_bounded(&root, &state, &candidate("sample.rs"), 1024);
        assert!(matches!(
            result,
            Err(ApplyMutationError::Fatal(MutationError::SourceTooLarge { .. }))
        ));
        assert_eq!(fs::metadata(&path)?.len(), 1024 * 1024 * 1024);
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn oversized_replacement_is_rejected_before_journaling() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let mut oversized = candidate("sample.rs");
        oversized.replacement = "x".repeat(32);

        assert!(matches!(
            SourceRestoreGuard::apply_bounded(&root, &state, &oversized, 16),
            Err(ApplyMutationError::Invalid(_))
        ));
        assert_eq!(fs::read(&path)?, b"true\n");
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn sparse_growth_after_apply_is_a_recoverable_conflict() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let mut guard = SourceRestoreGuard::apply_bounded(&root, &state, &candidate("sample.rs"), 1024)?;
        OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(1024 * 1024 * 1024)?;

        assert!(matches!(
            guard.restore(),
            Err(MutationError::RecoveryConflict { .. })
        ));
        assert_eq!(fs::metadata(&path)?.len(), 1024 * 1024 * 1024);
        assert!(journal_path(&state).exists());
        drop(guard);

        fs::write(&path, b"false\n")?;
        assert_eq!(recover_active_locked(&root, &state)?, RecoveryAction::Restored);
        assert_eq!(fs::read(&path)?, b"true\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_source_is_rejected_before_journaling() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::MetadataExt;

        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        let linked = directory.path().join("linked.rs");
        fs::write(&path, b"true\n")?;
        fs::hard_link(&path, &linked)?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;

        assert!(matches!(
            SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs")),
            Err(ApplyMutationError::Fatal(
                MutationError::UnsupportedSourceMetadata { .. }
            ))
        ));
        assert_eq!(fs::metadata(&path)?.ino(), fs::metadata(&linked)?.ino());
        assert_eq!(fs::read(&path)?, b"true\n");
        assert_eq!(fs::read(&linked)?, b"true\n");
        assert!(!journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn recovery_never_recreates_a_missing_source() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let path = directory.path().join("sample.rs");
        fs::write(&path, b"true\n")?;
        let root = canonical_root(directory.path())?;
        let state = prepare_state_directory_in(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs"))?;
        mem::forget(guard);
        fs::remove_file(&path)?;

        assert!(matches!(
            recover_active_locked(&root, &state),
            Err(MutationError::RecoveryConflict { .. })
        ));
        assert!(!path.exists());
        assert!(journal_path(&state).exists());
        Ok(())
    }

    #[test]
    fn path_traversal_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let root = canonical_root(directory.path())?;
        assert!(matches!(
            resolve_source_path(&root, "../outside.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));
        assert!(matches!(
            resolve_source_path(&root, "/outside.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));
        Ok(())
    }

    #[test]
    fn version_control_and_tool_state_paths_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let root = canonical_root(directory.path())?;
        for path in [".git/config", "nested/.hg/store", "target/reporigor/active.json"] {
            assert!(matches!(
                resolve_source_path(&root, path, false),
                Err(MutationError::UnsafePath { .. })
            ));
        }

        let control_root = directory.path().join(".git/worktree");
        fs::create_dir_all(&control_root)?;
        fs::write(control_root.join("source.rs"), b"true\n")?;
        let control_root = canonical_root(&control_root)?;
        assert!(matches!(
            resolve_source_path(&control_root, "source.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn source_and_state_symlinks_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let directory = tempdir()?;
        let outside = tempdir()?;
        let outside_source = outside.path().join("outside.rs");
        fs::write(&outside_source, b"true\n")?;
        symlink(&outside_source, directory.path().join("linked.rs"))?;
        let root = canonical_root(directory.path())?;
        assert!(matches!(
            resolve_source_path(&root, "linked.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));

        let state_root = tempdir()?;
        let state_link = state_root.path().join("state-link");
        symlink(outside.path(), &state_link)?;
        assert!(matches!(
            RunLockGuard::acquire_with_base(&root, &state_link),
            Err(MutationError::UnsafePath { .. })
        ));
        Ok(())
    }

    #[test]
    fn run_lock_rejects_a_second_executor() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let root = canonical_root(directory.path())?;
        let first = RunLockGuard::acquire_with_base(&root, state_base.path())?;
        assert!(matches!(
            RunLockGuard::acquire_with_base(&root, state_base.path()),
            Err(MutationError::AlreadyRunning { .. })
        ));
        drop(first);
        RunLockGuard::acquire_with_base(&root, state_base.path())?;
        Ok(())
    }

    #[test]
    fn global_lock_serializes_different_and_potentially_overlapping_roots(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let nested = workspace.path().join("package");
        fs::create_dir(&nested)?;
        let state_base = tempdir()?;
        let workspace_root = canonical_root(workspace.path())?;
        let nested_root = canonical_root(&nested)?;

        let first = RunLockGuard::acquire_with_base(&workspace_root, state_base.path())?;
        assert!(matches!(
            RunLockGuard::acquire_with_base(&nested_root, state_base.path()),
            Err(MutationError::AlreadyRunning { .. })
        ));
        drop(first);
        RunLockGuard::acquire_with_base(&nested_root, state_base.path())?;
        Ok(())
    }

    #[test]
    fn shared_analysis_locks_coexist_and_a_writer_waits_for_release() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let root = canonical_root(directory.path())?;
        let first = RunLockGuard::acquire_shared_with_base(&root, state_base.path())?;
        let second = RunLockGuard::acquire_shared_with_base(&root, state_base.path())?;
        assert!(matches!(
            RunLockGuard::acquire_with_base(&root, state_base.path()),
            Err(MutationError::AlreadyRunning { .. })
        ));
        drop(second);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(first);
        });
        let writer = RunLockGuard::acquire_waiting_with_base(&root, state_base.path())?;
        release
            .join()
            .map_err(|_| std::io::Error::other("reader release thread panicked"))?;
        drop(writer);
        Ok(())
    }

    #[test]
    fn project_target_cleanup_cannot_remove_lock_or_journal() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let state_base = tempdir()?;
        let root = canonical_root(directory.path())?;
        let source = root.join("sample.rs");
        fs::write(&source, b"true\n")?;
        fs::create_dir(root.join("target"))?;
        let lock = RunLockGuard::acquire_with_base(&root, state_base.path())?;
        let mut guard = SourceRestoreGuard::apply(&root, lock.state(), &candidate("sample.rs"))?;
        let journal = journal_path(lock.state());
        assert!(journal.is_file());

        fs::remove_dir_all(root.join("target"))?;

        assert!(journal.is_file());
        assert!(matches!(
            RunLockGuard::acquire_with_base(&root, state_base.path()),
            Err(MutationError::AlreadyRunning { .. })
        ));
        guard.restore()?;
        assert_eq!(fs::read(&source)?, b"true\n");
        assert!(!journal.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn state_directories_and_files_are_owner_only() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir()?;
        let state_base = tempdir()?;
        let root = canonical_root(directory.path())?;
        fs::write(root.join("sample.rs"), b"true\n")?;
        let lock = RunLockGuard::acquire_with_base(&root, state_base.path())?;
        let guard = SourceRestoreGuard::apply(&root, lock.state(), &candidate("sample.rs"))?;

        assert_eq!(
            fs::metadata(state_base.path())?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::metadata(lock.state())?.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(state_base.path().join(RUN_LOCK))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(journal_path(lock.state()))?.permissions().mode() & 0o777,
            0o600
        );
        drop(guard);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn override_parent_permissions_are_never_changed() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir()?;
        let shared_parent = tempdir()?;
        fs::set_permissions(shared_parent.path(), Permissions::from_mode(0o755))?;
        let root = canonical_root(directory.path())?;
        let dedicated_base = state_base_below_override(shared_parent.path());

        let state = prepare_state_directory_in(&root, &dedicated_base)?;

        assert_eq!(
            fs::metadata(shared_parent.path())?.permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::metadata(&dedicated_base)?.permissions().mode() & 0o777, 0o700);
        assert!(state.starts_with(dedicated_base.canonicalize()?));
        Ok(())
    }
}
