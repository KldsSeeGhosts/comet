# Integrated Chromium browser

Noches uses CEF 152 through the pinned `cef` Rust crate on macOS and Linux. GPUI remains the application shell. A separate `noches-chromium` process renders browser pages offscreen and sends BGRA frames over inherited pipes. GPUI draws those frames with its normal clipping and overlays and forwards pointer, keyboard, clipboard and IME input.

The user and agent operate the same tabs. Tabs belong to conversations, including when an agent runs in the background. Agent navigation does not switch to another conversation. Popup links retain their originating conversation.

## Agent tools

The desktop starts a Unix socket in a private directory. Up to 16 requests can run concurrently, so one conversation does not block others while awaiting Chromium. Before a local run, the engine adds browser instructions to its prompt. Codex, Claude and ACP runs also receive a `noches_browser` stdio MCP server. Other providers can use the CLI through their shell tool. Remote runs do not receive control of the local desktop browser.

Tools cover tab listing, opening, state, navigation, history, reload, close, DOM snapshots, clicks, field replacement, select options, scrolling, key presses, PNG screenshots, JavaScript evaluation, console messages and request summaries. Network and console history retain at most 100 entries per tab and may omit events under load. Network summaries exclude headers and bodies.

The injected instructions include the executable, socket and conversation arguments. For manual debugging:

```sh
zeron browser --socket /tmp/noches-browser-HASH/control.sock --session SESSION \
  '{"action":"open","url":"http://localhost:3000"}'
zeron browser --socket /tmp/noches-browser-HASH/control.sock --session SESSION \
  '{"action":"snapshot","tab":1}'
zeron browser --socket /tmp/noches-browser-HASH/control.sock --session SESSION \
  --output /tmp/page.png '{"action":"screenshot","tab":1}'
```

Use IDs returned by `open` or `tabs`. Opening and navigating acknowledge the request before loading finishes; check `state`. Take fresh snapshots after page changes. References reject disconnected, hidden, covered and stale elements. Screenshots return native MCP image content or a PNG file through the CLI.

## Build and run

Install the normal GPUI build dependencies plus CMake and Ninja. The first runtime build downloads the CEF distribution selected by the locked dependency. End users receive the runtime in the release package; Noches does not download executable browser code on first use.

```sh
scripts/build-chromium.sh /tmp/noches-runtime debug
cargo run -p zeron
```

For that development run set `NOCHES_CHROMIUM_HELPER` to an absolute path:

- macOS: `/tmp/noches-runtime/Noches Browser.app/Contents/MacOS/noches-chromium`
- Linux: `/tmp/noches-runtime/browser/noches-chromium`

On macOS ad hoc sign a development bundle with `codesign --force --deep --sign - '/tmp/noches-runtime/Noches Browser.app'`. Release packaging signs nested frameworks and helper bundles before the outer app. Distribution signing and notarization still require the release credentials and release pipeline.

`package-macos.sh`, `package-linux.sh`, and `run-macos-dev.sh` bundle Chromium automatically. On Linux the installer keeps the executable and browser directory together in a versioned directory under `~/.local/share/noches/app` or `~/.local/share/noches-dev/app`, with a launcher symlink in `~/.local/bin`. Updates retain the previous version, including its browser runtime. See [desktop channels and installation](../desktop-updates.md). The runtime must stay next to the real executable. CEF and Chromium license notices ship with it.

## Verification

```sh
cargo test -p zeron-browser
cargo test -p zeron-ui --lib -- --test-threads=1
python3 scripts/test-chromium-runtime.py "$NOCHES_CHROMIUM_HELPER" /tmp/runtime-results
cargo build -p zeron-ui --example chromium-fixture --features browser-fixture
```

Run the GPUI fixture with `scripts/run-macos-browser-fixture.sh target/debug/examples/chromium-fixture /tmp/gpui-results` on macOS. On Linux use `xvfb-run -a target/debug/examples/chromium-fixture /tmp/gpui-results`, with `xdotool` and ImageMagick installed. On Hyprland, run it in the Wayland session with `NOCHES_FIXTURE_HYPRLAND=1` and `grim` available; it captures only its own window. Both platforms need `NOCHES_CHROMIUM_HELPER` set. The runtime fixture tests real Chromium, native input, DOM actions, screenshots, navigation, resizing and clean exit. The GPUI fixture tests the shared displayed page, the Unix bridge, valid conversation isolation, background tabs and popups, console/network inspection, overlays and crash recovery.

## Boundaries and remaining work

Chromium keeps its sandbox enabled. CDP runs over inherited process pipes; there is no debugging TCP listener. Top-level navigation accepts HTTP(S) and internal `about:blank`, not filesystem or custom schemes. Browser data is ephemeral and shared among tabs in one desktop browser process. Closing all tabs releases that process and its profile.

The socket trusts processes running as the same OS user. Conversation checks prevent accidental cross-session tool routing; they do not sandbox agents that already have arbitrary shell access. Website text and JavaScript results are untrusted. DOM snapshot helpers run in the page's main JavaScript world and are not an integrity boundary against a hostile page.

This release does not provide persistent login profiles, a download/upload permission UI, remote browser forwarding, a complete accessibility tree, or zero-copy GPU textures. Snapshot references cover the main document, not cross-origin frames or closed shadow roots. Native complex IME and assistive technology behavior still need broader device testing. An already-running provider process cannot acquire new MCP configuration until it restarts. CEF updates require rebuilding and shipping the app; maintainers must track Chromium security releases.

The architecture was informed by [ZCode's browser guest manager](https://github.com/zai-org/ZCode/blob/main/packages/desktop/src/main/browserView/browserGuestManager.ts), which uses Electron guest webContents and CDP. Noches uses CEF to retain its Rust/GPUI architecture. [Codex's public browser documentation](https://developers.openai.com/codex/browser) describes the shared user/agent workflow; it does not establish the private implementation of the ChatGPT desktop app.
