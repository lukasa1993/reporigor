use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use reporigor_core::{MutationResult, MutationStatus};
use serde::{Deserialize, Serialize};

use crate::CancellationToken;

/// How the built-in mutation engine should treat the supplied inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationMode {
    /// Return the inventory as pending without changing source files or running commands.
    #[default]
    List,
    /// Apply each selected mutation and run validation/tests.
    Execute,
}

/// A command that can either be invoked directly or through the platform shell.
///
/// Direct commands avoid shell parsing and are preferred for programmatic callers.
/// Shell commands exist for compatibility with project configuration such as
/// `cargo test --workspace && cargo test --doc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CommandSpec {
    Shell {
        command: String,
    },
    Program {
        program: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl CommandSpec {
    #[must_use]
    pub fn shell(command: impl Into<String>) -> Self {
        Self::Shell {
            command: command.into(),
        }
    }

    #[must_use]
    pub fn program(program: impl Into<PathBuf>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Program {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Shell { command } => command.trim().is_empty(),
            Self::Program { program, .. } => program.as_os_str().is_empty(),
        }
    }

    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Shell { command } => command.clone(),
            Self::Program { program, args } => {
                let mut rendered = program.display().to_string();
                for argument in args {
                    rendered.push(' ');
                    rendered.push_str(argument);
                }
                rendered
            }
        }
    }
}

impl From<String> for CommandSpec {
    fn from(command: String) -> Self {
        Self::shell(command)
    }
}

impl From<&str> for CommandSpec {
    fn from(command: &str) -> Self {
        Self::shell(command)
    }
}

/// Execution policy for a mutation run.
#[derive(Debug, Clone)]
pub struct MutationOptions {
    pub mode: MutationMode,
    pub test_command: Option<CommandSpec>,
    pub validation_command: Option<CommandSpec>,
    pub timeout: Duration,
    pub run_baseline: bool,
    pub max_mutants: Option<usize>,
    pub output_limit_bytes: usize,
    /// Maximum source bytes re-read immediately before an executable edit.
    pub max_source_bytes: usize,
    /// Cooperative cancellation shared with the caller or CLI signal handler.
    pub cancellation: CancellationToken,
    /// Candidate IDs that a coverage provider proved are not exercised.
    pub no_coverage_ids: BTreeSet<u64>,
    /// Candidate IDs excluded by a provider or user policy.
    pub ignored_ids: BTreeSet<u64>,
}

impl MutationOptions {
    #[must_use]
    pub fn list() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn execute(test_command: impl Into<CommandSpec>) -> Self {
        Self {
            mode: MutationMode::Execute,
            test_command: Some(test_command.into()),
            ..Self::default()
        }
    }
}

impl Default for MutationOptions {
    fn default() -> Self {
        Self {
            mode: MutationMode::List,
            test_command: None,
            validation_command: None,
            timeout: Duration::from_secs(120),
            run_baseline: true,
            max_mutants: None,
            output_limit_bytes: 2 * 1024 * 1024,
            max_source_bytes: 8 * 1024 * 1024,
            cancellation: CancellationToken::new(),
            no_coverage_ids: BTreeSet::new(),
            ignored_ids: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryAction {
    #[default]
    None,
    /// A journal existed, but the file already contained its original bytes.
    AlreadyClean,
    /// A mutation left by an interrupted run was restored.
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaselinePhase {
    Validation,
    Test,
}

impl std::fmt::Display for BaselinePhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation => formatter.write_str("validation"),
            Self::Test => formatter.write_str("test"),
        }
    }
}

/// Bounded output and termination information for one child command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    #[serde(default, skip_serializing)]
    pub duration_seconds: f64,
    #[serde(default, skip_serializing)]
    pub output: String,
    #[serde(default, skip_serializing)]
    pub output_truncated: bool,
}

impl CommandOutcome {
    #[must_use]
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BaselineReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<CommandOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<CommandOutcome>,
}

/// Language-neutral result of inventory listing or mutation execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationRun {
    pub root: PathBuf,
    pub mode: MutationMode,
    pub recovery: RecoveryAction,
    pub baseline: BaselineReport,
    pub results: Vec<MutationResult>,
}

impl MutationRun {
    #[must_use]
    pub fn summary(&self) -> MutationSummary {
        MutationSummary::from_results(&self.results)
    }
}

/// Counts use the names from the Mutation Testing Elements status vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MutationSummary {
    pub total: usize,
    pub killed: usize,
    pub survived: usize,
    pub no_coverage: usize,
    pub compile_error: usize,
    pub runtime_error: usize,
    pub timeout: usize,
    pub invalid: usize,
    pub ignored: usize,
    pub pending: usize,
}

impl MutationSummary {
    #[must_use]
    pub fn from_results(results: &[MutationResult]) -> Self {
        let mut summary = Self::default();
        for result in results {
            summary.record(result.status);
        }
        summary.total = results.len();
        summary
    }

    fn record(&mut self, status: MutationStatus) {
        if self.record_outcome(status) || self.record_execution_error(status) {
            return;
        }
        self.record_selection(status);
    }

    fn record_outcome(&mut self, status: MutationStatus) -> bool {
        match status {
            MutationStatus::Killed => self.killed += 1,
            MutationStatus::Survived => self.survived += 1,
            MutationStatus::NoCoverage => self.no_coverage += 1,
            _ => return false,
        }
        true
    }

    fn record_execution_error(&mut self, status: MutationStatus) -> bool {
        let counter = if status == MutationStatus::CompileError {
            &mut self.compile_error
        } else if status == MutationStatus::RuntimeError {
            &mut self.runtime_error
        } else if status == MutationStatus::Timeout {
            &mut self.timeout
        } else {
            return false;
        };
        *counter += 1;
        true
    }

    fn record_selection(&mut self, status: MutationStatus) {
        let counter = match status {
            MutationStatus::Invalid => &mut self.invalid,
            MutationStatus::Ignored => &mut self.ignored,
            MutationStatus::Pending => &mut self.pending,
            _ => {
                debug_assert!(false, "status was handled by an earlier category");
                return;
            }
        };
        *counter += 1;
    }

    /// Return the mutation-score denominator. Only killed and surviving
    /// mutants are scoreable; no other status is inferred to be equivalent.
    #[must_use]
    pub const fn scoreable_mutants(self) -> usize {
        self.killed.saturating_add(self.survived)
    }

    /// Return `RepoRigor`'s mutation score as a unit-interval fraction.
    ///
    /// The denominator is exactly killed plus survived. Every operational,
    /// invalid, ignored, pending, and uncovered status is non-scoreable.
    #[must_use]
    pub fn mutation_score(self) -> Option<f64> {
        score_ratio(self.killed, self.scoreable_mutants())
    }
}

/// Calculate the shared mutation score for one native result inventory.
#[must_use]
pub fn mutation_score(results: &[MutationResult]) -> Option<f64> {
    let killed = results
        .iter()
        .filter(|result| result.status == MutationStatus::Killed)
        .count();
    let scoreable = results
        .iter()
        .filter(|result| is_scoreable_status(result.status))
        .count();
    score_ratio(killed, scoreable)
}

/// Whether a mutation outcome participates in the mutation-score denominator.
#[must_use]
pub const fn is_scoreable_status(status: MutationStatus) -> bool {
    matches!(status, MutationStatus::Killed | MutationStatus::Survived)
}

fn score_ratio(killed: usize, scoreable: usize) -> Option<f64> {
    if scoreable == 0 {
        return None;
    }
    let killed = u32::try_from(killed).unwrap_or(u32::MAX);
    let scoreable = u32::try_from(scoreable).unwrap_or(u32::MAX);
    Some(f64::from(killed) / f64::from(scoreable))
}

#[cfg(test)]
mod tests {
    use reporigor_core::{MutationResult, MutationStatus};

    use super::{is_scoreable_status, mutation_score, CommandSpec, MutationSummary};
    use crate::test_support::{candidate, COMPARISON_TEXT};

    #[test]
    fn mutation_summary_and_score_policy_is_exact() {
        let result = |status| MutationResult {
            mutation: candidate(1, "src/lib.rs", "comparison", "fixture", COMPARISON_TEXT),
            status,
            exit_code: None,
            duration_seconds: 0.0,
            detail: None,
        };
        assert!(is_scoreable_status(MutationStatus::Killed));
        assert!(is_scoreable_status(MutationStatus::Survived));
        for status in MutationStatus::ALL.into_iter().skip(2) {
            assert!(!is_scoreable_status(status));
        }

        let summary = MutationSummary {
            killed: 3,
            survived: 2,
            no_coverage: 7,
            timeout: 11,
            ..MutationSummary::default()
        };
        assert_eq!(summary.scoreable_mutants(), 5);
        assert_eq!(summary.mutation_score(), Some(0.6));
        assert_eq!(MutationSummary::default().mutation_score(), None);

        assert_eq!(CommandSpec::shell("cargo test").display(), "cargo test");
        assert_eq!(
            CommandSpec::program("cargo", ["test", "--workspace"]).display(),
            "cargo test --workspace"
        );

        let results = [
            result(MutationStatus::Killed),
            result(MutationStatus::Survived),
            result(MutationStatus::NoCoverage),
            result(MutationStatus::CompileError),
            result(MutationStatus::RuntimeError),
            result(MutationStatus::Timeout),
            result(MutationStatus::Invalid),
            result(MutationStatus::Ignored),
            result(MutationStatus::Pending),
        ];
        let summary = MutationSummary::from_results(&results);
        assert_eq!(
            (
                summary.total,
                (summary.killed, summary.survived, summary.no_coverage),
                (summary.compile_error, summary.runtime_error, summary.timeout),
                (summary.invalid, summary.ignored, summary.pending),
            ),
            (9, (1, 1, 1), (1, 1, 1), (1, 1, 1))
        );
        assert_eq!(summary.mutation_score(), Some(0.5));
        assert_eq!(mutation_score(&results), summary.mutation_score());
    }
}
