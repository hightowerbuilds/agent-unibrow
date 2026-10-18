//! The API rail: the user's own data behind OAuth (Gmail now; Calendar,
//! Microsoft Graph and Plaid later). Nothing here touches a web page.

pub mod codes;
pub mod gmail;
mod google_auth;
mod mime;

use router::{Rail, Tool};
use std::sync::Arc;

pub use google_auth::{GoogleAuth, client_secret_path};

pub struct ApiRail {
    gmail: Option<Arc<gmail::Gmail>>,
}

impl ApiRail {
    /// Builds the rail from whatever is configured. A missing Google client
    /// file just means no Gmail tools.
    pub fn from_config() -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let gmail = match GoogleAuth::from_client_file() {
            Ok(auth) => {
                notes.push(format!(
                    "Gmail tools enabled ({})",
                    if auth.has_refresh_token() { "signed in" } else { "will ask you to sign in on first use" }
                ));
                Some(Arc::new(gmail::Gmail::new(auth)))
            }
            Err(e) => {
                notes.push(format!("Gmail tools off: {e}"));
                None
            }
        };
        (Self { gmail }, notes)
    }
}

impl ApiRail {
    /// Where verification codes can be found: the user's Gmail, if set up.
    pub fn code_source(&self) -> Option<Arc<dyn router::CodeSource>> {
        self.gmail.clone().map(|g| Arc::new(codes::GmailCodes(g)) as Arc<dyn router::CodeSource>)
    }
}

impl Rail for ApiRail {
    fn name(&self) -> &'static str {
        "api"
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(gmail::CurrentTime)];
        if let Some(g) = &self.gmail {
            tools.extend(gmail::tools(g.clone()));
        }
        tools
    }
}
