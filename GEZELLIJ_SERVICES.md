# Gezellij Services 🇳🇱✨

> Background processes that behave like services, without a container in sight.

A **Gezellij service** is a named command that Gezellij supervises for you. Under the hood it is
nothing exotic: a detached Zellij session called `svc-<name>`, with a single command pane running
your program. It runs directly under your own user account — your files, your permissions, your
`$HOME`. No image to build, no UID mapping, no daemon socket.

The definition itself is a small JSON file:

```
<zellij config dir>/services/<name>.json
```

which looks like this:

```json
{
  "name": "api",
  "command": ["./start-api.sh", "--port", "8080"],
  "cwd": "/srv/api",
  "restart": "on-failure",
  "created_at": 1758000000
}
```

`zellij service add` always records a `cwd` (the `--cwd` you gave, or the directory you ran it from); a hand-written definition may leave it out. The file is written atomically (write-then-rename), so a
crash never leaves half a definition behind, and you are welcome to edit it by hand.

The binary is still called `zellij` for now — the rename to `gezellij` is Phase 5 of
[the plan](GEZELLIJ_PLAN.md).

---

## Quick start

```bash
# define and start a service (--restart on-failure is the default; shown here for clarity)
zellij service add --name api --restart on-failure -- ./start-api.sh

# what have I got, and is it running?
zellij service list

# what did it say?
zellij service logs api --tail 100
zellij service logs api -f          # keep streaming

# hop into it; detach with your normal Zellij detach keybinding (Ctrl-o d by default)
zellij service attach api           # the service keeps running after you detach

# lifecycle
zellij service stop api             # kills the svc-api session, keeps the definition
zellij service start api            # idempotent: does nothing if already running
zellij service remove api           # stop + delete the definition
```

Handy extras on `add`:

| Flag | What it does |
|---|---|
| `--no-start` | Only write the definition; don't start it yet |
| `--cwd <DIR>` | Working directory for the command (default: the directory you ran `add` from) |
| `--force` / `-f` | Overwrite an existing definition with the same name |
| `--name` / `-n`, `--restart` / `-r` | Short forms |

Everything after `--` is the command and its arguments, taken literally — no shell involved. If you
want shell features, ask for a shell: `-- bash -c 'foo | bar'`.

Aliases, because typing is work: `zellij svc` for `service`, `ls` for `list`, `rm` for `remove`,
`a` for `attach`, `-t` for `--tail`. `zellij service list --no-formatting` drops the colours and
alignment so you can pipe it somewhere.

Service names are also file names and part of a session name, so keep them boring: letters, digits,
`-`, `_` and `.`, up to 64 characters, not starting with `.` or `-`.

---

## Restart policies

Three policies, set with `--restart`:

* **`no`** — never restart. The classic Zellij behaviour: the pane is held after exit so you can
  read the last screen.
* **`on-failure`** *(default)* — restart when the command exits non-zero **or** is killed by a
  signal (an unknown exit status counts as failure).
* **`always`** — restart whenever the command exits, clean or not.

### Backoff

Gezellij does not hammer a broken program. After each exit it waits before re-running, doubling the
wait each consecutive time, capped at 30 seconds:

```
1s → 2s → 4s → 8s → 16s → 30s → 30s → …
```

The counter resets when a run **lasted at least 60 seconds**. So a service that crashes instantly
settles into one retry every 30 seconds, while a service that runs happily for a few minutes and
then dies gets an immediate 1-second restart again.

### Held panes, ENTER and Ctrl-c

Between runs the pane is *held*, exactly like a normal Zellij command pane that exited: it stays on
screen showing the exit status, waiting. This is not a special state — it means the usual keys work:

* **ENTER** on a held pane re-runs the command immediately. The pending restart timer notices that
  the run it was waiting for has been superseded and quietly stands down, so you never get a double
  start.
* **Ctrl-c** on a held pane closes it, which also ends supervision for that pane. In a
  single-pane service session, closing the pane ends the session — the same thing
  `zellij service stop` does.

One interaction worth knowing: if a pane has `close_on_exit` set and the policy is `on-failure`, a
clean exit 0 closes the pane instead of holding it (there is nothing to restart). Under `always`
the pane is always held and always comes back.

---

## The same feature outside services

Supervision is a property of a *command pane*, not of the service registry. You can use it anywhere
a command pane is defined.

### In KDL layouts

```kdl
layout {
    pane command="./worker" restart="always"

    pane command="./flaky-importer" {
        restart "on-failure"
    }

    floating_panes {
        pane command="./tail-logs" restart="always"
    }
}
```

Both the property form (`restart="always"`) and the child-node form (`restart "on-failure"`) work,
on panes, on floating panes, and on pane templates — a template can carry the policy and the panes
that consume it inherit it. Accepted values are `no`, `on-failure` and `always`; the layout parser
is forgiving and also takes the strings `"never"`, `"on_failure"`/`"onfailure"`, `"true"`/`"false"`
(they must be quoted strings — a bare KDL boolean is rejected).

The policy is persisted in session-resurrection layouts: when Gezellij serialises a live session it
writes the child-node form (`restart "on-failure"`) into the dumped layout, and omits it entirely
when the policy is `no`. A resurrected session therefore keeps supervising.

### One-off supervised background sessions

No definition file, no registry — just a detached session with a supervised command:

```bash
zellij attach --create-background nightly-sync --restart always -- ./sync.sh
```

`--restart` here applies to the initial command and requires one, and takes the same three values
(CLI parsing is strict: `no`, `on-failure`, `always`).

---

## systemd integration

Services start on demand. To have one come up at login, export it as a `systemd --user` unit:

```bash
zellij service export-systemd api             # print the unit to stdout
zellij service export-systemd api --install   # write ~/.config/systemd/user/gezellij-api.service
systemctl --user daemon-reload
systemctl --user enable --now gezellij-api
```

The generated unit is deliberately simple:

```ini
[Unit]
Description=Gezellij service: api

[Service]
Type=oneshot
RemainAfterExit=yes
WorkingDirectory=/srv/api
ExecStart=/usr/bin/zellij service start api
ExecStop=/usr/bin/zellij service stop api

[Install]
WantedBy=default.target
```

**Be aware of the honest limitation.** The Zellij server double-forks away from whatever started
it, so systemd has no process to track. That is why the unit is `Type=oneshot` with
`RemainAfterExit=yes`: systemd records "the start command succeeded, consider this active" and
leaves it at that. Consequences:

* systemd starts and stops the service correctly (`ExecStop` calls `zellij service stop`).
* systemd does **not** notice if the session dies on its own — `systemctl --user status` will still
  say active. Use `zellij service list` for the truth. Restarts *within* the session are Gezellij's
  job anyway, via the restart policy.
* systemd's own `Restart=` directives will not help here; don't reach for them.

If the service must keep running while you are logged out, enable lingering once:

```bash
loginctl enable-linger $USER
```

Paths and environment are quoted properly in the unit, so spaces in your binary path or working
directory are fine.

---

## How it works

Worth knowing if you go reading the source:

1. `RunCommand.restart` (`zellij-utils/src/input/command.rs`) carries a `RestartPolicy`. It is
   skipped during serialisation when it is `no`, so nothing changes for ordinary panes.
2. The policy travels to the server as the optional `restart` field on `RunCommandAction` in the
   protobuf contract (`zellij-utils/src/client_server_contract/common_types.proto`).
3. `zellij-server/src/pty.rs` builds a `command_exit_callback` per command pane. When the process
   exits it asks the policy `should_restart(exit_status)`; if yes, it holds the pane and arms a
   timer using the backoff above.
4. When the timer fires it sends `ScreenInstruction::RerunCommandPane` — **the very same path your
   ENTER key takes**. Each (re)start bumps a generation counter, which is how a superseded timer
   knows to do nothing, and closing a pane drops its supervision state entirely.
5. The registry and the systemd unit renderer live in `zellij-utils/src/host_fabric/`
   (`services.rs`, `systemd.rs`).

---

## Limitations / roadmap

- **`logs -f` streams pane snapshots, not a byte stream.** It rides Zellij's `subscribe`
  mechanism, which re-sends the visible pane (plus the requested scrollback) whenever it changes.
  Fine for watching a service, noisy for piping into other tools; a true append-only stream is on
  the list.
- **`logs` shows the current run.** A supervised pane is re-run through Zellij's normal
  "re-run held command" path, which resets the pane's screen and scrollback first. So after a
  crash loop you see the output of the run that is going on now, not of the runs that crashed.
  Keeping the previous run's output (or teeing service output to a file) is on the list.

Phase 1 of [GEZELLIJ_PLAN.md](GEZELLIJ_PLAN.md) is deliberately small. Today:

* **One pane per service.** A service is one supervised command, not a group. Multi-pane service
  layouts are not modelled yet.
* **No cgroup freeze yet.** `freeze` / `thaw` (cgroups v2, 0% CPU hibernation) is Phase 2.
* **No live upgrade yet.** Upgrading the binary still restarts the server and takes running
  processes with it. SCM_RIGHTS PTY handover is Phase 3.
* **Logs are the pane scrollback.** `zellij service logs` reads what the pane holds, so history is
  bounded by your `scroll_buffer_size` setting. If you need durable logs, have the service write a
  file (or pipe through `tee`) as you would anywhere else.
* **systemd only notices what it started**, as described above.
* **No socket activation or ULA loopback binding yet** — Phase 4.

Bug reports and rough edges are welcome. Cozy software gets cozier with use.
