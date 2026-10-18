//! Reader mode: the page's article (the same extraction `browser_extract`
//! uses, mode `article`) rebuilt as a plain, readable page.
//!
//! The reader page is a `data:` URL with no scripts and nothing loaded from
//! the network, so it shows only the extracted text.

use base64::Engine as _;
use serde_json::Value;

const EXTRACT_JS: &str = include_str!("tools/extract.js");
/// Fewer words than this isn't an article worth a reader view.
const MIN_WORDS: u64 = 40;
/// How much of a long article the reader shows.
const MAX_CHARS: usize = 300_000;

/// The expression that extracts the article from the page.
pub fn article_expression() -> String {
    format!("({EXTRACT_JS})({})", serde_json::json!({"mode": "article", "max_chars": MAX_CHARS}))
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Builds the reader page from an `article` extraction result. `None` if
/// there's no real article (too few words).
pub fn reader_html(article: &Value) -> Option<String> {
    let text = article["text"].as_str().unwrap_or("");
    let words = article["words"].as_u64().unwrap_or_else(|| text.split_whitespace().count() as u64);
    if words < MIN_WORDS {
        return None;
    }
    let doc_title = article["title"].as_str().unwrap_or("").trim();
    // The article's own first heading names it better than the tab title
    // ("Story – Site name"); that heading isn't repeated in the body.
    let first_heading = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.strip_prefix('#'))
        .map(|h| h.trim_start_matches('#').trim().to_string())
        .filter(|h| !h.is_empty());
    let title = first_heading.as_deref().unwrap_or(doc_title);
    let url = article["url"].as_str().unwrap_or("");
    let site = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()));
    let mut byline: Vec<String> = Vec::new();
    if let Some(s) = site {
        byline.push(s);
    }
    for key in ["author", "published"] {
        if let Some(v) = article[key].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            let v = if key == "published" { v.split('T').next().unwrap_or(v) } else { v };
            byline.push(v.to_string());
        }
    }
    let minutes = (words as f64 / 230.0).ceil().max(1.0) as u64;
    byline.push(format!("{minutes} min read"));

    let mut body = String::new();
    let mut first_heading_skipped = false;
    let mut lines = text.lines().peekable();
    let mut in_list = false;
    let mut paragraphs = 0;
    while let Some(line) = lines.next() {
        let line = line.trim_end();
        let is_item = line.starts_with("- ");
        if in_list && !is_item {
            body.push_str("</ul>\n");
            in_list = false;
        }
        if line.starts_with("```") {
            let mut code = String::new();
            for l in lines.by_ref() {
                if l.trim_end() == "```" {
                    break;
                }
                code.push_str(l);
                code.push('\n');
            }
            body.push_str(&format!("<pre>{}</pre>\n", escape(code.trim_end())));
        } else if let Some(rest) = line.strip_prefix('#') {
            let level = 1 + rest.chars().take_while(|c| *c == '#').count();
            let heading = rest.trim_start_matches('#').trim();
            if !first_heading_skipped && heading.eq_ignore_ascii_case(title) {
                first_heading_skipped = true;
                continue;
            }
            let tag = level.clamp(2, 4);
            body.push_str(&format!("<h{tag}>{}</h{tag}>\n", escape(heading)));
        } else if let Some(rest) = line.strip_prefix("> ") {
            body.push_str(&format!("<blockquote>{}</blockquote>\n", escape(rest)));
        } else if let Some(rest) = line.strip_prefix("- ") {
            if !in_list {
                body.push_str("<ul>\n");
                in_list = true;
            }
            body.push_str(&format!("<li>{}</li>\n", escape(rest)));
        } else if let Some(alt) = line.strip_prefix("[image: ").and_then(|l| l.strip_suffix(']')) {
            body.push_str(&format!("<p class=\"img\">Image: {}</p>\n", escape(alt)));
        } else if !line.trim().is_empty() {
            // The page's own "By ..." line repeats the byline above.
            if paragraphs == 0 && line.starts_with("By ") && line.len() < 120 {
                paragraphs += 1;
                continue;
            }
            paragraphs += 1;
            body.push_str(&format!("<p>{}</p>\n", escape(line)));
        }
    }
    if in_list {
        body.push_str("</ul>\n");
    }
    if article["truncated"].as_bool().unwrap_or(false) {
        body.push_str("<p class=\"img\">(The article continues on the original page.)</p>\n");
    }
    let heading = if title.is_empty() { "Article" } else { title };
    Some(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{doc_title}</title>
<style>
:root {{ color-scheme: light dark; --fg: #1f2328; --muted: #6a737d; --bg: #fbfaf7; --rule: #e4e1da; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg: #e6e1d8; --muted: #9a958c; --bg: #1c1b19; --rule: #34322e; }} }}
html {{ background: var(--bg); }}
body {{ max-width: 42rem; margin: 0 auto; padding: 3rem 1.5rem 6rem; color: var(--fg);
  font: 19px/1.65 Georgia, "Iowan Old Style", "Noto Serif", serif; }}
header {{ border-bottom: 1px solid var(--rule); margin-bottom: 2rem; padding-bottom: 1rem; }}
h1 {{ font-size: 2rem; line-height: 1.2; margin: 0 0 .6rem; }}
.byline, .img {{ color: var(--muted); font: 14px/1.5 system-ui, sans-serif; }}
h2, h3, h4 {{ line-height: 1.3; margin: 2rem 0 .6rem; }}
p, ul {{ margin: 0 0 1.1rem; }}
blockquote {{ margin: 0 0 1.1rem; padding-left: 1rem; border-left: 3px solid var(--rule); color: var(--muted); }}
pre {{ overflow-x: auto; padding: .8rem 1rem; background: rgba(127,127,127,.12); border-radius: 6px; font-size: 15px; }}
</style></head>
<body><header><h1>{title}</h1><div class="byline">{byline}</div></header>
<main>
{body}</main></body></html>
"#,
        title = escape(heading),
        doc_title = escape(if doc_title.is_empty() { heading } else { doc_title }),
        byline = escape(&byline.join(" · ")),
        body = body,
    ))
}

/// A `data:` URL for a reader page.
pub fn data_url(html: &str) -> String {
    format!("data:text/html;charset=utf-8;base64,{}", base64::engine::general_purpose::STANDARD.encode(html))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_a_page_and_escapes_text() {
        let words = "word ".repeat(60);
        let art = json!({
            "url": "https://www.example.com/post",
            "title": "A <b>title</b> – Site",
            "author": "Ada",
            "published": "2024-03-01T10:00:00Z",
            "words": 70,
            "text": format!("## A <b>title</b>\nBy Ada · March 1\n{words}\n### Part two\n- one\n- two & three\n> quoted\n```\nlet x = 1 < 2;\n```\n[image: a cat]\n<script>alert(1)</script>"),
            "truncated": true,
        });
        let html = reader_html(&art).expect("an article");
        // The first heading becomes the page heading, not repeated below;
        // the tab keeps the document's title.
        assert!(html.contains("<h1>A &lt;b&gt;title&lt;/b&gt;</h1>"));
        assert!(html.contains("<title>A &lt;b&gt;title&lt;/b&gt; – Site</title>"));
        assert_eq!(html.matches("A &lt;b&gt;title").count(), 2);
        assert!(html.contains("example.com · Ada · 2024-03-01 · 1 min read"));
        assert!(html.contains("<h3>Part two</h3>"));
        assert!(html.contains("<ul>\n<li>one</li>\n<li>two &amp; three</li>\n</ul>"));
        assert!(html.contains("<blockquote>quoted</blockquote>"));
        assert!(html.contains("<pre>let x = 1 &lt; 2;</pre>"));
        assert!(html.contains("Image: a cat"));
        assert!(!html.contains("By Ada"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("continues on the original page"));
        assert!(data_url(&html).starts_with("data:text/html;charset=utf-8;base64,"));
    }

    #[test]
    fn short_pages_are_not_articles() {
        assert!(reader_html(&json!({"text": "Sign in", "words": 2})).is_none());
    }

    #[test]
    fn expression_runs_article_mode() {
        let e = article_expression();
        assert!(e.contains(r#""mode":"article""#));
        assert!(e.starts_with('(') && e.contains("(function (args)"));
    }
}
