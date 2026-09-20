# zeron-workspace

Pure layout state for views, tabs and panes. No UI, session lifecycle or engine dependencies.

`WorkspaceLayout` stores a `SplitNode<ViewId>` and ordered maps of views, tabs and panes. `ViewId`, `TabId` and `PaneId` are transparent `u64` newtypes allocated from one persisted `next_id`. A tab owns its pane tree, active pane and primary pane. Pane state holds an optional session ID, mode, label and group.

## Editing

- `new` and `Default` create one view, tab and chat pane.
- `split_view`, `add_tab` and `split_pane` return the new ID and focus the new content.
- `close_pane`, `close_tab` and `close_view` remove empty containers and promote sibling trees. They refuse to empty the workspace. Focus falls back to the first tree leaf or lowest remaining tab ID. Closing a primary pane selects the first remaining leaf.
- `move_pane(source, target, direction)` splits the target leaf at its directional edge. It preserves the source ID and state, removes empty source containers and focuses the moved pane. Source and target may belong to different views or tabs. Moving a pane onto itself is an error.
- `focus_view`, `focus_tab` and `focus_pane` update the relevant ancestors.
- `set_view_ratio` and `set_pane_ratio` address splits with a slice of `Branch::First` and `Branch::Second`. An empty path addresses the root split. Paths ending at leaves fail.
- `active_pane_id`, `pane`, `pane_mut` and `pane_location` support UI lookup. Locations are `(ViewId, TabId)`.

Each structural method validates a draft before committing and advances `revision` once. `compose(expected_revision, closure)` combines edits into one commit. A stale revision, closure error, invalid draft, reused historical ID or counter decrease leaves the original unchanged. Nested structural calls are supported and the outer commit still advances revision once. External side effects inside a closure are not rolled back.

Fields are public for draft construction. Direct field changes and `pane_mut` bypass revision tracking; use them inside `compose` when tracking matters. Revisions guard in-memory edits, not concurrent file writers.

## Schema and limits

Trees use an internally tagged `type` field:

```json
{"type":"split","horizontal":true,"ratio":0.5,"first":{"type":"leaf","content":3},"second":{"type":"leaf","content":4}}
```

Horizontal means left/right; vertical means above/below. The ratio is the first child's share. Modes, tab placements, directions and branches serialize in snake case.

Validation requires globally unique nonzero IDs, exactly one reachable leaf per map entry, nonempty containers and valid active/primary IDs. Split ratios must be finite within `0.1..=0.9`. Rail widths must be finite within `0..=4096`. Limits are 16 split levels, 64 views, 512 total tabs and 4096 total panes. `next_id` must exceed every live ID; exhausted counters reject further allocations.

`edge_zone(x, y, width, height)` returns the nearest normalized edge in the outer 20%. Corner ties prefer left, right, up, then down. Invalid dimensions, non-finite inputs, outside points and the center return `None`.

## Persistence

`load` and direct `WorkspaceLayout` deserialization reject malformed layouts, unknown fields, duplicate map keys and stale counters. `load_with_policy(path, IdPolicy::Repair)` may raise a stale counter to the highest live ID plus one. It cannot recover deleted historical IDs from a damaged file; strict loading is the default.

`save` validates, writes a uniquely named sibling temporary file, syncs it and renames it over the destination. On Unix it also syncs the parent directory. The parent must already exist. Load/save files are limited to 8 MiB. A directory-sync error can be returned after the rename has committed.

## Verification

After adding `crates/workspace` to the root workspace members:

```sh
cargo test -p zeron-workspace --locked
cargo clippy -p zeron-workspace --tests --locked -- -D warnings
```
