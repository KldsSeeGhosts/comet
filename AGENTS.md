 # Noches Desktop: Open-Design Sidebar Transposition
 
 Architecture and implementation findings for transposing the open-design desktop prototype (`/Users/kidsseemac/superconductor/projects/open-design-project/zeron-desktop.html`) into the GPUI desktop shell (`crates/ui/src/shell.rs`) for the **Noches** fork.
 
 ## 1. Token & Theme Alignment
 
 - The tokens defined in `brand-spec.md` (`--bg`, `--surface`, `--accent`, `--wash`, etc.) map 1:1 to `crates/ui/src/theme.rs`.
 - Dark monochrome surfaces, 0-chroma neutrals, and low-alpha white hover washes (`crate::theme::wash()`) match existing GPUI primitives.
 - Font definitions map directly: display/body uses Geist (`theme.font_ui`), mono uses Geist Mono (`theme.font_mono`).
 
 ## 2. Structural Component Mapping
 
 ### Top Brand Identity (`.side-brand`)
 - **Design**: 30x30 rounded app mark with hover micro-animation, profile badge (`noches / local profile`), and "all runs" navigation.
 - **GPUI Target**: Insert above `filter_row` at the top of `render_chat_sidebar` in `crates/ui/src/shell.rs`. Currently, Zeron only applies top padding for the window titlebar.
 
 ### Space Switcher & Count Pill (`.sf-wrap`, `.space-filter`)
 - **Design**: Compact capsule trigger with folder/grid icon, active space label, tabular session count pill (`.sf-count`), and dropdown popover grouped by host device (`this device` vs `synced`).
 - **GPUI Target**: Refactor `render_spaces_filter` in `crates/ui/src/shell/spaces.rs` to include the session count pill and adopt the 10px rounded styling with bottom hairline border.
 
 ### Workspace Groups & Inline Session Creation (`.ws-group`, `.ws-head`)
 - **Design**: Section headers displaying workspace name, device fragment (`· studio` / `· serverseesghosts`), horizontal hairline rule, and an inline `+` button to start a session scoped to that space.
 - **GPUI Target**: Extend `render_active_rows` in `crates/ui/src/shell/spaces.rs`. GPUI already supports `SidebarOrganization::ByDevice`; update grouping logic to group by `(space_id, device_id)` and hook the inline `+` button to `new_chat_in_space`.
 
 ### Expressive Session Rows (`.sess`)
 - **Design**:
   - **Avatar slot (left)**: 36x36 animated buddy bot shapes (`#bot-orbit`, `#bot-visor`, etc.) with status dot overlays (`.sdot.working`, `.sdot.awaiting`, `.sdot.completed`, `.sdot.errored`).
   - **Line 1**: Title, unread emphasis (`font-weight: 600`), and live state indicators (animated 3-bar equalizer for working, pulsing dot for awaiting input, or relative timestamp).
   - **Line 2 / Footer**: Branch or worktree name on the left, harness brand mark on the right (Claude brand orange, Codex, Devin, OpenCode, Pi).
 - **GPUI Target**: Modify `render_chat_row` in `crates/ui/src/shell.rs`. Avatars can be drawn using GPUI SVG paths or sprite components. Status equalizers and dot pulses map directly to GPUI animations (`loaders::mini_glyph_spinner` / `with_animation`).
 
 ### Sidebar Footer (`.side-foot`)
 - **Design**: Current host device status (`● studio · online`), Remote Control shortcut button, and rotating Settings gear icon.
 - **GPUI Target**: Replace the user menu container in `render_chat_sidebar` with the status pill and action button strip, retaining popover bindings for Settings (`this.open_settings`).
 
 ## 3. Glass & Motion Capabilities
 
 - **Surface Glass**: The sidebar rests on Zeron's native macOS vibrancy (`NSVisualEffectView`) and Metal GPU backdrop blur (`crates/ui/src/frost.rs`).
 - **Transitions**: GPUI's FLIP resort animation (`resort_offsets` and `RESORT.animation()` in `crates/ui/src/shell.rs`) matches the staggered entrance and reordering in the open-design prototype.
 - **Edge Fade**: Retain `crate::edge_fade::edge_faded()` over `sidebar-lists` to maintain true per-glyph alpha fades over glass.
 
 ## 4. Primary Files to Modify
 
 - `crates/ui/src/shell.rs`: `render_chat_sidebar`, `render_chat_row`, brand header, and footer.
 - `crates/ui/src/shell/spaces.rs`: `render_spaces_filter`, `render_active_rows` (workspace + device grouping).
 - `crates/ui/src/icons.rs` & `assets/icons/`: Ensure harness SVGs and bot avatar definitions are available.
 - `crates/theme/src/lib.rs` / `crates/ui/src/theme.rs`: Verify token parity for live indicator colors (`--working`, `--ok`, `--warning`, `--danger`).
 
 ## 5. Execution Steps
 
 1. Add `.side-brand` header and `.side-foot` device/action strip to `crates/ui/src/shell.rs`.
 2. Restyle the space filter trigger and popover in `crates/ui/src/shell/spaces.rs`.
 3. Update session grouping to produce workspace section headers with inline session creation.
 4. Restructure `render_chat_row` layout to match the avatar + 2-line title/footer composition.
 5. Wire up the animated equalizer and status indicators.
 6. Validate rendering across macOS vibrancy and opaque fallbacks via `cargo test -p zeron-ui`.
