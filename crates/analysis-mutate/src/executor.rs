use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use reporigor_core::{MutationCandidate, MutationResult, MutationStatus};

use crate::command::{run_command_with_environment, MUTANT_FINGERPRINT_ENV, MUTANT_ID_ENV};
use crate::filesystem::{
    acquire_run_lock, acquire_shared_run_lock, canonical_root, pending_mutation_locked,
    recover_active_locked, resolve_source_path, ApplyMutationError, RunLockGuard, SourceRestoreGuard,
};
use crate::{
    run_command, BaselinePhase, BaselineReport, CommandOutcome, CommandSpec, MutationError, MutationMode,
    MutationOptions, MutationRun, PendingMutation, RecoveryAction,
};

const MAX_EXECUTABLE_CANDIDATES: usize = 1_000_000;
const MAX_CANDIDATE_PATH_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct MutationExecutor {
    root: PathBuf,
    options: MutationOptions,
}

/// An exclusive mutation execution session acquired before source analysis.
///
/// The session holds the process-wide state-base lock, performs crash recovery
/// before adapters inspect source, and keeps the lock until it is dropped.
#[derive(Debug)]
pub struct MutationExecutionSession {
    coordination: MutationSessionLock,
    recovery: RecoveryAction,
}

/// A shared coordination session held across read-only source analysis.
///
/// Creating this session may create owner-only coordination state outside the
/// project, but it never changes project or source files. Its shared global
/// lock prevents an exclusive mutation session from applying a transient
/// mutant while an analysis is reading the tree.
#[derive(Debug)]
pub struct MutationReadSession {
    coordination: MutationSessionLock,
}

#[derive(Debug)]
struct MutationSessionLock {
    root: PathBuf,
    lock: RunLockGuard,
}

#[derive(Clone, Copy)]
enum SessionAccess {
    Read,
    Execute,
}

impl MutationSessionLock {
    fn acquire(root: &Path, access: SessionAccess) -> Result<Self, MutationError> {
        let root = canonical_root(root)?;
        let lock = match access {
            SessionAccess::Read => acquire_shared_run_lock(&root),
            SessionAccess::Execute => acquire_run_lock(&root),
        }?;
        Ok(Self { root, lock })
    }
}

impl MutationReadSession {
    /// Acquire the global shared analysis lock before inspecting project files.
    ///
    /// Call [`Self::pending_mutation`] while retaining this value, and keep it
    /// alive until all project analysis and report source reads are complete.
    ///
    /// # Errors
    ///
    /// Returns an error when the root or persistent coordination state is
    /// unsafe, or an exclusive mutation execution currently holds the lock.
    pub fn begin(root: impl AsRef<Path>) -> Result<Self, MutationError> {
        begin_read_session(root.as_ref())
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.coordination.root
    }

    /// Inspect pending crash state while the shared analysis lock is held.
    ///
    /// # Errors
    ///
    /// Returns an error when the external recovery state is malformed or
    /// unsafe. Callers must treat either a returned journal or an error as a
    /// reason not to analyze source.
    pub fn pending_mutation(&self) -> Result<Option<PendingMutation>, MutationError> {
        pending_mutation_locked(&self.coordination.root, &self.coordination.lock.state)
    }
}

fn begin_read_session(root: &Path) -> Result<MutationReadSession, MutationError> {
    let coordination = MutationSessionLock::acquire(root, SessionAccess::Read)?;
    Ok(MutationReadSession { coordination })
}

impl MutationExecutionSession {
    /// Acquire the global mutation lock and recover this root before analysis.
    ///
    /// # Errors
    ///
    /// Returns an error when the root or persistent state is unsafe, another
    /// execution session is active, or recovery cannot be completed safely.
    pub fn begin(root: impl AsRef<Path>) -> Result<Self, MutationError> {
        let coordination = MutationSessionLock::acquire(root.as_ref(), SessionAccess::Execute)?;
        let recovery = recover_active_locked(&coordination.root, &coordination.lock.state)?;
        Ok(Self {
            coordination,
            recovery,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.coordination.root
    }

    #[must_use]
    pub const fn recovery(&self) -> RecoveryAction {
        self.recovery
    }
}

impl MutationExecutor {
    /// Construct an executor rooted in an existing project directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized or the execution
    /// options omit a required command.
    pub fn new(root: impl AsRef<Path>, options: MutationOptions) -> Result<Self, MutationError> {
        let root = canonical_root(root.as_ref())?;
        validate_options(&options)?;
        Ok(Self { root, options })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn options(&self) -> &MutationOptions {
        &self.options
    }

    /// List or execute an inventory of shared mutation candidates.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe or duplicate candidates, an active run in
    /// the same root, failed baseline commands, or any failure to journal,
    /// replace, execute, and restore the target source safely.
    pub fn run(&self, candidates: &[MutationCandidate]) -> Result<MutationRun, MutationError> {
        // Inventory is a genuinely read-only operation: it neither creates a
        // lock/state directory nor performs journal recovery. Recovery can
        // change source bytes and therefore belongs to explicit execution (or
        // the public recover command) only.
        if self.options.mode == MutationMode::List {
            preflight_candidates(&self.root, candidates, &self.options)?;
            return Ok(MutationRun {
                root: self.root.clone(),
                mode: MutationMode::List,
                recovery: RecoveryAction::None,
                baseline: BaselineReport::default(),
                results: candidates
                    .iter()
                    .map(|candidate| {
                        if self.options.ignored_ids.contains(&candidate.id) {
                            static_result(candidate, MutationStatus::Ignored, "candidate excluded by policy")
                        } else if self.options.no_coverage_ids.contains(&candidate.id) {
                            static_result(
                                candidate,
                                MutationStatus::NoCoverage,
                                "coverage data proves the candidate is not exercised",
                            )
                        } else {
                            static_result(
                                candidate,
                                MutationStatus::Pending,
                                "list mode does not execute mutations",
                            )
                        }
                    })
                    .collect(),
            });
        }

        self.options.cancellation.check()?;
        let session = MutationExecutionSession::begin(&self.root)?;
        self.run_in_session(candidates, &session)
    }

    /// Execute candidates under a session acquired before source analysis.
    ///
    /// # Errors
    ///
    /// Returns an error if this is not an execute-mode executor, the session
    /// belongs to another canonical root, candidate preflight fails, or
    /// mutation execution/restoration fails.
    pub fn run_in_session(
        &self,
        candidates: &[MutationCandidate],
        session: &MutationExecutionSession,
    ) -> Result<MutationRun, MutationError> {
        self.validate_session(session)?;
        self.options.cancellation.check()?;
        preflight_candidates(&self.root, candidates, &self.options)?;
        let baseline = self.baseline_for(candidates)?;
        let results = self.execute_candidates(candidates, &session.coordination.lock.state)?;
        Ok(MutationRun {
            root: self.root.clone(),
            mode: self.options.mode,
            recovery: session.recovery(),
            baseline,
            results,
        })
    }

    fn validate_session(&self, session: &MutationExecutionSession) -> Result<(), MutationError> {
        if self.options.mode != MutationMode::Execute {
            return Err(MutationError::InvalidOptions(
                "a pre-analysis mutation session is valid only in execute mode".into(),
            ));
        }
        if session.root() != self.root {
            return Err(MutationError::InvalidOptions(format!(
                "mutation session root {} does not match executor root {}",
                session.root().display(),
                self.root.display()
            )));
        }
        Ok(())
    }

    fn baseline_for(&self, candidates: &[MutationCandidate]) -> Result<BaselineReport, MutationError> {
        if self.options.run_baseline && executable_count(candidates, &self.options) > 0 {
            self.run_baseline()
        } else {
            Ok(BaselineReport::default())
        }
    }

    fn execute_candidates(
        &self,
        candidates: &[MutationCandidate],
        state: &Path,
    ) -> Result<Vec<MutationResult>, MutationError> {
        let mut results = Vec::with_capacity(candidates.len());
        let mut selected = 0_usize;
        for candidate in candidates {
            if let Some(result) = self.nonexecuted_result(candidate, selected) {
                results.push(result);
                continue;
            }
            selected += 1;
            self.options.cancellation.check()?;
            results.push(self.execute_candidate(candidate, state)?);
        }
        Ok(results)
    }

    fn nonexecuted_result(&self, candidate: &MutationCandidate, selected: usize) -> Option<MutationResult> {
        if self.options.ignored_ids.contains(&candidate.id) {
            return Some(static_result(
                candidate,
                MutationStatus::Ignored,
                "candidate excluded by policy",
            ));
        }
        if self.options.no_coverage_ids.contains(&candidate.id) {
            return Some(static_result(
                candidate,
                MutationStatus::NoCoverage,
                "coverage data proves the candidate is not exercised",
            ));
        }
        self.options
            .max_mutants
            .is_some_and(|maximum| selected >= maximum)
            .then(|| {
                static_result(
                    candidate,
                    MutationStatus::Ignored,
                    "candidate exceeds the max-mutants execution limit",
                )
            })
    }

    fn run_baseline(&self) -> Result<BaselineReport, MutationError> {
        self.options.cancellation.check()?;
        let validation = self.run_baseline_validation()?;
        self.options.cancellation.check()?;
        let test = self.run_baseline_test()?;
        Ok(BaselineReport {
            validation,
            test: Some(test),
        })
    }

    fn run_baseline_validation(&self) -> Result<Option<CommandOutcome>, MutationError> {
        let Some(command) = &self.options.validation_command else {
            return Ok(None);
        };
        let outcome = self.run_baseline_command(command)?;
        require_successful_baseline(BaselinePhase::Validation, &outcome)?;
        Ok(Some(outcome))
    }

    fn run_baseline_test(&self) -> Result<CommandOutcome, MutationError> {
        let command =
            self.options.test_command.as_ref().ok_or_else(|| {
                MutationError::InvalidOptions("execute mode requires a test command".into())
            })?;
        let outcome = self.run_baseline_command(command)?;
        require_successful_baseline(BaselinePhase::Test, &outcome)?;
        Ok(outcome)
    }

    fn run_baseline_command(&self, command: &CommandSpec) -> Result<CommandOutcome, MutationError> {
        let outcome = run_command(
            command,
            &self.root,
            self.options.timeout,
            self.options.output_limit_bytes,
            &self.options.cancellation,
        )?;
        Ok(outcome)
    }

    fn execute_candidate(
        &self,
        candidate: &MutationCandidate,
        state: &Path,
    ) -> Result<MutationResult, MutationError> {
        self.options.cancellation.check()?;
        let started = Instant::now();
        let mut guard = match SourceRestoreGuard::apply_bounded(
            &self.root,
            state,
            candidate,
            self.options.max_source_bytes,
        ) {
            Ok(value) => value,
            Err(ApplyMutationError::Invalid(detail)) => {
                return Ok(MutationResult {
                    mutation: candidate.clone(),
                    status: MutationStatus::Invalid,
                    exit_code: None,
                    duration_seconds: started.elapsed().as_secs_f64(),
                    detail: Some(detail),
                });
            }
            Err(ApplyMutationError::Fatal(error)) => return Err(error),
        };
        let result = self.execute_applied_candidate(candidate, started);
        let restoration = guard.restore();
        restoration?;
        result
    }

    fn execute_applied_candidate(
        &self,
        candidate: &MutationCandidate,
        started: Instant,
    ) -> Result<MutationResult, MutationError> {
        match &self.options.validation_command {
            Some(command) => self.run_validation(command, candidate, started),
            None => self.run_test(candidate, started),
        }
    }

    fn run_validation(
        &self,
        command: &CommandSpec,
        candidate: &MutationCandidate,
        started: Instant,
    ) -> Result<MutationResult, MutationError> {
        match self.run_candidate_command(command, candidate) {
            Ok(outcome) => self.classify_validation(candidate, started, &outcome),
            Err(error) => classify_command_error(candidate, started, error),
        }
    }

    fn classify_validation(
        &self,
        candidate: &MutationCandidate,
        started: Instant,
        outcome: &CommandOutcome,
    ) -> Result<MutationResult, MutationError> {
        if outcome.timed_out {
            return Ok(command_failure_result(
                candidate,
                MutationStatus::Timeout,
                outcome,
                started,
                "validation command timed out",
                self.options.output_limit_bytes,
            ));
        }
        if outcome.exit_code != Some(0) {
            return Ok(command_failure_result(
                candidate,
                validation_failure_status(outcome),
                outcome,
                started,
                "validation command failed",
                self.options.output_limit_bytes,
            ));
        }
        self.run_test(candidate, started)
    }

    fn run_test(
        &self,
        candidate: &MutationCandidate,
        started: Instant,
    ) -> Result<MutationResult, MutationError> {
        self.options.cancellation.check()?;
        let Some(command) = &self.options.test_command else {
            return Ok(runtime_error_result(
                candidate,
                started,
                "execute mode has no test command".into(),
            ));
        };
        match self.run_candidate_command(command, candidate) {
            Ok(outcome) => Ok(classify_test_outcome(
                candidate,
                started,
                &outcome,
                self.options.output_limit_bytes,
            )),
            Err(error) => classify_command_error(candidate, started, error),
        }
    }

    fn run_candidate_command(
        &self,
        command: &CommandSpec,
        candidate: &MutationCandidate,
    ) -> Result<CommandOutcome, MutationError> {
        let id = candidate.id.to_string();
        run_command_with_environment(
            command,
            &self.root,
            self.options.timeout,
            self.options.output_limit_bytes,
            &self.options.cancellation,
            &[
                (MUTANT_ID_ENV, id.as_str()),
                (MUTANT_FINGERPRINT_ENV, candidate.fingerprint.as_str()),
            ],
        )
    }
}

fn validation_failure_status(outcome: &CommandOutcome) -> MutationStatus {
    if outcome.exit_code.is_some() {
        MutationStatus::CompileError
    } else {
        MutationStatus::RuntimeError
    }
}

fn command_failure_result(
    candidate: &MutationCandidate,
    status: MutationStatus,
    outcome: &CommandOutcome,
    started: Instant,
    prefix: &str,
    output_limit_bytes: usize,
) -> MutationResult {
    mutation_result(
        candidate,
        status,
        outcome,
        started,
        Some(detail_with_output(prefix, outcome, output_limit_bytes)),
    )
}

fn classify_test_outcome(
    candidate: &MutationCandidate,
    started: Instant,
    outcome: &CommandOutcome,
    output_limit_bytes: usize,
) -> MutationResult {
    if outcome.timed_out {
        return command_failure_result(
            candidate,
            MutationStatus::Timeout,
            outcome,
            started,
            "test command timed out",
            output_limit_bytes,
        );
    }
    match outcome.exit_code {
        Some(0) => mutation_result(candidate, MutationStatus::Survived, outcome, started, None),
        Some(_) => mutation_result(candidate, MutationStatus::Killed, outcome, started, None),
        None => command_failure_result(
            candidate,
            MutationStatus::RuntimeError,
            outcome,
            started,
            "test command terminated without an exit code",
            output_limit_bytes,
        ),
    }
}

fn classify_command_error(
    candidate: &MutationCandidate,
    started: Instant,
    error: MutationError,
) -> Result<MutationResult, MutationError> {
    match error {
        MutationError::Cancelled => Err(MutationError::Cancelled),
        error @ MutationError::ProcessTree { .. } => Err(error),
        error => Ok(runtime_error_result(candidate, started, error.to_string())),
    }
}

fn validate_options(options: &MutationOptions) -> Result<(), MutationError> {
    if options.mode == MutationMode::Execute {
        validate_execute_options(options)?;
    }
    validate_validation_command(options.validation_command.as_ref())
}

fn validate_execute_options(options: &MutationOptions) -> Result<(), MutationError> {
    if options.max_source_bytes == 0
        || options.max_source_bytes > crate::filesystem::MAX_MUTATION_SOURCE_BYTES
    {
        return Err(MutationError::InvalidOptions(format!(
            "execute-mode max_source_bytes must be between 1 and {} bytes",
            crate::filesystem::MAX_MUTATION_SOURCE_BYTES
        )));
    }
    let test = options
        .test_command
        .as_ref()
        .ok_or_else(|| MutationError::InvalidOptions("execute mode requires a test command".into()))?;
    if test.is_empty() {
        return Err(MutationError::InvalidOptions(
            "test command cannot be empty".into(),
        ));
    }
    Ok(())
}

fn validate_validation_command(command: Option<&CommandSpec>) -> Result<(), MutationError> {
    if command.is_some_and(CommandSpec::is_empty) {
        Err(MutationError::InvalidOptions(
            "validation command cannot be empty".into(),
        ))
    } else {
        Ok(())
    }
}

fn preflight_candidates(
    root: &Path,
    candidates: &[MutationCandidate],
    options: &MutationOptions,
) -> Result<(), MutationError> {
    validate_inventory_size(candidates.len())?;
    let mut ids = BTreeSet::new();
    for candidate in candidates {
        validate_candidate_size(candidate)?;
        if !ids.insert(candidate.id) {
            return Err(MutationError::InvalidOptions(format!(
                "duplicate mutation candidate ID {}",
                candidate.id
            )));
        }
        resolve_source_path(root, &candidate.file, false)?;
    }
    validate_selected_ids(&ids, options)
}

fn validate_inventory_size(candidate_count: usize) -> Result<(), MutationError> {
    if candidate_count > MAX_EXECUTABLE_CANDIDATES {
        Err(MutationError::InvalidOptions(format!(
            "mutation inventory contains {candidate_count} candidates, exceeding the executable limit of {MAX_EXECUTABLE_CANDIDATES}"
        )))
    } else {
        Ok(())
    }
}

fn validate_candidate_size(candidate: &MutationCandidate) -> Result<(), MutationError> {
    let text_limit = crate::filesystem::MAX_MUTATION_SOURCE_BYTES;
    if candidate.file.len() > MAX_CANDIDATE_PATH_BYTES
        || candidate.original.len() > text_limit
        || candidate.replacement.len() > text_limit
    {
        Err(MutationError::InvalidOptions(format!(
            "mutation candidate {} exceeds immutable path/text field limits",
            candidate.id
        )))
    } else {
        Ok(())
    }
}

fn validate_selected_ids(ids: &BTreeSet<u64>, options: &MutationOptions) -> Result<(), MutationError> {
    for id in options.ignored_ids.iter().chain(&options.no_coverage_ids) {
        if !ids.contains(id) {
            return Err(MutationError::InvalidOptions(format!(
                "selection references unknown mutation candidate ID {id}"
            )));
        }
    }
    Ok(())
}

fn executable_count(candidates: &[MutationCandidate], options: &MutationOptions) -> usize {
    let available = candidates
        .iter()
        .filter(|candidate| {
            !options.ignored_ids.contains(&candidate.id) && !options.no_coverage_ids.contains(&candidate.id)
        })
        .count();
    options
        .max_mutants
        .map_or(available, |limit| available.min(limit))
}

fn require_successful_baseline(phase: BaselinePhase, outcome: &CommandOutcome) -> Result<(), MutationError> {
    if outcome.success() {
        return Ok(());
    }
    let reason = if outcome.timed_out {
        "command timed out".into()
    } else {
        format!("command exited with {:?}", outcome.exit_code)
    };
    Err(MutationError::BaselineFailed {
        phase,
        reason,
        outcome: Box::new(outcome.clone()),
    })
}

fn static_result(candidate: &MutationCandidate, status: MutationStatus, detail: &str) -> MutationResult {
    MutationResult {
        mutation: candidate.clone(),
        status,
        exit_code: None,
        duration_seconds: 0.0,
        detail: Some(detail.into()),
    }
}

fn mutation_result(
    candidate: &MutationCandidate,
    status: MutationStatus,
    outcome: &CommandOutcome,
    started: Instant,
    detail: Option<String>,
) -> MutationResult {
    MutationResult {
        mutation: candidate.clone(),
        status,
        exit_code: outcome.exit_code,
        duration_seconds: started.elapsed().as_secs_f64(),
        detail,
    }
}

fn runtime_error_result(candidate: &MutationCandidate, started: Instant, detail: String) -> MutationResult {
    MutationResult {
        mutation: candidate.clone(),
        status: MutationStatus::RuntimeError,
        exit_code: None,
        duration_seconds: started.elapsed().as_secs_f64(),
        detail: Some(detail),
    }
}

fn detail_with_output(prefix: &str, outcome: &CommandOutcome, limit: usize) -> String {
    if outcome.output.is_empty() {
        return prefix.into();
    }
    if limit == 0 {
        return prefix.into();
    }
    let separator = ": ";
    let reserved = prefix.len().saturating_add(separator.len());
    if reserved >= limit {
        return prefix.into();
    }
    let available = limit - reserved;
    let mut start = outcome.output.len().saturating_sub(available);
    while !outcome.output.is_char_boundary(start) {
        start += 1;
    }
    format!("{prefix}{separator}{}", &outcome.output[start..])
}

/// Recover an interrupted mutation under the same per-root run lock used by execution.
///
/// # Errors
///
/// Returns an error for an unsafe or invalid journal, a concurrent run, a
/// source changed independently since the crash, or an I/O failure while
/// restoring the original bytes.
pub fn recover_active(root: impl AsRef<Path>) -> Result<RecoveryAction, MutationError> {
    let root = canonical_root(root.as_ref())?;
    let lock = acquire_run_lock(&root)?;
    recover_active_locked(&root, &lock.state)
}

#[cfg(test)]
mod tests {
    use super::{detail_with_output, validate_execute_options};
    use crate::{CommandOutcome, CommandSpec, MutationError, MutationOptions};

    struct DetailCase {
        detail: &'static str,
        captured: &'static str,
        limit: usize,
        expected: &'static str,
    }

    impl DetailCase {
        const fn new(
            detail: &'static str,
            captured: &'static str,
            limit: usize,
            expected: &'static str,
        ) -> Self {
            Self {
                detail,
                captured,
                limit,
                expected,
            }
        }
    }

    #[test]
    fn executor_helper_contracts_cover_diagnostics_and_option_boundaries() {
        let output = |value: &str| CommandOutcome {
            exit_code: Some(1),
            timed_out: false,
            duration_seconds: 0.0,
            output: value.into(),
            output_truncated: false,
        };
        let cases = [
            DetailCase::new("failure", "", 64, "failure"),
            DetailCase::new("failure", "detail", 0, "failure"),
            DetailCase::new("failure", "detail", 9, "failure"),
            DetailCase::new("x", "éz", 5, "x: z"),
            DetailCase::new("x", "ok", 16, "x: ok"),
        ];
        for case in cases {
            assert_eq!(
                detail_with_output(case.detail, &output(case.captured), case.limit),
                case.expected
            );
        }

        let valid = MutationOptions::execute("test");
        assert!(validate_execute_options(&valid).is_ok());

        let mut invalid_limit = valid.clone();
        invalid_limit.max_source_bytes = 0;
        assert!(matches!(
            validate_execute_options(&invalid_limit),
            Err(MutationError::InvalidOptions(_))
        ));
        invalid_limit.max_source_bytes = crate::filesystem::MAX_MUTATION_SOURCE_BYTES + 1;
        assert!(validate_execute_options(&invalid_limit).is_err());

        let mut missing = valid.clone();
        missing.test_command = None;
        assert!(validate_execute_options(&missing).is_err());

        let mut empty = valid;
        empty.test_command = Some(CommandSpec::shell("  "));
        assert!(validate_execute_options(&empty).is_err());
    }
}
