# Noches design direction: control plane, in color

Noches supervises coding agents running on several machines. The chrome must
answer at a glance: **what** is each session doing, **where** (project,
branch, device), **which agent**, and **does it need me**. The sidebar
follows T3 Code's thread card; the color language follows Cursor and T3:
color is plentiful but always *means* something.

## Rules

1. **Color encodes state and identity, never decoration.**
   - State: `crate::status_palette::SessionState` is the only source for
     status hues. Working is sky, Awaiting input is indigo, Completed (unseen)
     is emerald, and Failed is the theme's danger color. Queued and idle
     sessions are neutral.
   - Identity: harness marks keep their brand tint (Claude orange; the other
     marks are monochrome by design). Projects show their repository favicon
     or a colored monogram (`Shell::render_project_icon`).
   - Change: PR badges keep their state colors. Diff stats in the Changes pane
     use `theme.diff_add` and `theme.diff_del`; per-card diff counts in the
     sidebar are deferred to a later phase (see Card line 3).
   - Surfaces, text, and hairlines stay neutral theme tokens.
2. **One surface, hairline splits.** Panes are flush on the same shell
   backdrop as a lone session and are separated by 1px `theme.border`
   hairlines. Do not add opaque pane fills that darken split sessions.
3. **Metadata is monospace.** Branch, device, elapsed time, and the model use
   `theme.font_mono` at 11px. Titles use the UI face.
4. **Weight is hierarchy.** Titles use NORMAL weight, or MEDIUM when the
   session needs you. Project names and status labels use MEDIUM at 11.5 to
   12px.
5. **No mascots.** Buddy avatars are gone. Identity comes from the project
   badge and the harness mark.

Status colors are explicit hues with light and dark variants (like the
monogram palette), so every theme reads a state the same way. Do not hardcode
any other hex values in UI code.

## Sidebar (T3 Code thread card)

```
 ●●●  ▯  ← →                           +
 [dir] All projects ⌄         [⌕] [≡]
 Needs you
▌[N] noches              ◌ Awaiting input     line 1: project + status
    Fix auth token refresh                    line 2: title
    ⑂ feat/auth-refresh            #42  ✳     line 3: branch, PR, device, harness
 Running
 [N] noches                 ◌ Working 2m
    Port sidebar sections
    ⑂ design/control-plane              ✳
 Recent
 [W] website                          3h
    Pricing page copy pass
    ⑂ main                              ◎
```

### Card (three lines, 72px)

- Line 1 (18px):
  - A 16px project badge from `render_project_icon`, then the project name at
    11.5px MEDIUM in `text_muted`.
  - The status slot sits on the right: a 12px icon or loader plus a label at
    11.5px MEDIUM in the state color.
  - Working also shows elapsed time in mono (`2m`, `1h 4m`) if the start
    time is available.
  - Settled rows show the relative time in mono `text_faint`.
  - On hover the slot swaps to the Archive action, and jump hints take the
    slot as they do today.
- Line 2 (18px): the title at 13px. It is `text` at 0.9 opacity at rest, and
  full `text` plus MEDIUM when the session needs you.
- Line 3 (16px), all mono 11px:
  - Left: a git-branch glyph and the branch in `text_faint`, truncating.
  - Right, in order: the PR badge, the remote device name in `text_faint`
    (only when it is not the local device), and the harness mark at 13px in
    its brand tint.
  - Deferred (later phase): per-card `+a -d` diff counts between the PR badge
    and the remote device name. Today `WatchCheckoutDiffs` is opened lazily by
    the Changes pane for a single target device and streams full `CheckoutDiff`
    frames carrying up to 3 MiB of unified patch (`MAX_PATCH_BYTES`). Showing
    counts on sidebar cards needs a bounded summary-only engine stream
    (`{checkout_id, device_id, cwd, additions, deletions, updated_at}` without
    the patch), subscribed once per engine connection in `AppState` and keyed
    to cards via `changes::resolve_diff`-style checkout identity
    (`checkout_id` first, then `device_id + cwd`, then `cwd`).
- Text starts at the badge's left edge; there is no leading column on lines
  2 and 3. That is how T3 aligns the card.
- Needs-you rows keep the 2px bar in the list's side padding, colored by
  state (indigo for awaiting, danger for failed).
- The selected row keeps today's wash.

### State sections

Unchanged from v1: **Needs you** (awaiting input or failed), **Running**
(working or queued), then the rest in the chosen organization. **Recent** is
shown only for `InOneList` and only when a section above exists. Headers are
30px tall with the label bottom-aligned. Each non-empty header carries a
6px dot in its state color (indigo for Needs you, sky for Running) before the
label. `sidebar_visible_order` must match the rendered order.

## Panes

Shared shell surface, hairline dividers, and focus cues are unchanged from v1. The
pane header is a 36px row, `pl` 10px / `pr` 6px (the trailing inset pairs
with the titlebar's `TITLEBAR_ACTION_EDGE_INSET`), content left to right:

- The harness mark at 14px in its brand tint inside a fixed 16px column.
  A bound chat shows its configured harness; an unbound new-session pane
  shows the harness picked in its composer. When no harness resolves the
  column stays empty - never a placeholder glyph.
- The title at 13px MEDIUM (`text` when focused, `text_muted` when not).
- A 14px project badge, then the context in mono 11px `text_faint`:
  `{project}:{branch}`, plus ` · {device}` for a remote device.
- A status label only when it adds information the transcript does not
  already show: icon and label in the state color at 11.5px MEDIUM. Awaiting
  input and Failed show on every pane; unfocused panes in a split show all
  states; Working hides on the focused pane because the transcript's live
  activity line already carries it.
- The project action control: a quiet 24px ghost segment. With no action
  configured it is a play glyph at 14px with an "Add action" tooltip; with
  one configured it is `[icon] {name}` at 12px `text_muted`. Hover is the
  single `wash(0.11)` blend - no pills, no plus signs.
- Changes toggle and close as 24px icon buttons on a 2px in-group rhythm
  (14px glyphs, `text_muted`, tooltips + aria labels).

While the sidebar is collapsed (or mid-collapse) the pane at the window's
top-left adds `pane_header_leading_inset` of left padding so its mark and
title start `TITLEBAR_IDENTITY_GAP` past the titlebar cluster (traffic
lights, sidebar toggle, nav, and the "+" slot). Only that one pane (or
top-left tab strip) insets; the value rides the sidebar and titlebar
tweens, so it animates rather than jumping.

## Composer

The placeholder is "Message {Harness}…". The model chip keeps the harness
mark's brand tint, shows the model name without the `provider/` prefix at
NORMAL weight in `text_muted`, and shows reasoning in `text_faint`.

The context ring (16px, 1.8px stroke) sits left of the send button
whenever the harness reports a window. Its fill is `text_muted`, turning
`warning` at 75% and `danger` at 90%. Hovering opens the context card:
"Context window" with the percent in mono, a 4px usage bar, then mono
11px lines - `used / window tokens`, `left`, `Auto-compacts at ~N%`
(only when the harness reports the threshold), and session totals
(`in · out · cache`) when available. Pi reports through the Noches Pi
extension (`crates/harness/src/pi/noches-context-usage.ts`), which writes
snapshots the harness polls; `tokens: null` after compaction reads as
"Waiting for context usage", never 0%.

## Tool rows

Transcript tool rows tint the 14px icon by the identity of the action -
`crate::tool_palette::ToolFamily`, the same (dark, light) hue-pair pattern
as `status_palette`. Verbs stay `text_muted` (MEDIUM on card chips),
details stay `text_faint`, and connectors/badges stay neutral.

- Explore (read, search, glob/list, fetch, web search): muted teal
  `0x7cc4bd` dark / `0x2f7f78` light.
- Change (edit, write, patch): muted amber `0xd9b26f` / `0x946514`.
- Delegate (subagent spawn, "Wait for agents"): muted orchid
  `0xc9a0dc` / `0x8a4a9e`.
- Run, Plan/Todo, MCP/Unknown, Thinking: neutral `text_muted` - commands
  are the bulk of the column and keeping them quiet is what makes the
  tinted families legible.

These hues are deliberately desaturated and in different sectors than the
session status hues (sky/indigo/emerald); a tinted tool icon must never be
readable as session state.

Failure is reserved: a failed row keeps its verb/detail colors, the icon
turns `theme.danger`, and a trailing mono 11px `failed` tag in danger at
0.9 opacity follows the detail. The tree connector stays neutral. Running
rows keep the existing live treatment, neutral.

## Subagents

Three surfaces read one selector, `subagents_for(state, chat)`, which
distills a chat's spawn tool parts into `SubagentSummary` rows: id, title
(from the spawn's `description`, else "Agent"), agent type, model, a
four-phase status (Running / Started / Done / Failed), start and finish
instants, and a one-line result tail. `Started` is the honest state for
background spawns - a bare tool result never means Done there - and for
doc-less harnesses like Pi where the call's lifecycle is all we have.
Ordering: running first (oldest first), then finished (newest first).
Status hues come from `SessionState` only: sky equalizer Running,
emerald check Done (neutral once the thread has been opened), danger
triangle Failed, neutral dot Started.

- **Dock strip**: a single 28px row above the composer, sharing the
  composer container's px(SPACE_LG) inset so its left/right edges equal
  the pill's outer edges at every width, 6px above the pill. Leading `Agents` label (11.5px MEDIUM,
  `text_faint`) with a mono `{done}/{total}` count, then 24px pills
  (border only, `wash(0.06)` hover): 12px status glyph, truncated title
  at 12px `text_muted`, mono 11px elapsed. A trailing chevron toggles
  the Agents tab, and a `+N` pill opens it when pills overflow. The
  strip shows the latest turn's agents plus anything still running, and
  animates its height in.
- **Agents panel**: `RightSurface::Agents`, one "Agents" tab with the
  `BOT` icon. `Active` and `Done · N` sections (30px headers like the
  sidebar's), 44px rows: glyph, title + one-line tail, mono elapsed and
  `agent_type · model` right-aligned. Rows open the child thread. There
  is no bulk stop - the engine exposes no subagent-interrupt call.
- **Sidebar children**: under selected or pane-open cards with running
  agents only, up to three 22px rows rendered as extra lines INSIDE the
  card after line 3 (sharing the card's wash and radius; no tree stubs,
  no hairlines). Each row: 12px status glyph at the card's text-start x,
  6px gap, 12px `text_muted` title truncating, mono 11px `text_faint`
  elapsed flush to the card's right edge. A `+N more` row (no glyph,
  indented to the title start) selects the chat and opens the Agents tab.
  2px between line 3 and the first child, 4px bottom pad. Child clicks
  stop propagation and open the agent thread. Finished agents never
  nest; card height animates via the disclosure tween.

Opening an agent opens its thread, not a dead end: real sub docs go
through `add_subagent_surface`; doc-less results (Pi) render a frozen
single-entry snapshot titled from the spawn, so the row never opens an
empty transcript.
