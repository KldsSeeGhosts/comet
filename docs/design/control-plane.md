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
