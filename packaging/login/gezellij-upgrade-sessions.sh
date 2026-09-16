#!/bin/sh
#
# gezellij-upgrade-sessions.sh — migrate this user's running Gezellij servers
# onto a freshly installed binary.
#
# This is the OPT-IN, automatic counterpart of the pacman notice. The package
# installs it (together with the two systemd units next to it) into
#     /usr/share/gezellij/systemd/
# and enables nothing. Nothing happens until you ask for it.
#
# ---------------------------------------------------------------------------
# ENABLING IT (systemd --user)
# ---------------------------------------------------------------------------
# /usr/share/gezellij/systemd/ is NOT in the systemd --user search path, so the
# units have to be linked in by absolute path:
#
#   systemctl --user link   /usr/share/gezellij/systemd/gezellij-upgrade-sessions.service
#   systemctl --user enable --now /usr/share/gezellij/systemd/gezellij-upgrade-sessions.path
#
# (the .path unit refers to the .service by name, so the .service must be
# linked separately). Equivalent alternative: copy both unit files into
# ~/.config/systemd/user/ and `systemctl --user daemon-reload` first.
#
# UNDO — leaves nothing behind:
#
#   systemctl --user disable --now gezellij-upgrade-sessions.path
#   systemctl --user disable gezellij-upgrade-sessions.service
#
# (`disable` removes the symlinks that `link` created; if you copied the units
# by hand, delete the copies from ~/.config/systemd/user/ instead.)
#
# ---------------------------------------------------------------------------
# USING IT FROM A SHELL RC INSTEAD
# ---------------------------------------------------------------------------
# Put this in your *login* rc (~/.zprofile, ~/.profile), BEFORE any
# gezellij-login-setup block, so the upgrade happens first and the auto-attach
# that follows lands on the already-upgraded server:
#
#   /usr/share/gezellij/systemd/gezellij-upgrade-sessions.sh >/dev/null 2>&1 || true
#
# Run it as a *command*, never `source`/`.` it: this script uses `exit` and
# `exec`, so sourcing it would end or replace your login shell.
#
# The `|| true` matters there: this script passes `upgrade-server`'s exit status
# through (so `systemctl --user status` shows real failures), and you do not
# want a failed upgrade to abort your login shell.
#
# ---------------------------------------------------------------------------
# NOTES
# ---------------------------------------------------------------------------
# * `upgrade-server --all` only touches servers whose binary was actually
#   replaced on disk, so running it repeatedly is a no-op. That matters because
#   a PathChanged= trigger can fire more than once during a single pacman
#   transaction.
# * The socket directory is derived from $XDG_RUNTIME_DIR, which the user
#   manager sets, so a unit-started run sees the same sessions as your shell.
#   If you override ZELLIJ_SOCKET_DIR in your shell rc, the unit will NOT see
#   it — use `systemctl --user set-environment ZELLIJ_SOCKET_DIR=...` too.

# Not installed (or not on PATH for the user manager): nothing to do.
command -v gezellij >/dev/null 2>&1 || exit 0

# Never upgrade the session we are sitting in. Run from an rc file inside a
# Gezellij pane this would restart the very server our client is attached to and
# drop us mid-login.
[ -n "${ZELLIJ:-}" ] && exit 0

exec gezellij upgrade-server --all
