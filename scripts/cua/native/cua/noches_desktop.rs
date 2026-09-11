// Included by impl_.rs so desktop input uses the existing session cursor helpers.
fn noches_display_definition(def: &mut ToolDef) {
    def.input_schema["properties"]["display_id"] = json!({"type":"string","minLength":1,"maxLength":256,
        "description":"Hyprland output name from get_screen_size.displays, e.g. DP-1. primary is a compatibility alias. Screenshot coordinates are native pixels LOCAL to this output."});
    def.input_schema["properties"]["expected_layout"] = json!({"type":"string",
        "description":"layout_token from the observation. Refuses input if output geometry changed."});
    def.description.push_str(" On native Hyprland, get_screen_size lists displays; get_desktop_state accepts display_id. Desktop pointer coordinates are local to that output's native PNG, never global logical coordinates. Echo layout_token as expected_layout for stale-layout protection. Named-display keyboard actions require a window target instead.");
}

fn noches_action(name: &str, args: &Value) -> anyhow::Result<(f64, f64, crate::wayland::noches_display::Action)> {
    use crate::wayland::noches_display::Action;
    fn number(args: &Value, name: &str) -> anyhow::Result<f64> {
        args.get(name).and_then(Value::as_f64).filter(|v| v.is_finite())
            .ok_or_else(|| anyhow::anyhow!("{name} must be a finite number"))
    }
    fn integer(args: &Value, name: &str, default: u32, max: u32) -> anyhow::Result<u32> {
        let value = match args.get(name) { Some(v) => v.as_u64().ok_or_else(|| anyhow::anyhow!("{name} must be an integer"))?, None => default as u64 };
        anyhow::ensure!(value > 0 && value <= max as u64, "{name} outside supported range 1..{max}");
        Ok(value as u32)
    }
    anyhow::ensure!(args.get("delivery_mode").is_none_or(|v| v == "foreground"), "desktop input requires foreground delivery");
    anyhow::ensure!(args.get("modifier").is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)), "modified desktop input is unsupported on this Wayland route");
    anyhow::ensure!(args.get("from_zoom").is_none_or(|v| v == false), "window zoom coordinates cannot target a display");
    for key in ["element_index", "element_token", "snapshot_id"] {
        anyhow::ensure!(args.get(key).is_none(), "{key} requires a window target");
    }
    let (x, y) = if name == "drag" { (number(args,"from_x")?, number(args,"from_y")?) } else { (number(args,"x")?, number(args,"y")?) };
    let action = match name {
        "move_cursor" => Action::Move,
        "click" => {
            let button = match args.get("button").and_then(Value::as_str).unwrap_or("left") {
                "left"=>272, "right"=>273, "middle"=>274, _=>anyhow::bail!("unknown button"),
            };
            Action::Click { button, count: integer(args,"count",1,3)? }
        }
        "scroll" => {
            anyhow::ensure!(args.get("by").is_none_or(|v| v == "line"), "output-bound scroll currently supports line units only");
            let direction = args.get("direction").and_then(Value::as_str).unwrap_or("");
            anyhow::ensure!(["up","down","left","right"].contains(&direction), "invalid scroll direction");
            Action::Scroll { direction: direction.into(), amount: integer(args,"amount",3,50)? }
        }
        "drag" => {
            anyhow::ensure!(args.get("button").is_none_or(|v| v == "left"), "output-bound drag currently supports the left button only");
            Action::Drag { end_x: number(args,"to_x")?, end_y: number(args,"to_y")?, duration_ms: integer(args,"duration_ms",500,5000)? }
        }
        _=>anyhow::bail!("unsupported desktop pointer action"),
    };
    Ok((x,y,action))
}

async fn noches_desktop_tool(name: &str, args: &mut Value, state: Option<&Arc<ToolState>>) -> Option<ToolResult> {
    use crate::wayland::noches_display as desktop;
    let inspect = matches!(name, "get_screen_size" | "get_desktop_state");
    let selected = match args.get("display_id") {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control) => s.clone(),
        Some(_) => return Some(ToolResult::error("display_id must be a nonempty output name")),
        None => "primary".to_owned(),
    };
    if !inspect && args.get("scope").and_then(Value::as_str) != Some("desktop") {
        return if args.get("display_id").is_some() || args.get("expected_layout").is_some() {
            Some(ToolResult::error("display_id and expected_layout require desktop scope"))
        } else { None };
    }
    if args.get("pid").is_some() || args.get("window_id").is_some() {
        return Some(ToolResult::error("display-scoped actions cannot contain a window target"));
    }
    let native = crate::wayland::is_wayland() && crate::wayland::hyprland::is_session();
    if !native || matches!(name, "type_text" | "press_key" | "hotkey") {
        if selected != "primary" || args.get("expected_layout").is_some() {
            return Some(ToolResult::error("this route cannot address a named display; use an exact window target for keyboard input"));
        }
        if let Some(map) = args.as_object_mut() { map.remove("display_id"); }
        return None;
    }
    let result: anyhow::Result<ToolResult> = async {
        let selection = selected.clone();
        let display = tokio::task::spawn_blocking(move || desktop::resolve(&selection)).await??;
        if let Some(token) = args.get("expected_layout") {
            anyhow::ensure!(token.as_str() == Some(display.layout_token().as_str()), "display layout changed; observe again");
        }
        let mut metadata = serde_json::to_value(&display)?;
        metadata["layout_token"] = json!(display.layout_token());
        metadata["coordinate_space"] = json!("output_native_pixels");
        metadata["scale_factor"] = json!(display.width as f64 / display.logical_width as f64);
        if name == "get_screen_size" {
            let displays = tokio::task::spawn_blocking(desktop::displays).await??;
            metadata["displays"] = serde_json::to_value(displays)?;
            return Ok(ToolResult::text(format!("Display {}: {}x{} native pixels", display.display_id, display.width, display.height)).with_structured(metadata));
        }
        if name == "get_desktop_state" {
            let d = display.clone();
            let png = tokio::task::spawn_blocking(move || desktop::capture(&d)).await??;
            metadata["platform"] = json!("linux");
            metadata["display"] = json!(display.display_id);
            metadata["screen_width"] = json!(display.width);
            metadata["screen_height"] = json!(display.height);
            metadata["screenshot_width"] = json!(display.width);
            metadata["screenshot_height"] = json!(display.height);
            metadata["screenshot_mime_type"] = json!("image/png");
            let mut result = ToolResult::text(format!("Display {} captured; use this display_id and layout_token for input.", display.display_id));
            if let Some(path) = args.get("screenshot_out_file") {
                let path = path.as_str().ok_or_else(|| anyhow::anyhow!("screenshot_out_file must be a string"))?.to_owned();
                let written = path.clone();
                tokio::task::spawn_blocking(move || std::fs::write(written, png)).await??;
                metadata["screenshot_file_path"] = json!(path);
            } else {
                use base64::Engine as _;
                result.content.push(cua_driver_core::protocol::Content::image_png(base64::engine::general_purpose::STANDARD.encode(png)));
            }
            return Ok(result.with_structured(metadata));
        }
        let (x,y,action) = noches_action(name,args)?;
        let (gx,gy) = display.logical_point(x,y)?;
        if let desktop::Action::Drag { end_x, end_y, .. } = &action { display.point(*end_x,*end_y)?; }
        if let Some(state) = state {
            reveal_pointer_action_for(state, &resolve_cursor_key(args), gx, gy, name == "click").await;
        }
        let d = display.clone();
        let end = match &action { desktop::Action::Drag {end_x,end_y,..} => Some((*end_x,*end_y)), _=>None };
        tokio::task::spawn_blocking(move || desktop::perform(&d,x,y,action)).await??;
        if let (Some(state),Some((ex,ey))) = (state,end) {
            let (ex,ey) = display.logical_point(ex,ey)?;
            reveal_pointer_action_for(state,&resolve_cursor_key(args),ex,ey,false).await;
        }
        metadata["effect"] = json!("unverifiable");
        metadata["path"] = json!("wayland_output_bound");
        metadata["scope"] = json!("desktop");
        Ok(ToolResult::text(format!("{name} sent to {}; inspect the same display to verify.",display.display_id)).with_structured(metadata))
    }.await;
    Some(result.unwrap_or_else(|error| ToolResult::error(error.to_string()).with_structured(json!({"code":"display_action_failed","display_id":selected}))))
}

#[cfg(test)]
mod noches_display_tool_tests {
    use super::*;
    #[test] fn rejects_wrong_coordinate_types() {
        assert!(noches_action("click",&json!({"x":"0","y":0})).is_err());
    }
    #[test] fn refuses_modifiers_without_moving_pointer() {
        assert!(noches_action("click",&json!({"x":0,"y":0,"modifier":["ctrl"]})).is_err());
    }
    #[test] fn count_is_not_clamped() {
        assert!(noches_action("click",&json!({"x":0,"y":0,"count":4})).is_err());
    }
    #[test] fn desktop_elements_cannot_silently_become_pixels() {
        assert!(noches_action("click",&json!({"x":0,"y":0,"element_token":"t"})).is_err());
    }
    #[test] fn zero_is_a_literal_coordinate() {
        let (x,y,_) = noches_action("move_cursor",&json!({"x":0,"y":0})).unwrap(); assert_eq!((x,y),(0.0,0.0));
    }
}
