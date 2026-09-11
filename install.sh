#!/usr/bin/env bash
# Build the selected release variant and install its binary, desktop entry,
# icon, and systemd user service. Mirrors the ProjectX dev/prod split.
#
# Variants:
#   dev:  cargo build --features dev -> zeron-dev  (debug profile; --release opts in)
#         app_id "zeron-dev", data dir ~/.zeron-dev, IPC 27655, zeron-dev.service
#   prod: cargo build --release -> zeron
#         app_id "zeron", data dir ~/.zeron, IPC 27654, zeron.service
#
# Variant is chosen by the checked-out branch: `dev` -> dev, clean `main` ->
# prod. Other branches or a detached HEAD require an explicit --dev.
#
# Dev installs the incremental debug build (target/debug) by default — thin LTO
# and symbol stripping are wasted on per-iteration rebuilds. Pass --release for
# a pre-merge optimized check. Prod is always release.
#
# Usage: ./install.sh [--dev | --prod] [--release] [--with-tests]
set -euo pipefail
cd "$(dirname "$0")"

TEMP_FILES=()
cleanup() {
    for f in "${TEMP_FILES[@]}"; do
        if [[ -e "$f" ]]; then
            rm -f "$f"
        fi
    done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

escape_desktop_exec() {
    local path="$1"
    if [[ ! "$path" =~ ^/[a-zA-Z0-9/._\ +%@:-]+$ ]]; then
        echo "error: HOME must be an absolute path using letters, digits, spaces, or /._+%@:-" >&2
        return 1
    fi
    printf '"%s"' "${path//%/%%}"
}

install_atomic() {
    local src="$1" dst="$2" mode="$3"
    local dir tmp
    dir="$(dirname "$dst")"
    mkdir -p "$dir"
    tmp="$(mktemp "${dir}/.$(basename "$dst").tmp.XXXXXX")"
    TEMP_FILES+=("$tmp")
    cp "$src" "$tmp"
    chmod "$mode" "$tmp"
    mv -f "$tmp" "$dst"
}

# Atomically repoint an app dir's `current` symlink (temp name + rename, like
# crates/update's swap — never a window with no `current`).
repoint_current() {
    local app_root="$1" target_dir="$2"
    local tmp
    tmp="$(mktemp -u "${app_root}/.current.XXXXXX")"
    ln -sfn "$target_dir" "$tmp"
    mv -fT "$tmp" "${app_root}/current"
}

variant=""
release=false
with_tests=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dev)
            if [[ "$variant" == "prod" ]]; then
                echo "error: conflicting options --dev and --prod" >&2
                exit 1
            fi
            variant="dev"
            shift
            ;;
        --prod)
            if [[ "$variant" == "dev" ]]; then
                echo "error: conflicting options --dev and --prod" >&2
                exit 1
            fi
            variant="prod"
            shift
            ;;
        --release)
            release=true
            shift
            ;;
        --with-tests)
            with_tests=true
            shift
            ;;
        -h|--help)
            echo "Usage: ./install.sh [--dev | --prod] [--release] [--with-tests]"
            exit 0
            ;;
        *)
            echo "error: unknown option '$1'" >&2
            exit 1
            ;;
    esac
done

current_branch="$(git branch --show-current 2>/dev/null || true)"

if [[ -z "$variant" ]]; then
    if [[ "$current_branch" == "dev" ]]; then
        variant="dev"
    elif [[ "$current_branch" == "main" ]]; then
        variant="prod"
    else
        echo "error: default install requires branch 'dev' or 'main'; use --dev on other branches or detached HEAD" >&2
        exit 1
    fi
fi

if [[ "$variant" == "prod" ]]; then
    release=true
    if [[ "$current_branch" != "main" ]]; then
        echo "error: production build is only allowed on clean main branch (current branch: ${current_branch:-detached})" >&2
        exit 1
    fi
    git_status="$(git status --porcelain)"
    if [[ -n "$git_status" ]]; then
        echo "error: production build requires a clean git working tree (uncommitted or untracked changes detected)" >&2
        exit 1
    fi
fi

if [[ "$release" == true ]]; then
    profile_args=(--release)
    target_profile="release"
else
    profile_args=()
    target_profile="debug"
fi

bin_dir="$HOME/.local/bin"
apps_dir="$HOME/.local/share/applications"
icons_dir="$HOME/.local/share/icons/hicolor"
systemd_dir="$HOME/.config/systemd/user"
version="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

if [[ "$variant" == "dev" ]]; then
    name="zeron-dev"
    data_dir="$HOME/.zeron-dev"
    app_root="$data_dir/app"
    ipc_port=27655
    app_id="zeron-dev"
    app_name="Noches dev"
    app_comment="Multi-device controller for coding agents (dev build)"
    unit="zeron-dev.service"
    feature_args=(--features dev)
else
    name="zeron"
    data_dir="$HOME/.zeron"
    app_root="$data_dir/app"
    ipc_port=27654
    app_id="zeron"
    app_name="Noches"
    app_comment="Multi-device controller for coding agents"
    unit="zeron.service"
    feature_args=()
fi

target_ver_dir="$app_root/$version"
target_bin="$target_ver_dir/zeron"
target_desktop="$apps_dir/$name.desktop"
target_unit="$systemd_dir/$unit"
icon_src_svg="dist/zeron.svg"
icon_src_png="dist/zeron.png"

exec_val="$(escape_desktop_exec "$bin_dir/$name")"
[[ -f "$icon_src_svg" ]] || { echo "error: missing $icon_src_svg" >&2; exit 1; }
[[ -f "$icon_src_png" ]] || { echo "error: missing $icon_src_png" >&2; exit 1; }
mkdir -p "$bin_dir" "$apps_dir" "$systemd_dir" "$app_root" \
    "$icons_dir/scalable/apps" "$icons_dir/1024x1024/apps"

# --- desktop entry ---
desktop_tmp="$(mktemp "${apps_dir}/.${name}.tmp.XXXXXX.desktop")"
TEMP_FILES+=("$desktop_tmp")
cat <<EOF > "$desktop_tmp"
[Desktop Entry]
Type=Application
Name=$app_name
GenericName=Coding Agent Controller
Comment=$app_comment
Exec=$exec_val %u
TryExec=$bin_dir/$name
Icon=$name
Terminal=false
Categories=Development;Utility;
Keywords=agent;claude;codex;ai;coding;
StartupWMClass=$app_id
MimeType=x-scheme-handler/zeron;
EOF
chmod 644 "$desktop_tmp"
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$desktop_tmp"
fi

# --- systemd user service ---
# ExecStart resolves through the `current` symlink; EnvironmentFile picks up
# the variant's own env (~/.zeron-dev/env for dev). ZERON_DATA_DIR and
# ZERON_IPC_PORT are baked in so the engine never touches the other variant's
# state even if the env file is empty.
unit_tmp="$(mktemp "${systemd_dir}/.${unit}.tmp.XXXXXX.service")"
TEMP_FILES+=("$unit_tmp")
cat <<EOF > "$unit_tmp"
[Unit]
Description=Noches native headless engine${variant:+ ($variant)}
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.local/bin/$name headless
Restart=on-failure
RestartSec=5
Environment=ZERON_DATA_DIR=$data_dir
Environment=ZERON_IPC_PORT=$ipc_port
EnvironmentFile=-$data_dir/env

[Install]
WantedBy=default.target
EOF
chmod 644 "$unit_tmp"

# --- build ---
if [[ "$with_tests" == true ]]; then
    cargo test --all-targets --locked "${profile_args[@]}" "${feature_args[@]}"
fi
cargo build --locked "${profile_args[@]}" -p zeron "${feature_args[@]}"

# --- install binary into versioned app dir, repoint current ---
mkdir -p "$target_ver_dir"
install_atomic "target/$target_profile/zeron" "$target_bin" 755
repoint_current "$app_root" "$target_ver_dir"
# PATH entry -> the variant's current build.
ln -sf "$target_ver_dir/zeron" "$bin_dir/$name"

# --- icons ---
install_atomic "$icon_src_svg" "$icons_dir/scalable/apps/$name.svg" 644
install_atomic "$icon_src_png" "$icons_dir/1024x1024/apps/$name.png" 644

mv -f "$desktop_tmp" "$target_desktop"
mv -f "$unit_tmp" "$target_unit"

# --- reload / enable ---
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$icons_dir" >/dev/null 2>&1 || true
fi
systemctl --user daemon-reload >/dev/null 2>&1 || true

echo "Installed $variant variant ($target_profile build):"
echo "  binary:  $bin_dir/$name -> $target_bin"
echo "  desktop: $target_desktop (app_id $app_id)"
echo "  icon:    $icons_dir/.../$name.{svg,png}"
echo "  service: $target_unit"
echo "  data:    $data_dir  ipc: $ipc_port"
echo
echo "The $unit service was not restarted. To pick up the new engine:"
echo "  systemctl --user restart $unit"
