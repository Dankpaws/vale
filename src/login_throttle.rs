//! Sign-in failure throttling.
//!
//! Failures are counted per (client address, username) pair and per client
//! address. Someone guessing passwords from one address is stopped after a
//! few attempts at one account and a few dozen overall, while the account
//! owner signing in from a different address is unaffected. The table is
//! bounded; when full, the least recently active entries are evicted instead
//! of refusing new sign-ins.

use std::{
	collections::HashMap,
	net::IpAddr,
	sync::{LazyLock, Mutex},
	time::{Duration, Instant},
};

pub const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Failures allowed for one username from one client address per window.
pub const PAIR_LIMIT: usize = 8;
/// Failures allowed from one client address across all usernames per window.
pub const CLIENT_LIMIT: usize = 32;
const MAX_KEYS: usize = 4_096;
const MAX_USERNAME_CHARS: usize = 64;

#[derive(Default)]
pub struct LoginThrottle {
	failures: HashMap<String, Vec<Instant>>,
}

static THROTTLE: LazyLock<Mutex<LoginThrottle>> = LazyLock::new(|| Mutex::new(LoginThrottle::default()));

/// A stable throttling key for a client address. IPv6 clients are grouped by
/// /64, the smallest block normally assigned to one network.
pub fn client_key(address: Option<IpAddr>) -> String {
	match address {
		Some(IpAddr::V4(v4)) => v4.to_string(),
		Some(IpAddr::V6(v6)) => {
			let segments = v6.segments();
			format!("{:x}:{:x}:{:x}:{:x}::/64", segments[0], segments[1], segments[2], segments[3])
		}
		None => "unknown".to_string(),
	}
}

pub fn username_key(username: &str) -> String {
	username.chars().take(MAX_USERNAME_CHARS).collect::<String>().to_ascii_lowercase()
}

fn pair_key(client: &str, username: &str) -> String {
	format!("pair\u{0}{client}\u{0}{}", username_key(username))
}

fn client_only_key(client: &str) -> String {
	format!("client\u{0}{client}")
}

impl LoginThrottle {
	fn recent(&self, key: &str, now: Instant) -> usize {
		self
			.failures
			.get(key)
			.map_or(0, |attempts| attempts.iter().filter(|attempt| now.duration_since(**attempt) < WINDOW).count())
	}

	fn prune(&mut self, now: Instant) {
		self.failures.retain(|_, attempts| {
			attempts.retain(|attempt| now.duration_since(*attempt) < WINDOW);
			!attempts.is_empty()
		});
	}

	pub fn allowed(&mut self, client: &str, username: &str, now: Instant) -> bool {
		self.prune(now);
		self.recent(&pair_key(client, username), now) < PAIR_LIMIT && self.recent(&client_only_key(client), now) < CLIENT_LIMIT
	}

	pub fn record_failure(&mut self, client: &str, username: &str, now: Instant) {
		for (key, limit) in [(pair_key(client, username), PAIR_LIMIT), (client_only_key(client), CLIENT_LIMIT)] {
			let attempts = self.failures.entry(key).or_default();
			attempts.push(now);
			// Only the newest `limit` failures can affect a decision.
			if attempts.len() > limit {
				let excess = attempts.len() - limit;
				attempts.drain(..excess);
			}
		}
		while self.failures.len() > MAX_KEYS {
			let Some(oldest) = self.failures.iter().min_by_key(|(_, attempts)| attempts.last().copied()).map(|(key, _)| key.clone()) else {
				break;
			};
			self.failures.remove(&oldest);
		}
	}

	/// A successful sign-in clears that pair's failures. The per-client count
	/// is left to expire so signing in to one account cannot reset guessing
	/// against the others.
	pub fn record_success(&mut self, client: &str, username: &str) {
		self.failures.remove(&pair_key(client, username));
	}
}

fn with_throttle<T>(work: impl FnOnce(&mut LoginThrottle) -> T) -> T {
	work(&mut THROTTLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

pub fn allowed(client: &str, username: &str) -> bool {
	with_throttle(|throttle| throttle.allowed(client, username, Instant::now()))
}

pub fn record_failure(client: &str, username: &str) {
	with_throttle(|throttle| throttle.record_failure(client, username, Instant::now()));
}

pub fn record_success(client: &str, username: &str) {
	with_throttle(|throttle| throttle.record_success(client, username));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn guessing_one_account_blocks_only_that_client_and_account() {
		let mut throttle = LoginThrottle::default();
		let now = Instant::now();
		for _ in 0..PAIR_LIMIT {
			assert!(throttle.allowed("198.51.100.4", "owner", now));
			throttle.record_failure("198.51.100.4", "owner", now);
		}
		assert!(!throttle.allowed("198.51.100.4", "owner", now));
		// Username matching is case-insensitive.
		assert!(!throttle.allowed("198.51.100.4", "OWNER", now));
		// The owner signing in from elsewhere is unaffected.
		assert!(throttle.allowed("192.0.2.10", "owner", now));
		// The guessing client can still reach other accounts until its overall limit.
		assert!(throttle.allowed("198.51.100.4", "reader", now));
	}

	#[test]
	fn spraying_usernames_blocks_only_the_spraying_client() {
		let mut throttle = LoginThrottle::default();
		let now = Instant::now();
		for index in 0..CLIENT_LIMIT {
			throttle.record_failure("198.51.100.4", &format!("name{index}"), now);
		}
		assert!(!throttle.allowed("198.51.100.4", "owner", now));
		assert!(throttle.allowed("192.0.2.10", "owner", now));
	}

	#[test]
	fn failures_expire_after_the_window() {
		let mut throttle = LoginThrottle::default();
		let start = Instant::now();
		for _ in 0..PAIR_LIMIT {
			throttle.record_failure("198.51.100.4", "owner", start);
		}
		assert!(!throttle.allowed("198.51.100.4", "owner", start));
		assert!(throttle.allowed("198.51.100.4", "owner", start + WINDOW));
		assert!(throttle.failures.is_empty());
	}

	#[test]
	fn success_clears_the_pair_but_not_the_client_budget() {
		let mut throttle = LoginThrottle::default();
		let now = Instant::now();
		for index in 0..CLIENT_LIMIT - 1 {
			throttle.record_failure("198.51.100.4", &format!("name{index}"), now);
		}
		throttle.record_failure("198.51.100.4", "owner", now);
		throttle.record_success("198.51.100.4", "owner");
		assert!(!throttle.allowed("198.51.100.4", "owner", now));
	}

	#[test]
	fn a_full_table_evicts_instead_of_refusing_new_clients() {
		let mut throttle = LoginThrottle::default();
		let start = Instant::now();
		for index in 0..MAX_KEYS {
			throttle.record_failure(&format!("client{index}"), "owner", start + Duration::from_millis(index as u64));
		}
		assert!(throttle.failures.len() <= MAX_KEYS);
		let later = start + Duration::from_secs(60);
		assert!(throttle.allowed("192.0.2.10", "owner", later));
		throttle.record_failure("192.0.2.10", "owner", later);
		assert!(throttle.failures.len() <= MAX_KEYS);
		assert!(throttle.failures.contains_key(&pair_key("192.0.2.10", "owner")));
	}

	#[test]
	fn keys_are_bounded_and_ipv6_is_grouped_by_prefix() {
		assert_eq!(username_key(&"A".repeat(500)).len(), MAX_USERNAME_CHARS);
		let first = client_key(Some("2001:db8:1:2:aaaa::1".parse().unwrap()));
		let second = client_key(Some("2001:db8:1:2:bbbb::9".parse().unwrap()));
		assert_eq!(first, second);
		assert_ne!(first, client_key(Some("2001:db8:1:3::1".parse().unwrap())));
		assert_eq!(client_key(None), "unknown");
	}
}
