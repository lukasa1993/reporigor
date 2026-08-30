use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::mpsc;

use reporigor_process_tree::{
    CleanupPolicy, ProcessTree, SpawnError, TerminationReport, WaitOutcome, WaitReason,
};
#[cfg(unix)]
use reporigor_process_tree::{CleanupStage, PollResult};

#[cfg(windows)]
const WINDOWS_HELPER_ROLE_ENV: &str = "REPORIGOR_PROCESS_TREE_TEST_ROLE";
#[cfg(windows)]
const WINDOWS_HELPER_READY_ENV: &str = "REPORIGOR_PROCESS_TREE_TEST_READY";
#[cfg(windows)]
const WINDOWS_HELPER_MARKER_ENV: &str = "REPORIGOR_PROCESS_TREE_TEST_MARKER";
#[cfg(windows)]
const WINDOWS_HELPER_TEST: &str = "windows_job_helper";
#[cfg(unix)]
const DIRECT_EXIT_WAIT: Duration = Duration::from_secs(2);

fn fast_cleanup() -> CleanupPolicy {
    CleanupPolicy::new(
        Duration::from_millis(20),
        Duration::from_secs(2),
        Duration::from_millis(5),
    )
}

fn silence(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

#[cfg(unix)]
fn unix_shell(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    silence(&mut command);
    command
}

#[cfg(unix)]
fn marker_fixture(name: &str) -> Result<(tempfile::TempDir, std::path::PathBuf), std::io::Error> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join(name);
    Ok((directory, marker))
}

#[cfg(unix)]
fn spawn_shell(script: &str) -> Result<ProcessTree, SpawnError> {
    ProcessTree::spawn(&mut unix_shell(script))
}

#[cfg(unix)]
fn stubborn_shell() -> Result<ProcessTree, SpawnError> {
    spawn_shell("trap '' TERM; sleep 30")
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum MarkerScenario {
    DirectExit,
    Timeout,
}

#[cfg(unix)]
fn marker_shell(
    scenario: MarkerScenario,
) -> Result<(tempfile::TempDir, std::path::PathBuf, Command), std::io::Error> {
    let (name, script) = match scenario {
        MarkerScenario::DirectExit => (
            "leaked-after-exit",
            "(trap '' HUP TERM; sleep 0.35; printf leaked > leaked-after-exit) & exit 7",
        ),
        MarkerScenario::Timeout => (
            "leaked-after-timeout",
            "trap '' TERM; (trap '' TERM; sleep 0.35; printf leaked > leaked-after-timeout) & wait",
        ),
    };
    let (directory, marker) = marker_fixture(name)?;
    let mut command = unix_shell(script);
    command.current_dir(directory.path());
    Ok((directory, marker, command))
}

fn assert_terminated(report: &TerminationReport) {
    assert!(report.status.is_some());
    assert!(report.tree_confirmed_gone);
}

fn assert_exited(outcome: &WaitOutcome, code: i32) {
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert_eq!(outcome.status.code(), Some(code));
    assert!(outcome.termination.tree_confirmed_gone);
}

fn assert_timed_out(outcome: &WaitOutcome, started: Instant) {
    assert_eq!(outcome.reason, WaitReason::TimedOut);
    assert!(outcome.termination.status.is_some());
    assert!(outcome.termination.tree_confirmed_gone);
    assert!(started.elapsed() < Duration::from_secs(3));
}

fn terminate_successfully(child: &mut ProcessTree) -> Result<(), Box<dyn std::error::Error>> {
    let report = child.terminate_bounded(fast_cleanup())?;
    assert_terminated(&report);
    Ok(())
}

fn finish_marker_scenario(
    marker: &std::path::Path,
    delay: Duration,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    std::thread::sleep(delay);
    if marker.exists() {
        return Err(message.to_owned().into());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn ordinary_fast_exit_without_descendants_is_not_a_cleanup_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut child = spawn_shell("exit 0")?;
    let outcome = child.wait_bounded(Duration::from_secs(2), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert!(outcome.status.success());
    assert!(outcome.termination.tree_confirmed_gone);
    Ok(())
}

#[cfg(unix)]
#[test]
fn observed_exit_does_not_pay_the_graceful_timeout() -> Result<(), Box<dyn std::error::Error>> {
    let mut policy = fast_cleanup();
    policy.graceful_timeout = Duration::from_secs(3);
    let started = Instant::now();
    let mut child = spawn_shell("exit 0")?;
    let outcome = child.wait_bounded(Duration::from_secs(2), policy)?;
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert!(
        started.elapsed() < Duration::from_millis(750),
        "observed leader exit paid the three-second graceful timeout"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn exit_and_timeout_both_clean_background_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_marker_scenario(MarkerScenario::DirectExit)?;
    assert_marker_scenario(MarkerScenario::Timeout)
}

#[cfg(unix)]
fn assert_marker_scenario(scenario: MarkerScenario) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = marker_shell(scenario)?;
    let (_directory, marker, mut command) = fixture;
    let started = Instant::now();
    let mut child = ProcessTree::spawn(&mut command)?;
    let outcome = wait_for_marker_scenario(&mut child, scenario)?;
    assert_marker_outcome(scenario, &mut child, &outcome, started)?;
    finish_marker_scenario(
        &marker,
        Duration::from_millis(500),
        "background descendant escaped cleanup",
    )
}

#[cfg(unix)]
fn wait_for_marker_scenario(
    child: &mut ProcessTree,
    scenario: MarkerScenario,
) -> Result<WaitOutcome, Box<dyn std::error::Error>> {
    match scenario {
        MarkerScenario::DirectExit => child
            .wait_bounded(DIRECT_EXIT_WAIT, fast_cleanup())
            .map_err(Into::into),
        MarkerScenario::Timeout => child
            .wait_bounded(Duration::from_millis(20), fast_cleanup())
            .map_err(Into::into),
    }
}

#[cfg(unix)]
fn assert_marker_outcome(
    scenario: MarkerScenario,
    child: &mut ProcessTree,
    outcome: &WaitOutcome,
    started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    match scenario {
        MarkerScenario::DirectExit => assert_exited(outcome, 7),
        MarkerScenario::Timeout => {
            assert_timed_out(outcome, started);
            assert!(matches!(
                child.poll_exit(fast_cleanup())?,
                PollResult::Exited(ref repeated) if repeated.reason == WaitReason::TimedOut
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn wait_slice_is_nonfinal_for_a_running_child() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = unix_shell("trap '' TERM; sleep 30");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = ProcessTree::spawn(&mut command)?;
    assert!(child.take_stdout().is_some());
    assert!(child.take_stderr().is_some());
    assert!(matches!(
        child.wait_slice(Duration::from_millis(15), fast_cleanup())?,
        PollResult::Running
    ));
    terminate_successfully(&mut child)
}

#[cfg(unix)]
#[test]
fn cleanup_timeout_is_surfaced_and_a_later_call_can_reap() -> Result<(), Box<dyn std::error::Error>> {
    let mut child = stubborn_shell()?;
    let zero_bound = CleanupPolicy::new(Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let error = match child.terminate_bounded(zero_bound) {
        Ok(report) => return Err(format!("zero-bound cleanup unexpectedly succeeded: {report:?}").into()),
        Err(error) => error,
    };
    assert!(error
        .issues()
        .iter()
        .any(|issue| issue.stage() == CleanupStage::ReapLeader));

    terminate_successfully(&mut child)
}

#[cfg(windows)]
fn windows_child(script: &str) -> Result<ProcessTree, SpawnError> {
    let mut command = Command::new("cmd.exe");
    command.args(["/D", "/S", "/C", script]);
    silence(&mut command);
    ProcessTree::spawn(&mut command)
}

#[cfg(windows)]
#[test]
fn windows_job_returns_an_ordinary_exit() -> Result<(), Box<dyn std::error::Error>> {
    let mut child = windows_child("exit 7")?;
    wait_for_exit(&mut child, Duration::from_secs(5), 7)?;
    Ok(())
}

#[cfg(windows)]
fn wait_for_exit(
    child: &mut ProcessTree,
    timeout: Duration,
    code: i32,
) -> Result<WaitOutcome, reporigor_process_tree::WaitError> {
    let outcome = child.wait_bounded(timeout, fast_cleanup())?;
    assert_exited(&outcome, code);
    Ok(outcome)
}

#[cfg(windows)]
#[test]
fn windows_job_timeout_is_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut child = windows_child("ping -n 30 127.0.0.1 >NUL")?;
    let outcome = child.wait_bounded(Duration::from_millis(25), fast_cleanup())?;
    assert_timed_out(&outcome, started);
    Ok(())
}

#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowsMarkerScenario {
    Exit,
    Timeout,
}

#[cfg(windows)]
#[test]
fn windows_job_cleans_descendants_after_exit_and_timeout() -> Result<(), Box<dyn std::error::Error>> {
    assert_windows_marker_scenario(WindowsMarkerScenario::Exit)?;
    assert_windows_marker_scenario(WindowsMarkerScenario::Timeout)
}

#[cfg(windows)]
fn assert_windows_marker_scenario(scenario: WindowsMarkerScenario) -> Result<(), Box<dyn std::error::Error>> {
    let (role, marker_name) = match scenario {
        WindowsMarkerScenario::Exit => ("exit-leader", "leaked-after-exit"),
        WindowsMarkerScenario::Timeout => ("timeout-leader", "leaked-after-timeout"),
    };
    let (_directory, marker, ready, mut command) = windows_marker_fixture(role, marker_name)?;
    if scenario == WindowsMarkerScenario::Exit {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    } else {
        silence(&mut command);
    }
    let mut child = ProcessTree::spawn(&mut command)?;
    let output_closed = child.take_stdout().map(spawn_pipe_reader);
    wait_for_path(&ready, Duration::from_secs(2))?;
    match scenario {
        WindowsMarkerScenario::Exit => {
            wait_for_exit(&mut child, Duration::from_secs(5), 7)?;
            let receiver = output_closed.ok_or("contained child stdout was unavailable")?;
            let _captured = receiver.recv_timeout(Duration::from_secs(2))??;
        }
        WindowsMarkerScenario::Timeout => {
            let outcome = child.wait_bounded(Duration::from_millis(50), fast_cleanup())?;
            assert_eq!(outcome.reason, WaitReason::TimedOut);
            assert!(outcome.termination.tree_confirmed_gone);
        }
    }
    finish_marker_scenario(
        &marker,
        Duration::from_millis(1_300),
        "Job Object descendant wrote after cleanup",
    )
}

#[cfg(windows)]
fn windows_marker_fixture(
    role: &str,
    marker_name: &str,
) -> std::io::Result<(tempfile::TempDir, PathBuf, PathBuf, Command)> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join(marker_name);
    let ready = directory.path().join("descendant-ready");
    let command = windows_helper_command(role, &ready, &marker)?;
    Ok((directory, marker, ready, command))
}

#[cfg(windows)]
#[test]
fn windows_job_helper() -> Result<(), Box<dyn std::error::Error>> {
    let Some(role) = std::env::var_os(WINDOWS_HELPER_ROLE_ENV) else {
        return Ok(());
    };
    let ready = required_helper_path(WINDOWS_HELPER_READY_ENV)?;
    let marker = required_helper_path(WINDOWS_HELPER_MARKER_ENV)?;
    match role.to_string_lossy().as_ref() {
        "descendant" => {
            std::fs::write(&ready, b"ready")?;
            std::thread::sleep(Duration::from_millis(750));
            std::fs::write(marker, b"leaked")?;
        }
        role @ ("exit-leader" | "timeout-leader") => {
            let mut descendant = windows_helper_command("descendant", &ready, &marker)?;
            let _descendant = descendant
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::null())
                .spawn()?;
            wait_for_path(&ready, Duration::from_secs(5))?;
            if role == "exit-leader" {
                std::process::exit(7);
            }
            std::thread::sleep(Duration::from_secs(30));
        }
        role => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unknown Windows Job Object helper role: {role}"),
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_helper_command(role: &str, ready: &Path, marker: &Path) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", WINDOWS_HELPER_TEST, "--nocapture"])
        .env(WINDOWS_HELPER_ROLE_ENV, role)
        .env(WINDOWS_HELPER_READY_ENV, ready)
        .env(WINDOWS_HELPER_MARKER_ENV, marker);
    Ok(command)
}

#[cfg(windows)]
fn required_helper_path(variable: &str) -> std::io::Result<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Windows Job Object helper is missing {variable}"),
        )
    })
}

#[cfg(windows)]
fn spawn_pipe_reader(mut pipe: std::process::ChildStdout) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes).map(|_count| bytes);
        let _ignored = sender.send(result);
    });
    receiver
}

#[cfg(windows)]
fn wait_for_path(path: &std::path::Path, timeout: Duration) -> std::io::Result<()> {
    let started = Instant::now();
    while !path.is_file() {
        if started.elapsed() >= timeout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} was not created by the Job Object descendant", path.display()),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
