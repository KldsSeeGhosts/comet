# Noches split-pane fidelity

## Recon

Completed against:

- Installed `super.engineering` bundle `com.zarifpour.superconductor`.
- Installed `Noches` bundle `app.noches.desktop`.
- `super-analysis/13-interaction-truth.md`.
- `super-analysis/screenshots/06-terminal-split-right.png`.
- `super-analysis/screenshots/07-pane-context-menu.png`.
- `super-analysis/screenshots/08-tab-layout-submenu.png`.

The installed Super app currently has no project configured, so live split
interactions are unreachable in that installation. The earlier live
computer-use capture and its screenshots are the reference for those states.

Two direct Noches drags were exercised from the second sidebar session into
the existing right pane, once at its center and once near its left edge.
Neither changed the pane's session or layout. The current installed view has
two side-by-side regions, no visible pane headers, and no visible close-view
control.

### Confirmed implementation defects

1. The outer chat dropzone and inner workspace outlet both process the same
   drag move while workspace mode is active. The outer handler clears or
   replaces the inner handler's state, defeating hysteresis and causing
   flickering or incorrect previews.
2. A sidebar session from another space can show a valid preview and then
   silently fail at commit.
3. A sidebar drop on a tab strip computes an insertion point but appends the
   new tab instead.
4. Dragging a session already present in the layout creates a duplicate
   session binding instead of focusing the existing pane.
5. The committed payload is not checked against the source that produced the
   stored drop plan.
6. The default screen models its one pane as the full workspace boundary, so
   every edge resolves to a split view. There is no distinct pane-edge band
   and outer view-split ring.
7. A single-pane split view has no pane header. The only direct close is a
   small hover-only tab close, while the explicit Super-style close-view
   control is missing.
8. Pop-out and maximize controls are visible but inert, and pressing them can
   start a pane drag.
9. Closing panes, tabs, or views chooses the first remaining leaf rather than
   the nearest sibling, causing focus and composer jumps in larger layouts.
10. The app menu and pane menu do not expose the complete close and layout
    actions shown by Super.

No source files were edited during recon.

## Implementation

Completed in the workspace and pane layout layers:

- Gave the workspace outlet sole ownership of workspace-mode drag state.
- Rejected cross-space sidebar sessions before displaying a drop preview.
- Preserved tab-strip insertion positions on sidebar drops.
- Focused an existing session pane instead of creating a duplicate binding.
- Cancelled commits whose payload no longer matches the stored drop plan.
- Added a distinct 18px outer workspace ring for view splits, leaving the
  remainder of each pane's 20% edge zone for pane splits.
- Added distinct pane-half and view-ring previews.
- Made pane close controls visible at rest and removed inert pop-out and
  maximize controls.
- Added an explicit close-view control to each closable view strip.
- Moved workspace tab strips below the unified titlebar.
- Retargeted pane, tab, and view close focus to the nearest surviving sibling.

## Verification

Passed:

- `cargo check --locked -p zeron-ui --lib`
- `cargo test --locked -p zeron-workspace` - 25 passed
- `cargo test --locked -p zeron-ui --lib pane::hit_test`
- `cargo test --locked -p zeron-ui --lib pane::tests` - 31 passed
- `git diff --check` for all split-pane implementation files
- `codesign --verify --deep --strict target/package/Noches.app`

Native checks against the reinstalled app confirmed:

- The workspace strips render below the titlebar.
- Both top-level views expose their close-view control.
- Dragging an already-open session into a pane focuses it without duplication.
- A cross-space sidebar session drop is rejected without changing the layout.
- The installed binary exactly matches the packaged binary.

Packaged and installed:

- `/Applications/Noches.app`
- `target/package/noches-0.2.72-macos-arm64-app.tar.gz`
- `target/package/noches-0.2.72-macos-arm64.dmg`

## Visual fidelity revision

The user supplied a restored two-pane Noches workspace and compared it with
Super's split view. Recon found four remaining presentation defects:

- The view tab strip shrink-wraps its chip instead of spanning the full view.
- The pane field has no inset frame, so split panes read as one continuous
  sheet instead of separate rounded cards.
- The gap and border hierarchy are too weak to distinguish pane ownership.
- Dormant pane chrome, ghost composer controls, and footer status text fall
  below useful contrast on the dark theme.

Reference evidence:

- User-supplied Image 3, captured 2026-09-17.
- `super-analysis/screenshots/06-terminal-split-right.png`.
- `super-analysis/10-design-system.md`.
- `super-analysis/13-interaction-truth.md`.

## Visual fidelity extraction

The native GPUI layout has no DOM or CSSOM, so extraction used authored GPUI
rules, theme tokens, saved Super screenshots, and the live interaction report.
The implementation targets are now fixed:

- 30px full-width view strip.
- 6px inset pane field.
- 8px divider hit region with a 1px visual line.
- Independent 8px rounded pane cards with their own background and border.
- `text_muted` for dormant controls and `border_strong` for the ghost composer.

Artifacts live under `02-extraction/`, with the pane measurements in
`fragments/native-workspace.layout.json`.

## Visual fidelity design and architecture

The design spec and component map constrain this pass to two render files.
No workspace-tree, persistence, drag-commit, or close behavior changes are
needed.

- `03-design-spec/DESIGN.md`
- `03-design-spec/assertions.json`
- `04-architecture/file-tree.md`
- `04-architecture/component-map.md`

## Visual fidelity implementation and QA

Implemented the view strip, pane field, pane card, and dormant-control fixes
in `pane/chrome.rs` and `pane/render.rs`.

Verification passed:

- `cargo check --locked -p zeron-ui --lib`
- 31 `pane::tests`
- 14 `pane::hit_test` tests
- 7 of 7 native style assertions
- Native CUA inspection at 1152x768
- Package signature and installed binary hash

The rebuilt Noches 0.2.72 is installed at `/Applications/Noches.app`.

## Focus and empty-pane revision

Removed focus-driven identity changes from the unified titlebar and workspace
tab chip. The titlebar now shows the selected workspace and device, while a
tab chip derives its label from the tab's stable first pane.

Removed dormant composer replicas and duplicate centered labels from
unfocused session-less panes. Splitting now creates and focuses Noches'
standard session-less chat pane directly, with provider and model selection
available in its real composer instead of an intermediate tool picker.

Verification passed:

- `cargo check --locked -p zeron-ui --lib`
- 31 `pane::tests`
- 14 `pane::hit_test` tests
- `git diff --check` for the changed pane and shell files
- Package signature and bundle identifier validation
- Native CUA inspection at 1152x768 after reinstall

## Focus-layout correction

The first focus revision changed the unified title instead of removing the
duplicate identity row, and it left the live composer absolutely overlaid on
the transcript. The corrected implementation:

- Suppresses the unified title identity block throughout workspace mode.
- Keeps the view tab strip as the single identity and control row.
- Places the focused composer in normal flex layout as the pane footer.
- Clips the transcript to the remaining pane body above that footer.
- Resets the transcript's dock clearance to zero in workspace mode.

Native QA switched focus from the left chat to the right chat and back. The
tab identity stayed fixed, the transcript stayed inside its pane, and no
messages painted beneath the composer or footer. A new `Command-D` split also
placed the normal new-session composer at the pane bottom without a picker.
