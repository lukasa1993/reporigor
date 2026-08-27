use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reporigor_process_tree::{CleanupPolicy, PollResult, ProcessTree};

use crate::{CancellationToken, CommandOutcome, CommandSpec, MutationError};

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
    fn with_limit(limit: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(limit.min(64 * 1024)),
            truncated: false,
        }
    }

    fn extend_tail(&mut self, incoming: &[u8], limit: usize) {
        if limit == 0 {
            self.truncated |= !incoming.is_empty();
            return;
        }
        if incoming.len() >= limit {
            self.truncated |= !self.bytes.is_empty() || incoming.len() > limit;
            self.bytes.clear();
            self.bytes
                .extend(incoming[incoming.len() - limit..].iter().copied());
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
}

fn spawn_command(specification: &CommandSpec, root: &Path) -> Result<ProcessTree, MutationError> {
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
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let captured = Arc::new(Mutex::new(CapturedOutput::with_limit(limit)));
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
        Ok(Ok(())) => {
            reader
                .handle
                .join()
                .map_err(|_| MutationError::Command("command output reader panicked".into()))?;
        }
        Ok(Err(source)) => {
            let _ = reader.handle.join();
            return Err(MutationError::io("read command output", "<pipe>", source));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = reader.handle.join();
            return Err(MutationError::Command(
                "command output reader stopped unexpectedly".into(),
            ));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // A descendant may have inherited the pipe after the process-group
            // leader exited. Never let that keep the mutation run blocked.
            let mut captured = reader
                .captured
                .lock()
                .map_err(|_| MutationError::Command("command output capture lock was poisoned".into()))?
                .clone();
            captured.truncated = true;
            return Ok(captured);
        }
    }
    reader
        .captured
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
    if specification.is_empty() {
        return Err(MutationError::InvalidOptions("command cannot be empty".into()));
    }
    cancellation.check()?;
    let started = Instant::now();
    let mut child = spawn_command(specification, root)?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| MutationError::Command("child stdout pipe is unavailable".into()))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| MutationError::Command("child stderr pipe is unavailable".into()))?;
    let stdout_reader = drain_output(stdout, output_limit_bytes);
    let stderr_reader = drain_output(stderr, output_limit_bytes);

    let (status, timed_out) = loop {
        if cancellation.is_cancelled() {
            let cleanup = cancellation_cleanup(&mut child);
            let _ = finish_output(stdout_reader, OUTPUT_DRAIN_GRACE);
            let _ = finish_output(stderr_reader, OUTPUT_DRAIN_GRACE);
            cleanup?;
            return Err(MutationError::Cancelled);
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            match timeout_cleanup(&mut child) {
                Ok(status) => break (status, true),
                Err(error) => {
                    let _ = finish_output(stdout_reader, OUTPUT_DRAIN_GRACE);
                    let _ = finish_output(stderr_reader, OUTPUT_DRAIN_GRACE);
                    return Err(error);
                }
            }
        }
        let wait_for = timeout.saturating_sub(elapsed).min(CANCELLATION_POLL_INTERVAL);
        match child.wait_slice(wait_for, CleanupPolicy::default()) {
            Ok(PollResult::Exited(outcome)) => break (outcome.status, false),
            Ok(PollResult::Running) => {}
            Err(source) => {
                let _ = finish_output(stdout_reader, OUTPUT_DRAIN_GRACE);
                let _ = finish_output(stderr_reader, OUTPUT_DRAIN_GRACE);
                return Err(MutationError::ProcessTree {
                    operation: "waiting for a contained command",
                    message: source.to_string(),
                });
            }
        }
    };

    let stdout = finish_output(stdout_reader, OUTPUT_DRAIN_GRACE)?;
    let stderr = finish_output(stderr_reader, OUTPUT_DRAIN_GRACE)?;
    let mut combined: Vec<u8> = stdout.bytes.into_iter().collect();
    combined.extend(stderr.bytes);
    let (combined, combined_truncated) = tail_bytes(combined, output_limit_bytes);
    let (output, encoding_truncated) = bounded_lossy(&combined, output_limit_bytes);
    cancellation.check()?;

    Ok(CommandOutcome {
        exit_code: if timed_out { None } else { status.code() },
        timed_out,
        duration_seconds: started.elapsed().as_secs_f64(),
        output,
        output_truncated: stdout.truncated || stderr.truncated || combined_truncated || encoding_truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn command_output_is_drained_and_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let command = CommandSpec::shell(
            "i=0; while [ \"$i\" -lt 10000 ]; do printf '0123456789\\n'; i=$((i + 1)); done",
        );
        let cancellation = CancellationToken::new();
        let outcome = run_command(
            &command,
            directory.path(),
            Duration::from_secs(20),
            4096,
            &cancellation,
        )?;
        assert!(outcome.success());
        assert!(outcome.output_truncated);
        assert!(outcome.output.len() <= 4096);
        assert!(outcome.output.ends_with("0123456789\n"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendant_processes_before_returning() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let marker = directory.path().join("leaked.txt");
        let command = CommandSpec::shell("(sleep 0.2; printf leaked > leaked.txt) & wait");
        let cancellation = CancellationToken::new();
        let outcome = run_command(
            &command,
            directory.path(),
            Duration::from_millis(20),
            4096,
            &cancellation,
        )?;
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        thread::sleep(Duration::from_millis(350));
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_descendant_processes_before_returning() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let marker = directory.path().join("leaked-after-cancellation.txt");
        let command = CommandSpec::shell("(sleep 0.4; printf leaked > leaked-after-cancellation.txt) & wait");
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let cancellation_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            canceller.cancel();
        });
        let started = Instant::now();
        let result = run_command(
            &command,
            directory.path(),
            Duration::from_secs(5),
            4096,
            &cancellation,
        );
        cancellation_thread
            .join()
            .map_err(|_| "cancellation thread panicked")?;

        assert!(matches!(result, Err(MutationError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn exited_parent_with_open_descendant_pipe_does_not_block() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let marker = directory.path().join("leaked-after-parent-exit.txt");
        let command = CommandSpec::shell(
            "printf parent-output; (sleep 0.5; printf leaked > leaked-after-parent-exit.txt) & exit 0",
        );
        let started = Instant::now();
        let cancellation = CancellationToken::new();
        let outcome = run_command(
            &command,
            directory.path(),
            Duration::from_secs(2),
            4096,
            &cancellation,
        )?;

        assert!(outcome.success());
        assert!(!outcome.timed_out);
        assert!(outcome.output.contains("parent-output"));
        assert!(started.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(700));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn direct_command_spawn_errors_are_reported() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let missing = directory.path().join("definitely-missing-command");
        let result = run_command(
            &CommandSpec::program(missing, Vec::<String>::new()),
            directory.path(),
            Duration::from_secs(1),
            1024,
            &CancellationToken::new(),
        );
        assert!(matches!(result, Err(MutationError::Io { .. })));
        assert_eq!(fs::read_dir(directory.path())?.count(), 0);
        Ok(())
    }
}
