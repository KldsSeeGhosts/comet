# Super (`super.engineering`) Design Specification Analysis
## Split Views, Layout Motion & Dual CLI/Chat Architecture for Noches

This specification provides a reverse-engineered architectural breakdown of Superconductor (`super.engineering`) as demonstrated in two canonical releases:
1. **Claude Fable 5.1** (`https://x.com/superdoteng/status/2094881170483814438`)
2. **Grok 4.6** (`https://x.com/superdoteng/status/2087653003201483065`)

The goal of this document is to serve as the reference design for implementing these features into **Noches** (the Comet fork) across `crates/ui/src/workspace.rs`, `crates/ui/src/session_pane.rs`, and `crates/ui/src/terminal/`.

---

## 1. Visual Reference Gallery

All reference frames extracted from the official videos are preserved in [`./screenshots/`](./screenshots/):

| Frame | Feature / State | Key Interaction & Design Elements |
|---|---|---|
| `01_single_tab_initial_state.jpg` | **Initial Single Pane** | Clean window surface, tabs (`Claude`, `Claude`, `+`), topbar cost pill (`.15`), empty-state action pills, composer with model selector (`Fable 5.1`) and reasoning effort (`High`). |
| `02_tab_drag_ghost_top_drop_zone.jpg` | **Top Edge Drop Zone** | Dragging tab detaches a floating ghost pill. Top 20% margin triggers a translucent rounded blue drop zone (`SplitNode::Split` vertical). |
| `03_tab_drag_right_drop_zone.jpg` | **Right Edge Drop Zone** | Dragging to right 20% margin triggers right vertical half overlay with blue accent border. |
| `04_tab_drag_bottom_drop_zone.jpg` | **Bottom Edge Drop Zone** | Dragging to bottom 20% margin triggers bottom half horizontal split overlay. |
| `05_tab_drag_left_drop_zone.jpg` | **Left Edge Drop Zone** | Dragging to left 20% margin triggers left vertical half split overlay. |
| `06_split_view_active_inactive_panes.jpg` | **Split View Active/Inactive** | Left pane boots Claude Code CLI (v2.1.257) with retro mascot. Right pane remains Chat UI. Inactive right pane shows muted contrast and placeholder `"Click to focus chat"`. |
| `07_cost_popover_breakdown.jpg` | **Telemetry & Cost Popover** | Clicking `.15` animates a floating GPUI popover with active conversation, worktree cost, per-tab cost, and rolling 7d/30d/90d/lifetime history. |
| `08_full_desktop_layout_sidebars.jpg` | **Full Desktop Environment** | Left projects sidebar (tree & branches), tab rail, right git/diff sidebar (`Files`, `Changes 0`, `Review 0`, `Checks`), and bottom drawer (`Terminal`, `Run`). |
| `09_tui_cli_mode_grok_build.jpg` | **TUI Mode (Grok Build CLI)** | Monospace terminal canvas, `12K / 500K` token meter, inline hook badges (`◆ Thought for 1.2s`, `◆ stop [hooks: 4]`), prompt `> writ|`, and keyboard legend `Shift+Tab:mode`. |
| `10_chat_ui_mode_and_thread_switcher.jpg` | **Chat UI Mode & Switcher** | Session switched to Chat UI with speech bubbles, collapsible thinking traces (`⊕ Thinking...`), and a dropdown thread switcher with a live hover preview card. |
| `11_tab_drag_to_split.jpg` | **Cross-Pane Tab Migration** | Dragging tab `Casual Greeting Interaction` towards right edge to create a mixed-mode split. |
| `12_side_by_side_chat_and_tui_splits.jpg` | **Side-by-Side Mixed Renderers** | Left pane: Chat UI for `Poem About Grok`. Right pane: live TUI CLI for `Casual Greeting Interaction` with active prompt and token meter. |

---

## 2. Split Views & Motion Engine Architecture

### 2.1 The Two-Tier Recursive Split Hierarchy
Super structures its window surface into two distinct tiers:

```
Window (NochesApp)
 └── Workspace (NochesWorkspace)
      └── SplitViewLayout (Root: SplitNode<ViewId>)
           ├── View 1 (Active View)
           │    ├── Tab Rail [ Tab 1 | Tab 2 ]
           │    └── Active Tab
           │         └── SplitTabLayout (Root: SplitNode<PaneId>)
           │              ├── Pane 1 (Left)  ── TabItem::Terminal (Claude Code / Grok TUI)
           │              └── Pane 2 (Right) ── TabItem::Chat (Rich ChatView)
           └── View 2 (Secondary View / Parked Tab)
```

1. **Workspace Tier (`SplitViewLayout`)**: Divides the main window into independent views (`ViewId`), each having its own independent tab rail, placement (top/left), and active tab.
2. **Tab Tier (`SplitTabLayout`)**: Within any single tab of a view, the content region is modeled as a recursive binary tree of panes (`PaneId`).

#### Data Structure
```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SplitNode<T> {
    #[serde(rename = "leaf")]
    Leaf { content: T },
    #[serde(rename = "split")]
    Split {
        horizontal: bool, // true = side-by-side (left/right); false = stacked (up/down)
        ratio: f32,       // 0.0 .. 1.0 (default 0.5)
        first: Box<SplitNode<T>>,
        second: Box<SplitNode<T>>,
    },
}
```

### 2.2 Drag-and-Drop Drop Zones & Ghost Rendering
When a tab or pane header is dragged:
1. **Drag Payload**:
   ```rust
   #[derive(Clone)]
   pub struct LayoutDrag {
       pub target: DragTarget, // DragTarget::Pane(PaneId) | DragTarget::Tab(TabId)
       pub title: String,
   }
   ```
2. **Drag Ghost**:
   A floating semi-transparent card rendered in GPUI following the cursor:
   ```rust
   div()
       .w(px(220.0))
       .px(px(12.0))
       .py(px(9.0))
       .rounded(px(7.0))
       .border_1()
       .border_color(theme.accent)
       .bg(theme.surface_raised)
       .shadow_md()
       .child(div().truncate().child(self.title.clone()))
       .child(div().text_color(theme.text_muted).text_size(px(10.0)).child("Center for tab · Edge to split"))
   ```
3. **Spatial Threshold Detection (`edge_zone`)**:
   Divided into 5 regions relative to the target pane bounds (W x H):
   - **Left**: x <= 0.20 * W -> Direction::Left
   - **Right**: x >= 0.80 * W -> Direction::Right
   - **Top**: y <= 0.20 * H -> Direction::Up
   - **Bottom**: y >= 0.80 * H -> Direction::Down
   - **Center**: 0.20 < x < 0.80 and 0.20 < y < 0.80 -> None (Tab merge)
   - **Outer Ring (14px window boundary)**: Drops into SplitViewLayout (Workspace tier).

4. **Drop Zone Visual Overlay**:
   ```rust
   div()
       .absolute()
       .rounded(px(8.0))
       .bg(theme.accent.opacity(0.18))
       .border_2()
       .border_color(theme.accent)
       .when(direction == Some(Direction::Left), |s| s.left_0().top_0().bottom_0().w(relative(0.5)))
       .when(direction == Some(Direction::Right), |s| s.right_0().top_0().bottom_0().w(relative(0.5)))
       .when(direction == Some(Direction::Up), |s| s.left_0().right_0().top_0().h(relative(0.5)))
       .when(direction == Some(Direction::Down), |s| s.left_0().right_0().bottom_0().h(relative(0.5)))
       .when(direction.is_none(), |s| s.inset(px(8.0)))
   ```

### 2.3 Motion & Layout Animation Mechanics
To prevent abrupt jumps when splits are inserted or destroyed:
1. **Ratio Easing**:
   A newly inserted split initializes with `ratio = 1.0` (or `0.0`), and animates to `0.5` over 200ms using `cubic-bezier(0.16, 1.0, 0.3, 1.0)`:
   ```rust
   R(t) = 0.5 * cubic_bezier(0.16, 1.0, 0.3, 1.0, t / T)
   ```
2. **Divider Interaction**:
   - Dividers have a 6px hit target with a 1px central border.
   - On hover, the divider lights up with `theme.accent.opacity(0.5)` and sets `col-resize` or `row-resize`.
   - **Double-Click Equalization**: Double-clicking any divider resets its branch ratio to `0.50` with an animated transition.

### 2.4 Active vs. Inactive Pane Hierarchy
1. **Active Pane**:
   - Bold tab label with an active accent underline (`h(px(2.0)).bg(theme.accent)`).
   - High text contrast (`theme.text`).
   - Focused cursor and active keystroke forwarding.
2. **Inactive Pane**:
   - Subtle background/typography dimming (`opacity(0.88)` or `text_muted`).
   - Border lines between inactive panes use `theme.border` instead of `theme.border_strong`.
   - **Dynamic Composer Redirection**: The composer input placeholder displays:
     ```
     Click to focus chat   (or ⌘L to focus chat)
     ```
   - Clicking anywhere in an inactive pane dispatches `FocusPane(id)` to activate it.

---

## 3. Dual-Representation Architecture: Chat View vs. CLI (TUI) View

Super's defining capability is the **dual-representation agent session**: any session can seamlessly alternate between a headless PTY terminal (TUI) and an interactive GPUI canvas (Chat UI).

### 3.1 The Canonical Session Identity
Both views share a single invariant tuple:
```
SessionIdentity = (session_id, provider, model, reasoning_effort, cwd)
```

The pane's runtime enum dynamically swaps between renderers:
```rust
pub enum TabItem {
    Chat(Entity<ChatView>),
    Terminal(Entity<TerminalPanel>),
}

pub struct PaneRuntime {
    pub session_id: Option<String>,
    pub item: TabItem,
    pub mode: PaneMode, // PaneMode::Chat | PaneMode::Terminal
}
```

### 3.2 Terminal Engine Internals
1. **Supervisor**: Uses `portable_pty` to spawn an agent CLI process (e.g., `claude`, `grok-build`, `pi`) in a background worker thread.
2. **Grid Emulation**: Feeds terminal output into `alacritty_terminal::term::Term`, maintaining cursor coordinates, ANSI colors, alt-screens, and character grids.
3. **GPUI TerminalElement**:
   - `request_layout`: Computes character cell dimensions based on font metrics.
   - `prepaint`: Batches contiguous cells sharing style attributes into `BatchedTextRun`s.
   - `paint`: Direct GPUI glyph draw calls, maintaining 120 FPS performance.
4. **Input Queuing (`pending_prompt.rs`)**: If input is submitted while the PTY process is initializing, keystrokes are buffered and drained once the shell indicates readiness.

### 3.3 Chat View Engine Internals
1. **Transcript Model**: Maintains a structured event graph of user turns, assistant markdown segments, and collapsible tool executions.
2. **Thinking Traces**: Displays expandable thinking widgets (`⊕ Thinking - The user is just saying hi...`).
3. **Token Telemetry**: Live token gauges (e.g. `12K / 500K`) positioned in the upper right.

### 3.4 The Mid-Session Handoff Protocol (`Shift+Tab:mode`)

```
      [ PTY / Terminal Mode ]                           [ Rich Chat Mode ]
   (Grok Build CLI / Claude Code)                      (Interactive GPUI Canvas)
                 │                                                 ▲
                 │  1. User presses Shift+Tab                      │
                 ▼                                                 │
      [ Check: Session Idle? ]                                     │
                 │ Yes                                             │
                 ▼                                                 │
      [ Stop PTY Process ]                                         │
                 │                                                 │
                 ▼                                                 │
      [ Read Provider History on Disk ]                            │
      (~/.claude/projects/.. or ~/.pi/sessions/..)                 │
                 │                                                 │
                 ▼                                                 │
      [ Hydrate Transcript Model ] ────────────────────────────────┘
                 │
                 ▼
      [ Swap TabItem::Terminal -> TabItem::Chat ]
                 │
                 ▼
      [ Notify GPUI & Focus Composer ]
```

#### Transition Guards
1. **Idle Constraint**: A session **cannot switch while busy** (streaming tokens or executing a tool). The switcher action is disabled and displays `"Session is busy"`.
2. **Resume Target**: Only sessions with a persistent identifier and verifiable resume command on disk are allowed to transition.

#### Handoff Steps: CLI -> Chat
1. Triggered via **`Shift+Tab`** or the pane header button.
2. Send orderly exit / `SIGINT` to the PTY process.
3. Locate provider transcript file on disk using the session ID.
4. Call `hydrate_toggled_chat_from_provider_history()`.
5. Construct new `Entity<ChatView>` and replace `TabItem::Terminal` -> `TabItem::Chat`.
6. Call `cx.notify()`.

#### Handoff Steps: Chat -> CLI
1. Triggered via **`Shift+Tab`** or the pane header button.
2. Flush any uncommitted composer text into the session store.
3. Resolve CLI resume arguments:
   - Claude: `claude --resume <session_id>`
   - Pi: `pi -r <session_file>`
   - Grok: `grok-build --session <session_id>`
4. Spawn `portable-pty` with shell hook wrappers (`SUPERCONDUCTOR_HOOK_SOCKET`).
5. Replace `TabItem::Chat` -> `TabItem::Terminal`.
6. Focus the PTY terminal element.

---

## 4. Concrete Implementation Blueprint for Noches

### Step 1: Add Animated Split Interpolation to `crates/ui/src/workspace.rs`
```rust
// Smoothly animate new split insertion using comet motion easing:
pub fn animate_split_insertion(
    &mut self,
    target_pane: PaneId,
    direction: Direction,
    cx: &mut Context<Self>,
) {
    let start = std::time::Instant::now();
    let duration = std::time::Duration::from_millis(200);

    cx.spawn(|this, mut cx| async move {
        let mut elapsed = std::time::Duration::ZERO;
        while elapsed < duration {
            cx.background_executor().timer(std::time::Duration::from_millis(16)).await;
            elapsed = start.elapsed();
            let progress = (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0);
            let ratio = crate::motion::cubic_bezier(0.16, 1.0, 0.3, 1.0, progress) * 0.5;

            this.update(&mut cx, |this, cx| {
                this.layout.set_temporary_split_ratio(target_pane, ratio);
                cx.notify();
            })?;
        }
        Ok::<(), anyhow::Error>(())
    }).detach();
}
```

### Step 2: Implement Double-Click Equalization on Dividers
```rust
// In render_resize_divider:
div()
    .id(SharedString::from(format!("divider-{:?}", path)))
    .when(horizontal, |d| d.w(px(6.0)).cursor_col_resize())
    .when(!horizontal, |d| d.h(px(6.0)).cursor_row_resize())
    .flex().items_center().justify_center()
    .child(
        div()
            .when(horizontal, |d| d.w(px(1.0)).h_full())
            .when(!horizontal, |d| d.h(px(1.0)).w_full())
            .bg(theme.border)
    )
    .hover(|s| s.bg(theme.accent.opacity(0.35)))
    .on_mouse_down(MouseButton::Left, cx.listener(move |this, event: &MouseDownEvent, _, cx| {
        if event.click_count == 2 {
            // Equalize branches to exactly 50/50:
            this.apply(|layout| layout.set_ratio(path.clone(), 0.5), cx);
        }
    }))
```

### Step 3: Implement Inactive Pane Dimming & Composer Redirection
```rust
// In crates/ui/src/session_pane.rs:
let is_active = self.state.read(cx).is_active_pane;

div()
    .id("session-chat")
    .size_full()
    .flex()
    .flex_col()
    .when(!is_active, |s| s.opacity(0.88))
    .child(div().absolute().inset_0().child(self.transcript.clone()))
    .child(div().flex_1().min_h_0())
    .child(
        div()
            .relative()
            .flex_none()
            .when(!is_active, |s| {
                s.child(
                    div()
                        .absolute()
                        .inset_0()
                        .z_index(10)
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                            this.focus(cx);
                        }))
                )
            })
            .child(self.composer.clone())
    )
```

### Step 4: Add `Shift+Tab:mode` Keybinding in `workspace.rs`
```rust
actions!(workspace, [ToggleSessionMode, FocusActiveChat]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("shift-tab", ToggleSessionMode, Some("NochesWorkspace")),
        KeyBinding::new("cmd-l", FocusActiveChat, Some("NochesWorkspace")),
    ]);
}
```
