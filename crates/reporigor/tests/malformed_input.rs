use std::{
    ffi::OsString,
    io::Read,
    path::Path,
    process::{Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

pub mod support;
use support::captured_output::captured_output;
use support::generic_backend_assertion::assert_generic_backend;
use support::invocation::spawn_piped;
use support::json_arguments_with_globals::json_arguments_with_globals;
use support::operational_error_assertion::assert_operational_error_contains;
use support::paths::fixture_path;
use support::GENERIC_LANGUAGES;

use reporigor_reporting::ReportEnvelope;

const INVALID_UTF8_FIXTURES: &str = "bash=invalid.sh,c=invalid.c,cpp=invalid.cpp,objective-c=invalid.m,python=invalid.py,rust=invalid.rs,swift=invalid.swift,typescript=invalid.ts";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn malformed_input_is_bounded_and_reported_by_every_generic_grammar() {
    let root = fixture_path("fixtures/projects/malformed");

    for language in GENERIC_LANGUAGES.split('|') {
        let permissive = run_bounded(arguments(language, &root, true), PROCESS_TIMEOUT);
        assert_normal_exit(&permissive, language, "permissive");
        assert_eq!(
            permissive.status.code(),
            Some(0),
            "{language}: permissive analysis failed\n{}",
            captured_output(&permissive.as_output()),
        );
        let report: ReportEnvelope = serde_json::from_slice(&permissive.stdout).unwrap_or_else(|error| {
            panic!(
                "{language}: permissive analysis emitted invalid JSON: {error}\n{}",
                captured_output(&permissive.as_output()),
            )
        });
        assert_eq!(report.summary.files, 1, "{language}: wrong fixture selection");
        assert!(
            report.summary.parse_errors > 0,
            "{language}: malformed source produced no parse errors: {report:?}"
        );
        assert_generic_backend(&report, language);

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
    for fixture in INVALID_UTF8_FIXTURES.split(',') {
        let (language, filename) = fixture
            .split_once('=')
            .unwrap_or_else(|| panic!("invalid UTF-8 fixture descriptor"));
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
            assert_operational_error_contains(
                &output.as_output(),
                &format!("{language}: invalid UTF-8"),
                "not valid UTF-8",
            );
        }
    }
}

fn arguments(language: &str, root: &Path, allow_parse_errors: bool) -> Vec<std::ffi::OsString> {
    let global_arguments = if allow_parse_errors {
        &["--allow-parse-errors"][..]
    } else {
        &[]
    };
    json_arguments_with_globals(
        "generic",
        language,
        false,
        global_arguments,
        ("dry", &["--min-tokens", "4"]),
        root,
    )
}

fn run_bounded(arguments: Vec<OsString>, timeout: Duration) -> CapturedOutput {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporigor"));
    command.args(arguments);
    let mut child = spawn_piped(command).unwrap_or_else(|error| panic!("failed to start reporigor: {error}"));
    let deadline = Instant::now() + timeout;

    let status = wait_for_child(&mut child, deadline, timeout);
    let [stdout, stderr] = capture_child_output(&mut child);
    CapturedOutput {
        status,
        stdout,
        stderr,
    }
}

fn wait_for_child(child: &mut std::process::Child, deadline: Instant, timeout: Duration) -> ExitStatus {
    loop {
        if let Some(status) = poll_child(child, deadline, timeout) {
            return status;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn poll_child(child: &mut std::process::Child, deadline: Instant, timeout: Duration) -> Option<ExitStatus> {
    match child.try_wait() {
        Ok(Some(status)) => Some(status),
        Ok(None) => pending_child(child, deadline, timeout),
        Err(error) => terminate_and_panic(child, &format!("failed while waiting for reporigor: {error}")),
    }
}

fn pending_child(
    child: &mut std::process::Child,
    deadline: Instant,
    timeout: Duration,
) -> Option<ExitStatus> {
    if Instant::now() < deadline {
        None
    } else {
        terminate_and_panic(
            child,
            &format!("reporigor exceeded the {timeout:?} subprocess timeout"),
        )
    }
}

fn capture_child_output(child: &mut std::process::Child) -> [Vec<u8>; 2] {
    let pipes: [(&str, Option<Box<dyn Read>>); 2] = [
        (
            "stdout",
            child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read>),
        ),
        (
            "stderr",
            child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read>),
        ),
    ];
    let mut captured = Vec::with_capacity(pipes.len());
    for (name, pipe) in pipes {
        let mut bytes = Vec::new();
        pipe.unwrap_or_else(|| panic!("reporigor {name} pipe was unexpectedly unavailable"))
            .read_to_end(&mut bytes)
            .unwrap_or_else(|error| panic!("failed to read reporigor {name}: {error}"));
        captured.push(bytes);
    }
    captured
        .try_into()
        .unwrap_or_else(|_| panic!("captured output count changed"))
}

fn terminate_and_panic(child: &mut std::process::Child, message: &str) -> ! {
    let _ = child.kill();
    let _ = child.wait();
    panic!("{message}")
}

fn assert_normal_exit(output: &CapturedOutput, language: &str, mode: &str) {
    assert!(
        output.status.code().is_some(),
        "{language}: {mode} subprocess terminated by a signal or platform exception"
    );
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CapturedOutput {
    fn as_output(&self) -> std::process::Output {
        std::process::Output {
            status: self.status,
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
        }
    }
}
