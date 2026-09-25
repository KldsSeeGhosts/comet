# Noches Desktop: UI design system

The desktop UI follows **docs/design/control-plane.md**: a control plane for coding agents, with T3 Code's thread cards in the sidebar and Cursor-style meaningful color. Read it before changing any sidebar, pane-header, or composer chrome. It supersedes the earlier open-design "buddy bot" sidebar plan. Buddy avatars were removed on purpose; do not bring them back.

## Rules in one breath

- Color encodes state or identity, never decoration.
- Status hues come only from `crate::status_palette::SessionState` (sky working, indigo awaiting input, emerald completed-unseen, danger failed).
- Projects show a repo favicon or a colored monogram (`Shell::render_project_icon`). Harness marks keep their brand tint.
- Panes are flush and opaque with 1px hairline dividers, not rounded islands.
- Metadata (branch, device, elapsed time, model) is monospace 11px.

## Where things live

- Sidebar cards: `render_chat_row` in `crates/ui/src/shell.rs`.
- State sections and visible order: `render_active_rows` / `sidebar_visible_order` in `crates/ui/src/shell/spaces.rs`. Keep both in sync, because jump shortcuts use the visible order.
- Project badges: `crates/ui/src/shell/project_icon.rs` (favicon discovery for local and remote projects, with a monogram fallback).
- Pane chrome: `crates/ui/src/pane/chrome.rs` (`pane_header`) and `crates/ui/src/pane/render.rs` (flush layout, dividers).
- Drop geometry: `crates/ui/src/pane/hit_test.rs`. Its tolerances are explicit constants; do not re-derive them from layout padding.
- Model chip and placeholder: `crates/ui/src/pickers.rs` (`trigger_chip`, `chip_model_label`) and `crates/ui/src/composer.rs`.

## Split views are load-bearing

Split panes, view splits, header drag, sidebar-row drag-to-split, drop previews, divider drag and equalize, and focus routing are covered by `crates/ui/src/shell/pane_surface_regressions.rs`, `workspace_regressions.rs`, and the `pane::` tests. Run `cargo test -p zeron-ui --lib` after any pane or sidebar change.

## Visual QA

`scripts/dev-demo.sh` boots a seeded mock engine and the headed app (it runs under macOS bash 3.2). For a split layout, seed `{data_dir}/workspace-layout.json` with a `split` root for the selected space; `SplitNode` is serde-tagged `{"type":"split","horizontal":..,"ratio":..,"first":..,"second":..}`. Check light and dark appearance.
