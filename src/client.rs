use crate::dbg_msg;
use crate::oauth::{force_refresh_token, Oauth};
use crate::server::RequestExt;
use crate::utils::{format_url, read_body_limited, safe_download_filename, safe_local_redirect, Post};
use arc_swap::ArcSwap;
use cached::proc_macro::cached;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{header, Body, Request as HyperRequest, Response as HyperResponse};
use log::{error, info, trace, warn};
use percent_encoding::{percent_decode_str, percent_encode, CONTROLS};
use serde_json::Value;
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU16};
use std::sync::LazyLock;
use url::Url;
use wreq::redirect::Policy;
use wreq::{header as wreq_header, Client as WreqClient, EmulationFactory, Method, Response as WreqResponse};
use wreq_util::{Emulation, EmulationOS, EmulationOption};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

pub static CLIENT: LazyLock<WreqClient> = LazyLock::new(build_client);

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| ArcSwap::new(Oauth::unavailable().into()));

pub fn oauth_ready() -> bool {
	OAUTH_CLIENT.load().headers_map.contains_key("Authorization")
}

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

const MAX_REDDIT_REDIRECTS: usize = 10;
const MAX_REDDIT_REDIRECT_LOCATION_BYTES: usize = 8 * 1024;
const MAX_REDDIT_JSON_BYTES: usize = 16 * 1024 * 1024;
const REDDIT_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MEDIA_PROXY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

fn canonical_redirect_path(value: &str) -> Option<String> {
	let local = safe_local_redirect(&format_url(value), "", 2_048);
	(!local.is_empty()).then_some(local)
}

pub fn build_client() -> WreqClient {
	// Keeping this list short to aid in privacy.
	// The more emulations, the more unique a fingerprint each instance has.
	// But some emulations should increase evasiveness.
	let emulation = [Emulation::Chrome145, Emulation::Firefox147];
	let emulation_os = [EmulationOS::Android, EmulationOS::Windows];

	let rand = fastrand::usize(..);
	let emulation = EmulationOption::builder()
		.emulation(emulation[rand % emulation.len()])
		.emulation_os(emulation_os[rand % emulation_os.len()])
		.build()
		.emulation();

	info!("Building Wreq client with random emulation {:?}", emulation);
	WreqClient::builder()
		.emulation(emulation)
		.redirect(Policy::none())
		.build()
		.expect("Should always be able to build a client")
}

/// Gets the canonical path for a resource on Reddit. This is accomplished by
/// making a `HEAD` request to Reddit at the path given in `path`.
///
/// This function returns `Ok(Some(path))`, where `path`'s value is identical
/// to that of the value of the argument `path`, if Reddit responds to our
/// `HEAD` request with a 2xx-family HTTP code. It will also return an
/// `Ok(Some(String))` if Reddit responds to our `HEAD` request with a
/// `Location` header in the response, and the HTTP code is in the 3xx-family;
/// the `String` will contain the path as reported in `Location`. The return
/// value is `Ok(None)` if Reddit responded with a 3xx, but did not provide a
/// `Location` header. An `Err(String)` is returned if Reddit responds with a
/// 429, or if we were unable to decode the value in the `Location` header.
#[cached(size = 1024, time = 600, result = true)]
#[async_recursion::async_recursion]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	if tries <= 0 {
		return Ok(None);
	}

	let res = {
		let mut res = None;
		for (url_base, url_base_host) in URL_PAIRS {
			res = reddit_short_head(path.clone(), true, url_base, url_base_host).await.ok();
			if let Some(res) = &res {
				if !res.status().is_client_error() {
					break;
				}
			}
		}
		res
	};

	let res = res.ok_or_else(|| "Unable to make HEAD request to Reddit.".to_string())?;
	let status = res.status().as_u16();
	let policy_error = res.headers().get(wreq_header::RETRY_AFTER).is_some();

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(canonical_redirect_path(&path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers().get(wreq_header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err("Unable to decode Location header.".to_string());
				};
				if original.len() > MAX_REDDIT_REDIRECT_LOCATION_BYTES {
					return Err("Reddit returned an oversized redirect location".to_string());
				}

				// We need to strip the .json suffix from the original path.
				// In addition, we want to remove share parameters.
				// Cut it off here instead of letting it propagate all the way
				// to main.rs
				let stripped_uri = original.strip_suffix(".json").unwrap_or(original).split('?').next().unwrap_or_default();

				// The reason why we now have to format_url, is because the new OAuth
				// endpoints seem to return full paths, instead of relative paths.
				// So we need to strip the .json suffix from the original path, and
				// also remove all Reddit domain parts with format_url.
				// Otherwise, it will literally redirect to Reddit.com.
				let Some(uri) = canonical_redirect_path(stripped_uri) else {
					return Err("Reddit returned a redirect outside the local Vale origin.".to_string());
				};

				// Decrement tries and try again
				canonical_path(uri, tries - 1).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err("Too many requests.".to_string()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err("Too many requests.".to_string()),

		_ => Ok(res.headers().get(wreq_header::LOCATION).and_then(|val| {
			let location = percent_encode(val.as_bytes(), CONTROLS).to_string();
			canonical_redirect_path(location.trim_start_matches(REDDIT_URL_BASE))
		})),
	}
}

pub async fn proxy(req: HyperRequest<Body>, format: &str) -> Result<HyperResponse<Body>, String> {
	let (upstream_query, download_name) = split_proxy_query(req.uri().query().unwrap_or_default());
	let mut url = if upstream_query.is_empty() {
		format.to_string()
	} else {
		format!("{format}?{upstream_query}")
	};

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
	}

	// Only the fixed Reddit media hosts used by the route table may be reached.
	let wreq_uri = validated_proxy_uri(&url)?;

	let mut builder = CLIENT.get(wreq_uri).timeout(MEDIA_PROXY_TIMEOUT);

	// Copy useful headers from original request
	for &key in &["Range", "If-Modified-Since", "If-None-Match", "Cache-Control"] {
		if let Some(value) = req.headers().get(key) {
			builder = builder.header(key, value.as_bytes());
		}
	}

	// Add User-Agent header of the currently spoofed device
	{
		let client = OAUTH_CLIENT.load_full();
		builder = builder.header("User-Agent", client.user_agent());
	}

	// Reddit's preview CDN negotiates substantially smaller WebP assets when it
	// sees an image Accept header. Other media retains the broad Accept value
	// required to avoid Reddit's HTML media landing page.
	let accepts_optimized_image = download_name.is_none() && matches!(req.uri().path(), path if path.starts_with("/preview/") || path.starts_with("/thumb/"));
	builder = builder.header(
		wreq_header::ACCEPT,
		if accepts_optimized_image {
			"image/avif,image/webp,image/apng,image/*,*/*;q=0.8"
		} else {
			"*/*"
		},
	);

	let mut res = builder.send().await.map_err(|error| error.to_string())?;
	if res.status().is_redirection() {
		return Err("Reddit redirected a proxied media request; Vale did not expose the browser to that destination.".to_string());
	}
	let headers = res.headers_mut();
	for key in [
		"access-control-expose-headers",
		"alt-svc",
		"connection",
		"content-security-policy",
		"keep-alive",
		"location",
		"nel",
		"proxy-authenticate",
		"report-to",
		"reporting-endpoints",
		"server",
		"set-cookie",
		"set-cookie2",
		"strict-transport-security",
		"transfer-encoding",
		"upgrade",
		"www-authenticate",
		"x-cdn",
		"x-cdn-client-region",
		"x-cdn-name",
		"x-cdn-server-region",
		"x-reddit-cdn",
		"x-reddit-video-features",
	] {
		headers.remove(key);
	}

	let mut response = res.into_hyper_response();
	if let Some(filename) = download_name {
		let filename = safe_download_filename(&filename, "vale-download");
		if let Ok(value) = header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
			response.headers_mut().insert(header::CONTENT_DISPOSITION, value);
		}
	}
	Ok(response)
}

fn validated_proxy_uri(value: &str) -> Result<wreq::Uri, String> {
	let parsed = Url::parse(value).map_err(|_| "Vale could not parse that media address.".to_string())?;
	if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() || parsed.port_or_known_default() != Some(443) {
		return Err("Vale refused an invalid proxied media destination.".to_string());
	}
	let host = parsed.host_str().unwrap_or_default();
	if !matches!(
		host,
		"v.redd.it"
			| "i.redd.it"
			| "a.thumbs.redditmedia.com"
			| "b.thumbs.redditmedia.com"
			| "emoji.redditmedia.com"
			| "reddit-econ-prod-assets-permanent.s3.amazonaws.com"
			| "preview.redd.it"
			| "external-preview.redd.it"
			| "styles.redditmedia.com"
			| "www.redditstatic.com"
	) {
		return Err("Vale refused an unrecognized proxied media host.".to_string());
	}
	wreq::Uri::try_from(parsed.as_str()).map_err(|_| "Vale could not parse that media address.".to_string())
}

fn split_proxy_query(query: &str) -> (String, Option<String>) {
	let mut upstream = Vec::new();
	let mut download = None;
	for component in query.split('&').filter(|component| !component.is_empty()) {
		let (key, value) = component.split_once('=').unwrap_or((component, ""));
		if key == "download" {
			download = percent_decode_str(value).decode_utf8().ok().map(|value| value.into_owned());
		} else {
			upstream.push(component);
		}
	}
	(upstream.join("&"), download)
}

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
fn reddit_get(path: String, quarantine: bool) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::GET, path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST)
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::HEAD, path, false, quarantine, base_path, host)
}

/// Makes a request to Reddit. If `redirect` is `true`, `request_with_redirect`
/// will recurse on the URL that Reddit provides in the Location HTTP header
/// in its response.
fn request(method: &'static Method, path: String, redirect: bool, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	request_with_redirects(method, path, redirect, quarantine, base_path, host, 0)
}

fn request_with_redirects(
	method: &'static Method,
	path: String,
	redirect: bool,
	quarantine: bool,
	base_path: &'static str,
	host: &'static str,
	redirects: usize,
) -> Boxed<Result<WreqResponse, String>> {
	// Build Reddit URL from path.
	let url = format!("{base_path}{path}");

	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), host.into()),
		(
			"Cookie".into(),
			if quarantine {
				"_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D".into()
			} else {
				"".into()
			},
		),
	];

	{
		let client = OAUTH_CLIENT.load_full();
		if !client.headers_map.contains_key("Authorization") {
			return async { Err("Reddit access is temporarily unavailable while Vale obtains a fresh OAuth token.".to_string()) }.boxed();
		}
		for (key, value) in client.headers_map.clone() {
			headers.push((key, value));
		}
	}

	// shuffle headers: https://github.com/redlib-org/redlib/issues/324
	fastrand::shuffle(&mut headers);

	let mut builder = CLIENT.request(method.clone(), &url).timeout(REDDIT_API_TIMEOUT);

	for (key, value) in headers {
		builder = builder.header(key, value);
	}

	async move {
		match builder.send().await {
			Ok(response) => {
				// Reddit may respond with a 3xx. Decide whether or not to
				// redirect based on caller params.
				if response.status().is_redirection() {
					if !redirect {
						return Ok(response);
					};
					if redirects >= MAX_REDDIT_REDIRECTS {
						return Err("Reddit returned too many redirects".to_string());
					}
					let location = response
						.headers()
						.get(wreq::header::LOCATION)
						.and_then(|value| value.to_str().ok())
						.ok_or_else(|| "Reddit returned a redirect without a valid Location header".to_string())?
						.to_string();
					if location == ALTERNATIVE_REDDIT_URL_BASE {
						return Err("Reddit response was invalid".to_string());
					}
					if location.len() > MAX_REDDIT_REDIRECT_LOCATION_BYTES {
						return Err("Reddit returned an oversized redirect location".to_string());
					}
					let new_path = percent_encode(location.as_bytes(), CONTROLS)
						.to_string()
						.trim_start_matches(REDDIT_URL_BASE)
						.trim_start_matches(ALTERNATIVE_REDDIT_URL_BASE)
						.to_string();
					return request_with_redirects(
						method,
						format!("{new_path}{}raw_json=1", if new_path.contains('?') { "&" } else { "?" }),
						true,
						quarantine,
						base_path,
						host,
						redirects + 1,
					)
					.await;
				};

				Ok(response)
			}
			Err(e) => {
				dbg_msg!("{method} {REDDIT_URL_BASE}{path}: {}", e);

				Err(e.to_string())
			}
		}
	}
	.boxed()
}

/// Make a request to a Reddit API and parse the JSON response
#[cached(size = 100, time = 30, result = true)]
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	// First, handle rolling over the OAUTH_CLIENT if need be.
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if current_rate_limit < 10 && !is_rolling_over {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	OAUTH_RATELIMIT_REMAINING.fetch_sub(1, Ordering::SeqCst);

	// Fetch the url...
	match reddit_get(path.clone(), quarantine).await {
		Ok(response) => {
			let status = response.status();

			let reset: Option<String> = if let (Some(remaining), Some(reset), Some(used)) = (
				response.headers().get("x-ratelimit-remaining").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				response.headers().get("x-ratelimit-reset").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				response.headers().get("x-ratelimit-used").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
			) {
				trace!(
					"Ratelimit remaining: Header says {remaining}, we have {current_rate_limit}. Resets in {reset}. Rollover: {}. Ratelimit used: {used}",
					if is_rolling_over { "yes" } else { "no" },
				);

				// If can parse remaining as a float, round to a u16 and save
				if let Ok(val) = remaining.parse::<f32>() {
					OAUTH_RATELIMIT_REMAINING.store(val.round() as u16, Ordering::SeqCst);
				}

				Some(reset)
			} else {
				None
			};

			// Read the upstream body with a hard limit before parsing JSON.
			let mut response = response.into_hyper_response();
			match read_body_limited(response.body_mut(), MAX_REDDIT_JSON_BYTES, "Reddit response body exceeds the configured limit").await {
				Ok(body) => {
					if body.is_empty() {
						// Rate limited, so spawn a force_refresh_token()
						tokio::spawn(force_refresh_token());
						return match reset {
							Some(val) => Err(format!(
								"Reddit rate limit exceeded. Try refreshing in a few seconds.\
								 Rate limit will reset in: {val}"
							)),
							None => Err("Reddit rate limit exceeded".to_string()),
						};
					}

					// Parse the response from Reddit as JSON
					match serde_json::from_slice(&body) {
						Ok(value) => {
							let json: Value = value;

							// If user is suspended
							if let Some(data) = json.get("data") {
								if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
									if is_suspended {
										return Err("suspended".into());
									}
								}
							}

							// If Reddit returned an error
							if json["error"].is_i64() {
								// OAuth token has expired; http status 401
								if json["message"] == "Unauthorized" {
									error!("Forcing a token refresh");
									force_refresh_token().await;
									return Err("OAuth token has expired. Please refresh the page!".to_string());
								}

								// Handle quarantined
								if json["reason"] == "quarantined" {
									return Err("quarantined".into());
								}
								// Handle gated
								if json["reason"] == "gated" {
									return Err("gated".into());
								}
								// Handle private subs
								if json["reason"] == "private" {
									return Err("private".into());
								}
								// Handle banned subs
								if json["reason"] == "banned" {
									return Err("banned".into());
								}

								Err(format!("Reddit error {} \"{}\": {} | {path}", json["error"], json["reason"], json["message"]))
							} else {
								Ok(json)
							}
						}
						Err(e) => {
							error!("Got an invalid response from reddit {e}. Status code: {status}");
							if status.is_server_error() {
								Err("Reddit is having issues, check if there's an outage".to_string())
							} else {
								err("Failed to parse page JSON data", e.to_string(), path)
							}
						}
					}
				}
				Err(e) => err("Failed receiving body from Reddit", e, path),
			}
		}
		Err(e) => err("Couldn't send request to Reddit", e, path),
	}
}

async fn self_check(sub: &str) -> Result<(), String> {
	let query = format!("/r/{sub}/hot.json?&raw_json=1");

	match Post::fetch(&query, true).await {
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	}
}

pub async fn rate_limit_check() -> Result<(), String> {
	// Reddit does not promise a fixed per-IP remainder. Verify that both the
	// current and a freshly acquired mobile-compatible token can make a request
	// without exhausting the observed allowance.
	self_check("reddit").await?;
	let first_remaining = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	if first_remaining == 0 {
		return Err("Reddit reported no remaining requests for the current compatibility token".to_string());
	}
	if !force_refresh_token().await {
		return Err("Vale could not refresh the compatibility token during the rate-limit check".to_string());
	}
	self_check("rust").await?;
	let refreshed_remaining = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	if refreshed_remaining == 0 {
		return Err("Reddit reported no remaining requests after refreshing the compatibility token".to_string());
	}

	Ok(())
}

trait IntoHyperResponse {
	fn into_hyper_response(self) -> HyperResponse<Body>;
}

impl IntoHyperResponse for WreqResponse {
	fn into_hyper_response(self) -> HyperResponse<Body> {
		let status = self.status();
		let version = self.version();

		let mut builder = HyperResponse::builder().status(status.as_u16()).version(match version {
			wreq::Version::HTTP_09 => hyper::Version::HTTP_09,
			wreq::Version::HTTP_10 => hyper::Version::HTTP_10,
			wreq::Version::HTTP_11 => hyper::Version::HTTP_11,
			wreq::Version::HTTP_2 => hyper::Version::HTTP_2,
			wreq::Version::HTTP_3 => hyper::Version::HTTP_3,
			_ => hyper::Version::HTTP_11,
		});

		for (name, value) in self.headers() {
			builder = builder.header(
				header::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
				header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
			);
		}

		builder.body(Body::wrap_stream(self.bytes_stream())).unwrap()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use {crate::config::get_setting, sealed_test::prelude::*};

	const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";

	#[test]
	fn download_metadata_never_reaches_reddit() {
		assert_eq!(
			split_proxy_query("width=1080&format=pjpg&auto=webp&download=vale_post_image.jpg&s=signature"),
			("width=1080&format=pjpg&auto=webp&s=signature".to_string(), Some("vale_post_image.jpg".to_string()))
		);
		assert_eq!(split_proxy_query("source=fallback"), ("source=fallback".to_string(), None));
	}

	#[test]
	fn media_proxy_destinations_are_fixed_to_reviewed_reddit_hosts() {
		assert!(validated_proxy_uri("https://preview.redd.it/image.jpg?width=320").is_ok());
		assert!(validated_proxy_uri("https://a.thumbs.redditmedia.com/image.jpg").is_ok());
		assert!(validated_proxy_uri("https://example.com/image.jpg").is_err());
		assert!(validated_proxy_uri("https://reader@i.redd.it/image.jpg").is_err());
		assert!(validated_proxy_uri("http://i.redd.it/image.jpg").is_err());
	}

	#[test]
	fn canonical_redirects_never_leave_the_vale_origin() {
		assert_eq!(
			canonical_redirect_path("https://www.reddit.com/r/rust/comments/example"),
			Some("/r/rust/comments/example".to_string())
		);
		assert_eq!(canonical_redirect_path("/r/rust"), Some("/r/rust".to_string()));
		assert_eq!(canonical_redirect_path("//example.com"), None);
		assert_eq!(canonical_redirect_path("https://example.com/post"), None);
	}

	#[tokio::test]
	async fn canonical_path_stops_at_zero_or_negative_redirect_budget() {
		assert_eq!(canonical_path("/r/rust".to_string(), 0).await, Ok(None));
		assert_eq!(canonical_path("/r/rust".to_string(), -1).await, Ok(None));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_rate_limit_check() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		rate_limit_check().await.unwrap();
	}

	#[test]
	#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
	fn test_default_subscriptions() {
		let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
		assert_eq!(subscriptions.as_deref(), Some("rust"));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_localization_popular() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let val = json(POPULAR_URL.to_string(), false).await.unwrap();
		assert_eq!("GLOBAL", val["data"]["geo_filter"].as_str().unwrap());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_obfuscated_share_link() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let share_link = "/r/rust/s/kPgq8WNHRK".into();
		// Correct link without share parameters
		let canonical_link = "/r/rust/comments/18t5968/why_use_tuple_struct_over_standard_struct/kfbqlbc/".into();
		assert_eq!(canonical_path(share_link, 3).await, Ok(Some(canonical_link)));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_private_sub() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("private".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_banned_sub() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("banned".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_gated_sub() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		// quarantine to false to specifically catch when we _don't_ catch it
		let link = json("/r/drugs/about.json?raw_json=1".into(), false).await;
		assert!(link.is_err());
		assert_eq!(link, Err("gated".into()));
	}
}
