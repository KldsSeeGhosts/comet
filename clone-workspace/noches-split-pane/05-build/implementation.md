# Visual fidelity implementation

Changed `crates/ui/src/pane/chrome.rs`:

- Stretched each view strip to the full view width.
- Raised pane-header control contrast.
- Rebuilt dormant composer colors from theme roles with AA text.

Changed `crates/ui/src/pane/render.rs`:

- Wrapped every view's pane tree in a 6px inset field.
- Gave each pane card its own opaque `theme.bg` fill.
- Used `theme.border_strong` for inactive pane edges.
- Removed the centered dormant title when a cached transcript exists.

Workspace layout data, drag/drop resolution, close semantics, and persisted
ratios were not changed.

## Focus and empty-pane revision

Changed `crates/ui/src/shell/tabs.rs` and `crates/ui/src/shell/panes.rs`:

- Replaced the focus-driven unified title with stable workspace and device identity.
- Made each workspace tab chip derive its label from the tab's first pane.
- Made pane split commands create a session-less chat pane immediately.
- Removed the split-specific tool-picker commit path.

Changed `crates/ui/src/pane/mod.rs` and `crates/ui/src/pane/render.rs`:

- Limited the tool picker to adding tool tabs.
- Removed duplicate center labels and fake composer chrome from unfocused session-less panes.
- Replaced the focused composer's absolute overlay with a flex footer.
- Clipped the focused transcript to the body above the footer.

Changed `crates/ui/src/shell.rs` and `crates/ui/src/shell/tabs.rs`:

- Suppressed the unified identity block in workspace mode.
- Cleared single-pane dock clearance while the composer consumes pane height.
