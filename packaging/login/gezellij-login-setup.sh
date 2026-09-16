#!/usr/bin/env bash
#
# gezellij-login-setup — make Gezellij greet you at login, safely and reversibly.
#
# It does three things, all of them undoable:
#   1. neutralises existing auto-start setups (byobu / tmux / screen) by
#      *commenting them out* with a marker, never deleting anything;
#   2. appends a clearly marked block to your shell rc file that starts
#      Gezellij for interactive terminal sessions;
#   3. records what it did in a state file so `undo` can put everything back.
#
# Safety rules it follows:
#   * every file it edits is backed up first to <file>.gezellij-backup-<stamp>;
#   * edits go to a temp copy which is then mv'd over the *resolved* path, so a
#     symlinked rc file (stow/chezmoi/dotfiles repos) stays a symlink and no
#     file is ever left half-written;
#   * only unindented, uncommented, non-heredoc lines are touched; anything
#     else that looks suspicious is reported for you to review by hand;
#   * it never runs `byobu-disable` (that command *deletes* lines from your rc
#     files, which would defeat the backup/undo guarantee); it creates byobu's
#     own `disable-autolaunch` flag file instead.
#
set -euo pipefail

PROG="gezellij-login-setup"
VERSION="1"

MARK_BEGIN="# >>> gezellij login setup >>>"
MARK_END="# <<< gezellij login setup <<<"
DISABLED_PREFIX="# gezellij-login-setup disabled: "

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/gezellij"
STATE_FILE="$STATE_DIR/login-setup.env"
OPTOUT_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/gezellij/no-autostart"

ACTION="install"
DRY_RUN=0
OPT_SHELL=""
OPT_BINARY=""
SESSION="main"

# Collected during a run, for the final summary.
declare -a CHANGED_FILES=()
declare -a BACKUPS_MADE=()
declare -a DISABLED_LINES=()
declare -a WARN_LINES=()

die() { printf '%s: error: %s\n' "$PROG" "$*" >&2; exit 1; }
info() { printf '%s\n' "$*"; }
warn() { printf '%s: warning: %s\n' "$PROG" "$*" >&2; }

usage() {
    cat <<EOF
$PROG — make Gezellij start automatically at login (reversibly).

USAGE:
    $PROG [install] [options]
    $PROG undo [options]
    $PROG status [options]

COMMANDS:
    install     (default) neutralise byobu/tmux/screen auto-start, then add a
                marked Gezellij auto-start block to your shell rc file(s).
    undo        reverse everything install did: remove the block, un-comment
                the lines it disabled, remove byobu's disable flag. Backups are
                kept and their paths printed.
    status      show the binary, which rc files carry the block, which lines
                are currently disabled, and which backups exist.

OPTIONS:
    --shell zsh|bash|fish|all   Which shell(s) to configure.
                                Default: detected from \$SHELL, plus bash if
                                ~/.bashrc exists.
    --binary PATH               Gezellij binary to use.
                                Default: first of 'gezellij', 'zellij' on PATH.
    --session NAME              Session name to attach to/create (default: main).
    --dry-run                   Print what would happen; change nothing at all.
    -h, --help                  This text.

AFTER INSTALL, TO OPT OUT WITHOUT UNDOING:
    touch $OPTOUT_FILE
    # or: export GEZELLIJ_NO_AUTOSTART=1

The auto-start block also skips itself when the shell is non-interactive, when
\$TERM is unset/dumb/linux (a Linux VT console), when you are already inside
zellij/tmux/screen, when \$SSH_ORIGINAL_COMMAND is set (scp/rsync/git over ssh),
and when the binary is missing. ZELLIJ_AUTO_EXIT is forced to "false" in the block, so
quitting Gezellij leaves you in a normal shell instead of logging you out.
EOF
}

# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------
parse_args() {
    local first=1
    while [ $# -gt 0 ]; do
        case "$1" in
            install|undo|status)
                [ $first -eq 1 ] || die "unexpected argument: $1"
                ACTION="$1"
                ;;
            --shell)
                [ $# -ge 2 ] || die "--shell needs a value"
                OPT_SHELL="$2"; shift
                ;;
            --shell=*) OPT_SHELL="${1#--shell=}" ;;
            --binary)
                [ $# -ge 2 ] || die "--binary needs a value"
                OPT_BINARY="$2"; shift
                ;;
            --binary=*) OPT_BINARY="${1#--binary=}" ;;
            --session)
                [ $# -ge 2 ] || die "--session needs a value"
                SESSION="$2"; shift
                ;;
            --session=*) SESSION="${1#--session=}" ;;
            --dry-run) DRY_RUN=1 ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown argument: $1 (try --help)" ;;
        esac
        first=0
        shift
    done

    case "$OPT_SHELL" in
        ""|zsh|bash|fish|all) : ;;
        *) die "--shell must be one of: zsh bash fish all (got '$OPT_SHELL')" ;;
    esac
    case "$SESSION" in
        *[!A-Za-z0-9._-]*|"") die "--session must be a simple name (letters, digits, . _ -)" ;;
    esac
}

# ---------------------------------------------------------------------------
# discovery
# ---------------------------------------------------------------------------
find_binary() {
    local cand
    if [ -n "$OPT_BINARY" ]; then
        [ -x "$OPT_BINARY" ] || die "--binary '$OPT_BINARY' is not an executable file"
        # Absolutise: the path is baked into your rc file and must work from
        # any working directory at login time.
        readlink -f -- "$OPT_BINARY"
        return
    fi
    for cand in gezellij zellij; do
        if command -v "$cand" >/dev/null 2>&1; then
            readlink -f -- "$(command -v "$cand")"
            return
        fi
    done
    die "no 'gezellij' or 'zellij' binary found on PATH; install one or pass --binary PATH"
}

# Which shells to configure.
target_shells() {
    local out=""
    case "$OPT_SHELL" in
        all) out="zsh bash fish" ;;
        zsh|bash|fish) out="$OPT_SHELL" ;;
        "")
            case "${SHELL:-}" in
                */zsh) out="zsh" ;;
                */bash) out="bash" ;;
                */fish) out="fish" ;;
                *) out="" ;;
            esac
            # Also cover bash if the user has a .bashrc (very common even when
            # the login shell is zsh).
            if [ -f "$HOME/.bashrc" ] && [ "$out" != "bash" ]; then
                out="$out bash"
            fi
            [ -n "${out// /}" ] || die "could not detect your shell from \$SHELL; pass --shell zsh|bash|fish|all"
            ;;
    esac
    printf '%s' "$out"
}

rc_for_shell() {
    case "$1" in
        zsh)  printf '%s' "$HOME/.zshrc" ;;
        bash) printf '%s' "$HOME/.bashrc" ;;
        fish) printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
        *) die "internal: unknown shell '$1'" ;;
    esac
}

# Files scanned for existing byobu/tmux/screen auto-start lines.
scan_candidates() {
    printf '%s\n' \
        "$HOME/.profile" \
        "$HOME/.bashrc" \
        "$HOME/.bash_profile" \
        "$HOME/.zshrc" \
        "$HOME/.zprofile" \
        "$HOME/.zlogin" \
        "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
}

# ---------------------------------------------------------------------------
# file helpers
# ---------------------------------------------------------------------------
timestamp() { date +%Y%m%d-%H%M%S; }

backup_of() {
    # backup_of FILE -> unique backup path
    local f="$1" base stamp n
    stamp="$(timestamp)"
    base="$f.gezellij-backup-$stamp"
    if [ ! -e "$base" ]; then printf '%s' "$base"; return; fi
    n=1
    while [ -e "$base-$n" ]; do n=$((n + 1)); done
    printf '%s' "$base-$n"
}

make_backup() {
    local f="$1" b
    b="$(backup_of "$f")"
    if [ "$DRY_RUN" -eq 1 ]; then
        info "  would back up $f -> $b"
        return 0
    fi
    cp -p -- "$f" "$b"
    BACKUPS_MADE+=("$b")
    state_add "backup" "$b"
    info "  backed up $f -> $b"
}

# Replace FILE's contents with NEWFILE's, atomically, following symlinks and
# preserving mode/ownership of the original.
replace_file() {
    local target="$1" newcontent="$2" resolved dir tmp
    resolved="$(readlink -f -- "$target")"
    dir="$(dirname -- "$resolved")"
    tmp="$(mktemp "$dir/.gezellij-login-setup.XXXXXX")"
    # cp -p from the original first, so mode/owner/timestamps are inherited.
    cp -p -- "$resolved" "$tmp"
    cat -- "$newcontent" > "$tmp"
    mv -f -- "$tmp" "$resolved"
}

state_init() {
    [ "$DRY_RUN" -eq 1 ] && return 0
    mkdir -p -- "$STATE_DIR"
    : >> "$STATE_FILE"
}

state_add() {
    [ "$DRY_RUN" -eq 1 ] && return 0
    mkdir -p -- "$STATE_DIR"
    printf '%s=%s\n' "$1" "$2" >> "$STATE_FILE"
}

state_values() {
    # state_values KEY -> one value per line
    [ -f "$STATE_FILE" ] || return 0
    sed -n "s/^$1=//p" "$STATE_FILE"
}

# ---------------------------------------------------------------------------
# neutralising existing auto-start lines
# ---------------------------------------------------------------------------
# Writes the neutralised version of $1 to stdout. Reports what it did / what it
# refused to touch through the DISABLED_LINES / WARN_LINES arrays (via files,
# because this runs in a subshell when used in a pipeline — so it doesn't).
neutralise_stream() {
    local file="$1" hits="$2" warns="$3"
    local -a lines=() out=()
    local i n line trimmed heredoc="" heredoc_line=0 prev next
    local trailing_newline=1

    mapfile -t lines < "$file"
    n=${#lines[@]}
    if [ -s "$file" ] && [ "$(tail -c1 "$file" | wc -l)" -eq 0 ]; then
        trailing_newline=0
    fi

    for ((i = 0; i < n; i++)); do
        line="${lines[i]}"

        # Inside a heredoc: pass through verbatim, look for the terminator.
        if [ -n "$heredoc" ]; then
            out+=("$line")
            trimmed="${line#"${line%%[![:space:]]*}"}"
            [ "$trimmed" = "$heredoc" ] && heredoc=""
            continue
        fi

        # Already a comment (including our own marker)? Leave alone, and do not
        # let a commented-out `cat <<EOF` open a phantom heredoc.
        case "$line" in
            '#'*) out+=("$line"); continue ;;
        esac

        # Start of a heredoc? (` <<WORD`, ` <<-WORD`, ` <<'WORD'`, ` <<"WORD"`)
        # The leading whitespace requirement keeps `$((x<<n))` out, and the
        # mandatory word character right after `<<`/`<<-` keeps `<<<` here-
        # strings out.
        if [[ "$line" =~ (^|[[:space:]])\<\<-?[[:space:]]*[\'\"]?([A-Za-z_][A-Za-z0-9_]*) ]]; then
            heredoc="${BASH_REMATCH[2]}"
            heredoc_line=$((i + 1))
            out+=("$line")
            continue
        fi

        if ! matches_autostart "$line"; then
            out+=("$line")
            continue
        fi

        # From here on the line looks like an auto-start. We only ever rewrite
        # it when doing so cannot change the syntax or the meaning of its
        # neighbours; anything else is reported instead of touched.
        if ! safe_to_disable lines "$i"; then
            out+=("$line")
            printf '%s\t%s\n' "$file" "$line" >> "$warns"
            continue
        fi

        out+=("$DISABLED_PREFIX$line")
        printf '%s\t%s\n' "$file" "$line" >> "$hits"
    done

    if [ -n "$heredoc" ]; then
        printf '%s\t%s\n' "$file" \
            "(unterminated heredoc '$heredoc' opened on line $heredoc_line — the rest of this file was NOT scanned)" \
            >> "$warns"
    fi

    if [ "${#out[@]}" -gt 0 ]; then
        if [ "$trailing_newline" -eq 1 ]; then
            printf '%s\n' "${out[@]}"
        else
            printf '%s\n' "${out[@]:0:${#out[@]}-1}"
            printf '%s' "${out[${#out[@]}-1]}"
        fi
    fi
}

# safe_to_disable ARRAYNAME INDEX
# False (non-zero) when commenting the line out could break syntax or silently
# change what runs:
#   * the line is indented (inside something we do not understand);
#   * it is an alias/export/function definition rather than an invocation;
#   * it ends with a line continuation;
#   * the previous significant line ends with \ && || | then do else { ( — its
#     right-hand side would become a separate statement;
#   * the next significant line closes a block (fi done else elif esac } ) ;;)
#     which would leave that block empty -> a syntax error in your rc file.
safe_to_disable() {
    local -n _lines="$1"
    local idx="$2" line="${_lines[$2]}" j cand
    local total="${#_lines[@]}"

    case "$line" in
        [[:space:]]*) return 1 ;;
        alias\ *|export\ *|function\ *|*'()'*) return 1 ;;
        *\\) return 1 ;;
    esac

    # previous significant line
    for ((j = idx - 1; j >= 0; j--)); do
        cand="${_lines[j]}"
        [ -z "${cand//[[:space:]]/}" ] && continue
        case "$cand" in '#'*) continue ;; esac
        case "$cand" in
            *\\|*"&&"|*"||"|*"|"|*then|*do|*else|*"{"|*"(") return 1 ;;
        esac
        break
    done

    # next significant line
    for ((j = idx + 1; j < total; j++)); do
        cand="${_lines[j]}"
        [ -z "${cand//[[:space:]]/}" ] && continue
        cand="${cand#"${cand%%[![:space:]]*}"}"
        case "$cand" in
            fi|fi\ *|fi';'*|done|done\ *|else|else\ *|elif\ *|esac|'}'*|')'*|';;'*|end|end\ *) return 1 ;;
        esac
        break
    done

    return 0
}

matches_autostart() {
    local l="$1"
    case "$l" in
        *byobu-launch*) return 0 ;;
        *"tmux attach"*) return 0 ;;
        *"tmux new-session -A"*) return 0 ;;
        *"exec tmux"*) return 0 ;;
        *"tmux new -A"*) return 0 ;;
        *"exec screen"*) return 0 ;;
        *"screen -RR"*) return 0 ;;
        *"screen -xRR"*) return 0 ;;
    esac
    return 1
}

neutralise_file() {
    local file="$1" tmpout hits warns n
    [ -f "$file" ] || return 0

    tmpout="$(mktemp)"; hits="$(mktemp)"; warns="$(mktemp)"
    neutralise_stream "$file" "$hits" "$warns" > "$tmpout"

    while IFS=$'\t' read -r f l; do
        [ -n "${f:-}" ] || continue
        WARN_LINES+=("$f: $l")
    done < "$warns"

    n="$(wc -l < "$hits" | tr -d ' ')"
    if [ "$n" -gt 0 ]; then
        info "disabling $n auto-start line(s) in $file"
        while IFS=$'\t' read -r f l; do
            [ -n "${f:-}" ] || continue
            DISABLED_LINES+=("$f: $l")
            info "  - $l"
        done < "$hits"
        if [ "$DRY_RUN" -eq 1 ]; then
            info "  (dry run: not modified)"
        else
            make_backup "$file"
            replace_file "$file" "$tmpout"
            CHANGED_FILES+=("$file")
            state_add "neutralised" "$file"
        fi
    fi
    rm -f -- "$tmpout" "$hits" "$warns"
}

# ---------------------------------------------------------------------------
# byobu
# ---------------------------------------------------------------------------
byobu_config_dir() {
    # Mirrors /usr/lib/byobu/include/dirs.
    if [ -n "${BYOBU_CONFIG_DIR:-}" ]; then
        printf '%s' "$BYOBU_CONFIG_DIR"
    elif [ -d "$HOME/.byobu" ]; then
        printf '%s' "$HOME/.byobu"
    else
        printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/byobu"
    fi
}

disable_byobu_autolaunch() {
    command -v byobu-launch >/dev/null 2>&1 || return 0
    local dir flag
    dir="$(byobu_config_dir)"
    flag="$dir/disable-autolaunch"
    if [ -e "$flag" ]; then
        info "byobu autolaunch already disabled ($flag exists, left as is)"
        return 0
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        info "would create byobu autolaunch flag $flag"
        return 0
    fi
    mkdir -p -- "$dir"
    : > "$flag"
    state_add "byobu_flag" "$flag"
    info "created byobu autolaunch flag $flag"
    # NOTE: we deliberately do NOT run `byobu-disable`. On Arch it calls
    # byobu-launcher-uninstall, which `sed -i`-deletes the launcher line from
    # your rc files (and can quit a running byobu session). Deleting lines
    # would break the verbatim-restore guarantee; the flag file above is the
    # same switch byobu-launch itself checks.
}

# ---------------------------------------------------------------------------
# the auto-start block
# ---------------------------------------------------------------------------
# The upstream snippet (`<bin> setup --generate-auto-start <shell>`) hardcodes
# the command name `zellij` and, for bash/zsh, contains NO interactivity guard
# at all — it only checks $ZELLIJ / $ZELLIJ_AUTO_ATTACH / $ZELLIJ_AUTO_EXIT.
# So we (a) wrap it in our own guards and (b) rewrite the two invocation lines
# to our binary and session. The rewrite happens at shell start-up so the block
# keeps following upstream if the binary is updated.
rewrite_snippet() {
    # rewrite_snippet BIN SHELL -> rewritten snippet on stdout
    "$1" setup --generate-auto-start "$2" | sed \
        -e 's|^[[:space:]]*zellij attach -c$|"$GEZELLIJ_BIN" attach -c "$GEZELLIJ_SESSION"|' \
        -e 's|^[[:space:]]*zellij$|"$GEZELLIJ_BIN"|'
}

verify_snippet() {
    # Fail loudly if upstream changed the snippet in a way our sed no longer
    # covers — otherwise we would silently launch the *system* zellij.
    local bin="$1" sh="$2" out
    out="$(rewrite_snippet "$bin" "$sh" || true)"
    [ -n "$out" ] || die "'$bin setup --generate-auto-start $sh' produced no output"
    if printf '%s\n' "$out" | grep -v '^[[:space:]]*#' | grep -qE '(^|[^-[:alnum:]_/])zellij([^-[:alnum:]_]|$)'; then
        die "the auto-start snippet for $sh still refers to the bare command 'zellij' after rewriting.
Upstream must have changed it. Refusing to install a block that would start the
wrong binary. Please report this; nothing was changed."
    fi
}

block_posix() {
    # block_posix SHELL BIN  (bash or zsh)
    local sh="$1" bin="$2" interactive_guard
    if [ "$sh" = "zsh" ]; then
        interactive_guard='[[ -o interactive ]]'
    else
        interactive_guard='[[ $- == *i* ]]'
    fi
    cat <<EOF
$MARK_BEGIN
# Added by $PROG (v$VERSION). Remove with: $PROG undo
GEZELLIJ_BIN="$bin"
GEZELLIJ_SESSION="$SESSION"
# Attach to (or create) the named session.
export ZELLIJ_AUTO_ATTACH=true
# ZELLIJ_AUTO_EXIT is forced OFF (it is *not* merely left unset: it may already
# be exported as "true" in your environment, and upstream's snippet would then
# close the shell — i.e. log you out — when you quit Gezellij).
export ZELLIJ_AUTO_EXIT=false
if $interactive_guard \\
   && [ -z "\${ZELLIJ:-}" ] && [ -z "\${TMUX:-}" ] && [ -z "\${STY:-}" ] \\
   && [ -z "\${SSH_ORIGINAL_COMMAND:-}" ] \\
   && [ -z "\${GEZELLIJ_NO_AUTOSTART:-}" ] \\
   && [ ! -e "\${XDG_CONFIG_HOME:-\$HOME/.config}/gezellij/no-autostart" ] \\
   && [ -n "\${TERM:-}" ] && [ "\$TERM" != "dumb" ] && [ "\$TERM" != "linux" ] \\
   && [ -x "\$GEZELLIJ_BIN" ]; then
    printf '%s\\n' "gezellij: session '\$GEZELLIJ_SESSION' (opt out: touch ~/.config/gezellij/no-autostart)"
    _gezellij_auto_start() {
        "\$GEZELLIJ_BIN" setup --generate-auto-start $sh | sed \\
            -e 's|^[[:space:]]*zellij attach -c\$|"\$GEZELLIJ_BIN" attach -c "\$GEZELLIJ_SESSION"|' \\
            -e 's|^[[:space:]]*zellij\$|"\$GEZELLIJ_BIN"|'
    }
    eval "\$(_gezellij_auto_start)"
    unset -f _gezellij_auto_start
fi
$MARK_END
EOF
}

block_fish() {
    local bin="$1"
    cat <<EOF
$MARK_BEGIN
# Added by $PROG (v$VERSION). Remove with: $PROG undo
set -g GEZELLIJ_BIN "$bin"
set -g GEZELLIJ_SESSION "$SESSION"
set -gx ZELLIJ_AUTO_ATTACH true
# Forced off, not just unset: an inherited ZELLIJ_AUTO_EXIT=true would make
# upstream's snippet kill the shell when you quit Gezellij.
set -gx ZELLIJ_AUTO_EXIT false
if status is-interactive
    and not set -q ZELLIJ; and not set -q TMUX; and not set -q STY
    and not set -q SSH_ORIGINAL_COMMAND; and not set -q GEZELLIJ_NO_AUTOSTART
    and not test -e "\$XDG_CONFIG_HOME/gezellij/no-autostart"
    and not test -e "\$HOME/.config/gezellij/no-autostart"
    and set -q TERM; and test "\$TERM" != "dumb"; and test "\$TERM" != "linux"
    and test -x "\$GEZELLIJ_BIN"
    echo "gezellij: session \$GEZELLIJ_SESSION (opt out: touch ~/.config/gezellij/no-autostart)"
    eval (\$GEZELLIJ_BIN setup --generate-auto-start fish | sed -e 's|^[[:space:]]*zellij attach -c\$|"\$GEZELLIJ_BIN" attach -c "\$GEZELLIJ_SESSION"|' -e 's|^[[:space:]]*zellij\$|"\$GEZELLIJ_BIN"|' | string collect)
end
$MARK_END
EOF
}

block_for() {
    case "$1" in
        fish) block_fish "$2" ;;
        *) block_posix "$1" "$2" ;;
    esac
}

has_block() {
    [ -f "$1" ] && grep -qF "$MARK_BEGIN" "$1"
}

install_block() {
    local sh="$1" bin="$2" rc tmp
    rc="$(rc_for_shell "$sh")"

    if [ "$DRY_RUN" -eq 1 ]; then
        if has_block "$rc"; then
            info "would replace the existing gezellij block in $rc"
        else
            info "would append the gezellij block to $rc"
        fi
        return 0
    fi

    mkdir -p -- "$(dirname -- "$rc")"
    if [ ! -e "$rc" ]; then
        : > "$rc"
        state_add "created" "$rc"
        info "created $rc"
    fi

    make_backup "$rc"
    tmp="$(mktemp)"
    if has_block "$rc"; then
        # Idempotent: replace what is between the markers so that a changed
        # --session / --binary takes effect without duplicating the block.
        awk -v b="$MARK_BEGIN" -v e="$MARK_END" '
            index($0, b) == 1 { skip = 1; next }
            index($0, e) == 1 { skip = 0; next }
            !skip { print }
        ' "$rc" > "$tmp"
        # Drop a trailing blank run left behind by the removal.
        block_for "$sh" "$bin" >> "$tmp"
        info "replaced the gezellij block in $rc"
    else
        cat -- "$rc" > "$tmp"
        # Ensure the file ends with a newline before appending. Remember that we
        # did, so `undo` can restore the file byte-for-byte.
        if [ -s "$tmp" ] && [ "$(tail -c1 "$tmp" | wc -l)" -eq 0 ]; then
            printf '\n' >> "$tmp"
            state_add "added_newline" "$rc"
        fi
        printf '\n' >> "$tmp"
        block_for "$sh" "$bin" >> "$tmp"
        info "appended the gezellij block to $rc"
    fi
    replace_file "$rc" "$tmp"
    rm -f -- "$tmp"
    CHANGED_FILES+=("$rc")
    state_add "block" "$rc"
}

remove_block() {
    local rc="$1" tmp
    has_block "$rc" || return 1
    tmp="$(mktemp)"
    awk -v b="$MARK_BEGIN" -v e="$MARK_END" '
        index($0, b) == 1 { skip = 1; blank = 1; next }
        index($0, e) == 1 { skip = 0; next }
        !skip { print }
    ' "$rc" > "$tmp"
    # install() inserts exactly one blank separator line before the block, so
    # remove exactly one trailing blank line — not all of them (a file that
    # ended in two newlines must stay that way).
    if [ -s "$tmp" ] && [ -z "$(tail -n 1 "$tmp")" ]; then
        head -n -1 -- "$tmp" > "$tmp.trim"
        mv -f -- "$tmp.trim" "$tmp"
    fi
    replace_file "$rc" "$tmp"
    rm -f -- "$tmp"
    return 0
}

trim_final_newline() {
    local f="$1" tmp
    tmp="$(mktemp)"
    printf '%s' "$(cat -- "$f")" > "$tmp"
    replace_file "$f" "$tmp"
    rm -f -- "$tmp"
}

restore_disabled_lines() {
    local file="$1" tmp n
    [ -f "$file" ] || return 1
    grep -qF "$DISABLED_PREFIX" "$file" || return 1
    n="$(grep -cF "$DISABLED_PREFIX" "$file")"
    tmp="$(mktemp)"
    # Strip exactly our prefix; the remainder is the original line, verbatim.
    sed "s|^$DISABLED_PREFIX||" "$file" > "$tmp"
    replace_file "$file" "$tmp"
    rm -f -- "$tmp"
    info "  re-enabled $n line(s) in $file"
    return 0
}

# ---------------------------------------------------------------------------
# commands
# ---------------------------------------------------------------------------
cmd_install() {
    local bin shells sh f
    bin="$(find_binary)"
    shells="$(target_shells)"
    info "$PROG: install"
    info "  binary : $bin"
    info "  shells : $shells"
    info "  session: $SESSION"
    [ "$DRY_RUN" -eq 1 ] && info "  MODE   : dry run, nothing will be changed"
    info ""

    for sh in $shells; do
        verify_snippet "$bin" "$sh"
    done

    state_init
    state_add "run" "$(date -Is 2>/dev/null || date) action=install version=$VERSION"
    state_add "binary" "$bin"
    state_add "session" "$SESSION"

    while IFS= read -r f; do
        neutralise_file "$f"
    done < <(scan_candidates)
    disable_byobu_autolaunch

    for sh in $shells; do
        install_block "$sh" "$bin"
    done

    info ""
    if [ ${#WARN_LINES[@]} -gt 0 ]; then
        warn "these lines look like auto-start setups but were left ALONE"
        warn "(indented, a definition, or in a position where commenting them out"
        warn "could change your shell's syntax or semantics). Review them yourself —"
        warn "they run BEFORE the gezellij block, so a tmux/screen started there will"
        warn "pre-empt Gezellij (the \$TMUX/\$STY guard then skips it):"
        for f in "${WARN_LINES[@]}"; do printf '    %s\n' "$f" >&2; done
        info ""
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        info "Dry run complete. Nothing was changed."
    else
        info "Done. Open a new terminal to try it."
        info "Opt out temporarily : touch $OPTOUT_FILE"
        info "Undo everything     : $0 undo"
    fi
}

cmd_undo() {
    local rc did=0 f
    info "$PROG: undo"
    [ "$DRY_RUN" -eq 1 ] && info "  MODE: dry run, nothing will be changed"
    info ""

    # Undo is marker-based rather than "restore the newest backup", so that any
    # edits you made to the rc file after installing are preserved. The backups
    # are kept and listed below as a manual fallback.
    while IFS= read -r rc; do
        [ -f "$rc" ] || continue
        if has_block "$rc"; then
            if [ "$DRY_RUN" -eq 1 ]; then
                info "  would remove the gezellij block from $rc"
            else
                remove_block "$rc" && info "  removed the gezellij block from $rc"
                # If install had to add a missing final newline, take it back.
                if state_values added_newline | grep -qxF "$rc" \
                   && [ -s "$rc" ] && [ "$(tail -c1 "$rc" | wc -l)" -eq 1 ]; then
                    trim_final_newline "$rc"
                    info "  restored the missing final newline state of $rc"
                fi
            fi
            did=1
        fi
        if grep -qF "$DISABLED_PREFIX" "$rc"; then
            if [ "$DRY_RUN" -eq 1 ]; then
                info "  would re-enable $(grep -cF "$DISABLED_PREFIX" "$rc") line(s) in $rc"
            else
                restore_disabled_lines "$rc"
            fi
            did=1
        fi
    done < <(scan_candidates)

    while IFS= read -r f; do
        [ -n "$f" ] || continue
        # An rc file that did not exist before install and is empty again now.
        if [ -f "$f" ] && [ ! -s "$f" ]; then
            if [ "$DRY_RUN" -eq 1 ]; then
                info "  would remove the empty $f (created by install)"
            else
                rm -f -- "$f"
                info "  removed the empty $f (created by install)"
            fi
            did=1
        fi
    done < <(state_values created)

    while IFS= read -r f; do
        [ -n "$f" ] || continue
        if [ -e "$f" ]; then
            if [ "$DRY_RUN" -eq 1 ]; then
                info "  would remove byobu flag $f"
            else
                rm -f -- "$f"
                info "  removed byobu flag $f (byobu autolaunch re-enabled)"
            fi
            did=1
        fi
    done < <(state_values byobu_flag)

    if [ "$DRY_RUN" -eq 0 ]; then
        state_add "run" "$(date -Is 2>/dev/null || date) action=undo"
    fi

    info ""
    if [ "$did" -eq 0 ]; then
        info "Nothing to undo — no gezellij block or disabled line found."
    else
        info "Undo complete."
    fi
    local backups
    backups="$(state_values backup || true)"
    if [ -n "$backups" ]; then
        info ""
        info "Backups are kept (delete them yourself when happy):"
        printf '%s\n' "$backups" | while IFS= read -r b; do
            [ -e "$b" ] && printf '    %s\n' "$b"
        done
    fi
}

cmd_status() {
    local bin rc f n
    info "$PROG: status"
    if bin="$(find_binary 2>/dev/null)"; then
        info "  binary      : $bin"
    else
        info "  binary      : NONE FOUND (neither gezellij nor zellij on PATH)"
    fi
    info "  state file  : $STATE_FILE$([ -f "$STATE_FILE" ] || printf ' (absent)')"
    if [ -f "$STATE_FILE" ]; then
        info "  session     : $(state_values session | tail -n1)"
    fi
    info "  opt-out file: $OPTOUT_FILE$([ -e "$OPTOUT_FILE" ] && printf ' (PRESENT — autostart is off)' || printf ' (absent)')"

    info ""
    info "  rc files with the gezellij block:"
    n=0
    while IFS= read -r rc; do
        if has_block "$rc"; then info "    $rc"; n=$((n + 1)); fi
    done < <(scan_candidates)
    [ "$n" -eq 0 ] && info "    (none)"

    info ""
    info "  lines currently disabled by $PROG:"
    n=0
    while IFS= read -r rc; do
        [ -f "$rc" ] || continue
        while IFS= read -r l; do
            [ -n "$l" ] || continue
            info "    $rc: ${l#"$DISABLED_PREFIX"}"
            n=$((n + 1))
        done < <(grep -F "$DISABLED_PREFIX" "$rc" || true)
    done < <(scan_candidates)
    [ "$n" -eq 0 ] && info "    (none)"

    info ""
    info "  byobu autolaunch flag:"
    f="$(byobu_config_dir)/disable-autolaunch"
    if [ -e "$f" ]; then info "    $f (present)"; else info "    (none)"; fi

    info ""
    info "  backups:"
    n=0
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        if [ -e "$f" ]; then info "    $f"; n=$((n + 1)); fi
    done < <(state_values backup)
    [ "$n" -eq 0 ] && info "    (none recorded)"
}

main() {
    parse_args "$@"
    case "$ACTION" in
        install) cmd_install ;;
        undo)    cmd_undo ;;
        status)  cmd_status ;;
    esac
}

main "$@"
