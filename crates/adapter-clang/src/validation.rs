use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use reporigor_core::SourceFile;
use reporigor_process_tree::{CleanupPolicy, ProcessTree, SpawnError, WaitError, WaitOutcome, WaitReason};

use crate::command::direct_command;
use crate::{sanitize_compile_command, ClangLanguage, CompileCommand, SanitizedCommand};

const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const READER_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub text: String,
    pub truncated: bool,
}

/// Outcome of asking Clang to parse a translation unit. Unavailability,
/// rejection, timeout, and a compiler diagnostic are intentionally distinct so
/// an orchestrator can make an explicit fallback decision.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "inspect the compiler validation outcome"]
pub enum ValidationStatus {
    #[doc = "Clang accepted the translation unit."]
    Valid,
    #[doc = "Command sanitization rejected unsafe compiler arguments."]
    Rejected { message: String },
    #[doc = "Clang ran and rejected the translation unit."]
    Invalid { exit_code: Option<i32>, stderr: String },
    #[doc = "Validation was explicitly disabled by the caller."]
    NotValidated { message: String },
    #[doc = "Clang exceeded the configured validation deadline."]
    TimedOut { timeout: Duration, stderr: String },
    #[doc = "The compiler process could not be started or observed."]
    Unavailable { message: String },
}

/// A compilation-database record resolved against the requested project, with
/// both original and actual validation command provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "inspect the command provenance and validation status"]
pub struct TranslationUnit {
    /// Original compilation-database command for this unit.
    pub command: CompileCommand,
    pub command_index: usize,
    /// Sanitized compiler invocation actually executed, when available.
    pub invocation: Option<SanitizedCommand>,
    pub source: Option<SourceFile>,
    /// Language selected from explicit flags or source extension.
    pub language: Option<ClangLanguage>,
    /// Result of validating this translation unit with Clang.
    pub status: ValidationStatus,
    pub elapsed: Duration,
}

pub(crate) fn validate_translation_unit(
    command: &CompileCommand,
    language: ClangLanguage,
    compiler: &Path,
    timeout: Duration,
) -> (Option<SanitizedCommand>, ValidationStatus, Duration) {
    let invocation = match sanitize_compile_command(command, compiler, language) {
        Ok(invocation) => invocation,
        Err(error) => {
            return (
                None,
                ValidationStatus::Rejected {
                    message: error.to_string(),
                },
                Duration::ZERO,
            );
        }
    };
    let started = Instant::now();
    let status = run_bounded(&invocation, timeout);
    (Some(invocation), status, started.elapsed())
}

pub(crate) fn probe_compiler_version(compiler: &Path, timeout: Duration) -> Result<String, String> {
    let invocation = direct_command(
        compiler.to_path_buf(),
        vec!["--version".to_string()],
        std::env::current_dir().map_err(|error| error.to_string())?,
    );
    let outcome = run_bounded_capture(&invocation, timeout, Some(MAX_CAPTURE_BYTES), MAX_CAPTURE_BYTES);
    match outcome {
        ProcessOutcome::Exited {
            success: true,
            stdout,
            stderr,
            exit_code: _,
        } => Ok(version_text(&stdout, &stderr)),
        failure => Err(version_failure(compiler, timeout, failure)),
    }
}

fn version_text(stdout: &CapturedOutput, stderr: &CapturedOutput) -> String {
    let output = if stdout.text.trim().is_empty() {
        &stderr.text
    } else {
        &stdout.text
    };
    output
        .lines()
        .next()
        .unwrap_or("unknown Clang version")
        .trim()
        .to_string()
}

fn version_failure(compiler: &Path, timeout: Duration, outcome: ProcessOutcome) -> String {
    match outcome {
        ProcessOutcome::Exited {
            exit_code, stderr, ..
        } => format!(
            "{} --version exited with {:?}: {}",
            compiler.display(),
            exit_code,
            compact(&stderr.text)
        ),
        ProcessOutcome::TimedOut { .. } => format!(
            "{} --version timed out after {:.3}s",
            compiler.display(),
            timeout.as_secs_f64()
        ),
        ProcessOutcome::Unavailable(message) => message,
    }
}

fn run_bounded(invocation: &SanitizedCommand, timeout: Duration) -> ValidationStatus {
    match run_bounded_capture(invocation, timeout, None, MAX_CAPTURE_BYTES) {
        ProcessOutcome::Exited { success: true, .. } => ValidationStatus::Valid,
        ProcessOutcome::Exited {
            exit_code, stderr, ..
        } => ValidationStatus::Invalid {
            exit_code,
            stderr: compact(&stderr.text),
        },
        ProcessOutcome::TimedOut { stderr } => ValidationStatus::TimedOut {
            timeout,
            stderr: compact(&stderr.text),
        },
        ProcessOutcome::Unavailable(message) => ValidationStatus::Unavailable { message },
    }
}

#[derive(Debug)]
pub(crate) enum ProcessOutcome {
    Exited {
        success: bool,
        exit_code: Option<i32>,
        stdout: CapturedOutput,
        stderr: CapturedOutput,
    },
    TimedOut {
        stderr: CapturedOutput,
    },
    Unavailable(String),
}

pub(crate) fn run_bounded_capture(
    invocation: &SanitizedCommand,
    timeout: Duration,
    stdout_limit: Option<usize>,
    stderr_limit: usize,
) -> ProcessOutcome {
    let mut command = configured_process(invocation, stdout_limit.is_some());
    let mut child = match ProcessTree::spawn(&mut command) {
        Ok(child) => child,
        Err(error) => return ProcessOutcome::Unavailable(spawn_failure_message(invocation, &error)),
    };
    let readers = match capture_readers(&mut child, invocation, stdout_limit, stderr_limit) {
        Ok(readers) => readers,
        Err(outcome) => return outcome,
    };
    let wait_result = child.wait_bounded(timeout, CleanupPolicy::default());
    finish_process(wait_result, readers, invocation)
}

fn configured_process(invocation: &SanitizedCommand, capture_stdout: bool) -> Command {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.arguments)
        .current_dir(&invocation.directory)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    configure_scratch(&mut command, invocation.scratch_directory());
    command
}

fn configure_scratch(command: &mut Command, scratch: Option<&Path>) {
    if let Some(scratch) = scratch {
        // Clang and its platform libraries consult different temporary-file
        // variables. Point all of them at the same RAII-owned directory so a
        // syntax-only process cannot spill temporary artifacts into the
        // project or the user's shared compiler caches.
        command
            .env("TMPDIR", scratch)
            .env("TMP", scratch)
            .env("TEMP", scratch);
    }
}

struct ProcessReaders {
    stdout: Option<OutputReader>,
    stderr: Option<OutputReader>,
}

fn capture_readers(
    child: &mut ProcessTree,
    invocation: &SanitizedCommand,
    stdout_limit: Option<usize>,
    stderr_limit: usize,
) -> Result<ProcessReaders, ProcessOutcome> {
    let stderr_pipe = child.take_stderr();
    let stderr = require_pipe(stderr_pipe, child, invocation, "stderr")?;
    let stdout = capture_stdout(child, invocation, stdout_limit)?;
    Ok(ProcessReaders {
        stdout,
        stderr: Some(spawn_reader(stderr, stderr_limit)),
    })
}

fn capture_stdout(
    child: &mut ProcessTree,
    invocation: &SanitizedCommand,
    limit: Option<usize>,
) -> Result<Option<OutputReader>, ProcessOutcome> {
    let Some(limit) = limit else {
        return Ok(None);
    };
    let stdout_pipe = child.take_stdout();
    let stdout = require_pipe(stdout_pipe, child, invocation, "stdout")?;
    Ok(Some(spawn_reader(stdout, limit)))
}

fn require_pipe<T>(
    pipe: Option<T>,
    child: &mut ProcessTree,
    invocation: &SanitizedCommand,
    name: &str,
) -> Result<T, ProcessOutcome> {
    pipe.ok_or_else(|| ProcessOutcome::Unavailable(missing_pipe_message(child, invocation, name)))
}

fn finish_process(
    wait_result: Result<WaitOutcome, WaitError>,
    readers: ProcessReaders,
    invocation: &SanitizedCommand,
) -> ProcessOutcome {
    let stdout = join_reader(readers.stdout);
    let stderr = join_reader(readers.stderr);
    match wait_result {
        Ok(outcome) if outcome.reason == WaitReason::Exited => ProcessOutcome::Exited {
            success: outcome.status.success(),
            exit_code: outcome.status.code(),
            stdout,
            stderr,
        },
        Ok(_) => ProcessOutcome::TimedOut { stderr },
        Err(error) => ProcessOutcome::Unavailable(format!(
            "failed while waiting for {}: {error}; {}",
            invocation.program.display(),
            compact(&stderr.text)
        )),
    }
}

fn spawn_failure_message(invocation: &SanitizedCommand, error: &SpawnError) -> String {
    let cleanup = error
        .cleanup_issues()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    match cleanup.as_str() {
        "" => format!("failed to start {}: {error}", invocation.program.display()),
        details => format!(
            "failed to start {}: {error}; {details}",
            invocation.program.display()
        ),
    }
}

fn missing_pipe_message(child: &mut ProcessTree, invocation: &SanitizedCommand, stream: &str) -> String {
    let base = format!(
        "failed to capture {stream} from {}: pipe is unavailable",
        invocation.program.display()
    );
    match child.terminate_bounded(CleanupPolicy::default()) {
        Ok(_) => base,
        Err(error) => format!("{base}; subsequent {error}"),
    }
}

#[derive(Clone, Debug, Default)]
struct CaptureBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

struct OutputReader {
    completed: mpsc::Receiver<CaptureBuffer>,
    snapshot: Arc<Mutex<CaptureBuffer>>,
}

fn spawn_reader<R>(mut reader: R, limit: usize) -> OutputReader
where
    R: Read + Send + 'static,
{
    let (completed_sender, completed) = mpsc::channel();
    let snapshot = Arc::new(Mutex::new(CaptureBuffer::default()));
    let thread_snapshot = Arc::clone(&snapshot);
    let _reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096 * 2];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let mut capture = thread_snapshot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let remaining = limit.saturating_sub(capture.bytes.len());
                    capture.bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                    capture.truncated |= count > remaining;
                }
            }
        }
        let final_capture = thread_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let _send_result = completed_sender.send(final_capture);
    });
    OutputReader { completed, snapshot }
}

fn join_reader(reader: Option<OutputReader>) -> CapturedOutput {
    let Some(reader) = reader else {
        return CapturedOutput {
            text: String::new(),
            truncated: false,
        };
    };
    let capture = match reader.completed.recv_timeout(READER_DRAIN_TIMEOUT) {
        Ok(capture) => capture,
        Err(mpsc::RecvTimeoutError::Timeout) => incomplete_snapshot(&reader.snapshot),
        Err(mpsc::RecvTimeoutError::Disconnected) => failed_snapshot(&reader.snapshot),
    };
    CapturedOutput {
        text: String::from_utf8(capture.bytes)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
        truncated: capture.truncated,
    }
}

fn incomplete_snapshot(snapshot: &Mutex<CaptureBuffer>) -> CaptureBuffer {
    let mut capture = try_snapshot(snapshot);
    capture.truncated = true;
    capture
}

fn failed_snapshot(snapshot: &Mutex<CaptureBuffer>) -> CaptureBuffer {
    let mut capture = try_snapshot(snapshot);
    if capture.bytes.is_empty() {
        capture.bytes = b"failed to capture compiler output".to_vec();
    }
    capture.truncated = true;
    capture
}

fn try_snapshot(snapshot: &Mutex<CaptureBuffer>) -> CaptureBuffer {
    match snapshot.try_lock() {
        Ok(capture) => capture.clone(),
        Err(TryLockError::Poisoned(error)) => error.into_inner().clone(),
        Err(TryLockError::WouldBlock) => CaptureBuffer {
            bytes: Vec::new(),
            truncated: true,
        },
    }
}

fn compact(value: &str) -> String {
    let value = value.trim();
    if value.len() < MAX_CAPTURE_BYTES {
        value.to_string()
    } else {
        format!("{value}\n[compiler output truncated]")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::{
        compile_command, create_dir, executable_fixture, expect_error, temp_dir, write,
    };
    use crate::CommandOrigin;

    fn command(directory: &Path) -> CompileCommand {
        compile_command(directory, "source.c", &["clang", "source.c", "-c"])
    }

    fn validation_status(directory: &Path, compiler: &Path, timeout: Duration) -> ValidationStatus {
        validate_translation_unit(&command(directory), ClangLanguage::C, compiler, timeout).1
    }

    fn capture(invocation: &SanitizedCommand, timeout: Duration) -> ProcessOutcome {
        run_bounded_capture(invocation, timeout, Some(MAX_CAPTURE_BYTES), MAX_CAPTURE_BYTES)
    }

    fn script_status(contents: &str, timeout: Duration) -> (tempfile::TempDir, ValidationStatus) {
        let (temp, compiler) = executable_fixture(contents);
        write(temp.path().join("source.c"), "int main(void) { return 0; }");
        let status = validation_status(temp.path(), &compiler, timeout);
        (temp, status)
    }

    fn probe_script(contents: &str, timeout: Duration) -> Result<String, String> {
        let (_temp, compiler) = executable_fixture(contents);
        probe_compiler_version(&compiler, timeout)
    }

    #[test]
    fn reports_valid_and_invalid_translation_units() {
        let (_valid_temp, valid_status) = script_status("#!/bin/sh\nexit 0\n", Duration::from_secs(1));
        assert_eq!(valid_status, ValidationStatus::Valid);

        let (_invalid_temp, invalid_status) = script_status(
            "#!/bin/sh\necho parse-failed >&2\nexit 7\n",
            Duration::from_secs(1),
        );
        assert!(matches!(
            invalid_status,
            ValidationStatus::Invalid {
                exit_code: Some(7),
                ref stderr
            } if stderr.contains("parse-failed")
        ));
    }

    #[test]
    fn kills_a_translation_unit_at_the_configured_timeout() {
        let started = Instant::now();
        let (_temp, status) = script_status("#!/bin/sh\nexec sleep 2\n", Duration::from_millis(50));
        let elapsed = started.elapsed();
        assert!(matches!(status, ValidationStatus::TimedOut { .. }) && elapsed < Duration::from_secs(1));
    }

    #[test]
    fn timeout_kills_descendants_before_they_can_escape_side_effects() {
        let (temp, compiler) = executable_fixture(
            "#!/bin/sh\nmarker=$1\n( sleep 0.3; printf leaked > \"$marker\" ) &\nsleep 10\n",
        );
        let marker = temp.path().join("descendant-marker");
        let invocation = direct_command(
            compiler,
            vec![marker.to_string_lossy().into_owned()],
            temp.path().to_path_buf(),
        );

        match capture(&invocation, Duration::from_millis(50)) {
            ProcessOutcome::TimedOut { .. } => {}
            outcome => panic!("expected timeout, got {outcome:?}"),
        }
        thread::sleep(Duration::from_millis(500));
        assert!(
            !marker.exists(),
            "a descendant survived the timeout and wrote its marker"
        );
    }

    #[test]
    fn successful_parent_with_descendant_holding_pipes_does_not_hang() {
        let (temp, compiler) = executable_fixture("#!/bin/sh\nsleep 5 &\nexit 0\n");
        let invocation = direct_command(compiler, Vec::new(), temp.path().to_path_buf());
        let started = Instant::now();

        let outcome = capture(&invocation, Duration::from_secs(1));

        assert!(matches!(outcome, ProcessOutcome::Exited { success: true, .. }));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "capture waited for a descendant which retained the output pipes"
        );
    }

    #[test]
    fn reader_returns_a_partial_snapshot_when_eof_never_arrives() {
        struct BlockingReader {
            first_read: bool,
            blocked: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
        }

        impl Read for BlockingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.first_read {
                    self.first_read = true;
                    let partial = b"partial compiler output";
                    let count = partial.len().min(buffer.len());
                    buffer[..count].copy_from_slice(&partial[..count]);
                    return Ok(count);
                }
                if let Some(blocked) = self.blocked.take() {
                    let _send_result = blocked.send(());
                }
                let _release_result = self.release.recv();
                Ok(0)
            }
        }

        let (blocked_sender, blocked_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let reader = spawn_reader(
            BlockingReader {
                first_read: false,
                blocked: Some(blocked_sender),
                release: release_receiver,
            },
            MAX_CAPTURE_BYTES,
        );
        assert!(
            blocked_receiver.recv_timeout(Duration::from_secs(1)).is_ok(),
            "reader did not reach its deliberately blocked second read"
        );
        let started = Instant::now();

        let captured = join_reader(Some(reader));

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(captured.text, "partial compiler output");
        assert!(captured.truncated);
        drop(release_sender);
    }

    #[test]
    fn reader_snapshot_remains_bounded() {
        let reader = spawn_reader(std::io::Cursor::new(vec![b'x'; 128]), 16);

        let captured = join_reader(Some(reader));

        assert_eq!(captured.text, "x".repeat(16));
        assert!(captured.truncated);
    }

    #[test]
    fn reports_an_unavailable_compiler_without_panicking() {
        let temp = temp_dir();
        let status = validation_status(
            temp.path(),
            &temp.path().join("missing-clang"),
            Duration::from_secs(1),
        );
        assert!(matches!(status, ValidationStatus::Unavailable { .. }));
    }

    #[test]
    fn compiler_version_probe_reports_stdout_stderr_and_failures() {
        let assert_probe = |script, expected: &str| {
            assert_eq!(
                probe_script(script, Duration::from_secs(1)),
                Ok(expected.to_string())
            );
        };
        assert_probe("#!/bin/sh\necho 'clang stdout 1'\n", "clang stdout 1");
        assert_probe("#!/bin/sh\necho 'clang stderr 2' >&2\n", "clang stderr 2");

        let failed = expect_error(probe_script(
            "#!/bin/sh\necho failed >&2\nexit 9\n",
            Duration::from_secs(1),
        ));
        assert!(failed.contains("Some(9)"));

        let timed_out = expect_error(probe_script(
            "#!/bin/sh\nexec sleep 2\n",
            Duration::from_millis(20),
        ));
        assert!(timed_out.contains("timed out"));

        let temp = temp_dir();
        let unavailable = expect_error(probe_compiler_version(
            &temp.path().join("missing"),
            Duration::from_secs(1),
        ));
        assert!(unavailable.contains("failed to start"));
    }

    #[test]
    fn installed_clang_cannot_honor_database_write_destinations() {
        if Command::new("clang").arg("--version").output().is_err() {
            return;
        }

        let temp = temp_dir();
        let module = temp.path().join("module");
        create_dir(&module);
        write(
            module.join("module.modulemap"),
            "module Danger { header \"danger.h\" export * }\n",
        );
        write(module.join("danger.h"), "#define DANGER_VALUE 7\n");
        let source = temp.path().join("source.c");
        write(
            &source,
            "#include <danger.h>\nint danger(void) { return DANGER_VALUE; }\n",
        );

        let [attacker_cache, attacker_output, attacker_module, attacker_crash, attacker_index] = [
            "attacker-cache",
            "attacker.o",
            "attacker.pcm",
            "attacker-crash",
            "attacker-index",
        ]
        .map(|name| temp.path().join(name));
        let arguments = vec![
            "clang".to_string(),
            "-fmodules".to_string(),
            "-fimplicit-module-maps".to_string(),
            format!("-I{}", module.to_string_lossy()),
            format!("-fmodules-cache-path={}", attacker_cache.to_string_lossy()),
            format!("-fmodule-output={}", attacker_module.to_string_lossy()),
            format!("-fcrash-diagnostics-dir={}", attacker_crash.to_string_lossy()),
            "-index-store-path".to_string(),
            attacker_index.to_string_lossy().into_owned(),
            "-c".to_string(),
            source.to_string_lossy().into_owned(),
            "-o".to_string(),
            attacker_output.to_string_lossy().into_owned(),
        ];
        let command = CompileCommand {
            directory: temp.path().to_path_buf(),
            file: source,
            origin: CommandOrigin::Arguments(arguments.clone()),
            arguments,
            output: None,
        };

        let (invocation, status, _) = validate_translation_unit(
            &command,
            ClangLanguage::C,
            Path::new("clang"),
            Duration::from_secs(10),
        );
        assert_eq!(status, ValidationStatus::Valid);
        let invocation = invocation.unwrap_or_else(|| panic!("missing sanitized invocation"));
        let scratch = invocation
            .scratch_directory()
            .unwrap_or_else(|| panic!("missing scratch directory"))
            .to_path_buf();
        assert!(scratch.join("module-cache").is_dir());
        assert!(
            fs::read_dir(scratch.join("module-cache"))
                .unwrap_or_else(|error| panic!("read module cache: {error}"))
                .next()
                .is_some(),
            "the implicit module build did not exercise the isolated cache"
        );
        for path in [
            &attacker_cache,
            &attacker_output,
            &attacker_module,
            &attacker_crash,
            &attacker_index,
        ] {
            assert!(
                !path.exists(),
                "Clang wrote database-selected path {}",
                path.display()
            );
        }

        drop(invocation);
        assert!(!scratch.exists(), "owned Clang scratch directory was not removed");
    }
}
