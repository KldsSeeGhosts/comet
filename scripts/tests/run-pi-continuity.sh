#!/usr/bin/env bash
# Compile normally, then run only this ignored test in a fresh credential-free process.
set -euo pipefail
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
[[ "$(git branch --show-current)" == dev ]] || { printf '%s\n' 'Run the dev variant on branch dev.' >&2; exit 1; }
real_pi="$(readlink -f -- "${REAL_PI_EXECUTABLE:-$(command -v pi)}")"
real_node="$(readlink -f -- "${REAL_PI_NODE:-$(command -v node)}")"
[[ -f "$real_pi" && -x "$real_node" ]] || { printf '%s\n' 'Installed Pi and Node are required.' >&2; exit 1; }
root="$(mktemp -d /tmp/zeron-pi-continuity.XXXXXX)"
chmod 700 "$root"
mkdir -p "$root/home" "$root/agent" "$root/engine" "$root/tmp"
printf 'Pi continuity artifacts: %s\n' "$root"
# No inherited provider keys, proxies, Node preload hooks, global Pi resources,
# or user sessions. Explicit bundled engine -e arguments still reach real Pi.
{
    printf '#!/bin/bash\nset -eu\nroot=%q\n' "$root"
    printf 'printf "pid=%%s cwd=%%q argv=" "$$" "$PWD" >> "$root/invocations.log"\n'
    printf 'printf "%%q " "$@" >> "$root/invocations.log"\nprintf "\\n" >> "$root/invocations.log"\n'
    printf 'exec /usr/bin/env -i HOME="$root/home" PI_CODING_AGENT_DIR="$root/agent" PI_CODING_AGENT_SESSION_DIR="$root/agent/sessions" TMPDIR="$root/tmp" PATH=%q TERM=xterm-256color LANG=C.UTF-8 PI_SKIP_VERSION_CHECK=1 PI_OFFLINE=1 PI_TELEMETRY=0 NOCHES_HOOK_CONTEXT="${NOCHES_HOOK_CONTEXT:-}" ' "$(dirname "$real_node"):/usr/bin:/bin"
    printf '%q %q --no-extensions --no-skills --no-prompt-templates --no-themes --no-context-files --no-tools "$@"\n' "$real_node" "$real_pi"
} > "$root/pi-wrapper"
chmod 700 "$root/pi-wrapper"
"$root/pi-wrapper" --version | tee "$root/pi-version.txt"
printf 'Pi CLI: %s\nNode: %s\n' "$real_pi" "$real_node" > "$root/executables.txt"
sha256sum "$real_pi" "$real_node" >> "$root/executables.txt"
cargo test -p zeron-engine --features dev --test pi_continuity --no-run --message-format=json > "$root/build.jsonl" 2> >(tee "$root/build.log" >&2)
binary="$(python3 -c 'import json,sys; items=[json.loads(line) for line in open(sys.argv[1])]; print(next(x["executable"] for x in items if x.get("reason")=="compiler-artifact" and x.get("target",{}).get("name")=="pi_continuity" and x.get("executable")))' "$root/build.jsonl")"
printf 'Test binary: %s\n' "$binary" >> "$root/executables.txt"
env -i PATH="$(dirname "$real_node"):/usr/bin:/bin" HOME="$root/home" \
    PI_CODING_AGENT_DIR="$root/agent" ZERON_DATA_DIR="$root/engine" \
    PI_EXECUTABLE="$root/pi-wrapper" PI_CONTINUITY_ROOT="$root" \
    PI_SKIP_VERSION_CHECK=1 PI_OFFLINE=1 PI_TELEMETRY=0 LANG=C.UTF-8 \
    RUST_BACKTRACE=1 "$binary" --ignored --exact installed_pi_chat_cli_chat_continuity --nocapture \
    2>&1 | tee "$root/test.log"
