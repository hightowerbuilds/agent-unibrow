//! Spellcheck in the page's text fields: experimental, off unless
//! `UNIBROW_SPELLCHECK` is set (`en-US`, or `en-US,de-DE`, …).
//!
//! What was found with CEF 154 on Linux: setting the profile's
//! `browser.enable_spellchecking` and `spellcheck.dictionaries` prefs works
//! (they're saved), and Chromium fetches the `.bdic` dictionary from
//! Google's dictionary server into the profile's `Dictionaries/` folder.
//! But our pages (windowless, Alloy-style browsers) never get spelling
//! markers: typed or existing misspellings aren't underlined, and the
//! context menu's `misspelled_word` and suggestions stay empty, on a fresh
//! page and after a restart with the dictionary in place. So it stays off
//! by default. (Chromium's defaults are already spellcheck on with en-US,
//! and it downloads that dictionary in every profile, with or without
//! this; the variable only picks other languages.) The context menu
//! already shows suggestions and "Add to dictionary", and replaces the
//! word, if Chromium ever reports one (see
//! [`crate::ContextMenuInfo::misspelled_word`]).

use cef::{CefString, ImplBrowser, ImplBrowserHost, ImplListValue, ImplPreferenceManager, ImplValue};
use std::cell::Cell;

/// The dictionaries to use; `None`: spellcheck isn't turned on.
pub fn languages() -> Option<Vec<String>> {
    let langs: Vec<String> = std::env::var("UNIBROW_SPELLCHECK")
        .ok()?
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Some(if langs.is_empty() { vec!["en-US".to_string()] } else { langs })
}

fn debug(msg: impl FnOnce() -> String) {
    if std::env::var_os("UNIBROW_DEBUG_PANE").is_some() {
        eprintln!("spelling: {}", msg());
    }
}

/// Turns spellcheck on in the page's profile. Like `harden_profile`, needs
/// an initialized request context, so each load tries until it has worked.
pub(crate) fn enable(browser: &cef::Browser) {
    thread_local! { static DONE: Cell<bool> = const { Cell::new(false) }; }
    if DONE.with(|d| d.get()) {
        return;
    }
    let Some(langs) = languages() else {
        DONE.with(|d| d.set(true));
        return;
    };
    let Some(ctx) = browser.host().and_then(|h| h.request_context()) else { return };
    let set = |name: &str, value: &mut cef::Value| {
        let mut error = CefString::from("-");
        let ok = ctx.set_preference(Some(&CefString::from(name)), Some(value), Some(&mut error)) == 1;
        debug(|| format!("set {name}: {ok} ({error})"));
        ok
    };
    let (Some(mut on), Some(mut dicts), Some(list)) = (cef::value_create(), cef::value_create(), cef::list_value_create())
    else {
        return;
    };
    on.set_bool(1);
    list.set_size(langs.len());
    for (i, l) in langs.iter().enumerate() {
        list.set_string(i, Some(&CefString::from(l.as_str())));
    }
    let mut list = list;
    dicts.set_list(Some(&mut list));
    let ok = set("browser.enable_spellchecking", &mut on) && set("spellcheck.dictionaries", &mut dicts);
    if ok {
        DONE.with(|d| d.set(true));
    }
}

/// Replaces the misspelled word the context menu was opened on.
pub(crate) fn replace(browser: &cef::Browser, word: &str) {
    if let Some(h) = browser.host() {
        h.replace_misspelling(Some(&CefString::from(word)));
    }
}

/// Adds `word` to the profile's own dictionary, so it isn't flagged again.
pub(crate) fn add_to_dictionary(browser: &cef::Browser, word: &str) {
    if let Some(h) = browser.host() {
        h.add_word_to_dictionary(Some(&CefString::from(word)));
    }
}
