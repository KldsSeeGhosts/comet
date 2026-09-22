# Upstream review: Zeron v0.2.84

Zeron [v0.2.84](https://github.com/zeronsh/zeron/releases/tag/v0.2.84) is the latest reviewed release. Noches `dev` forked at v0.2.72 and has already brought in selected fixes through v0.2.79, plus its own split-pane workspace, sidebar, browser, voice control, companion, and update channels. This is a selective update, **not** a claim of compatibility with every v0.2.84 feature.

This pass ports small changes that do not require replacing Noches's UI or synced document schema:

- [62334b72](https://github.com/zeronsh/zeron/commit/62334b72) and [8f98653b](https://github.com/zeronsh/zeron/commit/8f98653b): redact recognized credentials in agent crash diagnostics, including quoted and padded values.
- [6f2e04df](https://github.com/zeronsh/zeron/commit/6f2e04df): ignore diagnostic JSON objects that lack an RPC result or error, instead of completing a pending request with `null`.
- [f7ef2821](https://github.com/zeronsh/zeron/commit/f7ef2821): cancel permission requests from a foreign ACP session and ignore its live notifications.
- [44dde6a2](https://github.com/zeronsh/zeron/commit/44dde6a2): offer Opus 5.5 as the current curated Claude model and use its label for alias rows, leaving Noches's Fable entries intact.

The source workspace version moves from `0.2.72-noches.1` to `0.3.0-dev.1`. Packaged Noches builds still use the CI-owned `0.3.<run>-dev.<attempt>` or `0.3.<run>` version supplied by `NOCHES_VERSION`. Updating Cargo metadata neither publishes a release nor advances an installed client's update feed.

The larger changes need dedicated integration work:

- [v0.2.82](https://github.com/zeronsh/zeron/releases/tag/v0.2.82) adds an MCP server and more extensive ACP/OpenCode/Pi adapter work. Review process ownership, permissions, and Noches browser/remote session identities before enabling it.
- [v0.2.83](https://github.com/zeronsh/zeron/releases/tag/v0.2.83) adds rich composer references and compact work turns. The composer rewrite overlaps Noches's pane-specific drafts and session lifetimes. It needs a separate port with workspace tests.
- [v0.2.84](https://github.com/zeronsh/zeron/releases/tag/v0.2.84) fixes the upstream docked file explorer and composer width. Noches has a different right-side layout, so those patches do not apply as-is. Synced sidebar sections from earlier releases likewise require reconciling Noches's custom sidebar and registry schema.
- iOS durable sends and snapshot limits from v0.2.82 are useful for the companion but need an iOS build and device tests. They are not included in this Linux-validated pass.

Keep future upstream picks on reviewable branches from `dev`. Compare behavior rather than merging the upstream UI wholesale; run workspace, harness, update, and platform-specific tests before promotion to `main`.
