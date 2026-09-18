# Design system: Noches split panes

## Visual theme

Noches split panes are dense, dark, and quiet. A full-width tab strip frames
an inset field of independent near-black pane cards. Borders stay crisp,
inactive controls remain readable, and the violet accent appears only on the
focused pane and active tab.

## Colors

| Token | Hex | RGB | Role | Confidence | Evidence |
|---|---|---|---|---|---|
| `pane-bg-dark` | `#060606` | `rgb(6,6,6)` | Pane card fill | high | `css-variables.json` dark `bg` |
| `workspace-bg-dark` | `#0d0d0d` | `rgb(13,13,13)` | Workspace and strip field | high | `css-variables.json` dark `surface` |
| `text-primary-dark` | `#e5e5e5` | `rgb(229,229,229)` | Active labels | high | `theme.rs` `neutral(0.922)` |
| `text-muted-dark` | `#a1a1a1` | `rgb(161,161,161)` | Dormant labels and controls | high | `theme.rs` `neutral(0.708)` |
| `text-faint-dark` | `#737373` | `rgb(115,115,115)` | Disabled-only text | high | `theme.rs` `neutral(0.556)` |
| `border-dark` | `rgba(255,255,255,.14)` | `rgba(255,255,255,.14)` | Pane and ghost-composer edges | high | `theme.rs` `border_strong` |

## Gradients

No gradient belongs to the structural split-pane chrome. Drop previews use a
flat accent wash.

## Typography

| Level | Font | Size | Weight | Color |
|---|---|---:|---:|---|
| View tab | UI sans | 11px | 500 active, 400 inactive | primary or muted |
| Pane header | UI sans | 11px | 500 | muted |
| Ghost composer | UI sans | 12px | 400 | muted |
| Ghost control | UI sans | 10px | 400 | muted |

## Spacing scale

| Token | Value | Usage |
|---|---:|---|
| `space-tight` | 4px | Tab-strip item gap |
| `space-pane-inset` | 6px | Pane field inset |
| `space-control` | 8px | Divider hit region and control rhythm |
| `space-pane-label` | 10px | Header and ghost-composer padding |

## Border-radius scale

| Token | Value | Usage |
|---|---:|---|
| `radius-control` | 6px | Tab and header controls |
| `radius-pane` | 8px | Independent pane cards and previews |
| `radius-composer` | 12px | Ghost composer |
| `radius-pill` | 9999px | Provider and model pills |

## Shadows

Split-pane cards use borders and surface contrast, not drop shadows.

## Effects and backdrop filter

The workspace may retain native window frost. Each pane card paints its own
opaque theme background so transcript and inactive controls remain legible.

## Motion

Hover color changes use the existing Noches motion blend. Divider resizing
stays continuous and drop previews follow the pointer without a transition.

## States

| Element | State | Delta |
|---|---|---|
| Pane card | focused | Border changes from strong hairline to accent |
| Header control | hover | Text changes from muted to primary, wash appears |
| Tab chip | active | Raised wash, border, and 2px accent underline |
| Ghost composer | dormant | Muted text on an input plate with strong border |

## Layout

| Pattern | Value |
|---|---|
| Titlebar reserve | 38px |
| View strip | 30px, full view width |
| Pane field inset | 6px on every edge |
| Pane divider hit region | 8px |
| Divider visual line | 1px centered in hit region |
| Pane card radius | 8px |
| Pane header rule | Hidden for one pane, visible for two or more |

There are no responsive web breakpoints. The GPUI split tree distributes
available native-window space by persisted ratios.

## Assets

| Asset | Path | Usage |
|---|---|---|
| Close icon | `crates/ui/assets/icons/close.svg` | Tab, pane, and view close |

## Theme tokens

Dark mode uses `theme.bg` for pane cards and `theme.surface` for surrounding
chrome. Light mode uses the same semantic roles. Never hard-code a dark-only
color in the pane renderer.

## Design guardrails

- Stretch the view strip across its complete view.
- Inset the pane tree as one field, then let each leaf paint its own card.
- Keep the divider hit target at 8px even though its line is only 1px.
- Reserve `text_faint` for disabled content. Dormant interactive chrome uses
  `text_muted`.
- Do not add shadows between panes. The gap, opaque fill, radius, and border
  provide the separation.
- Use the accent only for focus, active tabs, and drag previews.

## Agent prompt guide

Build the view as a full-width strip followed by a padded pane field. Each
pane leaf must own its background, rounded clipping, and border. Preserve the
existing split tree, drag targets, and close semantics while changing only
presentation.
