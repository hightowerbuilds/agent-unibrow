//! Per-site permissions (roadmap 4.2): camera, microphone, location and
//! notifications.
//!
//! When a site asks, the request waits on a card over the page with Allow
//! and Deny. The answer is remembered per site (origin) in
//! `site_permissions.json` in the app data folder (`$UNIBROW_DATA_DIR` for
//! test instances), and can be forgotten from the Focus tab.
//!
//! Only a click on that card answers a request. No agent tool can grant a
//! permission: the agent can see that a request is waiting, nothing more.
//! Anything else a site asks for (clipboard, MIDI, screen capture, …) is
//! still denied.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Camera,
    Microphone,
    Location,
    Notifications,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Camera => "camera",
            Kind::Microphone => "microphone",
            Kind::Location => "location",
            Kind::Notifications => "notifications",
        }
    }
}

/// "camera and microphone", "location".
pub fn describe(kinds: &[Kind]) -> String {
    let names: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
    match names.as_slice() {
        [] => "a permission".into(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A request waiting for the user.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionPrompt {
    /// `https://example.com`
    pub origin: String,
    pub kinds: Vec<Kind>,
}

/// origin → kind → allowed.
type Stored = BTreeMap<String, BTreeMap<Kind, bool>>;

fn path() -> PathBuf {
    crate::data_dir().join("site_permissions.json")
}

fn store() -> &'static Mutex<Stored> {
    static STORE: OnceLock<Mutex<Stored>> = OnceLock::new();
    STORE.get_or_init(|| {
        let stored = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Mutex::new(stored)
    })
}

fn save(stored: &Stored) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(stored) {
        let _ = std::fs::write(p, s);
    }
}

/// `https://Example.com:443/` → `https://example.com`.
pub fn normalize_origin(origin: &str) -> String {
    match url::Url::parse(origin) {
        Ok(u) => u.origin().ascii_serialization(),
        Err(_) => origin.trim().trim_end_matches('/').to_ascii_lowercase(),
    }
}

/// What the user decided for all of `kinds` on `origin`: `Some(true)` if
/// every one is allowed, `Some(false)` if any is denied, `None` if they
/// haven't been asked about some.
pub fn decision(origin: &str, kinds: &[Kind]) -> Option<bool> {
    let s = store().lock().unwrap_or_else(|e| e.into_inner());
    decide(&s, &normalize_origin(origin), kinds)
}

fn decide(s: &Stored, origin: &str, kinds: &[Kind]) -> Option<bool> {
    let site = s.get(origin);
    let answers: Vec<Option<bool>> = kinds.iter().map(|k| site.and_then(|m| m.get(k).copied())).collect();
    if answers.contains(&Some(false)) {
        Some(false)
    } else if !kinds.is_empty() && answers.iter().all(|a| *a == Some(true)) {
        Some(true)
    } else {
        None
    }
}

/// Remembers the user's answer. Call only from a user's click.
pub fn remember(origin: &str, kinds: &[Kind], allow: bool) {
    let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
    let entry = s.entry(normalize_origin(origin)).or_default();
    for k in kinds {
        entry.insert(*k, allow);
    }
    save(&s);
}

/// Every remembered answer: (origin, kind, allowed).
pub fn list() -> Vec<(String, Kind, bool)> {
    let s = store().lock().unwrap_or_else(|e| e.into_inner());
    s.iter().flat_map(|(o, m)| m.iter().map(move |(k, a)| (o.clone(), *k, *a))).collect()
}

/// Forgets everything remembered for `origin`; it will ask again.
pub fn forget(origin: &str) {
    let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
    s.remove(&normalize_origin(origin));
    save(&s);
}

// CEF bit flags (cef_media_access_permission_types_t and
// cef_permission_request_types_t).
const MEDIA_AUDIO: u32 = 1;
const MEDIA_VIDEO: u32 = 2;
const PROMPT_CAMERA: u32 = 4;
const PROMPT_GEOLOCATION: u32 = 256;
const PROMPT_MIC: u32 = 4096;
const PROMPT_NOTIFICATIONS: u32 = 32768;

/// Kinds in a getUserMedia request; `None` if it asks for something we
/// don't offer (screen capture).
pub(crate) fn media_kinds(bits: u32) -> Option<Vec<Kind>> {
    if bits & !(MEDIA_AUDIO | MEDIA_VIDEO) != 0 || bits == 0 {
        return None;
    }
    let mut k = Vec::new();
    if bits & MEDIA_VIDEO != 0 {
        k.push(Kind::Camera);
    }
    if bits & MEDIA_AUDIO != 0 {
        k.push(Kind::Microphone);
    }
    Some(k)
}

/// Kinds in a permission prompt; `None` if it includes anything else.
pub(crate) fn prompt_kinds(bits: u32) -> Option<Vec<Kind>> {
    let known = PROMPT_CAMERA | PROMPT_GEOLOCATION | PROMPT_MIC | PROMPT_NOTIFICATIONS;
    if bits & !known != 0 || bits == 0 {
        return None;
    }
    let mut k = Vec::new();
    for (bit, kind) in [
        (PROMPT_CAMERA, Kind::Camera),
        (PROMPT_MIC, Kind::Microphone),
        (PROMPT_GEOLOCATION, Kind::Location),
        (PROMPT_NOTIFICATIONS, Kind::Notifications),
    ] {
        if bits & bit != 0 {
            k.push(kind);
        }
    }
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        let mut s = Stored::new();
        let o = "https://meet.example";
        assert_eq!(decide(&s, o, &[Kind::Camera]), None);
        s.entry(o.into()).or_default().insert(Kind::Camera, true);
        assert_eq!(decide(&s, o, &[Kind::Camera]), Some(true));
        assert_eq!(decide(&s, o, &[Kind::Camera, Kind::Microphone]), None, "mic not answered yet");
        s.entry(o.into()).or_default().insert(Kind::Microphone, false);
        assert_eq!(decide(&s, o, &[Kind::Camera, Kind::Microphone]), Some(false));
        assert_eq!(decide(&s, "https://other.example", &[Kind::Camera]), None, "per site");
        assert_eq!(decide(&s, o, &[]), None);
    }

    #[test]
    fn origins() {
        assert_eq!(normalize_origin("https://Meet.Example/"), "https://meet.example");
        assert_eq!(normalize_origin("https://meet.example:443/x"), "https://meet.example");
        assert_eq!(normalize_origin("http://localhost:8000/"), "http://localhost:8000");
    }

    #[test]
    fn kinds_from_bits() {
        assert_eq!(media_kinds(3), Some(vec![Kind::Camera, Kind::Microphone]));
        assert_eq!(media_kinds(1), Some(vec![Kind::Microphone]));
        assert_eq!(media_kinds(8), None, "screen capture isn't offered");
        assert_eq!(prompt_kinds(256), Some(vec![Kind::Location]));
        assert_eq!(prompt_kinds(32768 | 4096), Some(vec![Kind::Microphone, Kind::Notifications]));
        assert_eq!(prompt_kinds(16), None, "clipboard stays denied");
        assert_eq!(describe(&[Kind::Camera, Kind::Microphone]), "camera and microphone");
    }
}
