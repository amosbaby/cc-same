#!/bin/bash
# One-step, locked-down setup of the cc-same command-line tool on macOS (Linux works too).
#
#   ./scripts/secure-setup.sh                      # pick the orgs from a list
#   ./scripts/secure-setup.sh --org ACCT/ORG --org ACCT/ORG   # name them (full ids or unique prefixes)
#   ./scripts/secure-setup.sh --org ACCT/ORG --org ACCT/ORG --yes   # no questions
#
# What it does, in order:
#   1. builds and installs `cc-same` from this checkout with the locked dependency versions
#      (no GUI app, no auto-update, no account switching: those are not compiled in);
#   2. lists the account/org folders on this computer with their full ids;
#   3. limits syncing to exactly the orgs you choose (`config --only`), so any other account
#      or org, including ones you sign in to later, stays separate;
#   4. waits for Claude to quit, shows the plan, syncs once (a baseline snapshot is taken
#      first and kept forever), and starts the background agent.
#
# Undo at any time: `cc-same uninstall` (every org keeps its full copy of the sessions).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ORGS=()
YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        --org) ORGS+=("$2"); shift 2 ;;
        --yes|-y) YES=1; shift ;;
        -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
ask() {
    [ "$YES" = 1 ] && return 0
    local reply
    read -r -p "$1 [y/N] " reply </dev/tty
    case "$reply" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

# ---------------------------------------------------------------- 1. build and install
command -v cargo >/dev/null 2>&1 || die "Rust is not installed. Install it first (https://rustup.rs, or: brew install rustup && rustup-init), then run this again."
say "Building cc-same from $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'this checkout') (locked dependencies)"
if [ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]; then
    echo "note: this checkout has local changes; they are built as they are."
fi
cargo install --locked --force --path "$ROOT/crates/cli" --bin cc-same
BIN="$(command -v cc-same || true)"
[ -n "$BIN" ] || BIN="${CARGO_HOME:-$HOME/.cargo}/bin/cc-same"
[ -x "$BIN" ] || die "cc-same was built but is not on PATH; add ${CARGO_HOME:-$HOME/.cargo}/bin to PATH"
"$BIN" --version
if "$BIN" --help | grep -qE '^  (switch|add) '; then
    die "this cc-same has account switching compiled in; rebuild without --features account-switching"
fi

# ---------------------------------------------------------------- 2. choose the orgs
say "Account/org folders on this computer"
LIST="$("$BIN" orgs)"
echo "$LIST" | nl -w2 -s'. '
IDS=()
while IFS= read -r line; do
    id="${line%% *}"
    case "$id" in */*) IDS+=("$id") ;; esac
done <<EOF
$LIST
EOF
[ "${#IDS[@]}" -ge 2 ] || die "fewer than two account/org folders found. Open Claude, use each org once (start any session), then run this again."

if [ "${#ORGS[@]}" -eq 0 ]; then
    [ "$YES" = 0 ] || die "--yes needs --org for each org to sync"
    echo
    echo "Which orgs should share one session list? Type their numbers, separated by spaces (at least two)."
    read -r -p "> " picks </dev/tty
    for n in $picks; do
        case "$n" in ''|*[!0-9]*) die "not a number: $n" ;; esac
        [ "$n" -ge 1 ] && [ "$n" -le "${#IDS[@]}" ] || die "no row $n"
        ORGS+=("${IDS[$((n - 1))]}")
    done
fi
[ "${#ORGS[@]}" -ge 2 ] || die "choose at least two orgs"
for o in "${ORGS[@]}"; do
    # An account id alone would take in every org of that account, including later ones.
    case "$o" in */*) ;; *) die "--org takes <account>/<org> (a unique prefix of each is enough): $o" ;; esac
done

# ---------------------------------------------------------------- 3. lock the config down
say "Limiting sync to the chosen orgs; every other account and org stays separate"
ARGS=(config --clear-only --auto-join off --notify on)
for o in "${ORGS[@]}"; do ARGS+=(--only "$o"); done
"$BIN" "${ARGS[@]}" >/dev/null
"$BIN" orgs

# ---------------------------------------------------------------- 4. first sync and agent
claude_running() {
    if [ "$(uname)" = Darwin ]; then pgrep -xq Claude; else pgrep -x claude-desktop >/dev/null 2>&1 || pgrep -x Claude >/dev/null 2>&1; fi
}
if claude_running; then
    say "Quit Claude now (Cmd+Q) after its running tasks finish; waiting for it to close (Ctrl+C to stop)"
    while claude_running; do sleep 2; done
fi

say "What the first sync will do"
"$BIN" plan
ask "Sync now and keep these orgs in sync in the background?" || { echo "Stopped. Nothing was synced; the settings above are saved."; exit 0; }
"$BIN" sync --yes
"$BIN" install --yes
say "Done"
"$BIN" status
cat <<EOF

Open Claude again. Switch orgs in Claude as usual; the background agent keeps both lists the same.
  Check:     cc-same doctor        cc-same orgs
  Snapshots: cc-same snapshots     cc-same restore <id>   (quit Claude first)
  Stop:      cc-same uninstall     (each org keeps its full copy)
EOF
