use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::sync::mpsc;

use reporigor_process_tree::{CleanupPolicy, ProcessTree, WaitReason};
#[cfg(unix)]
use reporigor_process_tree::{CleanupStage, PollResult};

fn fast_cleanup() -> CleanupPolicy {
    CleanupPolicy {
        graceful_timeout: Duration::from_millis(20),
        kill_timeout: Duration::from_secs(2),
        poll_interval: Duration::from_millis(5),
    }
}

#[cfg(unix)]
#[test]
fn ordinary_fast_exit_without_descendants_is_not_a_cleanup_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ProcessTree::spawn(&mut command)?;
    let outcome = child.wait_bounded(Duration::from_secs(2), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert!(outcome.status.success());
    assert!(outcome.termination.tree_confirmed_gone);
    Ok(())
}

#[cfg(unix)]
#[test]
fn observed_exit_does_not_pay_the_graceful_timeout() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut policy = fast_cleanup();
    policy.graceful_timeout = Duration::from_secs(3);
    let started = Instant::now();
    let mut child = ProcessTree::spawn(&mut command)?;
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
fn direct_exit_cleans_background_descendants_before_status_is_exposed(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("leaked-after-exit");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "(trap '' HUP TERM; sleep 0.35; printf leaked > leaked-after-exit) & exit 7",
        ])
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = ProcessTree::spawn(&mut command)?;
    let outcome = child.wait_bounded(Duration::from_secs(2), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert_eq!(outcome.status.code(), Some(7));
    assert!(outcome.termination.tree_confirmed_gone);

    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "background descendant escaped cleanup");
    Ok(())
}

#[cfg(unix)]
#[test]
fn timeout_is_bounded_and_reaps_the_leader() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("leaked-after-timeout");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "trap '' TERM; (trap '' TERM; sleep 0.35; printf leaked > leaked-after-timeout) & wait",
        ])
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let started = Instant::now();
    let mut child = ProcessTree::spawn(&mut command)?;
    let outcome = child.wait_bounded(Duration::from_millis(20), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::TimedOut);
    assert!(outcome.termination.status.is_some());
    assert!(outcome.termination.tree_confirmed_gone);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(matches!(
        child.poll_exit(fast_cleanup())?,
        PollResult::Exited(ref repeated) if repeated.reason == WaitReason::TimedOut
    ));

    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "timed-out descendant escaped cleanup");
    Ok(())
}

#[cfg(unix)]
#[test]
fn wait_slice_is_nonfinal_for_a_running_child() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "trap '' TERM; sleep 30"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = ProcessTree::spawn(&mut command)?;
    assert!(child.take_stdout().is_some());
    assert!(child.take_stderr().is_some());
    assert!(matches!(
        child.wait_slice(Duration::from_millis(15), fast_cleanup())?,
        PollResult::Running
    ));
    let report = child.terminate_bounded(fast_cleanup())?;
    assert!(report.status.is_some());
    assert!(report.tree_confirmed_gone);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cleanup_timeout_is_surfaced_and_a_later_call_can_reap() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "trap '' TERM; sleep 30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ProcessTree::spawn(&mut command)?;
    let zero_bound = CleanupPolicy {
        graceful_timeout: Duration::ZERO,
        kill_timeout: Duration::ZERO,
        poll_interval: Duration::ZERO,
    };
    let error = match child.terminate_bounded(zero_bound) {
        Ok(report) => return Err(format!("zero-bound cleanup unexpectedly succeeded: {report:?}").into()),
        Err(error) => error,
    };
    assert!(error
        .issues()
        .iter()
        .any(|issue| issue.stage() == CleanupStage::ReapLeader));

    let report = child.terminate_bounded(fast_cleanup())?;
    assert!(report.status.is_some());
    assert!(report.tree_confirmed_gone);
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_job_returns_an_ordinary_exit() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C", "exit 7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ProcessTree::spawn(&mut command)?;
    let outcome = child.wait_bounded(Duration::from_secs(5), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert_eq!(outcome.status.code(), Some(7));
    assert!(outcome.termination.tree_confirmed_gone);
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_job_timeout_is_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C", "ping -n 30 127.0.0.1 >NUL"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let started = Instant::now();
    let mut child = ProcessTree::spawn(&mut command)?;
    let outcome = child.wait_bounded(Duration::from_millis(25), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::TimedOut);
    assert!(outcome.termination.status.is_some());
    assert!(outcome.termination.tree_confirmed_gone);
    assert!(started.elapsed() < Duration::from_secs(3));
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_leader_exit_closes_descendant_pipe_and_prevents_marker() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("leaked-after-exit");
    let ready = directory.path().join("descendant-ready");
    let script = concat!(
        "start \"\" /B cmd.exe /D /S /C ",
        "\"echo ready>descendant-ready & ping -n 2 127.0.0.1 >NUL & ",
        "echo inherited-output & ",
        "echo leaked>leaked-after-exit\" & exit /B 7"
    );
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C", script])
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ProcessTree::spawn(&mut command)?;
    let stdout = child
        .take_stdout()
        .ok_or("contained child stdout was unavailable")?;
    let output_closed = spawn_pipe_reader(stdout);
    wait_for_path(&ready, Duration::from_secs(2))?;
    let outcome = child.wait_bounded(Duration::from_secs(5), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::Exited);
    assert_eq!(outcome.status.code(), Some(7));
    let _captured = output_closed.recv_timeout(Duration::from_secs(2))??;
    std::thread::sleep(Duration::from_millis(1_300));
    assert!(!marker.exists(), "Job Object descendant wrote after cleanup");
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_timeout_kills_background_descendant_before_marker() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("leaked-after-timeout");
    let ready = directory.path().join("descendant-ready");
    let script = concat!(
        "start \"\" /B cmd.exe /D /S /C ",
        "\"echo ready>descendant-ready & ping -n 2 127.0.0.1 >NUL & ",
        "echo leaked>leaked-after-timeout\" ",
        "& ping -n 30 127.0.0.1 >NUL"
    );
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C", script])
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ProcessTree::spawn(&mut command)?;
    wait_for_path(&ready, Duration::from_secs(2))?;
    let outcome = child.wait_bounded(Duration::from_millis(50), fast_cleanup())?;
    assert_eq!(outcome.reason, WaitReason::TimedOut);
    assert!(outcome.termination.tree_confirmed_gone);
    std::thread::sleep(Duration::from_millis(1_300));
    assert!(!marker.exists(), "Job Object descendant wrote after timeout");
    Ok(())
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
