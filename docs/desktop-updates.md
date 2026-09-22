# Installing and updating Noches

The repository is [KldsSeeGhosts/noches](https://github.com/KldsSeeGhosts/noches). `dev` publishes Noches Dev, and `main` publishes Noches. Both applications have Settings → Updates and a Check for Updates application-menu action. Download the update while working, then select Install and restart after finishing active runs, closing terminals, and saving files.

## First installation

Existing source builds cannot receive this change through their old updater. Install one of the new packages once, after the first release workflow completes. Subsequent releases use the in-app updater.

Download a versioned installer from [GitHub Releases](https://github.com/KldsSeeGhosts/noches/releases). Releases named `noches-dev` and `noches-stable` hold machine-readable update metadata; choose a numbered version for an installer.

On macOS, open the DMG and drag Noches or Noches Dev to Applications. Eject the DMG and launch the installed app. These builds use ad-hoc signing by default and do not require a paid Apple Developer account to build or publish. macOS may require approval in System Settings → Privacy & Security before the first launch. Permission prompts can recur after ad-hoc-signed updates. Developer ID signing and notarization remain optional improvements when a paid account is available.

On Linux, extract the matching x86_64 or aarch64 tarball and run `bash install.sh` inside it. Python 3 is required by the installer. The app installs under your home directory, with no root privileges and no automatic daemon installation. Launch it from the desktop application menu or `~/.local/bin/noches` / `~/.local/bin/noches-dev`.

Linux release builds use Ubuntu 24.04. Other distributions need compatible glibc and graphics libraries. Packages include Chromium and require its system libraries and sandbox support; see [Linux browser dependencies](reference/linux-browser.md). macOS CI currently produces Apple silicon builds. Intel Mac release artifacts are not included.

## Channel separation and existing data

| | Noches | Noches Dev |
| --- | --- | --- |
| Source branch | `main` | `dev` |
| Channel | `stable` | `dev` |
| Data | `~/.noches` | `~/.noches-dev` |
| Linux application root | `~/.local/share/noches/app` | `~/.local/share/noches-dev/app` |
| IPC port | 27655 | 27656 |
| Optional Linux service | `noches.service` | `noches-dev.service` |
| macOS bundle ID | `io.github.kldsseeghosts.noches` | `io.github.kldsseeghosts.noches.dev` |

Ordinary source builds remain local builds, keep the existing `~/.zeron` data and IPC port, and do not install published updates. Your existing sessions are not deleted or automatically moved. The two new installed channels start with separate data directories.

To bring existing local data into one channel, quit all applications and stop the old daemon first. Make a backup of `~/.zeron`, then copy it into the chosen channel's data directory only if that destination is empty. Do not run two processes against the same data directory. Do not copy a synced device identity into multiple channels; reconnect or pair the other channel independently. Provider CLI credentials stay in the providers' own locations.

`NOCHES_DATA_DIR` overrides the data location. `ZERON_DATA_DIR` remains a compatibility fallback for existing scripts. Internal Rust crate and executable names still use `zeron`; public installer, launcher, menu, and bundle names use Noches.

## Release cadence

1. Merge a feature or fix into `dev`. The Noches releases workflow tests and packages Linux on both architectures and macOS on Apple silicon. It publishes a numbered prerelease, then advances the dev feed only when the complete platform set is available.
2. Promote through a `dev` → `main` PR. The same workflow publishes a stable build and advances only the stable feed.
3. In the installed app, open Settings → Updates. Checks also run after startup and every six hours, independently of account sign-in or remote-host connections.

Versions are CI-owned: stable `0.3.<workflow-run-number>` and development `0.3.<workflow-run-number>-dev.<attempt>`. The source commit appears on the Updates page and in the release manifest. A stable rerun may resume an unpublished draft but refuses to replace a completed release with different artifacts. Start a new workflow run for a new stable build.

Each channel uses a GitHub release containing `manifest.json`:

- `https://github.com/KldsSeeGhosts/noches/releases/download/noches-dev/manifest.json`
- `https://github.com/KldsSeeGhosts/noches/releases/download/noches-stable/manifest.json`

Manifests point to assets under immutable version tags. Publication is serialized per branch, rejects stale branch builds and older feed versions, and uploads all artifacts before advancing the feed. A workflow interrupted during replacement of a channel manifest may temporarily make checks fail; retrying the workflow repairs it. The installed app remains unchanged when a check fails.

The updater requires the Noches product ID, matching channel, valid newer version, HTTPS asset URL, exact size, and SHA-256 digest. There is no legacy upstream-feed fallback. HTTPS and the GitHub repository's access controls authenticate publication; manifests do not have a separate cryptographic signature. macOS verifies the staged bundle's code signature and bundle identifier, including ad-hoc signatures. This does not give an ad-hoc build a Developer ID identity or notarization ticket.

All release jobs use the normal GitHub Actions token. Apple credentials are optional. The previous Cloudflare release publishing path is removed, and the inherited deployment workflow is gated to the upstream owner so this fork cannot deploy into Zeron's infrastructure.

## Local packages

For a local build that can join the development update channel after installation:

```bash
NOCHES_CHANNEL=dev NOCHES_VERSION=0.3.0-dev.0 bash scripts/package-macos.sh
# Or on Linux:
NOCHES_CHANNEL=dev NOCHES_VERSION=0.3.0-dev.0 bash scripts/package-linux.sh
```

Install the resulting package from `target/package`. Future published dev versions will sort newer than this bootstrap version. `NOCHES_REPOSITORY` and `NOCHES_COMMIT` are compiled into packaged builds. `NOCHES_RELEASES_URL` is a runtime feed override for controlled testing; it must be an HTTPS base URL containing a valid channel manifest.

## Restart and recovery

The update screen keeps the downloaded version until restart. Close additional Noches windows before installing so their editors and local work can remain safe. Local engine checks refuse a restart while runs or terminals are active. A failed download or checksum leaves the installation intact. Save-file handling runs before replacement.

Linux retains an `app/previous` symlink. Quit the app and stop its optional service before manually restoring that version. macOS keeps a hidden `.Noches.app.old-<pid>` or `.Noches Dev.app.old-<pid>` bundle next to the installation. A failed bundle rename attempts to restore the old bundle. Old versions are retained for manual recovery; there is no automatic crash-detection rollback or old-version cleanup yet. Restoring an executable does not reverse a database migration.

Updating from a remote-connected window updates the local desktop only. If a Linux background service is running, return to a local window so its active work can be checked before restarting the service. Remote hosts are never implicitly upgraded.

## Mobile

This workflow distributes desktop applications only. iOS continues to build locally in Xcode. TestFlight and App Store distribution require a paid Apple Developer membership, so neither is required for desktop releases. Existing iOS identifiers and provisioning are unchanged by the desktop updater.
