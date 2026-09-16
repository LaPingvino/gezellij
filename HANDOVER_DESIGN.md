# Phase 3: Live Server Handover ("resurrection + FD adoption")

Status: design. Transport implemented in `zellij-utils/src/host_fabric/handover.rs`; nothing in
`zellij-server` uses it yet.

Goal (`GEZELLIJ_PLAN.md` §5.1): upgrading the Gezellij binary must not kill the processes that
are running in the panes. Terminal grid, scrollback and WASM plugin state are explicitly *not*
significant. So the new server does not try to clone the old server's memory; it re-creates the
session from the layout file that already exists for resurrection, and **adopts the live PTY master
file descriptors** instead of spawning fresh children.

## 1. What already exists

| Piece | Where | What it gives us |
|---|---|---|
| PTY master fds per pane | `zellij-server/src/os_input_output_unix.rs`, `UnixPtyBackend::terminal_id_to_raw_fd: BTreeMap<u32, Option<RawFd>>` | the exact thing we need to pass |
| child pid per pane | `zellij-server/src/pty.rs`, `Pty::id_to_child_pid: HashMap<u32, u32>` | signalling and liveness checks |
| reader task | `zellij-server/src/terminal_bytes.rs`, `TerminalBytes::listen()` reading a `Box<dyn AsyncReader>` built by `RawFdAsyncReader::new(pid_primary)` | can be re-attached to an adopted fd unchanged |
| layout resurrection | `zellij-utils/src/session_serialization.rs::serialize_session_layout` → `<session info>/session-layout.kdl` + `initial_contents_N` files, parsed back by `zellij-utils/src/kdl/kdl_layout_parser.rs` | layout *and* per-pane scrollback, already carrying `restart` |
| restart policy | `zellij-utils/src/input/command.rs::RestartPolicy`, applied in `pty.rs::command_exit_callback` | supervision survives if we carry it in the manifest |
| detached server start | `zellij-client/src/lib.rs::start_server_detached` / `create_session_on_running_server`, `--server-foreground` (`cli.rs:80`) | how to launch the new server |
| fd transport | `zellij-utils/src/host_fabric/handover.rs` (this design) | `HandoverManifest` + `SCM_RIGHTS` |

Two facts constrain everything below:

1. **`ZELLIJ_SOCK_DIR` is contract-version scoped** (`consts.rs:26,91`:
   `…/contract_version_<CLIENT_SERVER_CONTRACT_VERSION>/`). Two servers whose
   `CLIENT_SERVER_CONTRACT_VERSION` differs do not share a socket directory at all. A handover that
   crosses that boundary must therefore be told the rendezvous path explicitly; it cannot be
   discovered.
2. **`sessions::get_sessions()` connects to every socket file in `ZELLIJ_SOCK_DIR`.** That is why
   `handover_socket_path()` puts the rendezvous socket in a `handover/` *sub-directory*
   (`<sock dir>/handover/<session>`): a directory entry is skipped by the scan, a socket entry would
   appear as a phantom session in `list-sessions` and in the session-manager plugin.

## 2. Trigger

```
zellij upgrade-server [SESSION]        # explicit, one session
zellij attach --replace-server SESSION # implicit, on attach with a newer binary
```

`upgrade-server` resolves the session, connects to the running server over the normal IPC socket and
sends a new `ClientToServerMsg::PrepareHandover { socket_path, new_server_version }`. The *client*
binary — which is the new version, since the user just upgraded it — is the one that launches the
new server, exactly the way `start_server_detached` does today, plus `--adopt <socket path>`.

## 3. Sequence

```
 new binary (CLI)        old server                 new server              children
      |                      |                           |                      |
 1.   |--PrepareHandover---->|                            |                     |
      |                      |-- serialize_session_layout |                     |
      |                      |   -> session-layout.kdl    |                     |
      |                      |   + initial_contents_N     |                     |
      |                      |-- bind_handover_listener() |                     |
      |<---HandoverReady-----|   (<sock dir>/handover/S)  |                     |
      |                      |                            |                     |
 2.   |--spawn(--adopt S)------------------------------->  |                    |
      |                      |<---connect_handover(S)-----|                     |
 3.   |                      |--send_handover(manifest,fds)->                    |
      |                      |   one PTY master per pane  |                     |
 4.   |                      |                            |-- resurrect layout   |
      |                      |                            |-- adopt_terminal()  -|--> still alive
      |                      |<---HandoverAck-------------|                      |
 5.   |                      |-- stop reader tasks        |                      |
      |                      |-- close PTY masters        |                      |
      |                      |-- unlink <sock dir>/S      |                      |
      |                      |-- tell clients to reattach |                      |
      |                      |-- exit(0)                  |                      |
 6.   |                      x                            |-- bind <sock dir>/S  |
      |<--------------------------- accept clients -------|                      |
```

Step 5/6 ordering matters: the old server must unlink/close its IPC socket *before* the new server
binds the same path, otherwise the new bind fails with `EADDRINUSE` (and blindly removing the file
first would steal the socket from a still-healthy old server). The ack in step 4 is what makes this
safe — before the ack, nothing has been torn down.

## 4. What the old server sends

**Out of band (files):**

* `session-layout.kdl` via the existing `PtyInstruction::DumpLayout` path — tabs, splits, sizes,
  commands, cwds, `restart`, and `contents_file` references.
* One `initial_contents_N` file per pane: the scrollback dump. This is best-effort by decision; a
  pane that fails to dump simply comes back empty.

**Over the handover socket:** one `HandoverManifest` (JSON, length-prefixed) plus one PTY master fd
per entry of `manifest.panes`, in order, via `SCM_RIGHTS`:

```rust
PaneHandover {
    terminal_id: u32,                    // correlation key
    child_pid: Option<u32>,
    command: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    restart: RestartPolicy,
    scrollback_file: Option<PathBuf>,
    size: (u16 /*rows*/, u16 /*cols*/),
}
```

`command`/`cwd`/`restart` are redundant with the layout file on purpose: they let the new server
re-run a pane whose child died during the handover window without re-parsing the layout, and they
make the manifest self-describing for debugging (`--adopt-dump`).

## 5. What the new server does

Started as `zellij --server <ipc socket> --adopt <handover socket>`:

1. `connect_handover_at(path)`, `recv_handover()` → `(manifest, Vec<OwnedFd>)`.
2. Refuse if `!manifest.is_compatible()` or `manifest.session_name` is not the session we were told
   to become. Exit non-zero without acking → old server rolls back.
3. Read `session-layout.kdl` and build the session the way resurrection already does
   (`spawn_terminals_for_layout`), but in *adopt* mode: for each pane that has a manifest entry, do
   not fork anything.
4. For each `(PaneHandover, OwnedFd)` pair:
   * `UnixPtyBackend::adopt_terminal(terminal_id, fd)` — insert into `terminal_id_to_raw_fd`, build a
     `RawFdAsyncReader`, and raise `next_terminal_id_counter` above every adopted id so a later
     `next_terminal_id()` cannot collide.
   * `Pty::id_to_child_pid.insert(terminal_id, child_pid)`.
   * Replay `scrollback_file` into the grid, *then* spawn `TerminalBytes::listen()` on the reader, so
     live output can never be overtaken by the replay.
   * Re-apply `size` with `set_terminal_size_using_fd` (the resurrected layout may compute a slightly
     different geometry than the old server had).
   * Mark the pane adopted (see exit detection below).
5. Send `HandoverAck` on the handover socket, close it, `remove_handover_socket()`.
6. Bind the real IPC socket and start serving. Plugins start fresh — by decision.

Panes in the layout *without* a manifest entry (plugins, or a child that exited mid-handover) are
created the ordinary way.

## 6. The hard part: adopted children are not our children

`handle_openpty` (os_input_output_unix.rs:236) spawns a reaper thread per pane that does
`child.wait()` and then calls `quit_cb`, which is what drives hold-on-exit and the `RestartPolicy`
backoff in `pty.rs::command_exit_callback`. **The new server cannot do this.** `wait()`/`waitpid()`
only work for your own children; for an adopted pid they return `ECHILD`. Consequences:

* We can detect *that* an adopted process exited, cheaply and reliably:
  * `pidfd_open(pid)` + poll for `POLLIN` (Linux ≥ 5.3) — race-free, no polling loop;
  * fallback: `kill(pid, 0)` on a timer, or watching `/proc/<pid>`, both racy against pid reuse;
  * and in practice the PTY master hits EOF/`EIO` when the last slave closes, which
    `TerminalBytes::listen()` already treats as the end of the pane.
* We **cannot** obtain its exit status. Nobody but the parent can. Since the old server is gone, the
  child has been reparented to pid 1 (or the nearest ancestor subreaper), and *that* process reaps it
  and discards the status.

Be honest about the mitigation options, because the obvious one does not work:

* **`prctl(PR_SET_CHILD_SUBREAPER)` in the new server does not help.** Orphans are reparented to the
  nearest *ancestor* marked subreaper. The new server is a sibling of the old one (spawned by the
  CLI), not an ancestor of the old server's children, so it will never inherit them. Setting the flag
  costs nothing and is worth doing for the new server's *own* future descendants, but it does not
  recover exit statuses for this handover.
* **A reaper stub that is a common ancestor.** If the very first server for a session were started
  under a tiny long-lived `gezellij-reaper` process with `PR_SET_CHILD_SUBREAPER`, orphaned pane
  children would reparent to it, it could `waitpid()` them and report `(pid, status)` over a pipe to
  whichever server is current. This works, and it is the only way to keep exit statuses exactly. Cost:
  a new always-on process per session, and it must survive the handover, so it becomes the thing that
  needs upgrading instead.
* **`execve` in place instead of a socket.** The old server clears `FD_CLOEXEC` on the PTY masters,
  writes the manifest to a `memfd`, and `execve`s the new binary with `--adopt-fd <n>`. Same pid, so
  the children stay *ours* and remain waitable; fds survive `exec`; systemd's `MainPID` does not
  change. The transport in `handover.rs` is then unnecessary, though `HandoverManifest` is still the
  payload. Cost: **no rollback** — after `execve` the old server no longer exists, so a new binary
  that crashes on start takes the session with it. Mitigate with a `--adopt-check` dry run
  (new binary validates the layout and exits) immediately before the `execve`.

**Recommendation.** Implement the socket handover first (this module): it is rollback-safe, which is
the property that matters for a first version, and the cost is that adopted panes lose exit statuses
— `RestartPolicy::OnFailure` degrades to "unknown ⇒ restart", which is exactly what
`RestartPolicy::should_restart(None)` already does. Keep `execve`-in-place as a later
`--handover-mode=exec` for service sessions, where waitability is worth more than rollback.

### systemd interaction

Under `zellij service run` (`Type=simple`, the old server is `MainPID`), the default
`KillMode=control-group` means: when the old server exits at step 5, systemd kills the whole cgroup —
including the new server and every adopted child. A handover under systemd therefore additionally
needs either `KillMode=mixed`/`process` in the generated unit
(`zellij-utils/src/host_fabric/systemd.rs`), or an `sd_notify` `MAINPID=<new pid>` from the old server
before it exits. This must be settled before `upgrade-server` is offered for `svc-*` sessions.

## 7. Client experience

* **Same `CLIENT_SERVER_CONTRACT_VERSION`:** clients keep working. The old server, just before
  exiting, tells each attached client to reattach to the same session; the client already has that
  machinery (`reconnect_to_session` in `zellij-client/src/lib.rs`). The user sees a flicker and a
  re-render from the resurrected layout, not a dead session.
* **Different contract version:** old clients cannot even find the new socket directory. They must be
  told to exit with a message ("this session was upgraded, run `zellij attach <session>` with the new
  binary"). Automatic reconnection is not possible and should not be faked.
* Either way scrollback above the replayed dump is gone, and plugin panes lose their state. Both are
  accepted by decision.

## 8. Failure modes and rollback

| When | What happens | Recovery |
|---|---|---|
| Layout dump fails | old server refuses the handover before binding the socket | nothing changed |
| New server never connects (timeout, ~10 s) | old server unlinks the handover socket, logs, carries on | nothing changed |
| New server rejects the manifest (version, session name) | exits non-zero without acking | old server carries on; still owns every fd |
| `send_handover` fails mid-transfer | in-flight `SCM_RIGHTS` fds are closed by the kernel when the socket dies; **the old server still holds its originals** | old server carries on |
| `MSG_CTRUNC` on receive | kernel silently dropped fds; `recv_fd_chunk` turns this into a hard error | new server exits without ack |
| New server crashes after ack, before binding IPC | worst case: children alive, no server owns them | `zellij attach` finds no socket; the panes are orphaned and must be killed by hand. Narrow window, but real. |
| Old server dies before the new one binds IPC | same as above | same |

The ack is the commit point, and the window after it is the one genuinely unsafe interval. It can be
shrunk (bind the IPC socket *before* acking, since the old server has already stopped serving it —
this makes step 5/6 an exchange rather than a sequence) but not eliminated.

## 9. Version compatibility

Three independent surfaces:

1. **`HANDOVER_PROTOCOL_VERSION`** (currently 1). `HandoverManifest` and `PaneHandover` use
   `#[serde(default)]`, do *not* `deny_unknown_fields`, and are tested both ways: a v1 reader parses a
   manifest with unknown future keys, and a manifest missing fields gets defaults. A receiver refuses
   a manifest whose `protocol_version` is *higher* than it understands. Bump only for incompatible
   changes.
2. **`session-layout.kdl`** is the real compatibility surface and the fragile one. The layout parser
   uses a strict allow-list (`kdl_layout_parser.rs::is_a_valid_pane_property`), so an *older* binary
   reading a layout written by a *newer* one fails hard on any new property. That is fine for an
   upgrade (new reads old) and broken for a downgrade (old reads new). Pin it with a test that parses
   a checked-in corpus of layout files from previous releases — §5.3 of the plan already asks for
   this.
3. **`CLIENT_SERVER_CONTRACT_VERSION`** affects clients only (see §7); the handover itself is
   unaffected because the socket path is passed explicitly.

## 10. Correlation: the biggest unknown

The manifest's key is `terminal_id`. **`session-layout.kdl` does not record terminal ids.** So "map
pane → fd via terminal_id" needs one of:

* **(a) Add a `terminal_id` property to the serialized pane node.** Cheapest to implement
  (`session_serialization.rs::serialize_tiled_pane` plus the allow-lists in
  `kdl_layout_parser.rs`) — but it makes every new layout file unreadable by older zellij, per §9.2.
  Only acceptable if the property is written to a *separate* file used solely for handover.
* **(b) Correlate by position.** The old server emits `panes` in the same traversal order the layout
  serializer uses; the new server zips its resurrected panes against `manifest.panes` in that order.
  No format change, but any asymmetry in the two traversals silently attaches a pane to the wrong
  process — the worst possible failure mode.
* **(c) A side-car `session-handover.kdl`/JSON** written next to the layout, mapping
  `(tab index, pane path) → terminal_id`. No compatibility impact, one extra file, explicit. **This
  is the recommended option**; it keeps the layout file untouched and makes the mapping verifiable
  (mismatch ⇒ refuse the handover rather than guess).

Whichever is chosen, the new server must verify the mapping before acking: pane count, and for each
pane, that `child_pid` is still alive and that `tcgetpgrp(fd)` succeeds.

## 11. Phased implementation plan

**3.1 — transport (done).** `zellij-utils/src/host_fabric/handover.rs`: `HandoverManifest`,
`send_handover`/`recv_handover`, chunked `SCM_RIGHTS`, socket path helpers, tests.

**3.2 — pty backend adoption.** In `os_input_output_unix.rs`:
`UnixPtyBackend::adopt_terminal(&self, terminal_id: u32, fd: OwnedFd, rows: u16, cols: u16) -> Result<Box<dyn AsyncReader>>`
(insert into `terminal_id_to_raw_fd`, `RawFdAsyncReader::new`, `set_terminal_size_using_fd`, bump
`next_terminal_id_counter`). Add it to the `ServerOsApi` trait. No handover involved yet — unit-test
it against a locally `openpty`'d pair, which is what the existing tests at the bottom of that file
already do.

**3.3 — exit detection for adopted panes.** `pidfd_open` + a poll thread (or the `/proc` fallback)
producing the same `quit_cb(PaneId::Terminal(id), None, cmd)` call that the reaper thread makes
today, so `command_exit_callback` and the restart backoff are reused unchanged.

**3.4 — correlation side-car.** Write and read `session-handover.json` beside `session-layout.kdl`
(option (c)); verification helper that refuses a mismatch.

**3.5 — old-server side.** `ClientToServerMsg::PrepareHandover`, a `ServerInstruction::Handover`
that: dumps the layout, dumps scrollback per pane, builds the manifest from
`terminal_id_to_raw_fd` + `id_to_child_pid`, binds the listener, waits for the connection, sends,
waits for the ack, then tears down.

**3.6 — new-server side.** `--adopt <path>` in `cli.rs`; `PtyInstruction::AdoptTerminal { terminal_id,
fd, pane }`; an adopt-aware variant of `Pty::spawn_terminals_for_layout` that skips the fork for panes
present in the manifest.

**3.7 — CLI + client reconnect.** `zellij upgrade-server`, `--replace-server`, the reconnect message,
and the systemd `KillMode` change.

**3.8 — hardening.** Layout-compat corpus test (§9.2), a `--handover-mode=exec` variant (§6), and an
integration test that upgrades a session running `sleep 100000` and asserts the pid is unchanged.

## 12. Known unknowns

* Whether the resurrection path can be driven *without* spawning, or whether
  `spawn_terminals_for_layout` needs a parallel implementation. Unresolved; it is the largest chunk
  of 3.6.
* Whether the old server can dump scrollback for every pane fast enough that the handover window
  stays under a second on a busy session.
* Whether an adopted PTY master needs its termios re-applied, or whether the flags genuinely travel
  with the fd (they belong to the pty, not the fd, so they should — untested).
* `zellij service run` under `systemd --user` with `Delegate=yes` may put the new server in a
  different cgroup than the adopted children, which interacts with Phase 2 freezing.


## 13. Implemented (2026-09-16): the exec-in-place variant

The first working upgrade took the `--handover-mode=exec` road from §6 rather than the socket
handover, because it dissolves three of the risks above at once: the successor *is* the same
process (pid unchanged, so `systemd` keeps tracking it and every pane child stays a waitable
child), no manifest has to cross a process boundary, and the layout/PTY correlation can be
computed by the very serializer that writes the layout.

Flow, as built:

1. `zellij upgrade-server [session]` finds the server pid (`<sock dir>/<session>.server-pid`),
   refuses if `/proc/<pid>/exe` is not `(deleted)` (unless `--force` or
   `GEZELLIJ_UPGRADE_BINARY` is set), sends `SIGUSR2`, and waits until the pid's exe changed and
   the manifest was consumed.
2. The server's `upgrade_signal` thread turns the signal into `ServerInstruction::PrepareUpgrade`
   → `ScreenInstruction::PrepareUpgrade`, which takes the normal `SessionLayoutMetadata` snapshot
   with `upgrade_requested = true` and routes it through the plugin thread (plugin cwds) to the
   pty thread like any save.
3. `Pty::perform_exec_upgrade`: remembers each pane's original `RunCommand` (restart policy),
   populates the snapshot, writes `session-layout.kdl` + pane contents, computes the
   **adoption order** with `SessionLayoutMetadata::adoption_order` (the real tree builder is run
   once more with a marker in every terminal pane's `run`, then `extract_run_instructions` gives
   the exact order the successor will visit the leaves), builds `ExecUpgradeManifest`
   (per tab: tiled and floating `AdoptablePane { terminal_id, fd, child_pid, run,
   layout_command }`, plus the original `CliAssets` pointed at the new layout file), marks every
   fd close-on-exec, clears it again on the PTY masters (`prepare_fds_for_exec`), and
   `execve`s `<new binary> --server <sock> --adopt <manifest>`. If exec fails everything is
   restored and the old server carries on.
4. The successor (`--adopt` implies `--server-foreground`, never daemonize again) reads the
   manifest, stores the adoption plan, and feeds itself `FirstClientConnected(cli_assets)` +
   `RemoveClient`, i.e. a detached resurrection. In `spawn_terminals_for_layout` each leaf is
   compared with the next inherited pane of that tab (`layout_command` must match; plugin leaves
   never consume a slot); matches are adopted via `ServerOsApi::adopt_terminal` (registers the
   master, re-arms CLOEXEC, starts a `waitpid` reaper that calls the same exit callback the
   restart machinery uses), mismatches are spawned fresh and unclaimed PTYs are hung up.

What this variant does *not* give: rollback after `exec` (a successor that fails to adopt leaves
the session to be resurrected from the layout on disk), and attached clients are dropped rather
than told to reconnect. Scrollback is not carried (per §5.1 of the plan). The socket transport in
`host_fabric/handover.rs` remains the path to a rollback-safe variant.

## 14. The old server offering itself (design, not yet built)

§13 solved "upgrade the session I am sitting in" with `execve`. It cannot solve a different
case: a server that is *already* lingering from a previous version. By the time you notice, the
only process that could hand anything over is one you may not signal, and across a
`CLIENT_SERVER_CONTRACT_VERSION` change it is not even visible to `list-sessions`.

The way out, and the reason the socket transport in `host_fabric/handover.rs` is worth keeping:
**let the old server offer itself, over a protocol that never changes.**

### Why a separate protocol is the crux

`ZELLIJ_SOCK_DIR` is scoped by the contract version, and the client-server protocol evolves. The
handover protocol does not have to. A length-prefixed JSON manifest plus PTY master descriptors
over `SCM_RIGHTS` is a tiny, stable surface. Freeze it once (`HANDOVER_PROTOCOL_VERSION`) and a
binary from any future version can talk to a server from any past one, long after the main IPC
contract has moved on. That is precisely the gap that strands sessions today.

### Flow

1. **The old server notices it was replaced.** It already records its pid; it can equally watch
   its own `/proc/self/exe` for the ` (deleted)` marker (cheap, poll it on the same timer the
   auto-freeze supervisor already runs on). No CLI, no signal, no cooperation from anyone.
2. **It offers itself.** It binds `<version-independent dir>/<session>.handover`, writes a small
   advertisement next to it (session name, old version, protocol version, pid), and keeps serving
   its clients exactly as before. Nothing has changed for the user yet; this is an offer, not a
   commitment.
3. **A new binary looks for offers.** On `attach <name>` that finds nothing in its own contract
   directory - today's silent "create a fresh empty session" - it first checks for an
   advertisement. On `upgrade-server --all` it checks unconditionally.
4. **Freeze, then dump.** When a taker connects, the old server freezes every pane's cgroup
   (Phase 2). Now nothing can write to a pty while we work, so the scrollback dump and the
   pane-to-descriptor correlation describe one consistent instant rather than a moving target.
   This is the piece that makes a *separate-process* handover as trustworthy as `execve`.
5. **Hand over.** Manifest and descriptors go across the socket. The new server adopts them with
   the primitives from §11 (`adopt_terminal`), resurrects the layout, thaws the cgroups, and
   acknowledges.
6. **The old server steps aside** only after the acknowledgement, unlinking its socket. Before the
   ack it can still abort and simply thaw - which is the rollback the `execve` route cannot offer.

### What this buys over `execve`

* It crosses contract versions, because the handover protocol is not the contract.
* It is rollback-safe: nothing is irreversible until the ack.
* It needs no signal, so it is safe to attempt against any server that advertises - and a server
  that does not advertise is simply one that predates this, and is left alone.

### What it costs

* The adopted children are not the new server's children, so their exit status is unobtainable
  (§6). `RestartPolicy::OnFailure` degrades to "restart on any exit" for adopted panes.
* The pid changes, so a `systemd` unit tracking `MainPID` needs `sd_notify MAINPID=` or a restart.
* Two processes exist briefly, so the freeze in step 4 is not optional - without it the dump and
  the descriptors could disagree.

### The honest limit

This only helps from the version that ships it onward: a server has to already know how to offer
itself. Sessions stranded by *earlier* versions stay stranded, and can only be picked up by the
binary that started them. There is no way around that, and pretending otherwise would mean
signalling processes that would die of it.
