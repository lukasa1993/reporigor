use std::collections::VecDeque;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reporigor_process_tree::{CleanupPolicy, ProcessTree, WaitReason};

const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct CapturedStream {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

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

    fn wait_until(&self, deadline: Instant) -> bool {
        let (lock, wake) = &*self.state;
        let mut state = match lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        while !state.complete {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = match wake.wait_timeout(state, remaining) {
                Ok((state, timeout)) => {
                    if timeout.timed_out() && !state.complete {
                        return false;
                    }
                    state
                }
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }

    fn finish(&mut self, deadline: Instant, stream: &str, action: &str) -> Result<CapturedStream, String> {
        if !self.wait_until(deadline) {
            return Err(format!(
                "{action} {stream} pipe remained open after process-tree cleanup"
            ));
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| format!("{action} {stream} reader panicked"))?;
        }
        let (lock, _) = &*self.state;
        let state = match lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(error) = &state.error {
            return Err(format!("cannot read {action} {stream}: {error}"));
        }
        Ok(CapturedStream {
            bytes: state.captured.iter().copied().collect(),
            truncated: state.truncated,
        })
    }
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
    let mut state = match lock.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    update(&mut state);
    wake.notify_all();
}

fn finish_output(
    stdout_reader: &mut OutputReader,
    stderr_reader: &mut OutputReader,
    action: &str,
) -> Result<(CapturedStream, CapturedStream), String> {
    let deadline = Instant::now() + OUTPUT_DRAIN_TIMEOUT;
    let stdout = stdout_reader.finish(deadline, "stdout", action);
    let stderr = stderr_reader.finish(deadline, "stderr", action);
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => Ok((stdout, stderr)),
        (Err(stdout), Ok(_)) => Err(stdout),
        (Ok(_), Err(stderr)) => Err(stderr),
        (Err(stdout), Err(stderr)) => Err(format!("{stdout}; {stderr}")),
    }
}

fn missing_pipe_error(tree: &mut ProcessTree, action: &str, stream: &str) -> String {
    let base = format!("{action} {stream} pipe is unavailable");
    match tree.terminate_bounded(CleanupPolicy::default()) {
        Ok(_) => base,
        Err(cleanup) => format!("{base}; subsequent {cleanup}"),
    }
}

pub(crate) fn run_bounded(
    command: &mut Command,
    action: &str,
    limits: CommandLimits,
) -> Result<BoundedOutput, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut tree = ProcessTree::spawn(command).map_err(|error| format!("cannot start {action}: {error}"))?;
    let stdout = tree
        .take_stdout()
        .ok_or_else(|| missing_pipe_error(&mut tree, action, "stdout"))?;
    let stderr = tree
        .take_stderr()
        .ok_or_else(|| missing_pipe_error(&mut tree, action, "stderr"))?;
    let mut stdout_reader = OutputReader::spawn(stdout, limits.stdout_bytes);
    let mut stderr_reader = OutputReader::spawn(stderr, limits.stderr_bytes);

    let wait = tree.wait_bounded(limits.timeout, CleanupPolicy::default());
    let output = finish_output(&mut stdout_reader, &mut stderr_reader, action);
    let outcome = match wait {
        Ok(outcome) => outcome,
        Err(error) => {
            return Err(match output {
                Ok(_) => format!("cannot wait for {action}: {error}"),
                Err(output_error) => {
                    format!("cannot wait for {action}: {error}; additionally {output_error}")
                }
            });
        }
    };
    let (stdout, stderr) = output?;

    if outcome.reason == WaitReason::TimedOut {
        let rendered_stderr = render_stream(&stderr, limits.stderr_bytes);
        let detail = if rendered_stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", rendered_stderr.trim())
        };
        return Err(format!(
            "{action} timed out after {:.3} seconds{detail}; the Cargo process tree was terminated",
            limits.timeout.as_secs_f64()
        ));
    }

    Ok(BoundedOutput {
        status: outcome.status,
        stdout,
        stderr,
    })
}

pub(crate) fn render_stream(stream: &CapturedStream, limit: usize) -> String {
    let mut rendered = String::from_utf8_lossy(&stream.bytes).into_owned();
    if stream.truncated {
        rendered.insert_str(0, &format!("[output truncated to the last {limit} bytes]\n"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn captured(input: &[u8], limit: usize) -> CapturedStream {
        let mut reader = OutputReader::spawn(Cursor::new(input.to_vec()), limit);
        reader
            .finish(Instant::now() + OUTPUT_DRAIN_TIMEOUT, "test", "capture")
            .unwrap_or_else(|error| panic!("capture: {error}"))
    }

    #[test]
    fn tail_capture_preserves_limits_and_truncation() {
        let tail = captured(b"0123456789", 4);
        assert_eq!(tail.bytes, b"6789");
        assert!(tail.truncated);

        let exact = captured(b"0123", 4);
        assert_eq!(exact.bytes, b"0123");
        assert!(!exact.truncated);

        let zero = captured(b"x", 0);
        assert!(zero.bytes.is_empty());
        assert!(zero.truncated);

        let empty = captured(b"", 0);
        assert!(empty.bytes.is_empty());
        assert!(!empty.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn successful_leader_cleans_pipe_holding_descendant() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "(sleep 2; printf leaked > leaked.txt) & printf ready"])
            .current_dir(directory.path());
        let started = Instant::now();

        let output = run_bounded(
            &mut command,
            "successful background command",
            CommandLimits {
                timeout: Duration::from_secs(1),
                stdout_bytes: 4096,
                stderr_bytes: 4096,
            },
        )
        .unwrap_or_else(|error| panic!("bounded command: {error}"));

        assert!(output.status.success());
        assert_eq!(output.stdout.bytes, b"ready");
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
    }
}
