use crate::{
	client::{CLIENT, OAUTH_CLIENT, OAUTH_IS_ROLLING_OVER, OAUTH_RATELIMIT_REMAINING},
	oauth_resources::ANDROID_APP_VERSION_LIST,
};
use base64::{engine::general_purpose, Engine as _};
use log::{info, trace, warn};
use serde_json::{json, Value};
#[cfg(test)]
use std::sync::LazyLock;
use std::{collections::HashMap, sync::atomic::Ordering, time::Duration};
use tokio::time::timeout;

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";

const AUTH_ENDPOINT: &str = "https://www.reddit.com";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;

// Response from OAuth backend authentication
#[derive(Debug, Clone)]
pub struct OauthResponse {
	pub token: String,
	pub expires_in: u64,
	pub additional_headers: HashMap<String, String>,
}

// Spoofed client for Android devices
#[derive(Debug, Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	expires_in: u64,
	user_agent: String,
}

impl Oauth {
	pub(crate) fn unavailable() -> Self {
		let backend = MobileSpoofAuth::new();
		Self {
			headers_map: HashMap::new(),
			expires_in: 0,
			user_agent: backend.user_agent().to_string(),
		}
	}

	/// Attempt the currently verified installed-client compatibility flow once.
	/// Failure is recoverable: local Vale remains available and the token daemon
	/// retries in the background.
	async fn acquire() -> Result<Self, String> {
		let mut backend = MobileSpoofAuth::new();
		let attempt = timeout(OAUTH_TIMEOUT, async move {
			let response = backend.authenticate().await?;

			// Build headers_map from backend headers + Authorization header
			let mut headers_map = backend.get_headers();
			headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));
			headers_map.extend(response.additional_headers);

			Ok::<Self, AuthError>(Self {
				headers_map,
				expires_in: response.expires_in,
				user_agent: backend.user_agent().to_string(),
			})
		})
		.await;
		match attempt {
			Ok(Ok(oauth)) => {
				info!("[✅] Successfully created OAuth client");
				Ok(oauth)
			}
			Ok(Err(error)) => {
				warn!(
					"[⛔] OAuth mobile compatibility attempt failed: {}",
					match error {
						AuthError::Wreq(error) => error.to_string(),
						AuthError::SerdeDeserialize(error) => error.to_string(),
						AuthError::Field(field) => format!("OAuth response did not contain a valid {field} field"),
						AuthError::ResponseTooLarge => "OAuth response exceeded 64 KiB".to_string(),
					}
				);
				Err("Reddit OAuth is temporarily unavailable; Vale will retry in the background.".to_string())
			}
			Err(_) => {
				warn!("[⛔] OAuth mobile compatibility attempt timed out");
				Err("Reddit OAuth is temporarily unavailable; Vale will retry in the background.".to_string())
			}
		}
	}

	pub fn user_agent(&self) -> &str {
		&self.user_agent
	}
}

#[derive(Debug)]
enum AuthError {
	Wreq(wreq::Error),
	SerdeDeserialize(serde_json::Error),
	Field(&'static str),
	ResponseTooLarge,
}

impl From<wreq::Error> for AuthError {
	fn from(err: wreq::Error) -> Self {
		AuthError::Wreq(err)
	}
}

impl From<serde_json::Error> for AuthError {
	fn from(err: serde_json::Error) -> Self {
		AuthError::SerdeDeserialize(err)
	}
}

async fn bounded_oauth_json(mut response: wreq::Response) -> Result<Value, AuthError> {
	if response.content_length().is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64) {
		return Err(AuthError::ResponseTooLarge);
	}
	let mut bytes = Vec::with_capacity(response.content_length().unwrap_or_default().min(MAX_OAUTH_RESPONSE_BYTES as u64) as usize);
	while let Some(chunk) = response.chunk().await? {
		extend_oauth_bytes(&mut bytes, &chunk)?;
	}
	Ok(serde_json::from_slice(&bytes)?)
}

fn extend_oauth_bytes(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<(), AuthError> {
	if bytes.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
		return Err(AuthError::ResponseTooLarge);
	}
	bytes.extend_from_slice(chunk);
	Ok(())
}

pub async fn token_daemon() {
	loop {
		if !crate::client::oauth_ready() {
			if !force_refresh_token().await {
				tokio::time::sleep(Duration::from_secs(15)).await;
			}
			continue;
		}

		let expires_in = OAUTH_CLIENT.load_full().expires_in;
		let duration = Duration::from_secs(expires_in.saturating_sub(120).max(30));
		info!("[⏳] Refreshing the OAuth token after {duration:?}");
		tokio::time::sleep(duration).await;
		force_refresh_token().await;
	}
}

pub async fn force_refresh_token() -> bool {
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		trace!("Skipping refresh token roll over, already in progress");
		return false;
	}

	trace!("Rolling over refresh token. Current rate limit: {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst));
	let refreshed = match Oauth::acquire().await {
		Ok(new_client) => {
			OAUTH_CLIENT.swap(new_client.into());
			OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
			true
		}
		Err(message) => {
			warn!("{message}");
			false
		}
	};
	OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
	refreshed
}

#[cfg(test)]
pub(crate) async fn ensure_live_oauth() -> Result<(), String> {
	static LIVE_OAUTH_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
	if crate::client::oauth_ready() {
		return Ok(());
	}
	let _guard = LIVE_OAUTH_LOCK.lock().await;
	if crate::client::oauth_ready() || force_refresh_token().await {
		Ok(())
	} else {
		Err("Reddit OAuth is unavailable for this live integration test".to_string())
	}
}

#[derive(Debug, Clone, Default)]
struct Device {
	oauth_id: String,
	initial_headers: HashMap<String, String>,
	headers: HashMap<String, String>,
	user_agent: String,
}

// MobileSpoofAuth backend - spoofs an Android mobile device
#[derive(Debug, Clone)]
pub struct MobileSpoofAuth {
	device: Device,
	additional_headers: HashMap<String, String>,
}

impl MobileSpoofAuth {
	fn new() -> Self {
		Self {
			device: Device::new(),
			additional_headers: HashMap::new(),
		}
	}
}

impl MobileSpoofAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = format!("{AUTH_ENDPOINT}/auth/v2/oauth/access-token/loid");
		let mut builder = CLIENT.post(&url);

		// Add headers from spoofed client
		for (key, value) in &self.device.initial_headers {
			builder = builder.header(key, value);
		}
		// Set up HTTP Basic Auth - basically just the const OAuth ID's with no password,
		// Base64-encoded. https://en.wikipedia.org/wiki/Basic_access_authentication
		// This could be constant, but I don't think it's worth it. OAuth ID's can change
		// over time and we want to be flexible.
		let auth = general_purpose::STANDARD.encode(format!("{}:", self.device.oauth_id));
		builder = builder.header("Authorization", format!("Basic {auth}"));

		// Set JSON body. I couldn't tell you what this means. But that's what the client sends
		let json = json!({
				"scopes": ["*","email", "pii"]
		});

		trace!("Sending token request to {url}...");

		// Send request
		let resp = builder.json(&json).send().await?;

		trace!("Received response with status {} and length {:?}", resp.status(), resp.headers().get("content-length"));

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			if let Ok(value) = header.to_str() {
				self.additional_headers.insert("x-reddit-loid".to_owned(), value.to_string());
			}
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			if let Ok(value) = header.to_str() {
				self.additional_headers.insert("x-reddit-session".to_owned(), value.to_string());
			}
		}

		trace!("Serializing response...");

		// Serialize response
		let json = bounded_oauth_json(resp).await?;

		trace!("Accessing relevant fields...");

		// Save token and expiry
		let token = json
			.get("access_token")
			.ok_or(AuthError::Field("access_token"))?
			.as_str()
			.ok_or(AuthError::Field("access_token"))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or(AuthError::Field("expires_in"))?
			.as_u64()
			.ok_or(AuthError::Field("expires_in"))?;

		info!("[✅] OAuth token acquired; expires in {expires_in} seconds");

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.device.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		let mut headers = self.device.headers.clone();
		headers.extend(self.additional_headers.clone());
		headers
	}
}

impl Device {
	fn android() -> Self {
		// Generate uuid
		let uuid = uuid::Uuid::new_v4().to_string();

		// Generate random user-agent
		let android_app_version = choose(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(9..=14);

		let android_user_agent = format!("Reddit/{android_app_version}/Android {android_version}");

		let qos = fastrand::u32(1000..=100_000);
		let qos: f32 = qos as f32 / 1000.0;
		let qos = format!("{qos:.3}");

		let codecs = if fastrand::bool() {
			"available-codecs=video/avc, video/hevc, video/x-vnd.on2.vp9"
		} else {
			"available-codecs=video/avc, video/hevc"
		}
		.to_string();

		// Android device headers
		let headers: HashMap<String, String> = HashMap::from([
			("User-Agent".into(), android_user_agent.clone()),
			("x-reddit-retry".into(), "algo=no-retries".into()),
			("x-reddit-compression".into(), "1".into()),
			("x-reddit-qos".into(), qos),
			("x-reddit-media-codecs".into(), codecs),
			("Content-Type".into(), "application/json; charset=UTF-8".into()),
			("client-vendor-id".into(), uuid.clone()),
			("X-Reddit-Device-Id".into(), uuid.clone()),
		]);

		info!("[🔄] Prepared an ephemeral Android-compatible OAuth client");

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
		}
	}
	fn new() -> Self {
		// See https://github.com/redlib-org/redlib/issues/8
		Self::android()
	}
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit OAuth"]
	async fn test_mobile_spoof_backend() {
		// Test MobileSpoofAuth backend specifically
		let mut backend = MobileSpoofAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
		assert!(!backend.get_headers().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit OAuth"]
	async fn test_oauth_client() {
		// Integration test - tests the overall Oauth client
		ensure_live_oauth().await.unwrap();
		assert!(OAUTH_CLIENT.load_full().headers_map.contains_key("Authorization"));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit OAuth"]
	async fn test_oauth_client_refresh() {
		assert!(force_refresh_token().await);
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit OAuth"]
	async fn test_oauth_token_exists() {
		ensure_live_oauth().await.unwrap();
		let client = OAUTH_CLIENT.load_full();
		let auth_header = client.headers_map.get("Authorization").unwrap();
		assert!(auth_header.starts_with("Bearer "));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit OAuth"]
	async fn test_oauth_headers_len() {
		ensure_live_oauth().await.unwrap();
		assert!(OAUTH_CLIENT.load_full().headers_map.len() >= 3);
	}

	#[test]
	fn test_creating_device() {
		Device::new();
	}

	#[test]
	fn test_creating_backend() {
		MobileSpoofAuth::new();
	}

	#[test]
	fn oauth_response_accumulation_is_bounded() {
		let mut bytes = Vec::new();
		let maximum_payload = vec![0; MAX_OAUTH_RESPONSE_BYTES];
		extend_oauth_bytes(&mut bytes, &maximum_payload).unwrap();
		assert!(matches!(extend_oauth_bytes(&mut bytes, &[0]), Err(AuthError::ResponseTooLarge)));
	}
}
