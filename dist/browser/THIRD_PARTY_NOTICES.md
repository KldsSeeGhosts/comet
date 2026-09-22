# Browser runtime notices

Noches packages Chromium Embedded Framework, Copyright Marshall A. Greenblatt and the Chromium Embedded Framework Authors. Its BSD license is in CEF-LICENSE.txt.

Chromium includes software under additional licenses. The pinned CEF distribution supplies CREDITS.html. The packaging script copies it alongside the runtime on Linux and as Resources/Chromium-CREDITS.html in Noches Browser.app on macOS.

The Rust bindings come from https://github.com/tauri-apps/cef-rs under the Apache-2.0 OR MIT license. Their pinned version is recorded in Cargo.lock. The runtime is a separate executable; Noches does not incorporate Electron or ZCode source code.
