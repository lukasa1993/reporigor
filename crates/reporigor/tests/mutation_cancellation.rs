#![cfg(unix)]

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use analysis_mutate::{ACTIVE_JOURNAL, STATE_DIRECTORY, STATE_DIRECTORY_ENV};

const ORIGINAL_SOURCE: &str =
    "def classify(value: int) -> int:\n    if value > 0:\n        return 1\n    return 0\n";
const MUTATED_SOURCE: &str =
    "def classify(value: int) -> int:\n    if value <= 0:\n        return 1\n    return 0\n";
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn sigint_cancels_mutation_and_restores_source() -> Result<(), Box<dyn std::error::Error>> {
    assert_mutation_signal("INT")
}

#[test]
fn sigterm_cancels_mutation_and_restores_source() -> Result<(), Box<dyn std::error::Error>> {
    assert_mutation_signal("TERM")
}

#[test]
fn crash_requires_explicit_recovery_before_read_only_analysis() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let project = workspace.path().join("project");
    let state_parent = workspace.path().join("state");
    fs::create_dir_all(project.join("src"))?;
    fs::create_dir(&state_parent)?;
    let source = project.join("src/main.py");
    fs::write(&source, ORIGINAL_SOURCE)?;

    let test_command = "grep -q '^    if value <= 0:$' src/main.py || exit 91; \
         printf '%s\n' \"$$\" > command-group.pid; printf 'ready\n' > mutation-ready; sleep 30";
    let child = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args([
            "--backend",
            "generic",
            "--language",
            "python",
            "mutate",
            "--run",
            "--skip-baseline",
            "--no-validate",
            "--max-mutants",
            "1",
            "--timeout",
            "30",
            "--test-command",
            test_command,
        ])
        .arg(&project)
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child, project.join("command-group.pid"));
    wait_for_file_contents(
        &mut child,
        &project.join("mutation-ready"),
        b"ready\n",
        READY_TIMEOUT,
    )?;
    assert_eq!(fs::read_to_string(&source)?, MUTATED_SOURCE);
    assert_eq!(active_journals(&state_parent)?.len(), 1);

    send_signal("KILL", child.id())?;
    let status = child.wait_bounded(EXIT_TIMEOUT)?;
    assert!(!status.success());

    for (label, arguments) in [
        ("crap", vec!["crap"]),
        ("dry", vec!["dry"]),
        ("providers", vec!["providers"]),
        ("mutate-list", vec!["mutate", "--list"]),
        ("check", vec!["check"]),
    ] {
        assert_pending_refusal(label, &arguments, &project, &state_parent)?;
    }
    assert_eq!(fs::read_to_string(&source)?, MUTATED_SOURCE);
    assert_eq!(active_journals(&state_parent)?.len(), 1);

    let recovery = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(["--format", "json", "mutate", "--recover"])
        .arg(&project)
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .output()?;
    assert_eq!(
        recovery.status.code(),
        Some(0),
        "recovery failed: {}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(String::from_utf8(recovery.stdout)?.contains("\"recovery\": \"restored\""));
    assert_eq!(fs::read_to_string(&source)?, ORIGINAL_SOURCE);
    assert!(active_journals(&state_parent)?.is_empty());
    Ok(())
}

fn assert_pending_refusal(
    label: &str,
    arguments: &[&str],
    project: &Path,
    state_parent: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(["--backend", "generic", "--language", "python"])
        .args(arguments)
        .arg(project)
        .env(STATE_DIRECTORY_ENV, state_parent)
        .output()?;
    let stderr = String::from_utf8(output.stderr)?;
    assert_eq!(
        output.status.code(),
        Some(1),
        "{label} did not refuse pending recovery: {stderr}"
    );
    assert!(
        stderr.contains("reporigor mutate --recover"),
        "{label} refusal was not actionable: {stderr}"
    );
    Ok(())
}

#[test]
fn nonmutation_command_cancels_and_kills_its_child_tree() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let project = workspace.path().join("project");
    fs::create_dir(&project)?;
    fs::write(project.join("sample.py"), ORIGINAL_SOURCE)?;

    let command = "printf '%s\n' \"$$\" > command-group.pid; sleep 30 & descendant=$!; \
         printf '%s\n' \"$descendant\" > descendant.pid; printf 'ready\n' > coverage-ready; \
         wait \"$descendant\"";
    let child = Command::new(env!("CARGO_BIN_EXE_crap4python"))
        .args(["--root"])
        .arg(&project)
        .args(["--test-command", command, "--timeout", "30"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child, project.join("command-group.pid"));

    wait_for_file_contents(
        &mut child,
        &project.join("coverage-ready"),
        b"ready\n",
        READY_TIMEOUT,
    )?;
    let group = read_pid(&project.join("command-group.pid"))?;
    let descendant = read_pid(&project.join("descendant.pid"))?;
    assert!(process_exists(group), "coverage command group was not alive");
    assert!(process_exists(descendant), "coverage descendant was not alive");

    send_signal("INT", child.id())?;
    let status = child.wait_bounded(EXIT_TIMEOUT)?;
    let (stdout, stderr) = child.read_output()?;
    assert_eq!(
        status.code(),
        Some(1),
        "non-mutation cancellation did not return an operational exit\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("mutation run cancelled"),
        "non-mutation cancellation was not explained: {}",
        String::from_utf8_lossy(&stderr)
    );
    wait_for_process_exit(group, Duration::from_secs(2))?;
    wait_for_process_exit(descendant, Duration::from_secs(2))?;
    Ok(())
}

#[test]
fn legacy_coverage_refuses_a_crash_journal_before_running_commands() -> Result<(), Box<dyn std::error::Error>>
{
    let workspace = tempfile::tempdir()?;
    let state_workspace = tempfile::tempdir()?;
    let project = workspace.path().join("project");
    let state_parent = state_workspace.path().join("state");
    fs::create_dir_all(project.join("src"))?;
    fs::create_dir(&state_parent)?;
    let source = project.join("src/main.py");
    fs::write(&source, ORIGINAL_SOURCE)?;

    let test_command = "grep -q '^    if value <= 0:$' src/main.py || exit 91; \
         printf '%s\n' \"$$\" > command-group.pid; printf 'ready\n' > mutation-ready; exec sleep 30";
    let child = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args([
            "--backend",
            "generic",
            "--language",
            "python",
            "mutate",
            "--run",
            "--skip-baseline",
            "--no-validate",
            "--max-mutants",
            "1",
            "--timeout",
            "30",
            "--test-command",
            test_command,
        ])
        .arg(&project)
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child, project.join("command-group.pid"));
    wait_for_file_contents(
        &mut child,
        &project.join("mutation-ready"),
        b"ready\n",
        READY_TIMEOUT,
    )?;
    let command_group = read_pid(&project.join("command-group.pid"))?;
    assert_eq!(fs::read_to_string(&source)?, MUTATED_SOURCE);
    assert_eq!(active_journals(&state_parent)?.len(), 1);

    send_signal("KILL", child.id())?;
    let crash_status = child.wait_bounded(EXIT_TIMEOUT)?;
    assert!(
        crash_status.code().is_none(),
        "forced crash unexpectedly returned {crash_status}"
    );
    send_signal("KILL", command_group)?;
    wait_for_process_exit(command_group, Duration::from_secs(2))?;
    let _ = child.read_output()?;

    // Use the parent root to prove the global pointer protects overlapping
    // invocations, not only an exact canonical-root match.
    let marker = workspace.path().join("legacy-command-ran");
    let legacy = Command::new(env!("CARGO_BIN_EXE_crap4python"))
        .args(["--root"])
        .arg(workspace.path())
        .args([
            "--test-command",
            "printf ran > legacy-command-ran",
            "--timeout",
            "5",
        ])
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .output()?;
    assert_eq!(
        legacy.status.code(),
        Some(1),
        "legacy CRAP did not refuse the pending mutant\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&legacy.stdout),
        String::from_utf8_lossy(&legacy.stderr)
    );
    assert!(
        !marker.exists(),
        "legacy coverage command ran against a source left mutated by a crash"
    );
    let legacy_stderr = String::from_utf8_lossy(&legacy.stderr);
    assert!(
        legacy_stderr.contains("pending mutation") && legacy_stderr.contains("--recover"),
        "pending-journal refusal was not actionable: {legacy_stderr}"
    );

    let recovery = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(["mutate", "--recover"])
        .arg(&project)
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .output()?;
    assert_eq!(
        recovery.status.code(),
        Some(0),
        "explicit recovery failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&recovery.stdout),
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(fs::read_to_string(&source)?, ORIGINAL_SOURCE);
    assert!(active_journals(&state_parent)?.is_empty());
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn assert_mutation_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let project = workspace.path().join("project");
    let state_parent = workspace.path().join("state");
    fs::create_dir_all(project.join("src"))?;
    fs::create_dir(&state_parent)?;
    let source = project.join("src/main.py");
    fs::write(&source, ORIGINAL_SOURCE)?;

    let test_command = "grep -q '^    if value <= 0:$' src/main.py || exit 91; \
         printf '%s\n' \"$$\" > command-group.pid; sleep 30 & descendant=$!; \
         printf '%s\n' \"$descendant\" > descendant.pid; kill -0 \"$descendant\" || exit 92; \
         printf 'ready\n' > mutation-ready; wait \"$descendant\"";
    let child = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args([
            "--backend",
            "generic",
            "--language",
            "python",
            "--format",
            "json",
            "mutate",
            "--run",
            "--skip-baseline",
            "--no-validate",
            "--max-mutants",
            "1",
            "--timeout",
            "30",
            "--test-command",
            test_command,
        ])
        .arg(&project)
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child, project.join("command-group.pid"));

    wait_for_file_contents(
        &mut child,
        &project.join("mutation-ready"),
        b"ready\n",
        READY_TIMEOUT,
    )?;
    assert_eq!(fs::read_to_string(&source)?, MUTATED_SOURCE);
    assert_eq!(active_journals(&state_parent)?.len(), 1);
    let group = read_pid(&project.join("command-group.pid"))?;
    let descendant = read_pid(&project.join("descendant.pid"))?;
    assert!(process_exists(group), "mutation command group was not alive");
    assert!(process_exists(descendant), "mutation descendant was not alive");

    let cancelled_at = Instant::now();
    send_signal(signal, child.id())?;
    let status = child.wait_bounded(EXIT_TIMEOUT)?;
    let (stdout, stderr) = child.read_output()?;
    assert!(
        cancelled_at.elapsed() < EXIT_TIMEOUT,
        "mutation cancellation was not prompt"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "cancelled mutation did not return an operational exit\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("mutation run cancelled"),
        "cancelled mutation was not explained: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(fs::read_to_string(&source)?, ORIGINAL_SOURCE);
    assert!(active_journals(&state_parent)?.is_empty());
    wait_for_process_exit(group, Duration::from_secs(2))?;
    wait_for_process_exit(descendant, Duration::from_secs(2))?;

    let follow_up = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args([
            "--backend",
            "generic",
            "--language",
            "python",
            "mutate",
            "--run",
            "--skip-baseline",
            "--no-validate",
            "--max-mutants",
            "1",
            "--timeout",
            "5",
            "--test-command",
            "true",
            "--allow-survivors",
        ])
        .arg(&project)
        .env(STATE_DIRECTORY_ENV, &state_parent)
        .output()?;
    assert_eq!(
        follow_up.status.code(),
        Some(0),
        "state was not reusable after cancellation\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&follow_up.stdout),
        String::from_utf8_lossy(&follow_up.stderr)
    );
    assert_eq!(fs::read_to_string(&source)?, ORIGINAL_SOURCE);
    assert!(active_journals(&state_parent)?.is_empty());
    Ok(())
}

#[derive(Debug)]
struct ChildGuard {
    child: Child,
    command_group_file: PathBuf,
    reaped: bool,
}

impl ChildGuard {
    fn new(child: Child, command_group_file: PathBuf) -> Self {
        Self {
            child,
            command_group_file,
            reaped: false,
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }

    fn wait_bounded(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("reporigor did not exit within {timeout:?}"),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_output(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            pipe.read_to_end(&mut stdout)?;
        }
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_end(&mut stderr)?;
        }
        Ok((stdout, stderr))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(group) = read_pid(&self.command_group_file) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &format!("-{group}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_for_file_contents(
    child: &mut ChildGuard,
    path: &Path,
    expected: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read(path) {
            Ok(contents) if contents == expected => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "reporigor exited before readiness marker with {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("readiness marker {} was not written", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn send_signal(signal: &str, pid: u32) -> io::Result<()> {
    let status = Command::new("/bin/kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "/bin/kill -{signal} {pid} exited with {status}"
        )))
    }
}

fn process_exists(pid: u32) -> bool {
    let signallable = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !signallable {
        return false;
    }

    // A killed child can remain as a zombie under a container's minimal PID 1.
    // `kill -0` still succeeds for that PID even though no code can run. Both
    // BSD and procps `ps` expose `Z` as the first process-state character.
    match Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z'),
        Ok(_) => false,
        Err(_) => true,
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while process_exists(pid) {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process {pid} survived cancellation"),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn read_pid(path: &Path) -> io::Result<u32> {
    fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn active_journals(state_parent: &Path) -> io::Result<Vec<PathBuf>> {
    let base = state_parent.join(STATE_DIRECTORY);
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut journals = Vec::new();
    for entry in fs::read_dir(base)? {
        let path = entry?.path();
        if path.is_dir() {
            let journal = path.join(ACTIVE_JOURNAL);
            if journal.is_file() {
                journals.push(journal);
            }
        }
    }
    journals.sort();
    Ok(journals)
}
