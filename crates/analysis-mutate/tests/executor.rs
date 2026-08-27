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

fn project() -> Result<TempDir, std::io::Error> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("sample.txt"), b"true\n")?;
    Ok(directory)
}

fn candidate(id: u64) -> MutationCandidate {
    MutationCandidate {
        id,
        language: Language::Python,
        file: "sample.txt".into(),
        line: 1,
        column: 1,
        original: "true".into(),
        replacement: "false".into(),
        start_byte: 0,
        end_byte: 4,
    }
}

fn assert_clean(root: &Path) -> Result<(), std::io::Error> {
    assert_eq!(fs::read(root.join("sample.txt"))?, b"true\n");
    let state = mutation_state_directory(root).map_err(std::io::Error::other)?;
    assert!(!state.join(ACTIVE_JOURNAL).exists());
    Ok(())
}

#[cfg(windows)]
fn success_command() -> CommandSpec {
    CommandSpec::shell("exit /B 0")
}

#[cfg(not(windows))]
fn success_command() -> CommandSpec {
    CommandSpec::shell("exit 0")
}

#[cfg(windows)]
fn failure_command() -> CommandSpec {
    CommandSpec::shell("exit /B 1")
}

#[cfg(not(windows))]
fn failure_command() -> CommandSpec {
    CommandSpec::shell("exit 1")
}

#[test]
fn list_mode_returns_pending_without_running_or_mutating() -> Result<(), Box<dyn std::error::Error>> {
    let directory = project()?;
    let state = mutation_state_directory(directory.path())?;
    assert!(!state.exists());
    let executor = MutationExecutor::new(directory.path(), MutationOptions::list())?;
    let report = executor.run(&[candidate(1)])?;

    assert_eq!(report.mode, MutationMode::List);
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].status, MutationStatus::Pending);
    assert!(report.baseline.test.is_none());
    assert_clean(directory.path())?;
    assert!(
        !state.exists(),
        "read-only inventory must not create mutation state"
    );
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn baseline_and_failed_mutant_test_classify_as_killed() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let options = MutationOptions::execute("grep -q '^true$' sample.txt");
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[candidate(1)])?;

    assert!(report.baseline.test.as_ref().is_some_and(CommandOutcome::success));
    assert_eq!(report.results[0].status, MutationStatus::Killed);
    assert_eq!(report.results[0].exit_code, Some(1));
    assert_clean(directory.path())?;
    Ok(())
}

#[test]
fn successful_mutant_test_classifies_as_survived() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let mut options = MutationOptions::execute(success_command());
    options.run_baseline = false;
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[candidate(1)])?;

    assert_eq!(report.results[0].status, MutationStatus::Survived);
    assert_clean(directory.path())?;
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn failed_validation_classifies_as_compile_error() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let mut options = MutationOptions::execute(success_command());
    options.validation_command = Some(CommandSpec::shell("grep -q '^true$' sample.txt"));
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[candidate(1)])?;

    assert!(report
        .baseline
        .validation
        .as_ref()
        .is_some_and(CommandOutcome::success));
    assert_eq!(report.results[0].status, MutationStatus::CompileError);
    assert_eq!(report.results[0].exit_code, Some(1));
    assert_clean(directory.path())?;
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn timed_out_test_is_not_misclassified_as_killed() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let mut options = MutationOptions::execute("sleep 1");
    options.run_baseline = false;
    options.timeout = Duration::from_millis(20);
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[candidate(1)])?;

    assert_eq!(report.results[0].status, MutationStatus::Timeout);
    assert_eq!(report.results[0].exit_code, None);
    assert_clean(directory.path())?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn cancellation_restores_source_and_leaves_recovery_clean() -> Result<(), Box<dyn std::error::Error>> {
    use std::thread;
    use std::time::Instant;

    let _serial = serialize_execution();
    let directory = project()?;
    let cancellation = CancellationToken::new();
    let mut options = MutationOptions::execute(
        "if grep -q '^false$' sample.txt; then printf started > started.txt; \
         (sleep 0.4; printf leaked > leaked.txt) & wait; else exit 33; fi",
    );
    options.run_baseline = false;
    options.cancellation = cancellation.clone();

    let marker = directory.path().join("started.txt");
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
    let executor = MutationExecutor::new(directory.path(), options)?;
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
    assert_clean(directory.path())?;
    let state = mutation_state_directory(directory.path())?;
    assert!(!state.join(ACTIVE_JOURNAL).exists());
    assert_eq!(recover_active(directory.path())?, RecoveryAction::None);
    thread::sleep(Duration::from_millis(500));
    assert!(!directory.path().join("leaked.txt").exists());
    Ok(())
}

#[test]
fn pre_cancelled_run_stops_before_baseline_and_source_changes() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut options = MutationOptions::execute(success_command());
    options.cancellation = cancellation;
    let executor = MutationExecutor::new(directory.path(), options)?;

    assert!(matches!(
        executor.run(&[candidate(1)]),
        Err(MutationError::Cancelled)
    ));
    assert_clean(directory.path())?;
    Ok(())
}

#[test]
fn command_spawn_failure_classifies_as_runtime_error() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let missing = directory.path().join("missing-test-program");
    let mut options = MutationOptions::execute(CommandSpec::program(missing, Vec::<String>::new()));
    options.run_baseline = false;
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[candidate(1)])?;

    assert_eq!(report.results[0].status, MutationStatus::RuntimeError);
    assert_clean(directory.path())?;
    Ok(())
}

#[test]
fn stale_candidate_classifies_as_invalid() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let mut stale = candidate(1);
    stale.start_byte = 1;
    stale.end_byte = 5;
    let mut options = MutationOptions::execute(success_command());
    options.run_baseline = false;
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[stale])?;

    assert_eq!(report.results[0].status, MutationStatus::Invalid);
    assert_clean(directory.path())?;
    Ok(())
}

#[test]
fn baseline_failure_aborts_before_source_changes() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let executor = MutationExecutor::new(directory.path(), MutationOptions::execute(failure_command()))?;
    assert!(matches!(
        executor.run(&[candidate(1)]),
        Err(MutationError::BaselineFailed { .. })
    ));
    assert_clean(directory.path())?;
    Ok(())
}

#[test]
fn policy_and_limit_emit_standard_nonexecuted_statuses() -> Result<(), Box<dyn std::error::Error>> {
    let _serial = serialize_execution();
    let directory = project()?;
    let mut options = MutationOptions::execute(success_command());
    options.run_baseline = false;
    options.max_mutants = Some(1);
    options.no_coverage_ids.insert(2);
    options.ignored_ids.insert(3);
    let executor = MutationExecutor::new(directory.path(), options)?;
    let report = executor.run(&[candidate(1), candidate(2), candidate(3), candidate(4)])?;

    assert_eq!(report.results[0].status, MutationStatus::Survived);
    assert_eq!(report.results[1].status, MutationStatus::NoCoverage);
    assert_eq!(report.results[2].status, MutationStatus::Ignored);
    assert_eq!(report.results[3].status, MutationStatus::Ignored);
    let summary = report.summary();
    assert_eq!(summary.total, 4);
    assert_eq!(summary.survived, 1);
    assert_eq!(summary.no_coverage, 1);
    assert_eq!(summary.ignored, 2);
    assert_clean(directory.path())?;
    Ok(())
}

#[test]
fn duplicate_candidate_ids_are_rejected_before_baseline() -> Result<(), Box<dyn std::error::Error>> {
    let directory = project()?;
    let executor = MutationExecutor::new(directory.path(), MutationOptions::list())?;
    assert!(matches!(
        executor.run(&[candidate(1), candidate(1)]),
        Err(MutationError::InvalidOptions(_))
    ));
    assert_clean(directory.path())?;
    Ok(())
}
