//! CEF callback handlers. Each writes into the shared [`ViewState`].

use crate::{Cursor, DialogInfo, DownloadInfo, FileDialogRequest, Frame as PixelFrame, PopupLayer, ViewState};
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

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            let mut s = self.handler.state.borrow_mut();
            if show == 0 {
                s.popup = None;
            } else if s.popup.is_none() {
                s.popup = Some(PopupLayer { x: 0, y: 0, width: 0, height: 0, frame: None, frame_seq: 0 });
            }
            s.frame_seq += 1; // repaint either way
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(r) = rect else { return };
            let mut s = self.handler.state.borrow_mut();
            let p = s.popup.get_or_insert(PopupLayer { x: 0, y: 0, width: 0, height: 0, frame: None, frame_seq: 0 });
            p.x = r.x;
            p.y = r.y;
            p.width = r.width;
            p.height = r.height;
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
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = width as usize * height as usize * 4;
            // SAFETY: CEF guarantees `buffer` holds width*height BGRA pixels
            // for the duration of this call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
            let frame = PixelFrame { width: width as u32, height: height as u32, bgra: pixels.to_vec() };
            let mut s = self.handler.state.borrow_mut();
            if type_ == PaintElementType::from(cef::sys::cef_paint_element_type_t::PET_VIEW) {
                s.frame = Some(frame);
                s.frame_seq += 1;
            } else if let Some(p) = s.popup.as_mut() {
                // An open dropdown list, drawn over the page by the UI.
                p.frame = Some(frame);
                p.frame_seq += 1;
                s.frame_seq += 1;
            }
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
pub struct LifeSpan {
    state: Rc<RefCell<ViewState>>,
}

wrap_life_span_handler! {
    pub struct LifeSpanHandlerBuilder {
        handler: LifeSpan,
    }

    impl LifeSpanHandler {
        /// Links that want a new window or tab become tabs in the app (the
        /// UI opens them); Chromium's own popup window is cancelled.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let Some(url) = target_url.map(|u| u.to_string()).filter(|u| !u.is_empty()) {
                use cef::sys::cef_window_open_disposition_t as D;
                let raw: D = target_disposition.into();
                let foreground = raw != D::CEF_WOD_NEW_BACKGROUND_TAB;
                self.handler.state.borrow_mut().new_tabs.push((url, foreground));
            }
            1 // cancel Chromium's own popup window
        }
    }
}

// ------------------------------------------------------------ js dialogs

#[derive(Clone)]
pub struct Dialogs {
    state: Rc<RefCell<ViewState>>,
}

wrap_jsdialog_handler! {
    pub struct JsdialogHandlerBuilder {
        handler: Dialogs,
    }

    impl JsdialogHandler {
        /// Every alert/confirm/prompt is answered later, by the user (a card
        /// over the page) or the agent (`browser_dialog`).
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_jsdialog_type_t as T;
            let Some(cb) = callback else { return 0 };
            let raw: T = dialog_type.into();
            let kind = match raw {
                T::JSDIALOGTYPE_CONFIRM => "confirm",
                T::JSDIALOGTYPE_PROMPT => "prompt",
                _ => "alert",
            };
            let mut s = self.handler.state.borrow_mut();
            s.dialog = Some(DialogInfo {
                kind: kind.into(),
                message: message_text.map(|m| m.to_string()).unwrap_or_default(),
                default_text: default_prompt_text.map(|m| m.to_string()).unwrap_or_default(),
            });
            s.dialog_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let mut s = self.handler.state.borrow_mut();
            s.dialog = Some(DialogInfo {
                kind: "beforeunload".into(),
                message: message_text
                    .map(|m| m.to_string())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "Leave this page? Changes you made may not be saved.".into()),
                default_text: String::new(),
            });
            s.dialog_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }

        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            let mut s = self.handler.state.borrow_mut();
            s.dialog = None;
            s.dialog_callback = None;
        }

        fn on_dialog_closed(&self, _browser: Option<&mut Browser>) {
            let mut s = self.handler.state.borrow_mut();
            s.dialog = None;
            s.dialog_callback = None;
        }
    }
}

// ------------------------------------------------- find, files, downloads

#[derive(Clone)]
pub struct Misc {
    state: Rc<RefCell<ViewState>>,
}

wrap_find_handler! {
    pub struct FindHandlerBuilder {
        handler: Misc,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: ::std::os::raw::c_int,
            count: ::std::os::raw::c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: ::std::os::raw::c_int,
            _final_update: ::std::os::raw::c_int,
        ) {
            let mut s = self.handler.state.borrow_mut();
            s.find = Some((count, active_match_ordinal));
            s.frame_seq += 1;
        }
    }
}

/// A file name that doesn't overwrite anything already in `dir`.
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let clean: String = name.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let clean = if clean.trim().is_empty() { "download".to_string() } else { clean };
    let first = dir.join(&clean);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (clean.clone(), String::new()),
    };
    (1..1000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

wrap_download_handler! {
    pub struct DownloadHandlerBuilder {
        handler: Misc,
    }

    impl DownloadHandler {
        /// Allow downloads. (The binding's default returns 0, which would
        /// cancel every download once this handler exists.)
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            1
        }

        /// Downloads go straight to the agent-unibrow downloads folder.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            _download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let name = suggested_name.map(|n| n.to_string()).unwrap_or_default();
            let path = unique_path(&crate::downloads_dir(), &name);
            cb.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let text = |u: CefStringUserfree| CefString::from(&u).to_string();
            let path = text(item.full_path());
            let info = DownloadInfo {
                id: item.id(),
                name: std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| text(item.suggested_file_name())),
                path,
                url: text(item.url()),
                percent: item.percent_complete(),
                received: item.received_bytes(),
                total: item.total_bytes(),
                complete: item.is_complete() == 1,
                canceled: item.is_canceled() == 1 || item.is_interrupted() == 1,
            };
            let mut s = self.handler.state.borrow_mut();
            match s.downloads.iter_mut().find(|d| d.id == info.id) {
                Some(d) => *d = info,
                None => s.downloads.push(info),
            }
            s.frame_seq += 1;
        }
    }
}

wrap_dialog_handler! {
    pub struct DialogHandlerBuilder {
        handler: Misc,
    }

    impl DialogHandler {
        /// The page's file chooser: the UI shows the desktop's file picker.
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_file_dialog_mode_t as M;
            let Some(cb) = callback else { return 0 };
            let raw: M = mode.into();
            let mut s = self.handler.state.borrow_mut();
            s.file_dialog = Some(FileDialogRequest {
                multiple: raw == M::FILE_DIALOG_OPEN_MULTIPLE,
                title: title.map(|t| t.to_string()).unwrap_or_default(),
            });
            s.file_dialog_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }
    }
}

wrap_permission_handler! {
    pub struct PermissionHandlerBuilder {
        handler: Misc,
    }

    impl PermissionHandler {
        /// Camera and microphone: denied (roadmap 4.2: default deny).
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cancel();
            }
            let origin = requesting_origin.map(|o| o.to_string()).unwrap_or_default();
            self.handler.state.borrow_mut().blocked.push(format!("{origin} asked for the camera or microphone"));
            1
        }

        /// Location, notifications, clipboard and the rest: denied.
        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cont(PermissionRequestResult::from(
                    cef::sys::cef_permission_request_result_t::CEF_PERMISSION_RESULT_DENY,
                ));
            }
            let origin = requesting_origin.map(|o| o.to_string()).unwrap_or_default();
            self.handler.state.borrow_mut().blocked.push(format!("{origin} asked for a permission"));
            1
        }
    }
}

#[derive(Clone)]
pub struct PdfDone {
    done: Rc<RefCell<Option<futures::channel::oneshot::Sender<bool>>>>,
}

wrap_pdf_print_callback! {
    pub struct PdfCallbackBuilder {
        handler: PdfDone,
    }

    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, _path: Option<&CefString>, ok: ::std::os::raw::c_int) {
            if let Some(tx) = self.handler.done.borrow_mut().take() {
                let _ = tx.send(ok == 1);
            }
        }
    }
}

pub fn pdf_callback(done: futures::channel::oneshot::Sender<bool>) -> cef::PdfPrintCallback {
    PdfCallbackBuilder::new(PdfDone { done: Rc::new(RefCell::new(Some(done))) })
}

// ---------------------------------------------------------------- client

wrap_client! {
    pub struct ClientBuilder {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        dialogs: JsdialogHandler,
        find: FindHandler,
        downloads: DownloadHandler,
        files: DialogHandler,
        permissions: PermissionHandler,
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
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.dialogs.clone())
        }
        fn find_handler(&self) -> Option<FindHandler> {
            Some(self.find.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.downloads.clone())
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(self.files.clone())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permissions.clone())
        }
    }
}

pub fn client(state: Rc<RefCell<ViewState>>) -> cef::Client {
    ClientBuilder::new(
        RenderHandlerBuilder::new(Render { state: state.clone() }),
        DisplayHandlerBuilder::new(Display { state: state.clone() }),
        LoadHandlerBuilder::new(Load { state: state.clone() }),
        LifeSpanHandlerBuilder::new(LifeSpan { state: state.clone() }),
        JsdialogHandlerBuilder::new(Dialogs { state: state.clone() }),
        FindHandlerBuilder::new(Misc { state: state.clone() }),
        DownloadHandlerBuilder::new(Misc { state: state.clone() }),
        DialogHandlerBuilder::new(Misc { state: state.clone() }),
        PermissionHandlerBuilder::new(Misc { state }),
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
