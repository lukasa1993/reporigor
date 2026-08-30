use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{CleanupPolicy, ProcessTree, WaitError, WaitOutcome, WaitReason};

const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Configure a child for bounded capture without inheriting standard input.
pub fn configure_piped_command(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// Per-stream and wall-clock bounds for a contained command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

/// One bounded byte stream captured from a contained command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedStream {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Output returned only after the process tree and both stream readers finish.
#[derive(Clone, Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
}

/// Stage at which bounded command execution failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundedRunStage {
    Start,
    Wait,
    Output,
    Timeout,
}

/// Failure from bounded process-tree execution.
#[derive(Debug)]
pub struct BoundedRunError {
    stage: BoundedRunStage,
    message: String,
}

impl BoundedRunError {
    fn new(stage: BoundedRunStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }

    /// Failed execution stage.
    #[must_use]
    pub const fn stage(&self) -> BoundedRunStage {
        self.stage
    }

    /// Stable human-readable detail without caller-specific action text.
    #[must_use]
    pub fn detail(&self) -> &str {
        self.message()
    }

    fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for BoundedRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl Error for BoundedRunError {}

#[derive(Debug)]
struct OutputReader {
    state: Arc<(Mutex<ReaderState>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct ReaderState {
    captured: VecDeque<u8>,
    truncated: bool,
    complete: bool,
    error: Option<String>,
}

impl OutputReader {
    fn spawn<R: Read + Send + 'static>(mut reader: R, limit: usize) -> Self {
        let state = Arc::new((
            Mutex::new(ReaderState {
                captured: VecDeque::with_capacity(limit.min(64 * 1024)),
                truncated: false,
                complete: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let reader_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        update_reader_state(reader_state.as_ref(), |state| state.complete = true);
                        break;
                    }
                    Ok(count) => update_reader_state(reader_state.as_ref(), |state| {
                        retain_tail(state, &buffer[..count], limit);
                    }),
                    Err(error) => {
                        update_reader_state(reader_state.as_ref(), |state| {
                            state.error = Some(error.to_string());
                            state.complete = true;
                        });
                        break;
                    }
                }
            }
        });
        Self {
            state,
            thread: Some(thread),
        }
    }

    fn finish(&mut self, deadline: Instant, stream: &str) -> Result<CapturedStream, BoundedRunError> {
        if !self.wait_until(deadline) {
            return Err(BoundedRunError::new(
                BoundedRunStage::Output,
                format!("{stream} pipe remained open after process-tree cleanup"),
            ));
        }
        join_reader(self.thread.take(), stream)?;
        let (lock, _) = &*self.state;
        let state = lock_reader_state(lock);
        if let Some(error) = &state.error {
            return Err(BoundedRunError::new(
                BoundedRunStage::Output,
                format!("cannot read {stream}: {error}"),
            ));
        }
        Ok(CapturedStream {
            bytes: state.captured.iter().copied().collect(),
            truncated: state.truncated,
        })
    }

    fn wait_until(&self, deadline: Instant) -> bool {
        let (lock, wake) = &*self.state;
        let mut state = lock_reader_state(lock);
        while !state.complete {
            let Some(next) = wait_for_reader_completion(wake, state, deadline) else {
                return false;
            };
            state = next;
        }
        true
    }
}

fn wait_for_reader_completion<'a>(
    wake: &Condvar,
    state: MutexGuard<'a, ReaderState>,
    deadline: Instant,
) -> Option<MutexGuard<'a, ReaderState>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    let (next, timed_out) = wait_for_reader(wake, state, remaining);
    reader_finished(timed_out, next.complete).then_some(next)
}

fn reader_finished(timed_out: bool, complete: bool) -> bool {
    !timed_out || complete
}

fn lock_reader_state(lock: &Mutex<ReaderState>) -> MutexGuard<'_, ReaderState> {
    lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_for_reader<'a>(
    wake: &Condvar,
    state: MutexGuard<'a, ReaderState>,
    remaining: Duration,
) -> (MutexGuard<'a, ReaderState>, bool) {
    match wake.wait_timeout(state, remaining) {
        Ok((state, timeout)) => (state, timeout.timed_out()),
        Err(poisoned) => (poisoned.into_inner().0, false),
    }
}

fn join_reader(thread: Option<JoinHandle<()>>, stream: &str) -> Result<(), BoundedRunError> {
    let Some(thread) = thread else {
        return Ok(());
    };
    thread
        .join()
        .map_err(|_| BoundedRunError::new(BoundedRunStage::Output, format!("{stream} reader panicked")))
}

fn retain_tail(state: &mut ReaderState, incoming: &[u8], limit: usize) {
    if limit == 0 {
        state.truncated |= !incoming.is_empty();
        return;
    }
    if incoming.len() >= limit {
        state.truncated |= !state.captured.is_empty() || incoming.len() > limit;
        state.captured.clear();
        state
            .captured
            .extend(incoming[incoming.len() - limit..].iter().copied());
        return;
    }
    let excess = state
        .captured
        .len()
        .saturating_add(incoming.len())
        .saturating_sub(limit);
    if excess > 0 {
        state.captured.drain(..excess);
        state.truncated = true;
    }
    state.captured.extend(incoming.iter().copied());
}

fn update_reader_state(shared: &(Mutex<ReaderState>, Condvar), update: impl FnOnce(&mut ReaderState)) {
    let (lock, wake) = shared;
    let mut state = lock_reader_state(lock);
    update(&mut state);
    wake.notify_all();
}

/// Run one command with bounded output, time, and descendant cleanup.
///
/// # Errors
///
/// Returns the precise failed stage when the process cannot start, wait,
/// drain output, or finish before the configured deadline.
pub fn run_bounded(command: &mut Command, limits: CommandLimits) -> Result<BoundedOutput, BoundedRunError> {
    configure_piped_command(command);
    let mut tree = ProcessTree::spawn(command)
        .map_err(|error| BoundedRunError::new(BoundedRunStage::Start, error.to_string()))?;
    let (mut stdout_reader, mut stderr_reader) = output_readers(&mut tree, limits)?;
    let wait = tree.wait_bounded(limits.timeout, CleanupPolicy::default());
    let deadline = Instant::now() + OUTPUT_DRAIN_TIMEOUT;
    complete_bounded_output(
        wait,
        stdout_reader.finish(deadline, "stdout"),
        stderr_reader.finish(deadline, "stderr"),
        limits.timeout,
    )
}

fn output_readers(
    tree: &mut ProcessTree,
    limits: CommandLimits,
) -> Result<(OutputReader, OutputReader), BoundedRunError> {
    let stdout = take_pipe(tree, true)?;
    let stderr = take_pipe(tree, false)?;
    Ok((
        OutputReader::spawn(stdout, limits.stdout_bytes),
        OutputReader::spawn(stderr, limits.stderr_bytes),
    ))
}

fn complete_bounded_output(
    wait: Result<crate::WaitOutcome, crate::WaitError>,
    stdout: Result<CapturedStream, BoundedRunError>,
    stderr: Result<CapturedStream, BoundedRunError>,
    timeout: Duration,
) -> Result<BoundedOutput, BoundedRunError> {
    let outcome = completed_process(wait, &stdout, &stderr)?;
    let stdout = stdout?;
    let stderr = stderr?;
    if outcome.reason == WaitReason::TimedOut {
        return Err(BoundedRunError::new(
            BoundedRunStage::Timeout,
            format!("timed out after {:.3} seconds", timeout.as_secs_f64()),
        ));
    }
    Ok(BoundedOutput {
        status: outcome.status,
        stdout,
        stderr,
    })
}

fn take_pipe(tree: &mut ProcessTree, stdout: bool) -> Result<impl Read + Send + 'static, BoundedRunError> {
    let pipe = if stdout {
        tree.take_stdout().map(Pipe::Stdout)
    } else {
        tree.take_stderr().map(Pipe::Stderr)
    };
    pipe.ok_or_else(|| {
        let cleanup = tree.terminate_bounded(CleanupPolicy::default()).err();
        let suffix = cleanup.map_or_else(String::new, |error| format!("; subsequent {error}"));
        let name = if stdout { "stdout" } else { "stderr" };
        BoundedRunError::new(
            BoundedRunStage::Output,
            format!("{name} pipe is unavailable{suffix}"),
        )
    })
}

enum Pipe {
    Stdout(std::process::ChildStdout),
    Stderr(std::process::ChildStderr),
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdout(pipe) => pipe.read(buffer),
            Self::Stderr(pipe) => pipe.read(buffer),
        }
    }
}

fn completed_process(
    wait: Result<WaitOutcome, WaitError>,
    stdout: &Result<CapturedStream, BoundedRunError>,
    stderr: &Result<CapturedStream, BoundedRunError>,
) -> Result<WaitOutcome, BoundedRunError> {
    wait.map_err(|error| {
        let output_detail = stdout
            .as_ref()
            .err()
            .or_else(|| stderr.as_ref().err())
            .map_or_else(String::new, |output| format!("; additionally {output}"));
        BoundedRunError::new(BoundedRunStage::Wait, format!("{error}{output_detail}"))
    })
}
