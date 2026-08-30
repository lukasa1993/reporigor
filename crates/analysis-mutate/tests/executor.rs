use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
#[cfg(not(windows))]
use std::time::Duration;

#[cfg(not(windows))]
use analysis_mutate::CommandOutcome;
use analysis_mutate::{
    mutation_state_directory, CancellationToken, CommandSpec, MutationError, MutationExecutor, MutationMode,
    MutationOptions, ACTIVE_JOURNAL,
};
#[cfg(unix)]
use analysis_mutate::{recover_active, RecoveryAction};
use reporigor_core::{Language, MutationCandidate, MutationStatus};
use tempfile::TempDir;

static EXECUTION_TEST_MUTEX: Mutex<()> = Mutex::new(());

fn serialize_execution() -> MutexGuard<'static, ()> {
    match EXECUTION_TEST_MUTEX.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct ExecutionFixture {
    _serial: MutexGuard<'static, ()>,
    directory: TempDir,
}

impl ExecutionFixture {
    fn root(&self) -> &Path {
        self.directory.path()
    }
}

fn execution_fixture() -> Result<ExecutionFixture, std::io::Error> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("sample.txt"), b"true\n")?;
    Ok(ExecutionFixture {
        _serial: serialize_execution(),
        directory,
    })
}

fn candidate(id: u64) -> MutationCandidate {
    MutationCandidate {
        end_byte: 4,
        start_byte: 0,
        replacement: "false".into(),
        original: "true".into(),
        column: 1,
        line: 1,
        fingerprint: String::new(),
        operator: "boolean-literal".into(),
        stable_symbol: String::new(),
        file: "sample.txt".into(),
        language: Language::Python,
        id,
    }
}

fn assert_clean(fixture: &ExecutionFixture) -> Result<(), std::io::Error> {
    let root = fixture.root();
    assert_eq!(fs::read(root.join("sample.txt"))?, b"true\n");
    let state = mutation_state_directory(root).map_err(std::io::Error::other)?;
    assert!(!state.join(ACTIVE_JOURNAL).exists());
    Ok(())
}

fn fixture_executor(
    fixture: &ExecutionFixture,
    options: MutationOptions,
) -> Result<MutationExecutor, MutationError> {
    MutationExecutor::new(fixture.root(), options)
}

fn run_fixture(
    fixture: &ExecutionFixture,
    options: MutationOptions,
    candidates: &[MutationCandidate],
) -> Result<analysis_mutate::MutationRun, MutationError> {
    fixture_executor(fixture, options)?.run(candidates)
}

fn run_single(
    options: MutationOptions,
    selected: MutationCandidate,
) -> Result<analysis_mutate::MutationRun, Box<dyn std::error::Error>> {
    let fixture = execution_fixture()?;
    let report = run_fixture(&fixture, options, &[selected])?;
    assert_clean(&fixture)?;
    Ok(report)
}

#[derive(Clone, Copy)]
enum ExpectedOutcome {
    Killed,
    Survived,
    CompileError,
    Timeout,
    RuntimeError,
    Invalid,
}

const EXPECTED_OUTCOMES: [(MutationStatus, Option<i32>); 6] = [
    (MutationStatus::Killed, Some(1)),
    (MutationStatus::Survived, Some(0)),
    (MutationStatus::CompileError, Some(1)),
    (MutationStatus::Timeout, None),
    (MutationStatus::RuntimeError, None),
    (MutationStatus::Invalid, None),
];

fn assert_single_outcome(
    options: MutationOptions,
    selected: MutationCandidate,
    expected: ExpectedOutcome,
) -> Result<analysis_mutate::MutationRun, Box<dyn std::error::Error>> {
    let report = run_single(options, selected)?;
    let (status, exit_code) = EXPECTED_OUTCOMES[expected as usize];
    assert_eq!(report.results[0].status, status);
    assert_eq!(report.results[0].exit_code, exit_code);
    Ok(report)
}

fn run_error(
    options: MutationOptions,
    candidates: &[MutationCandidate],
) -> Result<MutationError, Box<dyn std::error::Error>> {
    let fixture = execution_fixture()?;
    let Err(error) = run_fixture(&fixture, options, candidates) else {
        panic!("mutation run unexpectedly succeeded");
    };
    assert_clean(&fixture)?;
    Ok(error)
}

#[cfg(not(windows))]
fn assert_successful_baselines(outcomes: &[Option<&CommandOutcome>]) {
    assert!(outcomes
        .iter()
        .all(|outcome| outcome.is_some_and(CommandOutcome::success)));
}

fn exit_command(code: u8) -> CommandSpec {
    #[cfg(windows)]
    let shell = format!("exit /B {code}");
    #[cfg(not(windows))]
    let shell = format!("exit {code}");
    CommandSpec::shell(shell)
}

fn without_baseline(command: impl Into<CommandSpec>) -> MutationOptions {
    let mut options = MutationOptions::execute(command);
    options.run_baseline = false;
    options
}

#[test]
fn list_mode_returns_pending_without_running_or_mutating() {
    let fixture =
        execution_fixture().unwrap_or_else(|error| panic!("failed to create list-mode fixture: {error:?}"));
    let state = mutation_state_directory(fixture.root())
        .unwrap_or_else(|error| panic!("failed to resolve mutation state: {error:?}"));
    assert!(!state.exists());
    let report = run_fixture(&fixture, MutationOptions::list(), &[candidate(1)])
        .unwrap_or_else(|error| panic!("list-mode inventory failed: {error:?}"));

    assert_eq!(report.mode, MutationMode::List);
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].status, MutationStatus::Pending);
    assert!(report.baseline.test.is_none());
    assert_clean(&fixture).unwrap_or_else(|error| panic!("list mode left source dirty: {error:?}"));
    assert!(
        !state.exists(),
        "read-only inventory must not create mutation state"
    );
}

#[cfg(not(windows))]
#[test]
fn baseline_validation_and_timeout_classify_native_outcomes() -> Result<(), Box<dyn std::error::Error>> {
    let options = MutationOptions::execute("grep -q '^true$' sample.txt");
    let report = assert_single_outcome(options, candidate(1), ExpectedOutcome::Killed)?;
    assert_successful_baselines(&[report.baseline.test.as_ref()]);

    let mut invalidated = MutationOptions::execute(exit_command(0));
    invalidated.validation_command = Some(CommandSpec::shell("grep -q '^true$' sample.txt"));
    let report = assert_single_outcome(invalidated, candidate(1), ExpectedOutcome::CompileError)?;
    assert_successful_baselines(&[report.baseline.validation.as_ref()]);

    let mut timed = without_baseline("sleep 1");
    timed.timeout = Duration::from_millis(20);
    assert_single_outcome(timed, candidate(1), ExpectedOutcome::Timeout)?;
    Ok(())
}

#[test]
fn nonbaseline_execution_classifies_survived_runtime_and_invalid() -> Result<(), Box<dyn std::error::Error>> {
    assert_single_outcome(
        without_baseline(exit_command(0)),
        candidate(1),
        ExpectedOutcome::Survived,
    )?;

    let missing = Path::new("__reporigor_missing__").join("missing-test-program");
    let options = without_baseline(CommandSpec::program(missing, Vec::<String>::new()));
    assert_single_outcome(options, candidate(1), ExpectedOutcome::RuntimeError)?;

    let mut stale = candidate(1);
    stale.start_byte = 1;
    stale.end_byte = 5;
    assert_single_outcome(without_baseline(exit_command(0)), stale, ExpectedOutcome::Invalid)?;
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn mutant_commands_receive_identity_but_baseline_does_not() -> Result<(), Box<dyn std::error::Error>> {
    let command = "if [ \"${REPORIGOR_MUTANT_ID-unset}\" = unset ]; then \
         test \"${REPORIGOR_MUTANT_FINGERPRINT-unset}\" = unset; \
         else test \"$REPORIGOR_MUTANT_ID\" = 17 && \
         test \"$REPORIGOR_MUTANT_FINGERPRINT\" = fixture-fingerprint; fi";
    let mut options = MutationOptions::execute(command);
    options.validation_command = Some(CommandSpec::shell(command));
    let mut selected = candidate(17);
    selected.fingerprint = "fixture-fingerprint".into();
    let report = assert_single_outcome(options, selected, ExpectedOutcome::Survived)?;

    assert_successful_baselines(&[report.baseline.validation.as_ref(), report.baseline.test.as_ref()]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cancellation_restores_source_and_leaves_recovery_clean() -> Result<(), Box<dyn std::error::Error>> {
    use std::thread;
    use std::time::Instant;

    let fixture = execution_fixture()?;
    let cancellation = CancellationToken::new();
    let mut options = MutationOptions::execute(
        "if grep -q '^false$' sample.txt; then printf started > started.txt; \
         (sleep 0.4; printf leaked > leaked.txt) & wait; else exit 33; fi",
    );
    options.run_baseline = false;
    options.cancellation = cancellation.clone();

    let marker = fixture.root().join("started.txt");
    let canceller = cancellation.clone();
    let cancellation_thread = thread::spawn(move || {
        for _ in 0..200 {
            if marker.is_file() {
                canceller.cancel();
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        canceller.cancel();
        false
    });
    let started = Instant::now();
    let executor = fixture_executor(&fixture, options)?;
    let result = executor.run(&[candidate(1)]);
    let observed_mutation = cancellation_thread
        .join()
        .map_err(|_| "cancellation thread panicked")?;

    assert!(
        observed_mutation,
        "test command never observed the applied mutation"
    );
    assert!(matches!(result, Err(MutationError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_clean(&fixture)?;
    let state = mutation_state_directory(fixture.root())?;
    assert!(!state.join(ACTIVE_JOURNAL).exists());
    assert_eq!(recover_active(fixture.root())?, RecoveryAction::None);
    thread::sleep(Duration::from_millis(500));
    assert!(!fixture.root().join("leaked.txt").exists());
    Ok(())
}

#[test]
fn preflight_rejects_cancellation_baseline_failure_and_duplicate_ids(
) -> Result<(), Box<dyn std::error::Error>> {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut options = MutationOptions::execute(exit_command(0));
    options.cancellation = cancellation;
    assert!(matches!(
        run_error(options, &[candidate(1)])?,
        MutationError::Cancelled
    ));
    assert!(matches!(
        run_error(MutationOptions::execute(exit_command(1)), &[candidate(1)])?,
        MutationError::BaselineFailed { .. }
    ));
    assert!(matches!(
        run_error(MutationOptions::list(), &[candidate(1), candidate(1)])?,
        MutationError::InvalidOptions(_)
    ));
    Ok(())
}

#[test]
fn policy_and_limit_emit_standard_nonexecuted_statuses() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = execution_fixture()?;
    let mut options = without_baseline(exit_command(0));
    options.max_mutants = Some(1);
    options.no_coverage_ids.insert(2);
    options.ignored_ids.insert(3);
    let report = run_fixture(
        &fixture,
        options,
        &[candidate(1), candidate(2), candidate(3), candidate(4)],
    )?;

    assert_eq!(report.results[0].status, MutationStatus::Survived);
    assert_eq!(report.results[1].status, MutationStatus::NoCoverage);
    assert_eq!(report.results[2].status, MutationStatus::Ignored);
    assert_eq!(report.results[3].status, MutationStatus::Ignored);
    let summary = report.summary();
    assert_eq!(summary.total, 4);
    assert_eq!(summary.survived, 1);
    assert_eq!(summary.no_coverage, 1);
    assert_eq!(summary.ignored, 2);
    assert_clean(&fixture)?;
    Ok(())
}
