//! Google OAuth for an installed desktop app: loopback redirect + PKCE.
//!
//! The client ID file comes from Google Cloud Console (OAuth client of type
//! "Desktop app"). The refresh token is stored in the OS secret service
//! (GNOME Keyring on this machine); access tokens live only in memory.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

const KEYRING_SERVICE: &str = "agent-unibrow";
const KEYRING_USER: &str = "google-refresh-token";
/// Read mail, and create/send drafts and messages; read and change calendar
/// events, and list the calendars. Nothing broader.
const SCOPES: &str = "https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.compose \
                      https://www.googleapis.com/auth/calendar.events \
                      https://www.googleapis.com/auth/calendar.calendarlist.readonly";
/// How long to wait for the user to finish the consent screen.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

pub fn client_secret_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("agent-unibrow").join("google_client.json"))
}

#[derive(Deserialize, Clone)]
struct InstalledClient {
    client_id: String,
    client_secret: String,
    #[serde(default = "default_auth_uri")]
    auth_uri: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_auth_uri() -> String {
    "https://accounts.google.com/o/oauth2/v2/auth".into()
}
fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

#[derive(Deserialize)]
struct ClientFile {
    installed: Option<InstalledClient>,
    web: Option<InstalledClient>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// Why a refresh didn't give an access token.
enum Refresh {
    /// Google no longer accepts the refresh token; only signing in again helps.
    Revoked,
    /// Anything else: no connection, a server error, a bad response.
    Failed(String),
}

/// Whether a failed refresh means the refresh token is no good (revoked,
/// expired, or for another client) rather than a passing problem.
fn refresh_token_revoked(status: u16, body: &str) -> bool {
    let error = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string));
    status == 401 || (status == 400 && error.as_deref().map_or(body.contains("invalid_grant"), |e| e == "invalid_grant"))
}

struct Cached {
    token: String,
    expires: Instant,
}

pub struct GoogleAuth {
    client: InstalledClient,
    http: reqwest::Client,
    access: Mutex<Option<Cached>>,
}

impl GoogleAuth {
    pub fn from_client_file() -> Result<Self, String> {
        let path = client_secret_path().ok_or("no config directory")?;
        let text = std::fs::read_to_string(&path)
            .map_err(|_| format!("no Google OAuth client at {}", path.display()))?;
        let file: ClientFile =
            serde_json::from_str(&text).map_err(|e| format!("{} is not a Google client file: {e}", path.display()))?;
        let client = file
            .installed
            .or(file.web)
            .ok_or_else(|| format!("{} has no \"installed\" client; create a Desktop app client", path.display()))?;
        Ok(Self { client, http: reqwest::Client::new(), access: Mutex::new(None) })
    }

    pub fn has_refresh_token(&self) -> bool {
        load_refresh_token().is_some()
    }

    /// A valid access token, refreshing or running the consent flow as needed.
    pub async fn access_token(&self) -> Result<String, String> {
        let mut cached = self.access.lock().await;
        if let Some(c) = cached.as_ref() {
            if c.expires > Instant::now() + Duration::from_secs(60) {
                return Ok(c.token.clone());
            }
        }
        let fresh = match tokio::task::spawn_blocking(load_refresh_token).await.ok().flatten() {
            Some(refresh) => match self.refresh(&refresh).await {
                Ok(t) => t,
                // Revoked or expired refresh token: sign in again.
                Err(Refresh::Revoked) => self.consent().await?,
                // Offline, or Google having trouble: signing in again
                // wouldn't help, so say so instead of opening the browser.
                Err(Refresh::Failed(e)) => return Err(format!("Couldn't reach Google to refresh sign-in: {e}; try again.")),
            },
            None => self.consent().await?,
        };
        let token = fresh.access_token.clone();
        *cached = Some(Cached {
            token: fresh.access_token,
            expires: Instant::now() + Duration::from_secs(fresh.expires_in.unwrap_or(3600)),
        });
        Ok(token)
    }

    /// Runs the consent flow now, regardless of any saved token, and saves
    /// the new refresh token. Used by `agent-unibrow --google-signin`.
    pub async fn sign_in(&self) -> Result<(), String> {
        let fresh = self.consent().await?;
        if fresh.refresh_token.is_none() {
            return Err("Google didn't return a refresh token. Remove agent-unibrow's access at \
                 https://myaccount.google.com/permissions and sign in again."
                .into());
        }
        *self.access.lock().await = Some(Cached {
            token: fresh.access_token,
            expires: Instant::now() + Duration::from_secs(fresh.expires_in.unwrap_or(3600)),
        });
        Ok(())
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, Refresh> {
        let resp = self
            .http
            .post(&self.client.token_uri)
            .form(&[
                ("client_id", self.client.client_id.as_str()),
                ("client_secret", self.client.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| Refresh::Failed(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(if refresh_token_revoked(status, &body) {
                Refresh::Revoked
            } else {
                Refresh::Failed(format!("HTTP {status}: {}", body.trim().chars().take(300).collect::<String>()))
            });
        }
        resp.json().await.map_err(|e| Refresh::Failed(format!("bad token response: {e}")))
    }

    /// Opens the browser to Google's consent screen and waits for the redirect.
    async fn consent(&self) -> Result<TokenResponse, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("could not open a local port for sign-in: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let redirect = format!("http://127.0.0.1:{port}");

        let verifier: String = rand::rng().sample_iter(&Alphanumeric).take(64).map(char::from).collect();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state: String = rand::rng().sample_iter(&Alphanumeric).take(24).map(char::from).collect();

        let mut url = url::Url::parse(&self.client.auth_uri).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client.client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_type", "code")
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("access_type", "offline")
            .append_pair("include_granted_scopes", "true")
            .append_pair("prompt", "consent");
        // Always print the link: the browser may open somewhere unexpected.
        eprintln!("\nGoogle sign-in for agent-unibrow. If no browser tab opened, visit:\n{url}\n");
        if let Err(e) = open::that_detached(url.as_str()) {
            eprintln!("(couldn't open the browser automatically: {e})");
        }

        let code = tokio::time::timeout(CONSENT_TIMEOUT, wait_for_code(&listener, &state))
            .await
            .map_err(|_| "Google sign-in timed out after 5 minutes".to_string())??;

        let resp = self
            .http
            .post(&self.client.token_uri)
            .form(&[
                ("client_id", self.client.client_id.as_str()),
                ("client_secret", self.client.client_secret.as_str()),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("code exchange failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("code exchange rejected: {}", resp.text().await.unwrap_or_default()));
        }
        let tokens: TokenResponse = resp.json().await.map_err(|e| format!("bad token response: {e}"))?;
        if let Some(refresh) = tokens.refresh_token.clone() {
            tokio::task::spawn_blocking(move || store_refresh_token(&refresh))
                .await
                .map_err(|e| e.to_string())??;
        }
        Ok(tokens)
    }
}

/// Serves the loopback redirect until a request carries our `state` and
/// either `code` or `error`. Anything else (a stray path, a bad request, a
/// wrong or missing state) gets an error page and is otherwise ignored, so
/// it can't end the sign-in; the caller's timeout bounds the wait.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 8192];
        // A connection that never sends a request mustn't hold up the rest.
        let n = match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let (status, body, outcome) = redirect_outcome(&String::from_utf8_lossy(&buf[..n]), state);
        let html = format!(
            "<!doctype html><meta charset=utf-8><title>agent-unibrow</title><body style=\"font:16px system-ui;padding:40px\">{body}</body>"
        );
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

/// The reply to one request at the loopback redirect (status line and page
/// text) and, for the redirect itself, how the sign-in ended.
fn redirect_outcome(req: &str, state: &str) -> (&'static str, String, Option<Result<String, String>>) {
    let target = req.lines().next().and_then(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
        [_, target, version] if version.starts_with("HTTP/") && target.starts_with('/') => Some(target),
        _ => None,
    });
    let Some(url) = target.and_then(|t| url::Url::parse(&format!("http://localhost{t}")).ok()) else {
        return ("400 Bad Request", String::new(), None);
    };
    if url.path() != "/" {
        return ("404 Not Found", String::new(), None); // favicon and the like
    }
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    if q.get("state").map(String::as_str) != Some(state) {
        return ("400 Bad Request", "This isn't the sign-in agent-unibrow is waiting for. Close this tab.".into(), None);
    }
    if let Some(err) = q.get("error") {
        // Shown on the page, so only plain characters.
        let shown: String = err.chars().filter(|c| c.is_ascii_alphanumeric() || "_-. ".contains(*c)).take(100).collect();
        ("400 Bad Request", format!("Sign-in failed: {shown}. You can close this tab."), Some(Err(format!("Google sign-in failed: {err}"))))
    } else if let Some(code) = q.get("code") {
        ("200 OK", "agent-unibrow is signed in to Gmail. You can close this tab.".into(), Some(Ok(code.clone())))
    } else {
        ("400 Bad Request", String::new(), None)
    }
}

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| format!("keyring unavailable: {e}"))
}

/// Whether the user has signed in to Google (a refresh token is saved).
/// Says nothing about the token itself.
pub fn signed_in() -> bool {
    load_refresh_token().is_some()
}

fn load_refresh_token() -> Option<String> {
    entry().ok()?.get_password().ok()
}

fn store_refresh_token(token: &str) -> Result<(), String> {
    entry()?.set_password(token).map_err(|e| format!("could not save the Google token to the keyring: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_revoked_token_means_signing_in_again() {
        assert!(refresh_token_revoked(400, r#"{"error": "invalid_grant", "error_description": "Token has been expired or revoked."}"#));
        assert!(refresh_token_revoked(401, r#"{"error": "unauthorized_client"}"#));
        assert!(refresh_token_revoked(400, "invalid_grant"));
        assert!(!refresh_token_revoked(400, r#"{"error": "invalid_request"}"#));
        assert!(!refresh_token_revoked(500, r#"{"error": "internal_failure"}"#));
        assert!(!refresh_token_revoked(503, "invalid_grant"));
        assert!(!refresh_token_revoked(429, ""));
    }

    #[test]
    fn redirect_requests() {
        let get = |target: &str| format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        let ok = redirect_outcome(&get("/?state=s1&code=abc&scope=x"), "s1");
        assert_eq!((ok.0, ok.2), ("200 OK", Some(Ok("abc".to_string()))));
        let denied = redirect_outcome(&get("/?state=s1&error=access_denied"), "s1");
        assert!(matches!(denied.2, Some(Err(e)) if e.contains("access_denied")));
        // None of these end the sign-in.
        for req in [
            get("/?state=other&code=abc"),
            get("/?code=abc"),
            get("/?state=other&error=access_denied"),
            get("/favicon.ico"),
            get("/x?state=s1&code=abc"),
            get("/?state=s1"),
            "garbage".to_string(),
            "GET http://[::1 HTTP/1.1\r\n".to_string(),
            String::new(),
        ] {
            assert!(redirect_outcome(&req, "s1").2.is_none(), "{req:?} ended the sign-in");
        }
    }

    #[tokio::test]
    async fn stray_requests_dont_end_the_wait() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let send = |req: &'static str| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            let mut reply = String::new();
            let _ = s.read_to_string(&mut reply).await;
            reply
        };
        let client = async {
            assert!(send("GET /?state=wrong&code=evil HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 400"));
            assert!(send("nonsense\r\n\r\n").await.starts_with("HTTP/1.1 400"));
            assert!(send("GET /favicon.ico HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 404"));
            assert!(send("GET /?state=right&code=good HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 200"));
        };
        let (code, ()) = tokio::join!(wait_for_code(&listener, "right"), client);
        assert_eq!(code.unwrap(), "good");
    }
}
