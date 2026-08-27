use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use reporigor_reporting::ReportEnvelope;

const LANGUAGES: [&str; 8] = [
    "bash",
    "c",
    "cpp",
    "objective-c",
    "python",
    "rust",
    "swift",
    "typescript",
];
const INVALID_UTF8_FIXTURES: [(&str, &str); 8] = [
    ("bash", "invalid.sh"),
    ("c", "invalid.c"),
    ("cpp", "invalid.cpp"),
    ("objective-c", "invalid.m"),
    ("python", "invalid.py"),
    ("rust", "invalid.rs"),
    ("swift", "invalid.swift"),
    ("typescript", "invalid.ts"),
];
const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn malformed_input_is_bounded_and_reported_by_every_generic_grammar() {
    let root = fixture_path("fixtures/projects/malformed");

    for language in LANGUAGES {
        let permissive = run_bounded(arguments(language, &root, true), PROCESS_TIMEOUT);
        assert_normal_exit(&permissive, language, "permissive");
        assert_eq!(
            permissive.status.code(),
            Some(0),
            "{language}: permissive analysis failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&permissive.stdout),
            String::from_utf8_lossy(&permissive.stderr),
        );
        let report: ReportEnvelope = serde_json::from_slice(&permissive.stdout).unwrap_or_else(|error| {
            panic!(
                "{language}: permissive analysis emitted invalid JSON: {error}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&permissive.stdout),
                String::from_utf8_lossy(&permissive.stderr),
            )
        });
        assert_eq!(report.summary.files, 1, "{language}: wrong fixture selection");
        assert!(
            report.summary.parse_errors > 0,
            "{language}: malformed source produced no parse errors: {report:?}"
        );
        assert!(
            report
                .backends
                .iter()
                .any(|backend| backend.id == "tree-sitter-generic" && !backend.native),
            "{language}: generic tree-sitter provenance is missing: {:?}",
            report.backends
        );

        let strict = run_bounded(arguments(language, &root, false), PROCESS_TIMEOUT);
        assert_normal_exit(&strict, language, "strict");
        assert_eq!(
            strict.status.code(),
            Some(1),
            "{language}: strict malformed input was not an operational error\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&strict.stdout),
            String::from_utf8_lossy(&strict.stderr),
        );
        assert!(
            !strict.stderr.is_empty(),
            "{language}: strict parse failure did not explain the operational error"
        );
    }
}

#[test]
fn invalid_utf8_is_operational_for_every_generic_language_and_parse_policy() {
    for (language, filename) in INVALID_UTF8_FIXTURES {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("{language} fixture: {error}"));
        std::fs::write(
            project.path().join(filename),
            [b'v', b'a', b'l', b' ', 0xff, b'\n'],
        )
        .unwrap_or_else(|error| panic!("{language} source: {error}"));

        for allow_parse_errors in [false, true] {
            let output = run_bounded(
                arguments(language, project.path(), allow_parse_errors),
                PROCESS_TIMEOUT,
            );
            assert_normal_exit(&output, language, "invalid UTF-8");
            assert_eq!(
                output.status.code(),
                Some(1),
                "{language}: invalid UTF-8 unexpectedly produced a report"
            );
            assert!(
                output.stdout.is_empty(),
                "{language}: invalid UTF-8 emitted partial JSON"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"),
                "{language}: unexpected invalid UTF-8 error: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

fn arguments(language: &str, root: &Path, allow_parse_errors: bool) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--backend"),
        OsString::from("generic"),
        OsString::from("--language"),
        OsString::from(language),
        OsString::from("--format"),
        OsString::from("json"),
    ];
    if allow_parse_errors {
        arguments.push(OsString::from("--allow-parse-errors"));
    }
    arguments.extend([
        OsString::from("dry"),
        OsString::from("--min-tokens"),
        OsString::from("4"),
        root.as_os_str().to_owned(),
    ]);
    arguments
}

fn run_bounded(arguments: Vec<OsString>, timeout: Duration) -> CapturedOutput {
    let mut child = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start reporigor: {error}"));
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("reporigor exceeded the {timeout:?} subprocess timeout");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("failed while waiting for reporigor: {error}");
            }
        }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap_or_else(|| panic!("reporigor stdout pipe was unexpectedly unavailable"))
        .read_to_end(&mut stdout)
        .unwrap_or_else(|error| panic!("failed to read reporigor stdout: {error}"));
    child
        .stderr
        .take()
        .unwrap_or_else(|| panic!("reporigor stderr pipe was unexpectedly unavailable"))
        .read_to_end(&mut stderr)
        .unwrap_or_else(|error| panic!("failed to read reporigor stderr: {error}"));

    CapturedOutput {
        status,
        stdout,
        stderr,
    }
}

fn assert_normal_exit(output: &CapturedOutput, language: &str, mode: &str) {
    assert!(
        output.status.code().is_some(),
        "{language}: {mode} subprocess terminated by a signal or platform exception"
    );
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(relative)
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
