//! Upgrade awareness (interim safety net until the Phase 3 live handover lands).
//!
//! When a package manager replaces the `zellij`/`gezellij` binary, every running server keeps
//! executing its old, unlinked image. That is harmless by itself, but it means:
//!
//! * a newer client may no longer be able to talk to the old server (if the client-server
//!   contract changed), and
//! * the only binary that *could* still talk to it is gone.
//!
//! Linux makes the situation detectable for free: `/proc/<pid>/exe` of such a server resolves to
//! `<path> (deleted)`. The server records its pid next to its socket
//! (`<socket dir>/<session>.server-pid`) so any CLI can check this and warn the user before they
//! find out the hard way.

use crate::consts::ZELLIJ_SOCK_DIR;
use std::fs;
use std::io;
use std::path::PathBuf;

const PID_RECORD_SUFFIX: &str = ".server-pid";

fn pid_record_file(session_name: &str) -> PathBuf {
    ZELLIJ_SOCK_DIR.join(format!("{}{}", session_name, PID_RECORD_SUFFIX))
}

/// Called by the server once it is listening.
pub fn record_server_pid(session_name: &str) -> io::Result<()> {
    fs::create_dir_all(&*ZELLIJ_SOCK_DIR)?;
    fs::write(
        pid_record_file(session_name),
        std::process::id().to_string(),
    )
}

/// Called by the server on its way out.
pub fn remove_server_pid_record(session_name: &str) {
    let _ = fs::remove_file(pid_record_file(session_name));
}

/// The pid a server recorded for `session_name`, if any and if that process still exists.
pub fn server_pid(session_name: &str) -> Option<u32> {
    let pid: u32 = fs::read_to_string(pid_record_file(session_name))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if PathBuf::from(format!("/proc/{}", pid)).is_dir() {
        Some(pid)
    } else {
        None
    }
}

/// Whether the executable of process `pid` has been replaced or removed on disk since it started
/// (Linux: `/proc/<pid>/exe` then ends in ` (deleted)`).
pub fn binary_replaced(pid: u32) -> io::Result<bool> {
    let exe = fs::read_link(format!("/proc/{}/exe", pid))?;
    Ok(exe.to_string_lossy().ends_with(" (deleted)"))
}

/// One-line human verdict for a session, `None` when nothing is known or nothing is wrong.
pub fn session_upgrade_warning(session_name: &str) -> Option<String> {
    let pid = server_pid(session_name)?;
    match binary_replaced(pid) {
        Ok(true) => Some(format!(
            "the server of session '{}' (pid {}) runs a binary that has since been replaced on disk; \
             new clients may not be able to attach until the session is restarted (or handed over)",
            session_name, pid
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_binary_is_not_reported_as_replaced() {
        // the test binary is on disk while it runs
        if PathBuf::from("/proc/self/exe").exists() {
            assert_eq!(binary_replaced(std::process::id()).unwrap(), false);
        }
    }

    #[test]
    fn unknown_pid_has_no_record() {
        assert_eq!(server_pid("gezellij-no-such-session-for-tests"), None);
    }
}
