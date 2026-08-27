//! Unix process-group containment.
//!
//! This module is the only Unix unsafe boundary. Calls are limited to POSIX
//! `waitid` and `kill`; every process identifier originates from a successfully
//! spawned direct child that was made its own process-group leader.

#![allow(unsafe_code)]

use std::io;
use std::mem::MaybeUninit;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};

use crate::{PlatformSpawnError, SpawnOptions};

pub(crate) const SUPPORTS_GRACEFUL_TREE_TERMINATION: bool = true;

#[derive(Debug)]
pub(crate) struct Prepared;

#[derive(Debug)]
pub(crate) struct Containment {
    process_group: libc::pid_t,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ObservedExit;

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn prepare(command: &mut Command, _options: SpawnOptions) -> Result<Prepared, PlatformSpawnError> {
    // Safe standard-library wrapper around setpgid(0, 0) in the child between
    // fork and exec. The spawned PID is therefore also the new PGID.
    command.process_group(0);
    Ok(Prepared)
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn attach(_prepared: Prepared, child: &mut Child) -> Result<Containment, PlatformSpawnError> {
    // Unix PID ranges fit pid_t; rejecting an unexpected conversion is not
    // useful because PlatformSpawnError requires a stage that cannot occur in
    // practice on supported Unix kernels.
    #[allow(clippy::cast_possible_wrap)]
    let process_group = child.id() as libc::pid_t;
    Ok(Containment { process_group })
}

#[allow(clippy::cast_sign_loss)]
pub(crate) fn observe_exit(
    _child: &mut Child,
    containment: &Containment,
) -> io::Result<Option<ObservedExit>> {
    loop {
        let mut information = MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: `information` points to writable storage for siginfo_t, the
        // PID is our live direct child, and WNOWAIT intentionally preserves the
        // waitable child identity until the process group has been signalled.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                containment.process_group as libc::id_t,
                information.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: successful waitid initialized the siginfo_t object. POSIX
            // specifies si_pid == 0 when WNOHANG found no waitable child.
            let process = unsafe { information.assume_init().si_pid() };
            return Ok((process != 0).then_some(ObservedExit));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) fn send_graceful(_child: &Child, containment: &Containment) -> io::Result<()> {
    signal_group(containment, libc::SIGTERM)
}

pub(crate) fn send_force(_child: &Child, containment: &Containment) -> io::Result<()> {
    signal_group(containment, libc::SIGKILL)
}

pub(crate) fn reap(child: &mut Child, _observed: ObservedExit) -> io::Result<ExitStatus> {
    // waitid(WNOWAIT) already proved that this direct child is waitable, so the
    // standard-library wait only collects a status and cannot wait for runtime.
    child.wait()
}

pub(crate) fn tree_alive(containment: &Containment) -> io::Result<bool> {
    // SAFETY: a negative, nonzero PID addresses the dedicated process group;
    // signal 0 performs existence/permission checking without delivering a
    // signal. No memory is accessed.
    let result = unsafe { libc::kill(-containment.process_group, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error),
    }
}

pub(crate) fn force_on_drop(child: &mut Child, containment: &Containment) {
    let _ignored = signal_group(containment, libc::SIGKILL);
    let _ignored = child.kill();
    let _ignored = child.try_wait();
}

fn signal_group(containment: &Containment, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: the dedicated PGID was captured from our successfully spawned
    // group leader. Negating it targets only that group. POSIX kill does not
    // dereference pointers and is safe for every integer signal value used here.
    let result = unsafe { libc::kill(-containment.process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // macOS reports EPERM when an anchored group contains only a zombie
        // leader and therefore has no signalable member. The bounded
        // post-reap tree-existence check remains authoritative: if an
        // unsignalable descendant is actually present it produces a surfaced
        // verification timeout instead of being silently accepted.
        Some(libc::ESRCH | libc::EPERM) => Ok(()),
        _ => Err(error),
    }
}
