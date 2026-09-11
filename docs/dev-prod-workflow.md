# Dev and production workflow

Develop on `dev` while running your real agent sessions in production. The two
variants are fully isolated: installing or restarting dev never touches the
production binary, engine, chats, or windows.

| | Development | Production |
|---|---|---|
| Branch | `dev`, or `--dev` on a topic branch | clean `main` |
| Build | `cargo build --features dev` (debug; `--release` for pre-merge checks) | `cargo build --release` |
| Binary | `~/.local/bin/zeron-dev` → `~/.zeron-dev/app/<ver>/zeron` | `~/.local/bin/zeron` → `~/.zeron/app/<ver>/zeron` |
| Desktop ID | `zeron-dev` | `zeron` |
| Wayland app_id | `zeron-dev` | `zeron` |
| Data dir | `~/.zeron-dev` | `~/.zeron` |
| IPC port | `27655` | `27654` |
| systemd unit | `zeron-dev.service` | `zeron.service` |

The release branch is `main`. The `dev` cargo feature is the single switch:
`zeron-ui`'s `dev` feature sets `app_id = "zeron-dev"` (`crates/ui/src/lib.rs`
`APP_ID`), and `apps/zeron`'s `dev` feature moves the default data dir and IPC
port (`dirs_data_dir` / `default_ipc_port` in `apps/zeron/src/main.rs`).
`ZERON_DATA_DIR` / `ZERON_IPC_PORT` env still override the defaults.

## Daily development

```sh
git switch dev
./dev.sh                    # build -> install -> restart zeron-dev.service -> relaunch window
./dev.sh --watch            # same, on every source change (needs cargo-watch)
```

`./dev.sh` is the iteration loop. It installs the **debug** build — thin LTO
and symbol stripping belong to distribution, not to every edit. Deps still
compile at `opt-level = 2` (`[profile.dev.package."*"]`), so gpui stays fast;
only workspace crates rebuild unoptimized. `.cargo/config.toml` sets mold as
the linker and sccache as the rustc wrapper, so repeated and cross-worktree
builds stay cheap.

```sh
cargo check -p zeron --features dev      # fastest compile verification
cargo test --all-targets --features dev
cargo clippy --all-targets --features dev -- -D warnings
./install.sh --dev --release            # optimized pre-merge check
```

`./install.sh` selects the variant from the current branch; `--dev` permits
topic-branch work, `--prod` requires a clean `main`. Installs are atomic
(sibling temp file + rename; `current` symlink swapped via temp + `mv -T`), so
a running instance keeps its executable. The service is **not** restarted —
restart it explicitly when you want the new engine.

Verify the dev window's identity before sending it input:

```sh
hyprctl clients -j | jq '.[] | select(.class | startswith("zeron")) | {address, pid, class}'
```

Use the dev window's exact `address`. Never `pkill zeron` — that can kill the
production window and its live agent sessions. Both engines may run at once
(`zeron.service` on :27654, `zeron-dev.service` on :27655); they share nothing.

## Promotion

1. Verify the installed dev build end to end (`./dev.sh`), then run one
   `./install.sh --dev --release` to catch release-only breakage (LTO
   monomorphization, stripped panics).
2. Commit on `dev`, push to `origin/dev`, open a PR `dev` → `main`.
3. Merge when the milestone lands. In a clean `main` checkout run
   `./install.sh --prod --with-tests`, then `systemctl --user restart
   zeron.service`.
4. `gtk-launch zeron` for the release smoke test. Existing production sessions
   stay on the old executable until deliberately closed.

Do not switch branches or clean the tree under another agent editing it; use a
separate clean release worktree while dev sessions are active.

## Upstream self-update

`zeron update` and the curl|sh installer target the **production** managed dir
(`~/.zeron/app`). The dev binary lives under `~/.zeron-dev` and is installed by
`./install.sh --dev` — it does not self-update and is never overwritten by a
production update. Dev stays source-built; prod can update itself.
