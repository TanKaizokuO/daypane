//! OAuth 2.0 for installed apps: bring-your-own client, loopback redirect, PKCE (S256).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use super::api::{http_agent, truncate};
use crate::app::APP_NAME;
use crate::config::Config;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The user's own OAuth "Desktop app" client.
#[derive(Debug, Clone, Copy)]
pub struct Client<'a> {
    pub id: &'a str,
    pub secret: Option<&'a str>,
}

impl<'a> Client<'a> {
    /// `None` when no client id is configured.
    pub fn from_config(cfg: &'a Config) -> Option<Self> {
        let id = cfg
            .google
            .client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        let secret = cfg
            .google
            .client_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        Some(Self { id, secret })
    }

    /// Like [`Client::from_config`], with an actionable error.
    pub fn require(cfg: &'a Config) -> Result<Self> {
        Self::from_config(cfg).ok_or_else(|| {
            anyhow!(
                "no Google OAuth client configured: set `client_id` (and `client_secret`) under [google] in {}. \
                 See the \"Google Calendar setup\" section of the README for creating one.",
                crate::paths::config_file().map(|p| p.display().to_string()).unwrap_or_else(|_| "config.toml".into())
            )
        })
    }

    fn form<'f>(&'f self, mut fields: Vec<(&'f str, &'f str)>) -> Vec<(&'f str, &'f str)> {
        fields.push(("client_id", self.id));
        if let Some(secret) = self.secret {
            fields.push(("client_secret", secret));
        }
        fields
    }
}

// ─── PKCE / URL construction ────────────────────────────────────────────────

/// `BASE64URL(SHA256(verifier))`, RFC 7636 §4.2.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 32 random bytes as 43 URL-safe characters (valid as PKCE verifier and as `state`).
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn auth_url(client_id: &str, redirect_uri: &str, code_challenge: &str, state: &str) -> Url {
    let mut url = Url::parse(AUTH_ENDPOINT).expect("constant URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        // Offline + forced consent guarantee Google returns a refresh token.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    url
}

// ─── Loopback callback ──────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code(String),
    /// Not the OAuth redirect (favicon, preconnect, …).
    Ignore,
}

/// Interpret the HTTP request line of a request hitting the loopback listener.
fn parse_callback(request_line: &str, expected_state: &str) -> Result<Callback> {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(Callback::Ignore);
    };
    let Ok(url) = Url::parse("http://127.0.0.1/").and_then(|base| base.join(target)) else {
        return Ok(Callback::Ignore);
    };
    if url.path() != "/" {
        return Ok(Callback::Ignore);
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(err) = param("error") {
        bail!("Google denied the authorization request: {err}");
    }
    let Some(code) = param("code") else {
        return Ok(Callback::Ignore);
    };
    if param("state").as_deref() != Some(expected_state) {
        bail!("OAuth `state` mismatch; refusing the response (possible forged redirect)");
    }
    Ok(Callback::Code(code))
}

fn respond(mut stream: TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{APP_NAME}</title>\
         <body style=\"font-family:sans-serif;text-align:center;margin-top:20vh\"><h2>{message}</h2></body>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Wait for the browser redirect on `listener`; returns the authorization code.
fn await_code(listener: &TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the browser authorization");
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(e) => return Err(e).context("accepting the OAuth redirect"),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT))?;
        let mut line = String::new();
        let read = BufReader::new((&stream).take(8 * 1024)).read_line(&mut line);
        if read.is_err() || line.is_empty() {
            continue; // idle speculative connection
        }
        match parse_callback(&line, state) {
            Ok(Callback::Code(code)) => {
                respond(stream, "200 OK", "Authorized. You can close this tab.");
                return Ok(code);
            }
            Ok(Callback::Ignore) => respond(stream, "404 Not Found", "Not found"),
            Err(e) => {
                respond(
                    stream,
                    "400 Bad Request",
                    "Authorization failed. See the terminal.",
                );
                return Err(e);
            }
        }
    }
}

fn open_browser(url: &str) {
    println!("Open this URL in your browser to authorize read-only calendar access:\n\n  {url}\n");
    if Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_err()
    {
        println!("(could not run xdg-open; open the URL manually)");
    }
}

// ─── Token endpoint ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

/// POST a form to the token endpoint, mapping error statuses to readable errors.
fn token_request(fields: &[(&str, &str)]) -> Result<TokenResponse> {
    let mut resp = http_agent()
        .post(TOKEN_ENDPOINT)
        .send_form(fields.iter().copied())
        .context("contacting Google's token endpoint")?;
    if resp.status().is_success() {
        return resp
            .body_mut()
            .read_json()
            .context("decoding token response");
    }
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    Err(token_error(status, &body))
}

fn token_error(status: u16, body: &str) -> anyhow::Error {
    match serde_json::from_str::<TokenError>(body) {
        Ok(e) if e.error == "invalid_grant" => anyhow!(
            "Google rejected the stored credentials ({}); the grant was revoked or expired. \
             Run `{APP_NAME} login` again.",
            e.error_description.unwrap_or_else(|| e.error.clone())
        ),
        Ok(e) => anyhow!(
            "Google token endpoint error `{}`{}",
            e.error,
            e.error_description
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ),
        Err(_) => anyhow!(
            "Google token endpoint returned HTTP {status}: {}",
            truncate(body, 300)
        ),
    }
}

/// Short-lived access token from a stored refresh token.
pub fn refresh_access_token(client: Client<'_>, refresh_token: &str) -> Result<String> {
    token_request(&client.form(vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ]))
    .map(|t| t.access_token)
}

/// Interactive authorization-code flow; returns the long-lived refresh token.
pub fn authorize(client: Client<'_>) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding a loopback port for the OAuth redirect")?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = random_token();
    let state = random_token();

    open_browser(auth_url(client.id, &redirect_uri, &pkce_challenge(&verifier), &state).as_str());
    let code = await_code(&listener, &state)?;

    let tokens = token_request(&client.form(vec![
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect_uri),
    ]))?;
    if let Some(granted) = &tokens.scope {
        if !granted.split_whitespace().any(|s| s == SCOPE) {
            bail!("the calendar read-only permission was not granted (granted: {granted})");
        }
    }
    tokens.refresh_token.filter(|t| !t.is_empty()).ok_or_else(|| {
        anyhow!("Google did not return a refresh token; revoke the app's access in your Google account and retry")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_appendix_b_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_are_valid_verifiers_and_distinct() {
        let (a, b) = (random_token(), random_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }

    #[test]
    fn auth_url_has_all_required_parameters() {
        let url = auth_url(
            "cid.apps.googleusercontent.com",
            "http://127.0.0.1:4242",
            "CHAL",
            "ST",
        );
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(url.path(), "/o/oauth2/v2/auth");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let expect = [
            ("client_id", "cid.apps.googleusercontent.com"),
            ("redirect_uri", "http://127.0.0.1:4242"),
            ("response_type", "code"),
            ("scope", "https://www.googleapis.com/auth/calendar.readonly"),
            ("code_challenge", "CHAL"),
            ("code_challenge_method", "S256"),
            ("state", "ST"),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ];
        assert_eq!(q.len(), expect.len());
        for (k, v) in expect {
            assert_eq!(q.get(k).map(String::as_str), Some(v), "{k}");
        }
    }

    #[test]
    fn callback_with_matching_state_yields_code() {
        assert_eq!(
            parse_callback("GET /?code=4%2F0AbC&state=xyz&scope=s HTTP/1.1\r\n", "xyz").unwrap(),
            Callback::Code("4/0AbC".into())
        );
    }

    #[test]
    fn callback_state_mismatch_and_missing_state_are_errors() {
        assert!(parse_callback("GET /?code=c&state=evil HTTP/1.1", "xyz").is_err());
        assert!(parse_callback("GET /?code=c HTTP/1.1", "xyz").is_err());
    }

    #[test]
    fn callback_error_param_is_reported() {
        let e = parse_callback("GET /?error=access_denied&state=xyz HTTP/1.1", "xyz").unwrap_err();
        assert!(e.to_string().contains("access_denied"));
    }

    #[test]
    fn unrelated_requests_are_ignored() {
        for line in [
            "GET /favicon.ico HTTP/1.1",
            "GET / HTTP/1.1",
            "POST /?code=c&state=xyz HTTP/1.1",
            "",
        ] {
            assert_eq!(
                parse_callback(line, "xyz").unwrap(),
                Callback::Ignore,
                "{line:?}"
            );
        }
    }

    #[test]
    fn token_errors_are_readable() {
        let e = token_error(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        );
        let msg = e.to_string();
        assert!(msg.contains("login") && msg.contains("revoked"), "{msg}");
        let e = token_error(
            401,
            r#"{"error":"invalid_client","error_description":"Unauthorized"}"#,
        );
        assert!(e.to_string().contains("invalid_client"));
        assert!(token_error(502, "<html>bad gateway</html>")
            .to_string()
            .contains("502"));
    }

    #[test]
    fn missing_client_id_points_at_readme() {
        let mut cfg = Config::default();
        let msg = Client::require(&cfg).unwrap_err().to_string();
        assert!(msg.contains("README") && msg.contains("client_id"), "{msg}");
        cfg.google.client_id = Some("  ".into());
        assert!(Client::from_config(&cfg).is_none());
        cfg.google.client_id = Some("id".into());
        cfg.google.client_secret = Some("sec".into());
        let c = Client::from_config(&cfg).unwrap();
        assert_eq!((c.id, c.secret), ("id", Some("sec")));
        let fields = c.form(vec![("grant_type", "refresh_token")]);
        assert!(
            fields.contains(&("client_secret", "sec")) && fields.contains(&("client_id", "id"))
        );
    }

    #[test]
    fn loopback_flow_end_to_end_without_network() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = std::thread::spawn(move || {
            for target in ["/favicon.ico", "/?code=THE_CODE&state=S"] {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                let mut resp = String::new();
                s.read_to_string(&mut resp).unwrap();
                assert!(resp.starts_with(if target == "/favicon.ico" {
                    "HTTP/1.1 404"
                } else {
                    "HTTP/1.1 200"
                }));
            }
        });
        assert_eq!(await_code(&listener, "S").unwrap(), "THE_CODE");
        browser.join().unwrap();
    }
}
