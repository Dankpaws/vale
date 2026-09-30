//! Client network identity: the connection peer, the configured trusted
//! reverse proxies, and the client address and scheme derived from them.
//!
//! Forwarding headers (`X-Forwarded-For`, `X-Forwarded-Proto`) can be sent by
//! anyone who reaches the listener, so Vale only reads a client address from
//! them when the connection comes from an address listed in
//! `VALE_TRUSTED_PROXIES`.

use crate::config::get_setting;
use hyper::{Body, Request};
use log::warn;
use std::{
	net::{IpAddr, SocketAddr},
	sync::{
		atomic::{AtomicBool, Ordering},
		LazyLock,
	},
};

/// The TCP peer of the connection a request arrived on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerAddr(pub SocketAddr);

/// An IP network in CIDR form. A bare address is a single-host network.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpNetwork {
	address: IpAddr,
	prefix: u8,
}

impl IpNetwork {
	pub fn parse(value: &str) -> Option<Self> {
		let value = value.trim();
		let (address, prefix) = match value.split_once('/') {
			Some((address, prefix)) => (address.parse::<IpAddr>().ok()?, Some(prefix.parse::<u8>().ok()?)),
			None => (value.parse::<IpAddr>().ok()?, None),
		};
		let maximum = if address.is_ipv4() { 32 } else { 128 };
		let prefix = prefix.unwrap_or(maximum);
		(prefix <= maximum).then_some(Self { address, prefix })
	}

	pub fn contains(&self, candidate: IpAddr) -> bool {
		match (self.address, canonical(candidate)) {
			(IpAddr::V4(network), IpAddr::V4(candidate)) => {
				let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
				u32::from(network) & mask == u32::from(candidate) & mask
			}
			(IpAddr::V6(network), IpAddr::V6(candidate)) => {
				let mask = u128::MAX.checked_shl(128 - u32::from(self.prefix)).unwrap_or(0);
				u128::from(network) & mask == u128::from(candidate) & mask
			}
			_ => false,
		}
	}
}

/// Treats IPv4-mapped IPv6 peers (`::ffff:a.b.c.d`) as IPv4.
fn canonical(address: IpAddr) -> IpAddr {
	match address {
		IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
		IpAddr::V4(_) => address,
	}
}

/// Parses a comma- or whitespace-separated list of addresses and CIDR
/// networks. Invalid entries are reported and ignored.
pub fn parse_networks(value: &str) -> Vec<IpNetwork> {
	value
		.split(|character: char| character == ',' || character.is_whitespace())
		.filter(|entry| !entry.is_empty())
		.filter_map(|entry| {
			let network = IpNetwork::parse(entry);
			if network.is_none() {
				warn!("Ignoring invalid VALE_TRUSTED_PROXIES entry {entry:?}; use an IP address or CIDR network.");
			}
			network
		})
		.collect()
}

static TRUSTED_PROXIES: LazyLock<Option<Vec<IpNetwork>>> = LazyLock::new(|| {
	get_setting("VALE_TRUSTED_PROXIES")
		.filter(|value| !value.trim().is_empty())
		.map(|value| parse_networks(&value))
});

static UNTRUSTED_FORWARDING_WARNED: AtomicBool = AtomicBool::new(false);

pub fn peer_ip(request: &Request<Body>) -> Option<IpAddr> {
	request.extensions().get::<PeerAddr>().map(|peer| canonical(peer.0.ip()))
}

/// Whether `VALE_TRUSTED_PROXIES` is configured at all.
pub fn trusted_proxies_configured() -> bool {
	TRUSTED_PROXIES.is_some()
}

fn is_trusted(networks: &[IpNetwork], address: IpAddr) -> bool {
	networks.iter().any(|network| network.contains(address))
}

fn header_value<'a>(request: &'a Request<Body>, name: &str) -> Option<&'a str> {
	request.headers().get(name).and_then(|value| value.to_str().ok())
}

/// Resolves the client address given an explicit trusted-proxy list.
///
/// When the peer is a trusted proxy, `X-Forwarded-For` is read from the right
/// and the first address that is not itself a trusted proxy is the client.
/// Otherwise the peer is the client.
pub fn client_ip_with(request: &Request<Body>, trusted: Option<&[IpNetwork]>) -> Option<IpAddr> {
	let peer = peer_ip(request)?;
	let Some(trusted) = trusted.filter(|trusted| is_trusted(trusted, peer)) else {
		return Some(peer);
	};
	let forwarded = request
		.headers()
		.get_all("x-forwarded-for")
		.iter()
		.filter_map(|value| value.to_str().ok())
		.collect::<Vec<_>>();
	let mut client = peer;
	for hop in forwarded.iter().rev().flat_map(|value| value.rsplit(',')) {
		let Ok(address) = hop.trim().parse::<IpAddr>() else {
			// An unparseable hop ends the trustworthy part of the chain.
			break;
		};
		client = canonical(address);
		if !is_trusted(trusted, client) {
			break;
		}
	}
	Some(client)
}

/// The client address for throttling and logging.
pub fn client_ip(request: &Request<Body>) -> Option<IpAddr> {
	if !trusted_proxies_configured() && request.headers().contains_key("x-forwarded-for") && !UNTRUSTED_FORWARDING_WARNED.swap(true, Ordering::Relaxed) {
		warn!(
			"Requests arrive with X-Forwarded-For but VALE_TRUSTED_PROXIES is unset. Vale ignores the header, so sign-in throttling treats every client of that proxy as one address. Set VALE_TRUSTED_PROXIES to the proxy's address."
		);
	}
	client_ip_with(request, TRUSTED_PROXIES.as_deref())
}

/// Whether forwarding headers on this request may be believed, given an
/// explicit trusted-proxy list. With no list configured, Vale keeps its
/// historical behavior and believes `X-Forwarded-Proto`; set
/// `VALE_TRUSTED_PROXIES` to restrict it to known proxies.
pub fn forwarding_trusted_with(request: &Request<Body>, trusted: Option<&[IpNetwork]>) -> bool {
	match trusted {
		None => true,
		Some(trusted) => peer_ip(request).is_some_and(|peer| is_trusted(trusted, peer)),
	}
}

/// The scheme the client used, from the request URI or a trusted
/// `X-Forwarded-Proto` header, defaulting to `http`.
pub fn request_scheme_with<'a>(request: &'a Request<Body>, trusted: Option<&[IpNetwork]>) -> &'a str {
	request
		.uri()
		.scheme_str()
		.or_else(|| {
			forwarding_trusted_with(request, trusted)
				.then(|| header_value(request, "x-forwarded-proto"))
				.flatten()
				.and_then(|value| value.split(',').next())
				.map(str::trim)
		})
		.unwrap_or("http")
}

pub fn request_scheme(request: &Request<Body>) -> &str {
	request_scheme_with(request, TRUSTED_PROXIES.as_deref())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request(peer: &str, headers: &[(&str, &str)]) -> Request<Body> {
		let mut builder = Request::builder().uri("/login");
		for (name, value) in headers {
			builder = builder.header(*name, *value);
		}
		let mut request = builder.body(Body::empty()).unwrap();
		request.extensions_mut().insert(PeerAddr(peer.parse().unwrap()));
		request
	}

	fn ip(value: &str) -> IpAddr {
		value.parse().unwrap()
	}

	#[test]
	fn networks_parse_addresses_and_prefixes() {
		let networks = parse_networks("127.0.0.1, 10.0.0.0/8 fd00::/8,bogus,10.0.0.0/33");
		assert_eq!(networks.len(), 3);
		assert!(networks[0].contains(ip("127.0.0.1")));
		assert!(!networks[0].contains(ip("127.0.0.2")));
		assert!(networks[1].contains(ip("10.200.3.4")));
		assert!(!networks[1].contains(ip("11.0.0.1")));
		assert!(networks[2].contains(ip("fd12::1")));
		assert!(!networks[2].contains(ip("10.0.0.1")));
		assert!(IpNetwork::parse("0.0.0.0/0").unwrap().contains(ip("203.0.113.9")));
	}

	#[test]
	fn mapped_ipv4_peers_match_ipv4_networks() {
		let networks = parse_networks("172.16.0.0/12");
		assert!(networks[0].contains(ip("::ffff:172.18.0.1")));
	}

	#[test]
	fn untrusted_peers_cannot_choose_their_client_address() {
		let request = request("203.0.113.7:5000", &[("x-forwarded-for", "198.51.100.1")]);
		assert_eq!(client_ip_with(&request, None), Some(ip("203.0.113.7")));
		let trusted = parse_networks("127.0.0.1");
		assert_eq!(client_ip_with(&request, Some(&trusted)), Some(ip("203.0.113.7")));
	}

	#[test]
	fn trusted_proxies_resolve_the_nearest_untrusted_hop() {
		let trusted = parse_networks("127.0.0.1, 10.0.0.0/8");
		// A client-supplied spoofed hop sits left of the real client address.
		let request = request("127.0.0.1:5000", &[("x-forwarded-for", "192.0.2.99, 198.51.100.4, 10.1.1.1")]);
		assert_eq!(client_ip_with(&request, Some(&trusted)), Some(ip("198.51.100.4")));
	}

	#[test]
	fn trusted_proxies_without_forwarding_are_the_client() {
		let trusted = parse_networks("127.0.0.1");
		let request = request("127.0.0.1:5000", &[]);
		assert_eq!(client_ip_with(&request, Some(&trusted)), Some(ip("127.0.0.1")));
	}

	#[test]
	fn forwarded_scheme_requires_a_trusted_peer_once_configured() {
		let request = request("203.0.113.7:5000", &[("x-forwarded-proto", "https")]);
		assert_eq!(request_scheme_with(&request, None), "https");
		let trusted = parse_networks("127.0.0.1");
		assert_eq!(request_scheme_with(&request, Some(&trusted)), "http");
		let proxied = self::request("127.0.0.1:5000", &[("x-forwarded-proto", "https, http")]);
		assert_eq!(request_scheme_with(&proxied, Some(&trusted)), "https");
	}
}
