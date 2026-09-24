use cef::*;
use serde_json::Value;
use std::sync::atomic::{AtomicU8, Ordering};

fn flags(mods: u64) -> u32 {
    u32::from(mods & 1 != 0) * 2
        | u32::from(mods & 4 != 0) * 4
        | u32::from(mods & 8 != 0) * 8
        | u32::from(mods & (1 << 26) != 0) * 128
        | u32::from(mods & (1 << 8) != 0) * 16
        | u32::from(mods & (1 << 9) != 0) * 32
        | u32::from(mods & (1 << 10) != 0) * 64
}

/// CDP modifier bitmask (Alt=1, Ctrl=2, Meta=4, Shift=8).
fn cdp_flags(mods: u64) -> u32 {
    u32::from(mods & 8 != 0)
        | u32::from(mods & 4 != 0) * 2
        | u32::from(mods & (1 << 26) != 0) * 4
        | u32::from(mods & 1 != 0) * 8
}

/// CDP button bitmask of the currently held mouse buttons (Left=1, Right=2,
/// Middle=4); moves must report it so drags survive the CDP input path.
static BUTTONS: AtomicU8 = AtomicU8::new(0);

fn cdp_button_bit(button_id: u64) -> u8 {
    match button_id {
        2 => 4,
        3 => 2,
        _ => 1,
    }
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
            if cmd == "down" {
                host.set_focus(1);
            }
            let button_id = v["button"].as_u64().unwrap_or(1);
            if button_id == 8 {
                browser.go_back();
                return;
            }
            if button_id == 9 {
                browser.go_forward();
                return;
            }
            let held = cdp_button_bit(button_id);
            if cmd == "down" {
                BUTTONS.fetch_or(held, Ordering::Relaxed);
            } else {
                BUTTONS.fetch_and(!held, Ordering::Relaxed);
            }
            #[cfg(target_os = "linux")]
            {
                // CEF's windowless mouse-click API loses press/release events on Linux
                // even though mouse moves arrive. Deliver real input via Chromium's
                // input dispatcher, as we already do for keyboard events below.
                let button_name = match button_id {
                    2 => "middle",
                    3 => "right",
                    _ => "left",
                };
                let mods = v["mods"].as_u64().unwrap_or(0);
                let message = serde_json::json!({"id":if cmd == "down" {1000000001} else {1000000002},"method":"Input.dispatchMouseEvent","params":{
                    "type":if cmd == "down" {"mousePressed"} else {"mouseReleased"},
                    "x":mouse.x,"y":mouse.y,"button":button_name,"clickCount":1,"buttons":BUTTONS.load(Ordering::Relaxed),"modifiers":cdp_flags(mods)
                }});
                host.send_dev_tools_message(Some(&serde_json::to_vec(&message).unwrap()));
            }
            #[cfg(not(target_os = "linux"))]
            {
                let button = match button_id {
                    2 => MouseButtonType::MIDDLE,
                    3 => MouseButtonType::RIGHT,
                    _ => MouseButtonType::LEFT,
                };
                host.send_mouse_click_event(Some(&mouse), button, i32::from(cmd == "up"), 1);
            }
        }
        "move" => {
            #[cfg(target_os = "linux")]
            {
                // Windowless mouse moves are unreliable on Linux, exactly as
                // clicks were above: moves can be dropped entirely, which also
                // breaks the move-before-press ordering. Deliver them through
                // Chromium's input dispatcher alongside clicks and keys.
                let message = serde_json::json!({"id":1000000000,"method":"Input.dispatchMouseEvent","params":{
                    "type":"mouseMoved",
                    "x":mouse.x,"y":mouse.y,"button":"none","clickCount":0,
                    "buttons":BUTTONS.load(Ordering::Relaxed),
                    "modifiers":cdp_flags(v["mods"].as_u64().unwrap_or(0))
                }});
                host.send_dev_tools_message(Some(&serde_json::to_vec(&message).unwrap()));
            }
            #[cfg(not(target_os = "linux"))]
            {
                host.send_mouse_move_event(Some(&mouse), 0);
            }
        }
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
