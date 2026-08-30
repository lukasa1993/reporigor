use std::process::Command;

use reporigor_process_tree::{BoundedRunError, BoundedRunStage, CommandLimits};

use crate::{BoundedCommand, CommandEffect, ProviderError};

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
        Self::ensure_read_only(command)?;
        run_probe(command)
    }
}

impl SystemCommandRunner {
    fn ensure_read_only(command: &BoundedCommand) -> Result<(), ProviderError> {
        if command.effect == CommandEffect::ReadOnlyProbe {
            Ok(())
        } else {
            Err(ProviderError::EffectfulCommand(command.display()))
        }
    }
}

fn run_probe(command: &BoundedCommand) -> Result<CommandOutput, ProviderError> {
    let mut process = Command::new(&command.program);
    process.args(&command.args).current_dir(&command.cwd);
    let limits = CommandLimits {
        timeout: command.timeout,
        stdout_bytes: command.output_limit_bytes,
        stderr_bytes: command.output_limit_bytes,
    };
    let output = reporigor_process_tree::run_bounded(&mut process, limits)
        .map_err(|error| provider_command_error(command, &error))?;
    Ok(CommandOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr.bytes).into_owned(),
        output_truncated: output.stdout.truncated || output.stderr.truncated,
    })
}

fn provider_command_error(command: &BoundedCommand, error: &BoundedRunError) -> ProviderError {
    match error.stage() {
        BoundedRunStage::Start => ProviderError::CommandStart {
            program: command.program.clone(),
            source: std::io::Error::other(error.detail()),
        },
        BoundedRunStage::Timeout => ProviderError::CommandTimeout {
            program: command.program.clone(),
            seconds: command.timeout.as_secs_f64(),
        },
        BoundedRunStage::Wait | BoundedRunStage::Output => ProviderError::CommandOutput {
            program: command.program.clone(),
            message: error.detail().to_string(),
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

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

    fn shell_fixture(
        script: &str,
        timeout: Duration,
    ) -> Result<(tempfile::TempDir, BoundedCommand), std::io::Error> {
        let directory = tempdir()?;
        let command = shell_command(directory.path(), script, timeout);
        Ok((directory, command))
    }

    fn assert_successful_parent(
        script: &str,
        maximum_elapsed: Option<Duration>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (directory, command) = shell_fixture(script, Duration::from_secs(1))?;
        let started = Instant::now();
        let output = SystemCommandRunner.run(&command)?;
        assert!(output.success());
        assert_eq!(output.stdout, "ready");
        if let Some(maximum) = maximum_elapsed {
            assert!(started.elapsed() < maximum);
        }
        assert_no_descendant_leak(&directory);
        Ok(())
    }

    fn assert_no_descendant_leak(directory: &tempfile::TempDir) {
        thread::sleep(Duration::from_millis(350));
        assert!(!directory.path().join("leaked.txt").exists());
    }

    #[test]
    fn timeout_kills_descendants_before_collecting_output() -> Result<(), Box<dyn std::error::Error>> {
        let timeout_fixture = shell_fixture(
            "(sleep 0.2; printf leaked > leaked.txt) & wait",
            Duration::from_millis(20),
        )?;
        let (directory, command) = timeout_fixture;

        let result = SystemCommandRunner.run(&command);

        assert!(matches!(result, Err(ProviderError::CommandTimeout { .. })));
        assert_no_descendant_leak(&directory);
        Ok(())
    }

    #[test]
    fn successful_parent_cleans_descendants_with_open_or_closed_pipes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            (
                "(sleep 2; printf leaked > leaked.txt) & printf ready",
                Some(Duration::from_secs(1)),
            ),
            (
                "(exec 1>&- 2>&-; sleep 0.2; printf leaked > leaked.txt) & printf ready",
                None,
            ),
        ];
        for (script, maximum_elapsed) in cases {
            assert_successful_parent(script, maximum_elapsed)?;
        }
        Ok(())
    }
}
