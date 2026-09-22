use super::*;
use serde_json::{Value, json};
use zeron_browser::{Action, ReplySender};

impl BrowserSurface {
    pub(crate) fn agent_action(
        &mut self,
        tab: u64,
        action: Action,
        reply: ReplySender,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &action {
            Action::Navigate { url, .. } => {
                if let Err(error) = model::normalize_address(url) {
                    let _ = reply.send(Err(error.into()));
                    return;
                }
                let focus = window.focused(cx);
                self.navigate(url, window, cx);
                if let Some(focus) = focus {
                    window.focus(&focus, cx);
                }
            }
            Action::Back { .. } => self.history(false),
            Action::Forward { .. } => self.history(true),
            Action::Reload { .. } => self.reload(cx),
            Action::Console { .. } | Action::Network { .. } => {
                let result = self
                    .native
                    .as_ref()
                    .map(|native| native.logs(matches!(action, Action::Network { .. })))
                    .ok_or_else(|| "Open a page first".into());
                let _ = reply.send(result);
                return;
            }
            Action::State { .. } => {}
            _ => {
                let Some(native) = &self.native else {
                    let _ = reply.send(Err("Open a page before inspecting this tab".into()));
                    return;
                };
                let screenshot = matches!(action, Action::Screenshot { .. });
                let evaluate = matches!(action, Action::Evaluate { .. });
                let press = matches!(action, Action::Press { .. });
                let mut key_down = None;
                let (method, params) = if screenshot {
                    (
                        "Page.captureScreenshot",
                        json!({"format":"png","captureBeyondViewport":false}),
                    )
                } else if let Action::Evaluate { expression, .. } = &action {
                    (
                        "Runtime.evaluate",
                        json!({"expression":expression,"returnByValue":true,"awaitPromise":true,"timeout":10000,"userGesture":true}),
                    )
                } else if let Action::Press { key, .. } = &action {
                    let code = match key.as_str() {
                        "Enter" => 13,
                        "Tab" => 9,
                        "Escape" => 27,
                        "Backspace" => 8,
                        "Delete" => 46,
                        "ArrowLeft" => 37,
                        "ArrowUp" => 38,
                        "ArrowRight" => 39,
                        "ArrowDown" => 40,
                        _ => {
                            let _ = reply.send(Err("Unsupported key".into()));
                            return;
                        }
                    };
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    key_down = Some(rx);
                    native.cdp("Input.dispatchKeyEvent",json!({"type":"keyDown","key":key,"windowsVirtualKeyCode":code,"text":if code==13 {"\r"} else {""}}),tx);
                    (
                        "Input.dispatchKeyEvent",
                        json!({"type":"keyUp","key":key,"windowsVirtualKeyCode":code}),
                    )
                } else if let Some(script) = zeron_browser::script::script(&action) {
                    (
                        "Runtime.evaluate",
                        json!({"expression":script,"returnByValue":true,"awaitPromise":true,"timeout":10000,"userGesture":true}),
                    )
                } else {
                    let _ = reply.send(Err("Unsupported browser action".into()));
                    return;
                };
                let (tx, rx) = tokio::sync::oneshot::channel();
                native.cdp(method, params, tx);
                cx.spawn(async move |_, cx| {
                    let timeout = cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(12));
                    let response = async move {
                        if let Some(down) = key_down {
                            match down.await {
                                Ok(Ok(_)) => {}
                                Ok(Err(error)) => return Ok(Err(error)),
                                Err(error) => return Err(error),
                            }
                        }
                        rx.await
                    };
                    let result = match futures::future::select(
                        Box::pin(response),
                        Box::pin(timeout),
                    )
                    .await
                    {
                        futures::future::Either::Left((Ok(result), _)) => result
                            .and_then(|value| parse_result(value, screenshot, evaluate, press)),
                        futures::future::Either::Left((Err(_), _)) => {
                            Err("Browser closed before the command completed".into())
                        }
                        futures::future::Either::Right(_) => Err(
                            "Browser operation timed out. Inspect the page before retrying.".into(),
                        ),
                    };
                    let _ = reply.send(result);
                })
                .detach();
                return;
            }
        }
        let _=reply.send(Ok(json!({"tab":tab,"url":self.page.url,"title":self.page.title,"loading":self.page.loading,"error":self.page.error,"canBack":self.page.can_back,"canForward":self.page.can_forward})));
    }
}
fn parse_result(
    value: Value,
    screenshot: bool,
    evaluate: bool,
    press: bool,
) -> zeron_browser::Reply {
    if screenshot {
        return value["data"]
            .as_str()
            .map(|png| json!({"png":png,"mimeType":"image/png"}))
            .ok_or_else(|| "Chromium returned no screenshot".into());
    }
    if value.get("exceptionDetails").is_some() {
        return Err(format!(
            "Browser script failed: {}",
            value["exceptionDetails"]["text"]
        ));
    }
    if press {
        return Ok(json!({"performed":"press"}));
    }
    if evaluate {
        return Ok(value["result"].get("value").cloned().unwrap_or(Value::Null));
    }
    let text = value["result"]["value"]
        .as_str()
        .ok_or("Chromium returned no page result")?;
    let result: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if let Some(error) = result["error"].as_str() {
        return Err(error.into());
    }
    Ok(result)
}
