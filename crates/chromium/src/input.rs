use cef::*;
use serde_json::Value;

fn flags(mods: u64) -> u32 {
    u32::from(mods & 1 != 0) * 2
        | u32::from(mods & 4 != 0) * 4
        | u32::from(mods & 8 != 0) * 8
        | u32::from(mods & (1 << 26) != 0) * 128
        | u32::from(mods & (1 << 8) != 0) * 16
        | u32::from(mods & (1 << 9) != 0) * 32
        | u32::from(mods & (1 << 10) != 0) * 64
}
fn key_code(key: &str) -> i32 {
    match key {
        "Return" => 13,
        "Tab" => 9,
        "BackSpace" => 8,
        "Delete" => 46,
        "Escape" => 27,
        "Left" => 37,
        "Up" => 38,
        "Right" => 39,
        "Down" => 40,
        "Home" => 36,
        "End" => 35,
        "Page_Up" => 33,
        "Page_Down" => 34,
        "space" => 32,
        _ => key
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase() as i32)
            .unwrap_or(0),
    }
}
pub fn dispatch(cmd: &str, v: &Value, browser: &Browser, host: &BrowserHost) {
    let modifiers = flags(v["mods"].as_u64().unwrap_or(0));
    let mouse = MouseEvent {
        x: v["x"].as_f64().unwrap_or(0.) as i32,
        y: v["y"].as_f64().unwrap_or(0.) as i32,
        modifiers,
    };
    match cmd {
        "down" | "up" => {
            host.set_focus(1);
            let button = match v["button"].as_u64().unwrap_or(1) {
                2 => MouseButtonType::MIDDLE,
                3 => MouseButtonType::RIGHT,
                8 => {
                    browser.go_back();
                    return;
                }
                9 => {
                    browser.go_forward();
                    return;
                }
                _ => MouseButtonType::LEFT,
            };
            host.send_mouse_click_event(Some(&mouse), button, i32::from(cmd == "up"), 1);
        }
        "move" => host.send_mouse_move_event(Some(&mouse), 0),
        "scroll" => host.send_mouse_wheel_event(
            Some(&mouse),
            (-v["dx"].as_f64().unwrap_or(0.) * 40.) as i32,
            (-v["dy"].as_f64().unwrap_or(0.) * 40.) as i32,
        ),
        "key_down" | "key_up" => {
            let key = v["key"].as_str().unwrap_or("");
            let code = key_code(key);
            // CDP maps logical keys consistently across the CEF platform hosts.
            // Text entry comes separately from GPUI's native input/IME handler.
            let logical = match key {
                "Return" => "Enter",
                "BackSpace" => "Backspace",
                "Left" => "ArrowLeft",
                "Right" => "ArrowRight",
                "Up" => "ArrowUp",
                "Down" => "ArrowDown",
                "Page_Up" => "PageUp",
                "Page_Down" => "PageDown",
                "space" => " ",
                _ => key,
            };
            let mods = v["mods"].as_u64().unwrap_or(0);
            let cdp_mods = u32::from(mods & 8 != 0)
                | u32::from(mods & 4 != 0) * 2
                | u32::from(mods & (1 << 26) != 0) * 4
                | u32::from(mods & 1 != 0) * 8;
            let message = serde_json::json!({"id":0,"method":"Input.dispatchKeyEvent","params":{
                "type":if cmd=="key_up" {"keyUp"} else {"rawKeyDown"},
                "key":logical,"windowsVirtualKeyCode":code,"modifiers":cdp_mods
            }});
            host.send_dev_tools_message(Some(&serde_json::to_vec(&message).unwrap()));
            if cmd == "key_down" && code == 13 {
                let message = serde_json::json!({"id":0,"method":"Input.dispatchKeyEvent","params":{"type":"char","text":"\r","key":"Enter","windowsVirtualKeyCode":13,"modifiers":cdp_mods}});
                host.send_dev_tools_message(Some(&serde_json::to_vec(&message).unwrap()));
            }
        }
        "text" | "commit" => host.ime_commit_text(
            Some(&v["text"].as_str().unwrap_or("").into()),
            Some(&Range {
                from: u32::MAX,
                to: u32::MAX,
            }),
            0,
        ),
        "preedit" => {
            let text = v["text"].as_str().unwrap_or("");
            let count = text.encode_utf16().count() as u32;
            host.ime_set_composition(
                Some(&text.into()),
                None,
                Some(&Range {
                    from: u32::MAX,
                    to: u32::MAX,
                }),
                Some(&Range {
                    from: count,
                    to: count,
                }),
            );
        }
        "unmark" => host.ime_finish_composing_text(0),
        "copy" => {
            if let Some(frame) = browser.focused_frame() {
                frame.copy();
            }
        }
        "cut" => {
            if let Some(frame) = browser.focused_frame() {
                frame.cut();
            }
        }
        "select-all" => {
            if let Some(frame) = browser.focused_frame() {
                frame.select_all();
            }
        }
        _ => {}
    }
}
