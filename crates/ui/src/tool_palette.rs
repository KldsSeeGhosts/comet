//! Tool-family icon hues for transcript tool rows (docs/design/
//! control-plane.md, "Tool rows"). Color encodes the identity of the action,
//! never decoration: the 14px icon carries the family hue while verbs stay
//! `text_muted` and details `text_faint`.
//!
//! The families are deliberately desaturated and sit in different hue
//! sectors than the session status hues (sky/indigo/emerald) so a tinted
//! tool icon can never be misread as session state. Commands (Run), plans
//! (Todo), MCP/unknown tools and thinking stay NEUTRAL - commands are the
//! bulk of the column, and keeping them quiet is what makes the tinted
//! families readable and keeps danger red rare.

use gpui::Hsla;
use zeron_proto::ToolCall;

use crate::theme::Theme;

/// The display family of one tool call, derived from the typed call.
/// `is_subagent_spawn` is the genus gate for Delegate (same convention the
/// subagent binding uses), plus the "Wait for agents" sentinel name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFamily {
    /// Reading/searching/listing/fetching - information in.
    Explore,
    /// Writing/editing/patching - tree changes.
    Change,
    /// Subagent spawns and agent-wait calls.
    Delegate,
    /// Everything else: exec, todo, mcp, unknown, thinking.
    Neutral,
}

impl ToolFamily {
    pub fn of(call: &ToolCall, is_thought: bool) -> Self {
        if is_thought {
            return Self::Neutral;
        }
        match call {
            ToolCall::ReadFile { .. }
            | ToolCall::Search { .. }
            | ToolCall::Glob { .. }
            | ToolCall::WebFetch { .. }
            | ToolCall::WebSearch { .. } => Self::Explore,
            ToolCall::WriteFile { .. } | ToolCall::EditFile { .. } | ToolCall::ApplyPatch { .. } => {
                Self::Change
            }
            _ if call.is_subagent_spawn() => Self::Delegate,
            ToolCall::Unknown { name, .. } if name == "Wait for agents" => Self::Delegate,
            _ => Self::Neutral,
        }
    }

    /// The family's icon color, or `None` for neutral (`text_muted` stays).
    pub fn color(self, theme: &Theme) -> Option<Hsla> {
        let dark = theme.appearance == crate::theme::Appearance::Dark;
        let pick = |(on_dark, on_light): (u32, u32)| -> Hsla {
            gpui::rgb(if dark { on_dark } else { on_light }).into()
        };
        match self {
            Self::Explore => Some(pick(TEAL)),
            Self::Change => Some(pick(AMBER)),
            Self::Delegate => Some(pick(ORCHID)),
            Self::Neutral => None,
        }
    }
}

/// (dark appearance, light appearance) pairs - desaturated off the status
/// hues so tool identity never reads as session state.
const TEAL: (u32, u32) = (0x7cc4bd, 0x2f7f78);
const AMBER: (u32, u32) = (0xd9b26f, 0x946514);
const ORCHID: (u32, u32) = (0xc9a0dc, 0x8a4a9e);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_split_by_action_kind() {
        assert_eq!(
            ToolFamily::of(
                &ToolCall::Search {
                    pattern: "x".into(),
                    path: None
                },
                false
            ),
            ToolFamily::Explore
        );
        assert_eq!(
            ToolFamily::of(
                &ToolCall::EditFile {
                    path: "f".into(),
                    old_string: None,
                    new_string: None
                },
                false
            ),
            ToolFamily::Change
        );
        assert_eq!(
            ToolFamily::of(
                &ToolCall::Unknown {
                    name: "Agent: probe".into(),
                    input: None
                },
                false
            ),
            ToolFamily::Delegate
        );
        assert_eq!(
            ToolFamily::of(
                &ToolCall::Exec {
                    command: "ls".into()
                },
                false
            ),
            ToolFamily::Neutral
        );
        assert_eq!(
            ToolFamily::of(
                &ToolCall::ReadFile { path: "f".into() },
                true
            ),
            ToolFamily::Neutral
        );
    }
}
