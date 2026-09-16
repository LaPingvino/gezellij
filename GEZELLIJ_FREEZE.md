# Gezellij Freeze & Thaw ❄️

> Put a whole session to sleep in the kernel. No signals, no lost state, no CPU. Wake it in a
> millisecond.

```bash
zellij freeze            # freeze the current / only active session
zellij freeze mywork     # freeze a named session
zellij thaw mywork       # and back
zellij freeze mywork --status
zellij freeze mywork --pane-id terminal_3   # just one pane

zellij service freeze api                   # the same, aimed at a service
zellij service thaw api
```

## What it does

Every terminal pane the Gezellij server spawns is placed in its own **cgroup v2** subgroup,
`<server cgroup>/gezellij-<session>/pane-<id>`. The child process joins it *before* `exec`, so
everything the command starts later (your build, the language server it spawned, the browser your
test runner opened) inherits the membership.

`zellij freeze` writes `1` to each pane's `cgroup.freeze`. The kernel stops scheduling every task in
those cgroups. Nothing is signalled, memory stays exactly as it was, the processes do not know it
happened. `zellij thaw` writes `0` and they carry on mid-instruction. This is the same mechanism
container runtimes use for `docker pause`, minus the container.

Typical uses:

- a memory-heavy dev stack (IDE, database, three watchers) you want out of the way for an hour
  without tearing it down and warming it up again;
- a runaway process you want to look at calmly before deciding what to do;
- laptop battery: freeze the noisy session, keep the notes session.

The session itself (the Zellij server, the plugins, your ability to attach and look around) keeps
running; only the pane processes are frozen. A frozen pane simply stops producing output and stops
reacting to keys until thawed.

## Requirements and how it degrades

- Linux with the unified cgroup hierarchy (cgroup v2), which is the default on every current
  distribution.
- Your processes must be allowed to create sub-cgroups where the server lives. Under
  `systemd --user` sessions this is always the case: the whole `user@<uid>.service` subtree is
  delegated to you, and `cgroup.freeze` is part of the core interface, so no controller needs
  enabling and no root is involved. Gezellij checked this on Arch: creating a cgroup, moving a
  process, freezing and thawing all work as a plain user.
- Where that delegation is missing (some containers, non-systemd inits, a server started as a
  system service without `Delegate=yes`) the server logs one warning and runs panes exactly as
  upstream Zellij does. `zellij freeze` then tells you the session has no pane cgroups.

## How the CLI finds the cgroups

The server records its per-session cgroup root next to the session's socket
(`<socket dir>/<session>.cgroup-root`). The `freeze`/`thaw`
commands read that file and talk to `/sys/fs/cgroup` directly. That is deliberate: it works even
when the server is busy or wedged, and it needs no protocol change.

## Services

`zellij service freeze <name>` and `zellij service thaw <name>` are sugar for the session commands
above, aimed at the service's `svc-<name>` session. They refuse a service that is not running, and
otherwise freeze or thaw every pane of it — the same `cgroup.freeze` writes.

`zellij service list` reflects it in the `STATUS` column: a running service whose panes are *all*
frozen shows `frozen`, one where only some are shows `partly frozen` (both in cyan, next to green
`running` and red `stopped`). `--no-formatting` prints the same words without colour.

## Auto-freeze of idle sessions

Off by default. Put a humantime duration in your `config.kdl`:

```kdl
auto_freeze_after "10m"
```

and every session started by a server with that configuration freezes itself once **no client has
been attached to it for that long**, and thaws again **the moment a client attaches**. The thaw is
immediate (it happens on the attach path itself, not at the next poll), so attaching feels the
same as always.

The semantics, precisely:

- a supervisor thread wakes every 15 seconds; it only exists when the option is set *and* the
  session actually has pane cgroups. Without cgroup v2 delegation the server logs one warning at
  startup and the option does nothing;
- the idle clock starts when the last client disconnects (detach, `zellij kill-client`, a crashed
  terminal - every removal path goes through the same place in the server) and is re-derived from
  the live client list on every tick, so it cannot get stuck;
- the freezer state is always read back from `/sys/fs/cgroup`, never remembered, and auto-freeze
  only ever undoes *its own* freeze. So: if auto-freeze froze the session and you then run
  `zellij thaw <session>` without attaching, it stays thawed until a client has attached and left
  again; and a session you froze by hand with `zellij freeze` while attached stays frozen - the
  supervisor will not thaw it behind your back. Attaching, on the other hand, always thaws: that
  is an explicit "I want to look at this now";
- a session with no pane cgroups at all (no terminal panes, or no delegation) is never touched,
  and every sysfs error is logged at warn level and otherwise ignored - auto-freeze can never take
  the session down;
- it is a configuration-file option only: there is no CLI flag and it is not sent over the client
  protocol, so it is the *server's* configuration that decides.

`auto_freeze_after` applies to the whole server's session, services included.

### The honest caveat

A frozen session does nothing. That is the entire point for a development stack you left lying
around, and exactly the wrong thing for a service you want to keep serving traffic: a
`zellij service` whose session auto-freezes stops answering requests as soon as you detach, until
someone attaches again. There is **no per-service opt-out yet** - the option is global to the
server. So set it in a configuration used by development sessions, not in one used by the servers
that run your services, until per-service control exists.

## Housekeeping

- A pane's cgroup is removed when its process tree has exited (after the pane's exit callback
  runs); if descendants linger it is left in place and swept when the session ends.
- `thaw` is applied before removal, so a frozen pane can always be cleaned up.
- Plugin panes run inside the server process and are not part of this; only terminal panes have
  cgroups.

## Roadmap

- A frozen indicator in the pane frame and in the session manager. The session-manager plugin is
  a WASM guest and cannot read `/sys/fs/cgroup` itself, so this needs the server to poll
  `cgroup.events` and carry the state on the session info it already sends.
- Per-service (and per-session) opt-out for auto-freeze, so a machine can auto-freeze its
  development sessions while its services keep serving.
- Memory/CPU limits per pane through the same cgroups (`memory.max`, `cpu.max`) once a
  controller is delegated.
