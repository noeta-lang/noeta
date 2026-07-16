//! GitHub OAuth **device flow** (namespace-protection #1, laptop claim) — how `noeta claim` proves a
//! scope owner's GitHub identity when there is no CI OIDC token (a laptop). The device flow is the
//! OAuth grant for clients that can't hold a secret: the client shows the user a short code to enter at
//! a GitHub URL, then polls until the user authorizes, receiving an access token. That token goes to
//! the registry's `claim` endpoint, which verifies org/user ownership server-side (see the registry's
//! `github.ts`).
//!
//! Endpoints are parameterized (`oauth_base`) so tests drive the flow against a local double; the CLI
//! passes `https://github.com` (overridable via `NOETA_GITHUB_OAUTH_URL`).

use std::time::{Duration, Instant};

use crate::error::PmError;

/// A pending device authorization — what to show the user, plus the state [`poll_for_token`] needs.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    /// Where the user goes to authorize (e.g. `https://github.com/login/device`).
    pub verification_uri: String,
    /// The short code the user types there (e.g. `WDJB-MJHT`).
    pub user_code: String,
    /// The opaque code the client polls with (not shown to the user).
    device_code: String,
    /// Seconds to wait between polls (GitHub throttles; a `slow_down` bumps this).
    interval: u64,
    /// Seconds until the request expires.
    expires_in: u64,
}

/// A blocking HTTP client for the device flow (short timeout — these are quick calls).
fn client() -> Result<reqwest::blocking::Client, PmError> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("noeta/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| PmError::Network(format!("cannot build the GitHub OAuth client: {err}")))
}

/// Start the device flow: request a device + user code for `client_id` with the given `scope`
/// (`read:org` so the registry can check org admin membership). Returns the codes to show the user.
pub fn request_device_code(
    oauth_base: &str,
    client_id: &str,
    scope: &str,
) -> Result<DeviceAuth, PmError> {
    let base = oauth_base.trim_end_matches('/');
    let resp = client()?
        .post(format!("{base}/login/device/code"))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[("client_id", client_id), ("scope", scope)])
        .send()
        .map_err(|err| {
            PmError::Network(format!("requesting a GitHub device code failed: {err}"))
        })?;
    if !resp.status().is_success() {
        return Err(PmError::Auth(format!(
            "GitHub device-code request returned {} (is the OAuth client id correct?)",
            resp.status()
        )));
    }
    #[derive(serde::Deserialize)]
    struct DeviceResponse {
        device_code: String,
        user_code: String,
        verification_uri: String,
        #[serde(default = "default_interval")]
        interval: u64,
        #[serde(default)]
        expires_in: u64,
    }
    fn default_interval() -> u64 {
        5
    }
    let d: DeviceResponse = resp.json().map_err(|err| {
        PmError::Network(format!(
            "GitHub device-code response was not the expected JSON: {err}"
        ))
    })?;
    Ok(DeviceAuth {
        verification_uri: d.verification_uri,
        user_code: d.user_code,
        device_code: d.device_code,
        interval: d.interval.max(1),
        expires_in: if d.expires_in == 0 { 900 } else { d.expires_in },
    })
}

/// Poll for the access token until the user authorizes (namespace-protection #1). Blocks, waiting
/// `interval` between polls (honoring a `slow_down`), until success, denial, or expiry. The first poll
/// is immediate so a test (or an already-authorized user) returns without sleeping.
pub fn poll_for_token(
    oauth_base: &str,
    client_id: &str,
    device: &DeviceAuth,
) -> Result<String, PmError> {
    let base = oauth_base.trim_end_matches('/');
    let client = client()?;
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval;
    loop {
        let resp = client
            .post(format!("{base}/login/oauth/access_token"))
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", &device.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .map_err(|err| {
                PmError::Network(format!("polling for the GitHub token failed: {err}"))
            })?;
        #[derive(serde::Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            error: Option<String>,
            interval: Option<u64>,
        }
        let body: TokenResponse = resp.json().map_err(|err| {
            PmError::Network(format!(
                "GitHub token response was not the expected JSON: {err}"
            ))
        })?;
        if let Some(token) = body.access_token {
            return Ok(token);
        }
        match body.error.as_deref() {
            // Still waiting for the user — keep polling.
            Some("authorization_pending") => {}
            // GitHub asked us to back off; it also sends a new interval.
            Some("slow_down") => interval = body.interval.unwrap_or(interval + 5),
            Some("access_denied") => {
                return Err(PmError::Auth("authorization was denied".to_string()));
            }
            Some("expired_token") => {
                return Err(PmError::Auth(
                    "the device code expired before you authorized — run the command again"
                        .to_string(),
                ));
            }
            other => {
                return Err(PmError::Auth(format!(
                    "GitHub device authorization failed: {}",
                    other.unwrap_or("unknown error")
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(PmError::Auth(
                "timed out waiting for GitHub authorization".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    /// A tiny in-process HTTP server: `handler(path, nth_call) -> (status, json)` where `nth_call`
    /// counts calls to that path (so a test can return `authorization_pending` then success).
    fn mock_server(handler: impl Fn(&str, usize) -> (u16, String) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let counts: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicUsize>>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut content_length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    reader.read_exact(&mut body).unwrap();
                }
                let nth = {
                    let mut map = counts.lock().unwrap();
                    let c = map.entry(path.clone()).or_default().clone();
                    c.fetch_add(1, Ordering::SeqCst)
                };
                let (status, json) = handler(&path, nth);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        ready_rx.recv().unwrap();
        format!("http://{addr}")
    }

    #[test]
    fn device_flow_requests_a_code_then_polls_through_pending_to_a_token() {
        let base = mock_server(|path, nth| {
            match path {
            "/login/device/code" => (
                200,
                r#"{"device_code":"DC","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0}"#
                    .to_string(),
            ),
            // First poll: still pending; second: the token.
            "/login/oauth/access_token" if nth == 0 => {
                (200, r#"{"error":"authorization_pending"}"#.to_string())
            }
            "/login/oauth/access_token" => (200, r#"{"access_token":"gho_abc123"}"#.to_string()),
            _ => (404, "{}".to_string()),
        }
        });
        let device = request_device_code(&base, "client-id", "read:org").unwrap();
        assert_eq!(device.user_code, "WDJB-MJHT");
        assert!(device.verification_uri.contains("github.com"));
        let token = poll_for_token(&base, "client-id", &device).unwrap();
        assert_eq!(token, "gho_abc123");
    }

    #[test]
    fn a_denied_authorization_is_an_error() {
        let base = mock_server(|path, _| {
            match path {
            "/login/device/code" => (
                200,
                r#"{"device_code":"DC","user_code":"U","verification_uri":"https://x","interval":0}"#
                    .to_string(),
            ),
            "/login/oauth/access_token" => (200, r#"{"error":"access_denied"}"#.to_string()),
            _ => (404, "{}".to_string()),
        }
        });
        let device = request_device_code(&base, "client-id", "read:org").unwrap();
        let err = poll_for_token(&base, "client-id", &device).unwrap_err();
        assert!(err.message().contains("denied"), "{err}");
    }
}
