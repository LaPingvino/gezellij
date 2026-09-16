use crate::os_input_output::{command_exists, AsyncReader};
use crate::panes::PaneId;

use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, FdFlag, OFlag},
    pty::{openpty, OpenptyResult, Winsize},
    sys::{
        signal::{kill, Signal},
        termios,
        wait::{waitpid, WaitStatus},
    },
    unistd,
};
use tokio::io::unix::AsyncFd;

use libc::{self, ioctl, TIOCSWINSZ};
use signal_hook;
use signal_hook::consts::*;

use std::os::unix::ffi::OsStringExt;
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    os::fd::{BorrowedFd, FromRawFd, IntoRawFd},
    os::unix::{
        io::{AsRawFd, RawFd},
        process::CommandExt,
    },
    process::{Child, Command},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use zellij_utils::{errors::prelude::*, input::command::RunCommand};

pub use async_trait::async_trait;

/// An `AsyncReader` that wraps a `RawFd` using epoll via `AsyncFd`.
///
/// Construction sets O_NONBLOCK but defers `AsyncFd` registration to the first
/// `read()` call, because `AsyncFd::new()` requires a live Tokio reactor and
/// `spawn_terminal` runs on the plain PTY thread (outside the runtime).
struct RawFdAsyncReader {
    /// Holds the file before reactor registration; `None` after promotion.
    pending: Option<File>,
    /// Populated on first `read()` inside the Tokio runtime.
    async_fd: Option<AsyncFd<File>>,
}

impl RawFdAsyncReader {
    fn new(fd: RawFd) -> io::Result<Self> {
        // Set O_NONBLOCK so AsyncFd can use epoll correctly
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let flags = fcntl(borrowed_fd, FcntlArg::F_GETFL)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.insert(OFlag::O_NONBLOCK);
        fcntl(borrowed_fd, FcntlArg::F_SETFL(oflags))
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Self {
            pending: Some(file),
            async_fd: None,
        })
    }

    /// Lazily register with the Tokio reactor on first use.
    fn get_async_fd(&mut self) -> io::Result<&mut AsyncFd<File>> {
        if self.async_fd.is_none() {
            let file = self
                .pending
                .take()
                .expect("RawFdAsyncReader used after init");
            self.async_fd = Some(AsyncFd::new(file)?);
        }
        Ok(self.async_fd.as_mut().unwrap())
    }
}

#[async_trait]
impl AsyncReader for RawFdAsyncReader {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let async_fd = self.get_async_fd()?;
        loop {
            let mut guard = async_fd.readable().await?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let ret =
                    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if ret < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}

fn set_terminal_size_using_fd(
    fd: RawFd,
    columns: u16,
    rows: u16,
    width_in_pixels: Option<u16>,
    height_in_pixels: Option<u16>,
) {
    // TODO: do this with the nix ioctl
    let ws_xpixel = width_in_pixels.unwrap_or(0);
    let ws_ypixel = height_in_pixels.unwrap_or(0);
    let winsize = Winsize {
        ws_col: columns,
        ws_row: rows,
        ws_xpixel,
        ws_ypixel,
    };
    // TIOCGWINSZ is an u32, but the second argument to ioctl is u64 on
    // some platforms. When checked on Linux, clippy will complain about
    // useless conversion.
    #[allow(clippy::useless_conversion)]
    unsafe {
        ioctl(fd, TIOCSWINSZ.into(), &winsize)
    };
}

/// Handle some signals for the child process. This will loop until the child
/// process exits.
fn handle_command_exit(mut child: Child) -> Result<Option<i32>> {
    let id = child.id();
    let err_context = || {
        format!(
            "failed to handle signals and command exit for child process pid {}",
            id
        )
    };

    // returns the exit status, if any
    let mut should_exit = false;
    let mut attempts = 3;
    let mut signals =
        signal_hook::iterator::Signals::new(&[SIGINT, SIGTERM]).with_context(err_context)?;
    'handle_exit: loop {
        // test whether the child process has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                // if the child process has exited, break outside of the loop
                // and exit this function
                // TODO: handle errors?
                break 'handle_exit Ok(status.code());
            },
            Ok(None) => {
                thread::sleep(Duration::from_millis(10));
            },
            Err(e) => panic!("error attempting to wait: {}", e),
        }

        if !should_exit {
            for signal in signals.pending() {
                if signal == SIGINT || signal == SIGTERM {
                    should_exit = true;
                }
            }
        } else if attempts > 0 {
            // let's try nicely first...
            attempts -= 1;
            kill(
                unistd::Pid::from_raw(child.id() as i32),
                Some(Signal::SIGTERM),
            )
            .with_context(err_context)?;
            continue;
        } else {
            // when I say whoa, I mean WHOA!
            let _ = child.kill();
            break 'handle_exit Ok(None);
        }
    }
}

/// Gezellij: the per-session tree of pane cgroups, created once per server process. `None` when
/// cgroup v2 delegation is unavailable (logged once); panes then run un-isolated as in upstream.
fn pane_cgroups() -> &'static Option<zellij_utils::host_fabric::cgroups::PaneCgroups> {
    use std::sync::OnceLock;
    static PANE_CGROUPS: OnceLock<Option<zellij_utils::host_fabric::cgroups::PaneCgroups>> =
        OnceLock::new();
    PANE_CGROUPS.get_or_init(|| {
        let session_name = match zellij_utils::envs::get_session_name() {
            Ok(name) => name,
            Err(_) => return None,
        };
        match zellij_utils::host_fabric::cgroups::PaneCgroups::create_for_session(&session_name) {
            Ok(tree) => {
                log::info!(
                    "pane cgroups for this session live under {}",
                    tree.root().display()
                );
                Some(tree)
            },
            Err(e) => {
                log::warn!(
                    "cgroup v2 delegation unavailable ({}); panes will not be freezable",
                    e
                );
                None
            },
        }
    })
}

/// Gezellij: thaw and remove this session's pane cgroups; called once when the server exits.
pub fn cleanup_pane_cgroups() {
    if let Some(tree) = pane_cgroups().as_ref() {
        if let Ok(session_name) = zellij_utils::envs::get_session_name() {
            tree.remove_all(&session_name);
        }
    }
}

/// Gezellij (handover 3.2): set or clear `FD_CLOEXEC` on a single fd, leaving its other
/// descriptor flags alone.
fn set_cloexec(fd: RawFd, on: bool) -> Result<()> {
    let err_context = || format!("failed to change FD_CLOEXEC on fd {}", fd);
    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let flags = fcntl(borrowed_fd, FcntlArg::F_GETFD).with_context(err_context)?;
    let mut fd_flags = FdFlag::from_bits_truncate(flags);
    if on {
        fd_flags.insert(FdFlag::FD_CLOEXEC);
    } else {
        fd_flags.remove(FdFlag::FD_CLOEXEC);
    }
    fcntl(borrowed_fd, FcntlArg::F_SETFD(fd_flags)).with_context(err_context)?;
    Ok(())
}

/// Gezellij (handover 3.2): the reaper for an *adopted* pane child.
///
/// After an in-place `execve` upgrade the pid is unchanged, so the pane's child is still our
/// child and `waitpid` gives us a real exit status - exactly like the `child.wait()` reaper that
/// `handle_openpty` spawns for a freshly forked pane. If it is *not* our child (`ECHILD` - e.g. a
/// future socket-handover mode, where the children were reparented to init), we degrade to
/// polling `kill(pid, 0)` and report an unknown exit status, which
/// `RestartPolicy::should_restart(None)` already handles.
fn spawn_adopted_reaper(
    terminal_id: u32,
    child_pid: u32,
    quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
    run_command: RunCommand,
) {
    thread::spawn(move || {
        let pid = unistd::Pid::from_raw(child_pid as i32);
        let mut waitable = true;
        let exit_status = loop {
            match waitpid(pid, None) {
                Ok(WaitStatus::Exited(_, exit_code)) => break Some(exit_code),
                Ok(WaitStatus::Signaled(..)) => break None,
                // stopped/continued/ptrace stops: the process is still around, keep waiting
                Ok(_) => continue,
                Err(Errno::EINTR) => continue,
                Err(Errno::ECHILD) => {
                    waitable = false;
                    break None;
                },
                Err(e) => {
                    log::error!(
                        "waitpid on adopted pane {} (pid {}) failed: {}",
                        terminal_id,
                        child_pid,
                        e
                    );
                    break None;
                },
            }
        };
        if waitable {
            log::info!(
                "adopted pane {} (pid {}) exited with {:?} (reaped with waitpid)",
                terminal_id,
                child_pid,
                exit_status
            );
        } else {
            log::info!(
                "adopted pane {} (pid {}) is not our child (ECHILD); falling back to kill(pid, 0) liveness polling, exit status will be unknown",
                terminal_id,
                child_pid
            );
            loop {
                match kill(pid, None::<Signal>) {
                    Err(Errno::ESRCH) => break,
                    _ => thread::sleep(Duration::from_millis(250)),
                }
            }
            log::info!(
                "adopted pane {} (pid {}) is gone (liveness poll)",
                terminal_id,
                child_pid
            );
        }
        quit_cb(PaneId::Terminal(terminal_id), exit_status, run_command);
        // Gezellij: same tidy-up as the reaper in `handle_openpty` - after an in-place exec the
        // pane's cgroup is still there (same session, same tree)
        if let Some(tree) = pane_cgroups().as_ref() {
            let _ = tree.remove_pane(terminal_id);
        }
    });
}

fn handle_openpty(
    open_pty_res: OpenptyResult,
    cmd: RunCommand,
    quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
    terminal_id: u32,
) -> Result<(RawFd, RawFd)> {
    let err_context = |cmd: &RunCommand| {
        format!(
            "failed to open PTY for command '{}'",
            cmd.command.to_string_lossy().to_string()
        )
    };

    // primary side of pty and child fd
    let pid_primary = open_pty_res.master.into_raw_fd();
    let pid_secondary = open_pty_res.slave.into_raw_fd();

    if !command_exists(&cmd) {
        return Err(ZellijError::CommandNotFound {
            terminal_id,
            command: cmd.command.to_string_lossy().to_string(),
        })
        .with_context(|| err_context(&cmd));
    }

    // Gezellij: give the pane its own cgroup so it can be frozen/thawed as a unit. The child joins
    // it in pre_exec (before exec, so every descendant inherits it); failure is non-fatal.
    let cgroup_procs: Option<std::ffi::CString> =
        pane_cgroups()
            .as_ref()
            .and_then(|tree| match tree.create_pane(terminal_id) {
                Ok(procs_file) => {
                    std::ffi::CString::new(procs_file.into_os_string().into_vec()).ok()
                },
                Err(e) => {
                    log::warn!("could not create cgroup for pane {}: {}", terminal_id, e);
                    None
                },
            });

    let mut child = unsafe {
        let cmd = cmd.clone();
        let command = &mut Command::new(cmd.command);
        if let Some(current_dir) = cmd.cwd {
            if current_dir.exists() && current_dir.is_dir() {
                command.current_dir(current_dir);
            } else {
                log::error!(
                    "Failed to set CWD for new pane. '{}' does not exist or is not a folder",
                    current_dir.display()
                );
            }
        }
        command
            .args(&cmd.args)
            .env("ZELLIJ_PANE_ID", &format!("{}", terminal_id))
            .pre_exec(move || -> io::Result<()> {
                if libc::login_tty(pid_secondary) != 0 {
                    panic!("failed to set controlling terminal");
                }
                if let Some(cgroup_procs) = cgroup_procs.as_ref() {
                    // best effort: an un-isolated pane is better than no pane
                    let _ = zellij_utils::host_fabric::cgroups::join_cgroup_raw(cgroup_procs);
                }
                close_fds::close_open_fds(3, &[]);
                Ok(())
            })
            .spawn()
            .expect("failed to spawn")
    };

    let child_id = child.id();
    thread::spawn(move || {
        child.wait().with_context(|| err_context(&cmd)).fatal();
        let exit_status = handle_command_exit(child)
            .with_context(|| err_context(&cmd))
            .fatal();
        let _ = unistd::close(pid_secondary);
        quit_cb(PaneId::Terminal(terminal_id), exit_status, cmd);
        // Gezellij: tidy the pane's cgroup once its process tree is gone (fails harmlessly if
        // descendants linger; the session teardown sweeps those up)
        if let Some(tree) = pane_cgroups().as_ref() {
            let _ = tree.remove_pane(terminal_id);
        }
    });

    Ok((pid_primary, child_id as RawFd))
}

/// Spawns a new terminal from the parent terminal with [`termios`](termios::Termios)
/// `orig_termios`.
fn handle_terminal(
    cmd: RunCommand,
    failover_cmd: Option<RunCommand>,
    orig_termios: Option<termios::Termios>,
    quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
    terminal_id: u32,
) -> Result<(RawFd, RawFd)> {
    let err_context = || "failed to spawn child terminal".to_string();

    // Create a pipe to allow the child the communicate the shell's pid to its
    // parent.
    match openpty(None, &orig_termios) {
        Ok(open_pty_res) => handle_openpty(open_pty_res, cmd, quit_cb, terminal_id),
        Err(e) => match failover_cmd {
            Some(failover_cmd) => {
                handle_terminal(failover_cmd, None, orig_termios, quit_cb, terminal_id)
                    .with_context(err_context)
            },
            None => Err::<(i32, i32), _>(e)
                .context("failed to start pty")
                .with_context(err_context)
                .to_log(),
        },
    }
}

/// The Unix PTY backend. Manages native PTY file descriptors and signals.
#[derive(Clone)]
pub(crate) struct UnixPtyBackend {
    orig_termios: Arc<Mutex<Option<termios::Termios>>>,
    terminal_id_to_raw_fd: Arc<Mutex<BTreeMap<u32, Option<RawFd>>>>,
    next_terminal_id_counter: Arc<AtomicU32>,
}

/// Try to write as many bytes from `buf` as possible to `fd` without blocking.
///
/// Loops on successful short writes and EINTR to drain as much as the kernel
/// will accept. On EAGAIN (fd buffer full), stops and returns how many bytes
/// were written so far (which may be 0). The caller is expected to re-queue
/// any unwritten remainder.
fn try_write_to_fd(fd: RawFd, buf: &[u8]) -> Result<usize> {
    let mut written = 0;
    while written < buf.len() {
        match unistd::write(unsafe { BorrowedFd::borrow_raw(fd) }, &buf[written..]) {
            Ok(0) => break, // fd returned 0 on non-empty buf; treat like EAGAIN
            Ok(n) => written += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::EAGAIN) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(written)
}

impl UnixPtyBackend {
    pub fn new() -> Result<Self, io::Error> {
        let current_termios = termios::tcgetattr(io::stdin()).ok();
        if current_termios.is_none() {
            log::warn!("Starting a server without a controlling terminal, using the default termios configuration.");
        }
        Ok(Self {
            orig_termios: Arc::new(Mutex::new(current_termios)),
            terminal_id_to_raw_fd: Arc::new(Mutex::new(BTreeMap::new())),
            next_terminal_id_counter: Arc::new(AtomicU32::new(0)),
        })
    }

    pub fn spawn_terminal(
        &self,
        cmd: RunCommand,
        failover_cmd: Option<RunCommand>,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
    ) -> Result<(Box<dyn AsyncReader>, RawFd)> {
        let orig_termios = self
            .orig_termios
            .lock()
            .to_anyhow()
            .context("failed to lock orig_termios")?;
        let (pid_primary, child_fd) = handle_terminal(
            cmd,
            failover_cmd,
            orig_termios.clone(),
            quit_cb,
            terminal_id,
        )?;
        self.terminal_id_to_raw_fd
            .lock()
            .to_anyhow()?
            .insert(terminal_id, Some(pid_primary));
        let async_reader = Box::new(
            RawFdAsyncReader::new(pid_primary)
                .map_err(|e| anyhow::anyhow!("failed to create async reader: {}", e))?,
        ) as Box<dyn AsyncReader>;
        Ok((async_reader, child_fd))
    }

    pub fn set_terminal_size(
        &self,
        terminal_id: u32,
        cols: u16,
        rows: u16,
        width_in_pixels: Option<u16>,
        height_in_pixels: Option<u16>,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "failed to set terminal id {} to size ({}, {})",
                terminal_id, rows, cols
            )
        };
        match self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(fd)) => {
                if cols > 0 && rows > 0 {
                    set_terminal_size_using_fd(*fd, cols, rows, width_in_pixels, height_in_pixels);
                }
            },
            _ => {
                Err::<(), _>(anyhow!("failed to find terminal fd for id {terminal_id}"))
                    .with_context(err_context)
                    .non_fatal();
            },
        }
        Ok(())
    }

    pub fn write_to_tty_stdin(&self, terminal_id: u32, buf: &[u8]) -> Result<usize> {
        let err_context = || format!("failed to write to stdin of TTY ID {}", terminal_id);

        let fd = match self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(fd)) => *fd,
            _ => {
                return Err(anyhow!("could not find raw file descriptor")).with_context(err_context)
            },
        };

        try_write_to_fd(fd, buf).with_context(err_context)
    }

    pub fn tcdrain(&self, terminal_id: u32) -> Result<()> {
        let err_context = || format!("failed to tcdrain to TTY ID {}", terminal_id);

        match self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(fd)) => {
                termios::tcdrain(unsafe { BorrowedFd::borrow_raw(*fd) }).with_context(err_context)
            },
            _ => Err(anyhow!("could not find raw file descriptor")).with_context(err_context),
        }
    }

    pub fn tcgetpgrp(&self, terminal_id: u32) -> Option<i32> {
        match self.terminal_id_to_raw_fd.lock().ok()?.get(&terminal_id) {
            Some(Some(fd)) => unistd::tcgetpgrp(unsafe { BorrowedFd::borrow_raw(*fd) })
                .ok()
                .map(|pgid| pgid.as_raw()),
            _ => None,
        }
    }

    pub fn kill(&self, pid: u32) -> Result<()> {
        let _ = kill(unistd::Pid::from_raw(pid as i32), Some(Signal::SIGHUP));
        Ok(())
    }

    pub fn force_kill(&self, pid: u32) -> Result<()> {
        let _ = kill(unistd::Pid::from_raw(pid as i32), Some(Signal::SIGKILL));
        Ok(())
    }

    pub fn send_sigint(&self, pid: u32) -> Result<()> {
        let _ = kill(unistd::Pid::from_raw(pid as i32), Some(Signal::SIGINT));
        Ok(())
    }

    pub fn reserve_terminal_id(&self, terminal_id: u32) {
        self.terminal_id_to_raw_fd
            .lock()
            .unwrap()
            .insert(terminal_id, None);
    }

    pub fn clear_terminal_id(&self, terminal_id: u32) {
        self.terminal_id_to_raw_fd
            .lock()
            .unwrap()
            .remove(&terminal_id);
    }

    pub fn next_terminal_id(&self) -> Option<u32> {
        Some(
            self.next_terminal_id_counter
                .fetch_add(1, Ordering::Relaxed),
        )
    }

    /// Gezellij (handover 3.2): adopt an already-open PTY master (and, if given, the
    /// already-running child behind it) instead of forking a new one.
    ///
    /// Registers `master_fd` under `terminal_id`, raises `next_terminal_id_counter` above
    /// `terminal_id` so a later `next_terminal_id()` cannot collide with an adopted pane,
    /// re-applies the pane geometry (the resurrected layout may compute a slightly different one
    /// than the old server had), puts `FD_CLOEXEC` back on the fd (it was cleared so it would
    /// survive `execve`) and returns the same kind of async reader `spawn_terminal` returns.
    pub fn adopt_terminal(
        &self,
        terminal_id: u32,
        master_fd: RawFd,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        run_command: RunCommand,
    ) -> Result<Box<dyn AsyncReader>> {
        let err_context = || {
            format!(
                "failed to adopt terminal {} (fd {})",
                terminal_id, master_fd
            )
        };

        // every fallible step happens before we register anything or start the reaper, so a bad
        // manifest fd leaves no half-adopted pane behind. The CLOEXEC round-trip doubles as the
        // validity check on the fd: it travelled through `execve` with FD_CLOEXEC cleared, and we
        // put it back so it does not leak into panes we spawn from here on.
        set_cloexec(master_fd, true).with_context(err_context)?;
        let async_reader = Box::new(
            RawFdAsyncReader::new(master_fd)
                .map_err(|e| anyhow!("failed to create async reader: {}", e))?,
        ) as Box<dyn AsyncReader>;

        self.terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .insert(terminal_id, Some(master_fd));

        // never hand out an id that an adopted pane already occupies
        self.next_terminal_id_counter
            .fetch_max(terminal_id.saturating_add(1), Ordering::Relaxed);

        if cols > 0 && rows > 0 {
            set_terminal_size_using_fd(master_fd, cols, rows, None, None);
        }

        if let Some(child_pid) = child_pid {
            spawn_adopted_reaper(terminal_id, child_pid, quit_cb, run_command);
        }

        Ok(async_reader)
    }

    /// Gezellij (handover 3.2): a snapshot of the live PTY masters, for building the handover
    /// manifest. Reserved-but-not-yet-opened terminal ids (`None` entries) are skipped.
    pub fn terminal_fd_table(&self) -> Vec<(u32, RawFd)> {
        match self.terminal_id_to_raw_fd.lock() {
            Ok(terminal_id_to_raw_fd) => terminal_id_to_raw_fd
                .iter()
                .filter_map(|(terminal_id, fd)| fd.map(|fd| (*terminal_id, fd)))
                .collect(),
            Err(e) => {
                log::error!("failed to lock terminal fd table: {}", e);
                vec![]
            },
        }
    }

    /// Gezellij (handover 3.2): clear `FD_CLOEXEC` on exactly these fds so they survive the
    /// `execve` into the new server binary. Every other fd in the process keeps its own
    /// close-on-exec state.
    pub fn prepare_fds_for_exec(&self, fds: &[RawFd]) -> Result<()> {
        for fd in fds {
            set_cloexec(*fd, false)
                .with_context(|| format!("failed to prepare fd {} for exec", fd))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::sys::termios;
    use std::io::Read;

    /// Verify that `try_write_to_fd` writes as many bytes as the kernel will
    /// accept in one pass and returns a partial count (not an error) when the
    /// PTY buffer fills up.
    ///
    /// A concurrent reader drains the slave side so some bytes are accepted.
    /// The key assertion: the function returns Ok(n) where n <= buf.len(),
    /// and the caller (PtyWriter) is responsible for re-queuing the rest.
    #[test]
    fn try_write_to_fd_returns_partial_on_full_buffer() {
        let pty = openpty(None, &None).expect("openpty failed");
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();
        let borrowed_master = unsafe { BorrowedFd::borrow_raw(master_fd) };
        let borrowed_slave = unsafe { BorrowedFd::borrow_raw(slave_fd) };

        let mut attrs = termios::tcgetattr(borrowed_slave).expect("tcgetattr failed");
        termios::cfmakeraw(&mut attrs);
        termios::tcsetattr(borrowed_slave, termios::SetArg::TCSANOW, &attrs)
            .expect("tcsetattr failed");

        // O_NONBLOCK so write() returns EAGAIN instead of blocking
        let flags = fcntl(borrowed_master, FcntlArg::F_GETFL).expect("F_GETFL");
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.insert(OFlag::O_NONBLOCK);
        fcntl(borrowed_master, FcntlArg::F_SETFL(oflags)).expect("F_SETFL");

        // Fill most of the buffer, leaving some space
        let chunk = vec![0x42u8; 1024];
        let mut total_filled = 0;
        loop {
            match super::try_write_to_fd(master_fd, &chunk) {
                Ok(0) => break,
                Ok(n) => total_filled += n,
                Err(e) => panic!("unexpected error filling buffer: {e}"),
            }
        }
        assert!(
            total_filled > 0,
            "should have written some bytes to fill buffer"
        );

        // Read a small amount from the slave to free partial space
        let mut drain = vec![0u8; 512];
        let slave_file = unsafe { std::fs::File::from_raw_fd(slave_fd) };
        let mut slave_reader = std::io::BufReader::new(&slave_file);
        let drained = slave_reader.read(&mut drain).expect("slave read failed");
        assert!(drained > 0, "should have drained some bytes");
        // Prevent File from closing the slave fd — we close it manually below
        std::mem::forget(slave_file);

        // Now write more than the freed space — should get a partial write
        let size = 128 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let written = super::try_write_to_fd(master_fd, &data)
            .expect("try_write_to_fd should not error on EAGAIN");

        assert!(
            written > 0 && written < size,
            "expected partial write, got {written}/{size}",
        );

        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
    }

    /// Verify that `try_write_to_fd` returns Ok(0) — not an error — when the
    /// fd is completely full and cannot accept any bytes at all.
    #[test]
    fn try_write_to_fd_returns_zero_on_stuck_pty() {
        let pty = openpty(None, &None).expect("openpty failed");
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();
        let borrowed_master = unsafe { BorrowedFd::borrow_raw(master_fd) };
        let borrowed_slave = unsafe { BorrowedFd::borrow_raw(slave_fd) };

        let mut attrs = termios::tcgetattr(borrowed_slave).expect("tcgetattr failed");
        termios::cfmakeraw(&mut attrs);
        termios::tcsetattr(borrowed_slave, termios::SetArg::TCSANOW, &attrs)
            .expect("tcsetattr failed");

        let flags = fcntl(borrowed_master, FcntlArg::F_GETFL).expect("F_GETFL");
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.insert(OFlag::O_NONBLOCK);
        fcntl(borrowed_master, FcntlArg::F_SETFL(oflags)).expect("F_SETFL");

        // Fill the buffer completely — keep writing until we get Ok(0)
        let fill = vec![0x42u8; 1024];
        loop {
            match super::try_write_to_fd(master_fd, &fill) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => panic!("unexpected error filling buffer: {e}"),
            }
        }

        // Now the buffer is full — next write should return Ok(0)
        let written = super::try_write_to_fd(master_fd, &[0x01, 0x02, 0x03])
            .expect("try_write_to_fd should not error on EAGAIN");

        assert_eq!(written, 0, "expected zero bytes written on full buffer");

        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
    }

    // --- Gezellij (handover 3.2): PTY/child adoption ---------------------------------------

    fn cloexec_is_set(fd: RawFd) -> bool {
        let flags = fcntl(unsafe { BorrowedFd::borrow_raw(fd) }, FcntlArg::F_GETFD)
            .expect("F_GETFD failed");
        FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC)
    }

    fn run_command_for_test(script: &str) -> RunCommand {
        RunCommand {
            command: std::path::PathBuf::from("sh"),
            args: vec!["-c".to_string(), script.to_string()],
            ..Default::default()
        }
    }

    /// Spawn `sh -c <script>` on the slave side of an already-opened pty, the way
    /// `handle_openpty` does, and return (master_fd, child_pid). The parent's copy of the slave
    /// is closed, so the master sees EOF/EIO once the child is gone.
    fn spawn_on_pty(script: &str) -> (RawFd, u32) {
        let pty = openpty(None, &None).expect("openpty failed");
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();
        let child = unsafe {
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .pre_exec(move || -> io::Result<()> {
                    if libc::login_tty(slave_fd) != 0 {
                        panic!("failed to set controlling terminal");
                    }
                    Ok(())
                })
                .spawn()
                .expect("failed to spawn test child")
        };
        let child_pid = child.id();
        // the reaper thread inside `adopt_terminal` is the only waiter; dropping `Child` here does
        // not reap
        std::mem::forget(child);
        let _ = unistd::close(slave_fd);
        (master_fd, child_pid)
    }

    /// The core of handover step 3.2: a master fd plus a running child that were handed to us
    /// (here: freshly made, but the backend cannot tell the difference after an in-place
    /// `execve`) become a working pane - we read its output through the returned `AsyncReader`
    /// and get its real exit status through `quit_cb`.
    #[test]
    fn adopt_terminal_reads_output_and_reaps_child() {
        let (master_fd, child_pid) = spawn_on_pty("echo hello; sleep 0.3");
        let backend = UnixPtyBackend::new().expect("backend");
        let (quit_tx, quit_rx) = std::sync::mpsc::channel();
        let mut reader = backend
            .adopt_terminal(
                3,
                master_fd,
                Some(child_pid),
                24,
                80,
                Box::new(move |pane_id, exit_status, _cmd| {
                    let _ = quit_tx.send((pane_id, exit_status));
                }),
                run_command_for_test("echo hello; sleep 0.3"),
            )
            .expect("adopt_terminal failed");

        // the adopted fd is put back into close-on-exec state
        assert!(
            cloexec_is_set(master_fd),
            "adopt_terminal should re-arm FD_CLOEXEC on the adopted master"
        );

        let output = crate::global_async_runtime::get_tokio_runtime().block_on(async {
            let mut collected = String::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let mut buf = [0u8; 1024];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if collected.contains("hello") {
                                break;
                            }
                        },
                        // EIO once the last slave side is gone
                        Err(_) => break,
                    }
                }
            })
            .await;
            collected
        });
        assert!(
            output.contains("hello"),
            "expected to read 'hello' from the adopted pty, got {:?}",
            output
        );

        let (pane_id, exit_status) = quit_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("quit_cb was not called for the adopted child");
        assert_eq!(pane_id, PaneId::Terminal(3));
        assert_eq!(exit_status, Some(0));

        drop(reader); // closes the master
    }

    /// A pane id that arrived in a handover manifest must never be handed out again.
    #[test]
    fn adopt_terminal_advances_next_terminal_id() {
        let pty = openpty(None, &None).expect("openpty failed");
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();
        let backend = UnixPtyBackend::new().expect("backend");
        assert_eq!(backend.next_terminal_id(), Some(0));
        let reader = backend
            .adopt_terminal(
                41,
                master_fd,
                None,
                24,
                80,
                Box::new(|_, _, _| {}),
                run_command_for_test("true"),
            )
            .expect("adopt_terminal failed");
        assert_eq!(
            backend.next_terminal_id(),
            Some(42),
            "next_terminal_id must not collide with an adopted id"
        );
        drop(reader);
        let _ = unistd::close(slave_fd);
    }

    /// `prepare_fds_for_exec` clears close-on-exec on exactly the fds it is given.
    #[test]
    fn prepare_fds_for_exec_clears_cloexec_only_for_listed_fds() {
        let pty = openpty(None, &None).expect("openpty failed");
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();
        // openpty does not set FD_CLOEXEC, so arm it explicitly first
        super::set_cloexec(master_fd, true).expect("set_cloexec");
        assert!(cloexec_is_set(master_fd));

        // std opens files with O_CLOEXEC: our untouched control
        let unrelated = File::open("/dev/null").expect("open /dev/null");
        let unrelated_fd = unrelated.as_raw_fd();
        assert!(cloexec_is_set(unrelated_fd));

        let backend = UnixPtyBackend::new().expect("backend");
        backend
            .prepare_fds_for_exec(&[master_fd])
            .expect("prepare_fds_for_exec failed");

        assert!(
            !cloexec_is_set(master_fd),
            "listed fd should survive execve"
        );
        assert!(
            cloexec_is_set(unrelated_fd),
            "unlisted fd must keep its close-on-exec state"
        );

        drop(unrelated);
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
    }

    /// The manifest source: adopted (and spawned) terminals show up, reserved-but-unopened ones
    /// do not.
    #[test]
    fn terminal_fd_table_lists_adopted_fd() {
        let pty = openpty(None, &None).expect("openpty failed");
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();
        let backend = UnixPtyBackend::new().expect("backend");
        assert!(backend.terminal_fd_table().is_empty());
        backend.reserve_terminal_id(9); // reserved, no fd yet
        let reader = backend
            .adopt_terminal(
                5,
                master_fd,
                None,
                24,
                80,
                Box::new(|_, _, _| {}),
                run_command_for_test("true"),
            )
            .expect("adopt_terminal failed");
        assert_eq!(backend.terminal_fd_table(), vec![(5, master_fd)]);
        drop(reader);
        let _ = unistd::close(slave_fd);
    }
}
