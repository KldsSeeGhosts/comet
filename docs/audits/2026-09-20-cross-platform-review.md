Noches cross-platform code review, 20 September 2026

Noches has substantial recovery logic and regression coverage already. The most useful next work is to make the Linux input patch reproducible, close several lifecycle and memory bounds, and make update installation transactional. A general rewrite would put working recovery behavior at risk.

This review covers the Rust workspace, Swift iOS app, edge service, packaging, and CI. It inventories 431 Rust, Swift, TypeScript, and C files across the application directories, including tests and examples. It is a repository-wide risk review with targeted source tracing and execution, not a claim that every line or every third-party dependency was exhaustively verified. The landing site and distribution assets received a lighter review. No application source was changed, no live service was restarted, and no account or workspace data was modified. Test builds and dependency installation produced local build artifacts.

The local baseline was `dev` at `d83815ee21aa9da94ad859aad76fb3e07fc66a95`, workspace version `0.2.72`. There were existing edits in `crates/ui/src/pane/chrome.rs` and `crates/ui/src/shell/actions_ui.rs`. `crates/ui/src/shell/spaces.rs` also changed during the review through concurrent work. Findings describe the files inspected, rather than a frozen release candidate.

**The actual CachyOS deployment has separate production and development services.**

| Component | Observed state |
| --- | --- |
| Production service | `zeron.service`, PID 1825 at inspection, IPC `127.0.0.1:27654`, data directory `/home/kidsseeghosts/.zeron`, executable `/home/kidsseeghosts/.zeron/app/0.2.59/zeron` |
| Development service | `zeron-dev.service`, PID 129064 at inspection, IPC `127.0.0.1:27656`, data directory `/home/kidsseeghosts/.zeron-upstream`, executable `/home/kidsseeghosts/.local/share/noches-dev/builds/a1676c2c/zeron` |
| Development binary provenance | Running executable SHA-256 matched the installed `a1676c2c` build: `6c9da1fde5bfef2c5d53e487880c73c03c98099dc961ed08c8151d793352c859` |
| Desktop runtime | CachyOS x86_64, kernel `7.2.6-1-cachyos`; WebKitGTK `2.52.6`, JSON-GLib `1.10.8`, Hyprland portal `1.4.1`, PipeWire and WirePlumber installed |
| Portal capability | Screenshot interface version 2, `AvailableTargets = 0`; GlobalShortcuts version 1 |
| Concurrent Pi work | One task is restoring the GPUI physical/agent input-seat overlay; another is repairing the CUA driver and Pi adapter. Their logs were inspected read-only. Their in-progress fixes are not counted as completed or deployed by this review. |

The first check saw only the production service. The later process and service inspection confirms the user's development build is running as well. The remote development commit differs from this local checkout; it is not evidence that the current local sources have been tested on Linux.

**Priority findings**

P1 means address before relying on the affected operation for daily work. P2 means a concrete correctness, resource, or maintainability issue to schedule next. Compatibility gaps and unmeasured optimizations are identified separately below.

1. **P1: Preserve the Wayland input fix in the dependency graph.**

   The pinned `gpui_linux` implementation binds each seat it encounters at startup, and its dynamic `wl_seat` handler releases the current pointer and keyboard before binding the newly announced seat. When CUA advertises an agent seat, this can replace the physical seat and make typing or pointer input stop working. This agrees with the Pi task's diagnosis, and the dependency source was independently checked. The app's [dependency pin](/Users/kidsseeghosts/AiStack/Noches/Cargo.toml:70) still selects upstream `c2d273d`; the inspected [Wayland handler](/Users/kidsseeghosts/.cargo/git/checkouts/zui-05467652636d6f6d/c2d273d/crates/gpui_linux/src/linux/wayland/client.rs:1249) contains the replacement behavior.

   The current repair uses an external overlay and a special build script under `/home/kidsseeghosts/.local/share/noches-dev/`. A normal clean build can omit it again. Finish the existing repair, then pin a reviewed fork commit or a tracked patch in the repository. Test seat announcement, seat removal, physical typing, agent input, and fractional scaling together. Do not infer successful input from compilation alone. This issue is already being worked on by the Pi task; avoid a competing implementation.

2. **P1: A partial macOS update is accepted as a complete staged app.**

   [stage_mac_app](/Users/kidsseeghosts/AiStack/Noches/crates/update/src/lib.rs:513) returns immediately when `Zeron.app/Contents/MacOS/zeron` exists. It extracts directly into that same staging directory. An extraction error or process termination after the executable is written can therefore leave an incomplete bundle that the next attempt accepts. The bundle swap then removes the working installation after copying the incomplete replacement.

   Reproduced against the compiled updater library with a temporary fixture containing only the executable, no `Info.plist`, resources, or completed extraction. `stage_mac_app` returned success without contacting the deliberately invalid feed. The probe is at `/tmp/noches-audit-20260920/update_stage_probe.rs`.

   Extract into a unique temporary directory, validate the expected bundle and executable, then atomically publish the finished stage. Reuse only a stage with a completion record tied to the artifact digest. Add interruption and truncated-archive recovery tests. Also reject malformed version strings before using them as path components.

3. **P1: Terminal output queues can grow independently of the advertised replay limit.**

   The 1 MiB limit applies to replay history, but [LiveTerminal::emit](/Users/kidsseeghosts/AiStack/Noches/crates/engine/src/terminals.rs:58) clones every event into unbounded subscriber channels. [subscribe](/Users/kidsseeghosts/AiStack/Noches/crates/engine/src/terminals.rs:385) returns an unbounded receiver, and the [RPC stream](/Users/kidsseeghosts/AiStack/Noches/crates/engine/src/rpc.rs:2487) consumes it at the downstream client's pace. The [PTY reader queue](/Users/kidsseeghosts/AiStack/Noches/crates/engine/src/terminals.rs:323) is also unbounded, and batching has no byte ceiling.

   A terminal producing build logs faster than a remote connection can consume them accumulates queued output despite replay eviction. This is a source-confirmed missing bound, not a measured OOM in the user's daemon. Use byte-budgeted subscriber queues and a documented overflow policy, such as disconnecting a lagging subscriber with explicit resynchronization. Bound the raw reader channel and batch size too. Test a paused reader under sustained output; preserve exit delivery and make any replay gap visible rather than silently corrupting the terminal display.

4. **P1: iOS sign-out does not retire all deferred work from the old identity.**

   [preloadSessions](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/App/AppModel.swift:784) launches untracked tasks that retain a store, wait for registry connectivity, and later call `releaseDial()`. [signOut](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/App/AppModel.swift:243) stops and removes stores, but does not cancel those tasks. [SessionStore.stop](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/Sync/SessionStore.swift:376) clears `chatRoom` without clearing `started`. A pending preload can therefore pass [connectIfReady](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/Sync/SessionStore.swift:216) after sign-out and construct a room using the old configuration.

   There is a related credential race: [refreshedToken](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/App/AppConfig.swift:69) can complete after sign-out and write the old credentials back to the shared Keychain keys. Source tracing confirms both missing retirement checks; no real account was used to reproduce them.

   Give each authenticated runtime a generation or cancellation owner. Cancel preload and refresh tasks when retiring it, mark stores stopped, and guard every late callback and Keychain write against the current generation. Test sign-out during a delayed preload and a delayed token refresh, including signing in as a different account immediately afterward.

5. **P2: Permanently rejected iOS refresh credentials never become a reauthentication state.**

   [AppConfig.refreshedToken](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/App/AppConfig.swift:77) discards the refresh error with `try?` and returns the expired access token on every failure. A revoked or invalid refresh token is treated like a temporary outage, leaving the app in its ready state while transports repeatedly fail.

   Preserve structured auth errors and distinguish a transient network/server failure from terminal credential rejection. Present a reauthentication state and pause protected requests. Preserve locally queued work; automatically calling the existing `signOut()` would wipe the local document cache and is not a safe substitute for this state transition. Verify the edge's actual refresh error contract before classifying status codes.

6. **P2: The iOS ordinary-image cache accounts for compressed bytes instead of decoded memory.**

   [AttachmentImageCache.seed](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/Composer/Attachments.swift:363) and [readImage](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/Composer/Attachments.swift:437) use `UIImage(data:)` and charge `data.count` to a 64 MiB budget. A small JPEG can decode to a much larger bitmap when displayed. The generated-image path already uses bounded ImageIO thumbnails and decoded byte accounting.

   Apply that bounded decoding approach to ordinary attachments and camera staging, with an appropriate source-dimension policy. Charge `bytesPerRow * height` plus retained encoded data where applicable. Keep full-resolution export separate from the thumbnail cache. Test highly compressed, high-resolution photos and several simultaneously visible attachments on a physical phone. No fixed iOS memory-kill threshold or measured memory saving is assumed here.

7. **P2: iOS eagerly retains every session document, even though only eight are meant to keep warm connections.**

   [preloadSessions](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/App/AppModel.swift:784) creates and starts a store for every overview chat before applying `warmDialCap`. Starting a store [loads its document and projects entries](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/Sync/SessionStore.swift:149). [releaseSessionStore](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/App/AppModel.swift:764) does nothing, and stores are removed on sign-out rather than navigation or memory pressure.

   This is unbounded retention by account size, not necessarily an unreachable-object leak. The connection cap is not a document-memory cap. Use a measured resident-byte budget and LRU eviction, pinning visible sessions and sessions with pending commands or attachment delivery. Hydrate inactive transcripts on demand. Add memory-warning handling and a large-history fixture. Do not discard durable pending work just to meet the cache budget.

8. **P2: The fork still shares upstream service, identity, and update defaults.**

   The binary defaults to [edge.zeron.sh](/Users/kidsseeghosts/AiStack/Noches/apps/zeron/src/main.rs:76), the updater derives its default [release URL from that edge](/Users/kidsseeghosts/AiStack/Noches/crates/update/src/lib.rs:224), and [macOS bundle metadata](/Users/kidsseeghosts/AiStack/Noches/dist/macos/Info.plist:10), [desktop registration](/Users/kidsseeghosts/AiStack/Noches/dist/zeron.desktop:1), data paths, and service names still identify Zeron. This is a fork-release readiness issue, not proof of unintended sync: a clean installation remains local-only, source builds are update-unmanaged, and unattended updates require explicit opt-in.

   A packaged Noches app can offer an upstream Zeron update and replace fork behavior. Establish explicit production and development identities, release feeds, storage roots, and service labels. Preserve the already separated Linux dev/prod data directories. Show build commit and channel in status/About so directory names and workspace versions do not have to stand in for provenance. Migrate existing data deliberately rather than simply renaming `.zeron`.

9. **P2: Daemon installation does not preserve update configuration consistently.**

   [CAPTURED_ENV](/Users/kidsseeghosts/AiStack/Noches/apps/zeron/src/daemon.rs:23) omits `ZERON_RELEASES_URL` and `ZERON_AUTO_UPDATE`. Setting either before `zeron daemon install` does not persist it into the installed service. For a fork-specific feed, the resulting daemon can use a different feed from the invoking shell.

   There is also an asymmetry for managed macOS CLI installs: [systemd_exec_path](/Users/kidsseeghosts/AiStack/Noches/apps/zeron/src/daemon.rs:245) restores the stable `current` symlink, but [render_launchd_plist](/Users/kidsseeghosts/AiStack/Noches/apps/zeron/src/daemon.rs:262) writes the resolved executable path. Updating `current` and kickstarting launchd can relaunch the old version. Preserve update settings in both service formats and resolve a stable executable path for both. These paths need service-rendering regression tests; the existing CachyOS dev service was not modified.

10. **P2: Automatic update idleness is checked before asynchronous work, not at the restart boundary.**

    [auto_apply_when_idle](/Users/kidsseeghosts/AiStack/Noches/crates/update/src/lib.rs:768) waits for idleness, then calls [apply](/Users/kidsseeghosts/AiStack/Noches/crates/update/src/lib.rs:816), which fetches the manifest again, may download a different release, and schedules a restart 800 ms later. A run or terminal can start in that interval. The pre-staged artifact does not remove the race.

    Introduce an update/quiescence guard shared with admission of new work, recheck under that guard, and keep the guard until shutdown begins. Test a command arriving during a delayed manifest request and during the delayed restart. This affects opt-in unattended managed updates, not ordinary source-build launches.

11. **P2: Linux browser input writes can block the GPUI thread.**

    [Worker::send](/Users/kidsseeghosts/AiStack/Noches/crates/ui/src/browser/linux/mod.rs:175) synchronously locks and writes to the helper's stdin. Native page commands invoke it from UI event and layout paths. A helper that stops reading can fill the pipe and block input, resize, or tab closure. The helper also writes full frames synchronously; bounded latest-frame retention on the reader side does not bound these foreground writes.

    Put helper writes on an owned worker with bounded queues, preserve input ordering, and coalesce replaceable resize/motion updates. Add a deadline and helper failure state. Verify that a deliberately stalled helper cannot freeze the window. This is a blocking path confirmed in source, not a freeze injected into the user's active desktop.

12. **P2: Edge backfill and checkpoint reads need tighter resource bounds.**

    [handleRowsReq](/Users/kidsseeghosts/AiStack/Noches/edge/src/chat-room.ts:415) sends the entire selected row history in one synchronous WebSocket handler, unlike the bounded HTTP rows response. Row size and per-minute push limits do not cap total retained backlog or queued output for one backfill. Add a byte/page budget and protocol continuation, coordinated across Rust and Swift clients. Test a long uncheckpointed history and a slow reader.

    Separately, [BlobStore.get](/Users/kidsseeghosts/AiStack/Noches/edge/src/blobs.ts:27) loads all chunks and concatenates them. [Checkpoint Range handling](/Users/kidsseeghosts/AiStack/Noches/edge/src/chat-room.ts:147) applies the requested offset afterward. Resuming near the end of a 32 MiB checkpoint still reads and copies the whole blob. Add range-aware chunk reads or streaming with consistent checkpoint metadata. The allocations are visible in source; a Cloudflare memory-limit failure was not reproduced.

13. **P2: Unix harness cleanup does not own descendant processes.**

    The shared [Unix process layer](/Users/kidsseeghosts/AiStack/Noches/crates/harness/src/process.rs:1) reexports Tokio process types, and [shutdown_child/send_signal](/Users/kidsseeghosts/AiStack/Noches/crates/harness/src/lib.rs:253) targets the direct child's PID. No Unix process-group setup was found in the harness sources. If a provider dies while its tool child survives, cleanup has no application-owned group to terminate.

    Define which subprocesses belong to the run, then create and retire an owned process group for those processes, with bounded graceful shutdown and escalation. Explicitly detached user servers need a separate policy. Never change `kill(pid)` to `kill(-pid)` without first establishing ownership of that group. Add a fake provider that leaves a child holding stdout open. The broader claim that closing a duplicate PTY master descriptor necessarily unblocks another thread's read was rejected; terminal cleanup needs its own tested design.

14. **P2: Standalone sync integration tests do not compile with the advertised default command.**

    Reproduced with `cargo test --locked -p zeron-sync --tests --no-run`. Both `registry_client.rs` and `transport_reliability.rs` import [registry::mock_server](/Users/kidsseeghosts/AiStack/Noches/crates/sync/src/registry.rs:1002), which is gated behind `test` or `mock-server`. Integration tests compile the library without `cfg(test)`, and the crate's [dev dependencies](/Users/kidsseeghosts/AiStack/Noches/crates/sync/Cargo.toml:31) do not enable that feature. Workspace feature unification can conceal the issue.

    Make test support explicit with a test-only feature dependency or `required-features` and documented test commands. Keep mock-server code out of production defaults. Add standalone package compilation to CI.

**Platform readiness and optimization work**

| Area | Evidence and recommended work |
| --- | --- |
| CachyOS Appshots | [portal.rs](/Users/kidsseeghosts/AiStack/Noches/crates/ui/src/appshots/linux/portal.rs:65) requires Screenshot version 3 and window targets. The inspected host exposes version 2 with no targets, so the current capture path is unavailable. This is an intentional capability gate that avoids silently capturing the entire screen. Add an explicit supported Hyprland window-capture path or require a capable portal; retain consent and cancellation behavior. The CUA input repair does not itself change this Screenshot capability. |
| Tailnet transport | Remote control RPC uses the authenticated edge relay. Tailscale reachability alone does not create a direct Noches control channel. Preview networking separately uses WebRTC, so saying all application traffic uses the relay would be wrong. Consider authenticated direct control transport only after measuring relay latency and bandwidth on the actual devices. Do not expose the local IPC listener to the tailnet without a new authorization boundary. Durable sync still depends on the edge. |
| Idle desktop CPU | [preview discovery](/Users/kidsseeghosts/AiStack/Noches/crates/preview/src/service.rs:155) scans every two seconds when projects exist. macOS runs `lsof` and `ps`; Linux walks process descriptors. Cache stable process identity and back off when no preview consumer or relevant changes exist. Preserve discovery of externally started servers. Measure scan time, CPU, and wakeups first. |
| Large sidebars | [render_active_rows](/Users/kidsseeghosts/AiStack/Noches/crates/ui/src/shell/spaces.rs:1230) clones/sorts chats and constructs row elements for the active list each render. Group lookup is linear in existing groups. Cache derived order and virtualize rows for large accounts, preserving resort animation, keyboard navigation, and edge fades. Profile 100, 1,000, and 5,000 sessions before choosing thresholds. |
| Linux animated browser | [render_frames](/Users/kidsseeghosts/AiStack/Noches/crates/ui/src/browser/linux/helper.c:333) converts and transfers complete CPU-addressable frames, with a 16 ms timer. Dirty and visibility checks already exist. A 1920x1080 RGBA frame is about 7.9 MiB, so animated content can require substantial copying. Measure frames, bytes, CPU, and upload cost before considering frame-rate caps, damage regions, or shared memory. |
| Foreground persistence | [settings flush](/Users/kidsseeghosts/AiStack/Noches/crates/ui/src/settings.rs:490) and workspace layout flushing perform synchronous writes on the UI thread. Debouncing reduces frequency but does not remove a slow write from the frame. Consider a serialized background writer with revision ordering and a final shutdown flush. Keep the existing atomic replacement behavior. |
| iOS code rendering | [attributedLine](/Users/kidsseeghosts/AiStack/Noches/apps/ios/Zeron/Markdown/MarkdownBlockView.swift:244) repeatedly allocates prefixes and walks character offsets for each syntax span. Use monotonic indices and cached attributed results keyed by content and style. Profile long token-dense lines. The Character array is allocated once per line, not once per token. |
| Linux network changes | [net_path.rs](/Users/kidsseeghosts/AiStack/Noches/crates/sync/src/net_path.rs:13) has native path monitoring only on macOS. Linux still has suspend detection, successful-dial broadcasts, and focus recovery, so it is not without reconnect recovery. Netlink or a suitable D-Bus adapter could shorten recovery and reduce useless retries. iOS already has its own `NWPathMonitor` and foreground hooks. |
| Headless builds | [apps/zeron/Cargo.toml](/Users/kidsseeghosts/AiStack/Noches/apps/zeron/Cargo.toml:15) unconditionally includes UI; [ui/build.rs](/Users/kidsseeghosts/AiStack/Noches/crates/ui/build.rs:8) requires Linux WebKit development packages. An optional UI feature could reduce server build dependencies and binary size. This is not a blocker on the inspected CachyOS host, where the browser libraries are present. |

No numerical performance improvement is claimed without a before/after benchmark. Existing protections worth preserving include transcript virtualization, incremental projection, bounded syntax and generated-image caches, attachment path containment, file-write conflict handling, durable command IDs, replay deduplication, and local/synced storage separation.

**Verification record**

Validation ran on an Apple Silicon Mac with Rust `1.98.1`, Xcode `27.0`, Node `22.23.2`, and an iPhone 18 Pro simulator on iOS `27.0`. Remote Linux inspection was read-only. No physical-iPhone memory test, current-checkout Linux rebuild, external provider credential test, or live cross-device destructive fault injection was performed.

| Check | Result |
| --- | --- |
| Rust libraries excluding harness, serialized | 1,639 passed, 1 ignored, including all 1,155 UI library tests |
| Harness library | 179 passed, 1 failed; shell fallback fixture failed again in isolation |
| iOS `ZeronTests` | 135 passed, 0 failed; result bundle `/tmp/noches-audit-20260920/ios-tests.xcresult` |
| Edge TypeScript | Passed |
| Edge unit tests | 41 passed |
| Edge workerd tests | 15 passed |
| Landing download tests | 12 passed |
| Standalone sync test compilation | Failed with two unresolved `mock_server` imports |
| Partial macOS update probe | Confirmed incomplete stage reuse |
| Full Rust library/integration sweep | Five test failures and one manually stopped stalled test; six failed targets. Not a green release gate. |

The full sweep used `cargo test --locked --workspace --tests --no-fail-fast -- --test-threads=1` and finished after the stalled preview test was stopped. Three of the five failures have identifiable macOS fixture portability problems; the shutdown and WebRTC failures remain unresolved. Passing library counts above describe the earlier run, not an assertion that those tests passed consistently across reruns.

The shell fixture failure is explained by [shell_env.rs](/Users/kidsseeghosts/AiStack/Noches/crates/harness/src/shell_env.rs:326): its fallback PATH contains only a fake directory and `/bin`, but the probe calls `env`, which is in `/usr/bin` on this Mac. A direct reproduction returned `env: command not found`. Repair the fixture's portability and test production behavior with deliberately unusual PATHs; this failure alone does not establish that normal shell discovery is broken.

The catalog test `models_discovers_visible_catalog_with_pagination` assumes [the failing executable is `/bin/false`](/Users/kidsseeghosts/AiStack/Noches/crates/harness/tests/codex.rs:627). On this Mac it is `/usr/bin/false`, and `/bin/false` is absent. Use a controlled test executable so the test actually reaches the intended failed-probe fallback.

The generated-image test `generated_image_is_materialized_before_publication_and_survives_reopen` compares a canonicalized image path with [the uncanonicalized upload directory](/Users/kidsseeghosts/AiStack/Noches/crates/engine/tests/e2e.rs:2591). macOS temporary paths can resolve through `/private`, making lexical containment false even for an image inside the directory. A separate probe against the compiled engine confirmed `lexical containment: false` and `canonical containment: true`. Normalize both paths in the assertion, retaining the production containment checks. This explains the failing assertion; it does not substitute for rerunning the complete persistence test after fixing the fixture.

The shutdown regression `online_runtime_shutdown_stops_edge_workers_and_retires_the_graph` failed both in the full sweep and alone, reporting 6 requests versus 5. Its [mock server](/Users/kidsseeghosts/AiStack/Noches/crates/engine/tests/local_first.rs:34) increments the count even when `read` returns EOF or an error, and already accepted requests can be observed after cancellation. Tighten the fixture to count complete requests and distinguish already-started traffic from new post-shutdown work before attributing the failure to a surviving engine worker. Keep this gate unresolved until that distinction is tested.

The preview test `actual_webrtc_pair_streams_in_both_directions` passed in the first library sweep but failed in the integration sweep and the isolated rerun at [peer.rs:641](/Users/kidsseeghosts/AiStack/Noches/crates/preview/src/peer.rs:641), with `P2P stream stalled` on the 1 MiB exchange. This is repeatable evidence of a reliability or test-environment problem; its root cause is not established. Capture candidate selection, stream credits, SCTP buffering, and EOF state on failure. Do not hide it by merely increasing the timeout.

The preview churn test `remote_preview_churn_does_not_accumulate_tasks_or_memory` made no visible progress for more than six minutes and lacks an overall deadline. Only that audit test process was terminated, allowing the remaining workspace tests to finish. This is an incomplete check, not an assertion failure or proof of a memory leak. Add an overall timeout and per-round progress diagnostics, then determine which operation stalls before relying on its resource-retention result.

Evidence logs are under `/tmp/noches-audit-20260920/`. They are temporary local audit artifacts, not committed fixtures. The shell's `npm` wrapper redirected to pnpm, so edge installation used the installed `npm-cli.js` through Node and tests invoked the installed TypeScript/Vitest entry points directly.

**Coverage and order of work**

| Code area | Review focus |
| --- | --- |
| `apps/zeron`, update, distribution, workflows | Startup/profile selection, dev/prod identity, daemon installation, updater publication and swap, build/test gates |
| `crates/engine`, `crates/harness` | Child lifecycle, terminal backpressure, command execution boundaries, storage and file operations, provider regression suites |
| `crates/doc`, `crates/sync`, `crates/rpc`, `crates/proto` | Document/command schemas, local persistence, protocol recovery, relay boundaries, cancellation and package tests |
| `crates/preview` | Discovery cost, proxy resource limits, stream lifetimes, WebRTC verification |
| `crates/ui`, `crates/workspace` | Sidebar/transcript construction, browser helper, files/editor, settings/layout persistence, terminal rendering and platform capture |
| `crates/theme`, `crates/syntax` | Import/read bounds, cache budgets, parse cancellation and unit coverage |
| `apps/ios` | Auth and sign-out, store residency, attachment decoding, sync lifecycle, transcript rendering and simulator tests |
| `edge` | Authenticated forwarding, row/checkpoint storage, WebSocket/HTTP bounds, unit and workerd tests |

First, finish and record the existing Linux input repair and make build identity observable. Next, fix partial update reuse, terminal queue bounds, and iOS runtime retirement. Follow with iOS reauthentication and image/document budgets, then service/update consistency. Resolve the red test gates before calling the resulting build hardened. Optimize scanning, sidebar rendering, and browser frames using measured workloads after the correctness changes settle.

For acceptance, exercise a Mac host and CachyOS host from a physical iPhone through background/foreground transitions, Wi-Fi and cellular handover, a tailnet reconnect, host restart, and a long offline queue. Include large transcript and image fixtures, a stalled terminal subscriber, interrupted update extraction, sign-out during refresh/preload, and Hyprland synthetic-seat announcement. Record CPU, resident memory, frame latency, bytes transferred, command outcomes, and build commit for each run.
