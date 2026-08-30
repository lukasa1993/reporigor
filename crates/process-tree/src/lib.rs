#![doc = include_str!("../README.md")]

use std::error::Error;
use std::fmt;
use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

mod bounded;

pub use bounded::{
    configure_piped_command, run_bounded, BoundedOutput, BoundedRunError, BoundedRunStage, CapturedStream,
    CommandLimits,
};

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
const DEFAULT_CLEANUP_POLICY: CleanupPolicy = CleanupPolicy {
    graceful_timeout: Duration::from_millis(100),
    kill_timeout: Duration::from_secs(1),
    poll_interval: Duration::from_millis(10),
};

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
        DEFAULT_CLEANUP_POLICY
    }
}

impl CleanupPolicy {
    /// Construct a cleanup policy from explicit phase bounds.
    #[must_use]
    pub const fn new(graceful_timeout: Duration, kill_timeout: Duration, poll_interval: Duration) -> Self {
        Self {
            graceful_timeout,
            kill_timeout,
            poll_interval,
        }
    }
}

mod polling {
    use super::{CleanupPolicy, Duration, MINIMUM_POLL_INTERVAL};

    pub(super) fn interval(policy: CleanupPolicy) -> Duration {
        policy.poll_interval.max(MINIMUM_POLL_INTERVAL)
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

impl SpawnStage {
    fn label(self) -> &'static str {
        const LABELS: &str = "create process containment
configure process containment
spawn process
assign process to containment
locate suspended primary thread
open suspended primary thread
resume suspended primary thread";
        LABELS.lines().nth(self as usize).unwrap_or("spawn process")
    }
}

impl fmt::Display for SpawnStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.label(), formatter)
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
    fn parts(&self) -> (SpawnStage, &[CleanupIssue]) {
        (self.stage, &self.cleanup_issues)
    }

    /// The failed creation stage.
    #[must_use]
    pub fn stage(&self) -> SpawnStage {
        self.parts().0
    }

    /// Cleanup problems encountered while aborting a partially created child.
    #[must_use]
    pub fn cleanup_issues(&self) -> &[CleanupIssue] {
        self.parts().1
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        display::failure(formatter, self.stage, &self.source)?;
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

impl CleanupStage {
    fn write_label(self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const LABELS: [&str; 6] = [
            "send graceful tree termination",
            "send forced tree termination",
            "terminate the process leader",
            "observe process-leader exit",
            "reap the process leader",
            "verify process-tree cleanup",
        ];
        formatter.write_str(LABELS[self as usize])
    }
}

impl fmt::Display for CleanupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_label(formatter)
    }
}

/// One concrete problem found while cleaning a process tree.
#[derive(Debug)]
pub struct CleanupIssue {
    stage: CleanupStage,
    source: io::Error,
}

impl CleanupIssue {
    fn parts(&self) -> (CleanupStage, &io::Error) {
        (self.stage, &self.source)
    }

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
    pub fn stage(&self) -> CleanupStage {
        self.parts().0
    }

    /// Underlying operating-system or timeout error.
    #[must_use]
    pub fn source_error(&self) -> &io::Error {
        self.parts().1
    }
}

impl fmt::Display for CleanupIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (stage, source) = self.parts();
        formatter.write_fmt(format_args!("failed to {stage}: {source}"))
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
    fn parts(&self) -> (&TerminationReport, &[CleanupIssue]) {
        (&self.report, &self.issues)
    }

    /// Partial cleanup report.
    #[must_use]
    pub fn report(&self) -> &TerminationReport {
        self.parts().0
    }

    /// All cleanup problems, in the order they occurred.
    #[must_use]
    pub fn issues(&self) -> &[CleanupIssue] {
        self.parts().1
    }
}

mod display {
    use super::{fmt, io};

    pub(super) fn failure(
        formatter: &mut fmt::Formatter<'_>,
        stage: impl fmt::Display,
        source: &io::Error,
    ) -> fmt::Result {
        write!(formatter, "failed to {stage}: {source}")
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

struct CleanupState {
    report: TerminationReport,
    issues: Vec<CleanupIssue>,
    observed: Option<platform::ObservedExit>,
    leader_was_already_observed: bool,
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
        let mut child = command.spawn().map_err(|source| SpawnError {
            stage: SpawnStage::SpawnProcess,
            source,
            cleanup_issues: Vec::new(),
        })?;
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
            return Self::outcome_from_report(report.clone(), reason).map(PollResult::Exited);
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
        Self::outcome_from_report(report, WaitReason::Exited).map(PollResult::Exited)
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
            thread::sleep(slice.saturating_sub(elapsed).min(polling::interval(cleanup)));
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
                Self::outcome_from_report(report, WaitReason::TimedOut)
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

    fn cleanup_internal(
        &mut self,
        policy: CleanupPolicy,
        observed: Option<platform::ObservedExit>,
    ) -> Result<TerminationReport, CleanupError> {
        if let Some(report) = &self.final_report {
            return Ok(report.clone());
        }
        let mut report = TerminationReport::incomplete();
        report.status = self.leader_status;
        let leader_was_already_observed = observed.is_some() || report.status.is_some();
        let mut state = CleanupState {
            report,
            issues: Vec::new(),
            observed,
            leader_was_already_observed,
        };
        self.request_graceful_cleanup(policy, &mut state);
        self.request_forced_cleanup(&mut state);
        let force_started = Instant::now();
        self.observe_forced_exit(policy, force_started, &mut state);
        self.reap_observed_leader(policy, &mut state);
        self.verify_tree_cleanup(policy, force_started, &mut state);
        self.finish_cleanup(state)
    }

    fn request_graceful_cleanup(&mut self, policy: CleanupPolicy, state: &mut CleanupState) {
        if !platform::SUPPORTS_GRACEFUL_TREE_TERMINATION {
            return;
        }
        state.report.graceful_requested = true;
        if let Err(source) = platform::send_graceful(&self.child, &self.containment) {
            state
                .issues
                .push(CleanupIssue::new(CleanupStage::SendGraceful, source));
        }
        self.await_graceful_exit(policy, state);
    }

    fn await_graceful_exit(&mut self, policy: CleanupPolicy, state: &mut CleanupState) {
        if state.leader_was_already_observed {
            return;
        }
        let started = Instant::now();
        while started.elapsed() < policy.graceful_timeout {
            if state.observed.is_none() && !self.observe_cleanup_exit(state) {
                break;
            }
            Self::sleep_until_bound(started, policy.graceful_timeout, polling::interval(policy));
        }
    }

    fn observe_cleanup_exit(&mut self, state: &mut CleanupState) -> bool {
        match platform::observe_exit(&mut self.child, &self.containment) {
            Ok(value) => {
                state.observed = value;
                true
            }
            Err(source) => {
                state
                    .issues
                    .push(CleanupIssue::new(CleanupStage::ObserveLeader, source));
                false
            }
        }
    }

    fn request_forced_cleanup(&mut self, state: &mut CleanupState) {
        state.report.force_requested = true;
        if let Err(source) = platform::send_force(&self.child, &self.containment) {
            state
                .issues
                .push(CleanupIssue::new(CleanupStage::SendForce, source));
            self.kill_leader_after_force_failure(state);
        }
    }

    fn kill_leader_after_force_failure(&mut self, state: &mut CleanupState) {
        if state.observed.is_some() {
            return;
        }
        Self::record_leader_kill(self.child.kill(), &mut state.issues);
    }

    fn record_leader_kill(result: io::Result<()>, issues: &mut Vec<CleanupIssue>) {
        if let Some(issue) = Self::leader_kill_issue(result) {
            issues.push(issue);
        }
    }

    fn leader_kill_issue(result: io::Result<()>) -> Option<CleanupIssue> {
        result
            .err()
            .filter(|source| source.kind() != io::ErrorKind::InvalidInput)
            .map(|source| CleanupIssue::new(CleanupStage::KillLeader, source))
    }

    fn observe_forced_exit(&mut self, policy: CleanupPolicy, started: Instant, state: &mut CleanupState) {
        while Self::awaiting_forced_exit(state, started, policy.kill_timeout) {
            if !self.observe_cleanup_exit(state) {
                break;
            }
            if state.observed.is_none() {
                Self::sleep_until_bound(started, policy.kill_timeout, polling::interval(policy));
            }
        }
    }

    fn awaiting_forced_exit(state: &CleanupState, started: Instant, timeout: Duration) -> bool {
        state.observed.is_none() && state.report.status.is_none() && started.elapsed() < timeout
    }

    fn reap_observed_leader(&mut self, policy: CleanupPolicy, state: &mut CleanupState) {
        if state.report.status.is_some() {
            return;
        }
        let Some(observed) = state.observed.take() else {
            state.issues.push(CleanupIssue::timed_out(
                CleanupStage::ReapLeader,
                policy.kill_timeout,
                "process leader",
            ));
            return;
        };
        match platform::reap(&mut self.child, observed) {
            Ok(status) => {
                self.leader_status = Some(status);
                state.report.status = Some(status);
            }
            Err(source) => state
                .issues
                .push(CleanupIssue::new(CleanupStage::ReapLeader, source)),
        }
    }

    fn verify_tree_cleanup(&self, policy: CleanupPolicy, started: Instant, state: &mut CleanupState) {
        if state.report.status.is_none() {
            return;
        }
        loop {
            if self.observe_tree_gone(state) {
                break;
            }
            if started.elapsed() >= policy.kill_timeout {
                state.issues.push(CleanupIssue::timed_out(
                    CleanupStage::VerifyTree,
                    policy.kill_timeout,
                    "process tree",
                ));
                break;
            }
            Self::sleep_until_bound(started, policy.kill_timeout, polling::interval(policy));
        }
    }

    fn observe_tree_gone(&self, state: &mut CleanupState) -> bool {
        match platform::tree_alive(&self.containment) {
            Ok(false) => {
                state.report.tree_confirmed_gone = true;
                true
            }
            Ok(true) => false,
            Err(source) => {
                state
                    .issues
                    .push(CleanupIssue::new(CleanupStage::VerifyTree, source));
                true
            }
        }
    }

    fn finish_cleanup(&mut self, state: CleanupState) -> Result<TerminationReport, CleanupError> {
        if state.report.status.is_some() && state.report.tree_confirmed_gone {
            self.final_report = Some(state.report.clone());
        }
        if state.issues.is_empty() {
            Ok(state.report)
        } else {
            Err(CleanupError {
                report: state.report,
                issues: state.issues,
            })
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
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.final_report.is_none() {
            platform::force_on_drop(&mut self.child, &self.containment);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleanup_issue(stage: CleanupStage, message: &str) -> CleanupIssue {
        CleanupIssue::new(stage, io::Error::other(message))
    }

    fn cleanup_error() -> CleanupError {
        CleanupError {
            report: TerminationReport::incomplete(),
            issues: vec![cleanup_issue(CleanupStage::VerifyTree, "tree remained alive")],
        }
    }

    #[test]
    fn spawn_error_display_and_accessors_cover_cleanup_details() {
        let empty = SpawnError {
            stage: SpawnStage::SpawnProcess,
            source: io::Error::other("spawn failed"),
            cleanup_issues: Vec::new(),
        };
        assert_eq!(empty.stage(), SpawnStage::SpawnProcess);
        assert!(empty.cleanup_issues().is_empty());
        assert_eq!(empty.to_string(), "failed to spawn process: spawn failed");

        let with_cleanup = SpawnError {
            stage: SpawnStage::AssignProcess,
            source: io::Error::other("assignment failed"),
            cleanup_issues: vec![cleanup_issue(CleanupStage::KillLeader, "kill failed")],
        };
        assert!(with_cleanup.to_string().contains("1 cleanup issue(s)"));
    }

    #[test]
    fn cleanup_error_display_and_accessors_include_each_issue() {
        let error = cleanup_error();
        assert!(error.report().status.is_none());
        assert_eq!(error.issues().len(), 1);
        assert_eq!(error.issues()[0].stage(), CleanupStage::VerifyTree);
        assert_eq!(error.issues()[0].source_error().kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("tree remained alive"));
    }

    #[test]
    fn wait_error_display_covers_observe_and_cleanup_forms() {
        let observe = WaitError::Observe {
            source: io::Error::other("observe failed"),
            cleanup: None,
        };
        assert_eq!(
            observe.to_string(),
            "failed to observe process leader: observe failed"
        );

        let observe_with_cleanup = WaitError::Observe {
            source: io::Error::other("observe failed"),
            cleanup: Some(cleanup_error()),
        };
        assert!(observe_with_cleanup
            .to_string()
            .contains("subsequent process-tree cleanup"));

        let cleanup = WaitError::Cleanup(cleanup_error());
        assert!(cleanup
            .to_string()
            .starts_with("process-tree cleanup had 1 issue(s)"));
    }

    #[test]
    fn leader_kill_results_ignore_already_gone_processes_and_record_real_errors() {
        let mut issues = Vec::new();
        ProcessTree::record_leader_kill(Ok(()), &mut issues);
        ProcessTree::record_leader_kill(
            Err(io::Error::new(io::ErrorKind::InvalidInput, "already gone")),
            &mut issues,
        );
        ProcessTree::record_leader_kill(Err(io::Error::other("kill failed")), &mut issues);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].stage(), CleanupStage::KillLeader);
    }
}
