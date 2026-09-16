//! Transport layer for a live server handover ("resurrection + FD adoption").
//!
//! Phase 3 of the Gezellij plan keeps *processes* alive across a server upgrade instead of
//! restarting them: the outgoing server hands the PTY master file descriptors of every pane to the
//! incoming server over a Unix domain socket, together with a JSON [`HandoverManifest`] that says
//! which descriptor belongs to which pane and how the pane should be re-created on the other side.
//! Layout travels separately, through the existing `session-layout.kdl` resurrection file; this
//! module only moves descriptors and the small amount of metadata needed to reconnect them.
//!
//! See `HANDOVER_DESIGN.md` at the repository root for the whole design. This file is deliberately
//! self-contained: it knows nothing about the server, panes or layouts, which makes it testable on
//! its own and reusable by an `execve`-based handover later on.
//!
//! # Wire format
//!
//! ```text
//! [u32 big endian: manifest length] [manifest JSON bytes]
//! then, repeated until manifest.panes.len() descriptors have been transferred:
//!   sendmsg( iov = [1 byte: number of fds in this chunk],
//!            cmsg = SCM_RIGHTS(that many fds) )
//! ```
//!
//! The descriptors arrive in `manifest.panes` order - one PTY master per pane. They are chunked
//! because `SCM_RIGHTS` is limited to `SCM_MAX_FD` (253) descriptors per control message; we use a
//! conservative [`FDS_PER_CHUNK`] of 64. The one-byte payload carries the chunk's descriptor count
//! so the receiver can tell "the kernel truncated my control message" from "the sender only had
//! that many descriptors".

use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use serde::{Deserialize, Serialize};

use crate::consts::{ZELLIJ_SOCK_DIR, ZELLIJ_SOCK_MAX_LENGTH};
use crate::input::command::RestartPolicy;
use crate::shared::set_permissions;

/// Version of the handover wire protocol implemented by this build.
///
/// Bumped only for *incompatible* changes. Adding optional fields to the manifest does not need a
/// bump: every field added after v1 carries `#[serde(default)]`, and unknown fields are ignored,
/// so an older receiver can read a newer manifest and vice versa.
pub const HANDOVER_PROTOCOL_VERSION: u32 = 1;

/// Descriptors per `SCM_RIGHTS` control message. The kernel limit (`SCM_MAX_FD`) is 253; staying
/// well below it keeps the control buffer small and leaves room for future ancillary data.
pub const FDS_PER_CHUNK: usize = 64;

/// Refuse to allocate a buffer for a manifest larger than this (a sanity bound against a
/// corrupt/hostile length prefix). 16 MiB is far more than any realistic session needs.
const MAX_MANIFEST_LEN: u32 = 16 * 1024 * 1024;

/// Sub-directory of [`ZELLIJ_SOCK_DIR`] holding handover rendezvous sockets.
///
/// It is deliberately a *directory*: `sessions::get_sessions()` enumerates every entry of
/// `ZELLIJ_SOCK_DIR` that is a socket and connects to it as if it were a session server, so a
/// handover socket sitting directly in the socket directory would show up as a phantom session in
/// `zellij list-sessions` and in the session-manager plugin, and would be sent a client handshake.
/// A directory entry is skipped by that scan.
pub const HANDOVER_SOCKET_SUBDIR: &str = "handover";

/// One pane's worth of handover metadata. One PTY master descriptor is transferred per entry, in
/// the order these appear in [`HandoverManifest::panes`].
///
/// New fields must be added with `#[serde(default)]` so that manifests written by other versions
/// keep deserializing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneHandover {
    /// The outgoing server's terminal id for this pane. The incoming server uses it to correlate
    /// the pane in the resurrected layout with the descriptor it received.
    pub terminal_id: u32,
    /// PID of the process attached to the PTY, if the outgoing server knows it. `None` for panes
    /// whose child already exited (the descriptor is then still useful for its scrollback-free
    /// grid, but the pane is effectively dead).
    pub child_pid: Option<u32>,
    /// argv of the command running in the pane (`None` for a plain shell pane opened with the
    /// default shell).
    pub command: Option<Vec<String>>,
    /// Working directory the command was started in.
    pub cwd: Option<PathBuf>,
    /// Supervision policy, so a re-run after the handover keeps behaving like a service.
    pub restart: RestartPolicy,
    /// Path of a best-effort scrollback dump written by the outgoing server, to be replayed into
    /// the pane before the live reader is attached.
    pub scrollback_file: Option<PathBuf>,
    /// Pane size at handover time, as `(rows, cols)`. The incoming server re-applies it with
    /// `TIOCSWINSZ` once the descriptor is installed, because the resurrected layout may compute a
    /// slightly different geometry.
    pub size: (u16, u16),
}

impl PaneHandover {
    /// A pane entry with only the mandatory correlation key filled in.
    pub fn new(terminal_id: u32) -> Self {
        PaneHandover {
            terminal_id,
            ..Default::default()
        }
    }
    /// Rows of [`PaneHandover::size`].
    pub fn rows(&self) -> u16 {
        self.size.0
    }
    /// Columns of [`PaneHandover::size`].
    pub fn cols(&self) -> u16 {
        self.size.1
    }
}

/// Everything the incoming server needs in order to adopt the outgoing server's panes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HandoverManifest {
    /// [`HANDOVER_PROTOCOL_VERSION`] of the sender.
    pub protocol_version: u32,
    /// Name of the session being handed over.
    pub session_name: String,
    /// Human-readable version string of the outgoing server, for logging and for refusing
    /// nonsensical handovers.
    pub old_server_version: String,
    /// One entry per transferred PTY master descriptor, in transfer order.
    pub panes: Vec<PaneHandover>,
}

impl Default for HandoverManifest {
    fn default() -> Self {
        HandoverManifest {
            protocol_version: HANDOVER_PROTOCOL_VERSION,
            session_name: String::new(),
            old_server_version: String::new(),
            panes: Vec::new(),
        }
    }
}

impl HandoverManifest {
    pub fn new(session_name: impl Into<String>, old_server_version: impl Into<String>) -> Self {
        HandoverManifest {
            protocol_version: HANDOVER_PROTOCOL_VERSION,
            session_name: session_name.into(),
            old_server_version: old_server_version.into(),
            panes: Vec::new(),
        }
    }
    /// How many descriptors follow the manifest on the wire.
    pub fn expected_fd_count(&self) -> usize {
        self.panes.len()
    }
    /// Whether a manifest received from a peer can be understood by this build.
    pub fn is_compatible(&self) -> bool {
        self.protocol_version <= HANDOVER_PROTOCOL_VERSION
    }
}

// ---------------------------------------------------------------------------------------------
// socket paths
// ---------------------------------------------------------------------------------------------

/// Directory holding handover rendezvous sockets for the current contract version.
///
/// Note that this lives *inside* the contract-version-scoped socket directory: two servers with a
/// different `CLIENT_SERVER_CONTRACT_VERSION` do not share it, so a handover across that boundary
/// must pass the socket path explicitly (see `HANDOVER_DESIGN.md`).
pub fn handover_socket_dir() -> PathBuf {
    ZELLIJ_SOCK_DIR.join(HANDOVER_SOCKET_SUBDIR)
}

/// Rendezvous socket path for `session_name`.
pub fn handover_socket_path(session_name: &str) -> PathBuf {
    handover_socket_dir().join(session_name)
}

fn check_socket_path_length(path: &Path) -> io::Result<()> {
    if path.as_os_str().len() >= ZELLIJ_SOCK_MAX_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "handover socket path is too long ({} bytes, max {}): {}",
                path.as_os_str().len(),
                ZELLIJ_SOCK_MAX_LENGTH,
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Bind a handover listener at an explicit path: create the parent directory, remove a stale
/// socket file, bind, and tighten the mode to 0600.
pub fn bind_handover_listener_at(path: &Path) -> io::Result<UnixListener> {
    check_socket_path_length(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = set_permissions(parent, 0o700);
    }
    match std::fs::remove_file(path) {
        Ok(()) => {},
        Err(e) if e.kind() == io::ErrorKind::NotFound => {},
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    set_permissions(path, 0o600)?;
    Ok(listener)
}

/// Bind the handover listener for `session_name` under [`handover_socket_dir`].
pub fn bind_handover_listener(session_name: &str) -> io::Result<UnixListener> {
    bind_handover_listener_at(&handover_socket_path(session_name))
}

/// Connect to the handover listener at an explicit path.
pub fn connect_handover_at(path: &Path) -> io::Result<UnixStream> {
    check_socket_path_length(path)?;
    UnixStream::connect(path)
}

/// Connect to the handover listener for `session_name`.
pub fn connect_handover(session_name: &str) -> io::Result<UnixStream> {
    connect_handover_at(&handover_socket_path(session_name))
}

/// Remove the rendezvous socket for `session_name`, ignoring a missing file.
pub fn remove_handover_socket(session_name: &str) -> io::Result<()> {
    match std::fs::remove_file(handover_socket_path(session_name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// sending
// ---------------------------------------------------------------------------------------------

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.into())
}

/// Write the length-prefixed JSON manifest. No descriptors are sent.
pub fn send_manifest(stream: &UnixStream, manifest: &HandoverManifest) -> io::Result<()> {
    let json = serde_json::to_vec(manifest)
        .map_err(|e| other(format!("failed to serialize handover manifest: {}", e)))?;
    if json.len() as u64 > MAX_MANIFEST_LEN as u64 {
        return Err(other(format!(
            "handover manifest is too large ({} bytes, max {})",
            json.len(),
            MAX_MANIFEST_LEN
        )));
    }
    let mut stream = stream;
    stream.write_all(&(json.len() as u32).to_be_bytes())?;
    stream.write_all(&json)?;
    stream.flush()
}

/// Send one `SCM_RIGHTS` chunk: a single payload byte holding the descriptor count, plus the
/// descriptors themselves. `fds` must be non-empty and at most [`FDS_PER_CHUNK`] long.
pub fn send_fd_chunk(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() || fds.len() > FDS_PER_CHUNK {
        return Err(other(format!(
            "invalid handover fd chunk size {} (must be 1..={})",
            fds.len(),
            FDS_PER_CHUNK
        )));
    }
    let payload = [fds.len() as u8];
    let iov = [IoSlice::new(&payload)];
    let cmsg = [ControlMessage::ScmRights(fds)];
    loop {
        match sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None) {
            Ok(1) => return Ok(()),
            Ok(n) => {
                return Err(other(format!(
                    "short sendmsg while transferring handover fds: wrote {} of 1 byte",
                    n
                )))
            },
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(io::Error::from(e)),
        }
    }
}

/// Send a complete handover: the manifest, then every descriptor in `panes` order.
///
/// `fds` must have exactly one entry per pane in `manifest`; the caller keeps ownership of them
/// (the kernel duplicates them into the receiver, so they can be closed right afterwards).
pub fn send_handover<F: AsRawFd>(
    stream: &UnixStream,
    manifest: &HandoverManifest,
    fds: &[F],
) -> io::Result<()> {
    if fds.len() != manifest.expected_fd_count() {
        return Err(other(format!(
            "handover manifest lists {} pane(s) but {} file descriptor(s) were given",
            manifest.expected_fd_count(),
            fds.len()
        )));
    }
    send_manifest(stream, manifest)?;
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    for chunk in raw.chunks(FDS_PER_CHUNK) {
        send_fd_chunk(stream, chunk)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// receiving
// ---------------------------------------------------------------------------------------------

/// Read the length-prefixed JSON manifest. Does not read any descriptors.
pub fn recv_manifest(stream: &UnixStream) -> io::Result<HandoverManifest> {
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MANIFEST_LEN {
        return Err(other(format!(
            "handover manifest length prefix is implausible ({} bytes, max {})",
            len, MAX_MANIFEST_LEN
        )));
    }
    let mut json = vec![0u8; len as usize];
    stream.read_exact(&mut json)?;
    let manifest: HandoverManifest = serde_json::from_slice(&json)
        .map_err(|e| other(format!("failed to parse handover manifest: {}", e)))?;
    if !manifest.is_compatible() {
        return Err(other(format!(
            "handover manifest speaks protocol version {}, this build understands at most {}",
            manifest.protocol_version, HANDOVER_PROTOCOL_VERSION
        )));
    }
    Ok(manifest)
}

/// Receive one `SCM_RIGHTS` chunk.
///
/// Returns `Ok(None)` on a clean EOF before the chunk started, so the caller can produce a precise
/// "the sender promised more descriptors than it delivered" error.
pub fn recv_fd_chunk(stream: &UnixStream) -> io::Result<Option<Vec<OwnedFd>>> {
    let mut payload = [0u8; 1];
    let mut cmsg_buffer = nix::cmsg_space!([RawFd; FDS_PER_CHUNK]);
    #[cfg(target_os = "linux")]
    let flags = MsgFlags::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = MsgFlags::empty();

    let (bytes, cmsgs) = loop {
        let mut iov = [IoSliceMut::new(&mut payload)];
        match recvmsg::<()>(stream.as_raw_fd(), &mut iov, Some(&mut cmsg_buffer), flags) {
            Ok(msg) => {
                let bytes = msg.bytes;
                // `cmsgs()` errors with ENOBUFS when MSG_CTRUNC is set, i.e. when the kernel had
                // to drop part of the control message. Those descriptors are already closed by the
                // kernel, so there is nothing to clean up - but the handover is unrecoverable.
                let cmsgs: Vec<ControlMessageOwned> =
                    match msg.cmsgs() {
                        Ok(iter) => iter.collect(),
                        Err(nix::errno::Errno::ENOBUFS) => return Err(other(
                            "handover control message was truncated (MSG_CTRUNC); the transferred \
                             file descriptors were dropped by the kernel",
                        )),
                        Err(e) => return Err(io::Error::from(e)),
                    };
                break (bytes, cmsgs);
            },
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(io::Error::from(e)),
        }
    };

    let mut received: Vec<OwnedFd> = Vec::new();
    for cmsg in cmsgs {
        if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
            for raw in raw_fds {
                // Take ownership immediately so any error path below still closes them.
                received.push(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
    }

    if bytes == 0 {
        if !received.is_empty() {
            return Err(other(format!(
                "handover peer sent {} file descriptor(s) without a chunk header",
                received.len()
            )));
        }
        return Ok(None); // clean EOF
    }
    let announced = payload[0] as usize;
    if announced == 0 || announced > FDS_PER_CHUNK {
        return Err(other(format!(
            "handover peer announced an invalid chunk size of {}",
            announced
        )));
    }
    if received.len() != announced {
        return Err(other(format!(
            "handover chunk announced {} file descriptor(s) but {} arrived",
            announced,
            received.len()
        )));
    }
    Ok(Some(received))
}

/// Receive a complete handover: the manifest followed by exactly one descriptor per pane.
///
/// On any error every descriptor received so far is dropped (and therefore closed).
pub fn recv_handover(stream: &UnixStream) -> io::Result<(HandoverManifest, Vec<OwnedFd>)> {
    let manifest = recv_manifest(stream)?;
    let expected = manifest.expected_fd_count();
    let mut fds: Vec<OwnedFd> = Vec::with_capacity(expected);
    while fds.len() < expected {
        match recv_fd_chunk(stream)? {
            Some(mut chunk) => {
                fds.append(&mut chunk);
                if fds.len() > expected {
                    return Err(other(format!(
                        "handover peer sent more file descriptors than its manifest promised \
                         ({} > {})",
                        fds.len(),
                        expected
                    )));
                }
            },
            None => {
                return Err(other(format!(
                    "handover ended early: manifest promised {} file descriptor(s), only {} \
                     arrived",
                    expected,
                    fds.len()
                )))
            },
        }
    }
    Ok((manifest, fds))
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::fd::{AsFd, OwnedFd};

    fn sample_manifest() -> HandoverManifest {
        let mut manifest = HandoverManifest::new("my-session", "gezellij 0.46.0");
        manifest.panes.push(PaneHandover {
            terminal_id: 0,
            child_pid: Some(4242),
            command: Some(vec!["bash".into(), "-lc".into(), "sleep 1000".into()]),
            cwd: Some(PathBuf::from("/home/joop/gezellij")),
            restart: RestartPolicy::OnFailure,
            scrollback_file: Some(PathBuf::from("/tmp/scrollback-0.dump")),
            size: (24, 80),
        });
        manifest.panes.push(PaneHandover::new(7));
        manifest
    }

    /// A temp file whose whole content is `content`, rewound, handed over as an owned fd.
    fn fd_with_content(content: &str) -> OwnedFd {
        let mut file = tempfile::tempfile().expect("could not create temp file");
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        OwnedFd::from(file)
    }

    fn read_all(fd: &OwnedFd) -> String {
        let mut file = std::fs::File::from(fd.try_clone().unwrap());
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut out = String::new();
        file.read_to_string(&mut out).unwrap();
        out
    }

    fn open_file_limit() -> u64 {
        let mut limit = libc::rlimit {
            rlim_cur: 1024,
            rlim_max: 1024,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
            limit.rlim_cur as u64
        } else {
            1024
        }
    }

    #[test]
    fn manifest_json_round_trip() {
        let manifest = sample_manifest();
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: HandoverManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
        assert_eq!(parsed.protocol_version, HANDOVER_PROTOCOL_VERSION);
        assert_eq!(parsed.expected_fd_count(), 2);
        assert_eq!(parsed.panes[0].rows(), 24);
        assert_eq!(parsed.panes[0].cols(), 80);
        assert_eq!(parsed.panes[1].restart, RestartPolicy::No);
    }

    #[test]
    fn manifest_tolerates_unknown_future_fields() {
        // What a future (v1-compatible) sender might emit: extra keys at both levels.
        let json = r#"{
            "protocol_version": 1,
            "session_name": "future",
            "old_server_version": "gezellij 0.99.0",
            "cgroup_path": "/sys/fs/cgroup/user.slice/gezellij-future",
            "panes": [
                {
                    "terminal_id": 3,
                    "child_pid": 17,
                    "size": [40, 120],
                    "restart": "always",
                    "ula_address": "fd00:2830::8008",
                    "pidfd_index": 0
                }
            ]
        }"#;
        let parsed: HandoverManifest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.session_name, "future");
        assert_eq!(parsed.panes.len(), 1);
        assert_eq!(parsed.panes[0].terminal_id, 3);
        assert_eq!(parsed.panes[0].restart, RestartPolicy::Always);
        assert_eq!(parsed.panes[0].size, (40, 120));
        // fields the future sender omitted fall back to their defaults
        assert_eq!(parsed.panes[0].command, None);
        assert_eq!(parsed.panes[0].cwd, None);
    }

    #[test]
    fn manifest_missing_fields_use_defaults() {
        let parsed: HandoverManifest = serde_json::from_str(r#"{"session_name": "bare"}"#).unwrap();
        assert_eq!(parsed.protocol_version, HANDOVER_PROTOCOL_VERSION);
        assert_eq!(parsed.session_name, "bare");
        assert!(parsed.panes.is_empty());
        assert!(parsed.is_compatible());
    }

    #[test]
    fn rejects_newer_protocol_version() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut manifest = HandoverManifest::new("too-new", "gezellij 99");
        manifest.protocol_version = HANDOVER_PROTOCOL_VERSION + 1;
        send_manifest(&a, &manifest).unwrap();
        let err = recv_manifest(&b).unwrap_err();
        assert!(
            err.to_string().contains("protocol version"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn transfers_three_fds_with_distinct_content() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let contents = ["first pane", "second pane", "third pane"];
        let fds: Vec<OwnedFd> = contents.iter().map(|c| fd_with_content(c)).collect();

        let mut manifest = HandoverManifest::new("three", "gezellij test");
        for (i, _) in contents.iter().enumerate() {
            manifest.panes.push(PaneHandover::new(i as u32 * 2));
        }

        let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
        send_handover(&sender, &manifest, &borrowed).unwrap();
        drop(fds); // the receiver has its own copies now

        let (received_manifest, received_fds) = recv_handover(&receiver).unwrap();
        assert_eq!(received_manifest.session_name, "three");
        assert_eq!(received_fds.len(), 3);
        for (i, fd) in received_fds.iter().enumerate() {
            assert_eq!(read_all(fd), contents[i], "fd {} carried wrong content", i);
        }
        assert_eq!(
            received_manifest
                .panes
                .iter()
                .map(|p| p.terminal_id)
                .collect::<Vec<_>>(),
            vec![0, 2, 4]
        );
    }

    #[test]
    fn transfers_zero_fds() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let manifest = HandoverManifest::new("empty", "gezellij test");
        let no_fds: Vec<OwnedFd> = Vec::new();
        send_handover(&sender, &manifest, &no_fds).unwrap();
        drop(sender);
        let (received_manifest, received_fds) = recv_handover(&receiver).unwrap();
        assert_eq!(received_manifest.session_name, "empty");
        assert!(received_fds.is_empty());
    }

    #[test]
    fn transfers_many_fds_across_chunks() {
        // 300 descriptors exercise the SCM_RIGHTS chunking (5 chunks of 64 plus a remainder).
        // Each temp file costs one descriptor on the sending side and one on the receiving side,
        // so a low RLIMIT_NOFILE forces us down to 200 (documented in the assertion message).
        let limit = open_file_limit();
        let count = if limit >= 1024 { 300 } else { 200 };
        assert!(
            limit > (count as u64) * 2 + 64,
            "RLIMIT_NOFILE={} is too low to run the chunking test with {} descriptors",
            limit,
            count
        );

        let (sender, receiver) = UnixStream::pair().unwrap();
        let fds: Vec<OwnedFd> = (0..count)
            .map(|i| fd_with_content(&format!("pane-{}", i)))
            .collect();
        let mut manifest = HandoverManifest::new("many", "gezellij test");
        for i in 0..count {
            manifest.panes.push(PaneHandover::new(i as u32));
        }

        let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
        send_handover(&sender, &manifest, &borrowed).unwrap();
        // Close the originals before receiving so the peak descriptor count stays near `count`:
        // in-flight SCM_RIGHTS descriptors do not occupy a slot in either process's fd table.
        drop(borrowed);
        drop(fds);

        let (received_manifest, received_fds) = recv_handover(&receiver).unwrap();
        assert_eq!(received_manifest.panes.len(), count);
        assert_eq!(received_fds.len(), count);
        for (i, fd) in received_fds.iter().enumerate() {
            assert_eq!(read_all(fd), format!("pane-{}", i));
        }
    }

    #[test]
    fn errors_when_manifest_promises_more_fds_than_sent() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut manifest = HandoverManifest::new("short", "gezellij test");
        manifest.panes.push(PaneHandover::new(0));
        manifest.panes.push(PaneHandover::new(1));

        // Bypass send_handover's own arity check to simulate a sender that crashed halfway.
        send_manifest(&sender, &manifest).unwrap();
        let only_fd = fd_with_content("only one");
        send_fd_chunk(&sender, &[only_fd.as_raw_fd()]).unwrap();
        drop(only_fd);
        drop(sender); // the receiver must see EOF rather than block forever

        let err = recv_handover(&receiver).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("promised 2") && msg.contains("only 1"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn send_handover_rejects_fd_count_mismatch() {
        let (sender, _receiver) = UnixStream::pair().unwrap();
        let mut manifest = HandoverManifest::new("mismatch", "gezellij test");
        manifest.panes.push(PaneHandover::new(0));
        manifest.panes.push(PaneHandover::new(1));
        let fd = fd_with_content("only one");
        let err = send_handover(&sender, &manifest, &[fd.as_fd()]).unwrap_err();
        assert!(
            err.to_string().contains("lists 2 pane(s)"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn send_fd_chunk_rejects_bad_chunk_sizes() {
        let (sender, _receiver) = UnixStream::pair().unwrap();
        assert!(send_fd_chunk(&sender, &[]).is_err());
        let too_many: Vec<RawFd> = vec![sender.as_raw_fd(); FDS_PER_CHUNK + 1];
        assert!(send_fd_chunk(&sender, &too_many).is_err());
    }

    #[test]
    fn socket_paths_live_in_a_subdirectory_of_the_socket_dir() {
        let path = handover_socket_path("my-session");
        assert_eq!(path.file_name().unwrap(), "my-session");
        assert_eq!(path.parent().unwrap(), handover_socket_dir());
        assert_eq!(
            handover_socket_dir().file_name().unwrap(),
            HANDOVER_SOCKET_SUBDIR
        );
        // Crucially, NOT directly inside ZELLIJ_SOCK_DIR: session listing would connect to it.
        assert_ne!(path.parent().unwrap(), &*ZELLIJ_SOCK_DIR);
        assert!(path.starts_with(&*ZELLIJ_SOCK_DIR));
    }

    #[test]
    fn rejects_overlong_socket_paths() {
        let long = PathBuf::from("/tmp").join("x".repeat(ZELLIJ_SOCK_MAX_LENGTH));
        let err = bind_handover_listener_at(&long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(connect_handover_at(&long).is_err());
    }

    #[test]
    fn bind_connect_and_handover_over_a_real_socket() {
        // Binding may be forbidden by the sandbox this test runs in; skip gracefully then.
        let dir = match tempfile::tempdir_in(std::env::temp_dir()) {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("skipping socket test: could not create temp dir ({})", e);
                return;
            },
        };
        let path = dir.path().join("handover.sock");
        let listener = match bind_handover_listener_at(&path) {
            Ok(listener) => listener,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ) || matches!(
                    e.raw_os_error(),
                    Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EOPNOTSUPP)
                ) =>
            {
                eprintln!("skipping socket test: bind not permitted here ({})", e);
                return;
            },
            Err(e) => panic!("unexpected bind failure: {}", e),
        };

        // A second bind at the same path must succeed by removing the stale socket file.
        let listener2 = bind_handover_listener_at(&path).expect("stale socket was not replaced");
        drop(listener);

        let mut manifest = HandoverManifest::new("real-socket", "gezellij test");
        manifest.panes.push(PaneHandover::new(11));
        let fd = fd_with_content("hello from the old server");

        let manifest_for_sender = manifest.clone();
        let sender_path = path.clone();
        let sender = std::thread::spawn(move || {
            let stream = connect_handover_at(&sender_path).unwrap();
            send_handover(&stream, &manifest_for_sender, &[fd.as_fd()]).unwrap();
            // keep the stream alive until the receiver is done
            std::thread::sleep(std::time::Duration::from_millis(50));
        });

        let (stream, _addr) = listener2.accept().unwrap();
        let (received_manifest, received_fds) = recv_handover(&stream).unwrap();
        assert_eq!(received_manifest.session_name, "real-socket");
        assert_eq!(received_manifest.panes[0].terminal_id, 11);
        assert_eq!(received_fds.len(), 1);
        assert_eq!(read_all(&received_fds[0]), "hello from the old server");
        sender.join().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "handover socket must be user-only");
        }
    }
}
