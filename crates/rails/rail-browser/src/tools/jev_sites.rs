//! Jev sites (roadmap Phase 11): focus sites that Jev has learned, so the
//! app can handle requests about them without Claude.
//!
//! - [`learn`] explores a site in hidden pages and lets Jev make every
//!   judgment call: which box is the search, whether a page is search
//!   results, what a result and a product page look like, which link is the
//!   cart. What it finds is saved with the site (`browsing::jev_sites`).
//! - [`handle`] answers a request about a learned site. One Jev call reads
//!   the request: what kind it is (find, price, stock, open, cart, a
//!   question about a product, or something else) and which words name the
//!   product. The app then does it and answers from a template. Anything
//!   Jev can't do, or isn't sure of, is handed back for Claude.
//! - Questions about a product ("does it come with glue?") are answered
//!   from the page itself: the whole page goes to the decision model, which
//!   picks the passage that answers it, and the reply quotes it. Long pages
//!   go to SubQ, which reads up to 384K tokens.
//!
//! The decision models answer typed questions only (pick one, yes/no); they
//! never write text, so every reply here is the app's own wording around
//! what it read from the page.

use super::Browser;
use super::extract::{BgPage, run_js};
use crate::BrowserBridge;
use crate::jev::Jev;
use browsing::jev_sites::{JevSite, JevSkills};
use browsing::sitemap::{page_kind, url_pattern};
use browsing::{Browsing, HistoryQuery};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// Query keys that carry the search words.
const QUERY_KEYS: &[&str] = &[
    "q", "k", "query", "s", "search", "keyword", "keywords", "term", "searchterm", "search_query", "text", "w", "st", "kw",
    "ntt", "searchtext",
];

/// Lists the page's text inputs that look like they take a search.
const FORMS_JS: &str = r#"(() => {
  const vis = (e) => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const out = [];
  for (const i of document.querySelectorAll('input')) {
    const t = (i.getAttribute('type') || 'text').toLowerCase();
    if (!['search', 'text'].includes(t) || !i.name) continue;
    const f = i.form;
    const label = (i.labels && i.labels[0] && i.labels[0].innerText) || '';
    out.push({
      name: i.name, placeholder: i.placeholder || '', aria: i.getAttribute('aria-label') || '',
      label: label.trim().slice(0, 80), type: t, visible: vis(i),
      action: f ? (f.getAttribute('action') ? f.action : location.href) : '',
      method: f ? (f.getAttribute('method') || 'get').toLowerCase() : '',
    });
    if (out.length >= 40) break;
  }
  return out;
})()"#;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

// ------------------------------------------------------------ learning

/// The search address in a URL like `/search?q=tusq+nut`, with `{q}` for
/// the words, and the words searched.
fn template_from(url: &str) -> Option<(String, String)> {
    let u = url::Url::parse(url).ok()?;
    let (key, val) = u
        .query_pairs()
        .find(|(k, v)| QUERY_KEYS.contains(&k.to_ascii_lowercase().as_str()) && v.chars().any(char::is_alphabetic))?;
    let mut base = u.clone();
    base.set_query(None);
    base.set_fragment(None);
    Some((format!("{base}?{key}={{q}}"), val.to_string()))
}

/// Ways to search the site, best first: from the user's own searches there,
/// from its search box (Jev picks which box), then common addresses.
async fn search_candidates(
    page: &BgPage,
    store: &Browsing,
    jev: &Jev,
    site: &str,
    home: &str,
    log: &mut Vec<String>,
) -> (Vec<String>, Option<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut query = None;
    let visits = store.history(&HistoryQuery { site: Some(site.to_string()), limit: 300, ..Default::default() });
    for v in &visits {
        if page_kind(&url_pattern(&v.url), &v.title) != "search" {
            continue;
        }
        if let Some((t, q)) = template_from(&v.url) {
            if !out.contains(&t) {
                log.push(format!("Search: found one of your searches here (“{q}”)"));
                query.get_or_insert(q);
                out.push(t);
            }
        }
    }
    let inputs = page.eval_expr(FORMS_JS).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let usable: Vec<&Value> = inputs.iter().filter(|i| s(i, "method") == "get" && !s(i, "action").is_empty()).collect();
    if !usable.is_empty() {
        let options: Vec<String> = usable
            .iter()
            .map(|i| {
                format!(
                    "input name={} placeholder=“{}” label=“{}” aria=“{}” type={}{} (form goes to {})",
                    s(i, "name"),
                    s(i, "placeholder"),
                    s(i, "label"),
                    s(i, "aria"),
                    s(i, "type"),
                    if i["visible"].as_bool() == Some(true) { "" } else { ", hidden" },
                    s(i, "action")
                )
            })
            .collect();
        let pick = jev
            .choose_best(
                &format!("Text boxes on the home page of {site}"),
                "Which of these is the site's main search box, for searching its products or content?",
                &options,
            )
            .await
            .ok()
            .flatten();
        let mut ordered: Vec<&Value> = Vec::new();
        if let Some((i, conf)) = pick {
            log.push(format!("Search: Jev picked the search box “{}” (confidence {conf:.2})", s(usable[i], "name")));
            ordered.push(usable[i]);
        }
        ordered.extend(usable.iter().filter(|i| QUERY_KEYS.contains(&s(i, "name").to_ascii_lowercase().as_str())));
        for i in ordered {
            let t = format!("{}?{}={{q}}", s(i, "action").split(['?', '#']).next().unwrap_or(""), s(i, "name"));
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    if let Ok(u) = url::Url::parse(home) {
        let origin = u.origin().ascii_serialization();
        for path in ["/search?q={q}", "/s?k={q}", "/search?query={q}"] {
            let t = format!("{origin}{path}");
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    (out, query)
}

/// One search result as the page lists it.
#[derive(Clone, Debug)]
struct Result_ {
    title: String,
    url: String,
    price: String,
}

fn results_of(v: &Value) -> Vec<Result_> {
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| Result_ { title: s(i, "title").into(), url: s(i, "url").into(), price: s(i, "price").into() })
                .filter(|r| !r.title.is_empty() && r.url.starts_with("http"))
                .collect()
        })
        .unwrap_or_default()
}

fn results_text(results: &[Result_], max: usize) -> String {
    results
        .iter()
        .take(max)
        .enumerate()
        .map(|(i, r)| if r.price.is_empty() { format!("{}. {}", i + 1, r.title) } else { format!("{}. {} — {}", i + 1, r.title, r.price) })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The URL pattern most results share.
fn result_pattern(results: &[Result_]) -> Option<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in results {
        *counts.entry(url_pattern(&r.url)).or_default() += 1;
    }
    counts.into_iter().filter(|(_, n)| *n >= 2).max_by_key(|(_, n)| *n).map(|(p, _)| p)
}

/// Learns `site`: its search, results, product pages and cart. `progress`
/// gets a line per finding. Errors only when nothing usable was learned.
pub async fn learn(
    bridge: &BrowserBridge,
    store: &Browsing,
    jev: &Jev,
    site: &str,
    start: Option<&str>,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<(JevSkills, Vec<String>), String> {
    let mut log: Vec<String> = Vec::new();
    let say = |log: &mut Vec<String>, line: String| {
        progress(line.clone());
        log.push(line);
    };
    let start = start.map(str::to_string).unwrap_or_else(|| format!("https://{site}/"));
    let page = BgPage::open(bridge, &start).await?;
    let home = page.url().await.unwrap_or(start);
    let mut skills = JevSkills { home: home.clone(), ..Default::default() };
    say(&mut log, format!("Opened {home}"));
    let home_links = page.eval(json!({"mode": "links", "max_links": 400})).await.unwrap_or_default();
    let links: Vec<(String, String, String)> = home_links["links"]
        .as_array()
        .map(|a| a.iter().map(|l| (s(l, "text").to_string(), s(l, "url").to_string(), s(l, "area").to_string())).collect())
        .unwrap_or_default();

    // Search.
    let mut found_log = Vec::new();
    let (candidates, history_query) = search_candidates(&page, store, jev, site, &home, &mut found_log).await;
    for l in found_log {
        say(&mut log, l);
    }
    // Things to search for: the user's own search here, then what the
    // site's links name (Jev's pick of a product first).
    let title = s(&home_links, "title").to_lowercase();
    let mut queries: Vec<String> = history_query.into_iter().collect();
    let options: Vec<String> = links
        .iter()
        .filter(|(_, _, area)| area == "main" || area == "nav")
        .map(|(t, _, _)| t.split_whitespace().take(4).collect::<Vec<_>>().join(" "))
        .filter(|t| {
            let n = t.split_whitespace().count();
            let l = t.to_lowercase();
            (1..=4).contains(&n)
                && t.chars().any(char::is_alphabetic)
                && !t.chars().any(|c| c.is_ascii_digit())
                && !title.contains(&l)
                && !site_words(site).iter().any(|w| l.contains(w.as_str()))
        })
        .take(200)
        .collect();
    if let Ok(Some((i, _))) = jev
        .choose_best(
            &format!("Links on the home page of {site} (“{}”)", s(&home_links, "title")),
            "Which of these names a kind of product sold on this site, something a shopper might search for?",
            &options,
        )
        .await
    {
        queries.push(options[i].clone());
    }
    for o in &options {
        if queries.len() >= 4 {
            break;
        }
        if !queries.contains(o) {
            queries.push(o.clone());
        }
    }
    if queries.is_empty() {
        queries.push("gift".into());
    }
    let mut results: Vec<Result_> = Vec::new();
    let mut tried = 0;
    'search: for t in candidates.iter().take(5) {
        for query in &queries {
            if tried >= 10 {
                break 'search;
            }
            tried += 1;
        let url = t.replace("{q}", &url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>());
        if page.navigate(&url).await.is_err() {
            continue 'search;
        }
        let found = results_of(&page.eval(json!({"mode": "items", "max_items": 30})).await.unwrap_or_default());
        if found.len() < 2 {
            continue;
        }
        let p = jev
            .yes(
                &format!("A page on {site} titled “{}”. It lists:\n{}", page.url().await.unwrap_or_default(), results_text(&found, 8)),
                &format!("Is this a page of search results for “{query}”?"),
            )
            .await
            .unwrap_or(0.0);
        if p >= 0.5 {
            say(&mut log, format!("Search: {t} gives results for “{query}” (Jev {p:.2})"));
            skills.search_url = Some(t.clone());
            skills.sample_query = query.clone();
            results = found;
            break 'search;
        }
        say(&mut log, format!("Search: {t} didn't look like results for “{query}” (Jev {p:.2})"));
        continue 'search;
        }
    }
    if skills.search_url.is_none() {
        say(&mut log, "Search: couldn't find a way to search this site".into());
    }

    // Results and product pages.
    if let Some(pattern) = result_pattern(&results) {
        say(&mut log, format!("Results: links shaped like {pattern}"));
        skills.result_pattern = Some(pattern.clone());
        let first = results.iter().find(|r| url_pattern(&r.url) == pattern).cloned();
        if let Some(r) = first {
            if page.navigate(&r.url).await.is_ok() {
                let v = page.eval(json!({"mode": "summary", "max_specs": 10, "excerpt_chars": 1500})).await.unwrap_or_default();
                let product = &v["product"];
                skills.product_data = !s(product, "price").is_empty() || !s(product, "availability").is_empty();
                let p = jev
                    .yes(
                        &format!("Title: {}\nHeading: {}\n{}", s(&v, "title"), s(&v, "heading"), s(&v, "excerpt")),
                        "Is this the page for a single product that can be bought?",
                    )
                    .await
                    .unwrap_or(0.0);
                let at = page.url().await.unwrap_or(r.url.clone());
                if p >= 0.5 {
                    let pp = url_pattern(&at);
                    say(
                        &mut log,
                        format!(
                            "Products: pages like {pp} (Jev {p:.2}){}",
                            if skills.product_data { ", with price and stock data" } else { "" }
                        ),
                    );
                    skills.product_pattern = Some(pp);
                } else {
                    say(&mut log, format!("Products: the first result didn't look like a product page (Jev {p:.2})"));
                }
            }
        }
    } else if skills.search_url.is_some() {
        say(&mut log, "Results: couldn't tell which links are results".into());
    }

    // Cart.
    let cart_options: Vec<(String, String)> = links
        .iter()
        .filter(|(t, u, area)| {
            let both = format!("{t} {u}").to_lowercase();
            area == "nav" || ["cart", "bag", "basket"].iter().any(|w| both.contains(w))
        })
        .map(|(t, u, _)| (format!("{t} → {u}"), u.clone()))
        .take(200)
        .collect();
    let names: Vec<String> = cart_options.iter().map(|(n, _)| n.clone()).collect();
    match jev.choose_one(&format!("Links at the top of {site}"), "Which link opens the shopping cart (bag, basket)?", &names).await {
        Ok(Some((i, conf))) if conf >= 0.6 => {
            say(&mut log, format!("Cart: {} (Jev {conf:.2})", cart_options[i].1));
            skills.cart_url = Some(cart_options[i].1.clone());
        }
        _ => say(&mut log, "Cart: no cart link found".into()),
    }

    say(&mut log, format!("Done: {} Jev calls, {} tokens read", jev.calls(), jev.input_tokens()));
    if skills.search_url.is_none() && skills.cart_url.is_none() {
        return Err(log.join("\n"));
    }
    Ok((skills, log))
}

// ------------------------------------------------------------ requests

/// What became of a request.
#[derive(Debug)]
pub enum Handled {
    /// Done without Claude; the reply to show.
    Answer { text: String },
    /// Not something Jev does (or it wasn't sure); hand it to Claude.
    NotForJev(String),
}

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "on", "at", "in", "for", "of", "to", "me", "my", "find", "search", "look", "looking", "show", "get",
    "open", "what", "what's", "whats", "is", "are", "how", "much", "price", "prices", "cost", "costs", "does", "do", "it",
    "this", "that", "whether", "if", "any", "some", "please", "can", "you", "could", "tell", "check", "stock", "available",
    "availability", "cart", "and", "with", "from", "about", "up", "there", "they", "have", "has", "i", "want", "need",
    "go", "pull", "see", "list", "page", "site", "one", "ones", "be", "by", "or", "still", "now", "currently", "would",
    "like", "should",
];

/// The phrases in a request that could name what it's about, longest first
/// within each start: "find a tusq xl nut on sweetwater" → "tusq xl nut",
/// "tusq xl", "tusq", "xl nut", … Site words are left out.
fn candidate_spans(text: &str, site_words: &[String]) -> Vec<String> {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '"' && c != '\'').trim_matches(['"', '\'']).to_string())
        .filter(|w| !w.is_empty())
        .collect();
    let skip = |w: &str| {
        let l = w.to_lowercase();
        STOPWORDS.contains(&l.as_str()) || site_words.iter().any(|s| l == *s || l.starts_with(&format!("{s}.")))
    };
    let mut out: Vec<String> = Vec::new();
    for i in 0..words.len() {
        if skip(&words[i]) {
            continue;
        }
        for len in (1..=7).rev() {
            if i + len > words.len() {
                continue;
            }
            let span = &words[i..i + len];
            if skip(span.last().unwrap()) || span.iter().any(|w| site_words.iter().any(|s| w.to_lowercase() == *s)) {
                continue;
            }
            let joined = span.join(" ");
            if !out.contains(&joined) {
                out.push(joined);
            }
        }
    }
    out.truncate(250);
    out
}

/// The words of a site's name: "www.sweetwater.com" → ["sweetwater"].
pub fn site_words(site: &str) -> Vec<String> {
    let s = site.trim_start_matches("www.").to_lowercase();
    let stem = s.rsplit_once('.').map(|(a, _)| a).unwrap_or(&s);
    stem.split(['.', '-']).filter(|w| w.len() > 2).map(str::to_string).collect()
}

const INTENTS: &[(&str, &str)] = &[
    ("find", "Search for or list products or items on the site"),
    ("price", "Asks the price or cost of a product"),
    ("stock", "Asks whether a product is in stock or available to buy"),
    ("open", "Open or show one specific product page"),
    ("cart", "Show or open the shopping cart"),
    (
        "question",
        "A factual question about one product that its page would answer: what's included, size, material, \
         compatibility, shipping, returns, warranty",
    ),
    (
        "other",
        "Anything else: buying, adding to cart, checking out, comparing or recommending products, opinions, summaries, \
         orders, the account, or several things at once",
    ),
];

fn money(product: &Value) -> String {
    let price = s(product, "price");
    if price.is_empty() {
        return String::new();
    }
    match s(product, "currency") {
        "USD" | "" if !price.starts_with('$') => format!("${price}"),
        "" | "USD" => price.to_string(),
        c => format!("{c} {price}"),
    }
}

/// "InStock" / "https://schema.org/OutOfStock" → words.
fn stock_words(availability: &str) -> Option<&'static str> {
    let a = availability.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    Some(match a.as_str() {
        "instock" | "in stock" | "limitedavailability" | "onlineonly" => "in stock",
        "outofstock" | "out of stock" | "soldout" | "discontinued" => "out of stock",
        "preorder" | "presale" => "available to pre-order",
        "backorder" => "on backorder",
        _ => return None,
    })
}

/// Handles `text`, a request about `site`, in the user's tab.
pub async fn handle(bridge: &BrowserBridge, jev: &Jev, site: &JevSite, text: &str) -> Result<Handled, String> {
    let b = Browser::new(bridge.clone());
    let abilities = site.skills.abilities();
    let spans = candidate_spans(text, &site_words(&site.site));
    let mut questions = json!({
        "intent": {
            "type": "choice",
            "instructions": "What does the user want done on the site?",
            "criteria": INTENTS.iter().map(|(k, d)| (k.to_string(), Value::String(d.to_string()))).collect::<serde_json::Map<_, _>>(),
        },
        "here": {"type": "noul", "instructions": "Does the request refer to the product already open on the page (this, it, this one)?"},
    });
    if !spans.is_empty() {
        let mut criteria: serde_json::Map<String, Value> =
            spans.iter().enumerate().map(|(i, s)| (i.to_string(), Value::String(s.clone()))).collect();
        criteria.insert("none".into(), Value::String("No product or item is named".into()));
        questions["item"] = json!({
            "type": "choice",
            "instructions": "Which phrase is the full name of the product or item the user wants (as much of the name as they gave)?",
            "criteria": criteria,
        });
    }
    let answers = jev.ask(json!(format!("A request to an assistant, about the website {}:\n{text}", site.site)), questions).await?;
    let intent = s(&answers["intent"], "choice").to_string();
    let intent_conf = answers["intent"]["probabilities"][intent.as_str()]
        .as_f64()
        .or(answers["intent"]["confidence"].as_f64())
        .unwrap_or(0.0);
    if intent == "other" || intent.is_empty() {
        return Ok(Handled::NotForJev("this isn't a find, price, stock, open, cart or product question request".into()));
    }
    if intent_conf < 0.6 {
        return Ok(Handled::NotForJev(format!("Jev wasn't sure what you meant (confidence {intent_conf:.2})")));
    }
    let item = s(&answers["item"], "choice").parse::<usize>().ok().and_then(|i| spans.get(i).cloned());
    let here = answers["here"]["noul"].as_f64().unwrap_or(0.0) >= 0.6;
    // A question about the page that's open needs nothing learned.
    let ability = if intent == "question" { "open" } else { intent.as_str() };
    if !abilities.contains(&ability) && !(intent == "question" && here) {
        return Ok(Handled::NotForJev(format!("Jev hasn't learned how to do “{intent}” on {}", site.site)));
    }

    let go = |url: String| async move {
        b.bridge.navigate(&url).await?;
        b.settle(Duration::from_secs(15)).await;
        Ok::<(), String>(())
    };
    let name_of = &site.site;

    if intent == "cart" {
        go(site.skills.cart_url.clone().unwrap_or_default()).await?;
        return Ok(Handled::Answer { text: format!("Opened your cart on {name_of}.") });
    }

    let on_product_page = {
        let st = bridge.state().await?;
        site.skills.product_pattern.as_deref().is_some_and(|p| url_pattern(&st.url) == p)
    };
    if intent == "question" && here {
        return answer_from_page(bridge, jev, text, name_of).await;
    }
    let query = match (&item, here && on_product_page) {
        (_, true) => None,
        (Some(q), false) => Some(q.clone()),
        (None, false) => return Ok(Handled::NotForJev("Jev couldn't tell which product you mean".into())),
    };

    if intent == "find" {
        let q = query.clone().or(item.clone()).ok_or("no search words")?;
        let url = site.skills.search_for(&q).ok_or("no search")?;
        go(url).await?;
        let results = results_of(&run_js(bridge, None, json!({"mode": "items", "max_items": 20})).await.unwrap_or_default());
        if results.is_empty() {
            return Ok(Handled::Answer { text: format!("Searched {name_of} for “{q}”. The results are on the page.") });
        }
        return Ok(Handled::Answer {
            text: format!("Top results on {name_of} for “{q}”:\n{}", results_text(&results, 5)),
        });
    }

    // open / price / stock: get to the product page.
    let mut listed_price = String::new();
    if let Some(q) = &query {
        // Model numbers anywhere in the request ("PT-1214-00"), even if
        // they're not in the words picked out as the product.
        let codes = model_numbers(text);
        let has_codes = |r: &Result_| codes.iter().all(|c| model_numbers(&r.title).contains(c));
        // The words as given; then, until a result has the model number,
        // the model number alone and the name without it.
        let mut tries = vec![q.clone()];
        if !codes.is_empty() {
            tries.push(codes.join(" "));
            let rest: Vec<&str> = q.split_whitespace().filter(|w| !model_numbers(w).iter().any(|c| codes.contains(c))).collect();
            if rest.len() >= 2 {
                tries.push(rest.join(" "));
            }
        }
        let (mut search, mut results) = (String::new(), Vec::new());
        for t in &tries {
            let url = site.skills.search_for(t).ok_or("no search")?;
            let page = BgPage::open(bridge, &url).await?;
            let found = results_of(&page.eval(json!({"mode": "items", "max_items": 30})).await.unwrap_or_default());
            if found.is_empty() {
                continue;
            }
            let done = codes.is_empty() || found.iter().any(&has_codes);
            if results.is_empty() || done {
                (search, results) = (url, found);
            }
            if done {
                break;
            }
        }
        if search.is_empty() {
            search = site.skills.search_for(q).ok_or("no search")?;
        }
        let options: Vec<String> = results.iter().map(|r| if r.price.is_empty() { r.title.clone() } else { format!("{} — {}", r.title, r.price) }).collect();
        // A model number the user typed settles it when one result has it.
        let coded: Vec<usize> = (0..results.len()).filter(|&i| !codes.is_empty() && has_codes(&results[i])).collect();
        let (pick, p) = if let [only] = coded[..] {
            (Some(only), 1.0)
        } else {
            jev.pick(
                &format!("Search results on {name_of} for “{q}”"),
                &format!(
                    "Which result is the product the user means by “{q}”? Prefer the product itself over accessories or \
                     parts for it, unless they asked for an accessory."
                ),
                &options,
            )
            .await?
        };
        let i = match pick {
            Some(i) if p >= 0.5 => i,
            // Several fit about as well: show them, and the price if they share one.
            Some(_) => {
                go(search).await?;
                let top: Vec<&Result_> = results.iter().take(5).collect();
                let same_price = top.first().map(|r| r.price.clone()).filter(|p| !p.is_empty() && top.iter().all(|r| r.price == *p));
                let lead = match (intent.as_str(), same_price) {
                    ("price", Some(price)) => format!("Several products on {name_of} match “{q}”, all {price}:"),
                    _ => format!("Several products on {name_of} match “{q}”. Which one do you mean?"),
                };
                return Ok(Handled::Answer { text: format!("{lead}\n{}", results_text(&results, 5)) });
            }
            None => {
                go(search).await?;
                let text = if results.is_empty() {
                    format!("Searched {name_of} for “{q}” and found nothing.")
                } else {
                    format!("Couldn't find “{q}” on {name_of}. Here's what the search shows:\n{}", results_text(&results, 5))
                };
                return Ok(Handled::Answer { text });
            }
        };
        listed_price = results[i].price.clone();
        go(results[i].url.clone()).await?;
    }
    if intent == "question" {
        return answer_from_page(bridge, jev, text, name_of).await;
    }
    let v = run_js(bridge, None, json!({"mode": "summary", "max_specs": 10, "excerpt_chars": 2500})).await?;
    let product = &v["product"];
    let name = [s(product, "name"), s(&v, "heading"), s(&v, "title")].into_iter().find(|n| !n.is_empty()).unwrap_or("This product").to_string();
    let mut price = money(product);
    if price.is_empty() {
        price = listed_price;
    }
    let stock = match stock_words(s(product, "availability")) {
        Some(w) => Some(w.to_string()),
        None if intent == "stock" => {
            let p = jev
                .yes(
                    &format!("Product page: {name}\n{}", s(&v, "excerpt")),
                    "Can this product be bought now (in stock, not sold out or unavailable)?",
                )
                .await?;
            Some(if p >= 0.7 { "in stock".into() } else if p <= 0.3 { "out of stock".into() } else { "unclear whether it's in stock".into() })
        }
        None => None,
    };
    let text = match intent.as_str() {
        "price" if price.is_empty() => format!("Opened {name} on {name_of}, but the page doesn't show a price I can read."),
        "price" => format!(
            "{name} is {price} on {name_of}{}.",
            stock.as_deref().map(|s| format!(", {s}")).unwrap_or_default()
        ),
        "stock" => format!(
            "{name} is {} on {name_of}{}.",
            stock.as_deref().unwrap_or("unclear"),
            if price.is_empty() { String::new() } else { format!(" ({price})") }
        ),
        _ => format!("Opened {name} on {name_of}{}.", if price.is_empty() { String::new() } else { format!(" — {price}") }),
    };
    Ok(Handled::Answer { text })
}

/// Model and part numbers in `text`: words with both letters and digits,
/// or digits joined by dashes ("PT-1214-00", "6643-00", "SM58"), in
/// upper case.
fn model_numbers(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | '/' | '“' | '”' | '"'))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_uppercase())
        .filter(|w| {
            let digits = w.chars().filter(char::is_ascii_digit).count();
            let letters = w.chars().filter(char::is_ascii_alphabetic).count();
            w.len() >= 4 && digits >= 2 && (letters > 0 || w.contains('-')) && !w.ends_with("MM")
        })
        .collect()
}

/// The page's visible text, up to about 300K tokens.
const PAGE_TEXT_JS: &str = r#"(() => ({
  title: document.title || '',
  text: (document.body ? document.body.innerText : '').slice(0, 1200000),
}))()"#;

/// Splits page text into passages of up to `PASSAGE_CHARS` for quoting:
/// short lines (headings, table rows) are joined to the next line or two,
/// and long paragraphs are split between sentences.
const PASSAGE_CHARS: usize = 300;

fn passages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let t = cur.trim();
        if t.chars().count() >= 3 {
            out.push(t.to_string());
        }
        cur.clear();
    };
    for line in text.lines() {
        let line = line.split('\t').map(str::trim).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(" · ");
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            continue;
        }
        let mut pieces = Vec::new();
        if line.chars().count() <= PASSAGE_CHARS {
            pieces.push(line);
        } else {
            // Between sentences, or between words for a sentence that's too long.
            let mut piece = String::new();
            for word in line.split(' ') {
                let ends = piece.ends_with(['.', '!', '?']);
                if !piece.is_empty()
                    && (piece.chars().count() + word.chars().count() >= PASSAGE_CHARS || (ends && piece.chars().count() > 180))
                {
                    pieces.push(std::mem::take(&mut piece));
                }
                if !piece.is_empty() {
                    piece.push(' ');
                }
                piece.push_str(word);
            }
            pieces.push(piece);
        }
        for p in pieces {
            if !cur.is_empty() && cur.chars().count() + p.chars().count() + 1 > PASSAGE_CHARS {
                flush(&mut cur, &mut out);
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(&p);
            // A paragraph, or a heading with a line or two after it.
            if cur.chars().count() >= 60 || cur.lines().count() >= 3 {
                flush(&mut cur, &mut out);
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// The passages most likely to answer `question`, up to the option limit,
/// in page order: those sharing the most words with it.
fn likely_passages(all: Vec<String>, question: &str, max: usize) -> Vec<String> {
    if all.len() <= max {
        return all;
    }
    let words: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(&w.to_lowercase().as_str()))
        .map(|w| w.to_lowercase())
        .collect();
    let score = |p: &str| {
        let p = p.to_lowercase();
        words.iter().filter(|w| p.contains(w.as_str())).count()
    };
    let mut ranked: Vec<(usize, usize)> = all.iter().enumerate().map(|(i, p)| (score(p), i)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut keep: Vec<usize> = ranked.into_iter().take(max).map(|(_, i)| i).collect();
    keep.sort_unstable();
    keep.into_iter().map(|i| all[i].clone()).collect()
}

/// Answers `question` from the page open in the user's tab, quoting the
/// passage that answers it; Claude gets it when the page doesn't say.
async fn answer_from_page(bridge: &BrowserBridge, jev: &Jev, question: &str, site: &str) -> Result<Handled, String> {
    let r = bridge.cdp("Runtime.evaluate", json!({"expression": PAGE_TEXT_JS, "returnByValue": true, "timeout": 10000})).await?;
    let v = &r["result"]["value"];
    let (title, text) = (s(v, "title"), s(v, "text"));
    let options = likely_passages(passages(text), question, 250);
    if options.is_empty() {
        return Ok(Handled::NotForJev("the page has no text to answer from".into()));
    }
    let mut criteria: serde_json::Map<String, Value> =
        options.iter().enumerate().map(|(i, p)| (i.to_string(), Value::String(p.clone()))).collect();
    criteria.insert("none".into(), Value::String("None of these passages answers the question".into()));
    let questions = json!({
        "passage": {
            "type": "choice",
            "instructions": "Which passage from the page answers the user's question? Only a passage that actually answers it.",
            "criteria": criteria,
        },
        "yes_no": {"type": "noul", "instructions": "Is the user's question a yes/no question?"},
        "yes": {"type": "noul", "instructions": "Going only by what the page says, is the answer to the user's question yes?"},
    });
    let state = format!("The user asked about this page: {question}\n\nThe page, “{title}” on {site}:\n{text}");
    let (a, _) = jev.ask_with(json!(state), questions).await?;
    let choice = s(&a["passage"], "choice");
    let p = a["passage"]["probabilities"][choice].as_f64().unwrap_or(0.0);
    // Several passages can answer it and split the vote; then it's enough
    // that the page answers it at all (little weight on "none").
    let none = a["passage"]["probabilities"]["none"].as_f64().unwrap_or(1.0);
    let sure = p >= 0.5 || (p >= 0.25 && none <= 0.15);
    let Some(passage) = choice.parse::<usize>().ok().and_then(|i| options.get(i)).filter(|_| sure) else {
        let best = if choice == "none" { "none of the page".to_string() } else { format!("one passage at {p:.2}") };
        return Ok(Handled::NotForJev(format!(
            "the page doesn't clearly answer that ({} passages read, best pick {best})",
            options.len()
        )));
    };
    // "Bone Jazz Bass nut – Unibrow Test Shop" → "Bone Jazz Bass nut".
    let name = [" – ", " | ", " - "].iter().fold(title, |t, sep| t.split(sep).next().unwrap_or(t)).trim();
    let lead = match (a["yes_no"]["noul"].as_f64().unwrap_or(0.0), a["yes"]["noul"].as_f64().unwrap_or(0.5)) {
        (q, y) if q >= 0.7 && y >= 0.8 => "Yes. ",
        (q, y) if q >= 0.7 && y <= 0.2 => "No. ",
        _ => "",
    };
    let quoted = passage.replace('\n', " / ");
    Ok(Handled::Answer { text: format!("{lead}The {} page on {site} says:\n“{quoted}”", if name.is_empty() { "product" } else { name }) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_skip_filler_and_the_site() {
        let spans = candidate_spans("Find a TUSQ XL jazz bass nut on Sweetwater", &site_words("www.sweetwater.com"));
        assert_eq!(spans[0], "TUSQ XL jazz bass nut");
        assert!(spans.contains(&"jazz bass nut".to_string()));
        assert!(!spans.iter().any(|s| s.to_lowercase().contains("sweetwater")));
        assert!(!spans.iter().any(|s| s.starts_with("a ") || s.ends_with(" on")));
        assert_eq!(site_words("guitar-center.com"), vec!["guitar", "center"]);
    }

    #[test]
    fn search_templates_from_history() {
        let (t, q) = template_from("https://www.sweetwater.com/store/search?s=tusq+nut&sb=popular").unwrap();
        assert_eq!((t.as_str(), q.as_str()), ("https://www.sweetwater.com/store/search?s={q}", "tusq nut"));
        assert!(template_from("https://x.com/item/123").is_none());
    }

    #[test]
    fn results_share_a_pattern() {
        let r = |u: &str| Result_ { title: "x".into(), url: u.into(), price: String::new() };
        let results = vec![
            r("https://s.com/store/detail/TusqNut--graph-tech-tusq"),
            r("https://s.com/store/detail/BoneNut--bone-nut-thing"),
            r("https://s.com/help"),
        ];
        assert_eq!(result_pattern(&results).as_deref(), Some(url_pattern("https://s.com/store/detail/TusqNut--graph-tech-tusq").as_str()));
    }

    #[test]
    fn model_numbers_pick_the_exact_product() {
        assert_eq!(model_numbers("the Graph Tech PT-1214-00 TUSQ XL jazz bass nut"), vec!["PT-1214-00"]);
        assert!(model_numbers("GRAPH TECH PQL-1214-00 TUSQ XL").iter().all(|c| c != "PT-1214-00"));
        assert_eq!(model_numbers("Shure SM58 mic, 43mm"), vec!["SM58"]);
        assert!(model_numbers("a 1962 Jazz Bass").is_empty());
    }

    #[test]
    fn page_text_becomes_passages() {
        let text = "Specifications\nMaterial\tUnbleached bone\nWidth\t38.0 mm\n\nQuestions\n".to_string()
            + &"A very long answer sentence about the nut and its slots. ".repeat(12);
        let p = passages(&text);
        assert!(p[0].starts_with("Specifications\nMaterial · Unbleached bone\nWidth · 38.0 mm"), "{p:?}");
        assert!(p.iter().all(|x| x.chars().count() <= PASSAGE_CHARS), "{p:?}");
        let many: Vec<String> = (0..400).map(|i| format!("Review {i}: nice tone")).chain(["It comes with glue".into()]).collect();
        let keep = likely_passages(many, "Does it come with glue?", 250);
        assert_eq!(keep.len(), 250);
        assert!(keep.contains(&"It comes with glue".to_string()));
    }

    #[test]
    fn stock_and_price_words() {
        assert_eq!(stock_words("https://schema.org/InStock"), Some("in stock"));
        assert_eq!(stock_words("OutOfStock"), Some("out of stock"));
        assert_eq!(stock_words(""), None);
        assert_eq!(money(&json!({"price": "9.99", "currency": "USD"})), "$9.99");
        assert_eq!(money(&json!({"price": "9.99", "currency": "EUR"})), "EUR 9.99");
        assert_eq!(money(&json!({})), "");
    }
}
