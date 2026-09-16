# Gezellij end-to-end tests

`gezellij-e2e.sh` is a self-contained regression suite for the **Gezellij-specific** behaviour
on top of upstream Zellij: the service registry, restart supervision, cgroup v2 freeze/thaw,
the in-place server upgrade, the ULA loopback addresses and the `systemd --user` export.

It is a shell script rather than a `cargo test` on purpose. Every one of these features needs
things a Rust test harness cannot give it: a real pseudo-terminal per pane, unix sockets in a
socket directory, a server that daemonizes away from the test process, `/proc` (the upgrade
path keys on `/proc/<pid>/exe` ending in `(deleted)`), and cgroup v2 delegation.

## Running it

```bash
cargo build -p zellij --no-default-features --features "vendored_curl,web_server_capability"
tests/gezellij-e2e.sh
```

```
Usage: tests/gezellij-e2e.sh [--binary PATH] [--keep] [test-name ...]

  --binary PATH   binary to test (default: $GEZELLIJ_TEST_BINARY, else target/debug/zellij)
  --keep          leave the temporary root behind and print its path
  test-name ...   run only these tests (default: all)
```

A full run takes roughly a minute. It prints one line per assertion, `PASS`/`FAIL`/`SKIP` per
test, a summary line, and exits non-zero if anything failed (a skip is not a failure). Colour
is used only when stdout is a terminal.

## Requirements

- **Linux** with a real `/proc`. The upgrade tests read `/proc/<pid>/exe` and `/proc/<pid>/status`.
- **Not inside a sandbox.** A seccomp/bubblewrap sandbox that blocks daemonizing or unix socket
  connections makes `zellij service start` hang forever. Run the suite on the host.
- **cgroup v2 delegation** for `freeze_thaw`. Under a normal `systemd --user` session this is
  already the case; where it is missing the test skips instead of failing.
- `bash` (4+), `find`, `pgrep`, `timeout`, `grep -E`.
- About 1.5 GB of free space in `$TMPDIR` — two upgrade tests each keep a copy of the binary,
  and a debug build of `zellij` is ~500 MB.

Keep `$TMPDIR` short. Session sockets live at
`$ZELLIJ_SOCKET_DIR/contract_version_<N>/<session>` and unix socket paths are capped at 108
bytes; the script refuses to run if its socket directory is longer than 70 bytes and tells you
to set `TMPDIR=/tmp`.

`net_addresses` does not depend on the ULA prefix actually being routed: it only checks that
`net-setup` reports a `fd00::/8` `/64` and that the address is threaded through `add`, `list`,
the pane's environment and the caddy export. `net-setup` exits 0 whether or not the prefix is
routed, so the test is portable; the unrouted branch of `--bind-ip` (the "not routed yet"
warning) is not exercised.

## Hermetic by construction

Every run makes a fresh `mktemp -d` root and points `ZELLIJ_CONFIG_DIR`, `ZELLIJ_SOCKET_DIR`,
`XDG_CACHE_HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` inside it, so it sees none of your real
sessions, services or network prefix and creates none outside the root. Before doing anything
it asserts that `$ZELLIJ_SOCKET_DIR` really is inside that root and bails out otherwise.
`ZELLIJ`/`ZELLIJ_SESSION_NAME` are unset so an ambient session cannot be resolved by accident.

An `EXIT`/`INT`/`TERM` trap thaws and kills every session the run created, `kill -9`s any server
pid it recorded that still answers nothing (the upgrade tests run servers from copied or deleted
binaries), and removes the root — unless `--keep`, which prints the path instead.

## What each test covers

| Test | What it asserts |
|---|---|
| `services_lifecycle` | `service add --no-start` writes `$ZELLIJ_CONFIG_DIR/services/<name>.json` and starts nothing; `list` shows `stopped`; `start` makes it `running`; `logs` shows the command's output; `stop` keeps the definition; `remove` deletes it and drops it from `list`. |
| `restart_policy_backoff` | A `--restart always` service running `sh -c 'echo run; sleep 1; exit 1'` is re-run: `service logs` ends up with at least two `run` lines *and* the `command exited … restarting` separator, i.e. scrollback is kept across supervised restarts. |
| `restart_policy_on_failure_exit_zero` | `attach --create-background … --restart on-failure -- sh -c 'echo once; exit 0'` runs the command exactly once — a clean exit under `on-failure` holds the pane instead of restarting it — and the session stays alive. |
| `freeze_thaw` | `freeze <session> --status` reports the pane `running`; `freeze` flips it to `frozen`; the service's tick count does not move for ~3 s; `thaw` flips it back and the ticks resume. |
| `upgrade_in_place` | A session started from a copy of the binary, with the copy then replaced the way a package upgrade replaces it (write-then-rename, so `/proc/<pid>/exe` gains `(deleted)`). `upgrade-server --all` exits 0, the server pid is unchanged, `/proc/<pid>/exe` is clean again, the pane's child process is alive *and still a child of that same server*, and the session still lists. A second `--all` exits 0 and says `Nothing to upgrade`. |
| `upgrade_failure_is_safe` | Same setup but the copy is deleted rather than replaced. `upgrade-server <session> --timeout 10` exits non-zero within ~10 s, prints a reason, and leaves the server pid and the pane's child alive and related. |
| `net_addresses` | `service net-setup` prints a `/64` prefix inside `fd00::/8`; `service add --bind-ip` prints the service's own address; `service list` shows it; the pane's environment really carries `GEZELLIJ_BIND_ADDR=<that address>`; `service net-export --format caddy` contains the bracketed address. |
| `systemd_unit_export` | `service export-systemd <name>` emits `Type=simple`, `Restart=on-failure`, an `ExecStart=… service run <name>` line, and no `After=default.target`. |

## When tests skip

Only `freeze_thaw` skips, and only for one reason: the session's server could not create
per-pane cgroups, so `zellij freeze --status` reports *"has no pane cgroups"*. That happens
without cgroup v2 delegation — some containers, non-systemd inits, or a server started as a
system service without `Delegate=yes`. The skip is reported with that reason and does not fail
the run. Everything else is expected to pass on any Linux host.
