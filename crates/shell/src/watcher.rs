//! Runs watches (roadmap 9.4): every half minute, checks the watches that
//! are due by loading their page in a background page, and reports the
//! changes the user asked to hear about.
//!
//! Runs on its own thread with its own Tokio runtime, so checks happen
//! whether or not an agent is running.

use browsing::{Browsing, WatchFired};
use rail_browser::BrowserBridge;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(30);

/// Test hook (isolated test instances only): check every watch every few
/// seconds instead of on its schedule.
fn fast() -> bool {
    std::env::var_os("UNIBROW_DATA_DIR").is_some() && std::env::var("UNIBROW_WATCH_FAST").as_deref() == Ok("1")
}

/// Let the browser start before the first check.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);

/// Starts the checker. Each check sends `None` (so the Focus tab can show
/// the new value), or `Some` when a watch fired.
pub fn spawn(store: Arc<Browsing>, bridge: BrowserBridge) -> async_channel::Receiver<Option<WatchFired>> {
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("watches".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async move {
                let fast = fast();
                tokio::time::sleep(if fast { Duration::from_secs(5) } else { FIRST_CHECK_AFTER }).await;
                loop {
                    let now = chrono::Local::now().timestamp() + if fast { 1_000_000_000 } else { 0 };
                    for w in store.due_watches(now) {
                        let result = rail_browser::tools::watch_value(
                            &bridge,
                            &w.url,
                            &w.kind,
                            w.selector.as_deref(),
                            w.needle.as_deref(),
                        )
                        .await;
                        let fired = store.record_watch_check(w.id, result);
                        if let Some(f) = &fired {
                            notify(f);
                        }
                        if tx.send(fired).await.is_err() {
                            return;
                        }
                    }
                    tokio::time::sleep(if fast { Duration::from_secs(3) } else { TICK }).await;
                }
            });
        })
        .expect("watch thread");
    rx
}

/// The line shown in chat and in the notification.
pub fn describe(f: &WatchFired) -> String {
    let change = match f.watch.when.as_str() {
        "drops" => "dropped",
        "rises" => "went up",
        "appears" => "now shows",
        "disappears" => "no longer shows",
        _ => "changed",
    };
    match f.watch.needle.as_deref() {
        Some(needle) if matches!(f.watch.when.as_str(), "appears" | "disappears") => {
            format!("{} {change} “{needle}”.", f.watch.label)
        }
        _ => format!("{} {change}: {} → {}", f.watch.label, f.old, f.new),
    }
}

/// A desktop notification (not from isolated test instances).
fn notify(f: &WatchFired) {
    if std::env::var_os("UNIBROW_DATA_DIR").is_some() {
        return;
    }
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=agent-unibrow", "Watch", &format!("{}\n{}", describe(f), f.watch.url)])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
