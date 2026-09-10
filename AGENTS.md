# Project instructions

## Launcher build after major changes

After a major implementation or integration change, do not stop after checks and tests. Build the release application and install it at the binary used by the local app launcher.

On this Linux/Caelestia workstation:

```bash
cargo build --release -p zeron
DEST="$(readlink -f "$HOME/.local/bin/zeron")"
install -m 755 target/release/zeron "${DEST}.new"
mv -f "${DEST}.new" "$DEST"
systemctl --user restart zeron.service
```

Verify that the launcher and running headless service use the new binary:

```bash
systemctl --user is-active zeron.service
PID="$(systemctl --user show -p MainPID --value zeron.service)"
readlink -f "$HOME/.local/bin/zeron"
readlink -f "/proc/$PID/exe"
sha256sum target/release/zeron "$HOME/.local/bin/zeron" "/proc/$PID/exe"
```

The three checksums must match. If the user service does not exist on another machine, install the release binary at the path resolved from the desktop entry's `Exec` command and verify that target instead.
