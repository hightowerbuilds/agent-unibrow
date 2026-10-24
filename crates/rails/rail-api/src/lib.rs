//! The API rail: the user's own data behind OAuth (Gmail and Google
//! Calendar now; Microsoft Graph and Plaid later). Nothing here touches a
//! web page.

pub mod calendar;
pub mod codes;
pub mod gmail;
mod google_auth;
mod mime;

use router::{Rail, Tool};
use std::sync::Arc;

pub use google_auth::{GoogleAuth, client_secret_path, signed_in as google_signed_in};

pub struct ApiRail {
    gmail: Option<Arc<gmail::Gmail>>,
    calendar: Option<Arc<dyn calendar::Backend>>,
}

impl ApiRail {
    /// Builds the rail from whatever is configured. A missing Google client
    /// file just means no Gmail or Calendar tools.
    pub fn from_config() -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let (gmail, calendar) = match GoogleAuth::from_client_file() {
            Ok(auth) => {
                notes.push(format!(
                    "Gmail and Calendar tools enabled ({})",
                    if auth.has_refresh_token() { "signed in" } else { "will ask you to sign in on first use" }
                ));
                let auth = Arc::new(auth);
                let cal: Arc<dyn calendar::Backend> = Arc::new(calendar::google::GoogleCalendar::new(auth.clone()));
                (Some(Arc::new(gmail::Gmail::new(auth))), Some(cal))
            }
            Err(e) => {
                notes.push(format!("Gmail tools off: {e}"));
                (None, None)
            }
        };
        (Self { gmail, calendar }, notes)
    }

    /// A test instance's rail: the clock, and a calendar kept in a file
    /// (`calendar::file`). No Gmail.
    pub fn test_calendar(path: std::path::PathBuf) -> Self {
        Self { gmail: None, calendar: Some(Arc::new(calendar::file::FileCalendar::new(path))) }
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
        if let Some(c) = &self.calendar {
            tools.extend(calendar::tools(c.clone()));
        }
        tools
    }
}
