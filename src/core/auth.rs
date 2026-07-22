//! OAuth2 (PKCE) foundation for subscription login, plus a token store.
//!
//! Generic and provider-agnostic: an [`AuthConfig`] carries the endpoints, client id, scopes,
//! and loopback redirect for a given provider (Claude Pro/Max, ChatGPT); the actual client ids
//! are supplied via config. The PKCE helpers, authorize-URL builder, and token store are pure
//! and unit-tested; [`AuthConfig::login`] runs the interactive loopback flow.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use url::Url;

/// A PKCE verifier/challenge pair (`S256`).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let verifier = random_b64url(32);
        Pkce {
            challenge: challenge_of(&verifier),
            verifier,
        }
    }
}

/// `base64url(sha256(verifier))`, no padding.
fn challenge_of(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}

fn random_b64url(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS RNG unavailable");
    URL_SAFE_NO_PAD.encode(buf)
}

/// `application/x-www-form-urlencoded` body from key/value pairs.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish()
}

/// Per-provider OAuth configuration.
pub struct AuthConfig {
    pub client_id: String,
    pub auth_url: String,
    pub token_url: String,
    /// Loopback redirect, e.g. `http://127.0.0.1:8788/callback`.
    pub redirect_uri: String,
    pub scopes: Vec<String>,
}

impl AuthConfig {
    /// Build the authorization URL the user opens in a browser.
    pub fn authorize_url(&self, pkce: &Pkce, state: &str) -> String {
        let mut url = Url::parse(&self.auth_url).expect("valid auth_url");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        url.to_string()
    }

    /// Exchange an authorization `code` for tokens.
    pub async fn exchange_code(&self, pkce: &Pkce, code: &str) -> anyhow::Result<AuthTokens> {
        self.post_token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
        ])
        .await
    }

    /// Refresh using a refresh token.
    pub async fn refresh(&self, refresh_token: &str) -> anyhow::Result<AuthTokens> {
        self.post_token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.client_id.as_str()),
        ])
        .await
    }

    async fn post_token(&self, form: &[(&str, &str)]) -> anyhow::Result<AuthTokens> {
        let body = encode_form(form);
        let resp = reqwest::Client::new()
            .post(&self.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json::<TokenResponse>().await?.into_tokens())
    }

    /// Run the full interactive loopback flow: print the URL, wait for the redirect, exchange the
    /// code, and persist tokens under `key`.
    pub async fn login(&self, store: &dyn TokenStore, key: &str) -> anyhow::Result<AuthTokens> {
        let pkce = Pkce::generate();
        let state = random_b64url(16);
        let bind = loopback_addr(&self.redirect_uri)?;
        let url = self.authorize_url(&pkce, &state);
        println!("Open this URL to authorize Cordy:\n{url}\n");

        let expected = state.clone();
        let code = tokio::task::spawn_blocking(move || wait_for_code(&bind, &expected)).await??;
        let tokens = self.exchange_code(&pkce, &code).await?;
        store.save(key, &tokens)?;
        Ok(tokens)
    }
}

/// Stored credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds at which `access_token` expires, if known.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl AuthTokens {
    pub fn is_expired(&self, now_unix: u64) -> bool {
        self.expires_at.is_some_and(|e| now_unix >= e)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn into_tokens(self) -> AuthTokens {
        let expires_at = self.expires_in.map(|secs| now_unix() + secs);
        AuthTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persistence for [`AuthTokens`].
pub trait TokenStore: Send + Sync {
    fn load(&self, key: &str) -> Option<AuthTokens>;
    fn save(&self, key: &str, tokens: &AuthTokens) -> anyhow::Result<()>;
}

/// Stores tokens as JSON files under a directory. (A keychain-backed store can replace this
/// later without touching callers.)
pub struct FileTokenStore {
    dir: PathBuf,
}

impl FileTokenStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FileTokenStore { dir: dir.into() }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

impl TokenStore for FileTokenStore {
    fn load(&self, key: &str) -> Option<AuthTokens> {
        let data = std::fs::read_to_string(self.path(key)).ok()?;
        serde_json::from_str(&data).ok()
    }

    fn save(&self, key: &str, tokens: &AuthTokens) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.path(key), serde_json::to_string_pretty(tokens)?)?;
        Ok(())
    }
}

/// A flat `name -> api key` store, persisted as a single JSON map (`~/.cordy/keys.json`). Used by
/// the in-TUI `/connect` flow so API keys added at runtime survive restarts without ever landing
/// in the (comment-preserving, VCS-committable) `config.toml`.
pub struct ApiKeyStore {
    path: PathBuf,
}

impl ApiKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ApiKeyStore { path: path.into() }
    }

    fn read(&self) -> std::collections::BTreeMap<String, String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// The stored key for `name`, if any.
    pub fn get(&self, name: &str) -> Option<String> {
        self.read().remove(name)
    }

    /// Store (or replace) the key for `name`, creating the file/dir as needed.
    pub fn set(&self, name: &str, key: &str) -> anyhow::Result<()> {
        let mut map = self.read();
        map.insert(name.to_string(), key.to_string());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&map)?)?;
        Ok(())
    }
}

/// The `host:port` to bind for the loopback listener, parsed from the redirect URI.
fn loopback_addr(redirect_uri: &str) -> anyhow::Result<String> {
    let url = Url::parse(redirect_uri)?;
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url.port().unwrap_or(80);
    Ok(format!("{host}:{port}"))
}

/// Block until the browser hits the loopback redirect, then return the `code` (validating
/// `state`). Reads one request line and writes a minimal success page.
fn wait_for_code(bind: &str, expected_state: &str) -> anyhow::Result<String> {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind(bind)?;
    let (mut stream, _) = listener.accept()?;
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let target = request
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed redirect request"))?;

    // Parse query against a dummy base.
    let url = Url::parse("http://localhost")?.join(target)?;
    let mut code = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }

    let body = "Cordy authorized. You may close this window.";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
        body.len(),
        body
    );

    if state.as_deref() != Some(expected_state) {
        anyhow::bail!("OAuth state mismatch");
    }
    code.ok_or_else(|| anyhow::anyhow!("no authorization code in redirect"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_of(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_has_pkce_params() {
        let cfg = AuthConfig {
            client_id: "cid".into(),
            auth_url: "https://auth.example/authorize".into(),
            token_url: "https://auth.example/token".into(),
            redirect_uri: "http://127.0.0.1:8788/callback".into(),
            scopes: vec!["a".into(), "b".into()],
        };
        let pkce = Pkce {
            verifier: "v".into(),
            challenge: "chal".into(),
        };
        let url = cfg.authorize_url(&pkce, "xyz");
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("scope=a+b"));
        assert!(url.contains("state=xyz"));
    }

    #[test]
    fn file_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(dir.path());
        assert!(store.load("openai").is_none());
        let tokens = AuthTokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(123),
        };
        store.save("openai", &tokens).unwrap();
        assert_eq!(store.load("openai"), Some(tokens));
    }

    #[test]
    fn expiry_check() {
        let t = AuthTokens {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: Some(100),
        };
        assert!(!t.is_expired(99));
        assert!(t.is_expired(100));
    }

    #[test]
    fn api_key_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = ApiKeyStore::new(dir.path().join("keys.json"));
        assert!(store.get("nvidia").is_none());
        store.set("nvidia", "nvapi-xxx").unwrap();
        store.set("openai", "sk-yyy").unwrap();
        assert_eq!(store.get("nvidia").as_deref(), Some("nvapi-xxx"));
        // Second write preserves the first entry.
        assert_eq!(store.get("openai").as_deref(), Some("sk-yyy"));
    }

    #[test]
    fn loopback_addr_from_redirect() {
        assert_eq!(
            loopback_addr("http://127.0.0.1:8788/callback").unwrap(),
            "127.0.0.1:8788"
        );
    }
}
