//! Runs Jev work (roadmap Phase 11) on its own thread: learning focus sites,
//! and handling requests about learned sites without Claude.
//!
//! Like the watch checker, it has its own Tokio runtime, so it works
//! whether or not an agent is running.

use browsing::Browsing;
use browsing::jev_sites::{JevSite, JevSkills};
use rail_browser::BrowserBridge;
use rail_browser::jev::Jev;
use rail_browser::tools::{Handled, jev_handle, jev_learn};
use std::sync::Arc;
use std::time::Instant;

pub enum Job {
    /// Learn a site, starting from its home page or the given address.
    Learn { site: String, start: Option<String> },
    Handle { id: u64, site: JevSite, text: String },
}

pub enum Event {
    /// A line of learning progress for the Focus tab.
    Progress { site: String, line: String },
    /// Learning finished: what the site can do now, or why it failed.
    Learned { site: String, result: Result<Vec<&'static str>, String> },
    /// A request handled by Jev.
    Answer { id: u64, text: String, calls: u64, secs: f32 },
    /// A request Jev couldn't handle; it goes to Claude.
    Fallback { id: u64, reason: String },
}

const NO_KEY: &str = "No Jev key saved. Save one with: secret-tool store --label=\"agent-unibrow Jev key\" \
                      service agent-unibrow-models account jev";

pub fn spawn(store: Arc<Browsing>, bridge: BrowserBridge) -> (async_channel::Sender<Job>, async_channel::Receiver<Event>) {
    let (job_tx, jobs) = async_channel::unbounded::<Job>();
    let (tx, events) = async_channel::unbounded::<Event>();
    // Learning that was cut off (the app closed) didn't finish.
    for s in store.jev_sites().into_iter().filter(|s| s.status == "learning") {
        let mut log = s.log.clone();
        log.push("Interrupted: the app closed while learning".into());
        store.save_jev_site(&s.site, "failed", &s.skills, &log);
    }
    std::thread::Builder::new()
        .name("jev".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() else { return };
            rt.block_on(async move {
                while let Ok(job) = jobs.recv().await {
                    let (store, bridge, tx) = (store.clone(), bridge.clone(), tx.clone());
                    tokio::spawn(async move {
                        match job {
                            Job::Learn { site, start } => learn(&store, &bridge, &tx, site, start).await,
                            Job::Handle { id, site, text } => {
                                let started = Instant::now();
                                let ev = match Jev::from_saved_key() {
                                    None => Event::Fallback { id, reason: "no Jev key is saved".into() },
                                    Some(jev) => match jev_handle(&bridge, &jev, &site, &text).await {
                                        Ok(Handled::Answer { text }) => {
                                            Event::Answer { id, text, calls: jev.calls(), secs: started.elapsed().as_secs_f32() }
                                        }
                                        Ok(Handled::NotForJev(reason)) => Event::Fallback { id, reason },
                                        Err(e) => Event::Fallback { id, reason: format!("Jev ran into a problem ({e})") },
                                    },
                                };
                                let _ = tx.send(ev).await;
                            }
                        }
                    });
                }
            });
        })
        .expect("jev thread");
    (job_tx, events)
}

async fn learn(store: &Browsing, bridge: &BrowserBridge, tx: &async_channel::Sender<Event>, site: String, start: Option<String>) {
    let Some(jev) = Jev::from_saved_key() else {
        let _ = tx.send(Event::Learned { site, result: Err(NO_KEY.into()) }).await;
        return;
    };
    store.save_jev_site(&site, "learning", &JevSkills::default(), &[]);
    let progress = {
        let (tx, site) = (tx.clone(), site.clone());
        move |line: String| {
            let _ = tx.try_send(Event::Progress { site: site.clone(), line });
        }
    };
    let result = match jev_learn(bridge, store, &jev, &site, start.as_deref(), &progress).await {
        Ok((skills, log)) => {
            store.save_jev_site(&site, "ready", &skills, &log);
            Ok(skills.abilities())
        }
        Err(e) => {
            let log: Vec<String> = e.lines().map(str::to_string).collect();
            store.save_jev_site(&site, "failed", &JevSkills::default(), &log);
            Err(log.last().cloned().unwrap_or(e))
        }
    };
    let _ = tx.send(Event::Learned { site, result }).await;
}
