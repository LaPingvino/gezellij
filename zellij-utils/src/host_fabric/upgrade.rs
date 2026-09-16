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
use crate::input::cli_assets::CliAssets;
use crate::input::command::RunCommand;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
            "session '{}' is still served by the binary it started with (pid {}), which has since \
             been replaced on disk",
            session_name, pid
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// In-place (exec) upgrade manifest
// ---------------------------------------------------------------------------------------------

/// Version of the exec-upgrade manifest; bump when the shape below changes incompatibly.
pub const EXEC_UPGRADE_PROTOCOL_VERSION: u32 = 1;

static ORIGINAL_EXE: OnceLock<PathBuf> = OnceLock::new();

/// Remember where our executable lived when we started. After a package upgrade
/// `/proc/self/exe` points at `... (deleted)`; this path is where the *new* binary now is.
pub fn record_original_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let cleaned = PathBuf::from(
            exe.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .to_string(),
        );
        let _ = ORIGINAL_EXE.set(cleaned);
    }
}

/// The binary an in-place upgrade should exec: `$GEZELLIJ_UPGRADE_BINARY` if set (handy for
/// tests and for trying a build before installing it), else the recorded original path.
pub fn upgrade_binary_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GEZELLIJ_UPGRADE_BINARY") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    ORIGINAL_EXE.get().cloned()
}

/// One terminal pane the new server should adopt instead of spawning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdoptablePane {
    pub terminal_id: u32,
    /// the PTY master, inherited across `execve` (CLOEXEC cleared)
    pub fd: i32,
    pub child_pid: Option<u32>,
    /// what the pane was originally asked to run (carries the restart policy); `None` for a
    /// plain shell pane
    #[serde(default)]
    pub run: Option<RunCommand>,
    /// the command as it appears in the resurrection layout for this pane (`None` = default
    /// shell); used to double-check that a layout leaf really is this pane before adopting
    #[serde(default)]
    pub layout_command: Option<Vec<String>>,
}

/// The panes of one tab in the exact order `spawn_terminals_for_layout` will visit them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TabAdoption {
    pub tiled: Vec<AdoptablePane>,
    pub floating: Vec<AdoptablePane>,
}

/// Everything the re-exec'd server needs to become the old one again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecUpgradeManifest {
    pub protocol_version: u32,
    pub session_name: String,
    pub old_server_version: String,
    pub old_server_pid: u32,
    /// the assets the very first client handed the old server, with `layout` pointed at the
    /// freshly written resurrection layout
    pub cli_assets: CliAssets,
    pub layout_file: PathBuf,
    pub tabs: Vec<TabAdoption>,
}

impl ExecUpgradeManifest {
    pub fn all_fds(&self) -> Vec<i32> {
        self.tabs
            .iter()
            .flat_map(|t| t.tiled.iter().chain(t.floating.iter()).map(|p| p.fd))
            .collect()
    }
    pub fn pane_count(&self) -> usize {
        self.tabs
            .iter()
            .map(|t| t.tiled.len() + t.floating.len())
            .sum()
    }
}

/// Sessions whose socket lives in a *different* `contract_version_N` directory than the one this
/// binary speaks. Such a server is stranded: it is still running, still holding your processes,
/// but `list-sessions` cannot see it because it only scans our own contract directory. Returns
/// `(session name, socket path, recorded server pid if any)`.
///
/// This is the "old server lingering in the background after an upgrade" case. We can *find* it;
/// whether we can migrate it depends on whether it recorded a pid (see [`server_pid`]), because
/// only a server that understands the handover signal may be signalled at all - the default
/// disposition of `SIGUSR2` is to kill the process.
pub fn stranded_sessions() -> Vec<(String, PathBuf, Option<u32>)> {
    let ours = &*ZELLIJ_SOCK_DIR;
    let Some(parent) = ours.parent() else {
        return vec![];
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return vec![];
    };
    let mut stranded = vec![];
    for entry in entries.flatten() {
        let dir = entry.path();
        if &dir == ours || !dir.is_dir() {
            continue;
        }
        if !dir
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("contract_version_"))
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(sockets) = fs::read_dir(&dir) else {
            continue;
        };
        for socket in sockets.flatten() {
            let path = socket.path();
            let name = socket.file_name().to_string_lossy().into_owned();
            // skip our own side-car records, we only want the sockets themselves
            if name.contains('.') {
                continue;
            }
            let pid = fs::read_to_string(dir.join(format!("{}{}", name, PID_RECORD_SUFFIX)))
                .ok()
                .and_then(|p| p.trim().parse::<u32>().ok())
                .filter(|pid| PathBuf::from(format!("/proc/{}", pid)).is_dir());
            stranded.push((name, path, pid));
        }
    }
    stranded.sort();
    stranded
}

pub fn exec_manifest_dir() -> PathBuf {
    ZELLIJ_SOCK_DIR.join("handover")
}

pub fn exec_manifest_path(session_name: &str) -> PathBuf {
    exec_manifest_path_in(&ZELLIJ_SOCK_DIR, session_name)
}

/// The manifest a server whose socket lives in `socket_dir` would write. A server stranded in an
/// older `contract_version_N` directory writes there, not next to ours, so anything watching for
/// its progress has to look in its directory.
pub fn exec_manifest_path_in(socket_dir: &Path, session_name: &str) -> PathBuf {
    socket_dir
        .join("handover")
        .join(format!("{}.exec.json", session_name))
}

/// See [`exec_manifest_path_in`].
pub fn exec_error_path_in(socket_dir: &Path, session_name: &str) -> PathBuf {
    socket_dir
        .join("handover")
        .join(format!("{}.exec.error", session_name))
}

/// The pid a server recorded next to its socket in `socket_dir`, if that process still exists.
pub fn server_pid_in(socket_dir: &Path, session_name: &str) -> Option<u32> {
    let pid: u32 =
        fs::read_to_string(socket_dir.join(format!("{}{}", session_name, PID_RECORD_SUFFIX)))
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

pub fn write_exec_manifest(manifest: &ExecUpgradeManifest) -> io::Result<PathBuf> {
    fs::create_dir_all(exec_manifest_dir())?;
    let path = exec_manifest_path(&manifest.session_name);
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Where the server drops the reason an in-place upgrade did not happen, so the CLI can say
/// *why* instead of waiting for a timeout.
pub fn exec_error_path(session_name: &str) -> PathBuf {
    exec_manifest_dir().join(format!("{}.exec.error", session_name))
}

pub fn write_exec_error(session_name: &str, reason: &str) {
    let _ = fs::create_dir_all(exec_manifest_dir());
    let _ = fs::write(exec_error_path(session_name), reason);
}

pub fn take_exec_error(session_name: &str) -> Option<String> {
    let path = exec_error_path(session_name);
    let reason = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Some(reason)
}

pub fn read_exec_manifest(path: &Path) -> io::Result<ExecUpgradeManifest> {
    let json = fs::read_to_string(path)?;
    let manifest: ExecUpgradeManifest =
        serde_json::from_str(&json).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if manifest.protocol_version > EXEC_UPGRADE_PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "exec upgrade manifest is version {} but this binary understands up to {}",
                manifest.protocol_version, EXEC_UPGRADE_PROTOCOL_VERSION
            ),
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_manifest_round_trips_and_counts() {
        let manifest = ExecUpgradeManifest {
            protocol_version: EXEC_UPGRADE_PROTOCOL_VERSION,
            session_name: "s".into(),
            old_server_version: "0.0".into(),
            old_server_pid: 1,
            cli_assets: CliAssets::default(),
            layout_file: PathBuf::from("/tmp/x.kdl"),
            tabs: vec![TabAdoption {
                tiled: vec![AdoptablePane {
                    terminal_id: 0,
                    fd: 7,
                    child_pid: Some(42),
                    run: None,
                    layout_command: None,
                }],
                floating: vec![AdoptablePane {
                    terminal_id: 3,
                    fd: 9,
                    child_pid: None,
                    run: Some(RunCommand::new(PathBuf::from("sleep"))),
                    layout_command: Some(vec!["sleep".into(), "1".into()]),
                }],
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: ExecUpgradeManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, manifest);
        assert_eq!(back.all_fds(), vec![7, 9]);
        assert_eq!(back.pane_count(), 2);
    }

    #[test]
    fn upgrade_binary_env_override_wins() {
        std::env::set_var("GEZELLIJ_UPGRADE_BINARY", "/tmp/some-binary");
        assert_eq!(
            upgrade_binary_path(),
            Some(PathBuf::from("/tmp/some-binary"))
        );
        std::env::remove_var("GEZELLIJ_UPGRADE_BINARY");
    }

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
