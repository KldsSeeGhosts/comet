#!/usr/bin/env bash
# Fast dev loop: incremental debug build -> install zeron-dev -> restart the
# dev service -> relaunch the dev window. Prod is never touched.
#
# Usage:
#   ./dev.sh            build + install + restart service + relaunch window
#   ./dev.sh --no-launch    skip the window relaunch (service still restarts)
#   ./dev.sh --watch        rebuild + reinstall + restart on every source change
#
# --watch needs cargo-watch (`cargo install cargo-watch`); everything else runs
# on a plain checkout. For an optimized pre-merge check use
# `./install.sh --dev --release` instead.
set -euo pipefail
cd "$(dirname "$0")"

launch=true
watch=false
for arg in "$@"; do
    case "$arg" in
        --no-launch) launch=false ;;
        --watch) watch=true ;;
        -h|--help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *) echo "error: unknown option '$arg'" >&2; exit 1 ;;
    esac
done

if [[ "$watch" == true ]]; then
    command -v cargo-watch >/dev/null 2>&1 || {
        echo "error: --watch needs cargo-watch (cargo install cargo-watch)" >&2
        exit 1
    }
    exec cargo watch -w crates -w apps \
        -s "$(printf '%q ' "$0" --no-launch)"
fi

./install.sh --dev
systemctl --user restart zeron-dev.service

if [[ "$launch" == true ]]; then
    # Drop the old dev window, then relaunch. Match on the dev app_id only —
    # never kill production `zeron` windows. Terminate by pid: this setup's
    # hyprctl dispatch shim rejects `closewindow address:...`, and a surviving
    # window keeps the control-plane instance lock, wedging the relaunched
    # UI's API bind.
    if command -v hyprctl >/dev/null 2>&1; then
        hyprctl clients -j 2>/dev/null \
            | jq -r '.[] | select(.class == "zeron-dev") | .pid' \
            | while read -r pid; do
                [[ -n "$pid" ]] && kill -TERM "$pid" 2>/dev/null || true
            done
        for _ in 1 2 3 4 5; do
            hyprctl clients -j 2>/dev/null | jq -e 'any(.class == "zeron-dev")' >/dev/null 2>&1 || break
            sleep 0.4
        done
    fi
    gtk-launch zeron-dev >/dev/null 2>&1 &
fi

echo "dev build installed; zeron-dev.service restarted"
