//! Which site a browser action acts on, for the user's site scope (6.3).
//! The check itself is the policy's, applied by the tool host.

use super::Browser;
use futures::future::BoxFuture;
use std::sync::Arc;

/// The site of the user's current page (host without `www.`), or `None`
/// for internal pages.
pub(super) fn current_site(b: &Arc<Browser>) -> BoxFuture<'static, Option<String>> {
    let b = b.clone();
    Box::pin(async move {
        let st = b.state().await.ok()?;
        let u = url::Url::parse(&st.url).ok()?;
        if !matches!(u.scheme(), "http" | "https") {
            return None;
        }
        let host = u.host_str()?.to_ascii_lowercase();
        Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
    })
}
