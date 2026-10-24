//! The sidebar's width: dragged on the strip between the sidebar and the
//! page, from [`MIN`] to [`MAX`] (twice the default), and kept between
//! launches in `ui.json` in the app's settings folder (the instance's data
//! folder for a test instance).

use std::path::PathBuf;

pub const DEFAULT: f32 = 420.;
pub const MIN: f32 = 320.;
pub const MAX: f32 = DEFAULT * 2.;

/// What's being dragged: the sidebar's edge.
pub struct Resize;

/// Drawn under the pointer during the drag: nothing (the sidebar itself
/// follows the pointer).
pub struct Ghost;

impl gpui::Render for Ghost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        gpui::Empty
    }
}

/// A width in range, whole pixels.
pub fn clamp(w: f32) -> f32 {
    if w.is_finite() { w.clamp(MIN, MAX).round() } else { DEFAULT }
}

fn path() -> Option<PathBuf> {
    crate::home::folder("config").map(|d| d.join("ui.json"))
}

pub fn load() -> f32 {
    path().map(|p| load_from(&p)).unwrap_or(DEFAULT)
}

pub fn save(w: f32) {
    if let Some(p) = path() {
        save_to(&p, w);
    }
}

fn load_from(p: &std::path::Path) -> f32 {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["sidebar_width"].as_f64())
        .map(|w| clamp(w as f32))
        .unwrap_or(DEFAULT)
}

/// Keeps the file's other settings.
fn save_to(p: &std::path::Path, w: f32) {
    let mut v = std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    v["sidebar_width"] = serde_json::json!(clamp(w));
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, v.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_stay_in_range_and_are_kept() {
        assert_eq!(clamp(100.), MIN);
        assert_eq!(clamp(5000.), MAX);
        assert_eq!(MAX, 840.);
        assert_eq!(clamp(f32::NAN), DEFAULT);
        let dir = std::env::temp_dir().join(format!("sidebar-width-{}", std::process::id()));
        let p = dir.join("ui.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_from(&p), DEFAULT);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, r#"{"other": 1}"#).unwrap();
        save_to(&p, 612.4);
        assert_eq!(load_from(&p), 612.);
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"other\":1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
