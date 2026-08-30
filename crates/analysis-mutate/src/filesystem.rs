use std::collections::HashMap;
#[cfg(any(unix, windows))]
use std::ffi::OsString;
use std::fs::{self, File, FileTimes, Metadata, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use reporigor_core::MutationCandidate;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::error::PathMessageKind;
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
const EXCLUSIVE_LOCK_WAIT: Duration = Duration::from_secs(3);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
/// Hard ceiling for source bytes held by executable mutation and recovery.
pub const MAX_MUTATION_SOURCE_BYTES: usize = 32 * 1024 * 1024;

fn reject_if(condition: bool, error: impl FnOnce() -> MutationError) -> Result<(), MutationError> {
    if condition {
        Err(error())
    } else {
        Ok(())
    }
}

trait MutationIoResult<T> {
    fn for_path(self, operation: &'static str, path: impl Into<PathBuf>) -> Result<T, MutationError>;
}

enum PermissionTarget<'a> {
    Path(&'a Path),
    File { file: &'a File, path: &'a Path },
}

impl PermissionTarget<'_> {
    fn restrict(self, unix_mode: u32, operation: &'static str) -> Result<(), MutationError> {
        #[cfg(unix)]
        {
            let permissions = Permissions::from_mode(unix_mode);
            match self {
                Self::Path(path) => fs::set_permissions(path, permissions).for_path(operation, path),
                Self::File { file, path } => file.set_permissions(permissions).for_path(operation, path),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (self, unix_mode, operation);
            Ok(())
        }
    }
}

impl<T, E> MutationIoResult<T> for Result<T, E>
where
    E: Into<std::io::Error>,
{
    fn for_path(self, operation: &'static str, path: impl Into<PathBuf>) -> Result<T, MutationError> {
        self.map_err(|source| MutationError::io(operation, path, source))
    }
}

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
    root_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_encoding: Option<String>,
    file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_base64: Option<String>,
    original_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_identity: Option<RootIdentity>,
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
    accessed: Option<StoredFileTime>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extended_attributes: Vec<StoredExtendedAttribute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified: Option<StoredFileTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
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

    fn apply(
        &self,
        file: &File,
        path: &Path,
        fallback: Permissions,
        restore_timestamps: bool,
    ) -> Result<(), MutationError> {
        file.set_permissions(self.materialize(fallback))
            .for_path("set replacement permissions", path)?;
        if restore_timestamps {
            self.restore_timestamps(file, path)?;
        }
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        apply_extended_attributes(path, file, &self.extended_attributes)?;
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        if !self.extended_attributes.is_empty() {
            return Err(MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                "stored extended attributes cannot be restored on this platform",
            ));
        }
        Ok(())
    }

    fn restore_timestamps(&self, file: &File, path: &Path) -> Result<(), MutationError> {
        let Some(times) = self.materialized_times(path)? else {
            return Ok(());
        };
        file.set_times(times).for_path("set replacement timestamps", path)
    }

    fn materialized_times(&self, path: &Path) -> Result<Option<FileTimes>, MutationError> {
        let accessed = materialize_optional_time(self.accessed, path, "access")?;
        let modified = materialize_optional_time(self.modified, path, "modification")?;
        if accessed.is_none() && modified.is_none() {
            return Ok(None);
        }
        let times = accessed.map_or_else(FileTimes::new, |value| FileTimes::new().set_accessed(value));
        Ok(Some(modified.map_or(times, |value| times.set_modified(value))))
    }
}

fn materialize_optional_time(
    stored: Option<StoredFileTime>,
    path: &Path,
    label: &str,
) -> Result<Option<SystemTime>, MutationError> {
    stored
        .map(|value| {
            value.materialize().ok_or_else(|| {
                MutationError::path_message(
                    PathMessageKind::UnsupportedMetadata,
                    path,
                    format!("stored {label} time is outside the platform time range"),
                )
            })
        })
        .transpose()
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
mod extended_attributes {
    use super::{
        reject_if, Engine, File, MutationError, MutationIoResult, Path, PathMessageKind,
        StoredExtendedAttribute, BASE64,
    };

    const MAX_EXTENDED_ATTRIBUTE_NAME_BYTES: usize = 64 * 1024;
    const MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES: usize = 1024 * 1024;
    const MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES: usize = 4 * 1024 * 1024;
    const MAX_EXTENDED_ATTRIBUTES: usize = 256;

    pub(super) fn capture_extended_attributes(
        path: &Path,
        file: &File,
    ) -> Result<Vec<StoredExtendedAttribute>, MutationError> {
        let names = list_extended_attribute_names(path, file)?;
        let mut stored = Vec::with_capacity(names.len());
        let mut retained_bytes = 0_usize;
        for name in names {
            stored.push(capture_extended_attribute(path, file, name, &mut retained_bytes)?);
        }
        Ok(stored)
    }

    fn capture_extended_attribute(
        path: &Path,
        file: &File,
        name: Vec<u8>,
        retained_bytes: &mut usize,
    ) -> Result<StoredExtendedAttribute, MutationError> {
        let name = extended_attribute_name(path, name)?;
        let value = read_extended_attribute_value(path, file, &name)?;
        *retained_bytes =
            checked_extended_attribute_total(path, *retained_bytes, name.as_bytes().len(), value.len())?;
        Ok(StoredExtendedAttribute {
            name_base64: BASE64.encode(name.as_bytes()),
            value_base64: BASE64.encode(value),
        })
    }

    fn extended_attribute_name(path: &Path, name: Vec<u8>) -> Result<std::ffi::CString, MutationError> {
        std::ffi::CString::new(name).map_err(|_| {
            MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                "extended-attribute name contains an embedded NUL byte",
            )
        })
    }

    fn read_extended_attribute_value(
        path: &Path,
        file: &File,
        name: &std::ffi::CStr,
    ) -> Result<Vec<u8>, MutationError> {
        let mut value = vec![0_u8; MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES];
        let length = match rustix::fs::fgetxattr(file, name, &mut value) {
            Ok(length) => length,
            Err(error) => return Err(classify_extended_attribute_read_error(path, name, error)),
        };
        value.truncate(length);
        Ok(value)
    }

    fn classify_extended_attribute_read_error(
        path: &Path,
        name: &std::ffi::CStr,
        error: rustix::io::Errno,
    ) -> MutationError {
        if error == rustix::io::Errno::RANGE {
            return MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                format!("extended attribute {name:?} exceeds {MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES} bytes"),
            );
        }
        MutationError::io("read mutation source extended attribute", path, error)
    }

    fn checked_extended_attribute_total(
        path: &Path,
        retained: usize,
        name_bytes: usize,
        value_bytes: usize,
    ) -> Result<usize, MutationError> {
        let total = retained
            .checked_add(name_bytes)
            .and_then(|value| value.checked_add(value_bytes))
            .ok_or_else(|| {
                MutationError::path_message(
                    PathMessageKind::UnsupportedMetadata,
                    path,
                    "extended-attribute size overflowed",
                )
            })?;
        if total > MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES {
            return Err(MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                format!("extended attributes exceed {MAX_EXTENDED_ATTRIBUTE_TOTAL_BYTES} retained bytes"),
            ));
        }
        Ok(total)
    }

    fn list_extended_attribute_names(path: &Path, file: &File) -> Result<Vec<Vec<u8>>, MutationError> {
        let Some(required) = extended_attribute_name_bytes(path, file)? else {
            return Ok(Vec::new());
        };
        read_extended_attribute_name_list(path, file, required)
    }

    fn read_extended_attribute_name_list(
        path: &Path,
        file: &File,
        required: usize,
    ) -> Result<Vec<Vec<u8>>, MutationError> {
        validate_extended_attribute_name_bytes(path, required)?;
        let mut encoded = vec![0_u8; required];
        let actual = read_extended_attribute_names(path, file, &mut encoded)?;
        if actual > required {
            return Err(MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                "extended attributes changed while they were being captured",
            ));
        }
        encoded.truncate(actual);
        decode_extended_attribute_names(path, &encoded)
    }

    fn extended_attribute_name_bytes(path: &Path, file: &File) -> Result<Option<usize>, MutationError> {
        let mut query = [0_u8; 0];
        match rustix::fs::flistxattr(file, &mut query) {
            Ok(required) => Ok((required != 0).then_some(required)),
            Err(error) => classify_extended_attribute_list_error(path, error),
        }
    }

    fn classify_extended_attribute_list_error(
        path: &Path,
        error: rustix::io::Errno,
    ) -> Result<Option<usize>, MutationError> {
        if error == rustix::io::Errno::NOTSUP {
            return Ok(None);
        }
        Err(MutationError::io(
            "list mutation source extended attributes",
            path,
            error,
        ))
    }

    fn validate_extended_attribute_name_bytes(path: &Path, required: usize) -> Result<(), MutationError> {
        reject_if(required > MAX_EXTENDED_ATTRIBUTE_NAME_BYTES, || {
            MutationError::path_message(
            PathMessageKind::UnsupportedMetadata,
            path,
            format!(
                "extended-attribute names require {required} bytes, exceeding {MAX_EXTENDED_ATTRIBUTE_NAME_BYTES}"
            ),
        )
        })
    }

    fn read_extended_attribute_names(
        path: &Path,
        file: &File,
        encoded: &mut [u8],
    ) -> Result<usize, MutationError> {
        rustix::fs::flistxattr(file, encoded).for_path("list mutation source extended attributes", path)
    }

    fn decode_extended_attribute_names(path: &Path, encoded: &[u8]) -> Result<Vec<Vec<u8>>, MutationError> {
        if encoded.last() != Some(&0) {
            return Err(MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                "extended-attribute name list is malformed",
            ));
        }
        let names = encoded
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        if names.len() > MAX_EXTENDED_ATTRIBUTES {
            return Err(MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                format!(
                    "file has {} extended attributes, exceeding {MAX_EXTENDED_ATTRIBUTES}",
                    names.len()
                ),
            ));
        }
        Ok(names)
    }

    pub(super) fn apply_extended_attributes(
        path: &Path,
        file: &File,
        stored: &[StoredExtendedAttribute],
    ) -> Result<(), MutationError> {
        let current = list_extended_attribute_names(path, file)?;
        let decoded = decode_extended_attributes(path, stored)?;
        let target = ExtendedAttributeFile { path, file };
        target.restore(&decoded)?;
        target.remove_unstored(&current, &decoded)
    }

    fn decode_extended_attributes(
        path: &Path,
        stored: &[StoredExtendedAttribute],
    ) -> Result<Vec<(std::ffi::CString, Vec<u8>)>, MutationError> {
        let mut decoded = Vec::with_capacity(stored.len());
        let mut retained_bytes = 0_usize;
        for attribute in stored {
            decoded.push(decode_extended_attribute(
                path,
                attribute,
                decoded.len(),
                &mut retained_bytes,
            )?);
        }
        Ok(decoded)
    }

    fn decode_extended_attribute(
        path: &Path,
        attribute: &StoredExtendedAttribute,
        decoded_count: usize,
        retained_bytes: &mut usize,
    ) -> Result<(std::ffi::CString, Vec<u8>), MutationError> {
        let (name, value) = validated_extended_attribute_payload(path, attribute, decoded_count)?;
        *retained_bytes = checked_extended_attribute_total(path, *retained_bytes, name.len(), value.len())?;
        Ok((extended_attribute_name(path, name)?, value))
    }

    fn validated_extended_attribute_payload(
        path: &Path,
        attribute: &StoredExtendedAttribute,
        decoded_count: usize,
    ) -> Result<(Vec<u8>, Vec<u8>), MutationError> {
        validate_encoded_extended_attribute(path, attribute)?;
        let (name, value) = decode_extended_attribute_payload(path, attribute)?;
        validate_decoded_extended_attribute(path, &name, &value, decoded_count)?;
        Ok((name, value))
    }

    fn decode_extended_attribute_payload(
        path: &Path,
        attribute: &StoredExtendedAttribute,
    ) -> Result<(Vec<u8>, Vec<u8>), MutationError> {
        let name = decode_extended_attribute_field(path, &attribute.name_base64, "name")?;
        let value = decode_extended_attribute_field(path, &attribute.value_base64, "value")?;
        Ok((name, value))
    }

    fn validate_encoded_extended_attribute(
        path: &Path,
        attribute: &StoredExtendedAttribute,
    ) -> Result<(), MutationError> {
        let name_too_large =
            attribute.name_base64.len() > MAX_EXTENDED_ATTRIBUTE_NAME_BYTES.saturating_mul(2);
        let value_too_large =
            attribute.value_base64.len() > MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES.saturating_mul(2);
        reject_if(name_too_large || value_too_large, || {
            extended_attribute_limit_error(path, "encoded")
        })
    }

    fn decode_extended_attribute_field(
        path: &Path,
        encoded: &str,
        field: &str,
    ) -> Result<Vec<u8>, MutationError> {
        BASE64.decode(encoded).map_err(|error| {
            MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                format!("invalid encoded extended-attribute {field}: {error}"),
            )
        })
    }

    fn validate_decoded_extended_attribute(
        path: &Path,
        name: &[u8],
        value: &[u8],
        decoded_count: usize,
    ) -> Result<(), MutationError> {
        let invalid_name = name.is_empty() || name.len() > MAX_EXTENDED_ATTRIBUTE_NAME_BYTES;
        let invalid_value = value.len() > MAX_EXTENDED_ATTRIBUTE_VALUE_BYTES;
        reject_if(
            invalid_name || invalid_value || decoded_count >= MAX_EXTENDED_ATTRIBUTES,
            || extended_attribute_limit_error(path, "decoded"),
        )
    }

    fn extended_attribute_limit_error(path: &Path, representation: &str) -> MutationError {
        MutationError::path_message(
            PathMessageKind::UnsupportedMetadata,
            path,
            format!("{representation} extended attribute exceeds its recovery limit"),
        )
    }

    struct ExtendedAttributeFile<'a> {
        path: &'a Path,
        file: &'a File,
    }

    impl ExtendedAttributeFile<'_> {
        fn restore(&self, decoded: &[(std::ffi::CString, Vec<u8>)]) -> Result<(), MutationError> {
            for (name, value) in decoded {
                rustix::fs::fsetxattr(self.file, name.as_c_str(), value, rustix::fs::XattrFlags::empty())
                    .for_path("restore mutation source extended attribute", self.path)?;
            }
            Ok(())
        }

        fn remove_unstored(
            &self,
            current: &[Vec<u8>],
            decoded: &[(std::ffi::CString, Vec<u8>)],
        ) -> Result<(), MutationError> {
            for name in current {
                if decoded.iter().any(|(stored, _)| stored.as_bytes() == name) {
                    continue;
                }
                let name = extended_attribute_name(self.path, name.clone())?;
                rustix::fs::fremovexattr(self.file, name.as_c_str())
                    .for_path("remove replacement-only extended attribute", self.path)?;
            }
            Ok(())
        }
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
use self::extended_attributes::{apply_extended_attributes, capture_extended_attributes};

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
    let metadata = fs::metadata(&canonical).for_path("inspect project root", canonical.clone())?;
    if !metadata.is_dir() {
        return Err(MutationError::InvalidRoot {
            path: canonical,
            message: "root is not a directory".into(),
        });
    }
    Ok(canonical)
}

fn ensure_directory(path: &Path) -> Result<(), MutationError> {
    if let Some(metadata) = optional_metadata(path, "inspect state directory")? {
        validate_state_directory(
            path,
            &metadata,
            "state directory is a symbolic link",
            "state path is not a directory",
        )?;
    } else {
        create_state_directory(path)?;
    }
    PermissionTarget::Path(path).restrict(0o700, "restrict mutation state directory permissions")
}

fn create_state_directory(path: &Path) -> Result<(), MutationError> {
    create_and_validate_directory(
        path,
        false,
        "create state directory",
        "inspect state directory",
        "new state directory was replaced with an unsafe path",
    )
}

fn create_and_validate_directory(
    path: &Path,
    recursive: bool,
    create_operation: &'static str,
    inspect_operation: &'static str,
    unsafe_message: &str,
) -> Result<(), MutationError> {
    let created = if recursive {
        fs::create_dir_all(path)
    } else {
        fs::create_dir(path)
    };
    created.for_path(create_operation, path)?;
    validate_prepared_directory(path, inspect_operation, unsafe_message)
}

fn validate_prepared_directory(
    path: &Path,
    operation: &'static str,
    unsafe_message: &str,
) -> Result<(), MutationError> {
    let metadata = fs::symlink_metadata(path).for_path(operation, path)?;
    validate_state_directory(path, &metadata, unsafe_message, unsafe_message)
}

fn validate_state_directory(
    path: &Path,
    metadata: &Metadata,
    symlink_message: &str,
    non_directory_message: &str,
) -> Result<(), MutationError> {
    let message = if metadata.file_type().is_symlink() {
        Some(symlink_message)
    } else if metadata.is_dir() {
        None
    } else {
        Some(non_directory_message)
    };
    reject_if(message.is_some(), || {
        MutationError::path_message(PathMessageKind::Unsafe, path, message.unwrap_or_default())
    })
}

fn optional_metadata(path: &Path, operation: &'static str) -> Result<Option<Metadata>, MutationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MutationError::io(operation, path, source)),
    }
}

fn required_parent<'a>(path: &'a Path, message: &'static str) -> Result<&'a Path, MutationError> {
    path.parent()
        .ok_or_else(|| MutationError::path_message(PathMessageKind::Unsafe, path, message))
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
    let platform_base = home_relative("Library/Application Support");

    #[cfg(all(unix, not(target_os = "macos")))]
    let platform_base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_relative(".local/state"));

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

#[cfg(unix)]
fn home_relative(relative: &str) -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(relative))
}

fn state_base_below_override(parent: &Path) -> PathBuf {
    parent.join(STATE_DIRECTORY)
}

fn validate_absolute_state_path(path: &Path) -> Result<(), MutationError> {
    let invalid = !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir));
    reject_if(invalid, || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            path,
            format!("mutation state must use an absolute normalized path (configure {STATE_DIRECTORY_ENV})"),
        )
    })
}

/// Resolve an absolute path through its nearest existing ancestor without
/// creating it. This lets us reject an override that would resolve inside the
/// analyzed project before writing any state there.
fn prospective_canonical_path(path: &Path) -> Result<PathBuf, MutationError> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    while optional_metadata(&existing, "inspect mutation state ancestor")?.is_none() {
        missing.push(pop_missing_component(path, &mut existing)?);
    }
    let mut resolved = existing
        .canonicalize()
        .for_path("canonicalize mutation state ancestor", &existing)?;
    append_missing_components(&mut resolved, &missing);
    Ok(resolved)
}

fn append_missing_components(resolved: &mut PathBuf, missing: &[std::ffi::OsString]) {
    for component in missing.iter().rev() {
        resolved.push(component);
    }
}

fn pop_missing_component(
    original: &Path,
    existing: &mut PathBuf,
) -> Result<std::ffi::OsString, MutationError> {
    let name = existing
        .file_name()
        .ok_or_else(|| no_existing_ancestor(original))?
        .to_os_string();
    if !existing.pop() {
        return Err(no_existing_ancestor(original));
    }
    Ok(name)
}

fn no_existing_ancestor(path: &Path) -> MutationError {
    MutationError::path_message(
        PathMessageKind::Unsafe,
        path,
        "mutation state has no existing ancestor",
    )
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
    validated_external_state(
        state,
        root,
        false,
        "mutation execution state must be outside the analyzed project",
    )
}

fn validated_external_state(
    state: PathBuf,
    root: &Path,
    changed_while_preparing: bool,
    message: &'static str,
) -> Result<PathBuf, MutationError> {
    reject_if(changed_while_preparing || state.starts_with(root), || {
        MutationError::path_message(PathMessageKind::Unsafe, &state, message)
    })?;
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
    let Some(metadata) = optional_metadata(state, "inspect mutation state")? else {
        return Ok(None);
    };
    validate_state_directory(
        state,
        &metadata,
        "mutation state is not a regular directory",
        "mutation state is not a regular directory",
    )?;
    pending_journal_path(state)
}

fn pending_journal_path(state: &Path) -> Result<Option<PathBuf>, MutationError> {
    let journal = JournalFile::in_state(state).path;
    ensure_regular_nonsymlink(&journal, "active journal")?;
    Ok(optional_metadata(&journal, "inspect active journal")?.map(|_| journal))
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
    canonical_root(root).and_then(|root| {
        configured_state_base().and_then(|base| {
            state_directory_path(&root, &base).and_then(|state| pending_mutation_in_state(&root, &state))
        })
    })
}

pub(crate) fn pending_mutation_locked(
    root: &Path,
    state: &Path,
) -> Result<Option<PendingMutation>, MutationError> {
    pending_mutation_in_state(root, state)
}

fn pending_mutation_in_state(root: &Path, state: &Path) -> Result<Option<PendingMutation>, MutationError> {
    let base = state_base(state)?;
    match ActiveRunFile::in_base(base).read()? {
        Some((pointer, record)) => pending_mutation_from_active(base, pointer, &record),
        None => Ok(pending_journal_in_state(state)?.map(|journal| PendingMutation {
            root: root.to_path_buf(),
            journal,
            active_pointer: None,
        })),
    }
}

fn pending_mutation_from_active(
    base: &Path,
    pointer: PathBuf,
    record: &ActiveRunRecord,
) -> Result<Option<PendingMutation>, MutationError> {
    let active_root = validated_active_root(&pointer, record)?;
    // A global pointer means some source may still contain a mutant. It blocks
    // every shared analysis session until recovery, including when a crashed
    // checkout has since moved to a non-overlapping path.
    Ok(Some(PendingMutation {
        journal: base.join(&record.root_key).join(ACTIVE_JOURNAL),
        root: active_root,
        active_pointer: Some(pointer),
    }))
}

fn prepare_state_directory_in(root: &Path, base: &Path) -> Result<PathBuf, MutationError> {
    let prospective = state_directory_path(root, base)?;
    prepare_state_base(base)?;
    let state = validated_prepared_state(root, base, &prospective)?;
    ensure_directory(&state)?;
    Ok(state)
}

fn validated_prepared_state(root: &Path, base: &Path, prospective: &Path) -> Result<PathBuf, MutationError> {
    let canonical_base = base
        .canonicalize()
        .for_path("canonicalize mutation state base", base)?;
    let state = canonical_base.join(project_state_key(root));
    let changed_while_preparing = state != prospective;
    validated_external_state(
        state,
        root,
        changed_while_preparing,
        "mutation state base changed while it was prepared or resolves inside the analyzed project",
    )
}

fn prepare_state_base(base: &Path) -> Result<(), MutationError> {
    create_and_validate_directory(
        base,
        true,
        "create mutation state base",
        "inspect mutation state base",
        "mutation state base is not a regular directory",
    )?;
    PermissionTarget::Path(base).restrict(0o700, "restrict mutation state directory permissions")
}

fn prepare_state_directory(root: &Path) -> Result<PathBuf, MutationError> {
    let base = configured_state_base()?;
    prepare_state_directory_in(root, &base)
}

fn ensure_regular_nonsymlink(path: &Path, purpose: &'static str) -> Result<(), MutationError> {
    let Some(metadata) = optional_metadata(path, "inspect state file")? else {
        return Ok(());
    };
    validate_regular_state_file(path, purpose, &metadata)
}

fn reject_symlink(path: &Path, metadata: &Metadata, message: impl Into<String>) -> Result<(), MutationError> {
    reject_if(metadata.file_type().is_symlink(), || {
        MutationError::path_message(PathMessageKind::Unsafe, path, message)
    })
}

fn validate_regular_state_file(path: &Path, purpose: &str, metadata: &Metadata) -> Result<(), MutationError> {
    reject_symlink(path, metadata, format!("{purpose} is a symbolic link"))?;
    reject_if(!metadata.is_file(), || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            path,
            format!("{purpose} is not a regular file"),
        )
    })
}

mod locking {
    #[cfg(test)]
    use super::prepare_state_directory_in;
    use super::{
        ensure_regular_nonsymlink, prepare_state_directory, required_parent, ErrorKind, File, HashMap,
        Instant, MutationError, MutationIoResult, Mutex, OnceLock, OpenOptions, Path, PathBuf,
        PermissionTarget, EXCLUSIVE_LOCK_WAIT, LOCK_RETRY_INTERVAL, RUN_LOCK,
    };
    use fs2::FileExt;

    #[derive(Debug, Default)]
    struct ProcessLockState {
        readers: usize,
        writer: bool,
    }

    impl ProcessLockState {
        fn try_acquire(&mut self, exclusive: bool) -> bool {
            if exclusive {
                return self.try_acquire_writer();
            }
            self.try_acquire_reader()
        }

        fn try_acquire_writer(&mut self) -> bool {
            if self.writer || self.readers != 0 {
                return false;
            }
            self.writer = true;
            true
        }

        fn try_acquire_reader(&mut self) -> bool {
            if self.writer {
                return false;
            }
            let Some(readers) = self.readers.checked_add(1) else {
                return false;
            };
            self.readers = readers;
            true
        }
    }

    fn process_lock_table() -> &'static Mutex<HashMap<PathBuf, ProcessLockState>> {
        static TABLE: OnceLock<Mutex<HashMap<PathBuf, ProcessLockState>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    #[derive(Debug)]
    struct ProcessLockGuard {
        path: PathBuf,
        exclusive: bool,
    }

    impl ProcessLockGuard {
        fn try_acquire(path: &Path, exclusive: bool) -> Option<Self> {
            let mut table = process_lock_table()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = table.entry(path.to_path_buf()).or_default();
            if !state.try_acquire(exclusive) {
                return None;
            }
            Some(Self {
                path: path.to_path_buf(),
                exclusive,
            })
        }
    }

    impl Drop for ProcessLockGuard {
        fn drop(&mut self) {
            let mut table = process_lock_table()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let remove = if let Some(state) = table.get_mut(&self.path) {
                if self.exclusive {
                    state.writer = false;
                } else {
                    state.readers = state.readers.saturating_sub(1);
                }
                !state.writer && state.readers == 0
            } else {
                false
            };
            if remove {
                table.remove(&self.path);
            }
        }
    }

    #[derive(Debug)]
    pub(crate) struct RunLockGuard {
        file: File,
        pub(crate) state: PathBuf,
        _process_lock: ProcessLockGuard,
    }

    #[derive(Clone, Copy)]
    pub(super) enum LockMode {
        Execution,
        Shared,
        #[cfg(test)]
        ImmediateExclusive,
    }

    impl LockMode {
        const fn flags(self) -> (bool, bool) {
            match self {
                Self::Execution => (true, true),
                Self::Shared => (false, false),
                #[cfg(test)]
                Self::ImmediateExclusive => (true, false),
            }
        }
    }

    pub(crate) fn acquire_run_lock(root: &Path) -> Result<RunLockGuard, MutationError> {
        acquire_run_lock_mode(root, LockMode::Execution)
    }

    pub(crate) fn acquire_shared_run_lock(root: &Path) -> Result<RunLockGuard, MutationError> {
        acquire_run_lock_mode(root, LockMode::Shared)
    }

    fn acquire_run_lock_mode(root: &Path, mode: LockMode) -> Result<RunLockGuard, MutationError> {
        let state = prepare_state_directory(root)?;
        let (exclusive, wait_for_readers) = mode.flags();
        acquire_run_lock_in_state(state, exclusive, wait_for_readers)
    }

    #[cfg(test)]
    pub(super) fn acquire_run_lock_with_base(
        root: &Path,
        base: &Path,
    ) -> Result<RunLockGuard, MutationError> {
        acquire_test_run_lock(root, base, LockMode::ImmediateExclusive)
    }

    #[cfg(test)]
    pub(super) fn acquire_test_run_lock(
        root: &Path,
        base: &Path,
        mode: LockMode,
    ) -> Result<RunLockGuard, MutationError> {
        let state = prepare_state_directory_in(root, base)?;
        let (exclusive, wait_for_readers) = mode.flags();
        acquire_run_lock_in_state(state, exclusive, wait_for_readers)
    }

    fn acquire_run_lock_in_state(
        state: PathBuf,
        exclusive: bool,
        wait_for_readers: bool,
    ) -> Result<RunLockGuard, MutationError> {
        let base = required_parent(&state, "mutation state has no global lock directory")?;
        // One lock for the entire state base deliberately serializes roots that
        // overlap (for example, a workspace and one nested package).
        let path = base.join(RUN_LOCK);
        let file = open_run_lock(&path)?;
        let deadline = wait_for_readers.then(|| Instant::now() + EXCLUSIVE_LOCK_WAIT);
        let process_lock = wait_for_run_lock(&path, &file, exclusive, deadline)?;
        Ok(RunLockGuard {
            file,
            state,
            _process_lock: process_lock,
        })
    }

    fn open_run_lock(path: &Path) -> Result<File, MutationError> {
        ensure_regular_nonsymlink(path, "run lock")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .for_path("open run lock", path)?;
        PermissionTarget::File { file: &file, path }.restrict(0o600, "restrict run lock permissions")?;
        Ok(file)
    }

    fn wait_for_run_lock(
        path: &Path,
        file: &File,
        exclusive: bool,
        deadline: Option<Instant>,
    ) -> Result<ProcessLockGuard, MutationError> {
        loop {
            if let Some(lock) = try_run_lock(path, file, exclusive)? {
                return Ok(lock);
            }
            if !lock_retry_allowed(deadline) {
                return Err(MutationError::AlreadyRunning {
                    path: path.to_path_buf(),
                });
            }
            std::thread::sleep(LOCK_RETRY_INTERVAL);
        }
    }

    fn try_run_lock(
        path: &Path,
        file: &File,
        exclusive: bool,
    ) -> Result<Option<ProcessLockGuard>, MutationError> {
        let Some(process_lock) = ProcessLockGuard::try_acquire(path, exclusive) else {
            return Ok(None);
        };
        classify_run_lock_result(path, process_lock, try_file_lock(file, exclusive))
    }

    fn try_file_lock(file: &File, exclusive: bool) -> std::io::Result<()> {
        if exclusive {
            FileExt::try_lock_exclusive(file)
        } else {
            FileExt::try_lock_shared(file)
        }
    }

    fn classify_run_lock_result(
        path: &Path,
        process_lock: ProcessLockGuard,
        result: std::io::Result<()>,
    ) -> Result<Option<ProcessLockGuard>, MutationError> {
        match result {
            Ok(()) => Ok(Some(process_lock)),
            Err(error) if lock_is_contended(&error) => Ok(None),
            Err(source) => Err(MutationError::io("acquire run lock", path, source)),
        }
    }

    fn lock_retry_allowed(deadline: Option<Instant>) -> bool {
        deadline.is_some_and(|value| Instant::now() < value)
    }

    fn lock_is_contended(error: &std::io::Error) -> bool {
        if error.kind() == ErrorKind::WouldBlock {
            return true;
        }
        #[cfg(windows)]
        {
            // LockFileEx reports lock or sharing violations as raw Win32 errors
            // instead of mapping them to ErrorKind::WouldBlock.
            matches!(error.raw_os_error(), Some(32) | Some(33))
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    impl Drop for RunLockGuard {
        fn drop(&mut self) {
            let _ = FileExt::unlock(&self.file);
        }
    }
}

pub(crate) use locking::{acquire_run_lock, acquire_shared_run_lock, RunLockGuard};
#[cfg(test)]
use locking::{acquire_run_lock_with_base, acquire_test_run_lock, LockMode};

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
    let message = if relative.as_os_str().is_empty() || relative.is_absolute() {
        Some("source path must be non-empty and relative")
    } else if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        Some("source path contains a root, parent, prefix, or current-directory component")
    } else if is_control_path(relative) {
        Some("version-control and reporigor control paths are not valid mutation targets")
    } else {
        None
    };
    reject_if(message.is_some(), || {
        MutationError::path_message(PathMessageKind::Unsafe, relative, message.unwrap_or_default())
    })?;
    Ok(relative.to_path_buf())
}

pub(crate) fn resolve_source_path(
    root: &Path,
    file: &str,
    allow_missing_file: bool,
) -> Result<PathBuf, MutationError> {
    let relative = relative_source_path(file)?;
    validate_mutation_root(root)?;
    let cursor = walk_source_path(root, &relative, allow_missing_file)?;
    validate_source_containment(root, cursor)
}

fn validate_mutation_root(root: &Path) -> Result<(), MutationError> {
    reject_if(is_control_path(root), || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            root,
            "a version-control or reporigor control directory cannot be a mutation root",
        )
    })
}

fn walk_source_path(
    root: &Path,
    relative: &Path,
    allow_missing_file: bool,
) -> Result<PathBuf, MutationError> {
    let component_count = relative.components().count();
    let mut cursor = root.to_path_buf();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(name) = component else {
            unreachable!("relative_source_path accepts only normal components");
        };
        cursor.push(name);
        let final_component = index + 1 == component_count;
        inspect_source_component(&cursor, final_component, allow_missing_file)?;
    }
    Ok(cursor)
}

fn inspect_source_component(
    path: &Path,
    final_component: bool,
    allow_missing_file: bool,
) -> Result<(), MutationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_source_component(path, &metadata, final_component),
        Err(error) => classify_source_component_error(path, error, final_component, allow_missing_file),
    }
}

fn classify_source_component_error(
    path: &Path,
    source: std::io::Error,
    final_component: bool,
    allow_missing_file: bool,
) -> Result<(), MutationError> {
    if source.kind() == ErrorKind::NotFound && final_component && allow_missing_file {
        return Ok(());
    }
    Err(MutationError::io("inspect mutation source", path, source))
}

fn validate_source_component(
    path: &Path,
    metadata: &Metadata,
    final_component: bool,
) -> Result<(), MutationError> {
    reject_symlink(path, metadata, "symbolic links are not valid mutation targets")?;
    if final_component {
        return validate_regular_source(path, metadata);
    }
    reject_if(!metadata.is_dir(), || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            path,
            "a mutation path parent is not a directory",
        )
    })
}

fn validate_regular_source(path: &Path, metadata: &Metadata) -> Result<(), MutationError> {
    reject_if(!metadata.is_file(), || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            path,
            "mutation target is not a regular file",
        )
    })
}

fn validate_source_containment(root: &Path, cursor: PathBuf) -> Result<PathBuf, MutationError> {
    let parent = cursor.parent().ok_or_else(|| {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            cursor.clone(),
            "mutation target has no parent directory",
        )
    })?;
    validate_canonical_containment(
        root,
        parent,
        &cursor,
        "canonicalize mutation source parent",
        "mutation source escapes the project root",
    )?;
    validate_existing_source_containment(root, &cursor)?;
    Ok(cursor)
}

fn validate_existing_source_containment(root: &Path, cursor: &Path) -> Result<(), MutationError> {
    if !cursor.exists() {
        return Ok(());
    }
    validate_canonical_containment(
        root,
        cursor,
        cursor,
        "canonicalize mutation source",
        "canonical mutation source escapes the project root",
    )
}

fn validate_canonical_containment(
    root: &Path,
    resolved_path: &Path,
    error_path: &Path,
    operation: &'static str,
    message: &'static str,
) -> Result<(), MutationError> {
    let canonical = resolved_path.canonicalize().for_path(operation, resolved_path)?;
    reject_if(!canonical.starts_with(root), || {
        MutationError::path_message(PathMessageKind::Unsafe, error_path, message)
    })
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_bounded_source(
    path: &Path,
    max_source_bytes: usize,
) -> Result<(Metadata, Vec<u8>, StoredPermissions), MutationError> {
    let max_source_bytes_u64 = u64::try_from(max_source_bytes).unwrap_or(u64::MAX);
    let (file, metadata, permissions) = open_supported_source(path)?;
    validate_source_size(path, metadata.len(), max_source_bytes_u64)?;
    let bytes = read_source_bytes(path, file, metadata.len(), max_source_bytes, max_source_bytes_u64)?;
    Ok((metadata, bytes, permissions))
}

fn open_supported_source(path: &Path) -> Result<(File, Metadata, StoredPermissions), MutationError> {
    let (file, metadata) = open_source(path)?;
    ensure_supported_source_metadata(path, &file, &metadata)?;
    let permissions = StoredPermissions::capture(path, &file, &metadata)?;
    Ok((file, metadata, permissions))
}

fn open_source(path: &Path) -> Result<(File, Metadata), MutationError> {
    let file = File::open(path).for_path("open mutation source", path)?;
    let metadata = file.metadata().for_path("inspect opened mutation source", path)?;
    Ok((file, metadata))
}

fn validate_source_size(path: &Path, actual_bytes: u64, max_source_bytes: u64) -> Result<(), MutationError> {
    reject_if(actual_bytes > max_source_bytes, || {
        MutationError::SourceTooLarge {
            path: path.to_path_buf(),
            actual_bytes,
            max_source_bytes,
        }
    })
}

fn read_source_bytes(
    path: &Path,
    file: File,
    actual_bytes: u64,
    max_source_bytes: usize,
    max_source_bytes_u64: u64,
) -> Result<Vec<u8>, MutationError> {
    let capacity = usize::try_from(actual_bytes).unwrap_or(max_source_bytes);
    let read_limit = max_source_bytes_u64.saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity.min(max_source_bytes));
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .for_path("read mutation source", path)?;
    reject_if(bytes.len() > max_source_bytes, || MutationError::SourceTooLarge {
        path: path.to_path_buf(),
        actual_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        max_source_bytes: max_source_bytes_u64,
    })?;
    Ok(bytes)
}

fn ensure_supported_source_metadata(
    path: &Path,
    file: &File,
    metadata: &Metadata,
) -> Result<(), MutationError> {
    #[cfg(unix)]
    {
        reject_if(metadata.nlink() != 1, || {
            MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                format!(
                    "file has {} hard links; atomic replacement would silently split the linked inode",
                    metadata.nlink()
                ),
            )
        })?;
    }
    #[cfg(windows)]
    {
        let links = winapi_util::file::information(file)
            .for_path("inspect mutation source links", path)?
            .number_of_links();
        reject_if(links != 1, || {
            MutationError::path_message(
                PathMessageKind::UnsupportedMetadata,
                path,
                format!(
                    "file has {links} hard links; atomic replacement would silently split the linked inode"
                ),
            )
        })?;
    }
    #[cfg(not(windows))]
    let _ = file;
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

pub(crate) fn sync_parent(path: &Path) -> Result<(), MutationError> {
    #[cfg(unix)]
    {
        let parent = required_parent(path, "atomic replacement target has no parent")?;
        let directory = File::open(parent).for_path("open directory for synchronization", parent)?;
        directory.sync_all().for_path("synchronize directory", parent)?;
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
    atomic_replace_with_timestamp_policy(path, bytes, permissions, true)
}

fn atomic_replace_with_timestamp_policy(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&StoredPermissions>,
    restore_timestamps: bool,
) -> Result<(), MutationError> {
    let parent = required_parent(path, "atomic replacement target has no parent")?;
    let mut temporary = create_atomic_replacement(parent, bytes)?;
    apply_replacement_permissions(&temporary, permissions, restore_timestamps)?;
    synchronize_replacement(&mut temporary)?;
    persist_and_sync_replacement(temporary, path)
}

fn atomic_replace_for_mutation(
    path: &Path,
    bytes: &[u8],
    permissions: &StoredPermissions,
) -> Result<(), MutationError> {
    atomic_replace_with_timestamp_policy(path, bytes, Some(permissions), false)
}

fn persist_and_sync_replacement(temporary: NamedTempFile, path: &Path) -> Result<(), MutationError> {
    persist_replacement(temporary, path)?;
    sync_parent(path)
}

fn create_atomic_replacement(parent: &Path, bytes: &[u8]) -> Result<NamedTempFile, MutationError> {
    let mut temporary = NamedTempFile::new_in(parent).for_path("create atomic replacement", parent)?;
    temporary
        .write_all(bytes)
        .for_path("write atomic replacement", temporary.path())?;
    Ok(temporary)
}

fn apply_replacement_permissions(
    temporary: &NamedTempFile,
    permissions: Option<&StoredPermissions>,
    restore_timestamps: bool,
) -> Result<(), MutationError> {
    let Some(value) = permissions else {
        return PermissionTarget::File {
            file: temporary.as_file(),
            path: temporary.path(),
        }
        .restrict(0o600, "restrict atomic replacement permissions");
    };
    let fallback = temporary
        .as_file()
        .metadata()
        .for_path("inspect replacement permissions", temporary.path())?
        .permissions();
    value.apply(
        temporary.as_file(),
        temporary.path(),
        fallback,
        restore_timestamps,
    )
}

fn synchronize_replacement(temporary: &mut NamedTempFile) -> Result<(), MutationError> {
    let path = temporary.path().to_path_buf();
    temporary
        .as_file_mut()
        .sync_all()
        .for_path("synchronize atomic replacement", &path)?;
    temporary
        .as_file_mut()
        .sync_all()
        .for_path("synchronize replacement metadata", path)
}

fn persist_replacement(temporary: NamedTempFile, path: &Path) -> Result<(), MutationError> {
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .for_path("persist atomic replacement", path)
        .map(|_| ())
}

fn write_serialized_state<T: Serialize>(
    path: &Path,
    purpose: &'static str,
    record: &T,
    max_bytes: u64,
    size_label: &'static str,
) -> Result<(), MutationError> {
    let encoded =
        serde_json::to_vec_pretty(record).map_err(|error| MutationError::State(error.to_string()))?;
    StateFile::write_new(path, purpose, &encoded, max_bytes, size_label)
}

fn validate_state_schema(path: &Path, actual: u8, subject: &str) -> Result<(), MutationError> {
    reject_if(actual != JOURNAL_SCHEMA_VERSION, || {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            path,
            format!("unsupported {subject}schema version {actual}"),
        )
    })
}

struct JournalFile {
    path: PathBuf,
}

impl JournalFile {
    fn in_state(state: &Path) -> Self {
        Self {
            path: state.join(ACTIVE_JOURNAL),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(self, record: &JournalRecord, max_journal_bytes: u64) -> Result<PathBuf, MutationError> {
        write_serialized_state(
            self.path(),
            "active journal",
            record,
            max_journal_bytes,
            "encoded journal",
        )?;
        Ok(self.path)
    }
}

struct StateFile;

impl StateFile {
    fn write_new(
        path: &Path,
        purpose: &'static str,
        encoded: &[u8],
        max_bytes: u64,
        size_label: &'static str,
    ) -> Result<(), MutationError> {
        ensure_regular_nonsymlink(path, purpose)?;
        reject_if(path.exists(), || {
            MutationError::State(format!(
                "refusing to replace existing {purpose} {}",
                path.display()
            ))
        })?;
        Self::validate_encoded_state_size(path, encoded.len(), max_bytes, size_label)?;
        atomic_replace(path, encoded, None)
    }

    fn validate_encoded_state_size(
        path: &Path,
        encoded_len: usize,
        max_bytes: u64,
        label: &str,
    ) -> Result<(), MutationError> {
        let encoded_len = u64::try_from(encoded_len)
            .map_err(|_| MutationError::State(format!("{label} length does not fit u64")))?;
        reject_if(encoded_len > max_bytes, || {
            MutationError::path_message(
                PathMessageKind::InvalidJournal,
                path,
                format!("{label} is {encoded_len} bytes, exceeding the safe {max_bytes}-byte limit"),
            )
        })
    }
}

impl StateFile {
    fn remove(path: &Path, operation: &'static str) -> Result<(), MutationError> {
        match fs::remove_file(path) {
            Ok(()) => sync_parent(path),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(MutationError::io(operation, path, source)),
        }
    }
}

impl JournalFile {
    fn read(&self) -> Result<Option<(PathBuf, JournalRecord)>, MutationError> {
        StateFile::read_record(
            self.path(),
            "active journal",
            MAX_JOURNAL_BYTES,
            Self::parse_journal_record,
        )
    }
}

impl StateFile {
    fn read_record<T>(
        path: &Path,
        purpose: &'static str,
        max_bytes: u64,
        parse: fn(&Path, &[u8]) -> Result<T, MutationError>,
    ) -> Result<Option<(PathBuf, T)>, MutationError> {
        let Some(bytes) = Self::read_bounded(path, purpose, max_bytes)? else {
            return Ok(None);
        };
        let record = parse(path, &bytes)?;
        Ok(Some((path.to_path_buf(), record)))
    }

    fn read_bounded(
        path: &Path,
        purpose: &'static str,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, MutationError> {
        let Some((file, size)) = Self::open_validated(path, purpose, max_bytes)? else {
            return Ok(None);
        };
        Ok(Some(Self::read_open(path, file, size, max_bytes, purpose)?))
    }

    fn open_validated(
        path: &Path,
        purpose: &'static str,
        max_bytes: u64,
    ) -> Result<Option<(File, u64)>, MutationError> {
        ensure_regular_nonsymlink(path, purpose)?;
        let Some((file, size)) = Self::open_optional(path, purpose)? else {
            return Ok(None);
        };
        Self::validate_state_file_size(path, size, max_bytes, purpose)?;
        Ok(Some((file, size)))
    }

    fn open_optional(path: &Path, purpose: &'static str) -> Result<Option<(File, u64)>, MutationError> {
        let metadata = match fs::metadata(path) {
            Ok(value) => value,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(MutationError::io("inspect state file", path, source)),
        };
        let file = File::open(path).for_path("open state file", path)?;
        let _ = purpose;
        Ok(Some((file, metadata.len())))
    }

    fn validate_state_file_size(
        path: &Path,
        size: u64,
        max_bytes: u64,
        purpose: &str,
    ) -> Result<(), MutationError> {
        reject_if(size > max_bytes, || {
            MutationError::path_message(
                PathMessageKind::InvalidJournal,
                path,
                format!("{purpose} exceeds {max_bytes} bytes"),
            )
        })
    }

    fn read_open(
        path: &Path,
        file: File,
        size: u64,
        max_bytes: u64,
        purpose: &str,
    ) -> Result<Vec<u8>, MutationError> {
        let mut bytes = Vec::with_capacity(usize::try_from(size.min(max_bytes)).unwrap_or(usize::MAX));
        file.take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .for_path("read state file", path)?;
        Self::validate_state_file_size(
            path,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max_bytes,
            purpose,
        )?;
        Ok(bytes)
    }
}

impl JournalFile {
    fn parse_journal_record(path: &Path, bytes: &[u8]) -> Result<JournalRecord, MutationError> {
        let record: JournalRecord = StateFile::decode_record(path, bytes)?;
        validate_state_schema(path, record.schema_version, "")?;
        Ok(record)
    }
}

impl StateFile {
    fn decode_record<T: DeserializeOwned>(path: &Path, bytes: &[u8]) -> Result<T, MutationError> {
        serde_json::from_slice(bytes).map_err(|error| {
            MutationError::path_message(PathMessageKind::InvalidJournal, path, error.to_string())
        })
    }
}

fn state_base(state: &Path) -> Result<&Path, MutationError> {
    required_parent(state, "per-project mutation state has no global state base")
}

struct ActiveRunFile {
    path: PathBuf,
}

impl ActiveRunFile {
    fn in_base(base: &Path) -> Self {
        Self {
            path: base.join(ACTIVE_RUN),
        }
    }

    fn in_state(state: &Path) -> Result<Self, MutationError> {
        Ok(Self::in_base(state_base(state)?))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(self, root: &Path, journal: &JournalRecord) -> Result<PathBuf, MutationError> {
        let (root_encoding, root_base64) = encode_active_root(root);
        let record = ActiveRunRecord {
            schema_version: JOURNAL_SCHEMA_VERSION,
            root: None,
            root_encoding: Some(root_encoding),
            root_base64: Some(root_base64),
            root_key: project_state_key(root),
            root_identity: RootIdentity::capture(root)?,
            file: journal.file.clone(),
            original_sha256: journal.original_sha256.clone(),
            mutated_sha256: journal.mutated_sha256.clone(),
        };
        write_serialized_state(
            self.path(),
            "global active mutation pointer",
            &record,
            MAX_ACTIVE_RUN_BYTES,
            "active mutation pointer",
        )?;
        Ok(self.path)
    }

    fn read(&self) -> Result<Option<(PathBuf, ActiveRunRecord)>, MutationError> {
        StateFile::read_record(
            self.path(),
            "global active mutation pointer",
            MAX_ACTIVE_RUN_BYTES,
            ActiveRunRecordValidator::parse,
        )
    }
}

impl RootIdentity {
    fn capture(root: &Path) -> Result<Option<Self>, MutationError> {
        #[cfg(unix)]
        {
            let metadata = fs::metadata(root).for_path("inspect project root identity", root)?;
            Ok(Some(Self {
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
}

struct ActiveRunRecordValidator {
    path: PathBuf,
    record: ActiveRunRecord,
}

impl ActiveRunRecordValidator {
    fn parse(path: &Path, bytes: &[u8]) -> Result<ActiveRunRecord, MutationError> {
        let record: ActiveRunRecord = StateFile::decode_record(path, bytes)?;
        Self {
            path: path.to_path_buf(),
            record,
        }
        .validated()
    }

    fn validated(self) -> Result<ActiveRunRecord, MutationError> {
        self.validate_schema()?;
        self.validate_fields()?;
        Ok(self.record)
    }

    fn validate_schema(&self) -> Result<(), MutationError> {
        validate_state_schema(&self.path, self.record.schema_version, "active pointer ")
    }

    fn validate_fields(&self) -> Result<(), MutationError> {
        let root = decode_active_root(&self.record).map_err(|message| {
            MutationError::path_message(PathMessageKind::InvalidJournal, &self.path, message)
        })?;
        let contains_relative_component = root
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir));
        let invalid_root = !root.is_absolute() || contains_relative_component;
        let invalid = invalid_root || !self.valid_payload(&root);
        reject_if(invalid, || {
            MutationError::path_message(
                PathMessageKind::InvalidJournal,
                &self.path,
                "active mutation pointer contains invalid root, source, or checksum fields",
            )
        })
    }

    fn valid_payload(&self, root: &Path) -> bool {
        self.record.root_key == project_state_key(root)
            && relative_source_path(&self.record.file).is_ok()
            && self.checksums_valid()
    }

    fn checksums_valid(&self) -> bool {
        let is_sha256 = |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        is_sha256(&self.record.original_sha256) && is_sha256(&self.record.mutated_sha256)
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
        return decode_encoded_active_root(encoding, encoded);
    }
    match (&record.root, &record.root_encoding, &record.root_base64) {
        (Some(root), None, None) => Ok(PathBuf::from(root)),
        _ => Err("active mutation pointer is missing a complete root encoding".into()),
    }
}

fn decode_encoded_active_root(encoding: &str, encoded: &str) -> Result<PathBuf, String> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|error| format!("invalid encoded mutation root: {error}"))?;
    decode_active_root_bytes(encoding, bytes)
}

fn decode_active_root_bytes(encoding: &str, bytes: Vec<u8>) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        require_root_encoding(encoding, "unix-bytes")?;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }

    #[cfg(windows)]
    {
        if bytes.len() % 2 != 0 {
            return Err(format!("invalid mutation root encoding {encoding}"));
        }
        require_root_encoding(encoding, "windows-utf16le")
            .map_err(|_| format!("invalid mutation root encoding {encoding}"))?;
        let words = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return Ok(PathBuf::from(OsString::from_wide(&words)));
    }

    #[cfg(not(any(unix, windows)))]
    {
        require_root_encoding(encoding, "utf8")?;
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|error| format!("invalid UTF-8 mutation root: {error}"))
    }
}

fn require_root_encoding(actual: &str, expected: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("unsupported mutation root encoding {actual}"))
    }
}

fn validated_active_root(pointer: &Path, record: &ActiveRunRecord) -> Result<PathBuf, MutationError> {
    let recorded_root = decode_active_root(record)
        .map_err(|message| MutationError::path_message(PathMessageKind::InvalidJournal, pointer, message))?;
    let canonical = canonical_root(&recorded_root).map_err(|error| {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            pointer,
            format!("recorded mutation root is unavailable or unsafe: {error}"),
        )
    })?;
    validate_canonical_active_root(pointer, record, &recorded_root, &canonical)?;
    validate_active_root_identity(pointer, record, &canonical)?;
    Ok(canonical)
}

fn validate_canonical_active_root(
    pointer: &Path,
    record: &ActiveRunRecord,
    recorded_root: &Path,
    canonical: &Path,
) -> Result<(), MutationError> {
    let identity_changed = canonical != recorded_root || record.root_key != project_state_key(canonical);
    reject_if(identity_changed, || {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            pointer,
            "recorded mutation root no longer resolves to its canonical identity",
        )
    })
}

fn validate_active_root_identity(
    pointer: &Path,
    record: &ActiveRunRecord,
    canonical: &Path,
) -> Result<(), MutationError> {
    if let Some(expected) = &record.root_identity {
        let changed = RootIdentity::capture(canonical)?.as_ref() != Some(expected);
        reject_if(changed, || {
            MutationError::path_message(
                PathMessageKind::InvalidJournal,
                pointer,
                "recorded mutation root path now resolves to a different filesystem identity",
            )
        })?;
    }
    Ok(())
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn existing_recovery_target(root: &Path, file: &str, journal: &Path) -> Result<PathBuf, MutationError> {
    let relative = relative_source_path(file)?;
    let unresolved = root.join(&relative);
    match optional_metadata(&unresolved, "inspect mutation source before recovery")? {
        Some(_) => resolve_source_path(root, file, false),
        None => Err(MutationError::recovery_conflict(unresolved, journal)),
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
            Err(MutationError::recovery_conflict(path, journal))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn recover_active_locked(
    requested_root: &Path,
    requested_state: &Path,
) -> Result<RecoveryAction, MutationError> {
    let base = state_base(requested_state)?;
    let Some((pointer, active)) = ActiveRunFile::in_base(base).read()? else {
        return recover_project_journal(requested_root, requested_state, None);
    };
    recover_pointed_and_requested(base, requested_root, requested_state, &pointer, &active)
}

fn recover_pointed_and_requested(
    base: &Path,
    requested_root: &Path,
    requested_state: &Path,
    pointer: &Path,
    active: &ActiveRunRecord,
) -> Result<RecoveryAction, MutationError> {
    let recorded_root = active_root_for_recovery(base, pointer, active, requested_root)?;
    validate_overlapping_recovery_root(requested_root, &recorded_root)?;
    let (active_state, active_action) = recover_pointed_state(base, &recorded_root, pointer, active)?;
    finish_requested_recovery(requested_root, requested_state, &active_state, active_action)
}

fn finish_requested_recovery(
    requested_root: &Path,
    requested_state: &Path,
    active_state: &Path,
    active_action: RecoveryAction,
) -> Result<RecoveryAction, MutationError> {
    if active_state == requested_state {
        return Ok(active_action);
    }
    let requested_action = recover_project_journal(requested_root, requested_state, None)?;
    Ok(combine_recovery_actions(active_action, requested_action))
}

fn recover_pointed_state(
    base: &Path,
    recorded_root: &Path,
    pointer: &Path,
    active: &ActiveRunRecord,
) -> Result<(PathBuf, RecoveryAction), MutationError> {
    let active_state = base.join(&active.root_key);
    validate_pointed_state(base, &active_state)?;
    let action = recover_project_journal(recorded_root, &active_state, Some((pointer, active)))?;
    Ok((active_state, action))
}

fn validate_overlapping_recovery_root(requested: &Path, recorded: &Path) -> Result<(), MutationError> {
    if !roots_overlap(requested, recorded) {
        return Err(MutationError::PendingMutationRoot {
            active_root: recorded.to_path_buf(),
            requested_root: requested.to_path_buf(),
        });
    }
    Ok(())
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
            recover_moved_active_root(base, pointer, active, requested_root, validation_error)
        }
    }
}

fn recover_moved_active_root(
    base: &Path,
    pointer: &Path,
    active: &ActiveRunRecord,
    requested_root: &Path,
    validation_error: MutationError,
) -> Result<PathBuf, MutationError> {
    let recorded = decode_active_root(active)
        .map_err(|message| MutationError::path_message(PathMessageKind::InvalidJournal, pointer, message))?;
    if recorded.exists() {
        return Err(validation_error);
    }
    MovedRootContext {
        pointer,
        active,
        recorded: &recorded,
        requested: requested_root,
    }
    .validate(base)?;
    Ok(requested_root.to_path_buf())
}

#[derive(Clone, Copy)]
enum MovedRootMismatch {
    Identity,
    Content,
}

struct MovedRootContext<'a> {
    pointer: &'a Path,
    active: &'a ActiveRunRecord,
    recorded: &'a Path,
    requested: &'a Path,
}

impl MovedRootContext<'_> {
    fn validate(&self, base: &Path) -> Result<(), MutationError> {
        self.validate_identity()?;
        self.validate_content(base)
    }

    fn validate_identity(&self) -> Result<(), MutationError> {
        let Some(expected) = &self.active.root_identity else {
            return Ok(());
        };
        let actual = RootIdentity::capture(self.requested)?;
        reject_if(actual.as_ref() != Some(expected), || {
            self.error(MovedRootMismatch::Identity)
        })
    }

    fn validate_content(&self, base: &Path) -> Result<(), MutationError> {
        let journal = base.join(&self.active.root_key).join(ACTIVE_JOURNAL);
        let target = existing_recovery_target(self.requested, &self.active.file, &journal)?;
        let current = read_current_for_recovery(&target, &journal, MAX_MUTATION_SOURCE_BYTES)?;
        let current_hash = sha256(&current);
        self.validate_hash(&current_hash)
    }

    fn validate_hash(&self, current_hash: &str) -> Result<(), MutationError> {
        let matches_original = current_hash == self.active.original_sha256;
        let matches_mutant = current_hash == self.active.mutated_sha256;
        reject_if(!matches_original && !matches_mutant, || {
            self.error(MovedRootMismatch::Content)
        })
    }

    fn error(&self, mismatch: MovedRootMismatch) -> MutationError {
        let message = match mismatch {
            MovedRootMismatch::Identity => format!(
                "recorded mutation root {} moved or disappeared, but {} has a different filesystem identity",
                self.recorded.display(),
                self.requested.display()
            ),
            MovedRootMismatch::Content => format!(
                "recorded mutation root {} is unavailable and {} does not contain the recorded source content",
                self.recorded.display(),
                self.requested.display()
            ),
        };
        MutationError::path_message(PathMessageKind::InvalidJournal, self.pointer, message)
    }
}

fn validate_pointed_state(base: &Path, state: &Path) -> Result<(), MutationError> {
    validate_pointed_state_parent(base, state)?;
    let Some(metadata) = optional_metadata(state, "inspect pointed mutation state")? else {
        // A pointer without a project directory is handled conservatively by
        // pointer-only recovery; it may only be cleared when source is proven
        // to match the recorded original checksum.
        return Ok(());
    };
    validate_state_directory(
        state,
        &metadata,
        "pointed mutation state is not a regular directory",
        "pointed mutation state is not a regular directory",
    )?;
    validate_pointed_state_canonical_identity(state)
}

fn validate_pointed_state_parent(base: &Path, state: &Path) -> Result<(), MutationError> {
    reject_if(state.parent() != Some(base), || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            state,
            "pointed mutation state is not an immediate child of the global state base",
        )
    })
}

fn validate_pointed_state_canonical_identity(state: &Path) -> Result<(), MutationError> {
    let canonical = state
        .canonicalize()
        .for_path("canonicalize pointed mutation state", state)?;
    reject_if(canonical != state, || {
        MutationError::path_message(
            PathMessageKind::Unsafe,
            state,
            "pointed mutation state changed canonical identity",
        )
    })
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
    match JournalFile::in_state(state).read()? {
        Some((journal, record)) => recover_existing_project_journal(root, state, active, &journal, &record),
        None => match active {
            Some((pointer, active)) => recover_pointer_without_journal(root, state, pointer, active),
            None => Ok(RecoveryAction::None),
        },
    }
}

fn recover_existing_project_journal(
    root: &Path,
    state: &Path,
    active: Option<(&Path, &ActiveRunRecord)>,
    journal: &Path,
    record: &JournalRecord,
) -> Result<RecoveryAction, MutationError> {
    validate_pointer_matches_journal(state, journal, record, active)?;
    let original = decode_original_journal_content(journal, record)?;
    let target = existing_recovery_target(root, &record.file, journal)?;
    let current = read_current_for_recovery(&target, journal, record.max_source_bytes)?;
    restore_recovery_target(&target, journal, record, &original, &current, active)
}

fn validate_pointer_matches_journal(
    state: &Path,
    journal: &Path,
    record: &JournalRecord,
    active: Option<(&Path, &ActiveRunRecord)>,
) -> Result<(), MutationError> {
    let Some((pointer, active)) = active else {
        return Ok(());
    };
    let mismatch = !pointer_matches_journal_payload(active, record) || journal != state.join(ACTIVE_JOURNAL);
    reject_if(mismatch, || {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            pointer,
            "global active pointer does not match its project recovery journal",
        )
    })
}

fn pointer_matches_journal_payload(active: &ActiveRunRecord, record: &JournalRecord) -> bool {
    active.file == record.file
        && active.original_sha256 == record.original_sha256
        && active.mutated_sha256 == record.mutated_sha256
}

fn decode_original_journal_content(journal: &Path, record: &JournalRecord) -> Result<Vec<u8>, MutationError> {
    let original = BASE64.decode(&record.original_base64).map_err(|error| {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            journal,
            format!("invalid original content: {error}"),
        )
    })?;
    validate_original_journal_content(journal, record, &original)?;
    Ok(original)
}

fn validate_original_journal_content(
    journal: &Path,
    record: &JournalRecord,
    original: &[u8],
) -> Result<(), MutationError> {
    validate_original_journal_limit(journal, record.max_source_bytes, original.len())?;
    reject_if(sha256(original) != record.original_sha256, || {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            journal,
            "original content checksum does not match",
        )
    })
}

fn validate_original_journal_limit(
    journal: &Path,
    max_source_bytes: usize,
    original_len: usize,
) -> Result<(), MutationError> {
    let invalid_limit = max_source_bytes == 0 || max_source_bytes > MAX_MUTATION_SOURCE_BYTES;
    reject_if(invalid_limit || original_len > max_source_bytes, || {
        MutationError::path_message(
            PathMessageKind::InvalidJournal,
            journal,
            format!(
                "original content or recorded limit exceeds the {MAX_MUTATION_SOURCE_BYTES}-byte recovery ceiling"
            ),
        )
    })
}

fn restore_recovery_target(
    target: &Path,
    journal: &Path,
    record: &JournalRecord,
    original: &[u8],
    current: &[u8],
    active: Option<(&Path, &ActiveRunRecord)>,
) -> Result<RecoveryAction, MutationError> {
    match classify_recovery_content(current, record) {
        RecoveryContent::Original => clear_original_recovery(journal, active),
        RecoveryContent::Mutated => restore_mutated_recovery(target, journal, record, original, active),
        RecoveryContent::Conflict => Err(MutationError::recovery_conflict(target, journal)),
    }
}

fn clear_original_recovery(
    journal: &Path,
    active: Option<(&Path, &ActiveRunRecord)>,
) -> Result<RecoveryAction, MutationError> {
    clear_recovery_state(journal, active)?;
    Ok(RecoveryAction::AlreadyClean)
}

fn restore_mutated_recovery(
    target: &Path,
    journal: &Path,
    record: &JournalRecord,
    original: &[u8],
    active: Option<(&Path, &ActiveRunRecord)>,
) -> Result<RecoveryAction, MutationError> {
    atomic_replace(target, original, Some(&record.original_permissions))?;
    clear_recovery_state(journal, active)?;
    Ok(RecoveryAction::Restored)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryContent {
    Original,
    Mutated,
    Conflict,
}

fn classify_recovery_content(current: &[u8], record: &JournalRecord) -> RecoveryContent {
    let current_sha256 = sha256(current);
    if current_sha256 == record.original_sha256 {
        return RecoveryContent::Original;
    }
    if current_sha256 == record.mutated_sha256 {
        return RecoveryContent::Mutated;
    }
    RecoveryContent::Conflict
}

fn clear_recovery_state(
    journal: &Path,
    active: Option<(&Path, &ActiveRunRecord)>,
) -> Result<(), MutationError> {
    if let Some((pointer, _)) = active {
        StateFile::remove(pointer, "remove active mutation pointer")?;
    }
    StateFile::remove(journal, "remove active journal")
}

fn recover_pointer_without_journal(
    root: &Path,
    state: &Path,
    pointer: &Path,
    active: &ActiveRunRecord,
) -> Result<RecoveryAction, MutationError> {
    let journal = JournalFile::in_state(state).path;
    let target = existing_recovery_target(root, &active.file, &journal)?;
    let current = read_current_for_recovery(&target, &journal, MAX_MUTATION_SOURCE_BYTES)?;
    let current_sha256 = sha256(&current);
    if current_sha256 == active.original_sha256 {
        StateFile::remove(pointer, "remove active mutation pointer")?;
        return Ok(RecoveryAction::AlreadyClean);
    }
    let message = missing_journal_message(&current_sha256, &active.mutated_sha256);
    Err(MutationError::MissingRecoveryJournal {
        path: target,
        pointer: pointer.to_path_buf(),
        journal,
        message: message.into(),
    })
}

fn missing_journal_message(current_sha256: &str, mutated_sha256: &str) -> &'static str {
    if current_sha256 == mutated_sha256 {
        "source still matches the active mutant and the original bytes are unavailable"
    } else {
        "source differs from both recorded checksums and the original bytes are unavailable"
    }
}

fn validate_mutation_source_limit(max_source_bytes: usize) -> Result<(), ApplyMutationError> {
    if max_source_bytes == 0 {
        return Err(ApplyMutationError::Invalid(
            "executable mutation source limit must be greater than zero".into(),
        ));
    }
    Ok(())
}

fn validate_mutation_candidate(
    mutation: &MutationCandidate,
    original: &[u8],
) -> Result<(), ApplyMutationError> {
    validate_mutation_range(mutation, original.len())?;
    validate_mutation_original(mutation, original)?;
    validate_mutation_replacement(mutation)
}

fn validate_mutation_range(
    mutation: &MutationCandidate,
    source_len: usize,
) -> Result<(), ApplyMutationError> {
    if mutation.start_byte > mutation.end_byte || mutation.end_byte > source_len {
        return Err(ApplyMutationError::Invalid(format!(
            "byte range {}..{} is outside a {source_len}-byte source file",
            mutation.start_byte, mutation.end_byte
        )));
    }
    Ok(())
}

fn validate_mutation_original(
    mutation: &MutationCandidate,
    original: &[u8],
) -> Result<(), ApplyMutationError> {
    if original[mutation.start_byte..mutation.end_byte] != *mutation.original.as_bytes() {
        return Err(ApplyMutationError::Invalid(
            "candidate original text no longer matches the source bytes".into(),
        ));
    }
    Ok(())
}

fn validate_mutation_replacement(mutation: &MutationCandidate) -> Result<(), ApplyMutationError> {
    if mutation.original == mutation.replacement {
        return Err(ApplyMutationError::Invalid(
            "mutation replacement is identical to the original text".into(),
        ));
    }
    Ok(())
}

fn build_mutated_source(
    mutation: &MutationCandidate,
    original: &[u8],
    max_source_bytes: usize,
) -> Result<Vec<u8>, ApplyMutationError> {
    let mutated_len = mutated_source_len(mutation, original.len())?;
    validate_mutated_source_size(mutated_len, max_source_bytes)?;
    let mut mutated = Vec::with_capacity(mutated_len);
    mutated.extend_from_slice(&original[..mutation.start_byte]);
    mutated.extend_from_slice(mutation.replacement.as_bytes());
    mutated.extend_from_slice(&original[mutation.end_byte..]);
    Ok(mutated)
}

fn mutated_source_len(
    mutation: &MutationCandidate,
    original_len: usize,
) -> Result<usize, ApplyMutationError> {
    let removed = mutation.end_byte - mutation.start_byte;
    original_len
        .checked_sub(removed)
        .and_then(|remaining| remaining.checked_add(mutation.replacement.len()))
        .ok_or_else(|| {
            ApplyMutationError::Invalid("candidate replacement size overflows addressable memory".into())
        })
}

fn validate_mutated_source_size(
    mutated_len: usize,
    max_source_bytes: usize,
) -> Result<(), ApplyMutationError> {
    if mutated_len > max_source_bytes {
        return Err(ApplyMutationError::Invalid(format!(
            "mutated source would be {mutated_len} bytes, exceeding the executable mutation limit of {max_source_bytes} bytes"
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct PersistedMutationState {
    original_sha256: String,
    mutated_sha256: String,
    journal: PathBuf,
    active_pointer: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct MutationLimits {
    source_bytes: usize,
    journal_bytes: u64,
}

fn load_mutation_source(
    root: &Path,
    mutation: &MutationCandidate,
    max_source_bytes: usize,
) -> Result<(PathBuf, Vec<u8>, StoredPermissions), ApplyMutationError> {
    validate_mutation_source_limit(max_source_bytes)?;
    let path = resolve_source_path(root, &mutation.file, false)?;
    let (_metadata, original, permissions) = read_bounded_source(&path, max_source_bytes)?;
    validate_mutation_candidate(mutation, &original)?;
    Ok((path, original, permissions))
}

fn prepare_mutated_state(
    root: &Path,
    state: &Path,
    mutation: &MutationCandidate,
    original: &[u8],
    permissions: &StoredPermissions,
    limits: MutationLimits,
) -> Result<(Vec<u8>, PersistedMutationState), ApplyMutationError> {
    let mutated = build_mutated_source(mutation, original, limits.source_bytes)?;
    let record = mutation_journal_record(mutation, original, &mutated, permissions, limits.source_bytes);
    let persisted = persist_mutation_state(root, state, &record, limits.journal_bytes)?;
    Ok((mutated, persisted))
}

fn mutation_journal_record(
    mutation: &MutationCandidate,
    original: &[u8],
    mutated: &[u8],
    permissions: &StoredPermissions,
    max_source_bytes: usize,
) -> JournalRecord {
    JournalRecord {
        schema_version: JOURNAL_SCHEMA_VERSION,
        file: mutation.file.clone(),
        original_base64: BASE64.encode(original),
        original_sha256: sha256(original),
        mutated_sha256: sha256(mutated),
        max_source_bytes,
        original_permissions: permissions.clone(),
    }
}

fn persist_mutation_state(
    root: &Path,
    state: &Path,
    record: &JournalRecord,
    max_journal_bytes: u64,
) -> Result<PersistedMutationState, ApplyMutationError> {
    let journal = JournalFile::in_state(state).write(record, max_journal_bytes)?;
    // Pointer persistence can fail after its atomic rename has committed.
    // Retaining the synced journal is the only crash-safe choice; recovery
    // proves the source is still original before clearing either artifact.
    let active_pointer = ActiveRunFile::in_state(state)?.write(root, record)?;
    Ok(PersistedMutationState {
        original_sha256: record.original_sha256.clone(),
        mutated_sha256: record.mutated_sha256.clone(),
        journal,
        active_pointer,
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

    fn apply_with_limits(
        root: &Path,
        state: &Path,
        mutation: &MutationCandidate,
        max_source_bytes: usize,
        max_journal_bytes: u64,
    ) -> Result<Self, ApplyMutationError> {
        let max_source_bytes = max_source_bytes.min(MAX_MUTATION_SOURCE_BYTES);
        let (path, original, permissions) = load_mutation_source(root, mutation, max_source_bytes)?;
        let (mutated, persisted) = prepare_mutated_state(
            root,
            state,
            mutation,
            &original,
            &permissions,
            MutationLimits {
                source_bytes: max_source_bytes,
                journal_bytes: max_journal_bytes,
            },
        )?;
        // Keep the original mode and extended attributes while giving the
        // active mutant a fresh timestamp. Build tools such as Cargo commonly
        // use timestamps in their incremental fingerprints; restoring the
        // original timestamp here can make a real mutant execute stale code.
        if let Err(error) = atomic_replace_for_mutation(&path, &mutated, &permissions) {
            let _ = recover_active_locked(root, state);
            return Err(ApplyMutationError::Fatal(error));
        }
        Ok(Self {
            root: root.to_path_buf(),
            file: mutation.file.clone(),
            original,
            original_sha256: persisted.original_sha256,
            mutated_sha256: persisted.mutated_sha256,
            max_source_bytes,
            permissions,
            journal: persisted.journal,
            active_pointer: persisted.active_pointer,
            restored: false,
        })
    }

    pub(crate) fn restore(&mut self) -> Result<(), MutationError> {
        if self.restored {
            return Ok(());
        }
        let (path, current_sha256) = self.current_recovery_target()?;
        if current_sha256 == self.original_sha256 {
            return self.finish_restore_without_replacement();
        }
        self.restore_mutated_source(&path, &current_sha256)?;
        self.finish_restore_without_replacement()
    }

    fn current_recovery_target(&self) -> Result<(PathBuf, String), MutationError> {
        let path = existing_recovery_target(&self.root, &self.file, &self.journal)?;
        let current = read_current_for_recovery(&path, &self.journal, self.max_source_bytes)?;
        Ok((path, sha256(&current)))
    }

    fn restore_mutated_source(&self, path: &Path, current_sha256: &str) -> Result<(), MutationError> {
        if current_sha256 != self.mutated_sha256 {
            return Err(MutationError::RecoveryConflict {
                path: path.to_path_buf(),
                journal: self.journal.clone(),
            });
        }
        atomic_replace(path, &self.original, Some(&self.permissions))
    }

    fn finish_restore_without_replacement(&mut self) -> Result<(), MutationError> {
        StateFile::remove(&self.active_pointer, "remove active mutation pointer")?;
        StateFile::remove(&self.journal, "remove active journal")?;
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

    use reporigor_core::MutationCandidate;
    use tempfile::tempdir;

    use super::*;
    use crate::test_support::{candidate as test_candidate, BOOLEAN_TEXT};

    macro_rules! must {
        ($result:expr) => {
            $result.unwrap_or_else(|error| panic!("unexpected test error: {error:?}"))
        };
    }

    fn candidate(file: &str) -> MutationCandidate {
        test_candidate(1, file, "boolean-literal", "", BOOLEAN_TEXT)
    }

    struct MutationFixture {
        directory: tempfile::TempDir,
        state_base: tempfile::TempDir,
        path: PathBuf,
        root: PathBuf,
        state: PathBuf,
    }

    impl MutationFixture {
        fn new() -> Self {
            let directory = must!(tempdir());
            let state_base = must!(tempdir());
            let path = directory.path().join("sample.rs");
            must!(fs::write(&path, b"true\n"));
            let root = must!(canonical_root(directory.path()));
            let state = must!(prepare_state_directory_in(&root, state_base.path()));
            Self {
                directory,
                state_base,
                path,
                root,
                state,
            }
        }
    }

    fn apply_fixture(fixture: &MutationFixture) -> SourceRestoreGuard {
        must!(SourceRestoreGuard::apply(
            &fixture.root,
            &fixture.state,
            &candidate("sample.rs")
        ))
    }

    fn interrupt_fixture(fixture: &MutationFixture) {
        mem::forget(apply_fixture(fixture));
    }

    fn recover_fixture(fixture: &MutationFixture) -> RecoveryAction {
        must!(recover_active_locked(&fixture.root, &fixture.state))
    }

    fn read_fixture(fixture: &MutationFixture) -> Vec<u8> {
        must!(fs::read(&fixture.path))
    }

    fn write_fixture(fixture: &MutationFixture, bytes: &[u8]) {
        must!(fs::write(&fixture.path, bytes));
    }

    fn fixture_journal(fixture: &MutationFixture) -> PathBuf {
        JournalFile::in_state(&fixture.state).path
    }

    fn assert_fixture_content(fixture: &MutationFixture, expected: &[u8]) {
        assert_eq!(read_fixture(fixture), expected);
    }

    fn assert_fixture_state(fixture: &MutationFixture, expected: &[u8], journal_exists: bool) {
        assert_fixture_content(fixture, expected);
        assert_eq!(fixture_journal(fixture).is_file(), journal_exists);
    }

    fn assert_restored(fixture: &MutationFixture) {
        assert_eq!(recover_fixture(fixture), RecoveryAction::Restored);
        assert_fixture_state(fixture, b"true\n", false);
    }

    fn restore_guard(fixture: &MutationFixture, guard: &mut SourceRestoreGuard) {
        must!(guard.restore());
        assert_fixture_state(fixture, b"true\n", false);
    }

    fn fixture_lock(fixture: &MutationFixture, mode: Option<LockMode>) -> RunLockGuard {
        let result = match mode {
            Some(mode) => acquire_test_run_lock(&fixture.root, fixture.state_base.path(), mode),
            None => acquire_run_lock_with_base(&fixture.root, fixture.state_base.path()),
        };
        must!(result)
    }

    fn assert_fixture_lock_contended(fixture: &MutationFixture) {
        assert!(matches!(
            acquire_run_lock_with_base(&fixture.root, fixture.state_base.path()),
            Err(MutationError::AlreadyRunning { .. })
        ));
    }

    fn resize_fixture(fixture: &MutationFixture, length: u64) {
        let file = must!(OpenOptions::new().write(true).open(&fixture.path));
        must!(file.set_len(length));
    }

    fn assert_fixture_size(fixture: &MutationFixture, length: u64, journal_exists: bool) {
        assert_eq!(must!(fs::metadata(&fixture.path)).len(), length);
        assert_eq!(fixture_journal(fixture).exists(), journal_exists);
    }

    fn assert_recovery_conflict(fixture: &MutationFixture) {
        assert!(matches!(
            recover_active_locked(&fixture.root, &fixture.state),
            Err(MutationError::RecoveryConflict { .. })
        ));
    }

    fn timestamped_fixture() -> (MutationFixture, SystemTime) {
        let fixture = MutationFixture::new();
        let original_modified = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        set_modified_time(&fixture.path, original_modified);
        (fixture, original_modified)
    }

    fn set_modified_time(path: &Path, modified: SystemTime) {
        let file = must!(File::options().write(true).open(path));
        must!(file.set_times(FileTimes::new().set_modified(modified)));
    }

    fn fixture_modified(fixture: &MutationFixture) -> SystemTime {
        must!(must!(fs::metadata(&fixture.path)).modified())
    }

    #[cfg(unix)]
    struct RelocatableFixture {
        _parent: tempfile::TempDir,
        state_base: tempfile::TempDir,
        original_path: PathBuf,
        alternate_path: PathBuf,
        state: PathBuf,
    }

    #[cfg(unix)]
    fn interrupted_checkout(original_name: &str, alternate_name: &str) -> RelocatableFixture {
        let parent = must!(tempdir());
        let state_base = must!(tempdir());
        let original_path = parent.path().join(original_name);
        let alternate_path = parent.path().join(alternate_name);
        must!(fs::create_dir(&original_path));
        must!(fs::write(original_path.join("sample.rs"), b"true\n"));
        let original_root = must!(canonical_root(&original_path));
        let state = must!(prepare_state_directory_in(&original_root, state_base.path()));
        let guard = must!(SourceRestoreGuard::apply(
            &original_root,
            &state,
            &candidate("sample.rs")
        ));
        mem::forget(guard);
        RelocatableFixture {
            _parent: parent,
            state_base,
            original_path,
            alternate_path,
            state,
        }
    }

    #[cfg(unix)]
    fn relocated_checkout(original_name: &str, alternate_name: &str) -> RelocatableFixture {
        let fixture = interrupted_checkout(original_name, alternate_name);
        must!(fs::rename(&fixture.original_path, &fixture.alternate_path));
        fixture
    }

    #[test]
    fn source_guard_atomically_restores_original_bytes() {
        let fixture = MutationFixture::new();

        {
            let _guard = apply_fixture(&fixture);
            assert_fixture_state(&fixture, b"false\n", true);
        }

        assert_fixture_state(&fixture, b"true\n", false);
        let temporary_files = must!(fs::read_dir(fixture.directory.path()))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
    }

    #[test]
    fn interrupted_mutation_is_recovered_from_journal() {
        let fixture = MutationFixture::new();
        interrupt_fixture(&fixture);

        assert_fixture_content(&fixture, b"false\n");
        assert_restored(&fixture);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_uses_root_identity_when_checkout_paths_change() {
        let fixture = relocated_checkout("original", "moved");
        let moved_root = must!(canonical_root(&fixture.alternate_path));
        let moved_state = must!(prepare_state_directory_in(&moved_root, fixture.state_base.path()));

        assert_eq!(
            must!(recover_active_locked(&moved_root, &moved_state)),
            RecoveryAction::Restored
        );
        assert_eq!(
            must!(fs::read(fixture.alternate_path.join("sample.rs"))),
            b"true\n"
        );

        let fixture = relocated_checkout("checkout", "displaced");
        must!(fs::create_dir(&fixture.original_path));
        must!(fs::write(fixture.original_path.join("sample.rs"), b"false\n"));
        let replacement_root = must!(canonical_root(&fixture.original_path));

        assert!(matches!(
            recover_active_locked(&replacement_root, &fixture.state),
            Err(MutationError::InvalidJournal { .. })
        ));
        assert_eq!(
            must!(fs::read(fixture.original_path.join("sample.rs"))),
            b"false\n"
        );
    }

    #[test]
    fn pending_journal_detection_is_read_only_and_tracks_recovery() {
        let directory = must!(tempdir());
        let state_base = must!(tempdir());
        let root = must!(canonical_root(directory.path()));
        let absent_state = state_base.path().join("absent");
        assert!(must!(pending_mutation_in_state(&root, &absent_state)).is_none());
        assert!(!absent_state.exists(), "detection must not create state");

        let path = root.join("sample.rs");
        must!(fs::write(&path, b"true\n"));
        let state = must!(prepare_state_directory_in(&root, state_base.path()));
        assert!(must!(pending_mutation_in_state(&root, &state)).is_none());
        let guard = must!(SourceRestoreGuard::apply(&root, &state, &candidate("sample.rs")));
        mem::forget(guard);

        let pending = must!(pending_mutation_in_state(&root, &state));
        let pending = pending.unwrap_or_else(|| panic!("active mutation was not reported"));
        assert_eq!(pending.root, root);
        assert_eq!(pending.journal, JournalFile::in_state(&state).path);
        assert!(pending.active_pointer.is_some());
        assert_eq!(
            must!(recover_active_locked(&root, &state)),
            RecoveryAction::Restored
        );
        assert!(must!(pending_mutation_in_state(&root, &state)).is_none());
    }

    #[test]
    fn public_pending_detection_rejects_a_missing_root_before_state_lookup() {
        let directory = must!(tempdir());
        let missing = directory.path().join("missing");

        assert!(matches!(
            pending_mutation_journal(&missing),
            Err(MutationError::InvalidRoot { .. })
        ));
        assert!(!missing.exists());
    }

    #[test]
    fn pointer_without_journal_is_cleared_only_when_source_is_original() {
        let clean = MutationFixture::new();
        interrupt_fixture(&clean);
        must!(StateFile::remove(
            &fixture_journal(&clean),
            "remove active journal"
        ));
        write_fixture(&clean, b"true\n");

        assert_eq!(recover_fixture(&clean), RecoveryAction::AlreadyClean);
        assert!(!ActiveRunFile::in_base(clean.state_base.path()).path.exists());

        let mutant = MutationFixture::new();
        interrupt_fixture(&mutant);
        must!(StateFile::remove(
            &fixture_journal(&mutant),
            "remove active journal"
        ));

        assert!(matches!(
            recover_active_locked(&mutant.root, &mutant.state),
            Err(MutationError::MissingRecoveryJournal { .. })
        ));
        assert_fixture_content(&mutant, b"false\n");
        assert!(ActiveRunFile::in_base(mutant.state_base.path()).path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_restores_original_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = MutationFixture::new();
        must!(fs::set_permissions(&fixture.path, Permissions::from_mode(0o744)));
        interrupt_fixture(&fixture);
        must!(fs::set_permissions(&fixture.path, Permissions::from_mode(0o600)));

        assert_restored(&fixture);
        assert_eq!(
            must!(fs::metadata(&fixture.path)).permissions().mode() & 0o777,
            0o744
        );
    }

    #[test]
    fn recovery_restores_original_file_timestamps() {
        let (fixture, original_modified) = timestamped_fixture();
        interrupt_fixture(&fixture);
        set_modified_time(&fixture.path, SystemTime::now());

        assert_restored(&fixture);
        assert_eq!(fixture_modified(&fixture), original_modified);
    }

    #[test]
    fn active_mutation_refreshes_timestamp_before_restoring_it() {
        let (fixture, original_modified) = timestamped_fixture();

        let mut guard = apply_fixture(&fixture);
        assert_ne!(fixture_modified(&fixture), original_modified);
        assert_fixture_content(&fixture, b"false\n");

        restore_guard(&fixture, &mut guard);
        assert_eq!(fixture_modified(&fixture), original_modified);
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn recovery_restores_extended_attributes() {
        let fixture = MutationFixture::new();
        let file = must!(File::open(&fixture.path));
        #[cfg(target_os = "linux")]
        let attribute = "user.reporigor-test";
        #[cfg(target_vendor = "apple")]
        let attribute = "com.reporigor.test";
        match rustix::fs::fsetxattr(&file, attribute, b"retained", rustix::fs::XattrFlags::empty()) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::NOTSUP => return,
            Err(error) => {
                panic!(
                    "unexpected xattr error: {:?}",
                    std::io::Error::from_raw_os_error(error.raw_os_error())
                );
            }
        }
        interrupt_fixture(&fixture);
        assert_restored(&fixture);
        let file = must!(File::open(&fixture.path));
        let mut value = vec![0_u8; 32];
        let length = must!(rustix::fs::fgetxattr(&file, attribute, &mut value));
        value.truncate(length);
        assert_eq!(value, b"retained");
    }

    #[test]
    fn recovery_does_not_overwrite_unrecognized_content() {
        let fixture = MutationFixture::new();
        interrupt_fixture(&fixture);
        write_fixture(&fixture, b"manual edit\n");

        assert_recovery_conflict(&fixture);
        assert_fixture_state(&fixture, b"manual edit\n", true);
    }

    #[test]
    fn normal_guard_refuses_to_overwrite_an_independent_edit() {
        let fixture = MutationFixture::new();
        let mut guard = apply_fixture(&fixture);
        write_fixture(&fixture, b"independent edit\n");

        let error = match guard.restore() {
            Ok(()) => {
                panic!("independent source edit was overwritten instead of conflicting");
            }
            Err(error) => error,
        };
        assert!(matches!(&error, MutationError::RecoveryConflict { .. }));
        assert!(error.to_string().contains("source was left unchanged"));
        drop(guard);

        assert_fixture_state(&fixture, b"independent edit\n", true);

        write_fixture(&fixture, b"false\n");
        assert_restored(&fixture);
    }

    #[test]
    fn oversized_journal_is_rejected_before_source_replacement() {
        let fixture = MutationFixture::new();
        let result = SourceRestoreGuard::apply_with_limits(
            &fixture.root,
            &fixture.state,
            &candidate("sample.rs"),
            MAX_MUTATION_SOURCE_BYTES,
            64,
        );
        assert!(matches!(
            result,
            Err(ApplyMutationError::Fatal(MutationError::InvalidJournal { .. }))
        ));
        assert_fixture_state(&fixture, b"true\n", false);
    }

    #[test]
    fn sparse_huge_source_is_rejected_before_journaling_or_allocation() {
        let fixture = MutationFixture::new();
        resize_fixture(&fixture, 1024 * 1024 * 1024);

        let result =
            SourceRestoreGuard::apply_bounded(&fixture.root, &fixture.state, &candidate("sample.rs"), 1024);
        assert!(matches!(
            result,
            Err(ApplyMutationError::Fatal(MutationError::SourceTooLarge { .. }))
        ));
        assert_fixture_size(&fixture, 1024 * 1024 * 1024, false);
    }

    #[test]
    fn oversized_replacement_is_rejected_before_journaling() {
        let fixture = MutationFixture::new();
        let mut oversized = candidate("sample.rs");
        oversized.replacement = "x".repeat(32);

        assert!(matches!(
            SourceRestoreGuard::apply_bounded(&fixture.root, &fixture.state, &oversized, 16),
            Err(ApplyMutationError::Invalid(_))
        ));
        assert_fixture_state(&fixture, b"true\n", false);
    }

    #[test]
    fn sparse_growth_after_apply_is_a_recoverable_conflict() {
        let fixture = MutationFixture::new();
        let mut guard = must!(SourceRestoreGuard::apply_bounded(
            &fixture.root,
            &fixture.state,
            &candidate("sample.rs"),
            1024
        ));
        resize_fixture(&fixture, 1024 * 1024 * 1024);

        assert!(matches!(
            guard.restore(),
            Err(MutationError::RecoveryConflict { .. })
        ));
        assert_fixture_size(&fixture, 1024 * 1024 * 1024, true);
        drop(guard);

        write_fixture(&fixture, b"false\n");
        assert_restored(&fixture);
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_source_is_rejected_before_journaling() {
        use std::os::unix::fs::MetadataExt;

        let fixture = MutationFixture::new();
        let linked = fixture.directory.path().join("linked.rs");
        must!(fs::hard_link(&fixture.path, &linked));

        assert!(matches!(
            SourceRestoreGuard::apply(&fixture.root, &fixture.state, &candidate("sample.rs")),
            Err(ApplyMutationError::Fatal(
                MutationError::UnsupportedSourceMetadata { .. }
            ))
        ));
        assert_eq!(
            must!(fs::metadata(&fixture.path)).ino(),
            must!(fs::metadata(&linked)).ino()
        );
        assert_fixture_state(&fixture, b"true\n", false);
        assert_eq!(must!(fs::read(&linked)), b"true\n");
    }

    #[test]
    fn recovery_never_recreates_a_missing_source() {
        let fixture = MutationFixture::new();
        interrupt_fixture(&fixture);
        must!(fs::remove_file(&fixture.path));

        assert_recovery_conflict(&fixture);
        assert!(!fixture.path.exists());
        assert!(fixture_journal(&fixture).exists());
    }

    #[test]
    fn source_path_validation_rejects_escapes_and_control_paths() {
        let fixture = MutationFixture::new();
        assert!(matches!(
            resolve_source_path(&fixture.root, "../outside.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));
        assert!(matches!(
            resolve_source_path(&fixture.root, "/outside.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));

        assert_eq!(
            must!(resolve_source_path(&fixture.root, "missing.rs", true)),
            fixture.root.join("missing.rs")
        );
        assert!(matches!(
            resolve_source_path(&fixture.root, "missing/source.rs", true),
            Err(MutationError::Io { .. })
        ));

        for path in [".git/config", "nested/.hg/store", "target/reporigor/active.json"] {
            assert!(matches!(
                resolve_source_path(&fixture.root, path, false),
                Err(MutationError::UnsafePath { .. })
            ));
        }

        let control_root = fixture.directory.path().join(".git/worktree");
        must!(fs::create_dir_all(&control_root));
        must!(fs::write(control_root.join("source.rs"), b"true\n"));
        let control_root = must!(canonical_root(&control_root));
        assert!(matches!(
            resolve_source_path(&control_root, "source.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));

        let nondirectory_root = fixture.directory.path().join("root-file");
        must!(fs::write(&nondirectory_root, b"not a directory"));
        assert!(matches!(
            existing_recovery_target(&nondirectory_root, "child.rs", &fixture_journal(&fixture)),
            Err(MutationError::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn source_and_state_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = must!(tempdir());
        let outside = must!(tempdir());
        let outside_source = outside.path().join("outside.rs");
        must!(fs::write(&outside_source, b"true\n"));
        must!(symlink(&outside_source, directory.path().join("linked.rs")));
        let root = must!(canonical_root(directory.path()));
        assert!(matches!(
            resolve_source_path(&root, "linked.rs", false),
            Err(MutationError::UnsafePath { .. })
        ));

        let state_root = must!(tempdir());
        let state_link = state_root.path().join("state-link");
        must!(symlink(outside.path(), &state_link));
        assert!(matches!(
            acquire_run_lock_with_base(&root, &state_link),
            Err(MutationError::UnsafePath { .. })
        ));
    }

    #[test]
    fn run_lock_rejects_a_second_executor() {
        let fixture = MutationFixture::new();
        let first = fixture_lock(&fixture, None);
        assert_fixture_lock_contended(&fixture);
        drop(first);
        fixture_lock(&fixture, None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_violations_are_classified_as_contention() {
        assert!(lock_is_contended(&std::io::Error::from_raw_os_error(32)));
        assert!(lock_is_contended(&std::io::Error::from_raw_os_error(33)));
    }

    #[test]
    fn global_lock_serializes_different_and_potentially_overlapping_roots() {
        let workspace = must!(tempdir());
        let nested = workspace.path().join("package");
        must!(fs::create_dir(&nested));
        let state_base = must!(tempdir());
        let workspace_root = must!(canonical_root(workspace.path()));
        let nested_root = must!(canonical_root(&nested));

        let first = must!(acquire_run_lock_with_base(&workspace_root, state_base.path()));
        assert!(matches!(
            acquire_run_lock_with_base(&nested_root, state_base.path()),
            Err(MutationError::AlreadyRunning { .. })
        ));
        drop(first);
        must!(acquire_run_lock_with_base(&nested_root, state_base.path()));
    }

    #[test]
    fn shared_analysis_locks_coexist_and_a_writer_waits_for_release() {
        let fixture = MutationFixture::new();
        let [first, second] = [0, 1].map(|_| fixture_lock(&fixture, Some(LockMode::Shared)));
        assert_fixture_lock_contended(&fixture);
        drop(second);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(first);
        });
        let writer = fixture_lock(&fixture, Some(LockMode::Execution));
        must!(release.join());
        drop(writer);
    }

    #[test]
    fn project_target_cleanup_cannot_remove_lock_or_journal() {
        let fixture = MutationFixture::new();
        must!(fs::create_dir(fixture.root.join("target")));
        let lock = fixture_lock(&fixture, None);
        let mut guard = must!(SourceRestoreGuard::apply(
            &fixture.root,
            &lock.state,
            &candidate("sample.rs")
        ));
        let journal = JournalFile::in_state(&lock.state).path;
        assert!(journal.is_file());

        must!(fs::remove_dir_all(fixture.root.join("target")));

        assert!(journal.is_file());
        assert_fixture_lock_contended(&fixture);
        restore_guard(&fixture, &mut guard);
    }

    #[cfg(unix)]
    #[test]
    fn state_directories_and_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = MutationFixture::new();
        let lock = fixture_lock(&fixture, None);
        let guard = must!(SourceRestoreGuard::apply(
            &fixture.root,
            &lock.state,
            &candidate("sample.rs")
        ));

        assert_eq!(
            must!(fs::metadata(fixture.state_base.path()))
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            must!(fs::metadata(&lock.state)).permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            must!(fs::metadata(fixture.state_base.path().join(RUN_LOCK)))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            must!(fs::metadata(JournalFile::in_state(&lock.state).path))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(guard);
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
