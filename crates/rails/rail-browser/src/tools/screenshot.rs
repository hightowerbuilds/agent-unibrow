//! `browser_screenshot`: the visible page, or one element or region of it
//! ("screenshot this chart").
//!
//! A region is captured at up to two image pixels per CSS pixel (sharper
//! than the page screenshot, so small text and chart labels are legible),
//! with the longest side capped at what the model takes in. The image can't
//! be bigger than the page view itself: Chromium copies the capture out of
//! the view's surface, and a bigger one comes back tiled. Sensitive
//! fields are masked as dots in every capture, as for the page screenshot
//! (roadmap 6.4).

use super::{Browser, SENSITIVE_SELECTOR, Screenshot, check_keys, site};
use crate::bridge::Rect;
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolImage, ToolOutput, ToolResult};
use serde_json::{Value, json};

/// Longest side of a region image, in pixels (larger images are scaled
/// down by the model anyway).
const REGION_MAX_SIDE: f64 = 1568.0;
/// Most image pixels per CSS pixel for a region.
const REGION_MAX_ZOOM: f64 = 2.0;
const REGION_QUALITY: u32 = 90;
/// Space kept around an element, CSS px, so its edges show.
const ELEMENT_PADDING: f64 = 6.0;
/// Smallest region side, CSS px.
const MIN_SIDE: f64 = 4.0;

/// `r` grown by `pad` and cut to the `vw` x `vh` viewport. `None` if
/// nothing of it is on screen; the flag says whether it was cut (other
/// than the padding).
pub(super) fn fit_region(r: Rect, pad: f64, vw: f64, vh: f64) -> Option<(Rect, bool)> {
    if !(r.width.is_finite() && r.height.is_finite()) || r.width <= 0.0 || r.height <= 0.0 {
        return None;
    }
    let cut = r.x < -0.5 || r.y < -0.5 || r.x + r.width > vw + 0.5 || r.y + r.height > vh + 0.5;
    let x0 = (r.x - pad).max(0.0);
    let y0 = (r.y - pad).max(0.0);
    let x1 = (r.x + r.width + pad).min(vw);
    let y1 = (r.y + r.height + pad).min(vh);
    if x1 - x0 < MIN_SIDE.min(r.width) || y1 - y0 < MIN_SIDE.min(r.height) || x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((Rect { x: x0.floor(), y: y0.floor(), width: (x1 - x0).ceil(), height: (y1 - y0).ceil() }, cut))
}

/// Image pixels per CSS pixel for a `w` x `h` region of a page view that is
/// `view_w` x `view_h` device pixels: up to [`REGION_MAX_ZOOM`], less when
/// that would pass [`REGION_MAX_SIDE`] or the view's own size.
pub(super) fn region_zoom(w: f64, h: f64, view_w: f64, view_h: f64) -> f64 {
    let (w, h) = (w.max(1.0), h.max(1.0));
    (REGION_MAX_SIDE / w.max(h)).min(REGION_MAX_ZOOM).min(view_w / w).min(view_h / h)
}

/// A JPEG's pixel size, from its frame header.
pub(super) fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15, except DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

impl Browser {
    /// `Page.captureScreenshot` with `params`, sensitive fields masked as
    /// dots while it's taken (6.4). Returns the base64 image data.
    pub(super) async fn capture_masked(&self, params: Value) -> Result<String, String> {
        let css = format!("{SENSITIVE_SELECTOR} {{ -webkit-text-security: disc !important; }}");
        let mask = format!(
            "(() => {{ let s = document.getElementById('__unibrow_mask'); if (!s) {{ s = document.createElement('style'); \
             s.id = '__unibrow_mask'; (document.head || document.documentElement).appendChild(s); }} s.textContent = {}; }})()",
            serde_json::to_string(&css).unwrap_or_default()
        );
        let _ = self.bridge.cdp("Runtime.evaluate", json!({"expression": mask})).await;
        let r = self.bridge.cdp("Page.captureScreenshot", params).await;
        let _ = self
            .bridge
            .cdp("Runtime.evaluate", json!({"expression": "document.getElementById('__unibrow_mask')?.remove()"}))
            .await;
        Ok(r?["data"].as_str().ok_or("no screenshot data")?.to_string())
    }

    /// A sharp JPEG of `region` (viewport CSS px). Returns the image and
    /// the region actually captured, and whether it had to be cut to the
    /// visible page.
    async fn region_screenshot(&self, region: Rect, pad: f64) -> Result<(ToolImage, Rect, bool), String> {
        let st = self.state().await?;
        let (fit, cut) = fit_region(region, pad, st.width.max(1) as f64, st.height.max(1) as f64)
            .ok_or("That part of the page isn't on screen. Scroll to it (or give a ref) and try again.")?;
        // The clip is in page (document) coordinates; device pixels per CSS
        // pixel are already in the capture, so the scale is relative to them.
        let v = self
            .bridge
            .cdp(
                "Runtime.evaluate",
                json!({
                    "expression": "[visualViewport.pageLeft, visualViewport.pageTop, devicePixelRatio]",
                    "returnByValue": true
                }),
            )
            .await?;
        let nums: Vec<f64> = v["result"]["value"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let (sx, sy, dpr) = match nums[..] {
            [x, y, d] if d > 0.0 => (x, y, d),
            _ => (0.0, 0.0, 1.0),
        };
        let (vw, vh) = (st.width.max(1) as f64 * dpr, st.height.max(1) as f64 * dpr);
        let zoom = region_zoom(fit.width, fit.height, vw, vh);
        let params = json!({
            "format": "jpeg",
            "quality": REGION_QUALITY,
            "captureBeyondViewport": false,
            "clip": {"x": fit.x + sx, "y": fit.y + sy, "width": fit.width, "height": fit.height, "scale": zoom / dpr},
        });
        let data = self.capture_masked(params).await?;
        Ok((ToolImage { mime: "image/jpeg".into(), base64: data }, fit, cut))
    }
}

impl Tool for Screenshot {
    fn name(&self) -> &'static str {
        "browser_screenshot"
    }
    fn description(&self) -> &'static str {
        "Take a screenshot of what's visible in the app's browser. Use it when the outline from browser_read \
         isn't enough: images, charts, maps, canvas apps, or checking how something looks. To look closely at \
         one thing (a chart, a product photo, a table, an error message), pass its `ref` from browser_read: \
         you get just that element, scrolled into view, at higher resolution. Or pass `clip`, a rectangle in \
         viewport CSS pixels."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Optional: element ref from browser_read (e.g. e123) to capture just that element."},
                "clip": {
                    "type": "object",
                    "description": "Optional: a region of the visible page, in viewport CSS pixels.",
                    "properties": {
                        "x": {"type": "number"}, "y": {"type": "number"},
                        "width": {"type": "number"}, "height": {"type": "number"}
                    },
                    "required": ["x", "y", "width", "height"],
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        match (input["ref"].as_str(), input.get("clip").filter(|c| c.is_object())) {
            (Some(r), _) => format!("Screenshot {r}"),
            (None, Some(_)) => "Screenshot part of the page".into(),
            _ => "Screenshot the page".into(),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref", "clip"])?;
            let r = input["ref"].as_str().filter(|s| !s.trim().is_empty());
            let clip = input.get("clip").filter(|c| !c.is_null());
            if r.is_some() && clip.is_some() {
                return Err("Invalid input: give `ref` or `clip`, not both".into());
            }
            if let Some(r) = r {
                let el = b.element(r).await?;
                let rect = b.rect(&el).await?;
                let (img, fit, cut) = b.region_screenshot(rect, ELEMENT_PADDING).await?;
                let st = b.state().await.unwrap_or_default();
                return Ok(region_output(img, &format!("[{r}] {}", el.label()), &st.url, fit, cut));
            }
            if let Some(c) = clip {
                let num = |k: &str| c[k].as_f64().ok_or_else(|| format!("Invalid input: `clip.{k}` must be a number"));
                let rect = Rect { x: num("x")?, y: num("y")?, width: num("width")?, height: num("height")? };
                let (img, fit, cut) = b.region_screenshot(rect, 0.0).await?;
                let st = b.state().await.unwrap_or_default();
                return Ok(region_output(img, "a region", &st.url, fit, cut));
            }
            let st = b.state().await.unwrap_or_default();
            let data = b.capture_masked(json!({"format": "jpeg", "quality": 75, "captureBeyondViewport": false})).await?;
            Ok(ToolOutput {
                content: format!("Screenshot of {} ({}x{} CSS px).", st.url, st.width, st.height),
                summary: format!("screenshot of {}", site(&st.url)),
                images: vec![ToolImage { mime: "image/jpeg".into(), base64: data }],
                ..Default::default()
            })
        })
    }
}

fn region_output(img: ToolImage, what: &str, url: &str, fit: Rect, cut: bool) -> ToolOutput {
    use base64::Engine;
    let px = base64::engine::general_purpose::STANDARD
        .decode(&img.base64)
        .ok()
        .and_then(|b| jpeg_size(&b))
        .map(|(w, h)| format!(", image {w}x{h} px"))
        .unwrap_or_default();
    let mut content = format!(
        "Screenshot of {what} on {url}: {}x{} CSS px at ({}, {}) in the viewport{px}.",
        fit.width, fit.height, fit.x, fit.y
    );
    if cut {
        content.push_str(" It doesn't all fit on screen, so this is the visible part; scroll and take another for the rest.");
    }
    ToolOutput { content, summary: format!("screenshot of {what} on {}", site(url)), images: vec![img], ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    #[test]
    fn regions_fit_the_viewport() {
        let (r, cut) = fit_region(rect(100., 50., 200., 80.), 6., 1000., 700.).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (94., 44., 212., 92.));
        assert!(!cut);
        // Taller than the screen: the visible part, flagged.
        let (r, cut) = fit_region(rect(10., 20., 300., 2000.), 0., 1000., 700.).unwrap();
        assert_eq!((r.y, r.height), (20., 680.));
        assert!(cut);
        // Padding alone doesn't count as cut.
        assert!(!fit_region(rect(0., 0., 50., 50.), 6., 1000., 700.).unwrap().1);
        // Off screen, or empty.
        assert!(fit_region(rect(10., 900., 100., 100.), 0., 1000., 700.).is_none());
        assert!(fit_region(rect(10., 10., 0., 100.), 0., 1000., 700.).is_none());
        assert!(fit_region(rect(10., 10., f64::NAN, 100.), 0., 1000., 700.).is_none());
    }

    #[test]
    fn small_regions_get_sharper() {
        let big = (4000., 4000.);
        assert_eq!(region_zoom(300., 200., big.0, big.1), 2.0);
        assert!((region_zoom(1568., 400., big.0, big.1) - 1.0).abs() < 1e-9);
        assert!(region_zoom(3000., 100., big.0, big.1) < 1.0);
        assert!(region_zoom(0., 0., big.0, big.1) <= 2.0);
        // Never bigger than the 966 x 770 view (it would come back tiled).
        let z = region_zoom(764., 437., 966., 770.);
        assert!(764. * z <= 966. + 1e-9 && 437. * z <= 770. + 1e-9 && z > 1.0, "{z}");
        assert_eq!(region_zoom(300., 200., 966., 770.), 2.0);
    }

    #[test]
    fn reads_jpeg_sizes() {
        // SOI, APP0 (len 16), SOF0 (len 17) with 480 high x 640 wide.
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        j.extend(std::iter::repeat_n(0u8, 14));
        j.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03]);
        j.extend(std::iter::repeat_n(0u8, 12));
        assert_eq!(jpeg_size(&j), Some((640, 480)));
        assert_eq!(jpeg_size(&[0xFF, 0xD8]), None);
    }
}
