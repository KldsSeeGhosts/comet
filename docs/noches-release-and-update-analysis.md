# Noches naming, releases, and desktop updates

Implementation follow-up: see [desktop updates](desktop-updates.md) for the current behavior. The user later confirmed there is no paid Apple Developer account; both channels therefore support ad-hoc macOS signing, and Developer ID signing is optional. The repository has since been renamed to `KldsSeeGhosts/noches`.

Audit date: 2026-09-22. Source reviewed: `dev` at `4ac9880d`. GitHub API and remote refs confirmed `main` at `148ffb46`. This document records current behavior and a proposed implementation. It does not change repository settings, release feeds, installed applications, or production infrastructure.

Noches already has much of the updater needed for the requested experience. Keep the Rust updater and GPUI interface, give Noches its own release identity, and complete the Linux desktop installation path. Both desktop platforms should use the same channel policy: published builds from `dev` update Noches Dev, and published builds from `main` update Noches.

An installed application should fetch a release manifest and compiled artifacts. GitHub Actions builds those artifacts from the appropriate branch. Installed applications do not need Git or a source checkout.

The following facts were verified against the checkout and GitHub.

| Area | Current state | Consequence |
| --- | --- | --- |
| Repository | Public fork `KldsSeeGhosts/comet`; GitHub parent is now `zeronsh/zeron` | Local repository name, product name, and upstream branding differ |
| Remotes | `origin` points to the personal `comet` fork; `upstream` still uses `zeronsh/comet` | Update the upstream URL to its canonical name when doing the naming cleanup |
| GitHub CLI | No default repository configured; an unqualified `gh repo view` resolved to upstream | Set the fork explicitly as the CLI default and use explicit repository arguments in automation |
| Branches | Default branch is `main`; `dev` is 13 commits ahead with no commits unique to `main` | The requested integration and promotion branches already exist |
| PR cadence | Feature/fix PRs target `dev`; earlier PRs #2, #3, and #4 promoted `dev` to `main` | Preserve this workflow and connect publication to it |
| Protection | GitHub reports `protected: false` for both branches; repository rulesets response is empty | Required checks and PR-only integration are not currently enforced |
| Releases | GitHub returned zero releases and no runs for `release.yml` in this fork | No published Noches update channel exists here yet |
| Version | Workspace version is `0.2.72-noches.1` | Noches currently inherits upstream's version base |
| Branding | Window title says Noches, but README, Cargo packages, binary, desktop launcher, macOS bundles, services, and iOS project retain Zeron names | Renaming the repository alone will not fix installed application identity |

Source locations: [`Cargo.toml`](../Cargo.toml), [`README.md`](../README.md), [`apps/zeron/src/main.rs`](../apps/zeron/src/main.rs), [`crates/ui/src/lib.rs`](../crates/ui/src/lib.rs), and [`dist`](../dist).

The release workflow only publishes on `v*` tag pushes. Manual runs build CI artifacts without publishing a release. The publishing job waits for Linux x86_64 and aarch64, macOS Apple silicon, Windows x86_64, and Cursor compatibility. It publishes a GitHub Release and optionally copies artifacts to the inherited Cloudflare R2 bucket, `comet-native-releases`. There is no branch-to-channel policy or tag ancestry check. A tag name alone does not establish that its commit was approved on `main`.

The workflow still contains the upstream Cloudflare account and deployment destinations. The edge and landing deployment workflows also target Zeron infrastructure. No secrets were read or changed during this audit; availability of usable signing and hosting credentials is unverified. Noches release automation must have its own destinations before publication is enabled.

See [`release.yml`](../.github/workflows/release.yml), [`deploy.yml`](../.github/workflows/deploy.yml), and [`edge/wrangler.jsonc`](../edge/wrangler.jsonc).

The updater has these useful pieces today:

- Background checks start after 20 seconds, repeat every six hours, and retry after failures in 30 minutes.
- A release manifest lists a version and artifact SHA-256 checksums. Downloads stream to disk and reject mismatched hashes when a checksum exists.
- macOS stages an app bundle, displays a restart action, replaces the installed bundle, and launches the replacement after exit. A failed second rename attempts to restore the old bundle.
- Managed Linux installations stage a versioned directory and atomically switch a `current` symlink. The CLI and engine can restart the service afterward.
- Headless automatic updates defer installation while the engine reports active sessions or terminals.

The gaps that block the requested behavior are concrete:

1. **The default feed belongs to upstream.** `release_base()` uses the configured edge URL plus `/releases`, and the application defaults to `https://edge.zeron.sh`. `ZERON_RELEASES_URL` can override it, but no compiled Noches channel exists. A managed fork installation using these defaults can offer an upstream Zeron build as its next update.
2. **Version ordering discards the fork/dev suffix.** `version_newer()` treats `0.2.72-noches.1` and `0.2.72-noches.2` as equal. The same issue affects `0.3.0-dev.1` and `0.3.0-dev.2`.
3. **Linux desktop installs are unmanaged.** The tarball's bundled installer copies the binary to `~/.local/bin/zeron`. Detection recognizes only executables under `~/.zeron/app` as managed. Even managed Linux installs return false from `supports_desktop_update()`, so the sidebar only advises running `zeron update`.
4. **Checking is not a complete user-facing flow.** The sidebar appears when an update is available. There is no manual Check for Updates action found in the UI, no byte progress, and no persistent place to see up-to-date or check-failed states. Remote-connected windows hide the strip entirely.
5. **The macOS developer bundle is not a dev release channel.** The local script isolates its bundle identifier, data directory, and IPC port, but uses the same binary version and default update feed. Bundle detection still classifies it as an updatable macOS application.
6. **Verification has a legacy bypass.** Missing manifests can fall back to `latest.txt`, and missing artifact checksums permit unverified downloads. There is no signed Noches manifest or product/channel check. Existing macOS swap recovery does not retain a backup through successful relaunch.
7. **Restart protection differs by path.** The desktop update path checks unsaved editors before exit. The headless auto-update path separately checks active runs and terminals. The desktop flow needs an explicit policy for those active processes too.

See [`crates/update/src/lib.rs`](../crates/update/src/lib.rs), [`crates/ui/src/shell.rs`](../crates/ui/src/shell.rs), [`apps/zeron/src/update_cli.rs`](../apps/zeron/src/update_cli.rs), [`scripts/package-linux.sh`](../scripts/package-linux.sh), and [`scripts/run-macos-dev.sh`](../scripts/run-macos-dev.sh).

I recommend this target layout. Names and paths below are proposed, not existing configuration.

| Property | Production | Development |
| --- | --- | --- |
| Repository | `KldsSeeGhosts/noches` | Same repository |
| Source branch | `main` | `dev` |
| Compiled channel | `stable` | `dev` |
| Application name | Noches | Noches Dev |
| macOS bundle | `Noches.app` | `Noches Dev.app` |
| CLI/launcher | `noches` | `noches-dev` |
| Linux desktop file | `noches.desktop` | `noches-dev.desktop` |
| Data root | `~/.noches` | `~/.noches-dev` |
| Linux managed application root | `~/.local/share/noches/app` | `~/.local/share/noches-dev/app` |
| Optional user service | `noches.service` | `noches-dev.service` |
| Release example | `v0.3.0` | `v0.4.0-dev.123` |
| GitHub release classification | Stable release | Prerelease |

Choose durable reverse-DNS bundle identifiers under an identity you control, with `.dev` for development. Isolate IPC endpoints, engine locks, staging directories, preferences, authentication storage, protocol handlers, and device/pairing identities as well as the visible app name. Existing `ZERON_DATA_DIR` overrides and the local macOS dev launcher provide useful starting points, but environment variables should not be required for normal installed apps.

Compile product, channel, version, build sequence, commit SHA, and update-feed location into each distributable. A Rust debug/release optimization profile is independent of the update channel. Source builds should identify as local builds and avoid offering installation from a published feed by default. A packaged Noches Dev build should be optimized and update normally.

Use real SemVer ordering, with increasing numeric prerelease components for dev builds. Keep the commit SHA in a separate metadata field; SemVer build metadata does not establish update order. Start Noches versioning independently, for example at `0.3.0`, and record upstream baselines in development documentation. macOS bundle version fields should use Apple-compatible numeric versions, with the dev channel and commit displayed separately in the application.

For hosting, use GitHub Releases for immutable, versioned binaries and small separate manifests for `stable` and `dev`. A GitHub Pages project can serve both manifests without requiring a new domain, for example `https://kldsseeghosts.github.io/noches/updates/stable.json` and `dev.json`. This would be new Pages configuration. The manifests should link directly to assets under the exact release tag, not a moving `latest` artifact URL. A Noches-owned domain or CDN can front these feeds later.

Each signed manifest should identify the product, channel, version, monotonically increasing channel build sequence, source commit, publication time, release notes, and supported targets. Each target needs an immutable HTTPS asset URL, byte size, SHA-256, and installation format. Use a defined signed byte representation and embed the verification public key in the app. Checksums detect corruption; the signature authenticates the manifest. Clients must reject the wrong product, channel, architecture, missing verification metadata, and older builds.

The current manifest schema has only version and filename/checksum entries, so this is an explicit schema and client change. Publish a complete release before advancing its channel manifest. Serialize manifest publication, preserve the other channel's manifest, and reject an older build attempting to replace a newer channel pointer. Reruns should reuse an existing immutable release or get a new ordered build number, never silently replace its payload. Do not use GitHub's `/releases/latest` endpoint to discover dev prereleases.

The intended release sequence is:

1. Merge feature and upstream-sync PRs into `dev`. Run required tests against the actual merged commit, then build and publish a Noches Dev prerelease for macOS and Linux. Advance only the dev manifest after all required artifacts pass verification.
2. Promote through a `dev` to `main` PR. Run the same relevant tests and build stable-channel artifacts from the accepted main commit. Publish only when the stable version is new and matches the release metadata. Advance only the stable manifest.
3. Keep non-release maintenance merges on `main` from reusing an existing version. A version guard must explicitly skip publication or fail with a clear correction, rather than overwrite an existing release.
4. Require PRs and checks for both branches. Expand CI coverage to `dev`, release configuration, packaging scripts, and `apps/zeron/**`; the existing UI workflow's path filters omit that application directory. Gate publication on tests instead of assuming another workflow happened to pass.
5. Keep Windows CI as desired, but make Windows publication independent of the macOS/Linux release gate unless it becomes a supported Noches target. Current release publication waits for Windows too.

Stable and dev packages need different embedded identities, so promotion usually builds a stable package from approved source rather than relabeling the dev binary. Preserve merge ancestry between the two long-lived branches to keep later promotions reviewable. Keep upstream syncs as dedicated PRs into `dev`; retain attribution and licensing. Archive historical branches only after confirming their unique work is preserved. No branch deletion is needed for the updater.

On macOS, retain the existing DMG for first installation and app tarball for updates. Package the correct Noches bundle name, executable, identifier, and channel. Require Developer ID signing and notarization for distributed stable and dev packages; fail publication if required credentials are absent. Validate the staged bundle's identity and signature before replacement, retain the old version until a successful launch is recorded, and handle read-only DMG launches and unwritable installation directories explicitly. Current CI covers Apple silicon only. Intel support requires a separate tested target or a tested universal build.

On Linux, first standardize on a user-owned managed installation. The desktop entry and CLI should resolve through the channel's `app/current` symlink. Extend the existing versioned staging and swap implementation to support desktop installation, resource updates, and relaunch. Detect managed installations from explicit metadata and the install root, independently of the data directory. Ship dependency checks and document the supported distribution baseline; current builds use Ubuntu 24.04, and the embedded browser requires WebKitGTK 4.1 and JSON-GLib.

This approach fits the existing tarballs and gives the requested in-app experience without requiring root. AppImage can be an additional distribution format later. System-managed packages such as deb, rpm, or Flatpak should report updates through their package manager instead of overwriting package-owned files. Source installations should show rebuild instructions.

Provide an Updates page in Settings and a Check for Updates menu action on both desktop platforms. Show installed version, channel, commit, last check, and release notes. Use these states consistently:

`Check for updates → Checking → Up to date / Update available → Download → Download progress → Ready to restart → Install and restart`

Network or verification failures should leave the running installation intact and expose a retry action. Keep the sidebar notification as an additional entry point. Downloads can happen during work; installation should save editors and explicitly handle active local agents, terminals, and any shared engine service. Keep the previous executable/bundle for recovery, while recognizing that executable rollback does not automatically reverse a data migration.

Local application updates must remain available while connected to a remote host. Separate the local desktop updater from remote engine status in that mode. Updating the local app must not implicitly update or restart the paired host.

The mobile app belongs in the same repository. The current iOS project and TestFlight workflow still use `Zeron`, `sh.zeron.ios`, and inherited signing configuration; TestFlight is manually dispatched from `main`. Use TestFlight for dev distribution and App Store releases for production. Mobile binaries update through Apple's distribution system rather than the desktop self-updater. Changing an existing registered iOS bundle identifier can require a new app record and migration work, so check ownership and existing distribution before renaming it. Add host/client protocol compatibility metadata and test the mobile app against both desktop channels.

The naming cleanup should be staged. Rename the GitHub repository and configure the local GitHub CLI default, then update visible names, packaging, feeds, and installed identities together. Internal `zeron-*` Cargo names can remain temporarily to reduce upstream merge conflicts. Rename them in a later mechanical change. Preserve upstream license notices and historical references instead of replacing every occurrence of Zeron or Comet.

Migration from `~/.zeron` needs an explicit import flow with a backup and all relevant processes stopped. That directory may belong to an upstream Zeron installation. Do not automatically claim it for both Noches channels. Import once into the chosen channel and keep the two channels' databases separate. Existing builds pointing to upstream will need a one-time installation of the first Noches-owned package unless their feed was already overridden; renaming the GitHub repository cannot redirect their compiled Zeron feed.

The work can be delivered in five reviewable changes:

| Change | Main files/responsibility | Acceptance evidence |
| --- | --- | --- |
| Identity and migration | Shared product/channel configuration; `apps/zeron/src/paths.rs`, daemon/IPC setup, platform metadata, README | Both channels run together with independent state; explicit legacy import preserves sessions |
| Release pipeline | `.github/workflows/release.yml`, CI triggers, packaging scripts, Noches manifest hosting | Test releases built from each branch publish only to the corresponding feed; failed/older builds cannot advance it |
| Update protocol | `crates/update`, CLI, local update-check API | Ordered dev updates, strict product/channel checks, verified manifests/artifacts, offline retry, downgrade rejection |
| Platform installation | Linux installer and desktop relaunch; macOS staging/swap | Install version A, update to B, restart, verify version/channel/state; simulate failed swap and failed launch |
| Update interface and mobile alignment | GPUI Settings/menu/sidebar; iOS naming and distribution configuration | Manual checks, progress, retry, active-work handling, local updates in remote mode, mobile compatibility tests |

Validation performed for this audit: `cargo test --locked -p zeron-update --lib` passed all 9 Linux tests. These exercise current behavior, including the existing suffix-insensitive comparison; passing does not establish the proposed channel behavior. GitHub repository, branch, PR, release, workflow-run, and ruleset metadata were read. No macOS install/relaunch test, Linux GUI update, signing operation, GitHub mutation, or live release was performed.
