//! Fixed DOM helpers encode all arguments as JSON. Explicit evaluate actions
//! are dispatched separately by the UI.
use crate::Action;

pub fn script(action: &Action) -> Option<String> {
    let operation = match action {
        Action::Snapshot { .. } => serde_json::json!({"kind":"snapshot"}),
        Action::Click { reference, .. } => {
            serde_json::json!({"kind":"click","reference":reference})
        }
        Action::Fill {
            reference, text, ..
        } => serde_json::json!({"kind":"fill","reference":reference,"text":text}),
        Action::Select {
            reference, value, ..
        } => serde_json::json!({"kind":"select","reference":reference,"value":value}),
        Action::Scroll { x, y, .. } => serde_json::json!({"kind":"scroll","x":x,"y":y}),
        _ => return None,
    };
    Some(format!(
        "JSON.stringify(({})({}))",
        include_str!("page.js"),
        operation
    ))
}
