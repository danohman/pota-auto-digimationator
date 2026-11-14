// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Signing in to pota.app (AWS Cognito hosted login, authorization code with PKCE) and
//! fetching the operator's hunt counts.
//!
//! The sign-in happens in the operator's own browser, so it works for every kind of
//! pota.app account (password, Google, ...) and the program never sees a password.
//! pota.app's Cognito client only accepts `http://localhost:8080/` as a loopback redirect,
//! so that port is fixed. The refresh token is what gets saved; `start_stats_poller`
//! trades it for ID tokens and posts `Event::PotaStats` or `Event::PotaStatsError`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::events::{Event, Post};
use crate::pota_spots::epoch_now;

pub const COGNITO_URL: &str = "https://cognito-idp.us-east-2.amazonaws.com/";
pub const AUTH_HOST: &str = "https://parksontheair.auth.us-east-2.amazoncognito.com";
pub const CLIENT_ID: &str = "7hluqct0n2nckib7i7sd5753oa";
/// the only loopback redirect pota.app's Cognito client allows (localhost:3000 and
/// 127.0.0.1:8080 were refused with redirect_mismatch, 2026-11-14)
pub const REDIRECT_PORT: u16 = 8080;
const REDIRECT_URI_ENC: &str = "http%3A%2F%2Flocalhost%3A8080%2F";
pub const REDIRECT_URI: &str = "http://localhost:8080/";
/// how long the browser sign-in may take
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
pub const STATS_URL: &str = "https://api.pota.app/user/stats/hunted_activated_list?type=";
pub const USER_AGENT: &str = "POTA-Auto-Digimationator (WD4DAN)";
/// hunt counts only change when a log is uploaded to pota.app
pub const REFRESH_EVERY: f64 = 30.0 * 60.0;
pub const BACKOFF: [f64; 3] = [120.0, 300.0, 600.0];

#[derive(Debug)]
pub enum FetchError {
    /// not signed in, or the saved sign-in no longer works: don't retry until the user signs in again
    Auth(String),
    /// an HTTP error status
    Http(u16, String),
    /// network trouble or a bad response
    Net(String),
}

impl From<ureq::Error> for FetchError {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Status(code, resp) => FetchError::Http(code, resp.status_text().to_string()),
            e => FetchError::Net(e.to_string()),
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FetchError::Auth(s) | FetchError::Net(s) => f.write_str(s),
            FetchError::Http(code, s) => write!(f, "HTTP Error {code}: {s}"),
        }
    }
}

/// A Cognito error body {"message": ..., "__type": ...} or OAuth {"error": ...} as text.
fn error_text(code: u16, body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|err| {
            ["message", "error_description", "error", "__type"]
                .iter()
                .find_map(|k| err.get(*k).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| format!("HTTP Error {code}"))
}

fn cognito(target: &str, body: Value) -> Result<Value, FetchError> {
    let r = ureq::post(COGNITO_URL)
        .set("Content-Type", "application/x-amz-json-1.1")
        .set("X-Amz-Target", &format!("AWSCognitoIdentityProviderService.{target}"))
        .timeout(Duration::from_secs(15))
        .send_string(&body.to_string());
    match r {
        Ok(resp) => {
            let text = resp.into_string().map_err(|e| FetchError::Net(e.to_string()))?;
            serde_json::from_str(&text).map_err(|e| FetchError::Net(e.to_string()))
        }
        Err(ureq::Error::Status(code, resp)) => Err(FetchError::Auth(error_text(code, &resp.into_string().unwrap_or_default()))),
        Err(e) => Err(FetchError::Net(e.to_string())),
    }
}

#[derive(Clone, Debug)]
pub struct Tokens {
    pub id_token: String,
    pub refresh_token: Option<String>,
    pub expires: f64,
}

impl Tokens {
    /// Only a saved refresh token: the first use trades it for an ID token.
    pub fn from_refresh(refresh_token: &str) -> Option<Tokens> {
        let r = refresh_token.trim();
        (!r.is_empty()).then(|| Tokens { id_token: String::new(), refresh_token: Some(r.to_string()), expires: 0.0 })
    }
}

fn tokens(result: &Value, old_refresh: Option<String>) -> Result<Tokens, FetchError> {
    let id = result.get("IdToken").and_then(Value::as_str).ok_or_else(|| FetchError::Net("no IdToken".into()))?;
    Ok(Tokens {
        id_token: id.to_string(),
        refresh_token: result.get("RefreshToken").and_then(Value::as_str).map(str::to_string).or(old_refresh),
        expires: epoch_now() + result.get("ExpiresIn").and_then(Value::as_f64).unwrap_or(3600.0) - 60.0,
    })
}

pub fn refresh(refresh_token: &str) -> Result<Tokens, FetchError> {
    let r = cognito("InitiateAuth", json!({
        "AuthFlow": "REFRESH_TOKEN_AUTH", "ClientId": CLIENT_ID,
        "AuthParameters": {"REFRESH_TOKEN": refresh_token}}))?;
    tokens(r.get("AuthenticationResult").unwrap_or(&Value::Null), Some(refresh_token.to_string()))
}

// --- browser sign-in (authorization code + PKCE) ----------------------------------------

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("random");
    b64url(&buf)
}

/// The S256 code challenge for a PKCE verifier (RFC 7636).
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

pub fn authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{AUTH_HOST}/oauth2/authorize?client_id={CLIENT_ID}&response_type=code&scope=openid+email+profile\
         &redirect_uri={REDIRECT_URI_ENC}&code_challenge={challenge}&code_challenge_method=S256&state={state}"
    )
}

/// What a request to the loopback listener turned out to be.
#[derive(Debug, PartialEq)]
pub enum Redirect {
    Code(String),
    /// the user said no, or the state did not match
    Denied(String),
    /// something else (a favicon request, a probe): ignore
    Other,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => match b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse the request line of the browser's redirect, e.g. `GET /?code=abc&state=xyz HTTP/1.1`.
pub fn parse_redirect(request_line: &str, state: &str) -> Redirect {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else { return Redirect::Other };
    let Some((path, query)) = target.split_once('?') else { return Redirect::Other };
    if path != "/" {
        return Redirect::Other;
    }
    let get = |k: &str| query.split('&').find_map(|kv| kv.split_once('=').filter(|(n, _)| *n == k).map(|(_, v)| percent_decode(v)));
    if let Some(e) = get("error") {
        return Redirect::Denied(get("error_description").filter(|d| !d.is_empty()).unwrap_or(e));
    }
    match (get("code"), get("state")) {
        (Some(code), Some(s)) if s == state && !code.is_empty() => Redirect::Code(code),
        (Some(_), _) => Redirect::Denied("the sign-in response did not match this request".into()),
        _ => Redirect::Other,
    }
}

fn open_browser(url: &str) -> Result<(), FetchError> {
    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
        .map_err(|e| FetchError::Net(format!("could not open your browser: {e}")))
}

const DONE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>POTA Auto-Digimationator</title></head>\
<body style=\"font-family:sans-serif;margin:3em\"><h2>You are signed in</h2>\
<p>You can close this tab and go back to POTA Auto-Digimationator.</p></body></html>";
const FAIL_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>POTA Auto-Digimationator</title></head>\
<body style=\"font-family:sans-serif;margin:3em\"><h2>Sign-in did not finish</h2>\
<p>Go back to POTA Auto-Digimationator and try again.</p></body></html>";

fn reply(mut s: TcpStream, status: &str, body: &str) {
    let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = s.flush();
}

/// Wait for the browser to come back with the code. `Ok(None)` = cancelled.
fn wait_for_code(listeners: &[TcpListener], state: &str, cancel: &AtomicBool) -> Result<Option<String>, FetchError> {
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(FetchError::Auth("timed out waiting for the browser sign-in".into()));
        }
        let mut idle = true;
        for l in listeners {
            let Ok((mut stream, _)) = l.accept() else { continue };
            idle = false;
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]);
            match parse_redirect(text.lines().next().unwrap_or(""), state) {
                Redirect::Code(c) => {
                    reply(stream, "200 OK", DONE_PAGE);
                    return Ok(Some(c));
                }
                Redirect::Denied(why) => {
                    reply(stream, "200 OK", FAIL_PAGE);
                    return Err(FetchError::Auth(why));
                }
                Redirect::Other => reply(stream, "404 Not Found", ""),
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn exchange_code(code: &str, verifier: &str) -> Result<Tokens, FetchError> {
    let r = ureq::post(&format!("{AUTH_HOST}/oauth2/token"))
        .timeout(Duration::from_secs(20))
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier),
        ]);
    let v: Value = match r {
        Ok(resp) => serde_json::from_str(&resp.into_string().map_err(|e| FetchError::Net(e.to_string()))?).map_err(|e| FetchError::Net(e.to_string()))?,
        Err(ureq::Error::Status(code, resp)) => return Err(FetchError::Auth(error_text(code, &resp.into_string().unwrap_or_default()))),
        Err(e) => return Err(FetchError::Net(e.to_string())),
    };
    let id = v.get("id_token").and_then(Value::as_str).ok_or_else(|| FetchError::Net("no id_token".into()))?;
    Ok(Tokens {
        id_token: id.to_string(),
        refresh_token: v.get("refresh_token").and_then(Value::as_str).map(str::to_string),
        expires: epoch_now() + v.get("expires_in").and_then(Value::as_f64).unwrap_or(3600.0) - 60.0,
    })
}

/// Sign in through the default browser. Blocks until done, cancelled (`Ok(None)`) or
/// timed out; call it from a worker thread.
pub fn browser_login(cancel: &AtomicBool) -> Result<Option<Tokens>, FetchError> {
    let mut listeners = vec![];
    match TcpListener::bind(("127.0.0.1", REDIRECT_PORT)) {
        Ok(l) => listeners.push(l),
        Err(e) => return Err(FetchError::Net(format!("port {REDIRECT_PORT} is in use ({e}); close whatever is using it and try again"))),
    }
    if let Ok(l) = TcpListener::bind(("::1", REDIRECT_PORT)) {
        listeners.push(l); // "localhost" may resolve to IPv6 first
    }
    for l in &listeners {
        l.set_nonblocking(true).map_err(|e| FetchError::Net(e.to_string()))?;
    }
    let verifier = random_b64(48);
    let state = random_b64(16);
    open_browser(&authorize_url(&pkce_challenge(&verifier), &state))?;
    match wait_for_code(&listeners, &state, cancel)? {
        Some(code) => exchange_code(&code, &verifier).map(Some),
        None => Ok(None),
    }
}

/// The account name (email, else Cognito username) inside an ID token, for display.
pub fn account_of(id_token: &str) -> String {
    let Some(payload) = id_token.split('.').nth(1) else { return String::new() };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else { return String::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { return String::new() };
    ["email", "cognito:username", "name"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string()
}

/// How many times the operator has hunted each park and each activator. pota.app
/// returns {"US-1234": 3, ...} and {"W1ABC": 5, ...}; activator keys may carry a /P
/// style suffix, so a lookup tries the full call first.
#[derive(Clone, Debug, Default)]
pub struct HuntCounts {
    pub parks: HashMap<String, i64>,
    pub activators: HashMap<String, i64>,
    pub fetched_at: f64,
}

fn counts(v: &Value) -> HashMap<String, i64> {
    v.as_object()
        .map(|o| o.iter().filter_map(|(k, v)| v.as_f64().map(|n| (k.to_uppercase(), n as i64))).collect())
        .unwrap_or_default()
}

impl HuntCounts {
    pub fn new(parks: &Value, activators: &Value) -> Self {
        HuntCounts { parks: counts(parks), activators: counts(activators), fetched_at: epoch_now() }
    }

    pub fn park(&self, r: &str) -> i64 {
        self.parks.get(&r.to_uppercase()).copied().unwrap_or(0)
    }

    pub fn activator(&self, call: &str, base: &str) -> i64 {
        match self.activators.get(&call.to_uppercase()).copied().unwrap_or(0) {
            0 => self.activators.get(&base.to_uppercase()).copied().unwrap_or(0),
            n => n,
        }
    }
}

fn get_stats(id_token: &str, kind: &str) -> Result<Value, FetchError> {
    let resp = ureq::get(&format!("{STATS_URL}{kind}"))
        .set("Authorization", id_token)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(20))
        .call()?;
    let text = resp.into_string().map_err(|e| FetchError::Net(e.to_string()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| FetchError::Net(e.to_string()))?;
    Ok(if v.is_object() { v } else { json!({}) })
}

pub fn fetch_counts(id_token: &str) -> Result<HuntCounts, FetchError> {
    Ok(HuntCounts::new(&get_stats(id_token, "park_counts")?, &get_stats(id_token, "activator_counts")?))
}

struct PollerState {
    tokens: Option<Tokens>,
    wake: bool,
    stop: bool,
    next_fetch: Option<f64>,
}

/// Refreshes the hunt counts every 30 minutes (and at once when `configure` is called)
/// with the saved sign-in. Idle when signed out.
pub struct StatsPoller {
    state: Mutex<PollerState>,
    cv: Condvar,
}

impl StatsPoller {
    /// A new sign-in (or none, when signed out).
    pub fn configure(&self, tokens: Option<Tokens>) {
        let mut s = self.state.lock().unwrap();
        s.tokens = tokens;
        s.wake = true;
        self.cv.notify_all();
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap();
        s.stop = true;
        s.wake = true;
        self.cv.notify_all();
    }

    /// A valid ID token for the saved sign-in (refreshed as needed).
    /// Blocks on the network; call from a worker thread.
    pub fn token(&self) -> Result<String, FetchError> {
        let mut tok = self.state.lock().unwrap().tokens.clone();
        let id = id_token(&Api::default(), &mut tok)?;
        self.state.lock().unwrap().tokens = tok;
        Ok(id)
    }

    pub fn next_fetch(&self) -> Option<f64> {
        self.state.lock().unwrap().next_fetch
    }
}

pub fn start_stats_poller(post: Post) -> Arc<StatsPoller> {
    let p = Arc::new(StatsPoller {
        state: Mutex::new(PollerState { tokens: None, wake: false, stop: false, next_fetch: None }),
        cv: Condvar::new(),
    });
    let me = p.clone();
    std::thread::Builder::new().name("pota-stats".into()).spawn(move || run(me, post)).expect("spawn pota-stats");
    p
}

/// The functions the poller uses, swappable for tests.
pub struct Api {
    pub refresh: Box<dyn Fn(&str) -> Result<Tokens, FetchError> + Send>,
    pub fetch: Box<dyn Fn(&str) -> Result<HuntCounts, FetchError> + Send>,
}

impl Default for Api {
    fn default() -> Self {
        Api { refresh: Box::new(refresh), fetch: Box::new(fetch_counts) }
    }
}

const EXPIRED: &str = "your pota.app sign-in has expired; sign in again in Settings";

fn id_token(api: &Api, tok: &mut Option<Tokens>) -> Result<String, FetchError> {
    let Some(t) = tok.as_ref() else { return Err(FetchError::Auth("not signed in to pota.app".into())) };
    if t.expires > epoch_now() && !t.id_token.is_empty() {
        return Ok(t.id_token.clone());
    }
    let Some(r) = t.refresh_token.clone() else { return Err(FetchError::Auth(EXPIRED.into())) };
    match (api.refresh)(&r) {
        Ok(t) => {
            let id = t.id_token.clone();
            *tok = Some(t);
            Ok(id)
        }
        // the server said no: the refresh token has expired or been revoked
        Err(FetchError::Auth(e)) => {
            *tok = None;
            Err(FetchError::Auth(format!("{EXPIRED} ({e})")))
        }
        Err(e) => Err(e),
    }
}

/// Fetch the counts; on 401/403 the token has expired or been revoked, so refresh it
/// and retry once.
pub fn fetch_with_retry(api: &Api, tok: &mut Option<Tokens>) -> Result<HuntCounts, FetchError> {
    match (api.fetch)(&id_token(api, tok)?) {
        Err(FetchError::Http(code, _)) if code == 401 || code == 403 => {}
        other => return other,
    }
    if let Some(t) = tok.as_mut() {
        t.expires = 0.0; // force a refresh
    }
    (api.fetch)(&id_token(api, tok)?)
}

fn run(me: Arc<StatsPoller>, post: Post) {
    let api = Api::default();
    let mut failures = 0usize;
    loop {
        let mut tok = {
            let mut s = me.state.lock().unwrap();
            if s.stop {
                return;
            }
            s.wake = false;
            s.tokens.clone()
        };
        let mut wait: Option<f64> = None;
        if tok.is_some() {
            match fetch_with_retry(&api, &mut tok) {
                Ok(c) => {
                    post(Event::PotaStats(Arc::new(c)));
                    failures = 0;
                    wait = Some(REFRESH_EVERY);
                }
                Err(FetchError::Auth(e)) => post(Event::PotaStatsError(format!("pota.app sign-in failed: {e}"))),
                Err(e) => {
                    let w = BACKOFF[failures.min(BACKOFF.len() - 1)];
                    failures += 1;
                    wait = Some(w);
                    post(Event::PotaStatsError(format!("pota.app hunt counts failed ({e}); retry in {} min", (w / 60.0) as i64)));
                }
            }
        }
        let mut s = me.state.lock().unwrap();
        if !s.wake {
            s.tokens = tok; // a new configure() has replaced them otherwise
        }
        s.next_fetch = wait.map(|w| epoch_now() + w);
        let deadline = wait.map(|w| Instant::now() + Duration::from_secs_f64(w));
        while !s.wake && !s.stop {
            match deadline {
                None => s = me.cv.wait(s).unwrap(),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        break;
                    }
                    s = me.cv.wait_timeout(s, d - now).unwrap().0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636_example() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn authorize_url_names_the_fixed_redirect() {
        let u = authorize_url("CH", "ST");
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8080%2F"));
        assert!(u.contains("code_challenge=CH&code_challenge_method=S256&state=ST"));
        assert!(u.contains(CLIENT_ID));
    }

    #[test]
    fn redirect_parsing() {
        assert_eq!(parse_redirect("GET /?code=abc%2Fd&state=xyz HTTP/1.1", "xyz"), Redirect::Code("abc/d".into()));
        assert!(matches!(parse_redirect("GET /?code=abc&state=other HTTP/1.1", "xyz"), Redirect::Denied(_)));
        assert_eq!(
            parse_redirect("GET /?error=access_denied&error_description=User+said+no&state=xyz HTTP/1.1", "xyz"),
            Redirect::Denied("User said no".into())
        );
        assert_eq!(parse_redirect("GET /favicon.ico HTTP/1.1", "xyz"), Redirect::Other);
        assert_eq!(parse_redirect("GET / HTTP/1.1", "xyz"), Redirect::Other);
        assert_eq!(parse_redirect("POST /?code=a&state=xyz HTTP/1.1", "xyz"), Redirect::Other);
        assert_eq!(parse_redirect("", "xyz"), Redirect::Other);
    }

    #[test]
    fn percent_decode_edges() {
        assert_eq!(percent_decode("a%20b+c%2"), "a b c%2");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn account_comes_from_the_id_token() {
        let payload = b64url(br#"{"email":"wd4dan@example.com","cognito:username":"Google_123"}"#);
        assert_eq!(account_of(&format!("h.{payload}.s")), "wd4dan@example.com");
        let payload = b64url(br#"{"cognito:username":"Google_123"}"#);
        assert_eq!(account_of(&format!("h.{payload}.s")), "Google_123");
        assert_eq!(account_of("garbage"), "");
    }

    #[test]
    fn lookups() {
        let c = HuntCounts::new(&json!({"us-1234": 3, "US-9": "x"}), &json!({"W1ABC/P": 2, "K2DM": 5}));
        assert_eq!(c.park("US-1234"), 3);
        assert_eq!(c.park("US-9"), 0);
        assert_eq!(c.park("US-1"), 0);
        assert_eq!(c.activator("W1ABC/P", "W1ABC"), 2);
        assert_eq!(c.activator("K2DM/P", "K2DM"), 5);
        assert_eq!(c.activator("N0CALL", "N0CALL"), 0);
    }

    #[test]
    fn saved_refresh_token_is_traded_for_an_id_token() {
        let api = Api {
            refresh: Box::new(|r| Ok(Tokens { id_token: "fresh".into(), refresh_token: Some(r.into()), expires: 9e12 })),
            fetch: Box::new(|_| unreachable!()),
        };
        let mut tok = Tokens::from_refresh(" saved ");
        assert_eq!(id_token(&api, &mut tok).unwrap(), "fresh");
        assert_eq!(tok.unwrap().refresh_token.as_deref(), Some("saved"));
        assert!(Tokens::from_refresh("  ").is_none());
    }

    #[test]
    fn rejected_refresh_token_means_signed_out() {
        let api = Api {
            refresh: Box::new(|_| Err(FetchError::Auth("Refresh Token has expired".into()))),
            fetch: Box::new(|_| unreachable!()),
        };
        let mut tok = Tokens::from_refresh("old");
        assert!(matches!(id_token(&api, &mut tok), Err(FetchError::Auth(_))));
        assert!(tok.is_none());
        assert!(matches!(id_token(&api, &mut tok), Err(FetchError::Auth(_))));
    }

    #[test]
    fn network_trouble_keeps_the_sign_in() {
        let api = Api {
            refresh: Box::new(|_| Err(FetchError::Net("offline".into()))),
            fetch: Box::new(|_| unreachable!()),
        };
        let mut tok = Tokens::from_refresh("old");
        assert!(matches!(id_token(&api, &mut tok), Err(FetchError::Net(_))));
        assert!(tok.is_some());
    }

    #[test]
    fn unauthorized_refreshes_token_and_retries() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let c2 = calls.clone();
        let api = Api {
            refresh: Box::new(|r| Ok(Tokens { id_token: "new".into(), refresh_token: Some(r.into()), expires: 9e12 })),
            fetch: Box::new(move |tok| {
                c2.lock().unwrap().push(tok.to_string());
                if tok == "old" {
                    return Err(FetchError::Http(401, "Unauthorized".into()));
                }
                Ok(HuntCounts::new(&json!({"US-1": 1}), &json!({})))
            }),
        };
        let mut tok = Some(Tokens { id_token: "old".into(), refresh_token: Some("r".into()), expires: 9e12 });
        assert_eq!(fetch_with_retry(&api, &mut tok).unwrap().park("US-1"), 1);
        assert_eq!(*calls.lock().unwrap(), ["old", "new"]);
    }
}
