// Each integration-test binary imports this shared module independently and
// therefore uses a different subset of its helpers.
#![allow(dead_code)]

pub(super) const GENERIC_LANGUAGES: &str = "bash|c|cpp|objective-c|python|rust|swift|typescript";

pub(super) mod invocation {
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::{Command, Output, Stdio};

    use analysis_mutate::STATE_DIRECTORY_ENV;

    pub(crate) fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Output, std::io::Error> {
        Command::new(env!("CARGO_BIN_EXE_reporigor"))
            .args(arguments)
            .output()
    }

    pub(crate) fn spawn_piped(mut command: Command) -> Result<std::process::Child, std::io::Error> {
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.spawn()
    }

    pub(crate) fn run_isolated(arguments: &[OsString]) -> Output {
        let state = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create isolated CLI state: {error}"));
        Command::new(env!("CARGO_BIN_EXE_reporigor"))
            .args(arguments)
            .env(STATE_DIRECTORY_ENV, state.path())
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("failed to start reporigor: {error}"))
    }

    pub(crate) fn run_at(arguments: &[&str], root: &Path) -> Result<Output, std::io::Error> {
        run(arguments
            .iter()
            .map(OsString::from)
            .chain(std::iter::once(root.as_os_str().to_owned())))
    }
}

pub(super) mod json_arguments {
    use std::ffi::OsString;
    use std::path::Path;

    pub(crate) fn json_arguments(
        backend: &str,
        language: &str,
        command: &str,
        command_arguments: &[&str],
        root: &Path,
    ) -> Vec<OsString> {
        super::json_arguments_with_globals::json_arguments_with_globals(
            backend,
            language,
            backend == "native",
            &[],
            (command, command_arguments),
            root,
        )
    }
}

pub(super) mod json_arguments_with_globals {
    use std::ffi::OsString;
    use std::path::Path;

    pub(crate) fn json_arguments_with_globals(
        backend: &str,
        language: &str,
        allow_project_exec: bool,
        global_arguments: &[&str],
        command: (&str, &[&str]),
        root: &Path,
    ) -> Vec<OsString> {
        let (command, command_arguments) = command;
        let mut arguments = vec![OsString::from("--backend"), OsString::from(backend)];
        if allow_project_exec {
            arguments.push(OsString::from("--allow-project-exec"));
        }
        arguments.extend([
            OsString::from("--language"),
            OsString::from(language),
            OsString::from("--format"),
            OsString::from("json"),
        ]);
        arguments.extend(global_arguments.iter().map(OsString::from));
        arguments.push(OsString::from(command));
        arguments.extend(command_arguments.iter().map(OsString::from));
        arguments.push(root.as_os_str().to_owned());
        arguments
    }
}

pub(super) mod exit_assertion {
    use std::process::Output;

    pub(crate) fn assert_exit(output: &Output, expected: i32, context: &str) {
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{context} returned {:?}\n{}",
            output.status.code(),
            super::captured_output::captured_output(output)
        );
    }
}

pub(super) mod operational_error {
    use std::process::Output;

    pub(crate) fn operational_error(output: &Output, context: &str) -> String {
        super::exit_assertion::assert_exit(output, 1, context);
        assert!(output.stdout.is_empty(), "{context} emitted a partial report");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

pub(super) mod operational_error_assertion {
    use std::process::Output;

    pub(crate) fn assert_operational_error_contains(output: &Output, context: &str, needle: &str) {
        let stderr = super::operational_error::operational_error(output, context);
        assert!(
            stderr.contains(needle),
            "{context} returned an unexpected error: {stderr}"
        );
    }
}

pub(super) mod message_assertion {
    pub(crate) fn assert_message_contains_all(message: &str, context: &str, expected: &[&str]) {
        assert!(
            expected.iter().all(|needle| message.contains(needle)),
            "{context}: {message}"
        );
    }
}

pub(super) mod output_parser {
    use std::process::Output;

    use serde::de::DeserializeOwned;

    pub(crate) fn parse_output<T: DeserializeOwned>(output: &Output, context: &str) -> T {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{context} emitted invalid JSON: {error}\n{}",
                super::captured_output::captured_output(output)
            )
        })
    }
}

pub(super) mod incomplete_check_assertion {
    use std::process::Output;

    pub(crate) fn assert_incomplete_json_check(output: &Output, context: &str) -> serde_json::Value {
        super::exit_assertion::assert_exit(output, 2, context);
        let report: serde_json::Value = super::output_parser::parse_output(output, context);
        assert!(
            report["summary"]["omitted_checks"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "{context} did not disclose omitted evidence"
        );
        assert_eq!(
            report["results"]["rules"]["baseline"]["gate_passed"], false,
            "{context} allowed omissions through the integrated gate"
        );
        report
    }
}

pub(super) mod success_assertion {
    use std::process::Output;

    pub(crate) fn assert_success(output: &Output, context: &str) {
        assert!(
            output.status.success(),
            "{context} failed\n{}",
            super::captured_output::captured_output(output)
        );
    }
}

pub(super) mod captured_output {
    use std::process::Output;

    pub(crate) fn captured_output(output: &Output) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

pub(super) mod generic_backend_assertion {
    use reporigor_reporting::ReportEnvelope;

    pub(crate) fn assert_generic_backend(report: &ReportEnvelope, context: &str) {
        assert!(
            report
                .backends
                .iter()
                .any(|backend| backend.id == "tree-sitter-generic" && !backend.native),
            "{context}: generic backend missing from report: {:?}",
            report.backends
        );
    }
}

pub(super) mod environment {
    use std::process::{Command, Stdio};

    pub(crate) fn command_available(program: &str) -> bool {
        Command::new(program)
            .arg("--version")
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

pub(super) mod paths {
    use std::path::PathBuf;

    pub(crate) fn fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative)
    }
}

pub(super) mod canonical_path {
    use std::path::{Path, PathBuf};

    pub(crate) fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()))
    }
}

pub(super) mod fixtures {
    use std::path::Path;

    pub(crate) fn write_fixture(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create fixture directory {}: {error}", parent.display()));
        }
        std::fs::write(path, contents)
            .unwrap_or_else(|error| panic!("write fixture {}: {error}", path.display()));
    }

    pub(crate) fn temporary_project_file(path: &str, contents: &str) -> tempfile::TempDir {
        let project = temporary_project();
        write_fixture(&project.path().join(path), contents);
        project
    }

    pub(crate) fn retained_python_project() -> tempfile::TempDir {
        temporary_project_file("sample.py", "def retained(value):\n    return value + 1\n")
    }

    pub(crate) fn rust_project(name: &str, source: &str) -> tempfile::TempDir {
        let project = temporary_project();
        let files = [
            (
                "Cargo.toml",
                format!(
                    "[package]\nname = {name:?}\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n"
                ),
            ),
            (
                "Cargo.lock",
                format!(
                    "# This file is automatically @generated by Cargo.\nversion = 3\n\n[[package]]\nname = {name:?}\nversion = \"0.1.0\"\n"
                ),
            ),
            ("src/lib.rs", source.to_owned()),
        ];
        for (path, contents) in files {
            write_fixture(&project.path().join(path), &contents);
        }
        project
    }

    fn temporary_project() -> tempfile::TempDir {
        tempfile::tempdir().unwrap_or_else(|error| panic!("fixture project: {error}"))
    }

    pub(crate) fn write_executable_fixture(path: &Path, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        mark_executable(path)
    }

    fn mark_executable(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }
}
