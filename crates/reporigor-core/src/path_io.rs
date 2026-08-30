use std::path::Path;

use crate::CoreError;

/// Return whether `path` names an executable regular file.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) trait CoreIoResult<T> {
    fn for_read_path(self, path: &Path) -> Result<T, CoreError>;
}

impl<T> CoreIoResult<T> for std::io::Result<T> {
    fn for_read_path(self, path: &Path) -> Result<T, CoreError> {
        self.map_err(|source| CoreError::Read {
            path: path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum ExpectedCoreError {
    FileTooLarge,
    UnsafePath,
}

#[cfg(test)]
pub(crate) fn assert_core_error<T>(result: Result<T, CoreError>, expected: ExpectedCoreError) {
    let error = require_error(result, "operation must be rejected");
    let matched = match expected {
        ExpectedCoreError::FileTooLarge => matches!(error, CoreError::FileTooLarge { .. }),
        ExpectedCoreError::UnsafePath => matches!(error, CoreError::UnsafePath { .. }),
    };
    assert!(matched, "unexpected core error: {error:?}");
}

#[cfg(test)]
pub(crate) fn require_error<T, E>(result: Result<T, E>, message: &str) -> E {
    let Err(error) = result else {
        panic!("{message}");
    };
    error
}

#[cfg(test)]
pub(crate) fn sparse_test_file(name: &str, size: u64) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = test_directory("fixture");
    let path = root.path().join(name);
    let file = std::fs::File::create(&path).unwrap_or_else(|error| panic!("sparse file: {error}"));
    file.set_len(size)
        .unwrap_or_else(|error| panic!("sparse length: {error}"));
    (root, path)
}

#[cfg(test)]
pub(crate) fn external_test_file(
    name: &str,
    contents: &str,
) -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let root = test_directory("root");
    let outside = test_directory("outside");
    let target = outside.path().join(name);
    std::fs::write(&target, contents).unwrap_or_else(|error| panic!("external file: {error}"));
    (root, outside, target)
}

#[cfg(test)]
fn test_directory(label: &str) -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap_or_else(|error| panic!("{label}: {error}"))
}
