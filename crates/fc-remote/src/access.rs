//! Server-side access control state: brute-force throttling and session tokens.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::Limits;
use crate::crypto::random;

/// Counts failed logins per key ("ip:…" and "user:…") and locks keys out.
///
/// * Per-IP: `max_failures` within `window` → locked for `lockout`.
/// * Per-account: 4× the per-IP threshold, which stops slow distributed guessing
///   against one account. The trade-off (an attacker can temporarily lock
///   a real user out) is the usual one; the lockout is time-limited.
///
/// Note that Argon2id runs on the *client* in SCRAM, so online guessing costs
/// the server only a few HMACs per attempt. These limits, not KDF cost, are what
/// make online brute force impractical. Argon2 protects a stolen config file.
pub struct LoginLimiter {
    limits: Limits,
    state: Mutex<HashMap<String, Entry>>,
}

#[derive(Default)]
struct Entry {
    failures: VecDeque<Instant>,
    locked_until: Option<Instant>,
}

impl LoginLimiter {
    pub fn new(limits: Limits) -> Self {
        Self { limits, state: Mutex::new(HashMap::new()) }
    }

    fn keys(ip: IpAddr, user: Option<&str>) -> Vec<(String, u32)> {
        let mut k = vec![(format!("ip:{}", ip.to_canonical()), 1)];
        if let Some(u) = user {
            k.push((format!("user:{u}"), 4));
        }
        k
    }

    /// `Err(retry_after)` if this IP (or user) is currently locked out.
    pub fn check(&self, ip: IpAddr, user: Option<&str>) -> Result<(), Duration> {
        let now = Instant::now();
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut wait = Duration::ZERO;
        for (k, _) in Self::keys(ip, user) {
            if let Some(until) = st.get(&k).and_then(|e| e.locked_until) {
                wait = wait.max(until.saturating_duration_since(now));
            }
        }
        if wait.is_zero() { Ok(()) } else { Err(wait) }
    }

    /// Record a failure. Returns the lockout duration if this failure triggered one.
    pub fn failure(&self, ip: IpAddr, user: Option<&str>) -> Option<Duration> {
        let now = Instant::now();
        let window = Duration::from_secs(self.limits.window_secs);
        let lockout = Duration::from_secs(self.limits.lockout_secs);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut locked = None;
        for (k, mult) in Self::keys(ip, user) {
            let e = st.entry(k).or_default();
            e.failures.push_back(now);
            while e.failures.front().is_some_and(|t| now.duration_since(*t) > window) {
                e.failures.pop_front();
            }
            if e.failures.len() as u32 >= self.limits.max_failures * mult {
                e.locked_until = Some(now + lockout);
                e.failures.clear();
                locked = Some(lockout);
            }
        }
        // Bound memory under a flood of distinct source addresses.
        if st.len() > 50_000 {
            st.retain(|_, e| e.locked_until.is_some_and(|u| u > now) || !e.failures.is_empty());
        }
        locked
    }

    pub fn success(&self, ip: IpAddr, user: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in Self::keys(ip, Some(user)) {
            st.remove(&k);
        }
    }
}

/// Opaque bearer tokens for reconnecting without re-running SCRAM.
pub struct SessionTokens {
    ttl: Duration,
    max: usize,
    tokens: Mutex<HashMap<[u8; 32], (String, Instant)>>,
}

impl SessionTokens {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self { ttl, max, tokens: Mutex::new(HashMap::new()) }
    }

    pub fn issue(&self, account: &str) -> [u8; 32] {
        let token = random::<32>();
        let now = Instant::now();
        let mut t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        t.retain(|_, (_, exp)| *exp > now);
        // Cap tokens per account: oldest are evicted first.
        let mut mine: Vec<([u8; 32], Instant)> = t.iter().filter(|(_, (a, _))| a == account).map(|(k, (_, e))| (*k, *e)).collect();
        if mine.len() >= self.max {
            mine.sort_by_key(|(_, e)| *e);
            for (k, _) in mine.iter().take(mine.len() + 1 - self.max) {
                t.remove(k);
            }
        }
        t.insert(token, (account.to_string(), now + self.ttl));
        token
    }

    /// Consume a token (one use each: a reconnect gets a fresh token back).
    pub fn redeem(&self, token: &[u8; 32]) -> Option<String> {
        let mut t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        match t.remove(token) {
            Some((account, exp)) if exp > Instant::now() => Some(account),
            _ => None,
        }
    }

    pub fn revoke_account(&self, account: &str) {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (a, _)| a != account);
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits { max_failures: 3, window_secs: 60, lockout_secs: 60, ..Limits::default() }
    }

    #[test]
    fn ip_locks_after_threshold_and_success_clears() {
        let l = LoginLimiter::new(limits());
        let ip: IpAddr = "192.168.1.9".parse().unwrap();
        assert!(l.check(ip, None).is_ok());
        assert!(l.failure(ip, Some("a")).is_none());
        assert!(l.failure(ip, Some("b")).is_none());
        assert!(l.failure(ip, Some("c")).is_some(), "third failure locks the IP");
        assert!(l.check(ip, None).is_err());
        let other: IpAddr = "192.168.1.10".parse().unwrap();
        assert!(l.check(other, None).is_ok(), "other IPs unaffected");
    }

    #[test]
    fn distributed_guessing_locks_the_account() {
        let l = LoginLimiter::new(limits());
        for i in 0..12u8 {
            let ip = IpAddr::from([10, 0, 0, i]);
            l.failure(ip, Some("alice"));
        }
        let fresh = IpAddr::from([10, 9, 9, 9]);
        assert!(l.check(fresh, Some("alice")).is_err(), "account locked after 4×3 failures");
        assert!(l.check(fresh, Some("bob")).is_ok());
    }

    #[test]
    fn ipv4_mapped_addresses_share_a_bucket() {
        let l = LoginLimiter::new(limits());
        for _ in 0..3 {
            l.failure("::ffff:192.168.1.5".parse().unwrap(), None);
        }
        assert!(l.check("192.168.1.5".parse().unwrap(), None).is_err());
    }

    #[test]
    fn tokens_are_single_use_and_expire() {
        let t = SessionTokens::new(Duration::from_millis(50), 4);
        let tok = t.issue("alice");
        assert_eq!(t.redeem(&tok).as_deref(), Some("alice"));
        assert_eq!(t.redeem(&tok), None, "single use");
        let tok = t.issue("alice");
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(t.redeem(&tok), None, "expired");
        assert_eq!(t.redeem(&[0; 32]), None);
    }

    #[test]
    fn token_count_per_account_is_capped() {
        let t = SessionTokens::new(Duration::from_secs(60), 2);
        let a = t.issue("u");
        let _b = t.issue("u");
        let _c = t.issue("u");
        assert_eq!(t.redeem(&a), None, "oldest evicted");
    }
}
