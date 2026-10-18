//! CEF callback handlers. Each writes into the shared [`ViewState`].

use crate::{Cursor, Frame as PixelFrame, ViewState};
use cef::rc::Rc as _;
use cef::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

// ------------------------------------------------------------------- app

#[derive(Clone)]
pub struct UnibrowApp;

wrap_app! {
    pub struct AppBuilder {
        app: UnibrowApp,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cl) = command_line else { return };
            cl.append_switch(Some(&"noerrdialogs".into()));
            cl.append_switch(Some(&"hide-crash-restore-bubble".into()));
            // Store passwords in Chromium's own encrypted file, not the
            // desktop keyring (avoids keyring prompts from a subprocess).
            cl.append_switch_with_value(Some(&"password-store".into()), Some(&"basic".into()));
        }
    }
}

pub fn app() -> cef::App {
    AppBuilder::new(UnibrowApp)
}

// ---------------------------------------------------------------- render

#[derive(Clone)]
pub struct Render {
    state: Rc<RefCell<ViewState>>,
}

wrap_render_handler! {
    pub struct RenderHandlerBuilder {
        handler: Render,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let s = self.handler.state.borrow();
                rect.x = 0;
                rect.y = 0;
                rect.width = s.width.max(1);
                rect.height = s.height.max(1);
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(info) = screen_info else { return 0 };
            let s = self.handler.state.borrow();
            info.device_scale_factor = s.scale;
            let r = Rect { x: 0, y: 0, width: s.width.max(1), height: s.height.max(1) };
            info.rect = r.clone();
            info.available_rect = r;
            1
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // Popups (open <select> menus) paint separately; not shown yet.
            if type_ != PaintElementType::from(cef::sys::cef_paint_element_type_t::PET_VIEW) {
                return;
            }
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = width as usize * height as usize * 4;
            // SAFETY: CEF guarantees `buffer` holds width*height BGRA pixels
            // for the duration of this call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
            let mut s = self.handler.state.borrow_mut();
            s.frame = Some(PixelFrame { width: width as u32, height: height as u32, bgra: pixels.to_vec() });
            s.frame_seq += 1;
        }
    }
}

// --------------------------------------------------------------- display

#[derive(Clone)]
pub struct Display {
    state: Rc<RefCell<ViewState>>,
}

wrap_display_handler! {
    pub struct DisplayHandlerBuilder {
        handler: Display,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            let is_main = frame.map(|f| f.is_main() == 1).unwrap_or(true);
            if let (true, Some(url)) = (is_main, url) {
                self.handler.state.borrow_mut().url = url.to_string();
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(t) = title {
                self.handler.state.borrow_mut().title = t.to_string();
            }
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: ::std::os::raw::c_ulong,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_cursor_type_t as C;
            let raw: C = type_.into();
            let cursor = match raw {
                C::CT_HAND => Cursor::Pointer,
                C::CT_IBEAM | C::CT_VERTICALTEXT => Cursor::Text,
                C::CT_WAIT | C::CT_PROGRESS => Cursor::Wait,
                C::CT_CROSS => Cursor::Crosshair,
                C::CT_MOVE | C::CT_GRAB | C::CT_GRABBING => Cursor::Move,
                C::CT_NOTALLOWED | C::CT_NODROP => Cursor::NotAllowed,
                C::CT_COLUMNRESIZE | C::CT_EASTWESTRESIZE | C::CT_EASTRESIZE | C::CT_WESTRESIZE => {
                    Cursor::ResizeColumn
                }
                C::CT_ROWRESIZE | C::CT_NORTHSOUTHRESIZE | C::CT_NORTHRESIZE | C::CT_SOUTHRESIZE => {
                    Cursor::ResizeRow
                }
                _ => Cursor::Arrow,
            };
            self.handler.state.borrow_mut().cursor = cursor;
            1
        }
    }
}

// ------------------------------------------------------------------ load

#[derive(Clone)]
pub struct Load {
    state: Rc<RefCell<ViewState>>,
}

wrap_load_handler! {
    pub struct LoadHandlerBuilder {
        handler: Load,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            let mut s = self.handler.state.borrow_mut();
            s.loading = is_loading == 1;
            s.can_go_back = can_go_back == 1;
            s.can_go_forward = can_go_forward == 1;
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let is_main = frame.map(|f| f.is_main() == 1).unwrap_or(true);
            let code: cef::sys::cef_errorcode_t = error_code.into();
            // ERR_ABORTED (-3) fires for every navigation the user cancels.
            if !is_main || code == cef::sys::cef_errorcode_t::ERR_ABORTED {
                return;
            }
            let text = error_text.map(|t| t.to_string()).unwrap_or_default();
            let url = failed_url.map(|u| u.to_string()).unwrap_or_default();
            self.handler.state.borrow_mut().load_error = Some(format!("{text} ({url})"));
        }
    }
}

// ------------------------------------------------------------- life span

#[derive(Clone)]
pub struct LifeSpan;

wrap_life_span_handler! {
    pub struct LifeSpanHandlerBuilder {
        handler: LifeSpan,
    }

    impl LifeSpanHandler {
        /// There is one page view, so links that want a new window or tab
        /// open in the current page instead.
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let (Some(browser), Some(url)) = (browser, target_url) {
                if let Some(frame) = browser.main_frame() {
                    frame.load_url(Some(url));
                }
            }
            1 // cancel the popup window
        }
    }
}

// ---------------------------------------------------------------- client

wrap_client! {
    pub struct ClientBuilder {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
    }
}

pub fn client(state: Rc<RefCell<ViewState>>) -> cef::Client {
    ClientBuilder::new(
        RenderHandlerBuilder::new(Render { state: state.clone() }),
        DisplayHandlerBuilder::new(Display { state: state.clone() }),
        LoadHandlerBuilder::new(Load { state }),
        LifeSpanHandlerBuilder::new(LifeSpan),
    )
}

// -------------------------------------------------------------- devtools

/// Replies waiting for DevTools method results, by message id.
pub type Pending = Rc<RefCell<HashMap<i32, crate::bridge::Reply<serde_json::Value>>>>;

#[derive(Clone)]
pub struct Cdp {
    pending: Pending,
}

wrap_dev_tools_message_observer! {
    pub struct CdpObserverBuilder {
        handler: Cdp,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            let Some(reply) = self.handler.pending.borrow_mut().remove(&message_id) else { return };
            let parsed: serde_json::Value =
                result.and_then(|r| serde_json::from_slice(r).ok()).unwrap_or(serde_json::Value::Null);
            let outcome = if success == 1 {
                Ok(parsed)
            } else {
                Err(parsed["message"].as_str().map(str::to_string).unwrap_or_else(|| parsed.to_string()))
            };
            let _ = reply.send(outcome);
        }
    }
}

pub fn cdp_observer(pending: Pending) -> cef::DevToolsMessageObserver {
    CdpObserverBuilder::new(Cdp { pending })
}
