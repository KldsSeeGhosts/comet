# Split-pane component map

| Component | Source | Responsibility |
|---|---|---|
| Workspace outlet | `pane/render.rs` | Titlebar reserve and top-level view tree |
| View leaf | `pane/render.rs::view_node` | Full-width strip plus inset pane field |
| Pane leaf | `pane/render.rs::pane_container` | Rounded card, border, fill, focus |
| Tab strip | `pane/chrome.rs::tab_strip` | Tabs, add button, close-view button |
| Pane header | `pane/chrome.rs::pane_header` | Split-pane identity and close control |
| Ghost composer | `pane/chrome.rs::ghost_composer` | Dormant chat affordance |

The pane field wrapper changes visual geometry without changing persisted
split ratios. Paint-time pane bounds remain leaf bounds after the inset, so
drop previews automatically align with the visible cards.
