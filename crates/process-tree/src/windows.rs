//! Windows Job Object containment.
//!
//! This module is the only Windows unsafe boundary. A process is created with
//! `CREATE_SUSPENDED`, assigned to a kill-on-close Job Object, and resumed only
//! after assignment. `std::process::Command` closes the primary thread handle,
//! so the sole suspended initial thread is recovered through Tool Help.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::ptr::{null, null_mut};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenThread, ResumeThread, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

use crate::{CleanupIssue, CleanupStage, PlatformSpawnError, SpawnOptions, SpawnStage};

pub(crate) const SUPPORTS_GRACEFUL_TREE_TERMINATION: bool = false;
const ABORT_SPAWN_TIMEOUT: Duration = Duration::from_secs(1);
const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const INITIAL_THREAD_LOOKUP_TIMEOUT: Duration = Duration::from_millis(250);
const FORCED_EXIT_CODE: u32 = 1;

#[derive(Debug)]
pub(crate) struct Prepared {
    job: OwnedHandle,
}

#[derive(Debug)]
pub(crate) struct Containment {
    job: OwnedHandle,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ObservedExit(ExitStatus);

pub(crate) fn prepare(command: &mut Command, options: SpawnOptions) -> Result<Prepared, PlatformSpawnError> {
    let job = create_kill_on_close_job()?;
    command.creation_flags(options.windows_creation_flags | CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
    Ok(Prepared { job })
}

pub(crate) fn attach(prepared: Prepared, child: &mut Child) -> Result<Containment, PlatformSpawnError> {
    let job_handle = prepared.job.as_raw_handle().cast::<c_void>();
    let process_handle = child.as_raw_handle().cast::<c_void>();
    // SAFETY: both handles are live kernel handles owned by `prepared` and
    // `child`. The child is still suspended and therefore cannot create an
    // escaping descendant before assignment finishes.
    if unsafe { AssignProcessToJobObject(job_handle, process_handle) } == 0 {
        let source = io::Error::last_os_error();
        return Err(abort_attach(
            child,
            job_handle,
            SpawnStage::AssignProcess,
            source,
            false,
        ));
    }

    let thread_id = match locate_initial_thread(child.id()) {
        Ok(thread_id) => thread_id,
        Err(source) => {
            return Err(abort_attach(
                child,
                job_handle,
                SpawnStage::LocatePrimaryThread,
                source,
                true,
            ));
        }
    };
    let thread = match open_thread(thread_id) {
        Ok(thread) => thread,
        Err(source) => {
            return Err(abort_attach(
                child,
                job_handle,
                SpawnStage::OpenPrimaryThread,
                source,
                true,
            ));
        }
    };
    // SAFETY: `thread` is a live handle opened with THREAD_SUSPEND_RESUME for
    // the sole initial thread of our CREATE_SUSPENDED child.
    if unsafe { ResumeThread(thread.as_raw_handle().cast::<c_void>()) } == u32::MAX {
        let source = io::Error::last_os_error();
        return Err(abort_attach(
            child,
            job_handle,
            SpawnStage::ResumePrimaryThread,
            source,
            true,
        ));
    }

    Ok(Containment { job: prepared.job })
}

fn abort_attach(
    child: &mut Child,
    job_handle: HANDLE,
    stage: SpawnStage,
    source: io::Error,
    assigned: bool,
) -> PlatformSpawnError {
    PlatformSpawnError {
        stage,
        source,
        cleanup_issues: abort_suspended_spawn(child, Some(job_handle), assigned),
    }
}

pub(crate) fn observe_exit(
    child: &mut Child,
    _containment: &Containment,
) -> io::Result<Option<ObservedExit>> {
    child.try_wait().map(|status| status.map(ObservedExit))
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn send_graceful(_child: &Child, _containment: &Containment) -> io::Result<()> {
    Ok(())
}

pub(crate) fn send_force(_child: &Child, containment: &Containment) -> io::Result<()> {
    let job = containment.job.as_raw_handle().cast::<c_void>();
    // SAFETY: job is a live Job Object handle owned by containment.
    if unsafe { TerminateJobObject(job, FORCED_EXIT_CODE) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn reap(_child: &mut Child, observed: ObservedExit) -> io::Result<ExitStatus> {
    // Child::try_wait already reaped this Windows process while the Job Object
    // continued to provide a stable identity for descendant cleanup.
    Ok(observed.0)
}

pub(crate) fn tree_alive(containment: &Containment) -> io::Result<bool> {
    let mut accounting = MaybeUninit::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>::zeroed();
    let job = containment.job.as_raw_handle().cast::<c_void>();
    // SAFETY: accounting is writable storage of exactly the length passed;
    // job remains live for this call, and the information class matches the
    // destination structure.
    let result = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            accounting.as_mut_ptr().cast::<c_void>(),
            structure_size::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(),
            null_mut(),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful QueryInformationJobObject initialized accounting.
    Ok(unsafe { accounting.assume_init() }.ActiveProcesses != 0)
}

pub(crate) fn force_on_drop(child: &mut Child, containment: &Containment) {
    let job = containment.job.as_raw_handle().cast::<c_void>();
    // SAFETY: job remains a valid owned handle during ProcessTree::drop.
    let _ignored = unsafe { TerminateJobObject(job, FORCED_EXIT_CODE) };
    let _ignored = child.kill();
    let _ignored = child.try_wait();
    // Dropping containment closes the kill-on-close job as the final fallback.
}

fn create_kill_on_close_job() -> Result<OwnedHandle, PlatformSpawnError> {
    // SAFETY: null security attributes and name request an unnamed Job Object
    // with default security. The returned handle is checked before ownership.
    let raw_job = unsafe { CreateJobObjectW(null(), null()) };
    if raw_job.is_null() {
        return Err(PlatformSpawnError {
            stage: SpawnStage::CreateContainment,
            source: io::Error::last_os_error(),
            cleanup_issues: Vec::new(),
        });
    }
    // SAFETY: CreateJobObjectW returned a new, non-null owned kernel handle.
    let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
    let limits = MaybeUninit::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>::zeroed();
    // SAFETY: a zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a valid base;
    // all fields are integer/handle counters and only LimitFlags is consumed.
    let mut limits = unsafe { limits.assume_init() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: job is live, `limits` points to an initialized structure, and the
    // information class and byte size match that structure exactly.
    let result = unsafe {
        SetInformationJobObject(
            job.as_raw_handle().cast::<c_void>(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast::<c_void>(),
            structure_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
        )
    };
    if result == 0 {
        return Err(PlatformSpawnError {
            stage: SpawnStage::ConfigureContainment,
            source: io::Error::last_os_error(),
            cleanup_issues: Vec::new(),
        });
    }
    Ok(job)
}

fn locate_initial_thread(process_id: u32) -> io::Result<u32> {
    let started = Instant::now();
    loop {
        if let Some(thread_id) = snapshot_initial_thread(process_id)? {
            return Ok(thread_id);
        }
        if started.elapsed() >= INITIAL_THREAD_LOOKUP_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "suspended child's initial thread was absent from Tool Help snapshots",
            ));
        }
        thread::sleep(ABORT_POLL_INTERVAL);
    }
}

fn snapshot_initial_thread(process_id: u32) -> io::Result<Option<u32>> {
    // SAFETY: Tool Help accepts the ignored process ID as zero for a global
    // thread snapshot. The returned pseudo-error handle is checked.
    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw_snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the snapshot API returned a new valid owned handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw_snapshot) };
    let entry = MaybeUninit::<THREADENTRY32>::zeroed();
    // SAFETY: zero is valid for all THREADENTRY32 fields and the API requires
    // dwSize to be initialized before its first call.
    let mut entry = unsafe { entry.assume_init() };
    entry.dwSize = structure_size::<THREADENTRY32>();

    // SAFETY: snapshot is live and entry points to writable initialized storage.
    if unsafe { Thread32First(snapshot.as_raw_handle().cast::<c_void>(), &raw mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        if entry.th32OwnerProcessID == process_id {
            return Ok(Some(entry.th32ThreadID));
        }
        // SAFETY: same valid snapshot and entry invariants as Thread32First.
        if unsafe { Thread32Next(snapshot.as_raw_handle().cast::<c_void>(), &raw mut entry) } == 0 {
            break;
        }
    }
    Ok(None)
}

fn open_thread(thread_id: u32) -> io::Result<OwnedHandle> {
    // SAFETY: thread_id came from a live Tool Help entry for our child; no
    // handle inheritance is requested and access is limited to resume rights.
    let raw_thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if raw_thread.is_null() {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: OpenThread returned a new, non-null owned handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw_thread) })
    }
}

fn abort_suspended_spawn(child: &mut Child, job: Option<HANDLE>, assigned: bool) -> Vec<CleanupIssue> {
    let mut issues = Vec::new();
    if assigned {
        if let Some(job) = job {
            // SAFETY: caller supplies the still-live prepared Job Object.
            if unsafe { TerminateJobObject(job, FORCED_EXIT_CODE) } == 0 {
                issues.push(CleanupIssue::new(
                    CleanupStage::SendForce,
                    io::Error::last_os_error(),
                ));
            }
        }
    }
    if let Err(source) = child.kill() {
        if source.kind() != io::ErrorKind::InvalidInput {
            issues.push(CleanupIssue::new(CleanupStage::KillLeader, source));
        }
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {}
            Err(source) => {
                issues.push(CleanupIssue::new(CleanupStage::ReapLeader, source));
                break;
            }
        }
        if started.elapsed() >= ABORT_SPAWN_TIMEOUT {
            issues.push(CleanupIssue::timed_out(
                CleanupStage::ReapLeader,
                ABORT_SPAWN_TIMEOUT,
                "partially spawned process leader",
            ));
            break;
        }
        thread::sleep(ABORT_POLL_INTERVAL);
    }
    issues
}

#[allow(clippy::cast_possible_truncation)]
fn structure_size<T>() -> u32 {
    size_of::<T>() as u32
}
