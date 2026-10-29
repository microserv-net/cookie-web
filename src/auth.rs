//! Access control for the website and the API.
//!
//! Off unless `COOKIE_API_KEY` or `COOKIE_PASSWORD` is set. Then:
//! - the **website** asks for a password (`COOKIE_PASSWORD`, or the API key when no password is set)
//!   and keeps people signed in with a signed, HttpOnly session cookie (30 days);
//! - the **API** accepts the API key (`Authorization: Bearer`, `X-API-Key`, `?api_key=`) or that
//!   session cookie (so the website's own calls just work);
//! - wrong passwords are rate-limited per client address.
//!
//! Sessions are stateless (`expiry.signature`, HMAC-SHA256 with a key derived from the secrets):
//! they survive restarts, and changing the password or API key signs everyone out.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use ring::hmac;

pub const COOKIE: &str = "cw_session";
pub const SESSION_DAYS: u64 = 30;
/// Wrong passwords allowed per client within [`FAIL_WINDOW`] before logins are refused.
const MAX_FAILURES: u32 = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(15 * 60);

pub struct Auth {
    api_key: Option<String>,
    password: Option<String>,
    key: hmac::Key,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 2);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Auth {
    pub fn new(api_key: Option<String>, password: Option<String>) -> Self {
        // the signing key is derived from the secrets: changing either one signs everyone out
        let material = format!(
            "cookie-web session v1\0{}\0{}",
            api_key.as_deref().unwrap_or(""),
            password.as_deref().unwrap_or("")
        );
        let digest = ring::digest::digest(&ring::digest::SHA256, material.as_bytes());
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, digest.as_ref()),
            api_key,
            password,
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Is any access control configured?
    pub fn required(&self) -> bool {
        self.api_key.is_some() || self.password.is_some()
    }

    /// The website password: `COOKIE_PASSWORD`, or the API key when no password is set.
    fn website_secret(&self) -> Option<&str> {
        self.password.as_deref().or(self.api_key.as_deref())
    }

    /// A key or password presented to the API.
    pub fn check_key(&self, given: &str) -> bool {
        [self.api_key.as_deref(), self.password.as_deref()]
            .into_iter()
            .flatten()
            .any(|s| constant_time_eq(given.as_bytes(), s.as_bytes()))
    }

    pub fn check_password(&self, given: &str) -> bool {
        self.website_secret().is_some_and(|s| constant_time_eq(given.as_bytes(), s.as_bytes()))
    }

    /// A new session token, valid for [`SESSION_DAYS`].
    pub fn issue(&self) -> String {
        self.issue_until(now_secs() + SESSION_DAYS * 86_400)
    }

    fn issue_until(&self, expiry: u64) -> String {
        let payload = format!("v1.{expiry}");
        let sig = hmac::sign(&self.key, payload.as_bytes());
        format!("{payload}.{}", b64(sig.as_ref()))
    }

    pub fn valid_session(&self, token: &str) -> bool {
        let Some((payload, sig)) = token.rsplit_once('.') else { return false };
        let Some(expiry) = payload.strip_prefix("v1.").and_then(|e| e.parse::<u64>().ok()) else { return false };
        if expiry < now_secs() {
            return false;
        }
        let expected = b64(hmac::sign(&self.key, payload.as_bytes()).as_ref());
        constant_time_eq(sig.as_bytes(), expected.as_bytes())
    }

    /// The session token in a `Cookie` header, if any.
    pub fn session_from(cookie_header: &str) -> Option<&str> {
        cookie_header.split(';').filter_map(|c| c.trim().split_once('=')).find(|(k, _)| *k == COOKIE).map(|(_, v)| v)
    }

    /// Too many wrong passwords from this client lately?
    pub fn locked_out(&self, client: &str) -> Option<Duration> {
        let mut f = self.failures.lock().unwrap();
        f.retain(|_, (_, since)| since.elapsed() < FAIL_WINDOW);
        f.get(client).filter(|(n, _)| *n >= MAX_FAILURES).map(|(_, since)| FAIL_WINDOW.saturating_sub(since.elapsed()))
    }

    pub fn record_failure(&self, client: &str) {
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(client.to_string()).or_insert((0, Instant::now()));
        e.0 += 1;
    }

    pub fn clear_failures(&self, client: &str) {
        self.failures.lock().unwrap().remove(client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_signed_and_expire() {
        let a = Auth::new(Some("key".into()), Some("pw".into()));
        let t = a.issue();
        assert!(a.valid_session(&t));
        assert!(!a.valid_session(&format!("{t}x")));
        let forged = t.replacen("v1.", "v1.9", 1);
        assert!(!a.valid_session(&forged), "changing the expiry breaks the signature");
        assert!(!a.valid_session(&a.issue_until(now_secs() - 1)), "expired");
        // a different secret signs everyone out
        assert!(!Auth::new(Some("key".into()), Some("new".into())).valid_session(&t));
    }

    #[test]
    fn password_falls_back_to_the_api_key() {
        let only_key = Auth::new(Some("k3y".into()), None);
        assert!(only_key.check_password("k3y"));
        let both = Auth::new(Some("k3y".into()), Some("pw".into()));
        assert!(both.check_password("pw") && !both.check_password("k3y"));
        assert!(both.check_key("k3y") && both.check_key("pw"));
        assert!(!Auth::new(None, None).required());
    }

    #[test]
    fn locks_out_after_repeated_failures() {
        let a = Auth::new(Some("k".into()), None);
        for _ in 0..MAX_FAILURES {
            assert!(a.locked_out("1.2.3.4").is_none());
            a.record_failure("1.2.3.4");
        }
        assert!(a.locked_out("1.2.3.4").is_some());
        assert!(a.locked_out("5.6.7.8").is_none());
    }

    #[test]
    fn reads_the_cookie() {
        assert_eq!(Auth::session_from("a=1; cw_session=v1.9.abc; b=2"), Some("v1.9.abc"));
        assert_eq!(Auth::session_from("a=1"), None);
        assert_eq!(b64(b"hi?"), "aGk_");
    }
}
