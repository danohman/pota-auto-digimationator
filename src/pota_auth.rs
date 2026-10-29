// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Signing in to pota.app (AWS Cognito, SRP) and fetching the operator's hunt counts.
//!
//! pota.app's Cognito client only allows USER_SRP_AUTH, so the password never leaves
//! this machine. `start_stats_poller` posts `Event::PotaStats` or `Event::PotaStatsError`.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::events::{Event, Post};
use crate::pota_spots::epoch_now;

pub const COGNITO_URL: &str = "https://cognito-idp.us-east-2.amazonaws.com/";
pub const POOL_ID: &str = "us-east-2_nA5jZ0klh";
pub const CLIENT_ID: &str = "7hluqct0n2nckib7i7sd5753oa";
pub const STATS_URL: &str = "https://api.pota.app/user/stats/hunted_activated_list?type=";
pub const USER_AGENT: &str = "POTA-Auto-Digimationator (WD4DAN)";
/// hunt counts only change when a log is uploaded to pota.app
pub const REFRESH_EVERY: f64 = 30.0 * 60.0;
pub const BACKOFF: [f64; 3] = [120.0, 300.0, 600.0];

/// Cognito's 3072-bit SRP group (RFC 5054), g = 2.
const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const INFO_BITS: &[u8] = b"Caldera Derived Key";

pub fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap()
}

pub fn g() -> BigUint {
    BigUint::from(2u32)
}

#[derive(Debug)]
pub enum FetchError {
    /// wrong password etc.: don't retry until it changes
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

/// Bytes of `n` as Cognito pads them: a leading 00 if the top bit is set.
pub fn pad(n: &BigUint) -> Vec<u8> {
    let mut b = n.to_bytes_be();
    if b[0] & 0x80 != 0 {
        b.insert(0, 0);
    }
    b
}

pub fn hash(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn int(data: &[u8]) -> BigUint {
    BigUint::from_bytes_be(data)
}

pub fn k() -> BigUint {
    int(&hash(&[pad(&n()), pad(&g())].concat()))
}

pub fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// (private a, public A) for the first SRP step.
pub fn srp_start(a: Option<BigUint>) -> (BigUint, BigUint) {
    let a = a.unwrap_or_else(|| {
        let mut buf = [0u8; 128];
        getrandom::getrandom(&mut buf).expect("random");
        int(&buf)
    }) % n();
    let big_a = g().modpow(&a, &n());
    (a, big_a)
}

/// The 16-byte HMAC key Cognito derives from the shared SRP secret.
pub fn srp_key(a: &BigUint, big_a: &BigUint, big_b: &BigUint, salt: &BigUint, username: &str, password: &str) -> Vec<u8> {
    let pool = POOL_ID.split_once('_').unwrap().1;
    let nn = n();
    let u = int(&hash(&[pad(big_a), pad(big_b)].concat()));
    let inner = hash(format!("{pool}{username}:{password}").as_bytes());
    let x = int(&hash(&[pad(salt), inner].concat()));
    let kgx = (k() * g().modpow(&x, &nn)) % &nn;
    let base = (big_b % &nn + &nn - kgx) % &nn;
    let s = base.modpow(&(a + &u * &x), &nn);
    let prk = hmac(&pad(&u), &pad(&s));
    hmac(&prk, &[INFO_BITS, b"\x01"].concat())[..16].to_vec()
}

/// e.g. "Sun Oct 25 12:00:00 UTC 2026": day without zero padding
pub fn timestamp(now: DateTime<Utc>) -> String {
    now.format("%a %b %-d %H:%M:%S UTC %Y").to_string()
}

pub fn srp_signature(key: &[u8], username: &str, secret_block: &str, timestamp: &str) -> Result<String, FetchError> {
    let pool = POOL_ID.split_once('_').unwrap().1;
    let b64 = base64::engine::general_purpose::STANDARD;
    let block = b64.decode(secret_block).map_err(|e| FetchError::Auth(e.to_string()))?;
    let msg = [pool.as_bytes(), username.as_bytes(), &block, timestamp.as_bytes()].concat();
    Ok(b64.encode(hmac(key, &msg)))
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
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(err) => {
                    let msg = err.get("message").and_then(Value::as_str)
                        .or_else(|| err.get("__type").and_then(Value::as_str))
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("HTTP Error {code}"));
                    Err(FetchError::Auth(msg))
                }
                Err(_) => Err(FetchError::Auth(format!("HTTP Error {code}"))),
            }
        }
        Err(e) => Err(FetchError::Net(e.to_string())),
    }
}

#[derive(Clone, Debug)]
pub struct Tokens {
    pub id_token: String,
    pub refresh_token: Option<String>,
    pub expires: f64,
}

fn tokens(result: &Value, old_refresh: Option<String>) -> Result<Tokens, FetchError> {
    let id = result.get("IdToken").and_then(Value::as_str).ok_or_else(|| FetchError::Net("no IdToken".into()))?;
    Ok(Tokens {
        id_token: id.to_string(),
        refresh_token: result.get("RefreshToken").and_then(Value::as_str).map(str::to_string).or(old_refresh),
        expires: epoch_now() + result.get("ExpiresIn").and_then(Value::as_f64).unwrap_or(3600.0) - 60.0,
    })
}

/// Sign in.
pub fn login(username: &str, password: &str) -> Result<Tokens, FetchError> {
    let (a, big_a) = srp_start(None);
    let r = cognito("InitiateAuth", json!({
        "AuthFlow": "USER_SRP_AUTH", "ClientId": CLIENT_ID,
        "AuthParameters": {"USERNAME": username, "SRP_A": big_a.to_str_radix(16)}}))?;
    if r.get("ChallengeName").and_then(Value::as_str) != Some("PASSWORD_VERIFIER") {
        return Err(FetchError::Auth(format!("unsupported sign-in step: {}", r.get("ChallengeName").unwrap_or(&Value::Null))));
    }
    let p = &r["ChallengeParameters"];
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let uid = s("USER_ID_FOR_SRP");
    let parse = |k: &str| BigUint::parse_bytes(s(k).as_bytes(), 16).ok_or_else(|| FetchError::Net(format!("bad {k}")));
    let key = srp_key(&a, &big_a, &parse("SRP_B")?, &parse("SALT")?, &uid, password);
    let ts = timestamp(Utc::now());
    let r = cognito("RespondToAuthChallenge", json!({
        "ChallengeName": "PASSWORD_VERIFIER", "ClientId": CLIENT_ID,
        "ChallengeResponses": {
            "USERNAME": uid, "PASSWORD_CLAIM_SECRET_BLOCK": s("SECRET_BLOCK"),
            "TIMESTAMP": ts,
            "PASSWORD_CLAIM_SIGNATURE": srp_signature(&key, &uid, &s("SECRET_BLOCK"), &ts)?}}))?;
    match r.get("AuthenticationResult") {
        Some(res) => tokens(res, None),
        None => Err(FetchError::Auth(format!("unsupported sign-in step: {}", r.get("ChallengeName").unwrap_or(&Value::Null)))),
    }
}

pub fn refresh(refresh_token: &str) -> Result<Tokens, FetchError> {
    let r = cognito("InitiateAuth", json!({
        "AuthFlow": "REFRESH_TOKEN_AUTH", "ClientId": CLIENT_ID,
        "AuthParameters": {"REFRESH_TOKEN": refresh_token}}))?;
    tokens(r.get("AuthenticationResult").unwrap_or(&Value::Null), Some(refresh_token.to_string()))
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
    user: String,
    password: String,
    tokens: Option<Tokens>,
    wake: bool,
    stop: bool,
    next_fetch: Option<f64>,
}

/// Signs in with the saved account and refreshes the hunt counts every 30 minutes
/// (and at once when `configure` is called). Idle without credentials.
pub struct StatsPoller {
    state: Mutex<PollerState>,
    cv: Condvar,
}

impl StatsPoller {
    pub fn configure(&self, username: &str, password: &str) {
        let mut s = self.state.lock().unwrap();
        s.user = username.trim().to_string();
        s.password = password.to_string();
        s.tokens = None;
        s.wake = true;
        self.cv.notify_all();
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap();
        s.stop = true;
        s.wake = true;
        self.cv.notify_all();
    }

    /// A valid ID token for the saved account (refreshed or signed in again as needed).
    /// Blocks on the network; call from a worker thread.
    pub fn token(&self) -> Result<String, FetchError> {
        let (user, password, mut tok) = {
            let s = self.state.lock().unwrap();
            (s.user.clone(), s.password.clone(), s.tokens.clone())
        };
        if user.is_empty() || password.is_empty() {
            return Err(FetchError::Auth("not signed in to pota.app".into()));
        }
        let id = id_token(&Api::default(), &mut tok, &user, &password)?;
        self.state.lock().unwrap().tokens = tok;
        Ok(id)
    }

    pub fn next_fetch(&self) -> Option<f64> {
        self.state.lock().unwrap().next_fetch
    }
}

pub fn start_stats_poller(post: Post) -> Arc<StatsPoller> {
    let p = Arc::new(StatsPoller {
        state: Mutex::new(PollerState { user: String::new(), password: String::new(), tokens: None, wake: false, stop: false, next_fetch: None }),
        cv: Condvar::new(),
    });
    let me = p.clone();
    std::thread::Builder::new().name("pota-stats".into()).spawn(move || run(me, post)).expect("spawn pota-stats");
    p
}

/// The functions the poller uses, swappable for tests.
pub struct Api {
    pub login: Box<dyn Fn(&str, &str) -> Result<Tokens, FetchError> + Send>,
    pub refresh: Box<dyn Fn(&str) -> Result<Tokens, FetchError> + Send>,
    pub fetch: Box<dyn Fn(&str) -> Result<HuntCounts, FetchError> + Send>,
}

impl Default for Api {
    fn default() -> Self {
        Api { login: Box::new(login), refresh: Box::new(refresh), fetch: Box::new(fetch_counts) }
    }
}

fn id_token(api: &Api, tok: &mut Option<Tokens>, user: &str, password: &str) -> Result<String, FetchError> {
    if let Some(t) = tok.as_ref() {
        if t.expires > epoch_now() {
            return Ok(t.id_token.clone());
        }
        if let Some(r) = t.refresh_token.clone() {
            if let Ok(t) = (api.refresh)(&r) {
                let id = t.id_token.clone();
                *tok = Some(t);
                return Ok(id);
            }
        }
    }
    let t = (api.login)(user, password)?;
    let id = t.id_token.clone();
    *tok = Some(t);
    Ok(id)
}

/// Fetch the counts; on 401/403 the token has expired or been revoked, so refresh it
/// (or sign in again) and retry once.
pub fn fetch_with_retry(api: &Api, tok: &mut Option<Tokens>, user: &str, password: &str) -> Result<HuntCounts, FetchError> {
    match (api.fetch)(&id_token(api, tok, user, password)?) {
        Err(FetchError::Http(code, _)) if code == 401 || code == 403 => {}
        other => return other,
    }
    let old = tok.take();
    if let Some(r) = old.and_then(|t| t.refresh_token) {
        *tok = (api.refresh)(&r).ok();
    }
    (api.fetch)(&id_token(api, tok, user, password)?)
}

fn run(me: Arc<StatsPoller>, post: Post) {
    let api = Api::default();
    let mut failures = 0usize;
    loop {
        let (user, password, mut tok) = {
            let mut s = me.state.lock().unwrap();
            if s.stop {
                return;
            }
            s.wake = false;
            (s.user.clone(), s.password.clone(), s.tokens.clone())
        };
        let mut wait: Option<f64> = None;
        if !user.is_empty() && !password.is_empty() {
            match fetch_with_retry(&api, &mut tok, &user, &password) {
                Ok(c) => {
                    post(Event::PotaStats(Arc::new(c)));
                    failures = 0;
                    wait = Some(REFRESH_EVERY);
                }
                Err(FetchError::Auth(e)) => post(Event::PotaStatsError(format!("pota.app sign-in failed: {e}"))),
                Err(e) => {
                    if matches!(e, FetchError::Http(..)) {
                        tok = None;
                    }
                    let w = BACKOFF[failures.min(BACKOFF.len() - 1)];
                    failures += 1;
                    wait = Some(w);
                    post(Event::PotaStatsError(format!("pota.app hunt counts failed ({e}); retry in {} min", (w / 60.0) as i64)));
                }
            }
        }
        let mut s = me.state.lock().unwrap();
        if !s.wake {
            s.tokens = tok; // a new configure() has dropped them otherwise
        }
        s.next_fetch = wait.map(|w| epoch_now() + w);
        let deadline = wait.map(|w| std::time::Instant::now() + Duration::from_secs_f64(w));
        while !s.wake && !s.stop {
            match deadline {
                None => s = me.cv.wait(s).unwrap(),
                Some(d) => {
                    let now = std::time::Instant::now();
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
    use chrono::TimeZone;

    #[test]
    fn client_and_server_derive_the_same_secret() {
        let (user, password) = ("uid-123", "hunter2");
        let salt = BigUint::parse_bytes(b"ABCDEF1234567890", 16).unwrap();
        let pool = POOL_ID.split_once('_').unwrap().1;
        let nn = n();
        let x = int(&hash(&[pad(&salt), hash(format!("{pool}{user}:{password}").as_bytes())].concat()));
        let v = g().modpow(&x, &nn);
        let (a, big_a) = srp_start(Some(BigUint::parse_bytes(b"12345678901234567890123456789", 10).unwrap()));
        let b = BigUint::parse_bytes(b"987654321987654321987654321", 10).unwrap();
        let big_b = (k() * &v + g().modpow(&b, &nn)) % &nn;
        let u = int(&hash(&[pad(&big_a), pad(&big_b)].concat()));
        let server_s = (&big_a * v.modpow(&u, &nn)).modpow(&b, &nn);
        let client_key = srp_key(&a, &big_a, &big_b, &salt, user, password);
        let prk = hmac(&pad(&u), &pad(&server_s));
        let server_key = hmac(&prk, &[INFO_BITS, b"\x01"].concat())[..16].to_vec();
        assert_eq!(client_key, server_key);
        assert_ne!(client_key, srp_key(&a, &big_a, &big_b, &salt, user, "wrong"));
    }

    #[test]
    fn pad_adds_zero_byte_when_top_bit_set() {
        assert_eq!(pad(&BigUint::from(0x80u32)), [0x00, 0x80]);
        assert_eq!(pad(&BigUint::from(0x7Fu32)), [0x7f]);
        assert_eq!(pad(&BigUint::from(0x123u32)), [0x01, 0x23]);
    }

    #[test]
    fn timestamp_has_unpadded_day() {
        let t = Utc.with_ymd_and_hms(2026, 10, 5, 7, 8, 9).unwrap();
        assert_eq!(timestamp(t), "Mon Oct 5 07:08:09 UTC 2026");
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
    fn unauthorized_refreshes_token_and_retries() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let c2 = calls.clone();
        let api = Api {
            login: Box::new(|_, _| panic!("should refresh, not sign in again")),
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
        assert_eq!(fetch_with_retry(&api, &mut tok, "u", "p").unwrap().park("US-1"), 1);
        assert_eq!(*calls.lock().unwrap(), ["old", "new"]);
    }
}
