# Noches Linux input repair

`gpui_linux` is vendored from `zeronsh/zui` at `c2d273dc3dadcb260b0fa7c35fc2fe02a14f5add`, the same revision the rest of the workspace pins. Sibling ZUI crates stay on that Git revision. The Noches workspace replaces only this crate:

```toml
[patch."https://github.com/zeronsh/zui"]
gpui_linux = { path = "vendor/gpui_linux" }
```

The crate is its own workspace so it does not inherit Noches dependency versions. `LICENSE-APACHE` is the upstream license.

## What the patch changes

Upstream binds every `wl_seat` it sees and, on a later seat, releases the current pointer and keyboard before binding the new seat. A Cua agent seat (`Cua-Agent`, `Cua-Test-Agent`, and numbered lanes) can replace the physical seat. The window maps, and the compositor never delivers keyboard or pointer enter events to it.

This tree keeps a registry id and name for each seat. A seat cannot take input devices until it has an ordinary name. Synthetic Cua seats stay bound for agent input and cannot replace the selected physical seat. Stale events from a seat that is no longer selected are ignored. `vendor/gpui_linux/src/linux/wayland/seat_selection.rs` holds the selection rules and their tests.

The same files also keep an opaque client-decorated window opaque to the compositor, and use `ext_background_effect_v1` when the compositor no longer offers `org_kde_kwin_blur_manager`.

## Provenance

The repair was previously a local Cargo patch (`zui-c2d273d-cua`) and, before that, a vendor tree on the `legacy` branch against ZUI `07fd941`. Neither was what `dev` or `main` compiled, so packaged updates did not include it. This directory is that repair on the current pin, in the tree both macOS and Linux release jobs build.
