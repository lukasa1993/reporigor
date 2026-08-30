use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reporigor_process_tree::{configure_piped_command, CleanupPolicy, PollResult, ProcessTree};

use crate::{CancellationToken, CommandOutcome, CommandSpec, MutationError};

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Environment variable set for validation and test commands executed against
/// one active mutant. It is absent from baseline commands.
pub const MUTANT_ID_ENV: &str = "REPORIGOR_MUTANT_ID";

/// Stable structural fingerprint of the active mutant. It is absent from
/// baseline commands.
pub const MUTANT_FINGERPRINT_ENV: &str = "REPORIGOR_MUTANT_FINGERPRINT";

struct OutputReader {
    captured: Arc<Mutex<CapturedOutput>>,
    completion: mpsc::Receiver<std::io::Result<()>>,
    handle: thread::JoinHandle<()>,
}

#[derive(Clone, Debug)]
struct CapturedOutput {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl CapturedOutput {
    fn extend_tail(&mut self, incoming: &[u8], limit: usize) {
        if limit == 0 {
            self.truncated |= !incoming.is_empty();
            return;
        }
        if incoming.len() >= limit {
            self.replace_with_tail(incoming, limit);
            return;
        }
        let excess = self
            .bytes
            .len()
            .saturating_add(incoming.len())
            .saturating_sub(limit);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.truncated = true;
        }
        self.bytes.extend(incoming.iter().copied());
    }

    fn replace_with_tail(&mut self, incoming: &[u8], limit: usize) {
        self.truncated |= !self.bytes.is_empty() || incoming.len() > limit;
        self.bytes.clear();
        self.bytes
            .extend(incoming[incoming.len() - limit..].iter().copied());
    }
}

fn empty_capture(limit: usize) -> CapturedOutput {
    CapturedOutput {
        bytes: VecDeque::with_capacity(limit.min(64 * 1024)),
        truncated: false,
    }
}

struct CommandTermination {
    status: ExitStatus,
    timed_out: bool,
}

fn spawn_command(
    specification: &CommandSpec,
    root: &Path,
    environment: &[(&str, &str)],
) -> Result<ProcessTree, MutationError> {
    let mut command = match specification {
        CommandSpec::Shell { command } => {
            #[cfg(windows)]
            let process = {
                let mut value = Command::new("cmd");
                value.args(["/D", "/S", "/C", command]);
                value
            };
            #[cfg(not(windows))]
            let process = {
                let mut value = Command::new("/bin/sh");
                value.args(["-c", command]);
                value
            };
            process
        }
        CommandSpec::Program { program, args } => {
            let mut process = Command::new(program);
            process.args(args);
            process
        }
    };
    command.current_dir(root);
    configure_piped_command(&mut command);
    command.env_remove(MUTANT_ID_ENV);
    command.env_remove(MUTANT_FINGERPRINT_ENV);
    command.envs(environment.iter().copied());
    ProcessTree::spawn(&mut command)
        .map_err(|source| MutationError::io("spawn contained command", root, std::io::Error::other(source)))
}

fn cancellation_cleanup(child: &mut ProcessTree) -> Result<(), MutationError> {
    child
        .terminate_bounded(CleanupPolicy::default())
        .map(|_| ())
        .map_err(|source| MutationError::ProcessTree {
            operation: "cleaning up a cancelled command",
            message: source.to_string(),
        })
}

fn timeout_cleanup(child: &mut ProcessTree) -> Result<ExitStatus, MutationError> {
    let report = child
        .terminate_bounded(CleanupPolicy::default())
        .map_err(|source| MutationError::ProcessTree {
            operation: "cleaning up a timed-out command",
            message: source.to_string(),
        })?;
    report.status.ok_or_else(|| {
        MutationError::Command("timed-out command cleanup did not reap the process leader".into())
    })
}

fn drain_output<R: Read + Send + 'static>(mut reader: R, limit: usize) -> OutputReader {
    let captured = Arc::new(Mutex::new(empty_capture(limit)));
    let thread_capture = Arc::clone(&captured);
    let (completion_sender, completion) = mpsc::channel();
    let handle = thread::spawn(move || {
        let result = (|| {
            let mut buffer = [0_u8; 8192];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                thread_capture
                    .lock()
                    .map_err(|_| std::io::Error::other("command output capture lock was poisoned"))?
                    .extend_tail(&buffer[..count], limit);
            }
            Ok(())
        })();
        let _ = completion_sender.send(result);
    });
    OutputReader {
        captured,
        completion,
        handle,
    }
}

fn finish_output(reader: OutputReader, grace: Duration) -> Result<CapturedOutput, MutationError> {
    match reader.completion.recv_timeout(grace) {
        Ok(result) => finish_completed_output(reader, result),
        Err(mpsc::RecvTimeoutError::Disconnected) => disconnected_output(reader),
        Err(mpsc::RecvTimeoutError::Timeout) => timed_out_output(&reader),
    }
}

fn finish_completed_output(
    reader: OutputReader,
    result: std::io::Result<()>,
) -> Result<CapturedOutput, MutationError> {
    let joined = reader.handle.join();
    result.map_err(|source| MutationError::io("read command output", "<pipe>", source))?;
    joined.map_err(|_| MutationError::Command("command output reader panicked".into()))?;
    captured_output(&reader.captured)
}

fn disconnected_output(reader: OutputReader) -> Result<CapturedOutput, MutationError> {
    let _ = reader.handle.join();
    Err(MutationError::Command(
        "command output reader stopped unexpectedly".into(),
    ))
}

fn timed_out_output(reader: &OutputReader) -> Result<CapturedOutput, MutationError> {
    // A descendant may have inherited the pipe after the process-group leader
    // exited. Never let that keep the mutation run blocked.
    let mut captured = captured_output(&reader.captured)?;
    captured.truncated = true;
    Ok(captured)
}

fn captured_output(captured: &Mutex<CapturedOutput>) -> Result<CapturedOutput, MutationError> {
    captured
        .lock()
        .map_err(|_| MutationError::Command("command output capture lock was poisoned".into()))
        .map(|captured| captured.clone())
}

fn tail_bytes(mut bytes: Vec<u8>, limit: usize) -> (Vec<u8>, bool) {
    if bytes.len() <= limit {
        return (bytes, false);
    }
    let remove = bytes.len() - limit;
    bytes.drain(..remove);
    (bytes, true)
}

fn bounded_lossy(bytes: &[u8], limit: usize) -> (String, bool) {
    if limit == 0 {
        return (String::new(), !bytes.is_empty());
    }
    let rendered = String::from_utf8_lossy(bytes);
    if rendered.len() <= limit {
        return (rendered.into_owned(), false);
    }
    let mut start = rendered.len() - limit;
    while !rendered.is_char_boundary(start) {
        start += 1;
    }
    (rendered[start..].to_owned(), true)
}

/// Execute a command with bounded output and whole-process-tree timeout cleanup.
///
/// # Errors
///
/// Returns an error when the command is empty, cannot be spawned or waited on,
/// or when one of its output streams cannot be drained.
pub fn run_command(
    specification: &CommandSpec,
    root: &Path,
    timeout: Duration,
    output_limit_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<CommandOutcome, MutationError> {
    run_command_with_environment(
        specification,
        root,
        timeout,
        output_limit_bytes,
        cancellation,
        &[],
    )
}

pub(crate) fn run_command_with_environment(
    specification: &CommandSpec,
    root: &Path,
    timeout: Duration,
    output_limit_bytes: usize,
    cancellation: &CancellationToken,
    environment: &[(&str, &str)],
) -> Result<CommandOutcome, MutationError> {
    validate_command(specification)?;
    cancellation.check()?;
    let started = Instant::now();
    let mut child = spawn_command(specification, root, environment)?;
    let (stdout_reader, stderr_reader) = output_readers(&mut child, output_limit_bytes)?;
    let termination = wait_for_termination(&mut child, started, timeout, cancellation);
    match termination {
        Ok(termination) => finish_command(
            &termination,
            stdout_reader,
            stderr_reader,
            started,
            output_limit_bytes,
            cancellation,
        ),
        Err(error) => {
            discard_output(stdout_reader, stderr_reader);
            Err(error)
        }
    }
}

fn validate_command(specification: &CommandSpec) -> Result<(), MutationError> {
    if specification.is_empty() {
        Err(MutationError::InvalidOptions("command cannot be empty".into()))
    } else {
        Ok(())
    }
}

fn output_readers(
    child: &mut ProcessTree,
    output_limit_bytes: usize,
) -> Result<(OutputReader, OutputReader), MutationError> {
    let stdout = child
        .take_stdout()
        .ok_or_else(|| MutationError::Command("child stdout pipe is unavailable".into()))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| MutationError::Command("child stderr pipe is unavailable".into()))?;
    Ok((
        drain_output(stdout, output_limit_bytes),
        drain_output(stderr, output_limit_bytes),
    ))
}

fn wait_for_termination(
    child: &mut ProcessTree,
    started: Instant,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<CommandTermination, MutationError> {
    loop {
        if let Some(termination) = poll_termination(child, started, timeout, cancellation)? {
            return Ok(termination);
        }
    }
}

fn poll_termination(
    child: &mut ProcessTree,
    started: Instant,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Option<CommandTermination>, MutationError> {
    if cancellation.is_cancelled() {
        cancellation_cleanup(child)?;
        return Err(MutationError::Cancelled);
    }
    let elapsed = started.elapsed();
    if elapsed >= timeout {
        return timeout_cleanup(child).map(|status| {
            Some(CommandTermination {
                status,
                timed_out: true,
            })
        });
    }
    poll_running_process(child, timeout.saturating_sub(elapsed))
}

fn poll_running_process(
    child: &mut ProcessTree,
    remaining: Duration,
) -> Result<Option<CommandTermination>, MutationError> {
    let wait_for = remaining.min(CANCELLATION_POLL_INTERVAL);
    match child.wait_slice(wait_for, CleanupPolicy::default()) {
        Ok(PollResult::Exited(outcome)) => Ok(Some(CommandTermination {
            status: outcome.status,
            timed_out: false,
        })),
        Ok(PollResult::Running) => Ok(None),
        Err(source) => Err(MutationError::ProcessTree {
            operation: "waiting for a contained command",
            message: source.to_string(),
        }),
    }
}

fn discard_output(stdout: OutputReader, stderr: OutputReader) {
    let _ = finish_output(stdout, OUTPUT_DRAIN_GRACE);
    let _ = finish_output(stderr, OUTPUT_DRAIN_GRACE);
}

fn finish_command(
    termination: &CommandTermination,
    stdout_reader: OutputReader,
    stderr_reader: OutputReader,
    started: Instant,
    output_limit_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<CommandOutcome, MutationError> {
    let stdout = finish_output(stdout_reader, OUTPUT_DRAIN_GRACE)?;
    let stderr = finish_output(stderr_reader, OUTPUT_DRAIN_GRACE)?;
    let mut combined: Vec<u8> = stdout.bytes.into_iter().collect();
    combined.extend(stderr.bytes);
    let (combined, combined_truncated) = tail_bytes(combined, output_limit_bytes);
    let (output, encoding_truncated) = bounded_lossy(&combined, output_limit_bytes);
    cancellation.check()?;

    Ok(CommandOutcome {
        exit_code: command_exit_code(termination),
        timed_out: termination.timed_out,
        duration_seconds: started.elapsed().as_secs_f64(),
        output,
        output_truncated: any_output_truncated([
            stdout.truncated,
            stderr.truncated,
            combined_truncated,
            encoding_truncated,
        ]),
    })
}

fn command_exit_code(termination: &CommandTermination) -> Option<i32> {
    if termination.timed_out {
        None
    } else {
        termination.status.code()
    }
}

fn any_output_truncated(indicators: [bool; 4]) -> bool {
    indicators.into_iter().any(std::convert::identity)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{tempdir, TempDir};

    use super::*;

    struct CommandFixture {
        directory: TempDir,
        cancellation: CancellationToken,
    }

    impl CommandFixture {
        fn new() -> Result<Self, std::io::Error> {
            Ok(Self {
                cancellation: CancellationToken::new(),
                directory: tempdir()?,
            })
        }
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum DescendantScenario {
        Timeout,
        Cancellation,
        ExitedParent,
    }

    #[cfg(unix)]
    const DESCENDANT_SCENARIOS: [(&str, &str, u64, u64); 3] = [
        (
            "leaked.txt",
            "(sleep 0.2; printf leaked > leaked.txt) & wait",
            20,
            350,
        ),
        (
            "leaked-after-cancellation.txt",
            "(sleep 0.4; printf leaked > leaked-after-cancellation.txt) & wait",
            5_000,
            500,
        ),
        (
            "leaked-after-parent-exit.txt",
            "printf parent-output; (sleep 0.5; printf leaked > leaked-after-parent-exit.txt) & exit 0",
            2_000,
            700,
        ),
    ];

    fn run_shell(
        fixture: &CommandFixture,
        shell: &str,
        timeout: Duration,
    ) -> Result<CommandOutcome, MutationError> {
        run_command(
            &CommandSpec::shell(shell),
            fixture.directory.path(),
            timeout,
            4096,
            &fixture.cancellation,
        )
    }

    #[cfg(unix)]
    fn run_cleaned_marked_shell(
        fixture: &CommandFixture,
        scenario: DescendantScenario,
    ) -> Result<CommandOutcome, MutationError> {
        let (marker_name, shell, timeout_ms, observation_ms) = DESCENDANT_SCENARIOS[scenario as usize];
        let marker = fixture.directory.path().join(marker_name);
        let started = Instant::now();
        let result = run_shell(fixture, shell, Duration::from_millis(timeout_ms));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_file_never_appears(&marker, Duration::from_millis(observation_ms));
        result
    }

    fn assert_file_never_appears(marker: &Path, wait: Duration) {
        thread::sleep(wait);
        assert!(!marker.exists());
    }

    #[test]
    fn command_io_is_scoped_bounded_and_reports_spawn_errors() {
        let fixture = CommandFixture::new()
            .unwrap_or_else(|error| panic!("failed to create command fixture: {error:?}"));
        #[cfg(not(windows))]
        {
            let command = CommandSpec::shell("test \"$REPORIGOR_MUTANT_ID\" = 17");
            let with_environment = run_command_with_environment(
                &command,
                fixture.directory.path(),
                Duration::from_secs(1),
                1024,
                &fixture.cancellation,
                &[(MUTANT_ID_ENV, "17")],
            )
            .unwrap_or_else(|error| panic!("scoped-environment command failed: {error:?}"));
            assert!(with_environment.success());
            let without_environment = run_command(
                &command,
                fixture.directory.path(),
                Duration::from_secs(1),
                1024,
                &fixture.cancellation,
            )
            .unwrap_or_else(|error| panic!("plain command failed: {error:?}"));
            assert!(!without_environment.success());

            let outcome = run_shell(
                &fixture,
                "i=0; while [ \"$i\" -lt 10000 ]; do printf '0123456789\\n'; i=$((i + 1)); done",
                Duration::from_secs(20),
            )
            .unwrap_or_else(|error| panic!("bounded-output command failed: {error:?}"));
            assert!(outcome.success());
            assert!(outcome.output_truncated);
            assert!(outcome.output.len() <= 4096);
            assert!(outcome.output.ends_with("0123456789\n"));
        }

        let missing = fixture.directory.path().join("definitely-missing-command");
        let result = run_command(
            &CommandSpec::program(missing, Vec::<String>::new()),
            fixture.directory.path(),
            Duration::from_secs(1),
            1024,
            &fixture.cancellation,
        );
        assert!(matches!(result, Err(MutationError::Io { .. })));
        let entries = fs::read_dir(fixture.directory.path())
            .unwrap_or_else(|error| panic!("failed to inspect command fixture: {error:?}"));
        assert_eq!(entries.count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn process_tree_cleanup_covers_timeout_cancellation_and_parent_exit(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = CommandFixture::new()?;
        let timed_out = run_cleaned_marked_shell(&fixture, DescendantScenario::Timeout)?;
        assert!(timed_out.timed_out);
        assert_eq!(timed_out.exit_code, None);

        let canceller = fixture.cancellation.clone();
        let cancellation_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            canceller.cancel();
        });
        let result = run_cleaned_marked_shell(&fixture, DescendantScenario::Cancellation);
        cancellation_thread
            .join()
            .map_err(|_| "cancellation thread panicked")?;

        assert!(matches!(result, Err(MutationError::Cancelled)));

        let exited_fixture = CommandFixture::new()?;
        let exited = run_cleaned_marked_shell(&exited_fixture, DescendantScenario::ExitedParent)?;

        assert!(exited.success());
        assert!(!exited.timed_out);
        assert!(exited.output.contains("parent-output"));
        Ok(())
    }
}
