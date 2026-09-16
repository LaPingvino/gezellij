//! `zellij upgrade-server`: replace a running session's server with the installed binary *in
//! place*, keeping every process in its panes alive (Gezellij Phase 3, exec variant).
//!
//! The CLI does very little itself: it finds the server's pid (recorded beside the socket),
//! sends `SIGUSR2`, and waits. The server snapshots its layout, hands its PTY masters across an
//! `execve` of the new binary, and the successor rebuilds the session around them (see
//! `HANDOVER_DESIGN.md` and `zellij_utils::host_fabric::upgrade`).

use std::time::{Duration, Instant};
use zellij_utils::{
    cli::{CliArgs, UpgradeServerCli},
    envs,
    host_fabric::upgrade::{
        binary_replaced, exec_error_path, exec_manifest_path, server_pid, take_exec_error,
    },
    sessions::{get_active_session, get_sessions, session_exists, ActiveSession},
};

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("{}", message);
    std::process::exit(2);
}

fn resolve_session(opts: &CliArgs, cli: &UpgradeServerCli) -> String {
    if let Some(name) = cli
        .session_name
        .clone()
        .or_else(|| opts.session.clone())
        .or_else(|| envs::get_session_name().ok())
    {
        if !session_exists(&name).unwrap_or(false) {
            fail(format!("No active session named '{}'", name));
        }
        return name;
    }
    match get_active_session() {
        ActiveSession::None => fail("There is no active session!"),
        ActiveSession::One(name) => name,
        ActiveSession::Many => {
            let names: Vec<String> = get_sessions()
                .unwrap_or_default()
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            fail(format!(
                "Several sessions are active, please name one: {}",
                names.join(", ")
            ))
        },
    }
}

fn exe_of(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{}/exe", pid))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".to_string())
}

pub(crate) fn run(opts: CliArgs, cli: UpgradeServerCli) {
    if cli.all {
        run_all(&cli);
        return;
    }
    let session = resolve_session(&opts, &cli);
    if !upgrade_one(&session, cli.force, cli.timeout) {
        std::process::exit(1);
    }
}

/// Upgrade every session whose server is running a binary that is no longer on disk.
fn run_all(cli: &UpgradeServerCli) {
    let sessions: Vec<String> = get_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if sessions.is_empty() {
        println!("No active sessions.");
        return;
    }
    let overridden = std::env::var_os("GEZELLIJ_UPGRADE_BINARY").is_some();
    let mut stale = vec![];
    let mut current = vec![];
    let mut unknown = vec![];
    for session in sessions {
        match server_pid(&session) {
            Some(pid) => {
                if binary_replaced(pid).unwrap_or(false) || cli.force || overridden {
                    stale.push(session);
                } else {
                    current.push(session);
                }
            },
            None => unknown.push(session),
        }
    }
    stale.sort();
    current.sort();
    unknown.sort();
    if !current.is_empty() {
        println!(
            "Already up to date: {} (use --force to re-exec them anyway)",
            current.join(", ")
        );
    }
    if !unknown.is_empty() {
        println!(
            "Cannot upgrade (server predates upgrade support, restart the session once): {}",
            unknown.join(", ")
        );
    }
    if stale.is_empty() {
        println!("Nothing to upgrade.");
        return;
    }
    let total = stale.len();
    let mut upgraded = 0;
    for session in stale {
        if upgrade_one(&session, cli.force, cli.timeout) {
            upgraded += 1;
        }
    }
    println!("Upgraded {}/{} sessions.", upgraded, total);
    if upgraded != total {
        std::process::exit(1);
    }
}

/// Returns whether the session was upgraded. Never exits, so `--all` can carry on.
fn upgrade_one(session: &str, force: bool, timeout_secs: Option<u64>) -> bool {
    let session = session.to_string();
    let pid = match server_pid(&session) {
        Some(pid) => pid,
        None => {
            eprintln!(
                "Cannot find the server pid of session '{}'. Its server predates upgrade support; \
                 restart the session once with this binary and it will be upgradable from then on.",
                session
            );
            return false;
        },
    };
    let replaced = binary_replaced(pid).unwrap_or(false);
    let overridden = std::env::var_os("GEZELLIJ_UPGRADE_BINARY").is_some();
    if !replaced && !force && !overridden {
        println!(
            "The server of session '{}' (pid {}) still runs the binary that is on disk ({}); nothing to upgrade.\n\
             Use --force to re-exec it anyway.",
            session,
            pid,
            exe_of(pid)
        );
        return true;
    }
    let old_exe = exe_of(pid);
    println!(
        "Upgrading the server of session '{}' (pid {}, running {}) in place…",
        session, pid, old_exe
    );
    println!(
        "Attached clients will be disconnected; re-attach with `zellij attach {}`.",
        session
    );

    let manifest = exec_manifest_path(&session);
    let _ = std::fs::remove_file(&manifest);
    let _ = std::fs::remove_file(exec_error_path(&session));
    // SIGUSR2 asks the server to snapshot and exec
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR2) };
    if sent != 0 {
        eprintln!(
            "Could not signal the server of '{}' (pid {}): {}",
            session,
            pid,
            std::io::Error::last_os_error()
        );
        return false;
    }

    // The successor removes the manifest once it has read it and the socket reappears when it
    // is listening. The pid must stay the same: that is the whole point.
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(30));
    let started = Instant::now();
    let mut saw_manifest = false;
    loop {
        if manifest.exists() {
            saw_manifest = true;
        }
        if let Some(reason) = take_exec_error(&session) {
            eprintln!("Could not upgrade session '{}': {}", session, reason);
            eprintln!("Its server is untouched and still running; nothing was lost.");
            return false;
        }
        let alive = std::path::Path::new(&format!("/proc/{}", pid)).exists();
        if !alive {
            eprintln!(
                "The server of '{}' (pid {}) exited during the upgrade. Check the server log; \
                 its resurrection layout is on disk, so `zellij attach {}` can rebuild it.",
                session, pid, session
            );
            return false;
        }
        let new_exe = exe_of(pid);
        let upgraded = new_exe != old_exe && !new_exe.ends_with(" (deleted)");
        if upgraded
            && saw_manifest
            && !manifest.exists()
            && session_exists(&session).unwrap_or(false)
        {
            println!(
                "Done: session '{}' is now served by {} (pid {} unchanged).",
                session, new_exe, pid
            );
            return true;
        }
        if started.elapsed() > timeout {
            eprintln!(
                "Timed out after {}s waiting for the upgraded server (pid {} now runs {}; manifest seen: {}).\n\
                 The old server keeps running if the exec never happened; see the server log for details.",
                timeout.as_secs(),
                pid,
                new_exe,
                saw_manifest
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
