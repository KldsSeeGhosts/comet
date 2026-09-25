//! Session status language shared by the sidebar, pane headers, and palette.
//!
//! Color encodes state and identity, never decoration. Each live state has
//! one hue (T3 Code's vocabulary): Working is sky, Awaiting input is indigo,
//! Completed-but-unseen is emerald, Failed uses the theme's danger role.
//! Queued and settled sessions stay neutral. The hues are explicit (like the
//! project monogram palette) so every theme reads the same state the same
//! way; only the light/dark variant follows the appearance.

use gpui::Hsla;
use zeron_proto::ChatIndicator;

use crate::theme::Theme;

/// The display state of one session, after send-truth overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// A send that was never adopted past the grace window.
    Failed,
    AwaitingInput,
    Working,
    /// A send waiting on a degraded delivery path.
    Queued,
    /// Finished while nobody was looking.
    Completed,
    Idle,
}

impl SessionState {
    /// Resolve the display state. Undelivered beats everything, then queued,
    /// then the engine's indicator.
    pub fn resolve(indicator: ChatIndicator, queued: bool, undelivered: bool) -> Self {
        if undelivered {
            return Self::Failed;
        }
        if queued {
            return Self::Queued;
        }
        match indicator {
            ChatIndicator::Errored => Self::Failed,
            ChatIndicator::AwaitingInput => Self::AwaitingInput,
            ChatIndicator::Working => Self::Working,
            ChatIndicator::Completed => Self::Completed,
            ChatIndicator::Idle => Self::Idle,
        }
    }

    /// True when the session is blocked on the user.
    pub fn needs_you(self) -> bool {
        matches!(self, Self::Failed | Self::AwaitingInput)
    }

    /// True while the agent is (or is about to be) doing work.
    pub fn running(self) -> bool {
        matches!(self, Self::Working | Self::Queued)
    }

    /// Short sentence-case label, or `None` for settled sessions.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Failed => Some("Failed"),
            Self::AwaitingInput => Some("Awaiting input"),
            Self::Working => Some("Working"),
            Self::Queued => Some("Queued"),
            Self::Completed => Some("Completed"),
            Self::Idle => None,
        }
    }

    /// The state's color, or `None` when it should render neutral.
    pub fn color(self, theme: &Theme) -> Option<Hsla> {
        let dark = theme.appearance == crate::theme::Appearance::Dark;
        let pick = |(on_dark, on_light): (u32, u32)| -> Hsla {
            gpui::rgb(if dark { on_dark } else { on_light }).into()
        };
        match self {
            Self::Failed => Some(theme.danger),
            Self::AwaitingInput => Some(pick(INDIGO)),
            Self::Working => Some(pick(SKY)),
            Self::Completed => Some(pick(EMERALD)),
            Self::Queued | Self::Idle => None,
        }
    }
}

/// (dark appearance, light appearance) pairs.
const SKY: (u32, u32) = (0x7dd3fc, 0x0284c7);
const INDIGO: (u32, u32) = (0xa5b4fc, 0x4f46e5);
const EMERALD: (u32, u32) = (0x6ee7b7, 0x059669);

/// Diff counts: additions and deletions, as in every code review tool.
pub fn diff_colors(theme: &Theme) -> (Hsla, Hsla) {
    let dark = theme.appearance == crate::theme::Appearance::Dark;
    if dark {
        (gpui::rgb(0x6ee7b7).into(), gpui::rgb(0xfca5a5).into())
    } else {
        (gpui::rgb(0x15803d).into(), gpui::rgb(0xb91c1c).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_truth_overrides_the_engine_indicator() {
        assert_eq!(
            SessionState::resolve(ChatIndicator::Working, true, true),
            SessionState::Failed
        );
        assert_eq!(
            SessionState::resolve(ChatIndicator::Idle, true, false),
            SessionState::Queued
        );
        assert_eq!(
            SessionState::resolve(ChatIndicator::AwaitingInput, false, false),
            SessionState::AwaitingInput
        );
        assert!(SessionState::Failed.needs_you());
        assert!(SessionState::Queued.running());
        assert_eq!(SessionState::Idle.label(), None);
    }
}
