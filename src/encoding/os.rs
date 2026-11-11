//! Small OS specific helpers (process priority, suspend/resume).
//!
//! These are the only places using `unsafe` (plain libc calls with checked
//! arguments).

/// Whether pausing a running process is supported on this platform.
pub const CAN_SUSPEND: bool = cfg!(unix);

/// Lower the scheduling priority of `pid` (Unix nice value 0..=19).
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn set_nice(pid: u32, nice: i32) -> bool {
    let nice = nice.clamp(0, 19);
    #[allow(clippy::useless_conversion, irrefutable_let_patterns)]
    let Ok(pid) = libc::id_t::try_from(pid) else {
        return false;
    };
    // SAFETY: setpriority only reads its integer arguments.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) == 0 }
}

#[cfg(not(unix))]
pub fn set_nice(_pid: u32, _nice: i32) -> bool {
    false
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn signal(pid: u32, sig: libc::c_int) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill only sends a signal to the given (positive) pid.
    unsafe { libc::kill(pid, sig) == 0 }
}

/// Suspend a process (SIGSTOP). Returns `false` if unsupported/failed.
#[cfg(unix)]
pub fn suspend(pid: u32) -> bool {
    signal(pid, libc::SIGSTOP)
}

#[cfg(unix)]
pub fn resume(pid: u32) -> bool {
    signal(pid, libc::SIGCONT)
}

#[cfg(not(unix))]
pub fn suspend(_pid: u32) -> bool {
    false
}

#[cfg(not(unix))]
pub fn resume(_pid: u32) -> bool {
    false
}
