# Gezellij: Architectural Plan & Roadmap 🇳🇱✨

> **Gezellij** (*Zellij* + *gezellig* — Dutch for cozy, companionable, and warm)  
> A resilient terminal workspace and lightweight host-native process fabric.

---

## 1. Executive Summary & Problem Statement

**Zellij** is one of the best terminal multiplexers ever built, featuring brilliant pane layouts, intuitive keybindings, and WebAssembly plugin support. However, it suffers from two major pain points for day-to-day power users and server operators:

1. **The Upgrade Fragility Problem:**  
   When Zellij updates (via `pacman -Syu`, `cargo install`, or package managers), the active `zellij-server` instance runs an older contract/binary than the incoming `zellij attach` client. Sessions cannot be attached across versions, and restarting the server sends `SIGHUP` to all child processes, terminating active builds, editors, long-running scripts, and servers. Zellij's "session resurrection" merely recreates layout panes with fresh shells—it does **not** preserve running processes or their memory.

2. **The Missing "Host-Native Service" Abstraction:**  
   Developers often run background APIs, bots, and workers in multiplexer panes. Traditional solutions either require heavy OCI containers (Docker/Podman with volume permission nightmares, UID mapping issues, and daemon socket risks) or raw terminal multiplexers with zero lifecycle management (no restart policies, no cgroup freezing, no systemd integration).

**Gezellij** bridges this gap: it retains Zellij's beloved UI/UX while building a **host-native process fabric** featuring **live server upgrades with zero process termination**, **cgroup v2 hibernation**, and **systemd service integration**.

---

## 2. Core Pillars & Specifications

```text
               ┌────────────────────────────────────────────────────────┐
               │                        GEZELLIJ                        │
               │                                                        │
               │  [ Interactive Panes ]       [ Background Services ]   │
               │   (Shell, Vim, Logs)          (APIs, Daemons, Bots)    │
               └──────────────┬────────────────────────────┬────────────┘
                              │                            │
                     SCM_RIGHTS PTY Passing         cgroups v2 Freezer
                  (Survives binary upgrades)      (Instant 0% CPU Sleep)
                              │                            │
               ┌──────────────▼────────────────────────────▼────────────┐
               │                   Host-Native Fabric                   │
               │     • ULA IPv6 Loopback binding (fd00:2830::/64)       │
               │     • systemd --user service generation                │
               │     • Zero-UID mismatch POSIX permissions              │
               └────────────────────────────────────────────────────────┘
```

### Pillar 1: SCM_RIGHTS PTY Handover (Live Server Upgrades)
* **Objective:** Allow `gezellij` binaries to be upgraded on the host without killing running terminal sessions.
* **Mechanism:**
  * In Unix, a process running inside a pseudoterminal (PTY) is bound to the master file descriptor. As long as the master FD stays open and connected to an event loop, the child process never dies.
  * When `gezellij upgrade` or a server replacement is triggered:
    1. Old server serializes its pane metadata, tab tree, and scrollback state.
    2. Old server opens a Unix domain socket to the newly spawned server binary.
    3. Old server passes master PTY file descriptors across the socket using ancillary data (`sendmsg` with `SCM_RIGHTS`).
    4. New server imports the inherited FDs into its Tokio PTY event loop.
    5. Old server drains output and exits cleanly.
  * **Result:** Editors, long-running compilation tasks, and REPLs continue running uninterrupted.

### Pillar 2: Host-Native Service Management (`gezellij service`)
* **Objective:** Run persistent background processes that behave like modern services without the bloat of Docker.
* **CLI Interface:**
  ```bash
  gezellij service add --name api --restart on-failure -- ./start-api.sh
  gezellij service list
  gezellij service attach <name>
  gezellij service logs <name> [--tail 100] [-f]
  ```
* **Features:**
  * **Direct POSIX Permissions:** Runs directly under the host user account (`joop`)—zero file permission issues, no root volume locking, and no `userns` mapping bugs.
  * **Systemd User Integration:** Optional `--systemd` flag automatically emits a `~/.config/systemd/user/gezellij-<name>.service` unit so headless services automatically boot with the system.
  * **Socket Activation:** Optionally inherit listening sockets from systemd.

### Pillar 3: Cgroups v2 Freezing & Hibernation (`gezellij freeze` / `gezellij thaw`)
* **Objective:** Put idle development environments, large IDEs, or memory-heavy stacks to sleep with 0% CPU usage.
* **Mechanism:**
  * Uses Linux cgroups v2 (`cgroup.freeze`).
  * Instantaneous in-kernel pause: no signals sent to user processes, no memory unloaded, zero CPU cycles consumed.
  * Wakeup is instant and transparent to the child processes.

### Pillar 4: Dedicated Loopback IPv6 (ULA) Orchestration
* **Objective:** Seamless microservice networking on `lo` (`fd00:2830::/64`).
* **Mechanism:**
  * Allows assigning dedicated IPv6 loopback addresses per service/pane (e.g. `[fd00:2830::8008]:8080`).
  * Completely prevents port collisions on `127.0.0.1`.
  * Integrates directly with host reverse proxies like Caddy.

---

## 3. Codebase Touchpoints & Architecture Map

| Subsystem | Existing Zellij Files | Gezellij Enhancements |
|---|---|---|
| **CLI & Commands** | `zellij-utils/src/cli.rs`<br>`src/commands.rs` | Add `Command::Service`, `Command::Freeze`, `Command::Thaw`, and upgrade flags |
| **PTY Management** | `zellij-server/src/pty.rs`<br>`zellij-server/src/os_input_output.rs` | Add FD inheritance, `SCM_RIGHTS` serialization, and `ReAttachTerminal` instruction |
| **IPC & Protocol** | `zellij-utils/src/ipc.rs`<br>`zellij-utils/src/consts.rs` | Backward-compatible IPC version negotiation, server handover protocol |
| **Session State** | `zellij-utils/src/sessions.rs`<br>`zellij-server/src/session_layout_metadata.rs` | Full PTY state persistence across server handovers |
| **Linux Sandboxing** | *New crate / module* (`zellij-utils/src/host_fabric/`) | Cgroups v2 freezer bindings, systemd `--user` unit generators, ULA IP helpers |

---

## 4. Phased Implementation Roadmap

### Phase 1: Foundation & Service Mode Primitives
- [x] Add `gezellij service` CLI subcommand schema in `zellij-utils/src/cli.rs`. *(2026-09-16: `zellij service add|start|stop|remove|list|attach|logs|export-systemd`, glue in `src/service_commands.rs`, registry in `zellij-utils/src/host_fabric/services.rs`; see `GEZELLIJ_SERVICES.md`)*
- [x] Implement headless pane execution (spawning without requiring an active GUI/TUI client attached). *(verified: `attach --create-background` runs without a controlling terminal; CLI actions on such a session must address panes explicitly since nothing is focused)*
- [x] Implement basic process supervision / auto-restart logic (`restart = "always" | "on-failure" | "no"`). *(`RunCommand.restart` + `pty.rs::command_exit_callback`, exponential backoff, KDL `restart` property, `attach --restart`)*
- [x] Implement `systemd --user` unit generation helper (`gezellij service export-systemd <name>`). *(`zellij-utils/src/host_fabric/systemd.rs`; `Type=oneshot` for now, see §5.3)*
- [x] Server foreground mode (`--server-foreground`, `zellij service run <name>`): systemd units are now `Type=simple` + `Restart=on-failure` with a real main process.
- [x] Supervised panes keep scrollback across restarts (separator line), so `service logs` shows the crash history.
- [x] Session-manager plugin marks `svc-*` sessions with a SERVICE badge (both list styles).
- [x] Arch `PKGBUILD` (`packaging/arch`, installs `/usr/bin/gezellij` beside stock zellij) and reversible login takeover script (`packaging/login`).

### Phase 2: Cgroups v2 Process Freezing
- [x] Add cgroups v2 detection and freezer controller interface in `zellij-utils`. *(`host_fabric/cgroups.rs`; every terminal pane joins its own cgroup `<server cgroup>/gezellij-<session>/pane-<id>` in `pre_exec`; verified as a plain user under `systemd --user` delegation)*
- [x] Implement `gezellij freeze <session/pane>` (write `1` to `cgroup.freeze`). *(`zellij freeze [session] [--pane-id]`, `--status`; talks to sysfs directly via the root recorded next to the session socket)*
- [x] Implement `gezellij thaw <session/pane>` (write `0` to `cgroup.freeze`).
- [ ] Display frozen/active status indicators in the UI status bar / tab bar. *(next: server polls `cgroup.events`; also `service freeze|thaw` sugar and auto-freeze of idle sessions — see `GEZELLIJ_FREEZE.md`)*
- [x] Interim upgrade awareness: server records its pid beside the socket; `host_fabric/upgrade.rs` detects a replaced binary via `/proc/<pid>/exe … (deleted)` (surfacing in the CLI pending).

### Phase 3: SCM_RIGHTS PTY Handover & Live Server Upgrades
- [x] Implement Unix domain socket file descriptor passing (`sendmsg` with `SCM_RIGHTS`) using `nix::sys::socket`. *(`host_fabric/handover.rs`: chunked SCM_RIGHTS transfer, 13 tests incl. 300 fds; handover sockets live in `<sock dir>/handover/` so they never look like sessions)*
- [x] Define serialization protocol for open master PTY descriptors + terminal cursor/scrollback state. *(`HandoverManifest` v1, JSON, forward-tolerant; per the decision in §5.1 scrollback is best-effort. Full design and risk list in `HANDOVER_DESIGN.md` — read it before 3.3: pane↔fd correlation needs a side-car keyed by terminal id, adopted children are not waitable, and systemd `KillMode` must change for `svc-*` sessions)*
- [ ] Add server handover listener in `zellij-server`: allows a newly started server binary to claim active PTYs from a dying server.
- [ ] Implement `gezellij upgrade-server` / `--replace-server` CLI action.

### Phase 4: Network & Loopback IPv6 Helpers
- [x] Add `--bind-ip <ipv6>` flag to service / pane runners. *(`service add --bind-ip`: the address is derived per service from a per-installation random ULA prefix and exported as `GEZELLIJ_BIND_ADDR/_PORT/_URL`; same port everywhere)*
- [x] Helper utilities for managing loopback ULA IPs on Linux. *(`host_fabric/net.rs`; one-time root `ip -6 route add local <prefix> dev lo` printed by `service net-setup`, no per-address adds)*
- [x] Provide Caddy / reverse-proxy configuration snippet export. *(`service net-export [name] --format caddy|hosts`)*
- [ ] Pane-level `--bind-ip` for `zellij run` (services only so far).

### Phase 5: Branding & UX Polish
- [ ] Rebrand internal strings, defaults, and cache folders to `gezellij` with fallback migration from `zellij`. *(interim: the Arch package installs the binary as `/usr/bin/gezellij` beside stock zellij, sharing config and socket dirs)*
- [x] Custom "Gezellig" themes (warm, cozy color palettes for long terminal sessions). *(`gezellig-dark`, `gezellig-light`; see `GEZELLIJ_THEMES.md`)*
- [ ] Comprehensive documentation and demo screencasts.

---

## 5. Design Notes (added 2026-09-16, after implementing Phase 1)

Three kinds of notes, deliberately kept apart: **decisions** Joop made, **findings** verified
against the code while building Phase 1, and **open questions** that may just be a blind spot of
the agent writing this.

### 5.1 Decisions

* **The idea behind it:** Zellij is the foundation because it looks nice and is great to work with.
  The product is a host-native way to run and look after things on a server that is *lighter,
  safer and easier than Docker*. Judge every feature by that, not by multiplexer purity.
* **Live upgrades (Phase 3) are about keeping processes alive.** Terminal grid, scrollback and
  WASM plugin state are not significant. Layout travels through the existing
  `session-layout.kdl` resurrection file (which now also carries the restart policy), the new
  server adopts the live PTY FDs instead of spawning fresh processes, scrollback is best-effort,
  plugins simply restart. In short: *resurrection + FD adoption*.
* Anything that needs root (loopback routes, cgroup delegation) is a documented one-time setup
  step, never something the binary does on its own. Safer-than-Docker means no privileged daemon.
* **Networking = swap ports for addresses.** Every service listens on the same well-known port on
  its own IPv6 ULA loopback address; the address is the service's identity, which is also how a
  reverse proxy wants to think. Per RFC 4193 the 40-bit global ID of the `fd00::/8` prefix must be
  generated randomly *per installation* and stored in config (`fd00:2830::/64` in this document is
  Joop's own local prefix and must not become a default); per-service interface IDs are derived
  from a UUID / session id rather than from guessable names. One-time root setup
  `ip -6 route add local <prefix>/64 dev lo` makes the whole prefix bindable without per-address
  `ip addr add`, so nothing needs root at run time.

### 5.2 Verified findings

* A service really is "a detached session + a restart policy". `attach --create-background`
  already ran headless (the server copes without a controlling terminal); the pane hold/rerun
  machinery already existed. Supervision took one exit-callback helper in `pty.rs`
  (`command_exit_callback`) that arms a backoff timer and re-runs through the same
  `ScreenInstruction::RerunCommandPane` path the user's ENTER key uses, so manual and automatic
  restarts cannot double-fire. Backoff: 1s doubling to a 30s cap, reset after a 60s stable run.
* Adding a field to `RunCommand` touches ~40 struct literals and 36 insta snapshots. Hand-written
  `Debug` impls that omit the default `restart` keep every upstream snapshot byte-identical.
* The IPC contract needed a new optional proto field (`RunCommandAction.restart = 9`), regenerated
  with `cargo xtask proto`; old clients simply omit it.
* `ZELLIJ_SOCK_DIR` derives from `XDG_RUNTIME_DIR`, which `systemd --user` sets identically, so a
  unit-started service is visible to the interactive shell. The exported unit is
  `Type=oneshot` + `RemainAfterExit=yes` because the server double-forks; systemd therefore does
  not notice if the session dies on its own.
* Debug builds of the binary default to `plugins_from_target`, which needs the `wasm32-wasip1`
  target; on a machine without it build with
  `--no-default-features --features "vendored_curl,web_server_capability"`.

### 5.3 Open questions (possibly the agent's blind spots)

* **Server `--foreground` mode.** A small change that would let the unit be `Type=simple` with
  `Restart=on-failure`, so systemd genuinely supervises the session. Feels like Phase 1.5, but the
  double-fork may exist for reasons not yet understood.
* **cgroup delegation.** Writing `cgroup.freeze` only works in a cgroup the user controls; under
  `systemd --user` that likely means spawning the server through `systemd-run --user --scope` or a
  unit with `Delegate=yes`. Needs a look at how the server is spawned before Phase 2.
* **ULA loopback.** One root step (`ip -6 route add local fd00:2830::/64 dev lo`) should make the
  whole prefix bindable without per-address `ip addr add`. Untested here.
* **Layout-file compatibility across versions** is what "resurrection + FD adoption" leans on; it
  is already designed to be readable across versions, but Phase 3 should add a test that pins it.
* **Services in the UI.** The session-manager plugin could show `svc-*` sessions with a status
  badge; cheap polish that fits "nice to live in".

---

## 6. Agent Handoff Checklist

When picking up work on this repository:
1. **Toolchain:** Rust 1.90+ (`cargo`, `rustc`).
2. **Git Workflow:**
   * Branch from `main`.
   * Keep upstream synchronized: `git fetch upstream && git merge upstream/main`.
   * Push changes to `origin` (`git@github.com:LaPingvino/gezellij.git`).
3. **Building & Testing:**
   * Build: `cargo build`
   * Test: `cargo test`
   * Fast syntax/type check: `cargo check`
