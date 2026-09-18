# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/zeron-<version>-linux-<arch>.tar.gz` containing:

- `zeron` — the binary (headed by default; `zeron headless` runs the engine alone)
- `zeron.desktop` — XDG desktop entry
- `zeron.png` — 1024×1024 Zeron app icon
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/noches-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Noches.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. Bundle identity: `app.noches.desktop`, internal executable `zeron`, icon
`noches.icns`, and the `noches://` deep-link scheme. The legacy `zeron://`
scheme stays declared so links copied by earlier builds keep opening. The
auto-update tarball (`noches-<version>-macos-<arch>-app.tar.gz`) contains
`Noches.app` at the archive root, which is the path the updater swaps in;
`scripts/run-macos-dev.sh` builds the isolated `Noches Dev.app` under
`app.noches.desktop.dev`. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host - gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p zeron --target aarch64-apple-darwin
   cargo build --release -p zeron --target x86_64-apple-darwin
   lipo -create -output zeron \
     target/aarch64-apple-darwin/release/zeron \
     target/x86_64-apple-darwin/release/zeron
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Noches.app/Contents/{MacOS,Resources}
   cp zeron Noches.app/Contents/MacOS/zeron
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Noches.app/Contents/Info.plist
   ```
3. Icon: generate `noches.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork - squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Noches.app/Contents/Resources/noches.icns`:
   ```sh
   mkdir noches.iconset && sips -z 256 256 dist/macos/icon-1024.png --out noches.iconset/icon_256x256.png
   iconutil -c icns noches.iconset -o Noches.app/Contents/Resources/noches.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Noches.app
   xcrun notarytool submit Noches.zip --keychain-profile … --wait
   xcrun stapler staple Noches.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Noches -srcfolder Noches.app -ov -format UDZO Noches.dmg`).
