//! Saved logins (roadmap 5.1).
//!
//! Passwords live in the desktop keyring (GNOME Keyring here), one entry per
//! login. A small index file lists which sites and usernames exist, with no
//! secrets. The agent can ask the app to fill a login, but never receives
//! the password: `browser_sign_in` types it into the page itself.
//!
//! A login only fills on the site it was saved for (or a subdomain of it),
//! so a look-alike phishing domain gets nothing.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "agent-unibrow-logins";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Login {
    /// Site as shown in history and focus (`example.com`).
    pub site: String,
    pub username: String,
    /// Unix seconds.
    pub added: i64,
}

fn index_path() -> PathBuf {
    crate::data_dir().join("logins.json")
}

fn keyring_user(site: &str, username: &str) -> String {
    format!("{site}\u{1f}{username}")
}

fn entry(site: &str, username: &str) -> Result<keyring::Entry, String> {
    // Test instances keep their logins apart from the real ones.
    let service = match std::env::var("UNIBROW_DATA_DIR") {
        Ok(_) => format!("{KEYRING_SERVICE}-test"),
        Err(_) => KEYRING_SERVICE.to_string(),
    };
    keyring::Entry::new(&service, &keyring_user(site, username)).map_err(|e| format!("keyring unavailable: {e}"))
}

/// Every saved login (no passwords).
pub fn list() -> Vec<Login> {
    std::fs::read_to_string(index_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn write_index(logins: &[Login]) -> Result<(), String> {
    let path = index_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(logins).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("couldn't save the login list: {e}"))
}

/// Normalizes what the user typed as a site: `https://www.Example.com/login` → `example.com`.
pub fn normalize_site(input: &str) -> String {
    let t = input.trim();
    let host = url::Url::parse(t)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| t.split('/').next().unwrap_or(t).to_string());
    let host = host.to_ascii_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

pub fn save(site: &str, username: &str, password: &str) -> Result<(), String> {
    let site = normalize_site(site);
    let username = username.trim().to_string();
    if site.is_empty() || username.is_empty() || password.is_empty() {
        return Err("Site, username and password are all needed.".into());
    }
    entry(&site, &username)?.set_password(password).map_err(|e| format!("couldn't save the password: {e}"))?;
    let mut all = list();
    all.retain(|l| !(l.site == site && l.username == username));
    all.push(Login { site, username, added: chrono_now() });
    write_index(&all)
}

pub fn remove(site: &str, username: &str) -> Result<(), String> {
    let _ = entry(site, username).and_then(|e| e.delete_credential().map_err(|e| e.to_string()));
    let mut all = list();
    all.retain(|l| !(l.site == site && l.username == username));
    write_index(&all)
}

/// Logins that may fill on a page whose host is `host`.
pub fn for_host(host: &str) -> Vec<Login> {
    let host = host.trim_start_matches("www.").to_ascii_lowercase();
    list().into_iter().filter(|l| host == l.site || host.ends_with(&format!(".{}", l.site))).collect()
}

/// The password, for the app's own use only (never returned to a model).
pub(crate) fn password(login: &Login) -> Result<String, String> {
    entry(&login.site, &login.username)?.get_password().map_err(|_| {
        format!("The password for {} on {} is missing from the keyring.", login.username, login.site)
    })
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_normalize() {
        assert_eq!(normalize_site("https://www.Sweetwater.com/login?x=1"), "sweetwater.com");
        assert_eq!(normalize_site("accounts.example.com/signin"), "accounts.example.com");
        assert_eq!(normalize_site("  example.com "), "example.com");
    }
}
