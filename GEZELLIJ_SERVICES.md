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

To have a service come up at login, export it as a `systemd --user` unit:

```bash
zellij service export-systemd api            # print the unit
zellij service export-systemd api --install  # write ~/.config/systemd/user/gezellij-api.service
systemctl --user daemon-reload
systemctl --user enable --now gezellij-api
```

The generated unit looks like this:

```ini
[Unit]
Description=Gezellij service: api

[Service]
Type=simple
Restart=on-failure
RestartSec=2
WorkingDirectory=/srv/api
ExecStart=/usr/bin/zellij service run api
ExecStop=/usr/bin/zellij service stop api

[Install]
WantedBy=default.target
```

`zellij service run <name>` is the foreground twin of `start`: it launches the session's server as
a *child* of itself (with the hidden `--server-foreground` flag, so the server does not daemonize),
performs the same first-client handshake `attach --create-background` does, and then simply waits
for the server to exit. That gives systemd a real main process to track, so `Restart=on-failure`
kicks in if the session dies unexpectedly, and `systemctl stop` first asks the session to shut down
cleanly via `ExecStop`. If `ZELLIJ_SOCKET_DIR` or `--config-dir` were in effect when you exported,
they are pinned with `Environment=` lines so the unit sees the same sessions your shell does.

Two things worth knowing:

- Services that must keep running while you are logged out need lingering enabled once:
  `loginctl enable-linger $USER`.
- `run` refuses to take over a service that is already running (started by hand with `start`);
  stop it first if systemd should own it.

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
- **`logs` keeps history across supervised restarts.** When a pane with a restart policy is
  re-run, its scrollback is kept and a dim `── command exited (exit status 1), restarting ──`
  separator marks the next run, so a crash loop stays diagnosable. Panes without a policy (and
  panes left in the alternate screen) get Zellij's classic full reset. History is still bounded
  by `scroll_buffer_size`.

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

---

## Networking

### Addresses instead of ports

`127.0.0.1` has one flat port namespace, so every service that wants to be "the web thing" has to
be talked out of `:8080` and into some arbitrary number nobody remembers. IPv6 makes that
unnecessary: one `/64` out of the Unique Local Address range routed *locally* on `lo` gives you
2^64 loopback addresses, and every service can bind **the same well-known port** on an address of
its own:

```
api   -> http://[fdxx:xxxx:xxxx::9f2c:....]:8080
blog  -> http://[fdxx:xxxx:xxxx::41d8:....]:8080
```

No port registry, no collisions, no `--port` flags to keep straight.

### The prefix (generated per installation)

RFC 4193 asks for a *pseudo-random* 40-bit global ID so two hosts that later get bridged do not
collide. Gezellij therefore generates your prefix once, on first use, and stores it in
`<config dir>/network.json`:

```json
{ "prefix": "fdxx:xxxx:xxxx::/64" }
```

It is never hardcoded and never rotated behind your back (a corrupt `network.json` is an error,
not a reason to mint a new prefix — rotating it would invalidate the route you installed as root).

Each service's address is the prefix plus a 64-bit interface id derived (SHA-256) from the
service's opaque `id`, not from its name: renaming a service does not move it. Definitions written
before this feature have no `id` and fall back to a stable hash of `name:<name>`.

### One-time root setup

```console
$ zellij service net-setup
Prefix:  fdxx:xxxx:xxxx::/64
Stored:  ~/.config/zellij/network.json
Port:    8080 (the same for every service)
Routed:  no - run the command below once, as root

    sudo ip -6 route add local fdxx:xxxx:xxxx::/64 dev lo
```

A `local` route makes the kernel treat *every* address in the /64 as one of its own, so services
can `bind()` them without an `ip addr add` per service. Check it with:

```console
$ ip -6 route show table local | grep fdxx:
```

`net-setup` prints a ready-made `/etc/systemd/system/gezellij-ula.service` oneshot unit
(`ExecStart=/usr/bin/ip -6 route add local <prefix> dev lo`, `RemainAfterExit=yes`) to make the
route survive a reboot; it works no matter which daemon manages `lo`. systemd-networkd users can
put the same route in their `lo` `.network` file instead.

Whether the prefix is routed is checked by *actually binding* a UDP socket on `<prefix>::1` — the
same thing your service will attempt — rather than by parsing routing tables.

### `--bind-ip`

```console
$ zellij service add --name api --bind-ip -- ./serve
Saved service 'api' -> ~/.config/zellij/services/api.json
  own address: [fdxx:xxxx:xxxx::9f2c:....]:8080 (GEZELLIJ_BIND_URL=http://[fdxx:...]:8080)
Started service 'api' in background session svc-api (restart: on-failure)
  listening on http://[fdxx:xxxx:xxxx::9f2c:....]:8080
```

The service's command gets three environment variables:

| Variable | Example |
|---|---|
| `GEZELLIJ_BIND_ADDR` | `fdxx:xxxx:xxxx::9f2c:....` |
| `GEZELLIJ_BIND_PORT` | `8080` |
| `GEZELLIJ_BIND_URL`  | `http://[fdxx:xxxx:xxxx::9f2c:....]:8080` |

Use them as your listen address (`app.listen(process.env.GEZELLIJ_BIND_ADDR, ...)`,
`--bind "[$GEZELLIJ_BIND_ADDR]:$GEZELLIJ_BIND_PORT"`, …). They are exported into the environment
of the process that starts the service; the session server is spawned as its child and the service
pane inherits from the server, so the variables arrive without touching the stored command. A
consequence: changing `--bind-ip` on an existing service (`add --force`) takes effect on the next
`stop` + `start`, not on the running pane.

If the prefix is not routed yet, the service still starts — you just get a warning with the setup
command, because the failure otherwise shows up as an opaque `EADDRNOTAVAIL` inside the service.

`zellij service list` shows the address in an `ADDRESS` column (`-` for services without one).

### Exporting to a reverse proxy

```console
$ zellij service net-export --format caddy
api.localhost {
	reverse_proxy [fdxx:xxxx:xxxx::9f2c:....]:8080
}

$ zellij service net-export api --format hosts
fdxx:xxxx:xxxx::9f2c:....	api.localhost
```

With no name, every `--bind-ip` service is exported. Pipe the Caddy output into a file that your
`Caddyfile` `import`s and each service keeps a stable name *and* a stable address, while the
service itself only ever knows about port 8080.
