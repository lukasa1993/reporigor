use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reporigor_process_tree::{CleanupPolicy, ProcessTree, WaitReason};

use crate::{BoundedCommand, CommandEffect, ProviderError};

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Bounded output returned by an injected provider probe runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
}

impl CommandOutput {
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }
}

/// Injectable execution boundary used exclusively by explicit preflight.
pub trait CommandRunner: Send + Sync {
    /// Run a bounded read-only command.
    ///
    /// # Errors
    ///
    /// Returns an error for effectful commands, startup failures, timeouts, or
    /// output collection failures.
    fn run(&self, command: &BoundedCommand) -> Result<CommandOutput, ProviderError>;
}

/// Timeout-aware process runner which rejects mutation commands by policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &BoundedCommand) -> Result<CommandOutput, ProviderError> {
        if command.effect != CommandEffect::ReadOnlyProbe {
            return Err(ProviderError::EffectfulCommand(command.display()));
        }

        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .current_dir(&command.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ProcessTree::spawn(&mut process).map_err(|error| ProviderError::CommandStart {
            program: command.program.clone(),
            source: std::io::Error::other(error),
        })?;
        let stdout = child.take_stdout().ok_or_else(|| ProviderError::CommandOutput {
            program: command.program.clone(),
            message: "stdout pipe is unavailable".to_owned(),
        })?;
        let stderr = child.take_stderr().ok_or_else(|| ProviderError::CommandOutput {
            program: command.program.clone(),
            message: "stderr pipe is unavailable".to_owned(),
        })?;
        let limit = command.output_limit_bytes;
        let mut stdout_reader = OutputReader::spawn(stdout, limit);
        let mut stderr_reader = OutputReader::spawn(stderr, limit);

        let wait_result = child.wait_bounded(command.timeout, CleanupPolicy::default());
        let deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
        // Finish both workers even when one pipe reports an error. Otherwise
        // the second reader would be detached while still owning its pipe.
        let stdout_result = stdout_reader.finish(deadline, &command.program, "stdout");
        let stderr_result = stderr_reader.finish(deadline, &command.program, "stderr");
        let outcome = wait_result.map_err(|error| ProviderError::CommandOutput {
            program: command.program.clone(),
            message: format!("failed while waiting for contained command: {error}"),
        })?;
        if outcome.reason == WaitReason::TimedOut {
            return Err(ProviderError::CommandTimeout {
                program: command.program.clone(),
                seconds: command.timeout.as_secs_f64(),
            });
        }
        let (stdout, stdout_truncated) = stdout_result?;
        let (stderr, stderr_truncated) = stderr_result?;
        Ok(CommandOutput {
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
    fn spawn<R: Read + Send + 'static>(mut reader: R, limit: usize) -> Self {
        let state = Arc::new((
            Mutex::new(ReaderState {
                captured: Vec::with_capacity(limit.min(64 * 1024)),
                ..ReaderState::default()
            }),
            Condvar::new(),
        ));
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
                        let remaining = limit.saturating_sub(state.captured.len());
                        let kept = count.min(remaining);
                        state.captured.extend_from_slice(&chunk[..kept]);
                        state.truncated |= kept < count;
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

    fn finish(
        &mut self,
        deadline: Instant,
        program: &std::path::Path,
        stream: &str,
    ) -> Result<(Vec<u8>, bool), ProviderError> {
        let complete = self.wait_until(deadline);
        if complete {
            if let Some(thread) = self.thread.take() {
                thread.join().map_err(|_| ProviderError::CommandOutput {
                    program: program.to_path_buf(),
                    message: format!("{stream} reader panicked"),
                })?;
            }
        }
        let (lock, _) = &*self.state;
        let state = match lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(error) = &state.error {
            return Err(ProviderError::CommandOutput {
                program: program.to_path_buf(),
                message: format!("failed to read {stream}: {error}"),
            });
        }
        Ok((state.captured.clone(), state.truncated || !complete))
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
    use tempfile::tempdir;

    use super::*;

    fn shell_command(cwd: &std::path::Path, script: &str, timeout: Duration) -> BoundedCommand {
        BoundedCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: cwd.to_path_buf(),
            timeout,
            output_limit_bytes: 4096,
            effect: CommandEffect::ReadOnlyProbe,
        }
    }

    #[test]
    fn timeout_kills_descendants_before_collecting_output() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path(),
            "(sleep 0.2; printf leaked > leaked.txt) & wait",
            Duration::from_millis(20),
        );

        let result = SystemCommandRunner.run(&command);

        assert!(matches!(result, Err(ProviderError::CommandTimeout { .. })));
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
        Ok(())
    }

    #[test]
    fn successful_parent_does_not_wait_for_descendant_owned_pipes() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path(),
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
        Ok(())
    }

    #[test]
    fn successful_parent_cannot_leak_a_descendant_that_closed_its_pipes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let command = shell_command(
            directory.path(),
            "(exec 1>&- 2>&-; sleep 0.2; printf leaked > leaked.txt) & printf ready",
            Duration::from_secs(1),
        );

        let output = SystemCommandRunner.run(&command)?;

        assert!(output.success());
        assert_eq!(output.stdout, "ready");
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
        Ok(())
    }
}
