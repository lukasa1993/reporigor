use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use reporigor_core::{MutationCandidate, MutationResult, MutationStatus};

use crate::filesystem::{
    canonical_root, pending_mutation_locked, recover_active_locked, resolve_source_path, ApplyMutationError,
    RunLockGuard, SourceRestoreGuard,
};
use crate::{
    run_command, BaselinePhase, BaselineReport, CommandOutcome, MutationError, MutationMode, MutationOptions,
    MutationRun, PendingMutation, RecoveryAction,
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
    root: PathBuf,
    recovery: RecoveryAction,
    lock: RunLockGuard,
}

/// A shared coordination session held across read-only source analysis.
///
/// Creating this session may create owner-only coordination state outside the
/// project, but it never changes project or source files. Its shared global
/// lock prevents an exclusive mutation session from applying a transient
/// mutant while an analysis is reading the tree.
#[derive(Debug)]
pub struct MutationReadSession {
    root: PathBuf,
    lock: RunLockGuard,
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
        let root = canonical_root(root.as_ref())?;
        let lock = RunLockGuard::acquire_shared(&root)?;
        Ok(Self { root, lock })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Inspect pending crash state while the shared analysis lock is held.
    ///
    /// # Errors
    ///
    /// Returns an error when the external recovery state is malformed or
    /// unsafe. Callers must treat either a returned journal or an error as a
    /// reason not to analyze source.
    pub fn pending_mutation(&self) -> Result<Option<PendingMutation>, MutationError> {
        pending_mutation_locked(&self.root, self.lock.state())
    }
}

impl MutationExecutionSession {
    /// Acquire the global mutation lock and recover this root before analysis.
    ///
    /// # Errors
    ///
    /// Returns an error when the root or persistent state is unsafe, another
    /// execution session is active, or recovery cannot be completed safely.
    pub fn begin(root: impl AsRef<Path>) -> Result<Self, MutationError> {
        let root = canonical_root(root.as_ref())?;
        let lock = RunLockGuard::acquire(&root)?;
        let recovery = recover_active_locked(&root, lock.state())?;
        Ok(Self { root, recovery, lock })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
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
        if self.options.mode != MutationMode::Execute {
            return Err(MutationError::InvalidOptions(
                "a pre-analysis mutation session is valid only in execute mode".into(),
            ));
        }
        if session.root != self.root {
            return Err(MutationError::InvalidOptions(format!(
                "mutation session root {} does not match executor root {}",
                session.root.display(),
                self.root.display()
            )));
        }
        self.options.cancellation.check()?;
        preflight_candidates(&self.root, candidates, &self.options)?;

        let executable_count = executable_count(candidates, &self.options);
        let baseline = if self.options.mode == MutationMode::Execute
            && self.options.run_baseline
            && executable_count > 0
        {
            self.run_baseline()?
        } else {
            BaselineReport::default()
        };

        let mut results = Vec::with_capacity(candidates.len());
        let mut selected = 0_usize;
        for candidate in candidates {
            if self.options.ignored_ids.contains(&candidate.id) {
                results.push(static_result(
                    candidate,
                    MutationStatus::Ignored,
                    "candidate excluded by policy",
                ));
                continue;
            }
            if self.options.no_coverage_ids.contains(&candidate.id) {
                results.push(static_result(
                    candidate,
                    MutationStatus::NoCoverage,
                    "coverage data proves the candidate is not exercised",
                ));
                continue;
            }
            if self
                .options
                .max_mutants
                .is_some_and(|maximum| selected >= maximum)
            {
                results.push(static_result(
                    candidate,
                    MutationStatus::Ignored,
                    "candidate exceeds the max-mutants execution limit",
                ));
                continue;
            }
            selected += 1;
            self.options.cancellation.check()?;
            results.push(self.execute_candidate(candidate, session.lock.state())?);
        }

        Ok(MutationRun {
            root: self.root.clone(),
            mode: self.options.mode,
            recovery: session.recovery,
            baseline,
            results,
        })
    }

    fn run_baseline(&self) -> Result<BaselineReport, MutationError> {
        self.options.cancellation.check()?;
        let mut report = BaselineReport::default();
        if let Some(command) = &self.options.validation_command {
            let outcome = run_command(
                command,
                &self.root,
                self.options.timeout,
                self.options.output_limit_bytes,
                &self.options.cancellation,
            )?;
            require_successful_baseline(BaselinePhase::Validation, &outcome)?;
            report.validation = Some(outcome);
        }
        self.options.cancellation.check()?;
        let command =
            self.options.test_command.as_ref().ok_or_else(|| {
                MutationError::InvalidOptions("execute mode requires a test command".into())
            })?;
        let outcome = run_command(
            command,
            &self.root,
            self.options.timeout,
            self.options.output_limit_bytes,
            &self.options.cancellation,
        )?;
        require_successful_baseline(BaselinePhase::Test, &outcome)?;
        report.test = Some(outcome);
        Ok(report)
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

        let result: Result<MutationResult, MutationError> =
            if let Some(command) = &self.options.validation_command {
                match run_command(
                    command,
                    &self.root,
                    self.options.timeout,
                    self.options.output_limit_bytes,
                    &self.options.cancellation,
                ) {
                    Ok(outcome) if outcome.timed_out => Ok(mutation_result(
                        candidate,
                        MutationStatus::Timeout,
                        &outcome,
                        started,
                        Some(detail_with_output(
                            "validation command timed out",
                            &outcome,
                            self.options.output_limit_bytes,
                        )),
                    )),
                    Ok(outcome) if outcome.exit_code != Some(0) => {
                        let status = if outcome.exit_code.is_some() {
                            MutationStatus::CompileError
                        } else {
                            MutationStatus::RuntimeError
                        };
                        Ok(mutation_result(
                            candidate,
                            status,
                            &outcome,
                            started,
                            Some(detail_with_output(
                                "validation command failed",
                                &outcome,
                                self.options.output_limit_bytes,
                            )),
                        ))
                    }
                    Ok(_) => self.run_test(candidate, started),
                    Err(MutationError::Cancelled) => Err(MutationError::Cancelled),
                    Err(error @ MutationError::ProcessTree { .. }) => Err(error),
                    Err(error) => Ok(runtime_error_result(candidate, started, error.to_string())),
                }
            } else {
                self.run_test(candidate, started)
            };

        let restoration = guard.restore();
        restoration?;
        result
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
        match run_command(
            command,
            &self.root,
            self.options.timeout,
            self.options.output_limit_bytes,
            &self.options.cancellation,
        ) {
            Ok(outcome) if outcome.timed_out => Ok(mutation_result(
                candidate,
                MutationStatus::Timeout,
                &outcome,
                started,
                Some(detail_with_output(
                    "test command timed out",
                    &outcome,
                    self.options.output_limit_bytes,
                )),
            )),
            Ok(outcome) if outcome.exit_code == Some(0) => Ok(mutation_result(
                candidate,
                MutationStatus::Survived,
                &outcome,
                started,
                None,
            )),
            Ok(outcome) if outcome.exit_code.is_some() => Ok(mutation_result(
                candidate,
                MutationStatus::Killed,
                &outcome,
                started,
                None,
            )),
            Ok(outcome) => Ok(mutation_result(
                candidate,
                MutationStatus::RuntimeError,
                &outcome,
                started,
                Some(detail_with_output(
                    "test command terminated without an exit code",
                    &outcome,
                    self.options.output_limit_bytes,
                )),
            )),
            Err(MutationError::Cancelled) => Err(MutationError::Cancelled),
            Err(error @ MutationError::ProcessTree { .. }) => Err(error),
            Err(error) => Ok(runtime_error_result(candidate, started, error.to_string())),
        }
    }
}

fn validate_options(options: &MutationOptions) -> Result<(), MutationError> {
    if options.mode == MutationMode::Execute {
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
    }
    if options
        .validation_command
        .as_ref()
        .is_some_and(crate::CommandSpec::is_empty)
    {
        return Err(MutationError::InvalidOptions(
            "validation command cannot be empty".into(),
        ));
    }
    Ok(())
}

fn preflight_candidates(
    root: &Path,
    candidates: &[MutationCandidate],
    options: &MutationOptions,
) -> Result<(), MutationError> {
    if candidates.len() > MAX_EXECUTABLE_CANDIDATES {
        return Err(MutationError::InvalidOptions(format!(
            "mutation inventory contains {} candidates, exceeding the executable limit of {MAX_EXECUTABLE_CANDIDATES}",
            candidates.len()
        )));
    }
    let mut ids = BTreeSet::new();
    for candidate in candidates {
        if candidate.file.len() > MAX_CANDIDATE_PATH_BYTES
            || candidate.original.len() > crate::filesystem::MAX_MUTATION_SOURCE_BYTES
            || candidate.replacement.len() > crate::filesystem::MAX_MUTATION_SOURCE_BYTES
        {
            return Err(MutationError::InvalidOptions(format!(
                "mutation candidate {} exceeds immutable path/text field limits",
                candidate.id
            )));
        }
        if !ids.insert(candidate.id) {
            return Err(MutationError::InvalidOptions(format!(
                "duplicate mutation candidate ID {}",
                candidate.id
            )));
        }
        resolve_source_path(root, &candidate.file, false)?;
    }
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
    let lock = RunLockGuard::acquire(&root)?;
    recover_active_locked(&root, lock.state())
}
