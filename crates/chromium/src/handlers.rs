use super::*;

wrap_client! {
    pub struct PageClient { data: Data }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> { Some(Paint::new(self.data.clone())) }
        fn display_handler(&self) -> Option<DisplayHandler> { Some(Display::new(self.data.clone())) }
        fn load_handler(&self) -> Option<LoadHandler> { Some(Load::new(self.data.clone())) }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> { Some(Life::new(self.data.clone())) }
        fn request_handler(&self) -> Option<RequestHandler> { Some(Navigation::new()) }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> { Some(ContextMenu::new(self.data.clone())) }
    }
}
wrap_render_handler! {
    struct Paint { data: Data }
    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect { let d=self.data.borrow();rect.width=d.width;rect.height=d.height; }
        }
        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> i32 {
            if let Some(info)=info {info.device_scale_factor=self.data.borrow().scale;return 1;} 0
        }
        fn on_paint(&self, _browser: Option<&mut Browser>, type_: PaintElementType, _dirty_rects: Option<&[Rect]>, buffer: *const u8, width: i32, height: i32) {
            if buffer.is_null() || width<=0 || height<=0 || width>8192 || height>8192 {return;}
            let bytes=unsafe {std::slice::from_raw_parts(buffer,(width as usize)*(height as usize)*4)};
            let mut d=self.data.borrow_mut();
            if type_==PaintElementType::VIEW {d.view=bytes.to_vec();d.view_width=width;d.view_height=height;}
            else {d.popup=bytes.to_vec();d.popup_width=width;d.popup_height=height;}
            d.frame();
        }
        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {if show==0 {let mut d=self.data.borrow_mut();d.popup.clear();d.popup_width=0;d.popup_height=0;d.frame();}}
        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {if let Some(rect)=rect {self.data.borrow_mut().popup_rect=rect.clone();}}
        fn on_ime_composition_range_changed(&self, _browser: Option<&mut Browser>, _selected_range: Option<&Range>, character_bounds: Option<&[Rect]>) {
            if let Some(rect)=character_bounds.and_then(|r|r.last()){let d=self.data.borrow();d.pipe.json(b'I',d.id,json!({"focused":true,"caret":[rect.x,rect.y,rect.width,rect.height]}));}
        }
        fn on_text_selection_changed(&self, _browser: Option<&mut Browser>, selected_text: Option<&CefString>, _selected_range: Option<&Range>) {
            let d=self.data.borrow();d.pipe.json(b'I',d.id,json!({"text":selected_text.map(|s|s.to_string()).unwrap_or_default(),"cursor":0,"selection":0}));
        }
    }
}
wrap_display_handler! {
    struct Display { data: Data }
    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|f|f.is_main()!=0) {let mut d=self.data.borrow_mut();d.state["url"]=url.map(|s|s.to_string()).into();d.state();}
        }
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {let mut d=self.data.borrow_mut();d.state["title"]=title.map(|s|s.to_string()).unwrap_or_default().into();d.state();}
    }
}
wrap_load_handler! {
    struct Load { data: Data }
    impl LoadHandler {
        fn on_loading_state_change(&self, _browser: Option<&mut Browser>, is_loading: i32, can_go_back: i32, can_go_forward: i32) {
            let mut d=self.data.borrow_mut();d.state["loading"]=(is_loading!=0).into();d.state["can_back"]=(can_go_back!=0).into();d.state["can_forward"]=(can_go_forward!=0).into();if is_loading!=0 {d.state["error"]=Value::Null;}d.state();
        }
        fn on_load_error(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, error_code: Errorcode, error_text: Option<&CefString>, _failed_url: Option<&CefString>) {
            if error_code!=Errorcode::ABORTED && frame.is_some_and(|f|f.is_main()!=0){let mut d=self.data.borrow_mut();d.state["loading"]=false.into();d.state["error"]=error_text.map(|s|s.to_string()).unwrap_or_else(||"Page load failed".into()).into();d.state();}
        }
    }
}
wrap_request_handler! {
    struct Navigation;
    impl RequestHandler {
        fn on_before_browse(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, request: Option<&mut Request>, _user_gesture: i32, _is_redirect: i32) -> i32 {
            // Subresources/frames may use blob/data internally. Top-level pages
            // never gain file:// or custom-protocol access through a redirect.
            if frame.is_some_and(|f|f.is_main()!=0) {return i32::from(!request.is_some_and(|r|allowed(&CefString::from(&r.url()).to_string())));} 0
        }
    }
}
wrap_life_span_handler! {
    struct Life { data: Data }
    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {self.data.borrow_mut().closed=true;}
        fn on_before_popup(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, _popup_id: i32, target_url: Option<&CefString>, _target_frame_name: Option<&CefString>, _target_disposition: WindowOpenDisposition, user_gesture: i32, _popup_features: Option<&PopupFeatures>, _window_info: Option<&mut WindowInfo>, _client: Option<&mut Option<Client>>, _settings: Option<&mut BrowserSettings>, _extra_info: Option<&mut Option<DictionaryValue>>, _no_javascript_access: Option<&mut i32>) -> i32 {
            if let Some(url)=target_url.map(|s|s.to_string()).filter(|url|allowed(url)) {if user_gesture!=0 {let d=self.data.borrow();d.pipe.send(b'N',d.id,url.into_bytes());}} 1
        }
    }
}
wrap_dev_tools_message_observer! {
    pub struct DevTools { data: Data }
    impl DevToolsMessageObserver {
        fn on_dev_tools_message(&self, _browser: Option<&mut Browser>, message: Option<&[u8]>) -> i32 {if let Some(bytes)=message {
                let d=self.data.borrow();
                if let Ok(value)=serde_json::from_slice::<Value>(bytes) {
                    if value.get("id").is_some() {d.pipe.send(b'A',d.id,bytes.to_vec());}
                    else if matches!(value["method"].as_str(),Some("Runtime.consoleAPICalled" | "Runtime.exceptionThrown" | "Network.requestWillBeSent" | "Network.responseReceived" | "Network.loadingFailed")) {d.pipe.telemetry(d.id,bytes.to_vec());}
                }
            }1}
    }
}
wrap_context_menu_handler! {
    struct ContextMenu { data: Data }
    impl ContextMenuHandler {
        fn on_before_context_menu(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, model: Option<&mut MenuModel>) {
            if let Some(model)=model {model.clear();}
            if let Some(params)=params {let d=self.data.borrow();d.pipe.json(b'M',d.id,json!({"x":params.xcoord(),"y":params.ycoord(),"items":[{"label":"Copy","action":"copy","enabled":true},{"label":"Cut","action":"cut","enabled":true},{"label":"Paste","action":"text","enabled":true},{"label":"Select all","action":"select-all","enabled":true}]}));}
        }
    }
}

wrap_app! {
    pub struct RuntimeApp;
    impl App {
        fn on_before_command_line_processing(&self, _process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
            if let Some(command_line) = command_line {
                #[cfg(target_os = "linux")]
                if std::env::var("WAYLAND_DISPLAY").is_ok_and(|value| !value.is_empty()) {
                    command_line.append_switch_with_value(Some(&"ozone-platform".into()), Some(&"wayland".into()));
                }
                for name in ["no-first-run", "no-default-browser-check", "no-startup-window"] {
                    command_line.append_switch(Some(&name.into()));
                }
            }
        }
    }
}
