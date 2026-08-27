#![doc = include_str!("../README.md")]

use std::error::Error;
use std::fmt;
use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

#[cfg(not(any(unix, windows)))]
compile_error!("reporigor-process-tree supports Unix and Windows targets");

const MINIMUM_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Controls the bounded graceful and forced phases of process-tree cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupPolicy {
    /// Time allowed after the platform's graceful tree signal.
    pub graceful_timeout: Duration,
    /// Time allowed for forced termination, leader reaping, and tree quiescence.
    pub kill_timeout: Duration,
    /// Maximum interval between non-blocking process-state observations.
    pub poll_interval: Duration,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            graceful_timeout: Duration::from_millis(100),
            kill_timeout: Duration::from_secs(1),
            poll_interval: Duration::from_millis(10),
        }
    }
}

impl CleanupPolicy {
    fn effective_poll_interval(self) -> Duration {
        self.poll_interval.max(MINIMUM_POLL_INTERVAL)
    }
}

/// Additional platform spawn settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpawnOptions {
    /// Extra Windows process-creation flags.
    ///
    /// The value is ignored on Unix. On Windows, the required
    /// `CREATE_SUSPENDED` and `CREATE_NEW_PROCESS_GROUP` flags are always added.
    pub windows_creation_flags: u32,
}

/// Stage at which contained process creation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnStage {
    CreateContainment,
    ConfigureContainment,
    SpawnProcess,
    AssignProcess,
    LocatePrimaryThread,
    OpenPrimaryThread,
    ResumePrimaryThread,
}

impl fmt::Display for SpawnStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::CreateContainment => "create process containment",
            Self::ConfigureContainment => "configure process containment",
            Self::SpawnProcess => "spawn process",
            Self::AssignProcess => "assign process to containment",
            Self::LocatePrimaryThread => "locate suspended primary thread",
            Self::OpenPrimaryThread => "open suspended primary thread",
            Self::ResumePrimaryThread => "resume suspended primary thread",
        };
        formatter.write_str(label)
    }
}

/// A failure while creating a contained process.
#[derive(Debug)]
pub struct SpawnError {
    stage: SpawnStage,
    source: io::Error,
    cleanup_issues: Vec<CleanupIssue>,
}

impl SpawnError {
    fn new(stage: SpawnStage, source: io::Error) -> Self {
        Self {
            stage,
            source,
            cleanup_issues: Vec::new(),
        }
    }

    /// The failed creation stage.
    #[must_use]
    pub const fn stage(&self) -> SpawnStage {
        self.stage
    }

    /// Cleanup problems encountered while aborting a partially created child.
    #[must_use]
    pub fn cleanup_issues(&self) -> &[CleanupIssue] {
        &self.cleanup_issues
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {}: {}", self.stage, self.source)?;
        if !self.cleanup_issues.is_empty() {
            write!(
                formatter,
                "; {} cleanup issue(s) followed",
                self.cleanup_issues.len()
            )?;
        }
        Ok(())
    }
}

impl Error for SpawnError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct PlatformSpawnError {
    stage: SpawnStage,
    source: io::Error,
    cleanup_issues: Vec<CleanupIssue>,
}

impl From<PlatformSpawnError> for SpawnError {
    fn from(error: PlatformSpawnError) -> Self {
        Self {
            stage: error.stage,
            source: error.source,
            cleanup_issues: error.cleanup_issues,
        }
    }
}

/// Cleanup operation that encountered an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupStage {
    SendGraceful,
    SendForce,
    KillLeader,
    ObserveLeader,
    ReapLeader,
    VerifyTree,
}

impl fmt::Display for CleanupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::SendGraceful => "send graceful tree termination",
            Self::SendForce => "send forced tree termination",
            Self::KillLeader => "terminate the process leader",
            Self::ObserveLeader => "observe process-leader exit",
            Self::ReapLeader => "reap the process leader",
            Self::VerifyTree => "verify process-tree cleanup",
        };
        formatter.write_str(label)
    }
}

/// One concrete problem found while cleaning a process tree.
#[derive(Debug)]
pub struct CleanupIssue {
    stage: CleanupStage,
    source: io::Error,
}

impl CleanupIssue {
    fn new(stage: CleanupStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    fn timed_out(stage: CleanupStage, duration: Duration, subject: &str) -> Self {
        Self::new(
            stage,
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{subject} did not finish within {duration:.3?}"),
            ),
        )
    }

    /// Operation that failed.
    #[must_use]
    pub const fn stage(&self) -> CleanupStage {
        self.stage
    }

    /// Underlying operating-system or timeout error.
    #[must_use]
    pub const fn source_error(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for CleanupIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {}: {}", self.stage, self.source)
    }
}

/// What bounded cleanup accomplished.
#[derive(Clone, Debug)]
pub struct TerminationReport {
    /// Reaped leader status, when the leader became observable within the bound.
    pub status: Option<ExitStatus>,
    /// Whether a graceful tree-termination request was made.
    pub graceful_requested: bool,
    /// Whether a forced tree-termination request was made.
    pub force_requested: bool,
    /// Whether the operating system confirmed that the containment is empty.
    pub tree_confirmed_gone: bool,
}

impl TerminationReport {
    fn incomplete() -> Self {
        Self {
            status: None,
            graceful_requested: false,
            force_requested: false,
            tree_confirmed_gone: false,
        }
    }
}

/// One or more failures encountered during bounded tree cleanup.
#[derive(Debug)]
pub struct CleanupError {
    report: TerminationReport,
    issues: Vec<CleanupIssue>,
}

impl CleanupError {
    /// Partial cleanup report.
    #[must_use]
    pub const fn report(&self) -> &TerminationReport {
        &self.report
    }

    /// All cleanup problems, in the order they occurred.
    #[must_use]
    pub fn issues(&self) -> &[CleanupIssue] {
        &self.issues
    }
}

impl fmt::Display for CleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "process-tree cleanup had {} issue(s)",
            self.issues.len()
        )?;
        for issue in &self.issues {
            write!(formatter, "; {issue}")?;
        }
        Ok(())
    }
}

impl Error for CleanupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.issues
            .first()
            .map(|issue| &issue.source as &(dyn Error + 'static))
    }
}

/// Why a bounded wait completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    Exited,
    TimedOut,
}

/// Completed child status, exposed only after descendant cleanup.
#[derive(Clone, Debug)]
pub struct WaitOutcome {
    pub status: ExitStatus,
    pub reason: WaitReason,
    pub termination: TerminationReport,
}

/// Result of a non-final polling interval.
#[derive(Clone, Debug)]
pub enum PollResult {
    Running,
    Exited(WaitOutcome),
}

/// Failure while observing or cleaning a contained child.
#[derive(Debug)]
pub enum WaitError {
    Observe {
        source: io::Error,
        cleanup: Option<CleanupError>,
    },
    Cleanup(CleanupError),
}

impl WaitError {
    /// Cleanup error that accompanied the wait failure, if any.
    #[must_use]
    pub const fn cleanup_error(&self) -> Option<&CleanupError> {
        match self {
            Self::Observe { cleanup, .. } => cleanup.as_ref(),
            Self::Cleanup(error) => Some(error),
        }
    }
}

impl fmt::Display for WaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Observe { source, cleanup } => {
                write!(formatter, "failed to observe process leader: {source}")?;
                if let Some(error) = cleanup {
                    write!(formatter, "; subsequent {error}")?;
                }
                Ok(())
            }
            Self::Cleanup(error) => error.fmt(formatter),
        }
    }
}

impl Error for WaitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Observe { source, .. } => Some(source),
            Self::Cleanup(error) => Some(error),
        }
    }
}

/// A child whose descendants are held in an operating-system containment.
#[derive(Debug)]
pub struct ProcessTree {
    child: Child,
    containment: platform::Containment,
    leader_status: Option<ExitStatus>,
    final_report: Option<TerminationReport>,
    completion_reason: Option<WaitReason>,
}

impl ProcessTree {
    /// Spawn a child with default platform options.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if containment creation, process creation,
    /// containment assignment, or suspended-child resumption fails.
    pub fn spawn(command: &mut Command) -> Result<Self, SpawnError> {
        Self::spawn_with(command, SpawnOptions::default())
    }

    /// Spawn a child with explicit platform options.
    ///
    /// On Windows this function controls the command's creation flags so that
    /// assignment to the Job Object happens before user code can run.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] for any failed spawn or containment stage.
    pub fn spawn_with(command: &mut Command, options: SpawnOptions) -> Result<Self, SpawnError> {
        let prepared = platform::prepare(command, options).map_err(SpawnError::from)?;
        let mut child = command
            .spawn()
            .map_err(|source| SpawnError::new(SpawnStage::SpawnProcess, source))?;
        let containment = platform::attach(prepared, &mut child).map_err(SpawnError::from)?;
        Ok(Self {
            child,
            containment,
            leader_status: None,
            final_report: None,
            completion_reason: None,
        })
    }

    /// Operating-system process identifier of the leader.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Take the child's configured standard-input pipe.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Take the child's configured standard-output pipe.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Take the child's configured standard-error pipe.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Observe once, cleaning the full tree before returning an exit status.
    ///
    /// # Errors
    ///
    /// Returns [`WaitError`] if leader observation or cleanup fails. An
    /// observation error triggers a best-effort bounded cleanup before return.
    pub fn poll_exit(&mut self, cleanup: CleanupPolicy) -> Result<PollResult, WaitError> {
        if let Some(report) = &self.final_report {
            let reason = self.completion_reason.unwrap_or(WaitReason::Exited);
            return outcome_from_report(report.clone(), reason).map(PollResult::Exited);
        }

        let observed = match platform::observe_exit(&mut self.child, &self.containment) {
            Ok(observed) => observed,
            Err(source) => {
                let cleanup_error = self.terminate_bounded(cleanup).err();
                return Err(WaitError::Observe {
                    source,
                    cleanup: cleanup_error,
                });
            }
        };
        let Some(observed) = observed else {
            return Ok(PollResult::Running);
        };
        let report = self
            .cleanup_internal(cleanup, Some(observed))
            .map_err(WaitError::Cleanup)?;
        self.completion_reason = Some(WaitReason::Exited);
        outcome_from_report(report, WaitReason::Exited).map(PollResult::Exited)
    }

    /// Poll for up to `slice` without interpreting a still-running child as a
    /// final timeout.
    ///
    /// This is the cooperative cancellation integration point: callers may use
    /// short slices, check their cancellation token, and call
    /// [`Self::terminate_bounded`] only when cancellation is requested.
    ///
    /// # Errors
    ///
    /// Returns [`WaitError`] if observation or cleanup fails.
    pub fn wait_slice(&mut self, slice: Duration, cleanup: CleanupPolicy) -> Result<PollResult, WaitError> {
        let started = Instant::now();
        loop {
            match self.poll_exit(cleanup)? {
                PollResult::Running => {}
                exited @ PollResult::Exited(_) => return Ok(exited),
            }
            let elapsed = started.elapsed();
            if elapsed >= slice {
                return Ok(PollResult::Running);
            }
            thread::sleep(
                slice
                    .saturating_sub(elapsed)
                    .min(cleanup.effective_poll_interval()),
            );
        }
    }

    /// Wait up to `timeout`, then forcibly clean and reap the full tree.
    ///
    /// A direct leader exit is never returned immediately: descendants are
    /// unconditionally cleaned before the status becomes visible.
    ///
    /// # Errors
    ///
    /// Returns [`WaitError`] if observation, cleanup, or bounded reaping fails.
    pub fn wait_bounded(
        &mut self,
        timeout: Duration,
        cleanup: CleanupPolicy,
    ) -> Result<WaitOutcome, WaitError> {
        match self.wait_slice(timeout, cleanup)? {
            PollResult::Exited(outcome) => Ok(outcome),
            PollResult::Running => {
                let report = self.cleanup_internal(cleanup, None).map_err(WaitError::Cleanup)?;
                self.completion_reason = Some(WaitReason::TimedOut);
                outcome_from_report(report, WaitReason::TimedOut)
            }
        }
    }

    /// Terminate and reap the full tree within `cleanup`'s explicit bounds.
    ///
    /// This method is intended for cancellation and error paths.
    ///
    /// # Errors
    ///
    /// Returns every observed cleanup failure in a [`CleanupError`].
    pub fn terminate_bounded(&mut self, cleanup: CleanupPolicy) -> Result<TerminationReport, CleanupError> {
        self.cleanup_internal(cleanup, None)
    }

    #[allow(clippy::too_many_lines)]
    fn cleanup_internal(
        &mut self,
        policy: CleanupPolicy,
        mut observed: Option<platform::ObservedExit>,
    ) -> Result<TerminationReport, CleanupError> {
        if let Some(report) = &self.final_report {
            return Ok(report.clone());
        }

        let mut report = TerminationReport::incomplete();
        report.status = self.leader_status;
        let mut issues = Vec::new();
        let leader_was_already_observed = observed.is_some() || report.status.is_some();

        if platform::SUPPORTS_GRACEFUL_TREE_TERMINATION {
            report.graceful_requested = true;
            if let Err(source) = platform::send_graceful(&self.child, &self.containment) {
                issues.push(CleanupIssue::new(CleanupStage::SendGraceful, source));
            }
            let graceful_started = Instant::now();
            while !leader_was_already_observed && graceful_started.elapsed() < policy.graceful_timeout {
                if observed.is_none() {
                    match platform::observe_exit(&mut self.child, &self.containment) {
                        Ok(value) => observed = value,
                        Err(source) => {
                            issues.push(CleanupIssue::new(CleanupStage::ObserveLeader, source));
                            break;
                        }
                    }
                }
                sleep_until_bound(
                    graceful_started,
                    policy.graceful_timeout,
                    policy.effective_poll_interval(),
                );
            }
        }

        report.force_requested = true;
        if let Err(source) = platform::send_force(&self.child, &self.containment) {
            issues.push(CleanupIssue::new(CleanupStage::SendForce, source));
            if observed.is_none() {
                if let Err(source) = self.child.kill() {
                    if source.kind() != io::ErrorKind::InvalidInput {
                        issues.push(CleanupIssue::new(CleanupStage::KillLeader, source));
                    }
                }
            }
        }

        let force_started = Instant::now();
        while observed.is_none() && report.status.is_none() && force_started.elapsed() < policy.kill_timeout {
            match platform::observe_exit(&mut self.child, &self.containment) {
                Ok(value) => observed = value,
                Err(source) => {
                    issues.push(CleanupIssue::new(CleanupStage::ObserveLeader, source));
                    break;
                }
            }
            if observed.is_none() {
                sleep_until_bound(
                    force_started,
                    policy.kill_timeout,
                    policy.effective_poll_interval(),
                );
            }
        }

        if report.status.is_none() {
            if let Some(observed) = observed {
                match platform::reap(&mut self.child, observed) {
                    Ok(status) => {
                        self.leader_status = Some(status);
                        report.status = Some(status);
                    }
                    Err(source) => issues.push(CleanupIssue::new(CleanupStage::ReapLeader, source)),
                }
            } else {
                issues.push(CleanupIssue::timed_out(
                    CleanupStage::ReapLeader,
                    policy.kill_timeout,
                    "process leader",
                ));
            }
        }

        if report.status.is_some() {
            loop {
                match platform::tree_alive(&self.containment) {
                    Ok(false) => {
                        report.tree_confirmed_gone = true;
                        break;
                    }
                    Ok(true) => {}
                    Err(source) => {
                        issues.push(CleanupIssue::new(CleanupStage::VerifyTree, source));
                        break;
                    }
                }
                if force_started.elapsed() >= policy.kill_timeout {
                    issues.push(CleanupIssue::timed_out(
                        CleanupStage::VerifyTree,
                        policy.kill_timeout,
                        "process tree",
                    ));
                    break;
                }
                sleep_until_bound(
                    force_started,
                    policy.kill_timeout,
                    policy.effective_poll_interval(),
                );
            }
        }

        if report.status.is_some() && report.tree_confirmed_gone {
            self.final_report = Some(report.clone());
        }
        if issues.is_empty() {
            Ok(report)
        } else {
            Err(CleanupError { report, issues })
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.final_report.is_none() {
            platform::force_on_drop(&mut self.child, &self.containment);
        }
    }
}

fn sleep_until_bound(started: Instant, bound: Duration, interval: Duration) {
    let remaining = bound.saturating_sub(started.elapsed());
    if !remaining.is_zero() {
        thread::sleep(remaining.min(interval));
    }
}

fn outcome_from_report(report: TerminationReport, reason: WaitReason) -> Result<WaitOutcome, WaitError> {
    let Some(status) = report.status else {
        return Err(WaitError::Cleanup(CleanupError {
            report,
            issues: vec![CleanupIssue::new(
                CleanupStage::ReapLeader,
                io::Error::other("cleanup completed without a reaped leader status"),
            )],
        }));
    };
    Ok(WaitOutcome {
        status,
        reason,
        termination: report,
    })
}
