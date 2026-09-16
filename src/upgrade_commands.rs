//! `zellij upgrade-server`: replace a running session's server with the installed binary *in
//! place*, keeping every process in its panes alive (Gezellij Phase 3, exec variant).
//!
//! The CLI does very little itself: it finds the server's pid (recorded beside the socket),
//! sends `SIGUSR2`, and waits. The server snapshots its layout, hands its PTY masters across an
//! `execve` of the new binary, and the successor rebuilds the session around them (see
//! `HANDOVER_DESIGN.md` and `zellij_utils::host_fabric::upgrade`).

use std::path::PathBuf;
use std::time::{Duration, Instant};
use zellij_utils::{
    cli::{CliArgs, UpgradeServerCli},
    consts::ZELLIJ_SOCK_DIR,
    envs,
    host_fabric::upgrade::{
        binary_replaced, exec_error_path_in, exec_manifest_path_in, server_pid, server_pid_in,
        stranded_sessions,
    },
    sessions::{get_active_session, get_sessions, session_exists, ActiveSession},
};

/// A server we could ask to replace itself.
struct Candidate {
    session: String,
    pid: u32,
    /// The socket directory *that server* uses. Normally ours; for a session stranded by a
    /// client-server contract change it is an older `contract_version_N` directory, and that is
    /// where it writes its handover manifest and any failure reason.
    socket_dir: PathBuf,
    stranded: bool,
}

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
    if !upgrade_named(&session, cli.force, cli.timeout) {
        std::process::exit(1);
    }
}

/// Upgrade every session whose server is running a binary that is no longer on disk - including
/// sessions stranded in an older contract directory, which `get_sessions` cannot see but whose
/// pid record proves they understand the handover.
fn run_all(cli: &UpgradeServerCli) {
    let overridden = std::env::var_os("GEZELLIJ_UPGRADE_BINARY").is_some();
    let mut stale: Vec<Candidate> = vec![];
    let mut current = vec![];
    let mut unknown = vec![];
    for (session, _) in get_sessions().unwrap_or_default() {
        match server_pid(&session) {
            Some(pid) => {
                if binary_replaced(pid).unwrap_or(false) || cli.force || overridden {
                    stale.push(Candidate {
                        session,
                        pid,
                        socket_dir: ZELLIJ_SOCK_DIR.to_path_buf(),
                        stranded: false,
                    });
                } else {
                    current.push(session);
                }
            },
            None => unknown.push(session),
        }
    }
    // A stranded session is worth rescuing whenever the binary at its recorded path really was
    // replaced - re-exec'ing the same old binary would leave it exactly where it is.
    for candidate in stranded_candidates() {
        if binary_replaced(candidate.pid).unwrap_or(false) || overridden {
            stale.push(candidate);
        }
    }
    if stale.is_empty() && current.is_empty() && unknown.is_empty() {
        println!("No active sessions.");
        return;
    }
    stale.sort_by(|a, b| a.session.cmp(&b.session));
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
    report_stranded_sessions();
    if stale.is_empty() {
        println!("Nothing to upgrade.");
        return;
    }
    let total = stale.len();
    let mut upgraded = 0;
    for candidate in &stale {
        if upgrade_one(candidate, cli.force, cli.timeout) {
            upgraded += 1;
        }
    }
    println!("Upgraded {}/{} sessions.", upgraded, total);
    if upgraded != total {
        std::process::exit(1);
    }
}

/// Sessions left behind in an older client-server contract directory, which `list-sessions`
/// cannot see. Reporting them is the most we can safely do: a server that never recorded a pid
/// predates the handover and must not be signalled (the default action for `SIGUSR2` is to kill
/// the process, which would take the session with it).
fn report_stranded_sessions() {
    let stranded = zellij_utils::host_fabric::upgrade::stranded_sessions();
    if stranded.is_empty() {
        return;
    }
    println!();
    println!(
        "{} session(s) from an older version are still running but invisible to this binary,",
        stranded.len()
    );
    println!("because it speaks a different client-server contract:");
    for (name, socket, pid) in &stranded {
        match pid {
            Some(pid) => println!("    {}  (pid {}, socket {})", name, pid, socket.display()),
            None => println!("    {}  (socket {})", name, socket.display()),
        }
    }
    println!();
    println!("They keep running, and whatever is in them is still alive. To pick one up you need");
    println!("the binary that started it, e.g. an older `zellij`, and `attach` with that. Nothing");
    println!("here can migrate them in place: only a server that recorded a pid understands the");
    println!("handover, and signalling one that does not would kill it.");
    println!();
}

/// Returns whether the session was upgraded. Never exits, so `--all` can carry on.
fn upgrade_named(session: &str, force: bool, timeout_secs: Option<u64>) -> bool {
    let pid = match server_pid(session) {
        Some(pid) => pid,
        None => {
            // It may still be reachable: a session stranded in an older contract directory is
            // invisible to `session_exists`, but its pid record is right next to its own socket.
            if let Some(candidate) = stranded_candidates()
                .into_iter()
                .find(|c| c.session == session)
            {
                return upgrade_one(&candidate, force, timeout_secs);
            }
            eprintln!(
                "Cannot find the server pid of session '{}'. Its server predates upgrade support; \
                 restart the session once with this binary and it will be upgradable from then on.",
                session
            );
            return false;
        },
    };
    upgrade_one(
        &Candidate {
            session: session.to_string(),
            pid,
            socket_dir: ZELLIJ_SOCK_DIR.to_path_buf(),
            stranded: false,
        },
        force,
        timeout_secs,
    )
}

/// Sessions stranded in an older contract directory whose server recorded a pid - the record is
/// itself the proof that it understands the handover and may safely be signalled.
fn stranded_candidates() -> Vec<Candidate> {
    stranded_sessions()
        .into_iter()
        .filter_map(|(session, socket, _)| {
            let socket_dir = socket.parent()?.to_path_buf();
            let pid = server_pid_in(&socket_dir, &session)?;
            Some(Candidate {
                session,
                pid,
                socket_dir,
                stranded: true,
            })
        })
        .collect()
}

fn upgrade_one(candidate: &Candidate, force: bool, timeout_secs: Option<u64>) -> bool {
    let Candidate {
        session,
        pid,
        socket_dir,
        stranded,
    } = candidate;
    let (session, pid, stranded) = (session.clone(), *pid, *stranded);
    let replaced = binary_replaced(pid).unwrap_or(false);
    let overridden = std::env::var_os("GEZELLIJ_UPGRADE_BINARY").is_some();
    if !replaced && !force && !overridden {
        if stranded {
            // Re-exec'ing would just start the same old binary again, leaving it stranded.
            eprintln!(
                "Session '{}' (pid {}) is stranded in an older contract directory, but the binary \
                 it runs ({}) is still the one on disk, so replacing itself would change nothing. \
                 Install the newer build at that path, then run this again.",
                session,
                pid,
                exe_of(pid)
            );
            return false;
        }
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

    if stranded {
        println!(
            "  (this session was stranded in {} - after the upgrade it rejoins the ones this \
             binary can see)",
            socket_dir.display()
        );
    }
    let manifest = exec_manifest_path_in(socket_dir, &session);
    let error_path = exec_error_path_in(socket_dir, &session);
    let _ = std::fs::remove_file(&manifest);
    let _ = std::fs::remove_file(&error_path);
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
        if let Some(reason) = std::fs::read_to_string(&error_path).ok().map(|r| {
            let _ = std::fs::remove_file(&error_path);
            r
        }) {
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
