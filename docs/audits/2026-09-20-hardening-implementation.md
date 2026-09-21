# Noches hardening implementation, 20 September 2026

This supplements the [initial cross-platform audit](2026-09-20-cross-platform-review.md). The initial audit describes the pre-change baseline. This record describes subsequent implementation in the shared `dev` checkout at `d83815ee`, alongside other active tasks. Changes remain uncommitted. No live service, installed application, or edge deployment was replaced by this task.

## Changes and remaining scope

| Audit finding | Implementation status |
| --- | --- |
| 1. Wayland seat handling and reproducible patch | Left to the existing Pi input-repair task. No competing dependency overlay was introduced. A reviewed, reproducible dependency pin is still needed. |
| 2. Partial macOS update staging | Fixed incomplete-stage reuse. Extract into a unique temporary directory, serialize publication with a process lock, verify bundle files, record artifact digest/version, then rename into place. Corrupt extraction leaves no completion marker. Refuse conflicting completed stages. Validate release versions before constructing paths. |
| 3. Terminal output queues | Bounded Unix raw reader to 32 chunks, output batches to 8 KiB, replay to 128 events/1 MiB, subscribers to 128 events and 16 clients. Disconnect slow clients and resume through replay. Desktop terminal resets its parser and displays an omission notice when replay has a sequence gap. Windows ConPTY raw buffering remains unchanged because its synchronous shutdown requires independent draining. |
| 4. iOS runtime retirement | Retire configuration before credential deletion; serialize token persistence against retirement. Cancel preload, projection application, and attachment escort tasks. Guard deferred dials and late document callbacks. Clear cached image previews and close their relays across identities. Force document persistence at background/stop boundaries, including when the update callback has not yet marked the saver dirty. |
| 5. iOS rejected refresh credentials | Explicit `invalid_grant` errors stop refresh retries and show cloud sign-in while retaining queued local documents. Transient failures retain retry behavior. Edge now distinguishes terminal rejection from upstream/network failure. Both client and edge changes must ship for this structured contract; generic legacy 401 responses remain retryable. |
| 6. iOS ordinary image memory | Downsample previews to 2048 pixels and charge decoded bytes, not compressed file size, against the cache budget. Supported upload formats retain their original bytes. Memory warnings evict decoded previews. HEIC-to-JPEG conversion still has a full-resolution transient allocation. |
| 7. iOS session residency | Keep eight warm documents, plus visible sessions and sessions with pending work. Evict other stores on navigation or memory pressure. Persist pending-command metadata in the existing snapshot header so cold queued work is still loaded for delivery. Old snapshots require a one-time document inspection per cached modification timestamp. Physical-device memory measurements remain outstanding. |
| 8. Fork identity and release defaults | Deferred. Signing, bundle/service identifiers, release feed ownership, and existing user-data migration must be handled together. No automatic identity migration was attempted. |
| 9. Daemon update configuration | Preserve `ZERON_RELEASES_URL` and `ZERON_AUTO_UPDATE` in installed services. Managed macOS launchd installs use the stable `current` symlink; source builds keep their literal executable path. |
| 10. Update admission race | Open. A true admission lock must cover the whole check/retire/restart transition and all new-work entry points. A one-time atomic flag would not fix this race. |
| 11. Linux browser command writes | Move helper stdin writes off the UI thread; bound outstanding bytes/commands and apply a two-second write deadline. Failed writers terminate the helper and surface a recoverable tab error. Shared Unix writer tests pass on macOS. Native Linux module compilation/runtime validation is still required. |
| 12. Edge backfill and blob reads | Partial. Checkpoint range reads now select only intersecting SQLite chunks instead of reconstructing the full blob first. Full responses still allocate their returned payload. WebSocket backfill pagination needs a coordinated sender/receiver protocol change and remains open. |
| 13. Harness descendant cleanup | Open. Safe process-group ownership and an explicit detached-server policy are needed before changing child termination. |
| 14. Standalone sync tests | Enable mock-server support through a test-only dependency. Standalone library and integration tests compile and pass without adding mocks to production defaults. |

The tailnet/control implementation, split-island UI, and mobile companion/theme changes belong to the other active tasks. Their overlapping files were preserved. The iOS root distinguishes cloud reauthentication from the companion landing screen.

## Regression validation

These are targeted results for this implementation, not a claim that the complete workspace or every platform passes its release gate.

| Check | Result |
| --- | --- |
| Desktop UI library, serialized | 1,160 passed, including all four bounded browser-writer tests. Parallel execution aborts in macOS HIToolbox because existing native font tests call keyboard APIs concurrently. |
| Updater library | 10 passed, including partial/corrupt staging recovery, simultaneous stage publication, and cache reuse. |
| Terminal backpressure | 3 passed, including paused subscriber recovery through exit, queue/subscriber limits, and bounded-reader shutdown. |
| Engine local-first and terminal/repository integration | 11 + 24 passed. |
| Engine end-to-end | 21 passed, 2 ignored in the earlier targeted run. |
| Harness library and Codex integration | 180 + 21 passed, 4 ignored. |
| Standalone sync | 53 library, 11 registry, and 1 transport tests passed; 2 ignored. |
| Daemon rendering/path tests | 4 passed. |
| Edge TypeScript | Passed. |
| Edge unit / workerd | 46 / 16 passed, including refresh rejection classification and chunk boundary/range behavior. |
| iOS, dedicated iPhone 18 Pro simulator | 147 passed, including eight new hardening regressions and the full scrolling suite. Includes companion tests added by the concurrent mobile task. |
| Isolated WebRTC 1 MiB exchange | Passed on the latest isolated run, but earlier failures mean this is not established as reliable. |
| Preview churn/re-pair | Still fails. Sixty ordinary churn rounds retained 16 live tasks; a subsequent re-pair stalled during HTTP echo. RSS is unavailable through this test's Linux-only `/proc` measurement on macOS, so its zero reading is not a memory result. |

Fixture corrections are separate from production fixes. The shell fixture now includes `/usr/bin`; the Codex failed-executable fixture uses a controlled script; the generated-image containment assertion canonicalizes the temporary directory. The shutdown request counter now counts complete HTTP headers and excludes observed local `HEAD /` port-discovery probes. That passing test does not establish that a production shutdown bug was fixed.

The preview churn test now bounds request rounds and identifies the round and operation that stalls. The latest reproduction reaches the re-pair path and times out during HTTP echo. No production WebRTC change or timeout inflation is presented as a fix.

Logs are local temporary artifacts under `/tmp/noches-hardening-*.log`; simulator build and result bundles are under `/tmp/noches-audit-20260920/DerivedData`. A dedicated temporary simulator isolates the final iOS run from concurrent tasks using the default simulator.

## Deployment and acceptance still required

The inspected CachyOS production service was 0.2.59 and the separate development service ran commit `a1676c2c`. This implementation has not been deployed to either. The read-only remote inspection cannot validate these new sources.

Before relying on this build daily, finish the open admission/process-lifecycle/protocol items, integrate the other tasks, compile and exercise the Linux browser path, and test on a physical iPhone. Include sign-out during refresh, background/foreground delivery, a long offline queue, large photos, terminal floods, peer re-pairing, interrupted updates, and physical/agent Wayland input. Sidebar, process discovery, browser frame transfer, and syntax-rendering performance proposals still need measured workloads; this pass makes no numerical speedup claim.
