# Split-pane workspace — implementation scaffold & analysis

Companion to `2026-09-17-split-pane-workspace.md`. Analysis only; no code
written against it yet. All `file:line` refs are against
`upstream-v0.2.72` unless marked `main@` (old branch, merge-base `a1adfde2`).

## 0. Verdict

The plan is sound and the two hardest unknowns are now measured:

1. **The engine port is cheap and verifiable in isolation.**
   `main@crates/workspace` is 1,773 lines total (lib 601, tree 238, arrange
   353, persistence 180, tests 401), depends only on serde/serde_json/
   thiserror, and its public API already covers every interaction the plan
   names (see §2.1). It can land as PR 1 with zero UI risk.
2. **The UI integration is the real project.** The current shell renders
   exactly one `Entity<Transcript>` + one `Entity<Composer>`
   (`crates/ui/src/shell.rs:1294-1295`) and `render_main`
   (`shell.rs:7528`) has no tab/split concept. Multi-pane chat is a new
   hosting model, not a restyle. Everything else (actions, persistence,
   chrome) is incremental on existing patterns.

One **plan correction** required (§3.1): "port verbatim + round-trip against
`reference/session.json`" are in tension — the old engine's serde shape
differs from Super's persisted format on four axes. A thin serde bridge
solves it without polluting the engine.

---

## 1. Ground truth

### 1.1 Current shell (what receives the feature)

- `Shell` struct: `crates/ui/src/shell.rs:1291`. Holds **one**
  `transcript: Entity<Transcript>` (:1294) and **one**
  `composer: Entity<Composer>` (:1295); selecting a chat re-targets them.
  Per-chat panel flags: `SessionPanels` (:1455, in-memory only).
- Content area = `render_main` (`shell.rs:7528`): `Route::Chat |
  Route::Settings` (:445); Chat → single transcript or new-thread hero.
  **No tabs, no splits.**
- Closest in-repo precedents to reuse:
  - Right-pane **surface tab strip** with drag-reorder, 112px chips, ghost
    drag: `render_right_tab_strip` (`shell.rs:8509`), `RightSurface` (:468),
    `RightTabDrag/State` (:734/744), `SurfaceTabGhost` (:752).
  - Divider resize seams: `PaneResizeKind` (`shell.rs:710`).
  - Empty-surface picker: `render_surface_picker` (`shell.rs:8332`).
  - FLIP resort: `resort_offsets` + `RESORT.animation()` (shell.rs,
    per AGENTS.md §3).
- Actions: `actions!` shell namespace at `shell.rs:68`, param action
  `JumpSession(usize)` at :213. Keybinding init fn ~`shell.rs:300-394`
  (`cx.clear_key_bindings()` → re-init → `cx.bind_keys([...])`).
  Rebindable combos live in `ShortcutId` (`settings.rs:721`) +
  `KeymapConfig` (`settings.rs:834`). **No Cmd-D binding exists today;
  Cmd-W = CloseWindow (`app_menus.rs:153`).**
- Persistence convention: `SettingsStore` (`settings.rs:213`) writes
  `{data_dir}/ui-settings.json` (atomic temp+rename, debounced); data_dir =
  `~/.zeron` (`apps/zeron/src/paths.rs:6`). Durable choices persist there;
  view state deliberately stays in-memory (`SessionPanels`). There is **no
  per-worktree/space layout file today** — one must be created.
- Theming: `Theme` at `crates/ui/src/theme.rs:591`. Has
  `accent/accent_strong/accent_wash`, `border/border_strong`,
  `selection`, status colors `danger/warning/success/busy` (:677-691),
  helpers `wash()` (:1492), `ink()`, `hairline()` (~:1489, documented for
  rings). **No dedicated `focus_ring` token** — rings route through
  `hairline()`.
- Icons: `icon(path) -> Svg` (`crates/ui/src/icons.rs:252`); ~116 embedded
  SVGs. Harness brand marks already present: `CLAUDE_MARK`, `OPENAI_MARK`,
  `CURSOR_MARK`, `DEVIN_MARK`, `PI_MARK`, `OPENCODE_MARK`
  (`icons.rs:214-221`), `claude_brand()` → `#D97757` (:245).
- `crates/ui/src/shell/spaces.rs`: sidebar only (space filter, sessions
  list). A "space" = (device, folder) pair — this is the natural analog of
  Super's per-worktree key.
- `crates/workspace` does **not exist** on this branch; workspace members:
  theme, proto, preview, doc, sync, harness, engine, rpc, update, ui,
  syntax, apps/zeron.

### 1.2 Engine to port (`main@crates/workspace`, `zeron-workspace`)

- Model: `WorkspaceLayout { root: SplitNode<ViewId>, active_view_id,
  views: BTreeMap<ViewId, ViewLayout>, next_id, revision }`; each view has
  `tabs: BTreeMap<TabId, SplitTabLayout>`; each tab has a pane tree
  `SplitNode<PaneId>` + `active_pane_id` + `primary_pane_id` +
  `panes: BTreeMap<PaneId, PaneState>`. This matches Super's three-level
  hierarchy (view → tab-strip → pane tree) 1:1 conceptually.
- Ops already implemented: `split_view`, `add_tab`, `split_pane`,
  `close_pane/tab/view`, `focus_view/tab/pane`, `move_pane`, `swap_panes`,
  `move_tab`, `pane_to_tab`, `reorder_tab`, `pane_to_view`, `tab_to_view`,
  `merge_tab`, `set_view_ratio`, `set_pane_ratio`, `validate`,
  revision-guarded `compose`, and `edge_zone` (20% normalized-edge detector
  — exactly the drag-to-edge split primitive).
- Guardrails: `MIN_RATIO 0.1 / MAX_RATIO 0.9`, `MAX_DEPTH 16`, `MAX_VIEWS
  64 / MAX_TABS 512 / MAX_PANES 4096`; `deny_unknown_fields` everywhere;
  `IdPolicy` + `load/save` in persistence.rs.
- Reference-only (do **not** port, mine instead): `main@crates/ui/src/
  workspace.rs` (3,012 L), `workspace/control.rs` (3,200 L — divider/drag
  controls), `workspace/animation.rs` (316 L), `workspace/launch.rs`
  (203 L), `workspace/tests.rs` (997 L). Built against the pre-sidebar
  shell per the plan; total ≈ 7.7k lines (plan undercounts at ~4.5k).

### 1.3 Super's persisted format (`super-analysis/reference/session.json`)

Per worktree value: `tabs[]` (full tab objects), `active_tab` (index),
`split_layouts[]` parallel to tabs (may contain `null`), each
`{root, active_pane_id, primary_pane_id}`. Nodes:

```json
{"kind":"split","axis":"horizontal|vertical","ratio":0.5,"first":…,"second":…}
{"kind":"leaf","leaf":{"pane_id":4,"content":{"kind":"primary-tab"} | {"kind":"tab","tab":{…full tab object…}}}}
```

Tab objects: `tab_uuid`, `kind: api-chat|terminal`, `provider_key`,
`provider_profile_selection`, `preferred_model_id`, `session_id`,
`conversation_id`, `messages_snapshot[]`, `thinking_enabled`,
`permission_mode`, `title`, `title_sc_owned`, `context_window`,
`last_context_tokens`. `content.kind = "primary-tab"` = "renders
`tabs[active_tab]`" — one pane per tab-strip is special.

---

## 2. Ordered workstreams

Order chosen so every step compiles green, ships standalone, and the
risky unknowns (computer-use truth, engine round-trip, multi-entity
hosting) are front-loaded.

**WS0 — Interaction truth pass (computer-use, no code).** Drive live
`super.engineering.app` per the plan's step 2 list: drop-zone threshold vs
`edge_zone`'s 20%, divider hit-area px, double-click equalize behavior,
Cmd-D with no provider (what opens?), ghost-composer exact copy/hit
behavior, drag-from-pane-header vs drag-from-tab affordances, focus-ring
radius/width. Append findings to `super-analysis/` as
`13-interaction-truth.md`. Gates WS4/WS6 details.

**WS1 — Engine port + format bridge (pure Rust, no UI).** Revive
`crates/workspace` verbatim from `main@a1adfde2`; add workspace member.
Add a `super` serde bridge module (in the crate or as `crates/workspace`
feature): `SuperWorktreeLayout ⇄ WorkspaceLayout` conversion + round-trip
tests against `reference/session.json` (see §3.1 for the four deltas).
Extend `PaneState`/`TabState` toward Super's tab-object fields
(`kind`, `provider_key`, `session_id`, `permission_mode`,
`title_sc_owned`…). Acceptance: `cargo test -p zeron-workspace`.

**WS2 — Pane-host foundation in the shell (the architectural PR).**
Introduce a `crates/ui/src/pane/` module (model + GPUI render +
hit-testing, per the porting plan's "concrete first PR slice"):
- `WorkspaceState` on `Shell` wrapping `WorkspaceLayout` + focus +
  per-pane entity caches (`HashMap<PaneKey, Entity<Transcript>>`; ghost
  panes need no live `Composer` — only the focused pane renders the real
  composer).
- Rewrite `render_main`'s Chat branch to render the workspace tree:
  view split (recursive, flex axis + ratio) → view = tab strip + active
  tab's pane tree → pane = header + content host (chat transcript,
  terminal, file, browser, diff…).
- Focus model: focused pane id, blue ring, click-to-focus, keyboard
  follows focus.
- Actions + keybindings (exact names, per plan):
  `workspace::{SplitPaneRight, SplitPaneDown, SplitViewRight,
  SplitViewDown, CloseSplitView}` → ⌘D/⇧⌘D/⌥⌘D/⌥⌘⇧D/⌥⌘W; register in the
  bind_keys fn (`shell.rs:300-394`) + `ShortcutId`/`KeymapConfig`
  (`settings.rs`) for rebindability.
- **Single-pane parity gate:** with no splits, rendering must be
  byte-identical to today (no extra wrappers hit the hot path).

**WS3 — Dividers + tab strips.** Divider drag (ratio tween via
`motion.rs`), double-click equalize, clamped by engine
`MIN/MAX_RATIO`. Per-view tab strip (adapt `render_right_tab_strip`
patterns: chips, scroll, `SurfaceTabGhost`), new-pane → tool-picker
dropdown, empty-view launcher cards + Recent
(`render_surface_picker` precedent), FLIP resort on reorder.

**WS4 — Drag & drop splits.** Pane-rect registry collected during render
(`HashMap<PaneId, Bounds>`); tab drag over `edge_zone` → split preview
(half-pane wash), center drop → move, outer drop ring → view split
(`tab_to_view`); pane-header drag → re-dock as tab (`pane_to_tab`);
cross-view moves (`move_tab`, `merge_tab`). Hit-test/drop-target
resolution as pure functions for unit tests.

**WS5 — Persistence.** New store following `SettingsStore` pattern:
`{data_dir}/workspace-layout.json`, keyed by space id
(worktree ↔ space mapping; `active_selection` style from session.json is
the model). Save debounced on revision change + drag end; restore on
space select + boot. Decisions (§3.3): `messages_snapshot` deferred;
terminal panes restore as *tabs/dirs only, no PTY reattachment* (porting
plan's guidance).

**WS6 — Chrome & motion polish.** Pane headers (provider mark via
`icons.rs` + title + hover pop-out/maximize/close + context menu with
`Tab layout ▸` presets — `arrange.rs` already provides arrangement
helpers); ghost composers ("Click to focus chat"); focus ring; drop-ring
animation; working/awaiting indicators via `loaders.rs` + status colors.

**WS7 — Verification.** `cargo test -p zeron-workspace`; keybinding
matrix in current shell; side-by-side vs live Super
(split/merge/equalize/drag); vibrancy + opaque fallbacks; no-regression
pass on single-chat layout and the right-pane tab strip (shares
patterns).

---

## 3. Key findings, discrepancies & decisions

### 3.1 Format mismatch (plan correction needed)

Old engine ≠ Super format on four axes; "port verbatim" alone cannot
round-trip `session.json`:

| Axis | Old engine (`SplitNode<T>`) | Super `session.json` |
|---|---|---|
| Node tag | `"type": "split"\|"leaf"` | `"kind": "split"\|"leaf"` |
| Axis field | `horizontal: bool` | `axis: "horizontal"\|"vertical"` |
| Leaf shape | `Leaf { content: T }` (id only; panes in side map) | `Leaf { leaf: { pane_id, content: {kind: primary-tab\|tab{…}} } }` (content embedded) |
| Identities | `TabId/PaneId` u64 from `next_id` counter | `tab_uuid` strings; sparse pane ids (2,3,4,7,8,9) |

**Recommendation:** keep the engine's internal representation (it carries
invariants: ratios, depth, revision guards) and add a boundary-only
`SuperLayout` serde struct + `From/TryFrom` conversions. Round-trip test:
parse `session.json` per-worktree value → convert → convert back →
compare (and golden-file the canonical re-serialization). `deny_unknown_fields`
stays; the bridge absorbs Super's extra fields
(`model_reasoning_effort`, `context_window`, …). Alternative (changing
`SplitNode`'s serde to Super's shape) contaminates the engine and breaks
its existing tests for no gain.

### 3.2 Multi-entity hosting is the core risk (see §5 R1)

`Transcript` already supports re-targeting via `chat_id: Option<String>`
(`transcript.rs:2595`) and per-chat saved viewports (`:2568`), so *N
transcript entities is a natural extension*. `Composer`/`composer_dock`
assume one dock (shared clock/geometry, `focus_composer` helpers). Ghost
composers in unfocused panes are the escape hatch the design itself
provides: only the focused pane hosts the live composer; unfocused panes
render a static ghost strip that focuses on click. Keep that invariant.

### 3.3 Decisions to lock before WS2/WS5 (recommendations in place)

1. **`messages_snapshot` in persisted layout:** defer. Zeron chat content
   lives in CRDT docs via engine; restoring by `session_id`/`conversation_id`
   is already fast. Revisit only if restore-perf disappoints. (Bridge
   accepts-and-drops the field so foreign files still round-trip
   structurally.)
2. **Terminal panes:** pane-hosted terminals get fresh PTYs at worktree
   root; layout restore recreates tabs/dirs but never reattaches
   processes (matches porting-plan note; avoids zombie PTYs).
3. **⌘W semantics:** do **not** rebind ⌘W from CloseWindow to Close Tab —
   out of plan scope and a UX regression risk. ⌥⌘W = CloseSplitView only.
   Super parity note recorded, decision deferred.
4. **`focus_ring` token:** try `accent` + `hairline()` first (zero new
   token); add a dedicated `focus_ring` field in `theme.rs` only if the
   "soft blue" must stay fixed regardless of accent theme.
5. **Layout file location:** separate `{data_dir}/workspace-layout.json`
   (not `ui-settings.json`) — the tree can grow large and needs its own
   migration/versioning; follows the store pattern in `settings.rs:213`.

### 3.4 Naming/terminology map (Super → Noches)

| Super | Noches |
|---|---|
| worktree (`workspace_id/project_id/worktree_name` key) | space (id = (device, folder) pair) |
| primary-tab pane | pane rendering `tabs[active_tab]` |
| view | top-level split node content with own tab strip |
| launcher dropdown | tool picker (reuse `render_surface_picker` model) |
| tab object | `TabState` (engine) + harness session binding |

---

## 4. Files to touch

### New

| Path | Contents |
|---|---|
| `crates/workspace/*` (revive from `main@a1adfde2`) | engine: `lib.rs`, `tree.rs`, `arrange.rs`, `persistence.rs`, `tests/layout.rs` + new `super.rs` bridge + format tests |
| root `Cargo.toml` | add `crates/workspace` to workspace members |
| `crates/ui/src/pane/mod.rs` | `WorkspaceState`, pane entity caches, focus model |
| `crates/ui/src/pane/render.rs` | recursive split renderer, pane chrome host, dividers |
| `crates/ui/src/pane/hit_test.rs` | pure drop-target/divider math (unit-testable), wraps `edge_zone` |
| `crates/ui/src/pane/drag.rs` | drag state machine: tab→edge split, header→re-dock, drop ring |
| `crates/ui/src/pane/chrome.rs` | pane header, ghost composer, tab strip per view, context menu |
| `crates/ui/assets/icons/` | ~4–6 SVGs: split-right, split-down, pop-out, maximize, tile presets (Solar set) |

### Modified (crates/ui)

| File | Change |
|---|---|
| `crates/ui/src/shell.rs` | `render_main` (:7528) Chat branch → workspace renderer; `Shell` fields (:1291+): workspace state, pane entity caches, drag state; actions (:68, :213) + bind_keys (:300-394); overlays for pane context menu |
| `crates/ui/src/settings.rs` | `ShortcutId` (:721) + `KeymapConfig` (:834): 5 new shortcuts; deprecate nothing new |
| `crates/ui/src/theme.rs` | optional `focus_ring` token (§3.3.4); verify status colors (:677-691) cover working/awaiting/errored |
| `crates/ui/src/icons.rs` | register new split/pop-out/tile SVGs via `icon_assets!` (:22); brand marks already exist (:214-221) |
| `crates/ui/src/shell/tabs.rs` | view-tab-strip helpers OR leave untouched and put strips in `pane/chrome.rs` (preferred — `tabs.rs` is nav-only now) |
| `crates/ui/src/shell/spaces.rs` | minimal: layout restore hook on space select; **no sidebar restyle** (AGENTS.md sidebar work stays separate) |
| `crates/ui/src/composer.rs` | ghost variant: render-only strip + click-to-focus intent (no second live dock) |
| `crates/ui/src/transcript.rs` | none expected — already per-chat re-targetable (:2595, :2568) |
| new `crates/ui/src/workspace_layout_store.rs` (or in `settings/`) | `{data_dir}/workspace-layout.json` store, `SettingsStore` pattern |

---

## 5. Risks

**R1 — Single-entity composer/dock assumption (high, WS2).**
`composer_dock.rs` shared clock/geometry and `focus_composer` helpers
(`shell/tabs.rs:44`) assume one composer. Mitigation: ghost-composer
invariant (§3.2); keep the live dock owned by the focused pane only;
audit `composer_dock/panel_handoff.rs` during WS2.

**R2 — Format bridge subtleties (medium, WS1).** Sparse Super pane ids
(4,7,2,3) vs engine `next_id` allocation; `split_layouts[]` containing
`null` (tab with no split — engine must represent "no split tree" vs
single-leaf tab); ratio range differences (engine clamps 0.1–0.9). All
mechanical, but round-trip tests must cover nulls, sparse ids, and
out-of-range ratios before any UI consumes the engine.

**R3 — Drag/drop across nested splits in GPUI (medium, WS4).** Hit-testing
needs pane rects at cursor time; rects only exist during render. Mitigation:
pane-rect registry collected during render (pattern likely present in
`main@workspace/control.rs` — mine it); keep drop-resolution pure and
unit-tested.

**R4 — Performance with N live transcripts (medium, WS2+).** Streaming
markdown × N panes. Mitigation: inactive tabs unmount (viewport restored
via `saved_viewports`), practical pane cap surfaced in UI well below
engine's 4,096, terminal panes pause scrollback when unfocused.

**R5 — Keybinding collisions (low, WS2).** ⌘D/⇧⌘D/⌥⌘D/⌥⌘⇧D/⌥⌘W are
currently unbound in-app (⌘W ≠ ⌥⌘W), but macOS system/menu interference
and composer-context capture need a check in `bind_keys`; register in the
same rebindable `KeymapConfig` system so users can escape collisions.

**R6 — Collision with open-design sidebar work (process, all WS).**
AGENTS.md's sidebar transposition and this feature both land in
`shell.rs`/`spaces.rs`. Disjoint regions today (sidebar fns vs
`render_main`), but sequence PRs to land sidebar work or rebase before
WS2, and keep split code inside `crates/ui/src/pane/` to minimize
overlap.

**R7 — Persistence write amplification (low, WS5).** Drag generates
revision churn. Mitigation: debounced save + save-on-drag-end; the
`SettingsStore` debounce pattern already exists.

**R8 — Engine port drift (low, WS1).** Verbatim port will need small
edits (workspace member paths, possible `#![deny]` lint updates, clippy
on this branch's toolchain). Keep the port commit byte-faithful, follow
with a separate bridge commit so review can diff both cleanly.

---

## 6. Suggested PR sequence

Each PR is independently mergeable, keeps the app shippable, and maps to
acceptance criteria incrementally.

| # | PR | Scope | Gate |
|---|---|---|---|
| 1 | `feat(workspace): port zeron-workspace layout engine + Super format bridge` | `crates/workspace` verbatim from `main@a1adfde2`; workspace member; `super.rs` bridge; round-trip tests vs `reference/session.json` | `cargo test -p zeron-workspace` |
| 2 | `feat(ui): workspace split actions + keybindings (no-op)` | 5 `workspace::` actions, exact names; `ShortcutId`/`KeymapConfig` entries; bindings; no-op handlers | keymap smoke; zero behavior change |
| 3 | `feat(ui): pane-host renderer for the content area` | `crates/ui/src/pane/{mod,render}.rs`; `render_main` Chat branch renders workspace tree; per-pane transcripts; focus ring + click-to-focus; keyboard splits/close work end-to-end | single-pane visual parity; ⌘D splits with live transcripts |
| 4 | `feat(ui): divider drag + double-click equalize` | `pane/render.rs` dividers; ratio tween via `motion.rs`; clamps; `hit_test.rs` pure math + unit tests | resize/equalize vs live Super |
| 5 | `feat(ui): per-view tab strips, tool picker, empty-view launcher` | `pane/chrome.rs`; adapt right-tab-strip chips + FLIP; new-pane tool picker; launcher cards + Recent | tab switch/create/close; empty view |
| 6 | `feat(ui): drag-to-split, drop ring, re-dock, cross-view moves` | `pane/drag.rs`; `edge_zone` previews; center-move / ring-view-split; header drag re-dock | side-by-side drag behaviors vs Super |
| 7 | `feat(ui): per-space layout persistence` | `workspace-layout.json` store; save on revision/drag-end; restore on select + boot; terminal no-reattach rule | layouts survive restart per space |
| 8 | `feat(ui): pane chrome & motion polish` | pane header (provider marks, hover controls, context menu, `Tab layout ▸` presets via `arrange.rs`); ghost composers; drop-ring/entrance motion; a11y | WS7 checklist + acceptance section of the plan |

PR 2 is optional as a standalone (could fold into PR 3) but keeps the
action-name parity reviewable in isolation, which the plan calls out
explicitly ("keep these exact action names for parity").

Out of scope (unchanged from plan): PiP (⇧⌘P), `sc`-style API/CLI,
provider hook pipeline.

---

## 7. Token / motion mapping to existing GPUI primitives

| Design element (Super) | Noches token / primitive |
|---|---|
| Focused-pane soft blue ring | `theme.accent` border + `hairline()` ring (theme.rs:1489); optional `focus_ring` token (§3.3.4); 120ms alpha tween via `motion.rs` |
| Divider idle / hover / drag | `hairline()` / `theme.border_strong` / `theme.accent`; width + hit-area from WS0 capture; ratio tweened with `motion.rs` |
| Split drop preview (half-pane) | `theme.accent_wash` fill + 1px `accent` edge on the split side |
| Outer drop ring (view split) | inset rounded ring: `accent` 1.5px border + `wash(0.08)` fill; slight scale-in via `motion.rs` |
| Ghost composer (unfocused chat) | `theme.input_bg` + `hairline()` rounded strip, `text_faint` label; cursor pointer; click → `focus_pane` + composer focus |
| Pane header | `theme.surface` bar, `text_muted` title, hover controls as `wash()` pills; close hover tinted `danger_muted` |
| Provider marks in headers/tabs | `icons.rs` marks (:214-221) via `icon()` (:252); Claude tint `claude_brand()` (:245) |
| Working / awaiting / errored states | `loaders::mini_glyph_spinner` / `theme.warning` pulse / `theme.danger`; done → `theme.success` (status colors theme.rs:677-691) |
| Tab chips + reorder | adapt `render_right_tab_strip` (shell.rs:8509): 112px chips, `SurfaceTabGhost` (:752), `EdgeFade`; FLIP via `resort_offsets`/`RESORT.animation()` |
| Entrance/reorder motion | `motion.rs` tweens + FLIP resort (AGENTS.md §3 pattern) |
| Glass/fade invariants | unchanged: `frost.rs` vibrancy, `edge_fade::edge_faded()`; splits live only in content area — sidebar untouched |

---

## 8. Acceptance mapping (from the plan)

- `cargo test -p zeron-workspace` passes → PR 1, kept green thereafter.
- ⌘D / ⇧⌘D / ⌥⌘D / ⌥⌘⇧D / ⌥⌘W work in the current shell → PR 2+3.
- Layouts persist & restore per worktree (space) across restart → PR 7.
- Verified side-by-side against live Super for split/merge/equalize/drag
  → WS0 truth doc + PR 4/6 gates.
