use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reporigor_core::CoreError;
use reporigor_process_tree::{CleanupPolicy, ProcessTree, WaitReason};

const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// A read-only project-provider command.
///
/// Merely constructing a command does not execute it. Commands are passed to a
/// [`CommandRunner`] only by the explicit preflight API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl ProviderCommand {
    #[must_use]
    pub fn new(
        program: PathBuf,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        cwd: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            program,
            args: args.into_iter().map(Into::into).collect(),
            cwd,
            timeout,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Whether either captured stream exceeded the fixed capture bound.
    pub output_truncated: bool,
}

impl ProviderCommandOutput {
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }
}

/// Injectable boundary used by project provider preflight checks.
pub trait CommandRunner: Send + Sync {
    /// Execute one bounded provider command.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Command`] when the command cannot start, times out,
    /// or its output cannot be collected. A nonzero exit is returned as normal
    /// [`ProviderCommandOutput`] so callers can report it as provider status.
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError>;
}

/// Bounded, timeout-aware subprocess runner used by normal preflight calls.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .current_dir(&command.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ProcessTree::spawn(&mut process).map_err(|error| {
            CoreError::Command(format!("failed to start {}: {error}", command.program.display()))
        })?;

        let stdout = child
            .take_stdout()
            .ok_or_else(|| CoreError::Command("provider stdout pipe is unavailable".to_string()))?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| CoreError::Command("provider stderr pipe is unavailable".to_string()))?;
        let mut stdout_reader = OutputReader::spawn(stdout);
        let mut stderr_reader = OutputReader::spawn(stderr);

        let wait_result = child.wait_bounded(command.timeout, CleanupPolicy::default());
        let deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
        // Always close/collect both readers before propagating either error;
        // returning early here would detach the other pipe worker.
        let stdout_result = stdout_reader.finish(deadline, "stdout");
        let stderr_result = stderr_reader.finish(deadline, "stderr");
        let outcome = wait_result.map_err(|error| {
            CoreError::Command(format!(
                "failed while waiting for {}: {error}",
                command.program.display()
            ))
        })?;
        if outcome.reason == WaitReason::TimedOut {
            return Err(CoreError::Command(format!(
                "{} timed out after {:.3} seconds",
                command.program.display(),
                command.timeout.as_secs_f64()
            )));
        }
        let (stdout, stdout_truncated) = stdout_result?;
        let (stderr, stderr_truncated) = stderr_result?;
        Ok(ProviderCommandOutput {
            exit_code: outcome.status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            output_truncated: stdout_truncated || stderr_truncated,
        })
    }
}

#[derive(Debug)]
struct OutputReader {
    state: Arc<(Mutex<ReaderState>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct ReaderState {
    captured: Vec<u8>,
    complete: bool,
    truncated: bool,
    error: Option<String>,
}

impl OutputReader {
    fn spawn<R: Read + Send + 'static>(mut reader: R) -> Self {
        let state = Arc::new((Mutex::new(ReaderState::default()), Condvar::new()));
        let reader_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => {
                        update_reader_state(&reader_state, |state| state.complete = true);
                        break;
                    }
                    Ok(count) => update_reader_state(&reader_state, |state| {
                        let remaining = MAX_CAPTURE_BYTES.saturating_sub(state.captured.len());
                        if remaining > 0 {
                            // Writing to a Vec cannot fail, and extending it
                            // directly avoids exposing an artificial I/O error.
                            state.captured.extend_from_slice(&chunk[..count.min(remaining)]);
                        }
                        state.truncated |= count > remaining;
                    }),
                    Err(error) => {
                        update_reader_state(&reader_state, |state| {
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

    fn finish(&mut self, deadline: Instant, stream: &str) -> Result<(Vec<u8>, bool), CoreError> {
        let complete = self.wait_until(deadline);
        if !complete {
            return Err(CoreError::Command(format!(
                "provider {stream} pipe remained open after process-tree cleanup"
            )));
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| CoreError::Command(format!("provider {stream} reader panicked")))?;
        }
        let (lock, _) = &*self.state;
        let state = match lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(error) = &state.error {
            return Err(CoreError::Command(format!(
                "failed to read provider {stream}: {error}"
            )));
        }
        Ok((state.captured.clone(), state.truncated))
    }
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

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn shell_command(cwd: PathBuf, script: &str, timeout: Duration) -> ProviderCommand {
        ProviderCommand::new(PathBuf::from("/bin/sh"), ["-c", script], cwd, timeout)
    }

    #[test]
    fn timeout_kills_descendants_before_returning() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path().to_path_buf(),
            "(sleep 0.2; printf leaked > leaked.txt) & wait",
            Duration::from_millis(20),
        );

        let error = SystemCommandRunner.run(&command);
        assert!(matches!(error, Err(CoreError::Command(message)) if message.contains("timed out")));
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
        Ok(())
    }

    #[test]
    fn successful_parent_cannot_leave_capture_waiting_on_descendant_pipes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path().to_path_buf(),
            "(sleep 2; printf leaked > leaked.txt) & printf ready",
            Duration::from_secs(1),
        );
        let started = Instant::now();

        let output = SystemCommandRunner.run(&command)?;

        assert!(output.success());
        assert_eq!(output.stdout, "ready");
        assert!(started.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
        assert_eq!(fs::read_dir(directory.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn successful_parent_cannot_leak_a_descendant_that_closed_its_pipes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path().to_path_buf(),
            "(exec 1>&- 2>&-; sleep 0.2; printf leaked > leaked.txt) & printf ready",
            Duration::from_secs(1),
        );

        let output = SystemCommandRunner.run(&command)?;

        assert!(output.success());
        assert_eq!(output.stdout, "ready");
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
        assert_eq!(fs::read_dir(directory.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn output_larger_than_eight_mib_is_bounded_and_marked_truncated() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path().to_path_buf(),
            "/usr/bin/yes x | /usr/bin/head -c 9000000",
            Duration::from_secs(10),
        );

        let output = SystemCommandRunner.run(&command)?;

        assert!(output.success());
        assert_eq!(output.stdout.len(), MAX_CAPTURE_BYTES);
        assert!(output.output_truncated);
        Ok(())
    }
}
