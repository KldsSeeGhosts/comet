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
   - Change: diff counts use `status_palette::diff_colors` (green `+N`, red
     `-N`). PR badges keep their state colors.
   - Surfaces, text, and hairlines stay neutral theme tokens.
2. **One surface, hairline splits.** Panes are flush and opaque (`theme.bg`)
   and are separated by 1px `theme.border` hairlines.
3. **Metadata is monospace.** Branch, diff counts, device, elapsed time, and
   the model use `theme.font_mono` at 11px. Titles use the UI face.
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
    ⑂ feat/auth-refresh     #42 +18 -3  ✳     line 3: branch, PR, diff, harness
 Running
 [N] noches                 ◌ Working 2m
    Port sidebar sections
    ⑂ design/control-plane        +210 -96  ✳
 Recent
 [W] website                          3h
    Pricing page copy pass
    ⑂ main                                  ◎
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
  - Right, in order: the PR badge, diff counts `+a -d` in diff colors (only
    when known and nonzero), the remote device name in `text_faint` (only
    when it is not the local device), and the harness mark at 13px in its
    brand tint.
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

Flush surface, hairline dividers, and focus cues are unchanged from v1. The
pane header, left to right:

- The harness mark at 14px in its brand tint.
- The title at 12.5px MEDIUM (`text` when focused, `text_muted` when not).
- A 14px project badge, then the context in mono 11px `text_faint`:
  `{project}:{branch}`, plus ` · {device}` for a remote device.
- A status label when not idle: icon and label in the state color at 11.5px
  MEDIUM, the same as the sidebar slot.
- The action control, the changes toggle, and close.

## Composer

The placeholder is "Message {Harness}…". The model chip keeps the harness
mark's brand tint, shows the model name without the `provider/` prefix at
NORMAL weight in `text_muted`, and shows reasoning in `text_faint`.
