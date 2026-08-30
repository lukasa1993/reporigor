use std::{
    fs,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use reporigor_core::{Language, SourceFile};

use crate::{CommandOrigin, CompileCommand};

pub(crate) fn temp_dir() -> TempDir {
    tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"))
}

pub(crate) fn expect_error<T: std::fmt::Debug, E>(result: Result<T, E>) -> E {
    match result {
        Ok(value) => panic!("expected error, got {value:?}"),
        Err(error) => error,
    }
}

pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fixture_io(path, "write", || fs::write(path, contents));
}

pub(crate) fn create_dir(path: impl AsRef<Path>) {
    let path = path.as_ref();
    fixture_io(path, "create", || fs::create_dir(path));
}

fn fixture_io(path: &Path, action: &str, operation: impl FnOnce() -> std::io::Result<()>) {
    if let Err(error) = operation() {
        panic!("{action} {}: {error}", path.display());
    }
}

pub(crate) fn write_database(root: &Path, contents: impl AsRef<[u8]>) -> PathBuf {
    write_file(root, "compile_commands.json", contents)
}

pub(crate) fn write_file(root: &Path, relative: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(relative);
    write(&path, contents);
    path
}

pub(crate) fn write_json_database(root: &Path, value: &serde_json::Value) {
    let contents = serde_json::to_vec(value)
        .unwrap_or_else(|error| panic!("serialize compilation database fixture: {error}"));
    write_database(root, contents);
}

pub(crate) fn compilation_entry(
    root: &Path,
    file: &Path,
    arguments: impl serde::Serialize,
) -> serde_json::Value {
    serde_json::json!({
        "directory": root,
        "file": file,
        "arguments": arguments,
    })
}

pub(crate) fn owned_words(value: &str) -> Vec<String> {
    value.split_ascii_whitespace().map(str::to_owned).collect()
}

pub(crate) fn compile_command(directory: &Path, file: &str, arguments: &[&str]) -> CompileCommand {
    let arguments = arguments.iter().map(ToString::to_string).collect::<Vec<_>>();
    CompileCommand {
        directory: directory.to_path_buf(),
        file: directory.join(file),
        origin: CommandOrigin::Arguments(arguments.clone()),
        arguments,
        output: None,
    }
}

pub(crate) fn source_file(path: impl Into<PathBuf>, relative: &str, language: Language) -> SourceFile {
    SourceFile {
        path: path.into(),
        relative: relative.to_string(),
        language,
        generated: false,
        test: false,
    }
}

#[cfg(unix)]
pub(crate) fn write_executable(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;

    let path = path.as_ref();
    write(path, contents);
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("metadata for {}: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    fixture_io(path, "set permissions for", || {
        fs::set_permissions(path, permissions)
    });
}

#[cfg(unix)]
pub(crate) fn executable_fixture(contents: impl AsRef<[u8]>) -> (TempDir, PathBuf) {
    let temp = temp_dir();
    let executable = temp.path().join("fake-clang");
    write_executable(&executable, contents);
    (temp, executable)
}
