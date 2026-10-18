//! Jev, TypeSafe's decision model (roadmap 8.3), as the playbook
//! [`Decider`]: closed questions during replays ("which search result
//! matches?") get a typed answer with a probability in a fraction of a
//! second, instead of a word match.
//!
//! The key comes from `JEV_API_KEY` or the keyring (service
//! `agent-unibrow-models`, account `jev`). Without one, or if Jev can't be
//! reached, decisions fall back to [`WordMatch`].

use crate::tools::{Decider, Decision, WordMatch};
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";
pub const KEYRING_SERVICE: &str = "agent-unibrow-models";
pub const KEYRING_ACCOUNT: &str = "jev";
/// Jev allows 255 options; one is kept for "none of these".
const MAX_OPTIONS: usize = 254;
/// Below this confidence Jev's pick isn't taken.
const MIN_CONFIDENCE: f32 = 0.5;
const NONE: &str = "none";

/// The Jev key, if one is set.
pub fn api_key() -> Option<String> {
    if let Ok(k) = std::env::var("JEV_API_KEY") {
        if !k.trim().is_empty() {
            return Some(k.trim().to_string());
        }
    }
    if let Some(k) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).ok().and_then(|e| e.get_password().ok()) {
        return Some(k);
    }
    // Stored with `secret-tool store … service agent-unibrow-models account jev`.
    let out = std::process::Command::new("secret-tool")
        .args(["lookup", "service", KEYRING_SERVICE, "account", KEYRING_ACCOUNT])
        .output()
        .ok()?;
    let k = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!k.is_empty()).then_some(k)
}

/// Jev when a key is set (and this isn't an isolated test instance),
/// otherwise the word match.
pub fn decider() -> Box<dyn Decider> {
    match Jev::from_saved_key() {
        Some(jev) => Box::new(jev),
        None => Box::new(WordMatch),
    }
}

/// A connection to Jev. Clones share the usage counts.
#[derive(Clone)]
pub struct Jev {
    key: String,
    http: reqwest::Client,
    usage: Arc<Usage>,
}

/// Calls made and tokens used, for "answered by Jev" notes.
#[derive(Default)]
pub struct Usage {
    pub calls: AtomicU64,
    pub input_tokens: AtomicU64,
}

impl Jev {
    pub fn new(key: String) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(8)).build().expect("http client");
        Self { key, http, usage: Arc::new(Usage::default()) }
    }

    /// Jev with the saved key, if there is one. Isolated test instances
    /// only use it when `UNIBROW_ALLOW_JEV=1`.
    pub fn from_saved_key() -> Option<Self> {
        let test_instance =
            std::env::var_os("UNIBROW_DATA_DIR").is_some() && std::env::var("UNIBROW_ALLOW_JEV").as_deref() != Ok("1");
        if test_instance {
            return None;
        }
        api_key().map(Self::new)
    }

    pub fn calls(&self) -> u64 {
        self.usage.calls.load(Ordering::Relaxed)
    }

    pub fn input_tokens(&self) -> u64 {
        self.usage.input_tokens.load(Ordering::Relaxed)
    }

    /// Asks `questions` (name → question) about `state`; returns the answers
    /// by name.
    pub async fn ask(&self, state: Value, questions: Value) -> Result<Value, String> {
        let body = json!({"model": MODEL, "state": state, "questions": questions});
        let mut tries = 0;
        loop {
            tries += 1;
            let resp = self.http.post(ENDPOINT).bearer_auth(&self.key).json(&body).send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            // Busy: back off and try again.
            if (status.as_u16() == 429 || status.as_u16() == 529) && tries < 3 {
                tokio::time::sleep(Duration::from_millis(400 * tries)).await;
                continue;
            }
            let v: Value = resp.json().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(format!("Jev: HTTP {status}: {v}"));
            }
            self.usage.calls.fetch_add(1, Ordering::Relaxed);
            self.usage.input_tokens.fetch_add(v["usage"]["input_tokens"].as_u64().unwrap_or(0), Ordering::Relaxed);
            return Ok(v["answers"].clone());
        }
    }

    /// Which of `options` fits `instructions`, given `state`? `None` when
    /// none does (or Jev isn't sure). Returns the index and its probability.
    pub async fn choose_one(&self, state: &str, instructions: &str, options: &[String]) -> Result<Option<(usize, f32)>, String> {
        self.choose_among(state, instructions, options, true).await
    }

    /// The best of `options`, with no "none of these" (for ranking
    /// candidates that are all plausible).
    pub async fn choose_best(&self, state: &str, instructions: &str, options: &[String]) -> Result<Option<(usize, f32)>, String> {
        if options.len() == 1 {
            return Ok(Some((0, 1.0)));
        }
        self.choose_among(state, instructions, options, false).await
    }

    /// Like [`Self::choose_one`], but says what Jev picked even when it
    /// isn't sure: `(Some(index), probability)`, or `(None, probability)`
    /// when it picked "none of these".
    pub async fn pick(&self, state: &str, instructions: &str, options: &[String]) -> Result<(Option<usize>, f32), String> {
        if options.is_empty() {
            return Ok((None, 1.0));
        }
        let mut criteria = Map::new();
        for (i, o) in options.iter().take(MAX_OPTIONS).enumerate() {
            criteria.insert(i.to_string(), Value::String(o.chars().take(300).collect()));
        }
        criteria.insert(NONE.into(), Value::String("None of these".into()));
        let a = self
            .ask(json!(state), json!({"pick": {"type": "choice", "instructions": instructions, "criteria": criteria}}))
            .await?;
        let choice = a["pick"]["choice"].as_str().ok_or_else(|| format!("unexpected answer: {a}"))?;
        let p = a["pick"]["probabilities"][choice].as_f64().unwrap_or(0.0) as f32;
        Ok((choice.parse::<usize>().ok().filter(|i| *i < options.len()), p))
    }

    async fn choose_among(&self, state: &str, instructions: &str, options: &[String], none: bool) -> Result<Option<(usize, f32)>, String> {
        if options.is_empty() {
            return Ok(None);
        }
        let mut criteria = Map::new();
        for (i, o) in options.iter().take(MAX_OPTIONS).enumerate() {
            criteria.insert(i.to_string(), Value::String(o.chars().take(300).collect()));
        }
        if none {
            criteria.insert(NONE.into(), Value::String("None of these".into()));
        }
        let answers = self
            .ask(json!(state), json!({"pick": {"type": "choice", "instructions": instructions, "criteria": criteria}}))
            .await?;
        let resp = json!({"answers": answers});
        let floor = if none { MIN_CONFIDENCE } else { 0.0 };
        Ok(answer_above(&resp, options.len().min(MAX_OPTIONS), floor)?.map(|d| (d.index, d.confidence)))
    }

    /// The probability Jev gives that `instructions` (a yes/no question) is
    /// true of `state`.
    pub async fn yes(&self, state: &str, instructions: &str) -> Result<f32, String> {
        let answers = self.ask(json!(state), json!({"q": {"type": "noul", "instructions": instructions}})).await?;
        answers["q"]["noul"].as_f64().map(|p| p as f32).ok_or_else(|| format!("unexpected answer: {answers}"))
    }
}

/// The request for "which of `options` is `wanted`?".
fn request(question: &str, wanted: &str, options: &[String]) -> Value {
    let mut criteria = Map::new();
    for (i, o) in options.iter().take(MAX_OPTIONS).enumerate() {
        criteria.insert(i.to_string(), Value::String(o.clone()));
    }
    criteria.insert(NONE.into(), Value::String("None of these is a good match".into()));
    json!({
        "model": MODEL,
        "state": format!("On a web page, looking for: {wanted}"),
        "questions": {
            "pick": {
                "type": "choice",
                "instructions": format!("{question} The one that is (or best matches) “{wanted}”."),
                "criteria": criteria,
            }
        }
    })
}

/// Jev's answer as a decision: `Ok(None)` when it says none match or isn't
/// sure enough.
fn answer(resp: &Value, options: usize) -> Result<Option<Decision>, String> {
    answer_above(resp, options, MIN_CONFIDENCE)
}

fn answer_above(resp: &Value, options: usize, floor: f32) -> Result<Option<Decision>, String> {
    let a = &resp["answers"]["pick"];
    let choice = a["choice"].as_str().ok_or_else(|| format!("unexpected answer: {resp}"))?;
    // The pick's probability; Jev's own `confidence` runs low whenever
    // "none" is a close second, even for a clear pick.
    let confidence = a["probabilities"][choice].as_f64().or(a["confidence"].as_f64()).unwrap_or(0.0) as f32;
    if choice == NONE || confidence < floor {
        return Ok(None);
    }
    let index: usize = choice.parse().map_err(|_| format!("unknown option {choice}"))?;
    if index >= options {
        return Err(format!("option {index} out of range"));
    }
    let model = resp["model"].as_str().unwrap_or(MODEL);
    Ok(Some(Decision { index, confidence, by: format!("Jev ({model})") }))
}

impl Decider for Jev {
    fn choose(&self, question: &str, wanted: &str, options: &[String]) -> BoxFuture<'static, Option<Decision>> {
        let fallback = WordMatch.choose(question, wanted, options);
        if options.is_empty() || wanted.trim().is_empty() {
            return fallback;
        }
        let body = request(question, wanted, options);
        let (jev, n) = (self.clone(), options.len().min(MAX_OPTIONS));
        Box::pin(async move {
            let asked = async {
                let answers = jev.ask(body["state"].clone(), body["questions"].clone()).await?;
                let mut v = json!({"answers": answers});
                v["model"] = json!(MODEL);
                answer(&v, n)
            };
            match asked.await {
                Ok(d) => d,
                // Jev unreachable: the word match decides instead.
                Err(e) => fallback.await.map(|d| Decision { by: format!("{} (Jev unavailable: {e})", d.by), ..d }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_a_closed_question_with_a_way_out() {
        let r = request("Which search result matches?", "bone jazz nut", &["TUSQ nut".into(), "Bone nut".into()]);
        let c = &r["questions"]["pick"]["criteria"];
        assert_eq!(c["0"], "TUSQ nut");
        assert_eq!(c["1"], "Bone nut");
        assert!(c[NONE].is_string());
        assert_eq!(r["questions"]["pick"]["type"], "choice");
    }

    #[test]
    fn reads_answers() {
        let v = json!({"model": "jev-1.13.0", "answers": {"pick": {"type": "choice", "choice": "1", "confidence": 0.97}}});
        let d = answer(&v, 2).unwrap().unwrap();
        assert_eq!((d.index, d.by.as_str()), (1, "Jev (jev-1.13.0)"));
        let none = json!({"answers": {"pick": {"choice": "none", "confidence": 0.99}}});
        assert!(answer(&none, 2).unwrap().is_none());
        let unsure = json!({"answers": {"pick": {"choice": "0", "confidence": 0.3}}});
        assert!(answer(&unsure, 2).unwrap().is_none());
        assert!(answer(&json!({"answers": {"pick": {"choice": "7", "confidence": 1.0}}}), 2).is_err());
    }

    /// Calls the real Jev; run with `cargo test -p rail-browser jev_live -- --ignored`.
    #[test]
    #[ignore]
    fn jev_live() {
        let key = api_key().expect("a Jev key in the keyring");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let options = vec!["Fender Jazz Bass nut, bone".to_string(), "TUSQ XL jazz bass nut".into(), "Guitar strap".into()];
        let d = rt.block_on(Jev::new(key).choose("Which search result matches?", "tusq jazz bass nut", &options));
        let d = d.expect("a pick");
        assert_eq!(d.index, 1);
        assert!(d.by.starts_with("Jev"), "{}", d.by);
    }
}
