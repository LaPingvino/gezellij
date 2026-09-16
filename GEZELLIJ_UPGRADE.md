# Upgrading Gezellij without losing your work 🔄

> The original itch: `pacman -Syu` replaces the binary, and from that moment your running
> multiplexer and your newly installed client are two different programs. Restarting the server
> means `SIGHUP` to every build, editor and server you had running in it. Gezellij fixes that.

```bash
zellij upgrade-server            # the current / only session
zellij upgrade-server mywork     # a named one
zellij upgrade-server --all      # every session of yours that is running an old binary
```

Nothing in your panes dies. The compile keeps compiling, the editor keeps its undo history, the
dev server keeps its socket. Even the process ids stay the same.

## What actually happens

The server does not restart: it *becomes* the new binary.

1. `zellij upgrade-server` looks up the session's server pid (recorded next to its socket) and
   checks `/proc/<pid>/exe`. Linux appends ` (deleted)` there once the file has been replaced, so
   "is this server running a stale binary" is a question with an exact answer. Without a
   replacement it does nothing and says so; `--force` overrides that.
2. It sends `SIGUSR2`. The server snapshots its layout exactly like the normal session-saving
   path, writes `session-layout.kdl`, and records which pseudo-terminal belongs to which pane of
   that layout in `<socket dir>/handover/<session>.exec.json`.
3. It marks every open file descriptor close-on-exec *except* the PTY masters, and calls
   `execve()` on the new binary with `--adopt <that manifest>`.

   This is the crux. `execve` replaces the program inside the same process: same pid, same open
   PTY masters, and the shells and commands in your panes remain children of that same process,
   so they never even receive a signal. From their point of view nothing happened at all.
4. The new program rebuilds the session from the layout file. For every pane in it, instead of
   spawning a fresh shell, it adopts the pseudo-terminal it inherited and starts watching the
   existing child again. Restart policies, working directories and pane titles come along.

If the `execve` fails (missing binary, wrong architecture), everything is put back and the old
server simply keeps running. You lose nothing by trying.

## What survives, and what does not

| | |
|---|---|
| Processes in panes | **survive**, same pids, never signalled |
| Layout, tabs, pane titles, working directories | **survive** |
| Service restart policies and their supervision | **survive** |
| Server pid (so `systemd` keeps tracking a `service run`) | **unchanged** |
| Scrollback | **lost** — panes come back with a clean screen |
| Attached clients | **cleanly detached**, told what happened and how to re-attach |
| Plugins (status bar, tab bar, …) | restarted, they are stateless WASM |

Scrollback is deliberate: the plan values keeping processes alive far above keeping pixels. It is
also the one thing that would have to cross a version boundary in a format both binaries agree
on, which is exactly the kind of coupling that makes upgrades fragile in the first place.

## When you forget

You do not have to remember any of this before updating your system. Everything above works
equally well an hour later, and Gezellij will remind you:

- Attaching to a session whose server runs a replaced binary prints a one-line note with the
  exact command to fix it.
- `zellij upgrade-server --all` sorts your sessions into "already up to date", "cannot be
  upgraded" and "upgrading now", and upgrades the last group.
- The Arch package ships a `pacman` hook that prints the same reminder after the package is
  upgraded (see `packaging/README.md`), and an opt-in `systemd --user` path unit that runs the
  upgrade for you the moment `/usr/bin/gezellij` changes.

## The fallback, if an upgrade ever goes wrong

Because step 2 writes the resurrection layout *before* anything risky happens, the worst case is
a session that has to be rebuilt rather than migrated:

```bash
zellij attach mywork     # resurrects the layout; commands are re-run, processes are new
```

That is the same safety net Zellij has always had for a killed server, and the upgrade path
leaves it strictly better armed than it found it.

## Freezing is not a prerequisite

`zellij freeze` (see `GEZELLIJ_FREEZE.md`) and the upgrade are independent. You do not need to
freeze anything before upgrading. Freezing a session and then upgrading works too: the frozen
cgroups belong to the panes, not to the server, and the successor re-reads them.

## Limits worth knowing

- Linux only. It leans on `/proc`, `execve` semantics and the process staying the parent of its
  pane children.
- Attached clients are detached rather than reconnected for you. They exit cleanly with a message
  naming the session and the command to come back; reconnecting them automatically would mean the
  client surviving the socket being re-bound by a different binary, which is exactly the coupling
  this design avoids.
- A pane that was *held* (a command that exited, waiting for ENTER) has no pseudo-terminal to
  adopt, so it comes back held, which is exactly where it was.
- An upgrade across a change in the client-server contract version moves the session's socket into
  a different directory; old clients cannot find it and must be replaced by the new binary, which
  is the point of upgrading in the first place.
- The rollback-safe alternative (handing the descriptors to a *separate* new process over a Unix
  socket, so a failed adoption can be undone) is implemented as a transport in
  `zellij-utils/src/host_fabric/handover.rs` and described in `HANDOVER_DESIGN.md`. The exec route
  shipped first because it keeps the pid, and with it systemd supervision and waitable children.
