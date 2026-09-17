//! Google OAuth 2.0 (installed/"Desktop app" client, PKCE) — the single
//! implementation shared by everything in this crate that authenticates to
//! Google:
//!
//! - **`jamaild`'s calendar sync**: a [`GoogleTokenSource`] handed to
//!   [`crate::caldav::AuthScheme::Bearer`], so an account's `caldav:` block
//!   can point straight at Google's CalDAV endpoint — no jadav in between
//!   (see [`crate::calsync::resolve_auth`]).
//! - **`jadav`'s Google mirror**: [`crate::jadav::google::GoogleAuth`]
//!   wraps the same [`refresh_access_token`] call and adds persistence in
//!   jadav's own store.
//!
//! ## The flow
//!
//! Google issues no password a daemon may hold. The one long-lived
//! credential is a *refresh token*, obtained once by a human in a browser
//! (`jamaild google-auth <account>`, `jadav google auth <remote>`):
//! [`build_auth_url`] builds an authorization URL carrying a PKCE S256
//! challenge, the browser ends on a `http://localhost/?code=...` page that
//! fails to load, and that pasted address is [`parse_redirect`]ed and
//! [`exchange_code`]d for the refresh token. From then on an access token
//! is one [`refresh_access_token`] call, cached in memory until a minute
//! before it expires ([`GoogleTokenSource`]).
//!
//! ## Deliberate limitations
//!
//! - **No loopback HTTP listener for the redirect** — the user pastes the
//!   address back. A listener would need a fixed port registered in the
//!   OAuth client's redirect list and a way to open a browser; pasting
//!   works over SSH, which is where a daemon is usually first configured.
//! - **This module stores no refresh token.** Where it lives is the
//!   caller's decision: `jamaild` reads it from config (`password`/
//!   `command` auth, the same shape as every other secret in this crate),
//!   `jadav` keeps it in its store. Access tokens are cached in memory
//!   only, so a daemon restart spends one refresh call and nothing is
//!   written to disk by this module.
//! - **`invalid_grant` is surfaced, never retried** ([`TokenError::InvalidGrant`]):
//!   a revoked or expired refresh token needs a human at a browser, and
//!   hammering the token endpoint would only get the client rate-limited.
//!   This is also the everyday failure of an OAuth client still in
//!   *Testing* publishing status, where Google expires refresh tokens
//!   after seven days.

use crate::httpc::{self, HttpResponse, HttpUrl};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Read/write access to calendars — the scope both Google's CalDAV
/// endpoint and the Calendar REST API are satisfied by.
pub const SCOPE_CALENDAR: &str = "https://www.googleapis.com/auth/calendar";
/// Google's redirect for an installed client with no loopback listener:
/// the browser lands on a page that fails to load and the user pastes the
/// address back. Must be listed on the OAuth client as-is.
pub const REDIRECT_URI: &str = "http://localhost";

/// Google's CalDAV v2 base. The per-account entry point is
/// `<CALDAV_BASE><address>/user` — the documented principal URL, from
/// which [`crate::caldav::CalDavClient::discover_calendars`]'s
/// `current-user-principal` -> `calendar-home-set` chain finds every
/// calendar of the account.
pub const CALDAV_BASE: &str = "https://apidata.googleusercontent.com/caldav/v2/";

const TIMEOUT: Duration = Duration::from_secs(60);
/// Treat an access token as expired this many seconds early, so a request
/// can't set off with a token that dies in flight.
const EXPIRY_SKEW_SECS: i64 = 60;

/// Google's CalDAV principal URL for one address — what a `caldav.url`
/// should be set to for a Google account.
/// (`@` is a legal path character per RFC 3986, and Google's own
/// documentation shows the address unescaped, so this stays readable in a
/// config file.)
pub fn caldav_url_for(address: &str) -> String {
    format!("{}{}/user", CALDAV_BASE, address)
}

/// What Google's token endpoint returned for a refresh or a code exchange.
#[derive(Debug, Default)]
pub struct TokenGrant {
    pub access_token: Option<String>,
    /// Lifetime of `access_token` in seconds (Google: 3600).
    pub expires_in: Option<i64>,
    /// Only ever present on a code exchange — a refresh never returns one.
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

#[derive(Deserialize, Default)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Why a token call failed, split the one way callers actually branch on.
#[derive(Debug)]
pub enum TokenError {
    /// `invalid_grant`: the refresh token is revoked, expired (an OAuth
    /// client in *Testing* status expires them after seven days), or was
    /// issued to a different client. Only a human at a browser fixes this.
    InvalidGrant,
    /// The token endpoint couldn't be reached, or answered 5xx/429 —
    /// worth retrying on the next poll.
    Transport(anyhow::Error),
    Other(anyhow::Error),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::InvalidGrant => write!(
                f,
                "Google rejected the refresh token (invalid_grant): it was revoked, \
                 expired, or belongs to another OAuth client — re-run the authorization"
            ),
            TokenError::Transport(e) => write!(f, "{:#}", e),
            TokenError::Other(e) => write!(f, "{:#}", e),
        }
    }
}

impl std::error::Error for TokenError {}

fn post_form(token_url: &str, body: &str) -> Result<HttpResponse> {
    let url = HttpUrl::parse(token_url)?;
    httpc::send(
        "POST",
        &url,
        &[
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        Some(body.as_bytes()),
        TIMEOUT,
    )
}

fn grant_from(resp: &HttpResponse) -> Result<(TokenResponse, TokenGrant), TokenError> {
    let parsed: TokenResponse = resp
        .json()
        .map_err(|e| TokenError::Other(e.context("decoding Google's token response")))?;
    let grant = TokenGrant {
        access_token: parsed.access_token.clone(),
        expires_in: parsed.expires_in,
        refresh_token: parsed.refresh_token.clone(),
        scope: parsed.scope.clone(),
    };
    Ok((parsed, grant))
}

/// Exchange a refresh token for a fresh access token. `token_url` is
/// [`TOKEN_URL`] in production and a local mock in tests.
pub fn refresh_access_token(
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenGrant, TokenError> {
    let body = httpc::form_urlencode(&[
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ]);
    let resp = post_form(token_url, &body).map_err(TokenError::Transport)?;
    let (parsed, grant) = grant_from(&resp)?;
    if resp.status == 400 && parsed.error.as_deref() == Some("invalid_grant") {
        return Err(TokenError::InvalidGrant);
    }
    if resp.status == 429 || (500..600).contains(&resp.status) {
        return Err(TokenError::Transport(anyhow!(
            "token refresh failed: HTTP {} {}",
            resp.status,
            parsed.error_description.unwrap_or_default()
        )));
    }
    if resp.status != 200 {
        return Err(TokenError::Other(anyhow!(
            "token refresh failed: HTTP {} {} {}",
            resp.status,
            parsed.error.unwrap_or_default(),
            parsed.error_description.unwrap_or_default()
        )));
    }
    Ok(grant)
}

/// Exchange an authorization code (plus its PKCE verifier) for the
/// long-lived refresh token.
pub fn exchange_code(
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenGrant> {
    let body = httpc::form_urlencode(&[
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("code_verifier", verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", REDIRECT_URI),
    ]);
    let resp = post_form(token_url, &body)?;
    let parsed: TokenResponse = resp.json()?;
    if resp.status != 200 {
        bail!(
            "token exchange failed: HTTP {} {} {}",
            resp.status,
            parsed.error.unwrap_or_default(),
            parsed.error_description.unwrap_or_default()
        );
    }
    Ok(TokenGrant {
        access_token: parsed.access_token,
        expires_in: parsed.expires_in,
        refresh_token: parsed.refresh_token,
        scope: parsed.scope,
    })
}

fn random_unreserved(n: usize) -> Result<String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect())
}

/// The authorization URL to open in a browser, plus the PKCE verifier and
/// the `state` to check the redirect against (Desktop-app flow, S256).
pub fn build_auth_url(client_id: &str, login_hint: &str) -> Result<(String, String, String)> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let verifier = random_unreserved(64)?;
    let state = random_unreserved(16)?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let url = format!(
        "{}?{}",
        AUTH_URL,
        httpc::build_query(&[
            ("client_id", client_id),
            ("redirect_uri", REDIRECT_URI),
            ("response_type", "code"),
            ("scope", SCOPE_CALENDAR),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("login_hint", login_hint),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ])
    );
    Ok((url, verifier, state))
}

/// Extract `code` (and check `state`) from a pasted redirect URL, or
/// accept a bare code.
pub fn parse_redirect(input: &str, expected_state: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("nothing pasted — re-run and paste the whole http://localhost/?code=... address");
    }
    if !input.contains('?') && !input.contains("code=") {
        return Ok(input.to_string());
    }
    let query = input.split_once('?').map(|(_, q)| q).unwrap_or(input);
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(httpc::percent_decode(v)),
                "state" => state = Some(httpc::percent_decode(v)),
                _ => {}
            }
        }
    }
    if let Some(s) = state
        && s != expected_state
    {
        bail!("state mismatch: expected {}, got {}", expected_state, s);
    }
    code.context("redirect URL has no code= parameter")
}

/// The whole browser round trip on a terminal: print the URL, read the
/// pasted redirect from stdin, exchange it. Storing the resulting refresh
/// token is the caller's job (see the module docs).
pub fn authorize_interactive(
    client_id: &str,
    client_secret: &str,
    login_hint: &str,
) -> Result<TokenGrant> {
    let (url, verifier, state) = build_auth_url(client_id, login_hint)?;
    println!(
        "Open this URL in a browser, sign in as {}, and approve:\n\n{}\n",
        login_hint, url
    );
    println!("The browser will end on an http://localhost/?code=... page that fails to load —");
    println!("copy that whole address (or just the code) and paste it here, then press Enter:");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the pasted redirect")?;
    let code = parse_redirect(&line, &state)?;
    let grant = exchange_code(TOKEN_URL, client_id, client_secret, &code, &verifier)?;
    if grant.refresh_token.is_none() {
        bail!(
            "Google returned no refresh token — the OAuth client must be of type \
             \"Desktop app\" and the request must carry access_type=offline"
        );
    }
    Ok(grant)
}

// ---------------------------------------------------------------------
// Refresh-token storage (the one file this module will write)
// ---------------------------------------------------------------------

/// One path segment safe for `account`: anything outside
/// `[A-Za-z0-9._-]` becomes `_`, so a config key with a slash or a space
/// in it can't escape the directory or confuse a shell.
fn sanitize_account(account: &str) -> String {
    let cleaned: String = account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim_matches('.').is_empty() {
        "account".to_string()
    } else {
        cleaned
    }
}

/// Where `jamaild google-auth <account>` parks a refresh token for an
/// account whose owner doesn't route it through a password manager:
/// `<data dir>/jamail/oauth/<account>.token`, next to `mail.db`.
pub fn refresh_token_path(account: &str) -> Result<std::path::PathBuf> {
    let data_dir = dirs::data_dir().context("could not determine the data directory")?;
    Ok(data_dir
        .join("jamail")
        .join("oauth")
        .join(format!("{}.token", sanitize_account(account))))
}

/// Write a refresh token to `path`, creating the directory `0700` and the
/// file `0600` — it is a password in every sense that matters, and the
/// point of writing it here instead of printing it is that it never
/// reaches the terminal's scrollback.
pub fn write_refresh_token(path: &std::path::Path, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", dir.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    // An existing file keeps its old mode through `create`, so set it too.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    writeln!(f, "{}", token).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A refresh token turned into a supply of access tokens, cached in
/// memory until [`EXPIRY_SKEW_SECS`] before expiry. Hand it to
/// [`crate::caldav::AuthScheme::Bearer`] wrapped in an `Arc<Mutex<_>>`
/// and keep that alive across poll cycles — one process-lifetime instance
/// means one refresh an hour, not one per cycle.
pub struct GoogleTokenSource {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_url: String,
    access_token: Option<String>,
    expires_at: i64,
}

impl GoogleTokenSource {
    pub fn new(client_id: &str, client_secret: &str, refresh_token: &str) -> Self {
        Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            refresh_token: refresh_token.to_string(),
            token_url: TOKEN_URL.to_string(),
            access_token: None,
            expires_at: 0,
        }
    }

    /// Point at a different token endpoint (tests use a local mock).
    pub fn with_token_url(mut self, url: &str) -> Self {
        self.token_url = url.to_string();
        self
    }

    /// A currently-valid access token, refreshing only when the cached one
    /// is gone or about to expire.
    pub fn access_token(&mut self) -> Result<String, TokenError> {
        if let Some(t) = &self.access_token
            && self.expires_at - EXPIRY_SKEW_SECS > Utc::now().timestamp()
        {
            return Ok(t.clone());
        }
        let grant = refresh_access_token(
            &self.token_url,
            &self.client_id,
            &self.client_secret,
            &self.refresh_token,
        )?;
        let token = grant.access_token.ok_or_else(|| {
            TokenError::Other(anyhow!("Google's token endpoint returned no access token"))
        })?;
        self.expires_at = Utc::now().timestamp() + grant.expires_in.unwrap_or(3600);
        self.access_token = Some(token.clone());
        Ok(token)
    }

    /// Drop the cached access token — called after a `401`, so the next
    /// request refreshes instead of replaying a token the server rejected.
    pub fn invalidate(&mut self) {
        self.access_token = None;
        self.expires_at = 0;
    }
}

impl crate::caldav::TokenSource for GoogleTokenSource {
    fn token(&mut self) -> Result<String> {
        self.access_token().map_err(|e| anyhow!("{}", e))
    }
    fn invalidate(&mut self) {
        GoogleTokenSource::invalidate(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldav::TokenSource;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// A scripted mock token endpoint: serves `responses` in order, one
    /// per connection, and returns the raw requests it received. Joining
    /// the handle therefore also asserts *how many* calls were made.
    fn mock_token_server(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut received = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            received.extend_from_slice(&buf[..n]);
                            // Headers plus the whole form body arrive in
                            // one write; a short read that already has the
                            // blank line is enough to answer.
                            if received.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                requests.push(String::from_utf8_lossy(&received).into_owned());
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
            requests
        });
        (format!("http://{}/token", addr), handle)
    }

    fn json_response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            status,
            body.len(),
            body
        )
    }

    const OK_BODY: &str = r#"{"access_token":"ya29.a0","expires_in":3599,"scope":"https://www.googleapis.com/auth/calendar","token_type":"Bearer"}"#;

    #[test]
    fn refresh_posts_the_refresh_grant_and_returns_the_access_token() {
        let (url, handle) = mock_token_server(vec![json_response(200, OK_BODY)]);
        let grant = refresh_access_token(&url, "cid", "csecret", "1//rtoken").unwrap();
        assert_eq!(grant.access_token.as_deref(), Some("ya29.a0"));
        assert_eq!(grant.expires_in, Some(3599));
        let req = handle.join().unwrap().remove(0);
        assert!(req.starts_with("POST /token HTTP/1.1"));
        assert!(req.contains("Content-Type: application/x-www-form-urlencoded"));
        assert!(req.contains("grant_type=refresh_token"));
        assert!(req.contains("refresh_token=1%2F%2Frtoken"));
        assert!(req.contains("client_id=cid"));
    }

    #[test]
    fn invalid_grant_is_its_own_error_so_callers_stop_retrying() {
        let body =
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#;
        let (url, _handle) = mock_token_server(vec![json_response(400, body)]);
        let err = refresh_access_token(&url, "cid", "csecret", "stale").unwrap_err();
        assert!(matches!(err, TokenError::InvalidGrant));
        assert!(format!("{}", err).contains("re-run the authorization"));
    }

    #[test]
    fn a_5xx_from_the_token_endpoint_is_transport_not_fatal() {
        let (url, _handle) = mock_token_server(vec![json_response(503, "{}")]);
        let err = refresh_access_token(&url, "cid", "csecret", "rt").unwrap_err();
        assert!(matches!(err, TokenError::Transport(_)));
    }

    #[test]
    fn an_unreachable_token_endpoint_is_transport_too() {
        // Port 1 on loopback: connection refused, not an HTTP answer.
        let err = refresh_access_token("http://127.0.0.1:1/token", "c", "s", "r").unwrap_err();
        assert!(matches!(err, TokenError::Transport(_)));
    }

    #[test]
    fn token_source_caches_until_expiry_so_a_poll_loop_refreshes_once() {
        // Exactly one scripted response: a second HTTP call would block on
        // an accept that never comes, and the join below would hang — so
        // the test passing *is* the assertion that only one was made.
        let (url, handle) = mock_token_server(vec![json_response(200, OK_BODY)]);
        let mut src = GoogleTokenSource::new("cid", "csecret", "rt").with_token_url(&url);
        assert_eq!(src.token().unwrap(), "ya29.a0");
        assert_eq!(src.token().unwrap(), "ya29.a0");
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn invalidate_forces_the_next_token_call_to_refresh() {
        let second = r#"{"access_token":"ya29.second","expires_in":3599}"#;
        let (url, handle) = mock_token_server(vec![
            json_response(200, OK_BODY),
            json_response(200, second),
        ]);
        let mut src = GoogleTokenSource::new("cid", "csecret", "rt").with_token_url(&url);
        assert_eq!(src.token().unwrap(), "ya29.a0");
        TokenSource::invalidate(&mut src);
        assert_eq!(src.token().unwrap(), "ya29.second");
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn an_already_expired_grant_is_not_served_from_cache() {
        let (url, handle) = mock_token_server(vec![
            json_response(200, r#"{"access_token":"short","expires_in":10}"#),
            json_response(200, r#"{"access_token":"fresh","expires_in":3599}"#),
        ]);
        let mut src = GoogleTokenSource::new("cid", "csecret", "rt").with_token_url(&url);
        // expires_in 10 < EXPIRY_SKEW_SECS: usable now, but never cached.
        assert_eq!(src.token().unwrap(), "short");
        assert_eq!(src.token().unwrap(), "fresh");
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn pkce_url_carries_challenge_scope_and_redirect() {
        let (url, verifier, state) = build_auth_url("cid", "me@example.com").unwrap();
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains(&format!(
            "scope={}",
            httpc::percent_encode_component(SCOPE_CALENDAR)
        )));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost"));
        assert!(url.contains("login_hint=me%40example.com"));
        assert!(url.contains(&format!("state={}", state)));
        assert_eq!(verifier.len(), 64);
    }

    #[test]
    fn redirect_parsing_accepts_a_url_or_a_bare_code_and_checks_state() {
        assert_eq!(
            parse_redirect("http://localhost/?state=st&code=4%2F0Abc", "st").unwrap(),
            "4/0Abc"
        );
        assert_eq!(parse_redirect("  4/0Abc \n", "st").unwrap(), "4/0Abc");
        assert!(parse_redirect("http://localhost/?state=other&code=x", "st").is_err());
        // An empty paste must not be sent to Google as a bare code.
        assert!(parse_redirect("  \n", "st").is_err());
    }

    #[test]
    fn account_names_are_reduced_to_one_safe_path_segment() {
        assert_eq!(sanitize_account("work"), "work");
        // Dots are kept (they are ordinary filename characters); the
        // separators are what must not survive.
        assert_eq!(sanitize_account("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_account("my account"), "my_account");
        assert_eq!(sanitize_account(".."), "account");
    }

    #[test]
    fn writing_a_refresh_token_leaves_it_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("jamail-goauth-{}", std::process::id()));
        let path = dir.join("oauth").join("work.token");
        write_refresh_token(&path, "1//secret").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1//secret\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        // Overwriting an existing token keeps the tight mode.
        write_refresh_token(&path, "1//second").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caldav_url_is_the_documented_principal_url() {
        assert_eq!(
            caldav_url_for("alice@gmail.com"),
            "https://apidata.googleusercontent.com/caldav/v2/alice@gmail.com/user"
        );
    }
}
