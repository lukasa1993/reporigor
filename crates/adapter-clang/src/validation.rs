use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use reporigor_core::SourceFile;
use reporigor_process_tree::{CleanupPolicy, ProcessTree, SpawnError, WaitReason};

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
pub enum ValidationStatus {
    Valid,
    Invalid { exit_code: Option<i32>, stderr: String },
    TimedOut { timeout: Duration, stderr: String },
    Unavailable { message: String },
    Rejected { message: String },
    NotValidated { message: String },
}

/// A compilation-database record resolved against the requested project, with
/// both original and actual validation command provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationUnit {
    pub command_index: usize,
    pub command: CompileCommand,
    pub language: Option<ClangLanguage>,
    pub source: Option<SourceFile>,
    pub invocation: Option<SanitizedCommand>,
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
    let invocation = SanitizedCommand::direct(
        compiler.to_path_buf(),
        vec!["--version".to_string()],
        std::env::current_dir().map_err(|error| error.to_string())?,
    );
    match run_bounded_capture(&invocation, timeout, Some(MAX_CAPTURE_BYTES), MAX_CAPTURE_BYTES) {
        ProcessOutcome::Exited {
            success: true,
            stdout,
            stderr,
            exit_code: _,
        } => Ok(if stdout.text.trim().is_empty() {
            &stderr.text
        } else {
            &stdout.text
        }
        .lines()
        .next()
        .unwrap_or("unknown Clang version")
        .trim()
        .to_string()),
        ProcessOutcome::Exited {
            exit_code, stderr, ..
        } => Err(format!(
            "{} --version exited with {:?}: {}",
            compiler.display(),
            exit_code,
            compact(&stderr.text)
        )),
        ProcessOutcome::TimedOut { .. } => Err(format!(
            "{} --version timed out after {:.3}s",
            compiler.display(),
            timeout.as_secs_f64()
        )),
        ProcessOutcome::Unavailable(message) => Err(message),
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
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.arguments)
        .current_dir(&invocation.directory)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(if stdout_limit.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if let Some(scratch) = invocation.scratch_directory() {
        // Clang and its platform libraries consult different temporary-file
        // variables. Point all of them at the same RAII-owned directory so a
        // syntax-only process cannot spill temporary artifacts into the
        // project or the user's shared compiler caches.
        command
            .env("TMPDIR", scratch)
            .env("TMP", scratch)
            .env("TEMP", scratch);
    }
    let mut child = match ProcessTree::spawn(&mut command) {
        Ok(child) => child,
        Err(error) => {
            return ProcessOutcome::Unavailable(spawn_failure_message(invocation, &error));
        }
    };

    let Some(stderr) = child.take_stderr() else {
        return ProcessOutcome::Unavailable(missing_pipe_message(&mut child, invocation, "stderr"));
    };
    let stdout = match stdout_limit {
        Some(limit) => {
            let Some(stdout) = child.take_stdout() else {
                return ProcessOutcome::Unavailable(missing_pipe_message(&mut child, invocation, "stdout"));
            };
            Some((stdout, limit))
        }
        None => None,
    };
    let stderr_reader = Some(spawn_reader(stderr, stderr_limit));
    let stdout_reader = stdout.map(|(reader, limit)| spawn_reader(reader, limit));
    let wait_result = child.wait_bounded(timeout, CleanupPolicy::default());
    let stdout = join_reader(stdout_reader);
    let stderr = join_reader(stderr_reader);
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
    let base = format!("failed to start {}: {error}", invocation.program.display());
    let cleanup = error
        .cleanup_issues()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if cleanup.is_empty() {
        base
    } else {
        format!("{base}; {cleanup}")
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
    let snapshot = Arc::new(Mutex::new(CaptureBuffer::default()));
    let thread_snapshot = Arc::clone(&snapshot);
    let (completed_sender, completed) = mpsc::channel();
    let _reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
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
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;
    use crate::CommandOrigin;

    fn make_script(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let script = temp.path().join("fake-clang");
        fs::write(&script, contents).unwrap_or_else(|error| panic!("write script: {error}"));
        let mut permissions = fs::metadata(&script)
            .unwrap_or_else(|error| panic!("metadata: {error}"))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap_or_else(|error| panic!("permissions: {error}"));
        (temp, script)
    }

    fn command(directory: &Path) -> CompileCommand {
        let arguments = vec!["clang".to_string(), "source.c".to_string(), "-c".to_string()];
        CompileCommand {
            directory: directory.to_path_buf(),
            file: directory.join("source.c"),
            origin: CommandOrigin::Arguments(arguments.clone()),
            arguments,
            output: None,
        }
    }

    #[test]
    fn reports_valid_and_invalid_translation_units() {
        let (temp, valid) = make_script("#!/bin/sh\nexit 0\n");
        fs::write(temp.path().join("source.c"), "int main(void) { return 0; }")
            .unwrap_or_else(|error| panic!("write source: {error}"));
        let (_, valid_status, _) = validate_translation_unit(
            &command(temp.path()),
            ClangLanguage::C,
            &valid,
            Duration::from_secs(1),
        );
        assert_eq!(valid_status, ValidationStatus::Valid);

        let (invalid_temp, invalid) = make_script("#!/bin/sh\necho parse-failed >&2\nexit 7\n");
        let (_, invalid_status, _) = validate_translation_unit(
            &command(invalid_temp.path()),
            ClangLanguage::C,
            &invalid,
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
        let (temp, compiler) = make_script("#!/bin/sh\nexec sleep 2\n");
        let started = Instant::now();
        let (_, status, _) = validate_translation_unit(
            &command(temp.path()),
            ClangLanguage::C,
            &compiler,
            Duration::from_millis(50),
        );
        assert!(matches!(status, ValidationStatus::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_kills_descendants_before_they_can_escape_side_effects() {
        let (temp, compiler) =
            make_script("#!/bin/sh\nmarker=$1\n( sleep 0.3; printf leaked > \"$marker\" ) &\nsleep 10\n");
        let marker = temp.path().join("descendant-marker");
        let invocation = SanitizedCommand::direct(
            compiler,
            vec![marker.to_string_lossy().into_owned()],
            temp.path().to_path_buf(),
        );

        let outcome = run_bounded_capture(
            &invocation,
            Duration::from_millis(50),
            Some(MAX_CAPTURE_BYTES),
            MAX_CAPTURE_BYTES,
        );
        assert!(matches!(outcome, ProcessOutcome::TimedOut { .. }));
        thread::sleep(Duration::from_millis(500));
        assert!(
            !marker.exists(),
            "a descendant survived the timeout and wrote its marker"
        );
    }

    #[test]
    fn successful_parent_with_descendant_holding_pipes_does_not_hang() {
        let (temp, compiler) = make_script("#!/bin/sh\nsleep 5 &\nexit 0\n");
        let invocation = SanitizedCommand::direct(compiler, Vec::new(), temp.path().to_path_buf());
        let started = Instant::now();

        let outcome = run_bounded_capture(
            &invocation,
            Duration::from_secs(1),
            Some(MAX_CAPTURE_BYTES),
            MAX_CAPTURE_BYTES,
        );

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
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let (_, status, _) = validate_translation_unit(
            &command(temp.path()),
            ClangLanguage::C,
            &temp.path().join("missing-clang"),
            Duration::from_secs(1),
        );
        assert!(matches!(status, ValidationStatus::Unavailable { .. }));
    }

    #[test]
    fn installed_clang_cannot_honor_database_write_destinations() {
        if Command::new("clang").arg("--version").output().is_err() {
            return;
        }

        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let module = temp.path().join("module");
        fs::create_dir(&module).unwrap_or_else(|error| panic!("create module: {error}"));
        fs::write(
            module.join("module.modulemap"),
            "module Danger { header \"danger.h\" export * }\n",
        )
        .unwrap_or_else(|error| panic!("write module map: {error}"));
        fs::write(module.join("danger.h"), "#define DANGER_VALUE 7\n")
            .unwrap_or_else(|error| panic!("write module header: {error}"));
        let source = temp.path().join("source.c");
        fs::write(
            &source,
            "#include <danger.h>\nint danger(void) { return DANGER_VALUE; }\n",
        )
        .unwrap_or_else(|error| panic!("write source: {error}"));

        let attacker_cache = temp.path().join("attacker-cache");
        let attacker_output = temp.path().join("attacker.o");
        let attacker_module = temp.path().join("attacker.pcm");
        let attacker_crash = temp.path().join("attacker-crash");
        let attacker_index = temp.path().join("attacker-index");
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
