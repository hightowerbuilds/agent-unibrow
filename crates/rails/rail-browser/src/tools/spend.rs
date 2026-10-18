//! Spending guard (roadmap 6.6): when a click buys or pays for something,
//! find the amount so the approval card can show it and the user's
//! "never auto-approve purchases over $X" rule can apply.
//!
//! The amount comes from, in order: the button ("Pay $129.00"), an amount
//! field in the form, a "total" line on the page, or failing those the
//! largest amount the page shows (marked as a guess).

use super::{Browser, Probe, has_word};
use router::Purchase;
use serde_json::{Value, json};

/// Button words that mean money changes hands.
const PURCHASE_WORDS: &[&str] = &[
    "buy", "buy now", "purchase", "complete purchase", "pay", "pay now", "place order", "place your order",
    "complete order", "submit order", "confirm order", "order now", "confirm and pay", "checkout", "check out",
    "donate", "subscribe", "start subscription", "rent", "book now", "reserve", "transfer", "send money", "tip",
];

/// Visible short texts with an amount, and whether each mentions a total.
const AMOUNTS_JS: &str = r#"(() => {
  const money = /(?:[$€£¥₹]|\b(?:USD|EUR|GBP|CAD|AUD)\b)\s?\d[\d.,]*|\d[\d.,]*\s?(?:[€£]|\b(?:USD|EUR|GBP)\b)/i;
  const vis = e => { const r = e.getBoundingClientRect(); const s = getComputedStyle(e);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; };
  const out = [];
  for (const e of document.querySelectorAll('body *')) {
    if (out.length >= 60) break;
    if (e.children.length > 4 || ['SCRIPT','STYLE','NOSCRIPT','OPTION'].includes(e.tagName)) continue;
    const t = (e.innerText || '').replace(/\s+/g, ' ').trim();
    if (!t || t.length > 120 || !money.test(t) || !vis(e)) continue;
    out.push(t);
  }
  return JSON.stringify(out);
})()"#;

/// Whether clicking `label` (with this form) buys or pays for something.
pub(super) fn is_purchase(label: &str, probe: &Probe) -> bool {
    has_word(label, PURCHASE_WORDS)
        || probe.values.iter().any(|(l, _)| {
            let l = l.to_lowercase();
            l.contains("card number") || l.contains("cvv") || l.contains("cvc") || l.contains("expiry")
        })
}

/// The purchase this action makes, with the amount if one can be found.
pub(super) async fn detect(b: &Browser, label: &str, probe: &Probe) -> Purchase {
    if let Some((amount, text)) = amount_in(label) {
        return Purchase { amount: Some(amount), shown: Some(format!("{text} (on the button)")) };
    }
    if let Some((amount, text)) = probe
        .values
        .iter()
        .filter(|(l, _)| has_word(l, &["amount", "total", "donation", "price", "pay"]))
        .find_map(|(_, v)| amount_in(v).or_else(|| bare_number(v).map(|n| (n, v.clone()))))
    {
        return Purchase { amount: Some(amount), shown: Some(format!("{text} (in the form)")) };
    }
    let lines: Vec<String> = b
        .bridge
        .cdp("Runtime.evaluate", json!({"expression": AMOUNTS_JS, "returnByValue": true}))
        .await
        .ok()
        .and_then(|r| r["result"]["value"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()))
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()))
        .unwrap_or_default();
    pick_from_page(&lines)
}

/// The order total among visible page texts, or the largest amount.
pub(super) fn pick_from_page(lines: &[String]) -> Purchase {
    let rank = |t: &str| {
        let t = t.to_lowercase();
        if ["grand total", "order total", "total due", "amount due", "you pay", "total to pay", "total charge"]
            .iter()
            .any(|k| t.contains(k))
        {
            3
        } else if has_word(&t, &["total"]) && !t.contains("subtotal") {
            2
        } else {
            0
        }
    };
    let mut best: Option<(i32, f64, String)> = None;
    for line in lines {
        let r = rank(line);
        if r == 0 {
            continue;
        }
        if let Some((a, text)) = amount_in(line) {
            if best.as_ref().is_none_or(|(br, ba, _)| r > *br || (r == *br && a > *ba)) {
                best = Some((r, a, text));
            }
        }
    }
    if let Some((_, a, text)) = best {
        return Purchase { amount: Some(a), shown: Some(format!("{text} (order total on the page)")) };
    }
    let largest = lines
        .iter()
        .filter_map(|l| amount_in(l))
        .fold(None::<(f64, String)>, |acc, (a, t)| match acc {
            Some((x, _)) if x >= a => acc,
            _ => Some((a, t)),
        });
    match largest {
        Some((a, t)) => Purchase {
            amount: Some(a),
            shown: Some(format!("{t}? (no total found; this is the largest amount on the page)")),
        },
        None => Purchase::default(),
    }
}

/// The first money amount in `text`: (value, as written).
pub(super) fn amount_in(text: &str) -> Option<(f64, String)> {
    const SYMBOLS: &[&str] = &["$", "€", "£", "¥", "₹", "USD", "EUR", "GBP", "CAD", "AUD"];
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, &(pos, c)) in chars.iter().enumerate() {
        // Only at the start of a number.
        if !c.is_ascii_digit() || (i > 0 && chars[i - 1].1.is_ascii_digit()) {
            continue;
        }
        // A number counts if a currency sign is right before or after it.
        let before = text[..pos].trim_end();
        let num: String = chars[i..].iter().map(|&(_, c)| c).take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
        let after = text[pos + num.len()..].trim_start();
        let sym_before = SYMBOLS.iter().find(|s| before.ends_with(*s));
        let sym_after = SYMBOLS.iter().find(|s| after.starts_with(*s));
        if let Some(sym) = sym_before.or(sym_after) {
            let value = parse_number(&num)?;
            let num = num.trim_end_matches(['.', ',']);
            let gap = if sym.chars().all(|c| c.is_ascii_alphabetic()) { " " } else { "" };
            let shown = if sym_before.is_some() { format!("{sym}{gap}{num}") } else { format!("{num} {sym}") };
            return Some((value, shown));
        }
    }
    None
}

fn bare_number(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',') {
        return None;
    }
    parse_number(t)
}

/// "1,299.00" → 1299.0, "1.299,00" → 1299.0, "13,98" → 13.98.
fn parse_number(num: &str) -> Option<f64> {
    let num = num.trim_end_matches(['.', ',']);
    let normalized = match (num.rfind('.'), num.rfind(',')) {
        (Some(d), Some(c)) if c > d => num.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => num.replace(',', ""),
        (None, Some(c)) if num.len() - c - 1 == 2 => num.replace(',', "."),
        _ => num.replace(',', ""),
    };
    normalized.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(amount_in("Pay $129.00 now"), Some((129.0, "$129.00".into())));
        assert_eq!(amount_in("Total: 1.299,50 €"), Some((1299.5, "1.299,50 €".into())));
        assert_eq!(amount_in("Order #UB-2041: nut ×1 — $13.98"), Some((13.98, "$13.98".into())));
        assert_eq!(amount_in("USD 40"), Some((40.0, "USD 40".into())));
        assert_eq!(amount_in("2 items, ships in 3 days"), None);
        assert_eq!(amount_in("Place order"), None);
    }

    #[test]
    fn purchase_words() {
        let p = Probe::default();
        assert!(is_purchase("Click button \"Place order\"", &p));
        assert!(is_purchase("Pay now", &p));
        assert!(is_purchase("Buy", &p));
        assert!(!is_purchase("Send message", &p));
        assert!(!is_purchase("Delete draft", &p));
        let card = Probe { values: vec![("Card number".into(), "[hidden]".into())], ..Default::default() };
        assert!(is_purchase("Continue", &card));
    }

    #[test]
    fn page_total_beats_other_amounts() {
        let lines: Vec<String> = ["Subtotal $119.00", "Shipping $10.00", "Total $129.00", "Gift card $500.00"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = pick_from_page(&lines);
        assert_eq!(p.amount, Some(129.0));
        assert!(p.shown.unwrap().contains("order total"));
        let grand: Vec<String> = vec!["Total $5.00".into(), "Order total: $64.20".into()];
        assert_eq!(pick_from_page(&grand).amount, Some(64.2));
    }

    #[test]
    fn largest_amount_is_a_marked_guess() {
        let lines: Vec<String> = vec!["nut ×1 — $13.98".into(), "Gift wrap (+$2.00)".into()];
        let p = pick_from_page(&lines);
        assert_eq!(p.amount, Some(13.98));
        assert!(p.shown.unwrap().contains("largest amount"));
        assert_eq!(pick_from_page(&[]), Purchase::default());
    }
}
