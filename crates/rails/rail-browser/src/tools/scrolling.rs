//! Infinite scroll and lazy content (roadmap 2.5): keep scrolling until
//! nothing new loads, or until enough items are on the page.

use super::Browser;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Rounds of scrolling unless asked otherwise, and at most.
pub(super) const LOAD_ROUNDS: u64 = 12;
pub(super) const MAX_LOAD_ROUNDS: u64 = 40;
/// How long one round waits for new content before calling it done.
const ROUND_WAIT: Duration = Duration::from_millis(2_500);
/// Rounds in a row with nothing new before stopping.
const QUIET_ROUNDS: u32 = 2;

/// Measures the page: its height, and the largest group of repeated
/// siblings (the feed's items: same tag and class under one parent).
/// `scroll` (bool) scrolls the feed to its end first: the window, or the
/// biggest scrollable box when the page itself doesn't scroll.
const MEASURE_FN: &str = r#"(scroll) => {
  const doc = document.scrollingElement || document.documentElement;
  let box = doc;
  if (doc.scrollHeight <= innerHeight + 40) {
    let best = null, area = 0;
    for (const e of document.querySelectorAll('*')) {
      const s = getComputedStyle(e);
      if (!/(auto|scroll)/.test(s.overflowY) || e.scrollHeight <= e.clientHeight + 40) continue;
      const a = e.clientWidth * e.clientHeight;
      if (a > area) { area = a; best = e; }
    }
    if (best) box = best;
  }
  if (scroll) {
    box.scrollTop = box.scrollHeight;
    if (box === doc) window.scrollTo(0, doc.scrollHeight);
  }
  let items = 0;
  for (const p of document.querySelectorAll('ul, ol, tbody, [role=feed], [role=list], main, section, div')) {
    if (p.children.length < 3) continue;
    const counts = new Map();
    for (const c of p.children) {
      const k = c.tagName + '.' + (typeof c.className === 'string' ? c.className.trim().split(/\s+/).sort().join('.') : '');
      counts.set(k, (counts.get(k) || 0) + 1);
    }
    for (const n of counts.values()) if (n > items) items = n;
  }
  const atEnd = Math.ceil(box.scrollTop + box.clientHeight) >= box.scrollHeight - 4;
  return JSON.stringify({ height: box.scrollHeight, items, atEnd, inner: box !== doc });
}"#;

#[derive(Debug, Clone, Copy, Default)]
struct Measure {
    height: i64,
    items: i64,
    inner: bool,
}

impl Browser {
    async fn measure(&self, scroll: bool) -> Measure {
        let expr = format!("({MEASURE_FN})({scroll})");
        let v: Value = self
            .bridge
            .cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}))
            .await
            .ok()
            .and_then(|r| r["result"]["value"].as_str().and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or(Value::Null);
        Measure {
            height: v["height"].as_i64().unwrap_or(0),
            items: v["items"].as_i64().unwrap_or(0),
            inner: v["inner"].as_bool().unwrap_or(false),
        }
    }

    /// Scrolls to the end again and again until the page stops growing (or
    /// has `min_items` items, or `rounds` run out). Says how much loaded.
    pub(super) async fn load_more(&self, min_items: Option<u64>, rounds: u64) -> String {
        let start = self.measure(false).await;
        let began = Instant::now();
        let mut last = start;
        let mut quiet = 0;
        let mut done = 0;
        let mut why = "ran out of rounds";
        for _ in 0..rounds {
            if let Some(n) = min_items {
                if last.items as u64 >= n {
                    why = "enough items are on the page";
                    break;
                }
            }
            if self.state().await.map(|s| s.dialog.is_some()).unwrap_or(false) {
                why = "a dialog opened";
                break;
            }
            self.measure(true).await;
            // A wheel turn too: some feeds only listen for wheel or scroll
            // events, not for the position.
            let st = self.state().await.unwrap_or_default();
            let _ = self
                .bridge
                .cdp(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": st.width as f64 / 2.0, "y": st.height as f64 / 2.0, "deltaX": 0, "deltaY": st.height.max(400)}),
                )
                .await;
            done += 1;
            // Wait for the page to grow, up to a few seconds.
            let wait = Instant::now();
            let mut now = self.measure(false).await;
            while now.height == last.height && now.items == last.items && wait.elapsed() < ROUND_WAIT {
                tokio::time::sleep(Duration::from_millis(250)).await;
                now = self.measure(false).await;
            }
            if now.height == last.height && now.items == last.items {
                quiet += 1;
                if quiet >= QUIET_ROUNDS {
                    why = "nothing new appeared";
                    break;
                }
            } else {
                quiet = 0;
                self.network_quiet(Duration::from_secs(2)).await;
                now = self.measure(false).await;
            }
            last = now;
        }
        let mut out = format!(
            "Scrolled to the end {done} time(s) in {:.1}s; stopped because {why}. The page grew from {} to {} px",
            began.elapsed().as_secs_f64(),
            start.height,
            last.height
        );
        if last.items > 0 {
            out.push_str(&format!("; repeated items (the feed's entries): {} before, {} now", start.items, last.items));
        }
        out.push('.');
        if last.inner {
            out.push_str(" (The feed scrolls inside a box on the page, not the page itself.)");
        }
        if let Some(n) = min_items.filter(|n| (last.items as u64) < *n) {
            out.push_str(&format!(" Only {} of the {n} items asked for are there.", last.items));
        }
        out
    }
}
