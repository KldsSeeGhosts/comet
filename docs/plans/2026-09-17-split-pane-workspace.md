# Split-pane & split-view workspace modeled on super.engineering

Linear ticket draft — file under the Noches project when back at a desk.
Type: Feature. Priority: High.

## Goal

Give Noches a Superconductor-style workspace: panes inside a tab
(Cmd-D / Cmd-Shift-D), full split views with independent tab strips
(Cmd-Opt-D / Cmd-Opt-Shift-D), drag-to-edge splits, and per-worktree layout
persistence. Splits are a hard requirement.

## Source material

- Analysis bundle: `super-analysis/` in this repo — especially
  `06-tabs-splits-and-pip.md` (serialization format + interactions),
  `03-ui-layout-and-navigation.md` section 4 (pane chrome), and
  `12-porting-plan.md` (Phase 2 mapping).
- Verbatim persisted layout: `super-analysis/reference/session.json` —
  recursive `{split|leaf}` tree, `axis: horizontal|vertical`, float `ratio`,
  stable `pane_id`s, `content: primary-tab | tab`.
- Prior attempt on `main` branch (merge-base `a1adfde2`, v0.2.59):
  `crates/workspace` is a dependency-free `SplitNode<T>` layout engine with
  views -> tabs -> panes, revision-guarded compose, edge zones, and tests.
  The UI layer (`crates/ui/src/workspace/**`, ~4.5k lines) was built against
  the pre-sidebar shell and should not be ported.

## Approach

1. Port `crates/workspace` from `main` mostly verbatim; adjust
   `PaneState`/`TabState` to match Super's serialized tab objects (`kind`,
   `provider_key`, `session_id`, `messages_snapshot`, `permission_mode`,
   `title_sc_owned`). Validate round-trip against `reference/session.json`.
2. Computer-use pass on the live `super.engineering.app` to capture
   interaction truth that screenshots don't show: drop-zone threshold,
   divider hit area, double-click equalize, Cmd-D-with-no-provider behavior,
   ghost composer in unfocused panes, drag-from-pane-header vs
   drag-from-tab, focus-ring treatment.
3. New renderer in current `shell.rs` — open-design sidebar stays the
   session list; panes/splits live only in the content area. Reuse
   `frost.rs`, `edge_fade.rs`, `motion.rs`, `loaders.rs`.

## Actions/keybindings to match Super

- `SplitPaneRight` Cmd-D · `SplitPaneDown` Cmd-Shift-D · `SplitViewRight`
  Cmd-Opt-D · `SplitViewDown` Cmd-Opt-Shift-D · `CloseSplitView` Cmd-Opt-W —
  keep these exact action names for parity.
- New pane opens the tool-picker dropdown; empty view shows launcher cards +
  Recent.
- Pane header: provider icon + title + hover pop-out/maximize/close +
  context menu (Split pane right/down, Split view right/down, Tab layout >).
- Focused pane gets a soft blue ring; unfocused chat panes show "Click to
  focus chat" ghost composers.
- Divider: drag to resize, double-click to equalize; drag tab to pane edge =
  split preview, center drop = move, outer drop ring = full view split.

## Out of scope (this ticket)

- PiP window (Cmd-Shift-P) — separate ticket.
- `sc`-style local API/CLI orchestration — separate ticket.
- Provider hook pipeline — separate ticket.

## Acceptance

- `cargo test -p zeron-workspace` passes (ported + new format tests).
- Cmd-D / Cmd-Shift-D / Cmd-Opt-D / Cmd-Opt-Shift-D / Cmd-Opt-W all work in
  the current shell; layouts persist and restore per worktree across
  restart.
- Verified side-by-side against live Super for split/merge/equalize/drag
  behaviors.
