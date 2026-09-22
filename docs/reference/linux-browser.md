# Browser on Linux

The default browser is bundled Chromium through CEF, matching macOS. WebKitGTK is no longer a runtime requirement for the default build. See [the integrated browser guide](integrated-browser.md) for architecture, agent tools, build steps and verification.

The runtime runs as a separate sandboxed process and sends offscreen frames to GPUI. This keeps pages within GPUI clipping and overlays on X11 and Wayland. Frames use CPU memory and texture uploads. The main app remains usable if the runtime is missing; browser tabs show an actionable error.

Release packages include `browser/noches-chromium`, `libcef.so`, resources and locales. Keep the complete browser directory next to the real Noches executable. Standard Chromium system libraries are still required. Diagnose a missing Linux library with `ldd browser/noches-chromium` and `ldd browser/libcef.so`. Linux must permit Chromium's sandbox mechanisms, including user namespaces. Noches does not disable the sandbox to bypass a host restriction.

For development run `scripts/build-chromium.sh /tmp/noches-runtime debug`, then set `NOCHES_CHROMIUM_HELPER=/tmp/noches-runtime/browser/noches-chromium`. CMake and Ninja are required in addition to the GPUI build dependencies.

The former WebKitGTK backend remains available with the explicit `webkit-browser` Cargo feature for regression fixtures. That build requires `libwebkit2gtk-4.1-dev` and `libjson-glib-dev` on Debian/Ubuntu, or `webkit2gtk4.1-devel` and `json-glib-devel` on Fedora, plus the corresponding runtime libraries. Agent control is available on the default Chromium backend.
