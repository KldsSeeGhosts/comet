//! The transcript's vertical overlay scrollbar — the floating rail treatment
//! shared by the menus ([`crate::popover`]'s `MenuScrollbarState`) and the
//! terminal scrollbar, adapted to the transcript's virtualized
//! [`gpui::ListState`].
//!
//! Behavior: hidden until the pointer is over the transcript (or a drag holds
//! it), a faint hairline track with a thumb that widens while hovered or
//! dragged. Pressing the thumb grabs it at the press point; pressing the
//! track first centers the thumb under the pointer (a click-track jump), and
//! the press continues as a drag. While the drag lasts, the list freezes its
//! content height (`ListState::scrollbar_drag_started`) so the thumb cannot
//! resize under streaming growth; the rail itself stays visible for the whole
//! drag even when the pointer travels off it (hover suppression under an
//! active drag is exactly why the grab counts as visibility on its own).
//!
//! All geometry is pure and unit-tested here; the transcript layers the gpui
//! listeners and its stick-to-bottom bookkeeping around these calls.

use gpui::{Context, Div, Window, div, prelude::*, px};

use crate::popover::{
    MENU_SCROLLBAR_HIT_WIDTH, MENU_SCROLLBAR_HOVER_THUMB_WIDTH, MENU_SCROLLBAR_THUMB_WIDTH,
    MENU_SCROLLBAR_TRACK_INSET, MenuScrollbarMetrics,
};
use crate::theme::Theme;

/// Marker for GPUI's captured drag stream. A private type (not the popover
/// one) so a transcript drag can never be routed into a menu's handler or
/// vice versa; the grab geometry lives in [`TranscriptScrollbarState`].
pub struct ScrollbarDrag;

/// Invisible drag preview: scrollbar drags manipulate the existing thumb.
pub struct ScrollbarDragGhost;

impl gpui::Render for ScrollbarDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::Empty
    }
}

/// Scroll-top along the list's pixel axis from the scrollbar accessor pair:
/// `scroll_px_offset_for_scrollbar` reports NEGATIVE distance from the top
/// while `max_offset_for_scrollbar` reports the positive travel. Pure.
pub fn scroll_top_from_list_offsets(max_scroll: f32, negative_offset: f32) -> f32 {
    (-negative_offset).clamp(0.0, max_scroll.max(0.0))
}

/// Thumb geometry for one frame. `None` while the content fits the viewport
/// (nothing to scroll) or the viewport has not had its first layout yet.
pub fn metrics(
    viewport_height: f32,
    max_scroll: f32,
    negative_offset: f32,
) -> Option<MenuScrollbarMetrics> {
    MenuScrollbarMetrics::from_viewport(
        viewport_height,
        max_scroll,
        scroll_top_from_list_offsets(max_scroll, negative_offset),
    )
}

/// Where the press grabbed the thumb: on the thumb it keeps the press point's
/// offset inside the thumb; on the track it centers the thumb under the
/// pointer first (the click-track jump). Pure.
pub fn grab_offset_for_press(metrics: &MenuScrollbarMetrics, pointer_in_track: f32) -> f32 {
    if (metrics.thumb_top..=metrics.thumb_top + metrics.thumb_height).contains(&pointer_in_track) {
        pointer_in_track - metrics.thumb_top
    } else {
        metrics.thumb_height / 2.0
    }
}

/// Scroll-top for a drag position: the grabbed thumb follows the pointer
/// (clamped to the track), and the thumb's position maps linearly back onto
/// the scroll range. Pure.
pub fn scroll_top_for_pointer(
    metrics: &MenuScrollbarMetrics,
    grab_offset: f32,
    pointer_in_track: f32,
) -> f32 {
    let thumb_top = (pointer_in_track - grab_offset).clamp(0.0, metrics.travel());
    if metrics.travel() <= 0.0 {
        0.0
    } else {
        thumb_top / metrics.travel() * metrics.max_scroll
    }
}

/// Hover/drag interaction state for the transcript's rail, owned by the
/// [`Transcript`](super::Transcript). Event handlers stay on the view (they
/// need its listeners and the list); they delegate here.
#[derive(Default)]
pub struct TranscriptScrollbarState {
    viewport_hovered: bool,
    bar_hovered: bool,
    grab: Option<f32>,
}

impl TranscriptScrollbarState {
    /// Whether the rail paints at all — an on-demand affordance like the
    /// menus' and terminal's: hidden until the transcript is hovered or a
    /// drag holds it.
    pub fn visible(&self) -> bool {
        self.viewport_hovered || self.grab.is_some()
    }

    /// Whether the thumb carries the expanded/stronger treatment.
    pub fn active(&self) -> bool {
        self.bar_hovered || self.grab.is_some()
    }

    /// The pointer entered/left the TRANSCRIPT. Returns whether anything
    /// changed.
    pub fn set_viewport_hovered(&mut self, hovered: bool) -> bool {
        if self.viewport_hovered == hovered {
            return false;
        }
        self.viewport_hovered = hovered;
        if !hovered && self.grab.is_none() {
            self.bar_hovered = false;
        }
        true
    }

    /// The pointer entered/left the RAIL. Keeps the active treatment while a
    /// captured drag travels outside (the hover callback correctly turns
    /// false there — gpui suppresses hover under an active drag). Returns
    /// whether anything changed.
    pub fn set_bar_hovered(&mut self, hovered: bool) -> bool {
        let active = hovered || self.grab.is_some();
        if self.bar_hovered == active {
            return false;
        }
        self.bar_hovered = active;
        true
    }

    /// A press landed on the rail: arm the grab (thumb keeps its relative
    /// position, track click centers the thumb first) and return the
    /// scroll-top to jump to. Always engages — the caller checked `metrics`
    /// beforehand.
    pub fn press(&mut self, m: MenuScrollbarMetrics, track_top: f32, pointer_y: f32) -> f32 {
        let pointer_in_track = pointer_y - track_top - MENU_SCROLLBAR_TRACK_INSET;
        let grab_offset = grab_offset_for_press(&m, pointer_in_track);
        self.grab = Some(grab_offset);
        scroll_top_for_pointer(&m, grab_offset, pointer_in_track)
    }

    /// Move an engaged drag to `pointer_y`, returning the new scroll-top.
    /// `None` when no drag is engaged.
    pub fn drag(&self, m: MenuScrollbarMetrics, track_top: f32, pointer_y: f32) -> Option<f32> {
        let grab_offset = self.grab?;
        let pointer_in_track = pointer_y - track_top - MENU_SCROLLBAR_TRACK_INSET;
        Some(scroll_top_for_pointer(&m, grab_offset, pointer_in_track))
    }

    /// The press ended anywhere: drop the drag; the rail stays armed only
    /// while the transcript is still hovered. Returns whether a drag was
    /// actually in progress.
    pub fn release(&mut self) -> bool {
        let was_dragging = self.grab.take().is_some();
        if was_dragging && !self.viewport_hovered && self.bar_hovered {
            self.bar_hovered = false;
        }
        was_dragging
    }

    /// The rail visuals: a fixed-width invisible hit strip on the right edge
    /// carrying the faint hairline track and the thumb. Callers layer
    /// listeners onto the returned strip (`.on_hover`, press, drag,
    /// release). The thumb is an absolute child inside the fixed-width hit
    /// rail, so hover expansion changes only paint geometry and never
    /// reflows the transcript.
    pub fn render_rail(
        &self,
        id: &'static str,
        theme: &Theme,
        m: &MenuScrollbarMetrics,
    ) -> gpui::Stateful<Div> {
        let active = self.active();
        let thumb_width = if active {
            MENU_SCROLLBAR_HOVER_THUMB_WIDTH
        } else {
            MENU_SCROLLBAR_THUMB_WIDTH
        };
        div()
            .id(id)
            // Test hook: bounds lookups by name (a noop outside tests).
            .debug_selector(|| id.into())
            .absolute()
            .top(px(0.0))
            .bottom(px(0.0))
            .right(px(0.0))
            .w(px(MENU_SCROLLBAR_HIT_WIDTH))
            .cursor_pointer()
            // Subtle full-height track: one hairline in the thumb's lane,
            // painted first so the thumb rides over it.
            .child(
                div()
                    .absolute()
                    .top(px(MENU_SCROLLBAR_TRACK_INSET))
                    .bottom(px(MENU_SCROLLBAR_TRACK_INSET))
                    .right(px(2.0 + thumb_width / 2.0 - 0.5))
                    .w(px(1.0))
                    .bg(theme.hairline(0.12)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(MENU_SCROLLBAR_TRACK_INSET + m.thumb_top))
                    .right(px(2.0))
                    .w(px(thumb_width))
                    .h(px(m.thumb_height))
                    .rounded(px(thumb_width / 2.0))
                    .bg(theme.text_faint.opacity(if active { 0.68 } else { 0.5 })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popover::MENU_SCROLLBAR_MIN_THUMB;

    fn m(viewport: f32, max_scroll: f32, current: f32) -> MenuScrollbarMetrics {
        metrics(viewport, max_scroll, -current).unwrap()
    }

    #[test]
    fn thumb_geometry_tracks_the_viewport_fraction() {
        // 600px viewport of 2000px content, scrolled to the middle.
        let metrics = m(600.0, 1400.0, 700.0);
        let track = 600.0 - MENU_SCROLLBAR_TRACK_INSET * 2.0;
        assert_eq!(metrics.track_height, track);
        let expected_thumb = track * 600.0 / 2000.0;
        assert!((metrics.thumb_height - expected_thumb).abs() < 1e-4);
        assert_eq!(metrics.travel(), track - expected_thumb);
        assert!((metrics.thumb_top - (track - expected_thumb) * 0.5).abs() < 1e-4);
        // The bottom maps to the full travel.
        let at_bottom = m(600.0, 1400.0, 1400.0);
        assert!((at_bottom.thumb_top - at_bottom.travel()).abs() < 1e-4);
        // The top maps to zero.
        assert_eq!(m(600.0, 1400.0, 0.0).thumb_top, 0.0);
    }

    #[test]
    fn long_transcripts_get_the_minimum_thumb() {
        // A very long chat clamps the thumb to the smallest readable size.
        let metrics = m(600.0, 59_400.0, 30_000.0);
        assert_eq!(metrics.thumb_height, MENU_SCROLLBAR_MIN_THUMB);
        assert_eq!(
            metrics.travel(),
            600.0 - MENU_SCROLLBAR_TRACK_INSET * 2.0 - MENU_SCROLLBAR_MIN_THUMB
        );
    }

    #[test]
    fn no_overflow_yields_no_rail() {
        assert!(metrics(600.0, 0.0, 0.0).is_none());
        assert!(metrics(0.0, 0.0, 0.0).is_none());
        // A negative accessor read (fresh bottom-aligned list) is still 0.
        assert!(metrics(600.0, 100.0, 40.0).is_some());
    }

    #[test]
    fn negative_list_offsets_read_as_scroll_top() {
        // scroll_px_offset_for_scrollbar is negative; distance_from_bottom is
        // max - scroll_top, so the pair must invert exactly.
        assert_eq!(scroll_top_from_list_offsets(1400.0, -700.0), 700.0);
        assert_eq!(scroll_top_from_list_offsets(1400.0, -0.0), 0.0);
        assert_eq!(scroll_top_from_list_offsets(1400.0, 50.0), 0.0);
        assert_eq!(scroll_top_from_list_offsets(1400.0, -99_999.0), 1400.0);
    }

    #[test]
    fn track_press_centers_the_thumb_and_jumps() {
        let metrics = m(600.0, 1400.0, 0.0);
        let track_top = 100.0;
        // A press far below the thumb (track click): the thumb centers under
        // the pointer first.
        let mut state = TranscriptScrollbarState::default();
        let pointer = 100.0 + MENU_SCROLLBAR_TRACK_INSET + metrics.travel() * 0.5;
        let pointer_in_track = pointer - track_top - MENU_SCROLLBAR_TRACK_INSET;
        let grab = grab_offset_for_press(&metrics, pointer_in_track);
        assert!((grab - metrics.thumb_height / 2.0).abs() < 1e-4);
        let scroll_top = state.press(metrics, track_top, pointer);
        // The centering invariant: the thumb's midpoint sits exactly under
        // the pointer after the jump.
        let thumb_top_after = scroll_top / metrics.max_scroll * metrics.travel();
        assert!(
            (thumb_top_after + metrics.thumb_height / 2.0 - pointer_in_track).abs() < 1e-4,
            "thumb centered under the pointer"
        );
    }

    #[test]
    fn thumb_press_keeps_the_grab_point() {
        let metrics = m(600.0, 1400.0, 0.0);
        // Press 5px into the thumb: the grab keeps that 5px offset, and the
        // scroll-top does not jump on press.
        let pointer_in_track = metrics.thumb_top + 5.0;
        let grab = grab_offset_for_press(&metrics, pointer_in_track);
        assert!((grab - 5.0).abs() < 1e-4);
        let mut state = TranscriptScrollbarState::default();
        let scroll_top = state.press(metrics, 0.0, pointer_in_track + MENU_SCROLLBAR_TRACK_INSET);
        assert_eq!(scroll_top, 0.0, "pressing the resting thumb does not move");
    }

    #[test]
    fn drag_maps_pointer_travel_onto_the_scroll_range() {
        let metrics = m(600.0, 1400.0, 0.0);
        let mut state = TranscriptScrollbarState::default();
        // Grab the resting thumb at its center.
        state.press(
            metrics,
            0.0,
            MENU_SCROLLBAR_TRACK_INSET + metrics.thumb_height / 2.0,
        );
        // Drag the thumb's center to the middle of the track: its TOP is
        // half the travel minus half the thumb, and that position maps
        // linearly onto the scroll range.
        let scroll_top = state
            .drag(
                metrics,
                0.0,
                MENU_SCROLLBAR_TRACK_INSET + metrics.travel() * 0.5,
            )
            .unwrap();
        let expected = (metrics.travel() * 0.5 - metrics.thumb_height / 2.0) / metrics.travel()
            * metrics.max_scroll;
        assert!((scroll_top - expected).abs() < 1e-4);
        // Past both ends it clamps.
        assert_eq!(state.drag(metrics, 0.0, -10_000.0).unwrap(), 0.0);
        assert_eq!(state.drag(metrics, 0.0, 10_000.0).unwrap(), 1400.0);
        // Without a drag engaged there is nothing to move.
        let idle = TranscriptScrollbarState::default();
        assert!(idle.drag(metrics, 0.0, 0.0).is_none());
    }

    #[test]
    fn hover_and_grab_drive_visibility() {
        let mut state = TranscriptScrollbarState::default();
        assert!(!state.visible());
        assert!(state.set_viewport_hovered(true));
        assert!(state.visible());
        // Leaving the viewport disarms the bar too (nothing grabbed).
        assert!(state.set_viewport_hovered(false));
        assert!(!state.visible());
        // A grab keeps the rail alive past the viewport edge, and widened.
        let metrics = m(600.0, 1400.0, 0.0);
        state.press(metrics, 0.0, 0.0);
        assert!(state.visible());
        assert!(state.active());
        // Release outside the viewport clears the active treatment.
        assert!(state.release());
        assert!(!state.visible());
        assert!(!state.release(), "release without a drag is a no-op");
    }
}
