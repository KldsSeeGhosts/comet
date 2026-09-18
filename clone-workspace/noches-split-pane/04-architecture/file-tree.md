# Split-pane presentation files

- `crates/ui/src/pane/chrome.rs`
  - Full-width tab strip
  - Readable header controls
  - Readable ghost composer
- `crates/ui/src/pane/render.rs`
  - Inset pane field
  - Opaque pane-card backgrounds
  - Focused and inactive border hierarchy
- `crates/ui/src/pane/hit_test.rs`
  - No presentation changes. Existing pane bounds continue to drive previews.
- `crates/ui/src/shell/panes.rs`
  - No presentation changes. Existing drag and close behavior stays intact.
