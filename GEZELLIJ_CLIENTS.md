# Clients: roaming between machines without shrinking yourself

Gezellij, like Zellij, lets several clients attach to the same session at once. That is the
feature that makes a session follow you around: the laptop in the morning, the phone on the
train, the desktop in the afternoon. This document is about the one thing that spoils it, and
what the fork does about it.

## Your phone should not shrink your desktop

A tab is sized to fit **every client that is looking at it**, which means it is as big as the
*smallest* of them. That is upstream Zellij behaviour and it is the right behaviour while
everyone really is looking: nobody wants half a pane cut off.

It is the wrong behaviour the moment somebody walks away. You attach from your phone at 80x24,
put the phone in your pocket, sit down at a 200x60 desktop — and the desktop is 80x24, because
the phone is still attached and still counts. The machine you are actually sitting at is
degraded by the machines you walked away from. Nothing is broken, nothing is reported, and the
only hint is that your panes are mysteriously small.

The fix in this fork is **parking** (opt-in, off by default):

```kdl
// ~/.config/zellij/config.kdl
park_inactive_clients_after "10m"
```

A client that has sent no input for that long is moved to a tab of its own, called `parked`. It
stops constraining the tab it was on, so the machine in front of you gets its full size back.
When you come back to the phone, **any keypress** picks it straight back up — same tab, same
panes, same focus, instantly.

Parking deliberately adds nothing to the sizing logic. A client viewing a *different* tab
already constrains nobody: `Screen::recompute_tab_size` only looks at clients whose active tab
is that tab. So "step aside for a moment" is expressible entirely in terms that Zellij already
implements and tests. There is no letterboxing, no exclusion list, no special case.

## The sharper version: a client that is never coming back

The behaviour above has a degenerate case, and it is how this feature came to exist.

A second machine was attached to session `main`. That machine rebooted. On the server, nothing
detectably died: the client process (`gezellij attach -c main` on `/dev/pts/0`) was still alive,
its parent bash was still alive, its pty still worked, and it would have rendered a frame if
asked. Meanwhile a new client attached on `/dev/pts/3`.

```
/dev/pts/0   61x238   the ghost
/dev/pts/3   68x238   the human
```

While the ghost sat on tab 2 it only shrank tab 2. When the user closed tab 2 the ghost moved
onto the tab they were using and silently cost them seven rows. Working out *why* took `ps`,
`stty` and a read of the source; fixing it took `kill`.

The important lesson is the one that shapes everything below: **at the protocol level, the ghost
was indistinguishable from a person who had stepped away from their desk.** Process alive,
socket healthy, would render on demand. There is no honest "detect dead client" test, so the
fork does not pretend there is one.

## Making it visible: `zellij action list-clients`

```
$ zellij action list-clients
CLIENT_ID SIZE      IDLE            ZELLIJ_PANE_ID RUNNING_COMMAND
1         61x238    2h 14m          terminal_3     N/A
2*        68x238    0s              terminal_7     N/A
```

Two columns are new in this fork:

* **SIZE** — that client's terminal size as `ROWSxCOLS`. It is read from `Screen::client_sizes`,
  which is the very map `recompute_tab_size` takes the minimum over, so the listing explains the
  size you are getting rather than merely describing the clients.
* **IDLE** — time since that client last sent any input (`12s`, `4m`, `2h 10m`), or `-` if we
  have no record of it. A parked client reads `2h 14m (parked)`, and its RUNNING_COMMAND reads
  `gezellij:parked` rather than the `printf` that draws the notice it is looking at.

The `*` marks the client the command is attributed to. A `zellij action` invocation is its own
short-lived client, so the server attributes it to the session's *last active* client — the one
that most recently typed anything. Right after you have typed the command, that is you. It is
absent when nothing has been typed in the session yet, and it is not to be trusted if you run
`list-clients` from a script while somebody else is working.

Small and idle, on your tab, is the signature of the problem.

## Making it fixable: `zellij action kick-client`

```
$ zellij action kick-client 1
Disconnected client 1.
```

The named client is sent `ExitReason::ForceDetached` and removed from the session — from the
server's client table *and* from `Screen` and the plugin thread, so it stops contributing to tab
sizes immediately. The tab grows back on the spot.

It refuses to kick the client the command is attributed to — the same last-active-client rule
as the `*` above, so in practice "you, just now". Disconnecting yourself is `zellij action
detach`. It also refuses an id that is not attached, and tells you to look at `list-clients`.

This is the manual lever. You should rarely need it if parking is on.

## The option, and why it is off by default

```kdl
park_inactive_clients_after "10m"
```

`Option<Duration>` in humantime spelling, configuration-file only, unset by default.

It is off by default because of the lesson from the incident: a client that has been abandoned
and a client whose owner is quietly reading are the same thing from here. Guessing wrong in the
"reading" direction is rude — your tab jumps out from under you while you are looking at it.
So Gezellij does not guess: it *asks*, by moving the client somewhere harmless and letting a
human prove presence with a keystroke. That is a cheap question with a cheap answer, but it is
still a question, and the answer should be your choice to ask.

Once you have decided to ask it, a **short** timeout is safe, and `"10m"` is a sensible starting
point for anyone who actually roams. The cost of being parked is one keypress; the cost of not
being parked is everybody else's screen. A genuine ghost never presses anything and stays parked
forever, harmlessly.

### What parking does and does not do

* It **never** parks the last remaining client of a session.
* It **never** parks a client that is alone on its tab — it is constraining nobody, so there is
  nothing to fix.
* It **never** disconnects anybody. Parking only changes which tab one client is looking at.
* It does nothing in a mirrored session (`mirror_session true`), where per-client tab focus does
  not exist by design.
* The `parked` tab is created when the first client is parked and closed again when the last one
  leaves. It holds one pane with an explanation and no running process.
* While somebody is parked, the `parked` tab is part of the session like any other, so a layout
  serialized for resurrection during that window will contain it. It does no harm — it is a
  single held pane — but you may want to delete the tab from a resurrection layout you keep.

### The keep-alive

Any input from a parked client un-parks it: a key, a mouse click, a scroll. The documented
gesture is ENTER, because that is what the parked pane says, but nothing is special about ENTER.

That first *keystroke* is **consumed** — it buys the trip home and is not delivered to the pane
you land on. Delivering it would mean an ENTER running whatever happened to be on your shell's
command line, which is precisely the kind of surprise this feature exists to avoid. Non-key
actions (a mouse click, and in particular the detach a client sends when it is signalled) do
un-park you and are then handled as usual: unlike a blind ENTER they mean something specific,
and dropping them would be worse than acting on them.

The idle clock is per client and lives in `zellij-server/src/client_activity.rs`. It is seeded
when a client attaches (so a fresh client is never "ancient"), updated on every key and every
non-CLI action, and dropped when the client goes away. `zellij action …` commands do *not* count
as presence: they can come from a cron job, and the server attributes them to the last active
client rather than to their sender.

## A ghost that cannot be asked to leave

One more thing turned up while testing the above, and it is fixed in the fork.

When a client's terminal goes away — the laptop closes its lid, the ssh session dies, the
machine reboots — the client keeps rendering into a pty that nobody is reading. The pty's
buffer fills and the client's main thread blocks forever inside a write to stdout.

Upstream's signal handling asks the server to detach and then waits for the server's reply to
wind the main loop down. With stdout blocked that wind-down can never happen, so `kill` would
take the client out of `list-clients` (the signal thread got that far) and leave the process
sitting in `Sl+` on its pty, removable only with `SIGKILL`. Exactly the client `kick-client` is
for, and it could not die when asked.

A signalled client now starts a three-second watchdog alongside the polite shutdown: if it has
not gone by then, it puts the terminal back and exits anyway. A healthy client always leaves
through the front door long before that.

## See also

* `zellij-server/src/client_activity.rs` — the presence map and the parked set
* `Screen::clients_to_park` / `Screen::unpark_client` in `zellij-server/src/screen.rs`
* `Screen::recompute_tab_size` — the upstream minimum this is all in aid of
* [GEZELLIJ_FREEZE.md](GEZELLIJ_FREEZE.md) — the other opt-in idleness feature, for whole
  sessions rather than single clients
