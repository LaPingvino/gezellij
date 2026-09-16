//! cgroup v2 helpers for Gezellij's freezer (Phase 2).
//!
//! Every terminal pane the server spawns is placed in its own cgroup,
//! `<server cgroup>/gezellij-<session>/pane-<terminal id>`, *by the child itself before exec* (so
//! everything the command forks later inherits it). That gives us three things for free:
//!
//! * `cgroup.freeze`: an in-kernel, signal-free pause of the whole process tree of a pane
//!   (0% CPU, memory untouched, instant thaw) - `zellij freeze` / `zellij thaw`;
//! * a trustworthy "is anything still running in there" signal (`cgroup.events: populated`);
//! * a place to hang resource limits later.
//!
//! No root is needed: under `systemd --user` the whole `user@<uid>.service` subtree is delegated
//! to the user, and `cgroup.freeze` is part of the core cgroup interface, so no controller has to
//! be enabled. Where that delegation is missing (containers, exotic inits) everything here degrades
//! to "not available" and the server simply runs panes un-isolated, exactly as upstream Zellij does.
//!
//! The server records its per-session root next to the session's socket
//! (`<socket dir>/<session>.cgroup-root`) so the CLI can freeze panes by writing to sysfs
//! directly - which keeps working even when the server itself is busy or wedged.

use crate::consts::ZELLIJ_SOCK_DIR;
use std::ffi::CStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const CGROUP_FS_ROOT: &str = "/sys/fs/cgroup";
const SESSION_RECORD_SUFFIX: &str = ".cgroup-root";
const PANE_DIR_PREFIX: &str = "pane-";

/// Whether the unified (v2) cgroup hierarchy is mounted at the standard place.
pub fn is_cgroup_v2_available() -> bool {
    Path::new(CGROUP_FS_ROOT)
        .join("cgroup.controllers")
        .is_file()
}

/// The sysfs directory of the cgroup the current process lives in (v2 only).
pub fn own_cgroup_dir() -> io::Result<PathBuf> {
    let content = fs::read_to_string("/proc/self/cgroup")?;
    // v2 has exactly one line of the form `0::/user.slice/.../foo.scope`
    let path = content
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no cgroup v2 membership found in /proc/self/cgroup",
            )
        })?
        .trim();
    Ok(Path::new(CGROUP_FS_ROOT).join(path.trim_start_matches('/')))
}

/// Freezer state of one cgroup, as reported by `cgroup.events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FreezeState {
    pub frozen: bool,
    /// whether any process (still) lives in the cgroup or its descendants
    pub populated: bool,
}

pub fn freeze_state(cgroup_dir: &Path) -> io::Result<FreezeState> {
    let events = fs::read_to_string(cgroup_dir.join("cgroup.events"))?;
    let mut state = FreezeState::default();
    for line in events.lines() {
        match line.split_once(' ') {
            Some(("frozen", value)) => state.frozen = value.trim() == "1",
            Some(("populated", value)) => state.populated = value.trim() == "1",
            _ => {},
        }
    }
    Ok(state)
}

/// Freeze (`true`) or thaw (`false`) every process in the cgroup and its descendants.
pub fn set_frozen(cgroup_dir: &Path, frozen: bool) -> io::Result<()> {
    fs::write(
        cgroup_dir.join("cgroup.freeze"),
        if frozen { "1\n" } else { "0\n" },
    )
}

/// Move the *calling* process into `cgroup_dir`.
///
/// `cgroup_procs` must be the full path of that cgroup's `cgroup.procs` file. This variant is
/// safe to call from a `pre_exec` hook right after `fork()`: it uses raw syscalls only and never
/// allocates, so it cannot deadlock on a lock some other thread of the parent held at fork time.
///
/// # Safety
/// Only the async-signal-safe `open`/`write`/`close` syscalls are used; the caller must provide a
/// valid NUL-terminated path.
pub fn join_cgroup_raw(cgroup_procs: &CStr) -> io::Result<()> {
    // "0" means "the writing process"
    const SELF: &[u8] = b"0\n";
    unsafe {
        let fd = libc::open(cgroup_procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(fd, SELF.as_ptr() as *const libc::c_void, SELF.len());
        let write_err = io::Error::last_os_error();
        libc::close(fd);
        if written < 0 {
            return Err(write_err);
        }
    }
    Ok(())
}

/// The per-session tree of pane cgroups managed by one server.
#[derive(Debug, Clone)]
pub struct PaneCgroups {
    root: PathBuf,
}

impl PaneCgroups {
    /// Create (or reuse) `<own cgroup>/gezellij-<session>` and record its location in the session
    /// cache so CLI tools can find it. Returns `Err` when cgroup v2 delegation is not available;
    /// callers should log once and carry on without isolation.
    pub fn create_for_session(session_name: &str) -> io::Result<Self> {
        if !is_cgroup_v2_available() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cgroup v2 is not mounted at /sys/fs/cgroup",
            ));
        }
        let root = own_cgroup_dir()?.join(format!("gezellij-{}", session_name));
        fs::create_dir_all(&root)?;
        // prove we may actually use it before advertising it
        let _ = freeze_state(&root)?;
        fs::create_dir_all(&*ZELLIJ_SOCK_DIR)?;
        fs::write(record_file(session_name), root.display().to_string())?;
        Ok(PaneCgroups { root })
    }
    /// Locate the tree a running server recorded for `session_name` (used by the CLI).
    pub fn from_session_record(session_name: &str) -> io::Result<Option<Self>> {
        let record = record_file(session_name);
        match fs::read_to_string(&record) {
            Ok(path) => {
                let root = PathBuf::from(path.trim());
                if root.is_dir() {
                    Ok(Some(PaneCgroups { root }))
                } else {
                    Ok(None)
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub fn at(root: PathBuf) -> Self {
        PaneCgroups { root }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn pane_dir(&self, terminal_id: u32) -> PathBuf {
        self.root
            .join(format!("{}{}", PANE_DIR_PREFIX, terminal_id))
    }
    /// Create the cgroup for a pane and return the path of its `cgroup.procs` file, ready to be
    /// handed to [`join_cgroup_raw`] in the child.
    pub fn create_pane(&self, terminal_id: u32) -> io::Result<PathBuf> {
        let dir = self.pane_dir(terminal_id);
        fs::create_dir_all(&dir)?;
        Ok(dir.join("cgroup.procs"))
    }
    /// Remove a pane's cgroup (only succeeds once it is empty; thaws it first so stragglers can
    /// actually exit).
    pub fn remove_pane(&self, terminal_id: u32) -> io::Result<()> {
        let dir = self.pane_dir(terminal_id);
        if !dir.exists() {
            return Ok(());
        }
        let _ = set_frozen(&dir, false);
        fs::remove_dir(&dir)
    }
    /// All pane cgroups currently present, sorted by terminal id.
    pub fn list_panes(&self) -> io::Result<Vec<(u32, PathBuf)>> {
        let mut panes = vec![];
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_prefix(PANE_DIR_PREFIX) {
                if let Ok(id) = id.parse::<u32>() {
                    panes.push((id, entry.path()));
                }
            }
        }
        panes.sort_by_key(|(id, _)| *id);
        Ok(panes)
    }
    /// Freeze or thaw every pane of the session.
    pub fn set_all_frozen(&self, frozen: bool) -> io::Result<Vec<(u32, io::Result<()>)>> {
        Ok(self
            .list_panes()?
            .into_iter()
            .map(|(id, dir)| (id, set_frozen(&dir, frozen)))
            .collect())
    }
    /// Best-effort teardown at server exit: thaw and remove every pane cgroup, then the root and
    /// the CLI record. Also sweeps empty leftovers of earlier sessions next to this root.
    pub fn remove_all(&self, session_name: &str) {
        if let Ok(panes) = self.list_panes() {
            for (_, dir) in panes {
                let _ = set_frozen(&dir, false);
                let _ = fs::remove_dir(&dir);
            }
        }
        let _ = fs::remove_dir(&self.root);
        let _ = fs::remove_file(record_file(session_name));
        sweep_stale_roots(self.root.parent());
    }
}

fn record_file(session_name: &str) -> PathBuf {
    ZELLIJ_SOCK_DIR.join(format!("{}{}", session_name, SESSION_RECORD_SUFFIX))
}

/// Remove sibling `gezellij-*` roots that no longer contain any process (e.g. left behind by a
/// server that was killed). `rmdir` refuses non-empty cgroups, so this can never hurt a live one.
fn sweep_stale_roots(parent: Option<&Path>) {
    let Some(parent) = parent else { return };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("gezellij-") {
            continue;
        }
        let root = entry.path();
        if let Ok(panes) = fs::read_dir(&root) {
            for pane in panes.flatten() {
                if pane
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PANE_DIR_PREFIX)
                {
                    let _ = fs::remove_dir(pane.path());
                }
            }
        }
        let _ = fs::remove_dir(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cgroup_events() {
        let dir = std::env::temp_dir().join(format!("gz-cg-events-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert_eq!(
            freeze_state(&dir).unwrap(),
            FreezeState {
                frozen: false,
                populated: true
            }
        );
        fs::write(dir.join("cgroup.events"), "populated 0\nfrozen 1\n").unwrap();
        assert_eq!(
            freeze_state(&dir).unwrap(),
            FreezeState {
                frozen: true,
                populated: false
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pane_dirs_are_listed_and_sorted() {
        let root = std::env::temp_dir().join(format!("gz-cg-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let tree = PaneCgroups::at(root.clone());
        for id in [10u32, 2, 7] {
            fs::create_dir_all(tree.pane_dir(id)).unwrap();
        }
        fs::create_dir_all(root.join("unrelated")).unwrap();
        let ids: Vec<u32> = tree
            .list_panes()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![2, 7, 10]);
        assert!(tree.pane_dir(2).ends_with("pane-2"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn own_cgroup_dir_points_under_sysfs_when_v2_is_present() {
        if !is_cgroup_v2_available() {
            return;
        }
        let dir = own_cgroup_dir().unwrap();
        assert!(dir.starts_with(CGROUP_FS_ROOT));
    }
}
