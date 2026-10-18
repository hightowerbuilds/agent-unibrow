//! Site scope (roadmap 6.3): with "Only act on focus sites" on, the agent
//! may read and navigate anywhere, but clicking, typing, pressing keys,
//! choosing options, answering dialogs, uploading or signing in on a site
//! that isn't in focus waits for the user's approval.
//!
//! A focus site covers its subdomains: `amazon.com` covers
//! `smile.amazon.com`. Approving with "Always allow on this site" adds the
//! site to the scope for later calls.

/// Pseudo tool name under which sites allowed outside focus are kept in
/// the always-allow list.
pub const SCOPE_KEY: &str = "site_scope";

/// The host a site key compares by: lowercase, no `www.`, no port.
pub fn normalize_site(site: &str) -> String {
    let s = site.trim().to_ascii_lowercase();
    let s = s.split("://").last().unwrap_or(&s).to_string();
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let host = match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Whether `site` is one of `focus` or a subdomain of one.
pub fn in_focus(site: &str, focus: &[String]) -> bool {
    let site = normalize_site(site);
    if site.is_empty() {
        return false;
    }
    focus.iter().map(|f| normalize_site(f)).filter(|f| !f.is_empty()).any(|f| {
        site == f || site.ends_with(&format!(".{f}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let focus = vec!["amazon.com".to_string(), "www.Sweetwater.com".to_string()];
        assert!(in_focus("amazon.com", &focus));
        assert!(in_focus("www.amazon.com", &focus));
        assert!(in_focus("smile.amazon.com", &focus));
        assert!(in_focus("sweetwater.com", &focus));
        assert!(!in_focus("notamazon.com", &focus));
        assert!(!in_focus("amazon.com.evil.net", &focus));
        assert!(!in_focus("", &focus));
        assert!(!in_focus("example.com", &[]));
        // Ports don't matter, the host does.
        assert!(in_focus("localhost:8080", &["localhost".into()]));
        assert!(!in_focus("127.0.0.1", &["localhost".into()]));
    }
}
