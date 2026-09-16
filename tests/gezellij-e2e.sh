#!/usr/bin/env bash
#
# Gezellij end-to-end regression suite.
#
# These are the Gezellij-specific behaviours (services, restart supervision, cgroup
# freeze/thaw, in-place server upgrade, ULA loopback addresses, systemd export). They need
# real PTYs, unix sockets, /proc, a daemonizing server and -- for freeze -- cgroup v2
# delegation, none of which a cargo test can provide, so the suite is a shell script.
#
# Usage:  tests/gezellij-e2e.sh [--binary PATH] [--keep] [test-name ...]
#
# See tests/README.md.

set -u -o pipefail   # deliberately no `set -e`: the assert helpers record failures

# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
BINARY=${GEZELLIJ_TEST_BINARY:-$REPO_ROOT/target/debug/zellij}
KEEP=0
SELECTED=()

usage() {
    cat <<EOF
Usage: $0 [--binary PATH] [--keep] [test-name ...]

  --binary PATH   zellij/gezellij binary to test
                  (default: \$GEZELLIJ_TEST_BINARY, else target/debug/zellij)
  --keep          do not remove the temporary root, print its path instead
  test-name ...   run only these tests (default: all)

Tests: $(printf '%s ' "${ALL_TESTS[@]}")
EOF
}

ALL_TESTS=(
    services_lifecycle
    restart_policy_backoff
    restart_policy_on_failure_exit_zero
    freeze_thaw
    upgrade_in_place
    upgrade_failure_is_safe
    net_addresses
    systemd_unit_export
    client_sizes_and_kick
    client_parking
    signalled_client_exits
)

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BINARY=${2:?--binary needs a path}; shift 2 ;;
        --binary=*) BINARY=${1#--binary=}; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift ;;
        -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *) SELECTED+=("$1"); shift ;;
    esac
done

if [ ! -x "$BINARY" ]; then
    echo "No zellij binary at '$BINARY'." >&2
    echo "Build one with:" >&2
    echo "  cargo build -p zellij --no-default-features --features \"vendored_curl,web_server_capability\"" >&2
    exit 2
fi
BINARY=$(cd -- "$(dirname -- "$BINARY")" && pwd -P)/$(basename -- "$BINARY")

# ---------------------------------------------------------------------------
# colours
# ---------------------------------------------------------------------------

if [ -t 1 ]; then
    C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'
    C_BOLD=$'\033[1m'; C_OFF=$'\033[0m'
else
    C_RED=''; C_GREEN=''; C_YELLOW=''; C_BOLD=''; C_OFF=''
fi

# ---------------------------------------------------------------------------
# hermetic environment
# ---------------------------------------------------------------------------

# Keep the root short: unix socket paths are capped at ~108 bytes and the session socket
# lives at $ZELLIJ_SOCKET_DIR/contract_version_N/<session>.
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/gez-e2e.XXXXXX") || exit 2
ROOT=$(cd -- "$ROOT" && pwd -P)

export ZELLIJ_CONFIG_DIR="$ROOT/config"
export ZELLIJ_SOCKET_DIR="$ROOT/sock"
export XDG_CACHE_HOME="$ROOT/cache"
export XDG_CONFIG_HOME="$ROOT/xdg-config"
export XDG_DATA_HOME="$ROOT/xdg-data"
mkdir -p "$ZELLIJ_CONFIG_DIR" "$ZELLIJ_SOCKET_DIR" "$XDG_CACHE_HOME" \
         "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$ROOT/tmp"
# A stale session name from the user's shell must not leak into session resolution.
unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID GEZELLIJ_UPGRADE_BINARY

# Safety net: never, ever operate on the user's real sessions.
case "$ZELLIJ_SOCKET_DIR" in
    "$ROOT"/*) : ;;
    *) echo "refusing to run: ZELLIJ_SOCKET_DIR ('$ZELLIJ_SOCKET_DIR') is not inside the temp root ('$ROOT')" >&2
       exit 2 ;;
esac
if [ ${#ZELLIJ_SOCKET_DIR} -gt 70 ]; then
    echo "refusing to run: ZELLIJ_SOCKET_DIR is $(( ${#ZELLIJ_SOCKET_DIR} )) bytes long; session sockets would exceed the 108 byte unix socket limit." >&2
    echo "Set TMPDIR to something shorter (e.g. TMPDIR=/tmp) and try again." >&2
    exit 2
fi

# Sessions and server pids this run created, for the cleanup trap.
CREATED_SESSIONS=()
CREATED_PIDS=()

track_session() { CREATED_SESSIONS+=("$1"); }
track_pid() { CREATED_PIDS+=("$1"); }

cleanup() {
    local s pid
    for s in "${CREATED_SESSIONS[@]:-}"; do
        [ -n "$s" ] || continue
        # thaw first: a frozen session cannot be killed cleanly
        "$BINARY" thaw "$s" >/dev/null 2>&1
        "$BINARY" kill-session "$s" >/dev/null 2>&1
    done
    "$BINARY" kill-all-sessions --yes >/dev/null 2>&1
    for pid in "${CREATED_PIDS[@]:-}"; do
        [ -n "$pid" ] || continue
        [ -d "/proc/$pid" ] || continue
        # Servers started from a copied (or deleted) binary may not answer the socket.
        kill -9 "$pid" >/dev/null 2>&1
    done
    if [ "$KEEP" = 1 ]; then
        echo "${C_YELLOW}kept temp root:${C_OFF} $ROOT"
    else
        rm -rf "$ROOT"
    fi
}
trap cleanup EXIT
# Ctrl-C must stop the suite, not clean up and then carry on with a deleted root.
trap 'trap - EXIT; cleanup; exit 130' INT TERM

# ---------------------------------------------------------------------------
# tiny test framework
# ---------------------------------------------------------------------------

PASSED=0
FAILED=0
SKIPPED=0
CURRENT_TEST=""
CURRENT_FAILED=0
CURRENT_SKIP=""

ok() {   # ok <description>
    printf '    %sok%s   %s\n' "$C_GREEN" "$C_OFF" "$1"
}

fail() { # fail <description> [detail ...]
    CURRENT_FAILED=1
    printf '    %sFAIL%s %s\n' "$C_RED" "$C_OFF" "$1"
    local line
    shift || true
    for line in "$@"; do
        printf '         %s\n' "$line"
    done
}

skip() { # skip <reason> -- marks the whole test skipped
    CURRENT_SKIP="$1"
}

assert_eq() { # assert_eq <actual> <expected> <description>
    if [ "$1" = "$2" ]; then
        ok "$3"
    else
        fail "$3" "expected: $2" "actual:   $1"
    fi
}

assert_contains() { # assert_contains <haystack> <needle> <description>
    case "$1" in
        *"$2"*) ok "$3" ;;
        *) fail "$3" "expected to contain: $2" \
                "actual:" "$(printf '%s\n' "$1" | sed 's/^/           | /' | head -30)" ;;
    esac
}

assert_not_contains() { # assert_not_contains <haystack> <needle> <description>
    case "$1" in
        *"$2"*) fail "$3" "expected NOT to contain: $2" \
                     "actual:" "$(printf '%s\n' "$1" | sed 's/^/           | /' | head -30)" ;;
        *) ok "$3" ;;
    esac
}

assert_match() { # assert_match <string> <ERE> <description>
    if printf '%s' "$1" | grep -Eq -- "$2"; then
        ok "$3"
    else
        fail "$3" "expected to match: $2" "actual:   $1"
    fi
}

assert_true() { # assert_true <shell condition> <description>
    if eval "$1" >/dev/null 2>&1; then
        ok "$2"
    else
        fail "$2" "condition failed: $1"
    fi
}

assert_false() { # assert_false <shell condition> <description>
    if eval "$1" >/dev/null 2>&1; then
        fail "$2" "condition unexpectedly succeeded: $1"
    else
        ok "$2"
    fi
}

# wait_for <shell condition> <seconds> -- polls 10x/s until the condition holds.
# Returns 0 when it held, 1 on timeout.
wait_for() {
    local cond=$1 secs=${2:-10} i
    local ticks=$(( secs * 10 ))
    for (( i = 0; i < ticks; i++ )); do
        if eval "$cond" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    eval "$cond" >/dev/null 2>&1
}

run_test() { # run_test <name>
    local name=$1
    CURRENT_TEST=$name
    CURRENT_FAILED=0
    CURRENT_SKIP=""
    printf '%s==> %s%s\n' "$C_BOLD" "$name" "$C_OFF"
    "test_$name"
    # A failure that already happened is never papered over by a later skip.
    if [ "$CURRENT_FAILED" = 1 ]; then
        FAILED=$(( FAILED + 1 ))
        printf '  %sFAIL%s %s\n\n' "$C_RED" "$C_OFF" "$name"
    elif [ -n "$CURRENT_SKIP" ]; then
        SKIPPED=$(( SKIPPED + 1 ))
        printf '  %sSKIP%s %s: %s\n\n' "$C_YELLOW" "$C_OFF" "$name" "$CURRENT_SKIP"
    else
        PASSED=$(( PASSED + 1 ))
        printf '  %sPASS%s %s\n\n' "$C_GREEN" "$C_OFF" "$name"
    fi
}

# ---------------------------------------------------------------------------
# helpers around the binary
# ---------------------------------------------------------------------------

z() { "$BINARY" "$@"; }

# server_pid_of <session> -- the pid the server recorded next to its socket, or empty.
server_pid_of() {
    local f
    f=$(find "$ZELLIJ_SOCKET_DIR" -name "$1.server-pid" -type f 2>/dev/null | head -1)
    [ -n "$f" ] || return 1
    tr -d '[:space:]' < "$f"
}

exe_of() { readlink "/proc/$1/exe" 2>/dev/null; }
ppid_of() { awk '/^PPid:/ {print $2}' "/proc/$1/status" 2>/dev/null; }

session_listed() { z list-sessions --no-formatting 2>/dev/null | grep -q "^$1[[:space:]]"; }

kill_session_quietly() {
    z thaw "$1" >/dev/null 2>&1
    z kill-session "$1" >/dev/null 2>&1
    z delete-session "$1" --force >/dev/null 2>&1
}

remove_service_quietly() {
    z service remove "$1" >/dev/null 2>&1
    rm -f "$ZELLIJ_CONFIG_DIR/services/$1.json"
}

# ---------------------------------------------------------------------------
# 1. services_lifecycle
# ---------------------------------------------------------------------------

test_services_lifecycle() {
    local name=e2elife def out
    def="$ZELLIJ_CONFIG_DIR/services/$name.json"
    remove_service_quietly "$name"
    track_session "svc-$name"

    out=$(z service add --name "$name" --no-start --cwd "$ROOT" \
              -- sh -c 'echo lifecycle-marker; sleep 300' 2>&1)
    assert_eq "$?" "0" "service add --no-start exits 0"
    assert_contains "$out" "$def" "add reports where the definition went"
    assert_true "[ -f '$def' ]" "definition file exists under \$ZELLIJ_CONFIG_DIR/services"

    out=$(z service list --no-formatting 2>&1)
    assert_match "$out" "^$name[[:space:]]+stopped[[:space:]]" "list shows the service as stopped"

    assert_false "z list-sessions --no-formatting 2>/dev/null | grep -q '^svc-$name'" \
                 "--no-start really did not start a session"

    out=$(z service start "$name" 2>&1)
    assert_eq "$?" "0" "service start exits 0"

    wait_for "z service list --no-formatting 2>/dev/null | grep -Eq '^$name[[:space:]]+running'" 15
    out=$(z service list --no-formatting 2>&1)
    assert_match "$out" "^$name[[:space:]]+running[[:space:]]" "list shows the service as running"

    wait_for "z service logs '$name' 2>/dev/null | grep -q lifecycle-marker" 15
    out=$(z service logs "$name" 2>&1)
    assert_contains "$out" "lifecycle-marker" "logs show the command's output"

    out=$(z service stop "$name" 2>&1)
    assert_eq "$?" "0" "service stop exits 0"
    wait_for "! z service list --no-formatting 2>/dev/null | grep -Eq '^$name[[:space:]]+running'" 15
    out=$(z service list --no-formatting 2>&1)
    assert_match "$out" "^$name[[:space:]]+stopped[[:space:]]" "list shows it stopped again"
    assert_true "[ -f '$def' ]" "stop keeps the definition"

    out=$(z service remove "$name" 2>&1)
    assert_eq "$?" "0" "service remove exits 0"
    assert_false "[ -f '$def' ]" "definition file is gone after remove"
    out=$(z service list --no-formatting 2>&1)
    assert_false "printf '%s' \"\$out\" | grep -q '^$name[[:space:]]'" "list no longer mentions it"

    kill_session_quietly "svc-$name"
}

# ---------------------------------------------------------------------------
# 2. restart_policy_backoff
# ---------------------------------------------------------------------------

test_restart_policy_backoff() {
    local name=e2eback out runs
    remove_service_quietly "$name"
    track_session "svc-$name"

    z service add --name "$name" --restart always --cwd "$ROOT" \
        -- sh -c 'echo run; sleep 1; exit 1' >/dev/null 2>&1
    assert_eq "$?" "0" "service add --restart always exits 0"

    # first run ~1s, exit, 1s backoff, second run ~1s, exit, 2s backoff, third run...
    # so two `run` lines are due after ~3s; give it 8s of slack.
    wait_for "[ \"\$(z service logs '$name' 2>/dev/null | grep -c '^run\$')\" -ge 2 ]" 12

    out=$(z service logs "$name" 2>&1)
    runs=$(printf '%s\n' "$out" | grep -c '^run$')
    if [ "$runs" -ge 2 ]; then
        ok "the command ran at least twice (saw $runs 'run' lines)"
    else
        fail "the command ran at least twice" "saw $runs 'run' lines" \
             "$(printf '%s\n' "$out" | sed 's/^/           | /' | head -30)"
    fi
    assert_contains "$out" "command exited" \
                    "scrollback is kept across restarts (restart separator present)"

    remove_service_quietly "$name"
    kill_session_quietly "svc-$name"
}

# ---------------------------------------------------------------------------
# 3. restart_policy_on_failure_exit_zero
# ---------------------------------------------------------------------------

test_restart_policy_on_failure_exit_zero() {
    local session=e2eonce dump count
    kill_session_quietly "$session"
    track_session "$session"

    z attach --create-background "$session" --restart on-failure \
        -- sh -c 'echo once; exit 0' >/dev/null 2>&1
    assert_eq "$?" "0" "attach --create-background --restart on-failure exits 0"

    wait_for "session_listed '$session'" 15
    assert_true "session_listed '$session'" "the background session exists"

    # A wrongly armed restart would fire after the 1s backoff; wait well past that.
    wait_for "z -s '$session' action dump-screen --full -p terminal_0 2>/dev/null | grep -q once" 15
    sleep 4

    dump=$(z -s "$session" action dump-screen --full -p terminal_0 2>&1)
    count=$(printf '%s\n' "$dump" | grep -c '\bonce\b')
    assert_eq "$count" "1" "the pane shows exactly one 'once' (a clean exit is held, not restarted)"
    assert_true "session_listed '$session'" "the session is still alive (the pane was held, not closed)"

    kill_session_quietly "$session"
}

# ---------------------------------------------------------------------------
# 4. freeze_thaw
# ---------------------------------------------------------------------------

tick_count() { # tick_count <service name>
    z service logs "$1" 2>/dev/null | grep -c '^tick'
}

test_freeze_thaw() {
    local name=e2efreeze session=svc-e2efreeze status a b c
    remove_service_quietly "$name"
    track_session "$session"

    z service add --name "$name" --restart no --cwd "$ROOT" \
        -- sh -c 'i=0; while true; do i=$((i+1)); echo tick; sleep 0.2; done' >/dev/null 2>&1
    assert_eq "$?" "0" "ticking service starts"

    wait_for "[ \"\$(tick_count '$name')\" -ge 3 ]" 20
    if [ "$(tick_count "$name")" -lt 1 ]; then
        fail "the ticking service produced output"
        remove_service_quietly "$name"; kill_session_quietly "$session"
        return
    fi

    status=$(z freeze "$session" --status 2>&1); local status_rc=$?
    # The one environmental reason to skip: the server could not make per-pane cgroups.
    # Any *other* non-zero exit is a real failure and must not be swallowed.
    if printf '%s' "$status" | grep -q 'no pane cgroups'; then
        skip "this host has no cgroup v2 delegation for the server's panes (freeze --status: $(printf '%s' "$status" | head -1))"
        remove_service_quietly "$name"; kill_session_quietly "$session"
        return
    fi
    assert_eq "$status_rc" "0" "freeze --status exits 0"
    assert_contains "$status" "running" "freeze --status reports the pane running"

    z freeze "$session" >/dev/null 2>&1
    assert_eq "$?" "0" "freeze exits 0"

    status=$(z freeze "$session" --status 2>&1)
    assert_contains "$status" "frozen" "freeze --status reports the pane frozen"

    # the server still has buffered PTY bytes to drain right after the freeze
    sleep 1
    a=$(tick_count "$name")
    sleep 3
    b=$(tick_count "$name")
    assert_eq "$b" "$a" "the tick count does not advance while frozen ($a -> $b)"

    z thaw "$session" >/dev/null 2>&1
    assert_eq "$?" "0" "thaw exits 0"

    status=$(z freeze "$session" --status 2>&1)
    assert_contains "$status" "running" "freeze --status reports the pane running again"

    wait_for "[ \"\$(tick_count '$name')\" -gt $b ]" 10
    c=$(tick_count "$name")
    assert_true "[ $c -gt $b ]" "ticks advance again after thaw ($b -> $c)"

    remove_service_quietly "$name"
    kill_session_quietly "$session"
}

# ---------------------------------------------------------------------------
# 5. upgrade_in_place
# ---------------------------------------------------------------------------

# Copy the binary the way a package manager installs one: write a new file next to the old
# one and rename over it. Overwriting in place is ETXTBSY for a running binary and would not
# produce the `(deleted)` marker the upgrade path keys on.
install_binary_copy() { # install_binary_copy <dest>
    cp -f "$BINARY" "$1.new" && mv -f "$1.new" "$1" && chmod +x "$1"
}

test_upgrade_in_place() {
    local session=e2eup copy="$ROOT/gezellij" spid child out rc newexe

    kill_session_quietly "$session"
    track_session "$session"

    install_binary_copy "$copy" || { fail "could not copy the binary to $copy"; return; }

    "$copy" attach --create-background "$session" --restart no -- sleep 100000 >/dev/null 2>&1
    assert_eq "$?" "0" "a session started from the copied binary"

    wait_for "session_listed '$session'" 20
    wait_for "server_pid_of '$session' >/dev/null" 20
    spid=$(server_pid_of "$session")
    if [ -z "$spid" ]; then
        fail "the server recorded its pid next to the socket"
        kill_session_quietly "$session"; return
    fi
    track_pid "$spid"
    ok "server pid recorded: $spid"

    wait_for "pgrep -P '$spid' -x sleep >/dev/null" 20
    child=$(pgrep -P "$spid" -x sleep | head -1)
    if [ -z "$child" ]; then
        fail "the pane's child process is a child of the server"
        kill_session_quietly "$session"; return
    fi
    ok "pane child pid: $child"

    # This is what `pacman -Syu` does to the file.
    install_binary_copy "$copy" || { fail "could not replace the binary copy"; return; }
    assert_match "$(exe_of "$spid")" ' \(deleted\)$' \
                 "/proc/<server>/exe is marked (deleted) after the replacement"

    out=$("$copy" upgrade-server --all 2>&1); rc=$?
    assert_eq "$rc" "0" "upgrade-server --all exits 0"

    assert_eq "$(server_pid_of "$session")" "$spid" "the server pid is unchanged"
    assert_true "[ -d /proc/$spid ]" "the server process is still alive"

    newexe=$(exe_of "$spid")
    assert_not_contains "$newexe" "(deleted)" \
                        "/proc/<server>/exe no longer ends in '(deleted)' ($newexe)"

    assert_true "[ -d /proc/$child ]" "the pane's child process ($child) is still alive"
    assert_eq "$(ppid_of "$child")" "$spid" "the child is still a child of the same server"

    wait_for "session_listed '$session'" 20
    assert_true "session_listed '$session'" "the session still lists after the upgrade"

    out=$("$copy" upgrade-server --all 2>&1); rc=$?
    assert_eq "$rc" "0" "a second upgrade-server --all exits 0"
    assert_contains "$out" "Nothing to upgrade" "the second run says there is nothing to upgrade"

    kill_session_quietly "$session"
    rm -f "$copy"
}

# ---------------------------------------------------------------------------
# 6. upgrade_failure_is_safe
# ---------------------------------------------------------------------------

test_upgrade_failure_is_safe() {
    local session=e2eupfail copy="$ROOT/gezellij-gone" spid child out rc start elapsed

    kill_session_quietly "$session"
    track_session "$session"

    install_binary_copy "$copy" || { fail "could not copy the binary to $copy"; return; }

    "$copy" attach --create-background "$session" --restart no -- sleep 100000 >/dev/null 2>&1
    assert_eq "$?" "0" "a session started from the copied binary"

    wait_for "server_pid_of '$session' >/dev/null" 20
    spid=$(server_pid_of "$session")
    if [ -z "$spid" ]; then
        fail "the server recorded its pid next to the socket"
        kill_session_quietly "$session"; return
    fi
    track_pid "$spid"

    wait_for "pgrep -P '$spid' -x sleep >/dev/null" 20
    child=$(pgrep -P "$spid" -x sleep | head -1)
    if [ -z "$child" ]; then
        fail "the pane's child process is a child of the server"
        kill_session_quietly "$session"; return
    fi

    # The binary the server runs is deleted and never replaced: the exec must fail and the
    # old server must carry on untouched.
    rm -f "$copy"
    assert_match "$(exe_of "$spid")" ' \(deleted\)$' "/proc/<server>/exe is marked (deleted)"

    start=$SECONDS
    # run with the *original* binary: the copy no longer exists
    out=$(timeout 25 "$BINARY" upgrade-server "$session" --timeout 10 2>&1); rc=$?
    elapsed=$(( SECONDS - start ))

    if [ "$rc" = 124 ]; then
        fail "upgrade-server returns within ~10s" "it was still running after 25s (timeout(1) killed it)"
    elif [ "$rc" = 0 ]; then
        fail "upgrade-server exits non-zero when the new binary is missing" \
             "it exited 0 after ${elapsed}s" "$(printf '%s\n' "$out" | sed 's/^/           | /')"
    else
        ok "upgrade-server exits non-zero ($rc) after ${elapsed}s"
    fi
    assert_true "[ $elapsed -le 15 ]" "it gave up within ~10s (took ${elapsed}s)"

    if printf '%s' "$out" | grep -Eqi 'could not|cannot|failed|no such file|timed out|error'; then
        ok "it printed a reason: $(printf '%s\n' "$out" | grep -Eim1 'could not|cannot|failed|no such file|timed out|error')"
    else
        fail "upgrade-server printed a reason for the failure" \
             "$(printf '%s\n' "$out" | sed 's/^/           | /')"
    fi

    assert_eq "$(server_pid_of "$session")" "$spid" "the server pid is unchanged"
    assert_true "[ -d /proc/$spid ]" "the server is still alive after the failed upgrade"
    assert_true "[ -d /proc/$child ]" "the pane's child is still alive"
    assert_eq "$(ppid_of "$child")" "$spid" "the child is still a child of the same server"

    kill_session_quietly "$session"
    rm -f "$copy"
}

# ---------------------------------------------------------------------------
# 7. net_addresses
# ---------------------------------------------------------------------------

test_net_addresses() {
    local name=e2enet out prefix addr listed caddy
    remove_service_quietly "$name"
    track_session "svc-$name"

    out=$(z service net-setup 2>&1)
    assert_eq "$?" "0" "service net-setup exits 0"
    prefix=$(printf '%s\n' "$out" | sed -n 's/^Prefix:[[:space:]]*//p' | head -1)
    assert_match "$prefix" '^fd[0-9a-f]{2}:' "the generated prefix is inside fd00::/8 ($prefix)"
    assert_contains "$prefix" "/64" "the prefix is a /64"

    out=$(z service add --name "$name" --bind-ip --cwd "$ROOT" \
              -- sh -c 'env | grep GEZELLIJ_BIND; sleep 300' 2>&1)
    assert_eq "$?" "0" "service add --bind-ip exits 0"
    addr=$(printf '%s\n' "$out" | sed -n 's/.*own address: \[\([0-9a-f:]*\)\].*/\1/p' | head -1)
    assert_match "$addr" '^fd[0-9a-f]{2}:' "add printed the service's own address ($addr)"
    if [ -z "$addr" ]; then
        fail "could not parse the address out of:" "$(printf '%s\n' "$out" | sed 's/^/           | /')"
        remove_service_quietly "$name"; return
    fi

    listed=$(z service list --no-formatting 2>&1)
    assert_contains "$listed" "$addr" "service list shows the address"

    wait_for "z service logs '$name' 2>/dev/null | grep -q GEZELLIJ_BIND_ADDR" 20
    out=$(z service logs "$name" 2>&1)
    assert_contains "$out" "GEZELLIJ_BIND_ADDR=$addr" \
                    "the pane's environment carries GEZELLIJ_BIND_ADDR with that address"

    caddy=$(z service net-export --format caddy 2>&1)
    assert_eq "$?" "0" "service net-export --format caddy exits 0"
    assert_contains "$caddy" "[$addr]" "the caddy export contains the bracketed address"
    assert_contains "$caddy" "$name.localhost" "the caddy export names the service"

    remove_service_quietly "$name"
    kill_session_quietly "svc-$name"
}

# ---------------------------------------------------------------------------
# 8. systemd_unit_export
# ---------------------------------------------------------------------------

test_systemd_unit_export() {
    local name=e2esystemd unit
    remove_service_quietly "$name"

    z service add --name "$name" --no-start --cwd "$ROOT" -- ./serve --flag >/dev/null 2>&1
    assert_eq "$?" "0" "service add --no-start exits 0"

    unit=$(z service export-systemd "$name" 2>&1)
    assert_eq "$?" "0" "service export-systemd exits 0"
    assert_contains "$unit" "Type=simple" "the unit is Type=simple"
    assert_contains "$unit" "Restart=on-failure" "the unit restarts on failure"
    assert_match "$unit" "^ExecStart=.* service run $name\$" \
                 "ExecStart runs \`service run $name\`"
    assert_false "printf '%s' \"\$unit\" | grep -q '^After=default.target'" \
                 "the unit has no After=default.target"

    remove_service_quietly "$name"
}

# ---------------------------------------------------------------------------
# client helpers (multi-client tab sizing -- see GEZELLIJ_CLIENTS.md)
#
# These tests need several clients attached at once on terminals of *different sizes*, which
# means real ptys with a winsize we choose. `script` inherits its size from its own stdin, so
# it is no use here; a ten line python helper that sets TIOCSWINSZ in the child before exec is.
# ---------------------------------------------------------------------------

PTY_HELPER="$ROOT/tmp/pty_client.py"
GHOST_HELPER="$ROOT/tmp/pty_ghost.py"

write_pty_helpers() {
    [ -f "$PTY_HELPER" ] && return 0
    cat > "$PTY_HELPER" <<'PY'
# pty_client.py ROWS COLS PIDFILE OUTFILE FIFO -- cmd args...
# Runs cmd on a pty of exactly ROWS x COLS, records its pid, drains everything it prints into
# OUTFILE, and types anything that appears on FIFO into it.
import fcntl, os, pty, select, struct, sys, termios
rows, cols = int(sys.argv[1]), int(sys.argv[2])
pidfile, outfile, fifo = sys.argv[3], sys.argv[4], sys.argv[5]
assert sys.argv[6] == "--"
cmd = sys.argv[7:]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.execvp(cmd[0], cmd)
    os._exit(127)
with open(pidfile, "w") as f:
    f.write(str(pid))
fifo_fd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
keepalive = os.open(fifo, os.O_WRONLY)  # so the fifo never reports EOF between keystrokes
out = open(outfile, "wb", buffering=0)
while True:
    try:
        ready, _, _ = select.select([fd, fifo_fd], [], [], 0.2)
    except OSError:
        break
    if fd in ready:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        out.write(data)
    if fifo_fd in ready:
        try:
            keys = os.read(fifo_fd, 4096)
        except OSError:
            keys = b""
        if keys:
            os.write(fd, keys)
    try:
        if os.waitpid(pid, os.WNOHANG)[0] == pid:
            break
    except ChildProcessError:
        break
PY
    cat > "$GHOST_HELPER" <<'PY'
# pty_ghost.py ROWS COLS PIDFILE DRAIN_SECONDS -- cmd args...
# Like pty_client.py, but stops reading the pty after DRAIN_SECONDS and never reads it again.
# That is what a client whose terminal emulator went away looks like from the inside: its pty
# buffer fills and any further write to stdout blocks forever.
import fcntl, os, pty, struct, sys, termios, time
rows, cols, pidfile, drain = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], float(sys.argv[4])
assert sys.argv[5] == "--"
cmd = sys.argv[6:]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.execvp(cmd[0], cmd)
    os._exit(127)
with open(pidfile, "w") as f:
    f.write(str(pid))
deadline = time.time() + drain
while time.time() < deadline:
    try:
        if not os.read(fd, 65536):
            break
    except OSError:
        break
time.sleep(600)
PY
    return 0
}

# attach_client <tag> <rows> <cols> <session> -- attaches a client, returns once the server
# lists it. Sets CLIENT_PID. Keystrokes go to "$ROOT/tmp/<tag>.fifo".
CLIENT_PID=""
attach_client() {
    local tag=$1 rows=$2 cols=$3 session=$4
    local fifo="$ROOT/tmp/$tag.fifo" pidfile="$ROOT/tmp/$tag.pid" before
    before=$(client_count "$session")
    rm -f "$fifo" "$pidfile"
    mkfifo "$fifo" || return 1
    python3 "$PTY_HELPER" "$rows" "$cols" "$pidfile" "$ROOT/tmp/$tag.out" "$fifo" \
            -- "$BINARY" attach "$session" >/dev/null 2>&1 &
    # a debug-build client takes its time over the terminal handshake
    wait_for "[ -s '$pidfile' ]" 15 || return 1
    CLIENT_PID=$(cat "$pidfile")
    # wait for *this* client, not merely for some client: the session may already have others
    wait_for "[ \"\$(client_count '$session')\" -gt $before ]" 40 || return 1
    return 0
}

# type_at <tag> <string> -- send keystrokes to one attached client
type_at() { printf '%s' "$2" > "$ROOT/tmp/$1.fifo"; }

# client_count <session>
client_count() { z --session "$1" action list-clients 2>/dev/null | tail -n +2 | grep -c . ; }

# client_line <session> <client_id>
client_line() { z --session "$1" action list-clients 2>/dev/null | awk -v id="$2" '$1 == id || $1 == id"*"'; }

# Client ids are assigned (and recycled) by the server, so a test must never assume that the
# first client it attached is client 1. Look them up by the one thing we do control: their size.
# client_line_by_size <session> <ROWSxCOLS> / client_id_by_size <session> <ROWSxCOLS>
client_line_by_size() { z --session "$1" action list-clients 2>/dev/null | awk -v s="$2" '$2 == s'; }
client_id_by_size() { client_line_by_size "$1" "$2" | awk '{gsub(/\*/, "", $1); print $1}'; }

# pane_rows <session> <pane_id> -- ROWS as reported by `list-panes --geometry`
pane_rows() { z --session "$1" action list-panes --geometry 2>/dev/null | awk -v p="$2" '$1 == p {print $(NF-1)}'; }
pane_cols() { z --session "$1" action list-panes --geometry 2>/dev/null | awk -v p="$2" '$1 == p {print $NF}'; }

kill_client_quietly() {
    local pid=$1
    [ -n "$pid" ] || return 0
    kill -9 "$pid" >/dev/null 2>&1
    return 0
}

# ---------------------------------------------------------------------------
# 9. client_sizes_and_kick
#
# The incident GEZELLIJ_CLIENTS.md is about: a second client on a smaller terminal silently
# caps the tab for everybody. `list-clients` must show who and how small, and `kick-client`
# must give the tab back.
# ---------------------------------------------------------------------------

test_client_sizes_and_kick() {
    local session=e2eclients big small out small_id
    write_pty_helpers
    track_session "$session"
    kill_session_quietly "$session"

    z attach --create-background "$session" --restart no -- sleep 100000 >/dev/null 2>&1
    wait_for "session_listed '$session'" 20 || { fail "session $session started"; return; }

    attach_client big 60 200 "$session" || { fail "big client attached"; return; }
    big=$CLIENT_PID
    assert_eq "$(pane_rows "$session" terminal_0)" "58" "one 60x200 client: the pane is 58 rows"

    attach_client small 20 80 "$session" || { fail "small client attached"; kill_client_quietly "$big"; return; }
    small=$CLIENT_PID
    wait_for "[ \"\$(pane_rows '$session' terminal_0)\" = 18 ]" 15
    assert_eq "$(pane_rows "$session" terminal_0)" "18" \
              "a second, smaller client shrinks the tab for everybody (the incident)"

    out=$(z --session "$session" action list-clients 2>&1)
    assert_contains "$out" "SIZE" "list-clients has a SIZE column"
    assert_contains "$out" "IDLE" "list-clients has an IDLE column"
    assert_true "[ -n \"\$(client_id_by_size '$session' 60x200)\" ]" \
                "the big client is listed at 60x200"
    small_id=$(client_id_by_size "$session" 20x80)
    assert_true "[ -n \"\$small_id\" ]" "the small client is listed at 20x80"
    assert_match "$(client_line_by_size "$session" 20x80)" "[0-9]+[smhd]" \
                 "the small client has an idle time"

    out=$(z --session "$session" action kick-client 99 2>&1)
    assert_contains "$out" "No client with id 99" "kicking an unknown id says so"

    # `kick-client` is attributed to the last client that typed, and refuses to kick it
    type_at big 'x'
    sleep 1
    out=$(z --session "$session" action kick-client "$(client_id_by_size "$session" 60x200)" 2>&1)
    assert_contains "$out" "Refusing to kick client" "kick-client refuses to kick you"
    assert_eq "$(client_count "$session")" "2" "...and nobody was disconnected by that"

    out=$(z --session "$session" action kick-client "$small_id" 2>&1)
    assert_contains "$out" "Disconnected client $small_id" "kick-client reports the disconnect"
    wait_for "[ \"\$(client_count '$session')\" = 1 ]" 15
    assert_eq "$(client_count "$session")" "1" "the kicked client is gone from list-clients"
    wait_for "[ \"\$(pane_rows '$session' terminal_0)\" = 58 ]" 15
    assert_eq "$(pane_rows "$session" terminal_0)" "58" "the tab grows back after the kick"
    wait_for "[ ! -d /proc/$small ]" 15
    assert_false "[ -d /proc/$small ]" "the kicked client's own process exits"

    kill_client_quietly "$big"
    kill_session_quietly "$session"
}

# ---------------------------------------------------------------------------
# 10. client_parking
#
# `park_inactive_clients_after`: the idle client steps aside on its own, and one keypress
# brings it straight back to the tab it was on.
# ---------------------------------------------------------------------------

test_client_parking() {
    local session=e2epark big small conf i
    write_pty_helpers
    track_session "$session"
    kill_session_quietly "$session"

    conf="$ROOT/tmp/park.kdl"
    cat > "$conf" <<'EOF'
park_inactive_clients_after "5s"
EOF

    z --config "$conf" attach --create-background "$session" --restart no -- sleep 100000 >/dev/null 2>&1
    wait_for "session_listed '$session'" 20 || { fail "session $session started"; return; }

    attach_client big 60 200 "$session" || { fail "big client attached"; return; }
    big=$CLIENT_PID
    attach_client small 20 80 "$session" || { fail "small client attached"; kill_client_quietly "$big"; return; }
    small=$CLIENT_PID

    wait_for "[ \"\$(pane_rows '$session' terminal_0)\" = 18 ]" 15
    assert_eq "$(pane_rows "$session" terminal_0)" "18" "both clients attached: the tab is capped at 20x80"

    # Keep the big client demonstrably present while the small one goes quiet.
    for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
        type_at big ' '
        sleep 1
        [ "$(pane_rows "$session" terminal_0)" = "58" ] && break
    done

    assert_eq "$(pane_rows "$session" terminal_0)" "58" \
              "the idle client is parked and the tab grows back on its own"
    assert_eq "$(client_count "$session")" "2" "parking disconnects nobody"
    assert_match "$(client_line_by_size "$session" 20x80)" "parked" \
                 "list-clients says the small client is parked"
    assert_contains "$(z --session "$session" action query-tab-names 2>&1)" "parked" \
                    "there is a tab called 'parked'"

    # ...and any keypress buys the trip home. (We keep tickling the big client throughout, so
    # that it does not get parked in turn while we are checking on the small one.)
    type_at small $'\r'
    for i in 1 2 3 4 5 6 7 8 9 10; do
        type_at big ' '
        [ "$(pane_rows "$session" terminal_0)" = "18" ] && break
        sleep 1
    done
    assert_eq "$(pane_rows "$session" terminal_0)" "18" \
              "one keypress puts the parked client back on its tab"
    assert_false "printf '%s' \"\$(client_line_by_size '$session' 20x80)\" | grep -q parked" \
                 "list-clients no longer calls the small client parked"
    assert_not_contains "$(z --session "$session" action query-tab-names 2>&1)" "parked" \
                        "the parked tab is cleaned up when the last client leaves it"

    kill_client_quietly "$big"
    kill_client_quietly "$small"
    kill_session_quietly "$session"
}

# ---------------------------------------------------------------------------
# 11. signalled_client_exits
#
# A client whose terminal has gone away blocks forever writing to its pty. Upstream's SIGTERM
# handling asks the server to detach and then waits for a shutdown that can never arrive, so
# the process lingers until SIGKILL - invisible to `list-clients`, still holding a pty.
# ---------------------------------------------------------------------------

test_signalled_client_exits() {
    local session=e2esignal ghost i
    write_pty_helpers
    track_session "$session"
    kill_session_quietly "$session"

    z attach --create-background "$session" --restart no -- sleep 100000 >/dev/null 2>&1
    wait_for "session_listed '$session'" 20 || { fail "session $session started"; return; }

    rm -f "$ROOT/tmp/ghost.pid"
    python3 "$GHOST_HELPER" 40 120 "$ROOT/tmp/ghost.pid" 20 \
            -- "$BINARY" attach "$session" >/dev/null 2>&1 &
    wait_for "[ -s '$ROOT/tmp/ghost.pid' ]" 15 || { fail "ghost client started"; return; }
    ghost=$(cat "$ROOT/tmp/ghost.pid")
    wait_for "[ \"\$(client_count '$session')\" = 1 ]" 40 || { fail "ghost client attached"; kill_client_quietly "$ghost"; return; }

    # let the helper stop draining, then generate enough output to block the client's stdout
    sleep 21
    for i in $(seq 40); do
        z --session "$session" action write-chars "padding line $i to fill the pty buffer" >/dev/null 2>&1
    done
    sleep 2
    assert_true "[ -d /proc/$ghost ]" "the ghost client is still running before the signal"

    kill -TERM "$ghost"
    wait_for "[ ! -d /proc/$ghost ] || [ \"\$(awk '{print \$3}' /proc/$ghost/stat 2>/dev/null)\" = Z ]" 20
    assert_true "[ ! -d /proc/$ghost ] || [ \"\$(awk '{print \$3}' /proc/$ghost/stat 2>/dev/null)\" = Z ]" \
                "a signalled client exits even with a blocked stdout"
    assert_eq "$(client_count "$session")" "0" "and it is gone from list-clients"

    kill_client_quietly "$ghost"
    kill_session_quietly "$session"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

if [ ${#SELECTED[@]} -eq 0 ]; then
    SELECTED=("${ALL_TESTS[@]}")
fi

for t in "${SELECTED[@]}"; do
    found=0
    for known in "${ALL_TESTS[@]}"; do
        [ "$t" = "$known" ] && found=1
    done
    if [ "$found" != 1 ]; then
        echo "unknown test: $t" >&2
        usage >&2
        exit 2
    fi
done

printf '%sGezellij e2e suite%s\n' "$C_BOLD" "$C_OFF"
printf '  binary:   %s\n' "$BINARY"
printf '  version:  %s\n' "$("$BINARY" --version 2>&1 | head -1)"
printf '  temp root: %s\n\n' "$ROOT"

for t in "${SELECTED[@]}"; do
    run_test "$t"
done

printf '%s%d passed, %d failed, %d skipped%s\n' \
    "$C_BOLD" "$PASSED" "$FAILED" "$SKIPPED" "$C_OFF"

[ "$FAILED" -eq 0 ]
