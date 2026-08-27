//! Process liveness. Compiled without the `server` feature: both the
//! `coretempod sessions` daemon and the `tempo` CLI read the same `api.json`
//! and have to tell a live daemon from a file a dead one left behind, and the
//! CLI depends on `coretempo-core` with `default-features = false`.

/// Whether a process with `pid` exists.
///
/// `kill(pid, 0)` delivers no signal; it only reports whether the pid can be
/// addressed. `EPERM` means it exists and belongs to someone else, which is
/// still alive. Pid `0` is never a process — to `kill(2)` it means "everything
/// in my process group", so it must not be reported alive or a caller would
/// signal itself.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 delivers nothing; it only checks the pid exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use crate::pid::pid_alive;

    #[test]
    fn this_process_is_alive_and_a_free_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        // The kernel's pid space stops well below i32::MAX, so nothing can
        // hold this one.
        assert!(!pid_alive(2_147_483_646));
    }

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!pid_alive(0));
    }
}
