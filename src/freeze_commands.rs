//! `zellij freeze` / `zellij thaw`: pause and resume a session's processes with the cgroup v2
//! freezer (Gezellij Phase 2).
//!
//! The server puts every pane in its own cgroup and records the session's cgroup root in the
//! session cache; this command talks to sysfs directly, so it works even when the server is busy.

use zellij_utils::{
    cli::{CliArgs, FreezeCli},
    envs,
    host_fabric::cgroups::{freeze_state, set_frozen, PaneCgroups},
    sessions::{get_active_session, get_sessions, session_exists, ActiveSession},
};

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("{}", message);
    std::process::exit(2);
}

fn resolve_session(opts: &CliArgs, cli: &FreezeCli) -> String {
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

/// Accepts `terminal_3`, `3`; rejects plugin panes (they run inside the server, not a cgroup).
fn parse_terminal_id(pane_id: &str) -> u32 {
    let digits = pane_id.strip_prefix("terminal_").unwrap_or(pane_id);
    match digits.parse::<u32>() {
        Ok(id) => id,
        Err(_) => fail(format!(
            "Invalid pane id '{}': expected terminal_<n> or a bare number (plugin panes cannot be frozen)",
            pane_id
        )),
    }
}

pub(crate) fn run(opts: CliArgs, cli: FreezeCli, freeze: bool) {
    let session = resolve_session(&opts, &cli);
    let tree = match PaneCgroups::from_session_record(&session) {
        Ok(Some(tree)) => tree,
        Ok(None) => fail(format!(
            "Session '{}' has no pane cgroups. Its server was started without cgroup v2 \
             delegation (or by an older version), so it cannot be frozen.",
            session
        )),
        Err(e) => fail(format!(
            "Could not look up the cgroups of '{}': {}",
            session, e
        )),
    };

    let panes = match tree.list_panes() {
        Ok(panes) => panes,
        Err(e) => fail(format!("Could not list the panes of '{}': {}", session, e)),
    };
    let selected: Vec<(u32, std::path::PathBuf)> = match cli.pane_id.as_deref() {
        Some(pane_id) => {
            let wanted = parse_terminal_id(pane_id);
            let found: Vec<_> = panes.into_iter().filter(|(id, _)| *id == wanted).collect();
            if found.is_empty() {
                fail(format!(
                    "Pane terminal_{} of '{}' has no cgroup (not a command/shell pane, or already gone)",
                    wanted, session
                ));
            }
            found
        },
        None => panes,
    };
    if selected.is_empty() {
        println!("Session '{}' has no terminal panes to act on.", session);
        return;
    }

    if cli.status {
        println!("{:<12} {:<8} {}", "PANE", "STATE", "POPULATED");
        for (id, dir) in &selected {
            match freeze_state(dir) {
                Ok(state) => println!(
                    "terminal_{:<3} {:<8} {}",
                    id,
                    if state.frozen { "frozen" } else { "running" },
                    if state.populated { "yes" } else { "no" }
                ),
                Err(e) => println!("terminal_{:<3} {:<8} ({})", id, "?", e),
            }
        }
        return;
    }

    let verb = if freeze { "Froze" } else { "Thawed" };
    let mut failures = 0;
    for (id, dir) in &selected {
        if let Err(e) = set_frozen(dir, freeze) {
            failures += 1;
            eprintln!("terminal_{}: {}", id, e);
        }
    }
    let count = selected.len() - failures;
    let scope = match cli.pane_id.as_deref() {
        Some(_) => "pane".to_string(),
        None => format!("all {} panes", selected.len()),
    };
    if failures == 0 {
        println!("{} {} of session '{}'.", verb, scope, session);
        if freeze {
            println!("Thaw with: zellij thaw {}", session);
        }
    } else {
        eprintln!(
            "{} {} of {} panes of session '{}' ({} failed).",
            verb,
            count,
            selected.len(),
            session,
            failures
        );
        std::process::exit(1);
    }
}
