#![cfg(unix)]

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use analysis_mutate::{ACTIVE_JOURNAL, STATE_DIRECTORY, STATE_DIRECTORY_ENV};

pub mod support;
use support::fixtures::write_fixture;
use support::invocation::spawn_piped;
use support::success_assertion::assert_success;

const ORIGINAL_SOURCE: &str =
    "def classify(value: int) -> int:\n    if value > 0:\n        return 1\n    return 0\n";
const MUTATED_SOURCE: &str =
    "def classify(value: int) -> int:\n    if value <= 0:\n        return 1\n    return 0\n";
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

struct Deadline(Instant);

impl Deadline {
    fn after(timeout: Duration) -> Self {
        Self(Instant::now() + timeout)
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.0
    }

    fn pause() {
        thread::sleep(POLL_INTERVAL);
    }

    fn poll(&self, timeout_message: String) -> io::Result<()> {
        if self.expired() {
            Err(timeout_error(timeout_message))
        } else {
            Self::pause();
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
struct MutationLaunch<'a> {
    test_command: &'a str,
    json_output: bool,
    isolated_state: bool,
}

struct RunningMutation {
    workspace: tempfile::TempDir,
    _state_workspace: Option<tempfile::TempDir>,
    project: PathBuf,
    state_parent: PathBuf,
    source: PathBuf,
    child: ChildGuard,
}

fn mutation_command(project: &Path, state: &Path, test_command: &str, timeout: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporigor"));
    command
        .args(
            "--backend|generic|--language|python|mutate|--run|--skip-baseline|--no-validate|--max-mutants|1"
                .split('|'),
        )
        .args(["--timeout", timeout, "--test-command", test_command])
        .arg(project)
        .env(STATE_DIRECTORY_ENV, state);
    command
}

fn spawn_guard(command: Command, command_group_file: PathBuf) -> io::Result<ChildGuard> {
    let child = spawn_piped(command)?;
    Ok(ChildGuard::new(child, command_group_file))
}

fn run_recovery(run: &RunningMutation, json: bool) -> io::Result<std::process::Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporigor"));
    if json {
        command.args(["--format", "json"]);
    }
    command
        .args(["mutate", "--recover"])
        .arg(&run.project)
        .env(STATE_DIRECTORY_ENV, &run.state_parent)
        .output()
}

#[derive(Clone, Copy)]
enum ExpectedSourceState {
    Mutated,
    Restored,
}

fn start_running_mutation(launch: MutationLaunch<'_>) -> Result<RunningMutation, Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let state_workspace = launch.isolated_state.then(tempfile::tempdir).transpose()?;
    let project = workspace.path().join("project");
    let state_parent = state_workspace.as_ref().map_or_else(
        || workspace.path().join("state"),
        |state| state.path().join("state"),
    );
    fs::create_dir(&state_parent)?;
    let source = project.join("src/main.py");
    write_fixture(&source, ORIGINAL_SOURCE);

    let mut command = mutation_command(&project, &state_parent, launch.test_command, "30");
    if launch.json_output {
        command.arg("--format=json");
    }
    let child = spawn_guard(command, project.join("command-group.pid"))?;
    Ok(RunningMutation {
        workspace,
        _state_workspace: state_workspace,
        project,
        state_parent,
        source,
        child,
    })
}

fn await_mutated(run: &mut RunningMutation) -> Result<(), Box<dyn std::error::Error>> {
    await_marker(&mut run.child, &run.project, "mutation-ready")?;
    assert_source_state(run, ExpectedSourceState::Mutated)?;
    Ok(())
}

fn assert_source_state(
    run: &RunningMutation,
    expected: ExpectedSourceState,
) -> Result<(), Box<dyn std::error::Error>> {
    let (expected_source, expected_journals) = match expected {
        ExpectedSourceState::Mutated => (MUTATED_SOURCE, 1),
        ExpectedSourceState::Restored => (ORIGINAL_SOURCE, 0),
    };
    assert_eq!(fs::read_to_string(&run.source)?, expected_source);
    assert_eq!(active_journals(&run.state_parent)?.len(), expected_journals);
    Ok(())
}

fn ready_mutation(launch: MutationLaunch<'_>) -> Result<RunningMutation, Box<dyn std::error::Error>> {
    let mut run = start_running_mutation(launch)?;
    await_mutated(&mut run)?;
    Ok(run)
}

fn command_processes(project: &Path) -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let group = read_pid(&project.join("command-group.pid"))?;
    let descendant = read_pid(&project.join("descendant.pid"))?;
    assert!(process_exists(group), "command group was not alive");
    assert!(process_exists(descendant), "command descendant was not alive");
    Ok((group, descendant))
}

fn assert_cancelled_output(status: ExitStatus, stdout: &[u8], stderr: &[u8], context: &str) {
    assert_eq!(
        status.code(),
        Some(1),
        "{context} did not return an operational exit\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    assert!(
        String::from_utf8_lossy(stderr).contains("mutation run cancelled"),
        "{context} was not explained: {}",
        String::from_utf8_lossy(stderr)
    );
}

struct RunningCoverage {
    _workspace: tempfile::TempDir,
    project: PathBuf,
    child: ChildGuard,
}

fn start_running_coverage() -> Result<RunningCoverage, Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let project = workspace.path().join("project");
    write_fixture(&project.join("sample.py"), ORIGINAL_SOURCE);
    let command = "printf '%s\n' \"$$\" > command-group.pid; sleep 30 & descendant=$!; \
         printf '%s\n' \"$descendant\" > descendant.pid; printf 'ready\n' > coverage-ready; \
         wait \"$descendant\"";
    let mut process = Command::new(env!("CARGO_BIN_EXE_crap4python"));
    process
        .arg("--root")
        .arg(&project)
        .args(["--test-command", command, "--timeout", "30"]);
    let child = spawn_guard(process, project.join("command-group.pid"))?;
    Ok(RunningCoverage {
        _workspace: workspace,
        project,
        child,
    })
}

type CapturedExit = (ExitStatus, Vec<u8>, Vec<u8>);

fn cancel_child(child: &mut ChildGuard, signal: &str) -> Result<CapturedExit, Box<dyn std::error::Error>> {
    send_signal(signal, child.id())?;
    let status = child.wait_bounded(EXIT_TIMEOUT)?;
    let (stdout, stderr) = child.read_output()?;
    Ok((status, stdout, stderr))
}

fn kill_child(child: &mut ChildGuard) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    send_signal("KILL", child.id())?;
    Ok(child.wait_bounded(EXIT_TIMEOUT)?)
}

fn crashing_test_command(final_command: &str) -> String {
    format!(
        "grep -q '^    if value <= 0:$' src/main.py || exit 91; printf '%s\\n' \"$$\" > command-group.pid; printf 'ready\\n' > mutation-ready; {final_command}"
    )
}

fn crash_fixture(
    final_command: &str,
    isolated_state: bool,
) -> Result<(RunningMutation, ExitStatus), Box<dyn std::error::Error>> {
    let test_command = crashing_test_command(final_command);
    let mut run = ready_mutation(MutationLaunch {
        test_command: &test_command,
        json_output: false,
        isolated_state,
    })?;
    let status = kill_child(&mut run.child)?;
    Ok((run, status))
}

fn assert_recovery_success(output: &std::process::Output, context: &str) {
    assert_success(output, context);
}

fn await_marker(child: &mut ChildGuard, project: &Path, name: &str) -> io::Result<()> {
    wait_for_file_contents(child, &project.join(name), b"ready\n", READY_TIMEOUT)
}

fn wait_for_processes(processes: [u32; 2]) -> io::Result<()> {
    for process in processes {
        wait_for_process_exit(process, Duration::from_secs(2))?;
    }
    Ok(())
}

#[test]
fn termination_signals_cancel_mutation_and_restore_source() -> Result<(), Box<dyn std::error::Error>> {
    for signal in ["INT", "TERM"] {
        assert_mutation_signal(signal)?;
    }
    Ok(())
}

#[test]
fn crash_requires_explicit_recovery_before_read_only_analysis() -> Result<(), Box<dyn std::error::Error>> {
    let (run, status) = crash_fixture("sleep 30", false)?;
    assert!(!status.success());
    assert_all_pending_refusals(&run)?;
    assert_source_state(&run, ExpectedSourceState::Mutated)?;
    recover_crashed_mutation(&run)
}

fn assert_all_pending_refusals(run: &RunningMutation) -> Result<(), Box<dyn std::error::Error>> {
    for (label, arguments) in [
        ("crap", vec!["crap"]),
        ("dry", vec!["dry"]),
        ("providers", vec!["providers"]),
        ("mutate-list", vec!["mutate", "--list"]),
        ("check", vec!["check"]),
    ] {
        assert_pending_refusal(label, &arguments, &run.project, &run.state_parent)?;
    }
    Ok(())
}

fn recover_crashed_mutation(run: &RunningMutation) -> Result<(), Box<dyn std::error::Error>> {
    let recovery = run_recovery(run, true)?;
    assert_recovery_success(&recovery, "recovery failed");
    assert!(String::from_utf8(recovery.stdout)?.contains("\"recovery\": \"restored\""));
    assert_source_state(run, ExpectedSourceState::Restored)?;
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
    let mut run = start_running_coverage()?;
    await_marker(&mut run.child, &run.project, "coverage-ready")?;
    let (group, descendant) = command_processes(&run.project)?;
    let (status, stdout, stderr) = cancel_child(&mut run.child, "INT")?;
    assert_cancelled_output(status, &stdout, &stderr, "non-mutation cancellation");
    wait_for_processes([group, descendant])?;
    Ok(())
}

#[test]
fn legacy_coverage_refuses_a_crash_journal_before_running_commands() -> Result<(), Box<dyn std::error::Error>>
{
    let run = prepare_legacy_crash_fixture()?;
    // Use the parent root to prove the global pointer protects overlapping
    // invocations, not only an exact canonical-root match.
    let (marker, legacy) = run_pending_legacy_coverage(&run)?;
    assert_legacy_pending_refusal(&legacy, &marker);
    let recovery = run_recovery(&run, false)?;
    assert_recovery_success(&recovery, "explicit recovery failed");
    assert_source_state(&run, ExpectedSourceState::Restored)?;
    Ok(())
}

fn prepare_legacy_crash_fixture() -> Result<RunningMutation, Box<dyn std::error::Error>> {
    let (mut run, crash_status) = crash_fixture("exec sleep 30", true)?;
    let command_group = read_pid(&run.project.join("command-group.pid"))?;
    assert!(
        crash_status.code().is_none(),
        "forced crash unexpectedly returned {crash_status}"
    );
    send_signal("KILL", command_group)?;
    wait_for_process_exit(command_group, Duration::from_secs(2))?;
    let _ = run.child.read_output()?;
    Ok(run)
}

fn assert_legacy_pending_refusal(legacy: &std::process::Output, marker: &Path) {
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
}

fn run_pending_legacy_coverage(run: &RunningMutation) -> io::Result<(PathBuf, std::process::Output)> {
    let marker = run.workspace.path().join("legacy-command-ran");
    let output = Command::new(env!("CARGO_BIN_EXE_crap4python"))
        .args(["--root"])
        .arg(run.workspace.path())
        .args([
            "--test-command",
            "printf ran > legacy-command-ran",
            "--timeout",
            "5",
        ])
        .env(STATE_DIRECTORY_ENV, &run.state_parent)
        .output()?;
    Ok((marker, output))
}

fn assert_mutation_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let test_command = mutation_signal_command();
    let mut run = ready_mutation(MutationLaunch {
        test_command: &test_command,
        json_output: true,
        isolated_state: false,
    })?;
    let processes = command_processes(&run.project)?;
    assert_cancelled_mutation(&mut run, signal, processes)?;
    assert_mutation_follow_up(&run)
}

fn mutation_signal_command() -> String {
    [
        "grep -q '^    if value <= 0:$' src/main.py || exit 91",
        "printf '%s\\n' \"$$\" > command-group.pid",
        "sleep 30 & descendant=$!",
        "printf '%s\\n' \"$descendant\" > descendant.pid",
        "kill -0 \"$descendant\" || exit 92",
        "printf 'ready\\n' > mutation-ready",
        "wait \"$descendant\"",
    ]
    .join("; ")
}

fn assert_cancelled_mutation(
    run: &mut RunningMutation,
    signal: &str,
    processes: (u32, u32),
) -> Result<(), Box<dyn std::error::Error>> {
    let cancelled_at = Instant::now();
    let (status, stdout, stderr) = cancel_child(&mut run.child, signal)?;
    assert!(
        cancelled_at.elapsed() < EXIT_TIMEOUT,
        "mutation cancellation was not prompt"
    );
    assert_cancelled_output(status, &stdout, &stderr, "cancelled mutation");
    assert_source_state(run, ExpectedSourceState::Restored)?;
    wait_for_processes([processes.0, processes.1]).map_err(Into::into)
}

fn assert_mutation_follow_up(run: &RunningMutation) -> Result<(), Box<dyn std::error::Error>> {
    let follow_up = mutation_command(&run.project, &run.state_parent, "true", "5")
        .arg("--allow-survivors")
        .output()?;
    assert_eq!(
        follow_up.status.code(),
        Some(0),
        "state was not reusable after cancellation\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&follow_up.stdout),
        String::from_utf8_lossy(&follow_up.stderr)
    );
    assert_source_state(run, ExpectedSourceState::Restored)?;
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
        let deadline = Deadline::after(timeout);
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            deadline.poll(format!("reporigor did not exit within {timeout:?}"))?;
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
    let deadline = Deadline::after(timeout);
    while !marker_ready(path, expected)? {
        ensure_marker_can_arrive(child, path, &deadline)?;
        Deadline::pause();
    }
    Ok(())
}

fn marker_ready(path: &Path, expected: &[u8]) -> io::Result<bool> {
    match fs::read(path) {
        Ok(contents) => Ok(contents == expected),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_marker_can_arrive(child: &mut ChildGuard, path: &Path, deadline: &Deadline) -> io::Result<()> {
    if let Some(status) = child.try_wait()? {
        return Err(io::Error::other(format!(
            "reporigor exited before readiness marker with {status}"
        )));
    }
    if deadline.expired() {
        return Err(timeout_error(format!(
            "readiness marker {} was not written",
            path.display()
        )));
    }
    Ok(())
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
        .output()
        .is_ok_and(|output| output.status.success());
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
    let deadline = Deadline::after(timeout);
    while process_exists(pid) {
        deadline.poll(format!("process {pid} survived cancellation"))?;
    }
    Ok(())
}

fn timeout_error(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
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
    collect_active_journals(&base)
}

fn collect_active_journals(base: &Path) -> io::Result<Vec<PathBuf>> {
    let mut journals = Vec::new();
    for entry in fs::read_dir(base)? {
        if let Some(journal) = active_journal(&entry?.path()) {
            journals.push(journal);
        }
    }
    journals.sort();
    Ok(journals)
}

fn active_journal(path: &Path) -> Option<PathBuf> {
    let journal = path.is_dir().then(|| path.join(ACTIVE_JOURNAL))?;
    journal.is_file().then_some(journal)
}
