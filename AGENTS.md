# Project instructions

## Dev and production builds

Two side-by-side variants exist, selected by a `dev` cargo feature and the
checked-out branch. Full contract: `docs/dev-prod-workflow.md`.

- `dev` is the development branch; `main` is the release branch. Do work on
  `dev`, merge to `main` via PR.
- dev: `cargo build --release --features dev` -> `zeron-dev` (app_id
  `zeron-dev`, data `~/.zeron-dev`, IPC `27655`, `zeron-dev.service`).
- prod: `cargo build --release` -> `zeron` (app_id `zeron`, data `~/.zeron`,
  IPC `27654`, `zeron.service`).
- **Variant rule:** on `dev`, build/verify the dev variant only; prod binary
  only on clean `main`. The distinct app_id + IPC port + data dir keep the two
  fully isolated — never let a dev engine touch `~/.zeron`.

## Launcher build after major changes

After a major change, install the selected variant (do not stop after tests):

```bash
./install.sh            # dev on `dev`, prod on clean `main`
systemctl --user restart zeron-dev.service   # or zeron.service for prod
```

`install.sh` lays down the binary at `~/.zeron{,-dev}/app/<ver>/zeron` behind a
`current` symlink, plus `~/.local/bin/zeron{,-dev}`, the desktop entry, icon,
and the systemd user unit. Swaps are atomic; running instances are not
restarted. Verify the launcher and running service use the new binary:

```bash
systemctl --user is-active zeron-dev.service
PID="$(systemctl --user show -p MainPID --value zeron-dev.service)"
readlink -f "$HOME/.local/bin/zeron-dev"
readlink -f "/proc/$PID/exe"
sha256sum target/release/zeron "$HOME/.local/bin/zeron-dev" "/proc/$PID/exe"
```

The three checksums must match. On a machine without the user service, install
the release binary at the path resolved from the desktop entry's `Exec`.
