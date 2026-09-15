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
- [ ] Add `gezellij service` CLI subcommand schema in `zellij-utils/src/cli.rs`.
- [ ] Implement headless pane execution (spawning without requiring an active GUI/TUI client attached).
- [ ] Implement basic process supervision / auto-restart logic (`restart = "always" | "on-failure" | "no"`).
- [ ] Implement `systemd --user` unit generation helper (`gezellij service export-systemd <name>`).

### Phase 2: Cgroups v2 Process Freezing
- [ ] Add cgroups v2 detection and freezer controller interface in `zellij-utils`.
- [ ] Implement `gezellij freeze <session/pane>` (write `1` to `cgroup.freeze`).
- [ ] Implement `gezellij thaw <session/pane>` (write `0` to `cgroup.freeze`).
- [ ] Display frozen/active status indicators in the UI status bar / tab bar.

### Phase 3: SCM_RIGHTS PTY Handover & Live Server Upgrades
- [ ] Implement Unix domain socket file descriptor passing (`sendmsg` with `SCM_RIGHTS`) using `nix::sys::socket`.
- [ ] Define serialization protocol for open master PTY descriptors + terminal cursor/scrollback state.
- [ ] Add server handover listener in `zellij-server`: allows a newly started server binary to claim active PTYs from a dying server.
- [ ] Implement `gezellij upgrade-server` / `--replace-server` CLI action.

### Phase 4: Network & Loopback IPv6 Helpers
- [ ] Add `--bind-ip <ipv6>` flag to service / pane runners.
- [ ] Helper utilities for managing loopback ULA IPs on Linux (`ip -6 addr add ... dev lo`).
- [ ] Provide Caddy / reverse-proxy configuration snippet export.

### Phase 5: Branding & UX Polish
- [ ] Rebrand internal strings, defaults, and cache folders to `gezellij` with fallback migration from `zellij`.
- [ ] Custom "Gezellig" themes (warm, cozy color palettes for long terminal sessions).
- [ ] Comprehensive documentation and demo screencasts.

---

## 5. Agent Handoff Checklist

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
