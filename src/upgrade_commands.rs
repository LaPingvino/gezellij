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
    host_fabric::upgrade::{binary_replaced, exec_manifest_path, server_pid},
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
    let session = resolve_session(&opts, &cli);
    let pid = match server_pid(&session) {
        Some(pid) => pid,
        None => fail(format!(
            "Cannot find the server pid of session '{}'. Its server predates upgrade support; \
             restart the session once with this binary and it will be upgradable from then on.",
            session
        )),
    };
    let replaced = binary_replaced(pid).unwrap_or(false);
    let overridden = std::env::var_os("GEZELLIJ_UPGRADE_BINARY").is_some();
    if !replaced && !cli.force && !overridden {
        println!(
            "The server of session '{}' (pid {}) still runs the binary that is on disk ({}); nothing to upgrade.\n\
             Use --force to re-exec it anyway.",
            session,
            pid,
            exe_of(pid)
        );
        return;
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
    // SIGUSR2 asks the server to snapshot and exec
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR2) };
    if sent != 0 {
        fail(format!(
            "Could not signal the server (pid {}): {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    // The successor removes the manifest once it has read it and the socket reappears when it
    // is listening. The pid must stay the same: that is the whole point.
    let timeout = Duration::from_secs(cli.timeout.unwrap_or(30));
    let started = Instant::now();
    let mut saw_manifest = false;
    loop {
        if manifest.exists() {
            saw_manifest = true;
        }
        let alive = std::path::Path::new(&format!("/proc/{}", pid)).exists();
        if !alive {
            fail(format!(
                "The server (pid {}) exited during the upgrade. Check the log at /tmp/zellij-<uid>/zellij-log/zellij.log; \
                 the session's resurrection layout is on disk, so `zellij attach {}` can rebuild it.",
                pid, session
            ));
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
            return;
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
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
