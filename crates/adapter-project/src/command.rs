use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use provider_mutation::{BoundedCommand, CommandEffect};
use reporigor_core::CoreError;

const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

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

pub type ProviderCommandOutput = provider_mutation::CommandOutput;

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
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        let bounded = BoundedCommand {
            program: command.program.clone(),
            args: command.args.clone(),
            cwd: command.cwd.clone(),
            timeout: command.timeout,
            output_limit_bytes: MAX_CAPTURE_BYTES,
            effect: CommandEffect::ReadOnlyProbe,
        };
        let output = provider_mutation::CommandRunner::run(&provider_mutation::SystemCommandRunner, &bounded)
            .map_err(|error| CoreError::Command(error.to_string()))?;
        Ok(ProviderCommandOutput {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
            output_truncated: output.output_truncated,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn delegates_read_only_probe_execution_to_the_shared_runner() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let command = ProviderCommand::new(
            PathBuf::from("/bin/sh"),
            ["-c", "printf project-out; printf project-err >&2"],
            directory.path().to_path_buf(),
            Duration::from_secs(1),
        );
        let output = SystemCommandRunner
            .run(&command)
            .unwrap_or_else(|error| panic!("bounded command: {error}"));

        assert!(output.success());
        assert_eq!(
            (output.stdout.as_str(), output.stderr.as_str()),
            ("project-out", "project-err")
        );
        assert!(!output.output_truncated);
    }
}
