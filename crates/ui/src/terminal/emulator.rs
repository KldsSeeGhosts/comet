//! The terminal emulator core: `alacritty_terminal`'s `Term` + vte's ANSI
//! `Processor` wrapped as a pure state machine.
//!
//! Bytes in ([`Emulator::feed`] — the decoded `SubscribeTerminal` Data frames),
//! grid snapshots out ([`Emulator::line`], [`Emulator::cursor`]). No I/O, no
//! timers, no gpui: the panel owns RPC and scheduling, the view owns paint.
//! That split makes the whole escape-sequence surface unit-testable with
//! scripted byte strings.
//!
//! Selection lives here too ([`Emulator::start_selection`] and friends) rather
//! than in the panel, because `Term` is what knows how to keep anchors on their
//! text as output scrolls the grid underneath them.
//!
//! Painting is fed by [`Emulator::lines`], which keeps a per-row cache keyed by
//! absolute line index: rows whose content fingerprint is unchanged reuse the
//! previous snapshot instead of re-deriving it every frame.
//!
//! API notes for the pinned `alacritty_terminal 0.26` / `vte 0.15`:
//! - `Processor::advance` consumes a byte slice; `Term` implements the
//!   `vte::ansi::Handler` trait directly, so no event-loop machinery is needed.
//! - `Term::new` takes any `grid::Dimensions` impl — [`GridSize`] here (the
//!   crate's own `TermSize` lives in a `term::test` helper module).
//! - Query responses (DSR/DA/…) surface as `Event::PtyWrite` on the listener;
//!   [`Emulator::feed`] returns them so the panel can write them back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionRange};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb as AnsiRgb,
};

/// Grid coordinates and selection granularity, re-exported so the panel and
/// view speak the emulator's vocabulary without depending on
/// `alacritty_terminal` directly — the same seam [`CellColor`] draws for
/// colors.
pub use alacritty_terminal::index::{Point as GridPoint, Side};
pub use alacritty_terminal::selection::SelectionType;

/// Scrollback history kept client-side (lines). The engine's replay window is
/// bounded separately (1 MiB); this only caps what stays scrollable in the UI.
pub const SCROLLBACK_LINES: usize = 10_000;

/// FNV-1a basis/prime for the render-cache fingerprints (shared with the
/// panel's theme fingerprint so both caches mix identically).
pub(crate) const ROW_HASH_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const ROW_HASH_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Fold one 64-bit lane into a running FNV-1a hash.
pub(crate) fn fold_hash(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(ROW_HASH_PRIME);
    }
    hash
}

/// Fold raw bytes into a running FNV-1a hash.
pub(crate) fn fold_hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(ROW_HASH_PRIME);
    }
    hash
}

/// Upper bound on cached rows. Deep-history scrolling can touch every line in
/// scrollback; past this the cache resets rather than growing without bound.
const ROW_CACHE_MAX_ENTRIES: usize = 2048;

/// One cached grid row: the content fingerprint the entry was built from and
/// the derived snapshot cells (selection excluded — that is a per-frame
/// overlay applied on the clone in [`Emulator::lines`]).
struct RowCacheEntry {
    fingerprint: u64,
    cells: Vec<CellSnapshot>,
}

/// Viewport dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}
impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(2),
            rows: rows.max(1),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// A cell's paint color, decoupled from the palette: the view resolves these
/// against the theme (default fg/bg, 256-color index, or direct RGB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// Indexed color: 0-15 ANSI, 16-231 color cube, 232-255 grayscale ramp.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

/// A color's fingerprint lane: discriminant plus payload bits.
fn color_bits(color: AnsiColor) -> u64 {
    match color {
        AnsiColor::Named(named) => named as usize as u64,
        AnsiColor::Indexed(index) => 0x1_0000 | u64::from(index),
        AnsiColor::Spec(AnsiRgb { r, g, b }) => {
            0x2_0000 | u64::from(r) | (u64::from(g) << 8) | (u64::from(b) << 16)
        }
    }
}

fn map_color(color: AnsiColor) -> CellColor {
    match color {
        AnsiColor::Spec(AnsiRgb { r, g, b }) => CellColor::Rgb(r, g, b),
        AnsiColor::Indexed(ix) => CellColor::Indexed(ix),
        AnsiColor::Named(named) => {
            let ix = named as usize;
            if ix < 16 {
                return CellColor::Indexed(ix as u8);
            }
            match named {
                NamedColor::Background => CellColor::Background,
                // Dim named colors fold onto their base index; the DIM flag
                // still travels on the cell for paint-time dimming.
                NamedColor::DimBlack
                | NamedColor::DimRed
                | NamedColor::DimGreen
                | NamedColor::DimYellow
                | NamedColor::DimBlue
                | NamedColor::DimMagenta
                | NamedColor::DimCyan
                | NamedColor::DimWhite => {
                    CellColor::Indexed((ix - NamedColor::DimBlack as usize) as u8)
                }
                _ => CellColor::Foreground,
            }
        }
    }
}

/// One rendered cell: char + colors + the flags paint cares about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSnapshot {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// A double-width char (occupies this cell plus the next spacer cell).
    pub wide: bool,
    /// The spacer half of a wide char — never shaped, only background-painted.
    pub wide_spacer: bool,
    /// Inside the active selection: the view paints a wash over this cell.
    pub selected: bool,
}

impl CellSnapshot {
    /// Effective paint colors after INVERSE/HIDDEN resolution.
    pub fn display_colors(&self) -> (CellColor, CellColor) {
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        if self.hidden { (bg, bg) } else { (fg, bg) }
    }
}

/// Cursor position in viewport coordinates (row 0 = top of the visible grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
}

/// Captures `Term` callbacks. Interior-mutable because `EventListener::send_event`
/// takes `&self`; single-threaded (the emulator lives inside a gpui entity).
#[derive(Default, Clone)]
struct EventCapture {
    events: Rc<RefCell<Vec<Event>>>,
}

impl EventListener for EventCapture {
    fn send_event(&self, event: Event) {
        self.events.borrow_mut().push(event);
    }
}

/// The emulator: a pure fold of PTY bytes into a renderable grid.
pub struct Emulator {
    term: Term<EventCapture>,
    parser: Processor,
    capture: EventCapture,
    title: Option<String>,
    bell: bool,
    /// Per-row render cache keyed by ABSOLUTE line index (0 = oldest line in
    /// history). Every frame re-fingerprints the visible rows cheaply and only
    /// re-snapshots rows whose contents changed — see [`Emulator::lines`].
    row_cache: HashMap<usize, RowCacheEntry>,
    /// History size the cache keys were last aligned to; growth shifts keys,
    /// shrink (reflow, alt-screen swap, ClearHistory) resets the cache.
    row_cache_history_base: usize,
    /// Rows actually re-snapshotted — the cache-miss counter the tests read.
    row_cache_builds: usize,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        let capture = EventCapture::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let term = Term::new(config, &GridSize::new(cols, rows), capture.clone());
        Self {
            term,
            parser: Processor::new(),
            capture,
            title: None,
            bell: false,
            row_cache: HashMap::new(),
            row_cache_history_base: 0,
            row_cache_builds: 0,
        }
    }

    /// Advance the state machine over decoded PTY output. Returns bytes the
    /// terminal wants written back to the PTY (DSR/DA query responses etc.).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        let mut responses = Vec::new();
        for event in self.capture.events.borrow_mut().drain(..) {
            match event {
                Event::PtyWrite(text) => responses.extend_from_slice(text.as_bytes()),
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                Event::Bell => self.bell = true,
                _ => {}
            }
        }
        responses
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(GridSize::new(cols, rows));
        // Reflow moves every cell; cached rows are meaningless afterwards.
        self.row_cache.clear();
    }

    pub fn cols(&self) -> usize {
        self.term.columns()
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    /// OSC title, if the running program set one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// True once a BEL arrived; reading clears it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Arrow keys should send SS3 (`ESC O A`) instead of CSI.
    pub fn app_cursor_mode(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Pastes should be wrapped in `ESC [200~` / `ESC [201~`.
    pub fn bracketed_paste_mode(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Applications in mouse mode own wheel events, including Pi's alternate-screen UI.
    pub fn mouse_reporting(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    pub fn utf8_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::UTF8_MOUSE)
    }

    pub fn alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Fullscreen programs without mouse reporting receive wheel events as arrows.
    pub fn alternate_scroll(&self) -> bool {
        self.term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
    }

    /// Lines scrolled back into history (0 = pinned to the live bottom).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines available above the viewport.
    pub fn history_lines(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Scroll the view: positive = up into history, negative = toward live.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Set the scrollback offset directly (0 = live bottom).
    pub fn scroll_to_offset(&mut self, offset: usize) {
        let target = offset.min(self.history_lines());
        let current = self.display_offset();
        let delta = (target as i64 - current as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        self.scroll(delta);
    }

    // ---- selection ----
    //
    // `Term` owns the selection outright, which is what makes this cheap: it
    // rotates the anchors when output scrolls the grid and drops them on clear
    // and resize, so a selection tracks live output without any bookkeeping
    // here. The panel supplies pointer positions; everything below is a thin
    // translation into grid coordinates.

    /// The grid point under a viewport cell (row 0 = top of the visible area).
    ///
    /// Viewport rows are what the pointer hits; grid lines are what a selection
    /// anchors to, and the two differ by the scrollback offset. Anchoring in
    /// grid space is what lets a selection stay on its text while the view
    /// scrolls out from under it.
    pub fn grid_point(&self, viewport_row: usize, col: usize) -> Point {
        Point::new(
            Line(viewport_row as i32 - self.display_offset() as i32),
            Column(col.min(self.cols().saturating_sub(1))),
        )
    }

    /// Begin a selection. `ty` picks the granularity: [`SelectionType::Simple`]
    /// for a drag, `Semantic` for a double-click word, `Lines` for a triple-
    /// click row.
    pub fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    /// Extend the in-progress selection to `point`. No-op without one.
    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The selected text, or `None` when there is no selection or it covers
    /// nothing (a click without a drag leaves an empty one behind).
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Whether a non-empty selection is active — drives the copy action and
    /// the "clear it" branch on the next click.
    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    fn selection_range(&self) -> Option<SelectionRange> {
        self.term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
    }

    /// Snapshot one viewport row (0 = top) honoring the scrollback offset.
    pub fn line(&self, viewport_row: usize) -> Vec<CellSnapshot> {
        self.snapshot_row(viewport_row, self.selection_range())
    }

    /// The shared body of [`Self::line`], taking the selection range as an
    /// argument so the cached [`Self::lines`] path resolves it once per frame
    /// rather than once per row — `to_range` re-walks the grid for semantic and
    /// line selections.
    fn snapshot_row(
        &self,
        viewport_row: usize,
        selection: Option<SelectionRange>,
    ) -> Vec<CellSnapshot> {
        let offset = self.display_offset() as i32;
        let line = Line(viewport_row as i32 - offset);
        let grid = self.term.grid();
        let row = &grid[line];
        (0..self.cols())
            .map(|col| {
                let cell = &row[Column(col)];
                CellSnapshot {
                    ch: cell.c,
                    fg: map_color(cell.fg),
                    bg: map_color(cell.bg),
                    bold: cell.flags.intersects(Flags::BOLD),
                    dim: cell.flags.intersects(Flags::DIM),
                    italic: cell.flags.intersects(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: cell.flags.intersects(Flags::INVERSE),
                    hidden: cell.flags.intersects(Flags::HIDDEN),
                    wide: cell.flags.intersects(Flags::WIDE_CHAR),
                    wide_spacer: cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                    selected: selection
                        .is_some_and(|range| range.contains(Point::new(line, Column(col)))),
                }
            })
            .collect()
    }

    /// Grid line (viewport-relative) → absolute line index. Absolute indices
    /// are stable while the user scrolls (scrolling moves the viewport, not the
    /// grid), which is what makes them the render cache's key.
    fn absolute_line(&self, viewport_row: usize) -> usize {
        let offset = self.display_offset() as i32;
        let line = Line(viewport_row as i32 - offset);
        (line.0 - self.term.grid().topmost_line().0) as usize
    }

    /// Content fingerprint of one viewport row: every field [`CellSnapshot`]
    /// derives from (char, both colors, all flag bits) folded with FNV-1a.
    /// Equal fingerprints imply equal snapshot rows (selection excluded), the
    /// guarantee the per-row render cache leans on. Pure, and cheap — no
    /// allocation, no snapshot construction.
    pub fn row_fingerprint(&self, viewport_row: usize) -> u64 {
        let offset = self.display_offset() as i32;
        let line = Line(viewport_row as i32 - offset);
        let grid = self.term.grid();
        let row = &grid[line];
        // Width in the mix: a resized (truncated) row can never collide with
        // its pre-resize entry, even before `resize` clears the cache.
        let mut hash = fold_hash(ROW_HASH_BASIS, self.cols() as u64);
        for col in 0..self.cols() {
            let cell = &row[Column(col)];
            hash = fold_hash(hash, u64::from(cell.c as u32));
            hash = fold_hash(hash, color_bits(cell.fg));
            hash = fold_hash(hash, color_bits(cell.bg));
            hash = fold_hash(hash, u64::from(cell.flags.bits()));
        }
        hash
    }

    /// All viewport rows, top to bottom, through the per-row cache. Returns
    /// each row's fingerprint alongside its snapshot so callers can key their
    /// own derived state (shaped text) off it.
    ///
    /// Fingerprints are computed straight off the grid first; only rows whose
    /// fingerprint differs from the cached entry are re-snapshotted. Entries
    /// are keyed by absolute line index, so plain scrolling reuses everything;
    /// when output grows the history the keys are re-based by the same delta
    /// (a shrink moves indices unpredictably and resets the cache instead).
    /// The grid itself is only ever read here, so stale reuse is impossible:
    /// a fingerprint mismatch is always resolved against fresh cell data.
    pub fn lines(&mut self) -> Vec<(u64, Vec<CellSnapshot>)> {
        let selection = self.selection_range();
        let history = self.term.grid().history_size();
        if history >= self.row_cache_history_base {
            let shift = history - self.row_cache_history_base;
            if shift > 0 {
                self.row_cache = self
                    .row_cache
                    .drain()
                    .map(|(abs, entry)| (abs + shift, entry))
                    .collect();
            }
        } else {
            self.row_cache.clear();
        }
        self.row_cache_history_base = history;

        let mut rows = Vec::with_capacity(self.rows());
        for row_ix in 0..self.rows() {
            let fingerprint = self.row_fingerprint(row_ix);
            let abs = self.absolute_line(row_ix);
            let hit = self
                .row_cache
                .get(&abs)
                .is_some_and(|entry| entry.fingerprint == fingerprint && !entry.cells.is_empty());
            if !hit {
                let cells = self.snapshot_row(row_ix, None);
                self.row_cache_builds += 1;
                self.row_cache
                    .insert(abs, RowCacheEntry { fingerprint, cells });
            }
            let mut cells = match self.row_cache.get(&abs) {
                Some(entry) if !entry.cells.is_empty() => entry.cells.clone(),
                _ => self.snapshot_row(row_ix, selection),
            };
            // Selection is an overlay: applied fresh on every frame.
            if let Some(range) = selection {
                let offset = self.display_offset() as i32;
                let line = Line(row_ix as i32 - offset);
                for (col, cell) in cells.iter_mut().enumerate() {
                    cell.selected = range.contains(Point::new(line, Column(col)));
                }
            }
            rows.push((fingerprint, cells));
        }
        // Drop entries for lines that left the grid (pruned scrollback).
        let valid_lines = history + self.rows();
        self.row_cache.retain(|abs, _| *abs < valid_lines);
        if self.row_cache.len() > ROW_CACHE_MAX_ENTRIES {
            self.row_cache.clear();
        }
        rows
    }

    /// Cursor in viewport coordinates; `None` when hidden or scrolled out.
    pub fn cursor(&self) -> Option<CursorSnapshot> {
        let content = self.term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }
        let Point { line, column } = content.cursor.point;
        let row = line.0 + self.display_offset() as i32;
        if row < 0 || row >= self.rows() as i32 {
            return None;
        }
        Some(CursorSnapshot {
            row: row as usize,
            col: column.0,
        })
    }

    /// Test/diagnostic helper: a viewport row as trimmed text (wide-char
    /// spacers skipped).
    pub fn row_text(&self, viewport_row: usize) -> String {
        let mut text: String = self
            .line(viewport_row)
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.ch)
            .collect();
        while text.ends_with(' ') {
            text.pop();
        }
        text
    }
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("cols", &self.cols())
            .field("rows", &self.rows())
            .field("display_offset", &self.display_offset())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu(cols: u16, rows: u16) -> Emulator {
        Emulator::new(cols, rows)
    }

    #[test]
    fn plain_text_lands_on_row_zero() {
        let mut e = emu(20, 5);
        e.feed(b"hello");
        assert_eq!(e.row_text(0), "hello");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 5 }));
    }

    #[test]
    fn crlf_moves_lines_and_cr_returns_to_column_zero() {
        let mut e = emu(20, 5);
        e.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(e.row_text(0), "one");
        assert_eq!(e.row_text(1), "two");
        assert_eq!(e.row_text(2), "three");
        e.feed(b"\rXX");
        assert_eq!(e.row_text(2), "XXree");
    }

    #[test]
    fn long_line_wraps_at_the_grid_width() {
        let mut e = emu(10, 4);
        e.feed(b"abcdefghijKLM");
        assert_eq!(e.row_text(0), "abcdefghij");
        assert_eq!(e.row_text(1), "KLM");
    }

    #[test]
    fn sgr_colors_and_attributes() {
        let mut e = emu(40, 4);
        e.feed(b"\x1b[31mred\x1b[0m plain \x1b[1;44mboldbg\x1b[0m");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(1));
        assert_eq!(line[0].bg, CellColor::Background);
        // After reset: defaults.
        assert_eq!(line[4].fg, CellColor::Foreground);
        // Bold + blue background segment starts at col 10 ("red plain " = 10).
        let bold_cell = line[10];
        assert!(bold_cell.bold);
        assert_eq!(bold_cell.bg, CellColor::Indexed(4));
    }

    #[test]
    fn bright_256_and_truecolor_sgr() {
        let mut e = emu(40, 2);
        e.feed(b"\x1b[95mA\x1b[38;5;196mB\x1b[38;2;10;20;30mC");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(13)); // bright magenta
        assert_eq!(line[1].fg, CellColor::Indexed(196));
        assert_eq!(line[2].fg, CellColor::Rgb(10, 20, 30));
    }

    #[test]
    fn inverse_and_hidden_resolve_in_display_colors() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[7mI\x1b[0m\x1b[8mH");
        let inv = e.line(0)[0];
        assert!(inv.inverse);
        assert_eq!(
            inv.display_colors(),
            (CellColor::Background, CellColor::Foreground)
        );
        let hid = e.line(0)[1];
        assert!(hid.hidden);
        let (fg, bg) = hid.display_colors();
        assert_eq!(fg, bg, "hidden text paints foreground as background");
    }

    #[test]
    fn cursor_addressing_and_relative_moves() {
        let mut e = emu(20, 6);
        e.feed(b"\x1b[3;5Hx");
        // CSI H is 1-based; cell written at row 2, col 4; cursor advanced by 1.
        assert_eq!(e.line(2)[4].ch, 'x');
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 5 }));
        e.feed(b"\x1b[2D"); // left twice
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 3 }));
        e.feed(b"\x1b[A"); // up
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 1, col: 3 }));
    }

    #[test]
    fn clear_screen_and_home() {
        let mut e = emu(20, 4);
        e.feed(b"aaa\r\nbbb\r\nccc");
        e.feed(b"\x1b[2J\x1b[H");
        for row in 0..4 {
            assert_eq!(e.row_text(row), "");
        }
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 0 }));
        e.feed(b"fresh");
        assert_eq!(e.row_text(0), "fresh");
    }

    #[test]
    fn erase_line_variants() {
        let mut e = emu(20, 2);
        e.feed(b"abcdef\x1b[3D\x1b[K"); // erase from cursor (col 3) to end
        assert_eq!(e.row_text(0), "abc");
    }

    #[test]
    fn scrollback_history_and_scrolling() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        // Viewport shows the tail (line7, line8, then the blank prompt row).
        assert_eq!(e.row_text(0), "line7");
        assert_eq!(e.history_lines(), 6);
        assert_eq!(e.display_offset(), 0);
        // Scroll up into history.
        e.scroll(2);
        assert_eq!(e.display_offset(), 2);
        assert_eq!(e.row_text(0), "line5");
        // Cursor is below the viewport while scrolled back.
        assert_eq!(e.cursor(), None);
        // Over-scroll clamps to the top of history.
        e.scroll(100);
        assert_eq!(e.display_offset(), 6);
        assert_eq!(e.row_text(0), "line1");
        e.scroll_to_offset(3);
        assert_eq!(e.display_offset(), 3);
        assert_eq!(e.row_text(0), "line4");
        e.scroll_to_offset(usize::MAX);
        assert_eq!(e.display_offset(), 6);
        e.scroll_to_bottom();
        assert_eq!(e.display_offset(), 0);
        assert_eq!(e.row_text(0), "line7");
    }

    #[test]
    fn alt_screen_restores_primary_content() {
        let mut e = emu(20, 4);
        e.feed(b"primary");
        // Enter the alt screen; 1049 keeps the cursor position, so home first.
        e.feed(b"\x1b[?1049h\x1b[H");
        e.feed(b"alt-content");
        assert_eq!(e.row_text(0), "alt-content");
        e.feed(b"\x1b[?1049l"); // leave
        assert_eq!(e.row_text(0), "primary");
    }

    #[test]
    fn dsr_cursor_report_produces_pty_response() {
        let mut e = emu(20, 4);
        e.feed(b"\x1b[2;3H");
        let responses = e.feed(b"\x1b[6n");
        assert_eq!(String::from_utf8_lossy(&responses), "\x1b[2;3R");
    }

    #[test]
    fn osc_title_and_bell() {
        let mut e = emu(20, 2);
        assert_eq!(e.title(), None);
        e.feed(b"\x1b]0;my title\x07");
        assert_eq!(e.title(), Some("my title"));
        assert!(!e.take_bell());
        e.feed(b"\x07");
        assert!(e.take_bell());
        assert!(!e.take_bell(), "bell reads clear it");
    }

    #[test]
    fn app_cursor_and_bracketed_paste_modes_toggle() {
        let mut e = emu(10, 2);
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?1h");
        assert!(e.app_cursor_mode());
        e.feed(b"\x1b[?1l");
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?2004h");
        assert!(e.bracketed_paste_mode());
    }

    #[test]
    fn hidden_cursor_mode() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[?25l");
        assert_eq!(e.cursor(), None);
        e.feed(b"\x1b[?25h");
        assert!(e.cursor().is_some());
    }

    #[test]
    fn resize_preserves_content_and_reflows_cursor() {
        let mut e = emu(20, 5);
        e.feed(b"keepme\r\nsecond");
        e.resize(30, 3);
        assert_eq!(e.cols(), 30);
        assert_eq!(e.rows(), 3);
        assert_eq!(e.row_text(0), "keepme");
        assert_eq!(e.row_text(1), "second");
    }

    #[test]
    fn wide_chars_occupy_two_cells_with_spacer() {
        let mut e = emu(10, 2);
        e.feed("宽w".as_bytes());
        let line = e.line(0);
        assert!(line[0].wide);
        assert_eq!(line[0].ch, '宽');
        assert!(line[1].wide_spacer);
        assert_eq!(line[2].ch, 'w');
        assert_eq!(e.row_text(0), "宽w");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 3 }));
    }

    /// Viewport row → grid line, which is the translation every selection
    /// anchor goes through. Unscrolled they coincide; scrolled back, the same
    /// viewport row names a line further up history.
    #[test]
    fn grid_point_offsets_by_the_scrollback_position() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(e.grid_point(0, 2), Point::new(Line(0), Column(2)));
        e.scroll(4);
        assert_eq!(e.grid_point(0, 2), Point::new(Line(-4), Column(2)));
        // Columns clamp into the grid so an over-wide pointer cannot anchor
        // outside it.
        assert_eq!(e.grid_point(0, 99).column, Column(9));
    }

    #[test]
    fn simple_selection_yields_its_text_and_marks_its_cells() {
        let mut e = emu(20, 3);
        e.feed(b"hello world");
        assert!(!e.has_selection());
        assert_eq!(e.selection_text(), None);

        // Drag across "hello".
        e.start_selection(SelectionType::Simple, e.grid_point(0, 0), Side::Left);
        e.update_selection(e.grid_point(0, 4), Side::Right);
        assert!(e.has_selection());
        assert_eq!(e.selection_text().as_deref(), Some("hello"));

        let line = e.line(0);
        assert!(line[..5].iter().all(|c| c.selected));
        assert!(!line[5].selected, "the space past the drag is not selected");

        e.clear_selection();
        assert!(!e.has_selection());
        assert!(e.line(0).iter().all(|c| !c.selected));
    }

    /// Double-click granularity: the anchor expands to the whole word without
    /// the caller computing any boundaries.
    #[test]
    fn semantic_selection_expands_to_the_word() {
        let mut e = emu(30, 2);
        e.feed(b"alpha beta gamma");
        e.start_selection(SelectionType::Semantic, e.grid_point(0, 7), Side::Left);
        assert_eq!(e.selection_text().as_deref(), Some("beta"));
    }

    /// Triple-click granularity. The trailing newline is part of the copy —
    /// pasting a line-selection should reproduce the line break, the way it
    /// does in every other terminal.
    #[test]
    fn line_selection_takes_the_whole_row() {
        let mut e = emu(30, 3);
        e.feed(b"first row\r\nsecond row");
        e.start_selection(SelectionType::Lines, e.grid_point(1, 3), Side::Left);
        assert_eq!(e.selection_text().as_deref(), Some("second row\n"));
    }

    /// A selection made across a line break keeps the newline, so pasting the
    /// copy reproduces the rows.
    #[test]
    fn selection_spans_rows_with_a_newline() {
        let mut e = emu(10, 3);
        e.feed(b"ab\r\ncd");
        e.start_selection(SelectionType::Simple, e.grid_point(0, 0), Side::Left);
        e.update_selection(e.grid_point(1, 1), Side::Right);
        assert_eq!(e.selection_text().as_deref(), Some("ab\ncd"));
    }

    /// The reason anchors live in grid space: output that scrolls the grid must
    /// carry the selection with its text, not leave it pinned to a screen row.
    #[test]
    fn selection_follows_its_text_when_output_scrolls() {
        let mut e = emu(10, 3);
        e.feed(b"target\r\n");
        e.start_selection(SelectionType::Simple, e.grid_point(0, 0), Side::Left);
        e.update_selection(e.grid_point(0, 5), Side::Right);
        assert_eq!(e.selection_text().as_deref(), Some("target"));
        // Push it up the screen; the text is unchanged, so the copy is too.
        e.feed(b"a\r\nb\r\nc\r\n");
        assert_eq!(e.selection_text().as_deref(), Some("target"));
    }

    /// A click with no drag selects nothing, and must not report a selection —
    /// otherwise the copy action fires on every bare click.
    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let mut e = emu(20, 2);
        e.feed(b"hello");
        e.start_selection(SelectionType::Simple, e.grid_point(0, 2), Side::Left);
        assert_eq!(e.selection_text(), None);
        assert!(!e.has_selection());
    }

    #[test]
    fn utf8_split_across_feeds_reassembles() {
        let mut e = emu(10, 2);
        let bytes = "é".as_bytes();
        e.feed(&bytes[..1]);
        e.feed(&bytes[1..]);
        assert_eq!(e.row_text(0), "é");
    }

    // ---- per-row render cache ----

    fn cached_row_text(rows: &[(u64, Vec<CellSnapshot>)], row: usize) -> String {
        rows[row]
            .1
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end_matches(' ')
            .to_string()
    }

    fn fingerprints(e: &mut Emulator) -> Vec<u64> {
        e.lines().into_iter().map(|(fp, _)| fp).collect()
    }

    #[test]
    fn unchanged_grid_snapshots_every_row_exactly_once() {
        let mut e = emu(20, 4);
        e.feed(b"alpha\r\nbeta");
        let first = e.lines();
        let builds = e.row_cache_builds;
        assert!(builds > 0);
        // Unchanged grid: no row re-snapshots, output identical.
        let second = e.lines();
        assert_eq!(e.row_cache_builds, builds);
        assert_eq!(
            first.into_iter().map(|(_, c)| c).collect::<Vec<_>>(),
            second.into_iter().map(|(_, c)| c).collect::<Vec<_>>()
        );
    }

    #[test]
    fn row_edit_changes_only_that_rows_fingerprint() {
        let mut e = emu(20, 4);
        e.feed(b"alpha\r\nbeta\r\ngamma");
        let before = fingerprints(&mut e);
        e.feed(b"\x1b[2;1HX"); // overwrite one cell on row 1
        let after = fingerprints(&mut e);
        assert_eq!(before.len(), after.len());
        for (row, (was, now)) in before.iter().zip(&after).enumerate() {
            assert_eq!(
                was != now,
                row == 1,
                "row {row}: fingerprint must only move on the edited row"
            );
        }
    }

    #[test]
    fn resize_invalidates_the_whole_cache() {
        let mut e = emu(20, 4);
        e.feed(b"keepme");
        e.lines();
        assert!(!e.row_cache.is_empty());
        e.resize(30, 3);
        assert!(e.row_cache.is_empty());
    }

    /// Scrolling and streaming output shift which absolute line each cache key
    /// names; the cached path must never serve one row's data under another.
    #[test]
    fn scrollback_shifts_keep_cached_rows_correct() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        e.lines();
        // Scroll back into history: same absolute lines, moved viewport.
        e.scroll(2);
        let cached = e.lines();
        for row in 0..e.rows() {
            assert_eq!(cached_row_text(&cached, row), e.row_text(row));
        }
        // New output grows the history and shifts every absolute index.
        e.scroll_to_bottom();
        e.feed(b"more\r\n");
        let cached = e.lines();
        for row in 0..e.rows() {
            assert_eq!(cached_row_text(&cached, row), e.row_text(row));
        }
    }

    /// The reuse payoff: once a range of history has been painted, scrolling
    /// across it rebuilds nothing — absolute indices are scroll-invariant.
    #[test]
    fn scrolling_through_painted_history_rebuilds_nothing() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        e.lines();
        let builds = e.row_cache_builds;
        // Scrolling up first reveals unpainted lines: exactly those build.
        e.scroll(2);
        e.lines();
        assert_eq!(e.row_cache_builds, builds + 2);
        // Everything the viewport can now reach is painted: no more builds.
        let builds = e.row_cache_builds;
        e.scroll(-1);
        e.lines();
        e.scroll(1);
        e.lines();
        e.scroll_to_bottom();
        e.lines();
        assert_eq!(e.row_cache_builds, builds);
    }
}
