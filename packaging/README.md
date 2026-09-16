# Packaging Gezellij

Three independent pieces live here, all designed so that **nothing they do can
break your login and everything is one command away from being undone**:

| Path | What it is |
|---|---|
| `arch/PKGBUILD` | `gezellij-git` Arch package — installs the fork as `/usr/bin/gezellij`, next to (not over) the stock `zellij`. |
| `login/gezellij-login-setup.sh` | Makes Gezellij greet you at login, replacing byobu/tmux/screen auto-attach setups reversibly. Installed by the package as `/usr/bin/gezellij-login-setup`. |
| `arch/gezellij.hook` + `login/gezellij-upgrade-sessions.*` | Makes a package upgrade obvious: a pacman hook that prints a notice, and an opt-in `systemd --user` pair that migrates your running sessions onto the new binary. See [§3 Upgrading](#3-upgrading). |

---

## 1. Building and installing the package

```sh
cd packaging/arch
makepkg -si          # build + install
```

Uninstall (this is the full rollback of step 1):

```sh
sudo pacman -R gezellij-git
```

### What lands on your system

```
/usr/bin/gezellij
/usr/bin/gezellij-login-setup
/usr/share/bash-completion/completions/gezellij
/usr/share/zsh/site-functions/_gezellij
/usr/share/fish/vendor_completions.d/gezellij.fish
/usr/share/libalpm/hooks/gezellij.hook
/usr/share/libalpm/scripts/gezellij-upgrade-notice
/usr/share/gezellij/systemd/gezellij-upgrade-sessions.{sh,service,path}
/usr/share/doc/gezellij/{GEZELLIJ_PLAN.md,GEZELLIJ_SERVICES.md,PACKAGING.md}
/usr/share/licenses/gezellij-git/LICENSE.md
```

The `libalpm` hook only *prints* a notice after an upgrade and the
`usr/share/gezellij/systemd` files are inert templates — nothing is enabled.
See [§3 Upgrading](#3-upgrading).

Nothing is installed under the name `zellij`, and the PKGBUILD deliberately has
**no `provides=`/`conflicts=` for `zellij`**: the distro `zellij` package stays
installed and untouched, so you always have a known-good binary to fall back on.
The completions are renamed (`gezellij`, `_gezellij`, …) for the same reason —
under the upstream names pacman would refuse the install with a file conflict.

### Source: it builds a *clone*, not your working tree

`source=()` points at the GitHub branch:

```
gezellij::git+https://github.com/LaPingvino/gezellij.git#branch=feat/phase1-service-mode
```

makepkg clones that, so **only committed and pushed content is built** —
uncommitted changes in your checkout are invisible to the build, and so is this
`packaging/` directory until it is committed. To build a local checkout instead,
commit first and edit the source line to an absolute `file://` URL:

```
source=("gezellij::git+file:///home/joop/gezellij#branch=feat/phase1-service-mode")
```

### Build notes

* A plain `cargo build --release` is enough. The builtin WASM plugins are
  embedded from the committed `zellij-utils/assets/plugins/*.wasm`; the
  `plugins_from_target` default feature only redirects to
  `target/wasm32-wasip1/debug` in `debug_assertions` builds, so no wasm target
  or toolchain is needed.
* Runtime deps are just `glibc` + `gcc-libs`. With the default features
  (`vendored_curl` → static curl + vendored openssl-sys, `web_server_capability`
  → bundled rusqlite) there is no system libssl/libcurl/libsqlite linkage.
  Building `--no-default-features` would change that — add `openssl`/`curl` then.
* `perl` is a makedepend because openssl-src's `Configure` needs it.
* `RUSTUP_TOOLCHAIN=stable` is exported so `rust-toolchain.toml`'s pinned
  1.95.0 + extra targets do not make cargo attempt a network download inside
  makepkg's build.

---

## 2. Login setup

```sh
gezellij-login-setup --dry-run       # show exactly what would change
gezellij-login-setup install         # do it
gezellij-login-setup status          # what is in place right now
gezellij-login-setup undo            # put everything back
```

Useful flags: `--shell zsh|bash|fish|all`, `--binary PATH`, `--session NAME`
(default `main`), `--dry-run`, `--help`.

`install` does three things:

1. **Neutralises existing auto-start setups.** Column-0, uncommented lines in
   `~/.profile`, `~/.bashrc`, `~/.bash_profile`, `~/.zshrc`, `~/.zprofile`,
   `~/.zlogin` and `~/.config/fish/config.fish` that launch byobu, tmux or
   screen are rewritten as
   `# gezellij-login-setup disabled: <the original line, verbatim>`.
   Nothing is ever deleted.
2. **Adds a marked block** (`# >>> gezellij login setup >>>` …
   `# <<< gezellij login setup <<<`) to your shell rc file. The block sets
   `ZELLIJ_AUTO_ATTACH=true`, forces `ZELLIJ_AUTO_EXIT=false`, and evaluates
   Zellij's own auto-start snippet (`<bin> setup --generate-auto-start <shell>`)
   with the two invocation lines rewritten to your binary and session name.
3. **Records what it did** in
   `${XDG_STATE_HOME:-~/.local/state}/gezellij/login-setup.env`.

### Safety guarantees

* **Backups.** Every file it edits is copied to
  `<file>.gezellij-backup-<YYYYmmdd-HHMMSS>` first. Backups are never deleted,
  not even by `undo` — it prints their paths as a manual fallback.
* **Atomic, symlink-safe edits.** Edits are made on a temp copy in the same
  directory and `mv`'d over the *resolved* path, so a `~/.zshrc` symlinked into
  a dotfiles repo stays a symlink and no file is ever half-written.
* **Conservative matching.** Only column-0, uncommented lines outside heredocs
  are rewritten, and only where commenting them out cannot change syntax or
  meaning. A match is reported as a warning instead of edited when it is
  indented, is an `alias`/`export`/function definition, ends in a line
  continuation, follows a line ending in `\` `&&` `||` `|` `then` `do` `else`
  `{` `(`, or is followed by a block closer (`fi`, `done`, `else`, `elif`,
  `esac`, `}`, `)`, `;;`, `end`) — commenting that last one out would leave an
  empty block, i.e. a syntax error in your rc file and a broken login.
  Warned-about lines run *before* the appended block, so a tmux/screen they
  start will pre-empt Gezellij (the `$TMUX`/`$STY` guard then skips it) until
  you hand-edit them.
* **Byte-for-byte undo.** A missing final newline, a trailing blank line, an rc
  file that install had to create — all are restored/removed by `undo`, so a
  `diff` against your pre-install file comes back empty.
* **`undo` works even without the backups**, because it reverses the markers on
  the live file — so any edits you made after installing survive the undo.
* **byobu is disabled the gentle way.** The script never runs `byobu-disable`:
  on Arch that command calls `byobu-launcher-uninstall`, which `sed -i`-deletes
  lines from your rc files (and can kill a running byobu session). Instead it
  creates byobu's own `disable-autolaunch` flag file in
  `~/.byobu/` or `~/.config/byobu/`, and `undo` removes it.
* **Drift detection.** At install time the rewritten snippet is checked; if a
  future Zellij still emits a bare `zellij` command after the rewrite, the
  script aborts *before changing anything* rather than silently arranging for
  the wrong binary to start at login.
* **`--dry-run` touches nothing** — no backups, no state file, no edits.
* Running `install` twice does not duplicate the block; it replaces the existing
  one (so changing `--session` takes effect).

### Guards in the installed block

The auto-start is skipped when any of these hold:

* the shell is not interactive;
* `$TERM` is unset, `dumb`, or `linux` (a Linux VT console — this is the guard
  that keeps a broken build from locking you out of tty1);
* you are already inside zellij (`$ZELLIJ`), tmux (`$TMUX`) or screen (`$STY`);
* `$SSH_ORIGINAL_COMMAND` is set (scp / rsync / git over ssh);
* `$GEZELLIJ_NO_AUTOSTART` is set;
* `~/.config/gezellij/no-autostart` exists;
* the binary is missing or not executable.

Quick opt-out without undoing anything:

```sh
touch ~/.config/gezellij/no-autostart     # or: export GEZELLIJ_NO_AUTOSTART=1
```

**If you ever do get stuck**, a rescue login is always available: boot into a VT
(`$TERM=linux`, auto-start is skipped there), or run
`GEZELLIJ_NO_AUTOSTART=1 bash --norc`.

---

## 3. Upgrading

### What a package upgrade does to running sessions

Upgrading `gezellij-git` replaces `/usr/bin/gezellij` on disk. Running servers
do **not** notice: each one keeps executing the old, now-deleted file (that is
exactly what `/proc/<pid>/exe` ending in `" (deleted)"` means), and so do all
the processes in your panes. Nothing breaks, but you stay on the old build
until you say otherwise.

Because of that, the package installs a pacman `PostTransaction` hook
(`/usr/share/libalpm/hooks/gezellij.hook`) whose only job is to print a short
reminder. It deliberately does not touch anybody's sessions: pacman runs as
root, sessions are per-user processes, and migrating someone else's session as
root would be wrong. The notice is unconditional — as root we cannot reliably
tell whether anyone has a session running — so you will also see it on a plain
reinstall (pacman counts reinstalling an installed package as an upgrade).

### The manual flow

As your own user:

```sh
gezellij list-sessions                 # what is running
gezellij upgrade-server --all          # migrate every session of this user
gezellij upgrade-server <session>      # ... or just one
gezellij attach <session>              # re-attach afterwards
```

`upgrade-server` sends `SIGUSR2` to the session's server (its pid is recorded
in `<socket dir>/<session>.server-pid`); the server writes its resurrection
layout, then `execve`s the *new* binary in place, handing over the live PTY
file descriptors. `--all` does that for every server of the current user whose
binary was replaced on disk, so running it twice is a no-op.

**What survives:**

* every process in every pane — same pid, not restarted, not reparented;
* the server's own pid (so `systemd` and any supervisor keep tracking it);
* the layout: tabs, tiled and floating panes, working directories, and the
  per-pane restart policy.

**What does not:**

* **scrollback** — pane history is not carried across the exec;
* **attached clients** — they are dropped, not told to reconnect, so re-attach
  with `gezellij attach <session>`;
* **plugins** — WASM plugins are restarted and lose their state;
* the terminal grid contents of a pane (the program repaints, as after a
  detach/attach cycle).

### The opt-in automatic flow

If you would rather never think about it, the package ships (but does not
enable) a per-user helper plus two `systemd --user` units in
`/usr/share/gezellij/systemd/`:

| File | What it is |
|---|---|
| `gezellij-upgrade-sessions.sh` | runs `gezellij upgrade-server --all`, skipping the session you are inside (`$ZELLIJ`) |
| `gezellij-upgrade-sessions.service` | `Type=oneshot`, runs that script |
| `gezellij-upgrade-sessions.path` | `PathChanged=/usr/bin/gezellij`, i.e. fires right after an upgrade replaces the binary |

That directory is **not** in the `systemd --user` search path, so enable the
units by absolute path:

```sh
systemctl --user link   /usr/share/gezellij/systemd/gezellij-upgrade-sessions.service
systemctl --user enable --now /usr/share/gezellij/systemd/gezellij-upgrade-sessions.path
```

(the `.path` unit refers to the `.service` by name, which is why the service is
linked separately). Copying both unit files into `~/.config/systemd/user/` and
running `systemctl --user daemon-reload` works just as well.

Undo — this leaves nothing behind:

```sh
systemctl --user disable --now gezellij-upgrade-sessions.path
systemctl --user disable gezellij-upgrade-sessions.service
```

(`disable` removes the symlinks `link` created; delete the copies by hand if
you copied the units instead.)

Prefer a shell rc? Put this in your *login* rc (`~/.zprofile`, `~/.profile`)
**before** any `gezellij-login-setup` block, so the upgrade runs first and the
auto-attach that follows lands on the new server:

```sh
/usr/share/gezellij/systemd/gezellij-upgrade-sessions.sh >/dev/null 2>&1 || true
```

The `|| true` is deliberate: the script passes `upgrade-server`'s exit status
through (so `systemctl --user status` shows real failures), and a failed
upgrade must not abort your login shell.

Two things worth knowing about the unit variant: `PathChanged` can fire more
than once per pacman transaction (harmless — `--all` is idempotent), and the
unit inherits `XDG_RUNTIME_DIR` from the user manager, so it sees the same
sessions as your shell — *unless* you override `ZELLIJ_SOCKET_DIR` in your
shell rc, which the unit will not see. Use
`systemctl --user set-environment ZELLIJ_SOCKET_DIR=...` in that case.

### If an upgrade goes wrong

There is no rollback after the `exec`: the successor process *is* the old
process, so a successor that fails to adopt the panes cannot hand them back.
The honest fallback is the resurrection layout the server wrote to disk right
before the exec (`session-layout.kdl` in the session's cache directory). If the
broken server is still running it holds the socket, so end it first — otherwise
`attach` just connects to the broken server and nothing is resurrected:

```sh
gezellij kill-session <session>            # or, if it no longer answers:
kill "$(cat <socket dir>/<session>.server-pid)"
gezellij attach <session>
```

rebuilds the session from it — same tabs, same panes, same commands and working
directories, but with **fresh processes**, so whatever was running in the panes
is started again rather than continued. And the stock distro `zellij` binary is
still installed and untouched (see §1), so a known-good multiplexer is always
one command away.

---

## 4. Caveats (until the rename phase lands)

* **Shared config directory.** Gezellij still reads `~/.config/zellij`
  (`ZELLIJ_CONFIG_DIR` / `--config-dir` override it). Config changes you make
  for one apply to the other.
* **Shared socket directory.** Both binaries report version 0.46.0 and use
  `$TMPDIR/zellij-$UID/<version>`, so `gezellij attach` can land on a server
  started by the stock `zellij`. Export `ZELLIJ_SOCKET_DIR=/run/user/$UID/gezellij`
  (see `SOCKET_DIR_ENV_KEY` in `zellij-utils/src/envs.rs`) if you want strict
  isolation while testing the fork.
* **Completion helper names.** The generated completion scripts still define
  internal helpers named `_zellij` / `__fish_zellij_*`. Harmless — the files and
  the completed command name are `gezellij`.
* **The upstream auto-start snippet has no interactivity guard** of its own for
  bash/zsh; all the guards listed above come from this login block. Do not copy
  the bare upstream snippet into your rc file as a substitute.
