//! Vale profiles and accounts: profile modes, the per-request authentication
//! gate, server-backed preferences, and the submodules for sessions and
//! passwords (`auth`), administrator account management (`users`), hidden
//! posts (`hidden`), reading history (`history`), and the schema (`schema`).

mod auth;
mod hidden;
mod history;
mod schema;
#[cfg(test)]
mod tests;
mod users;

pub use auth::*;
pub use hidden::*;
pub use history::*;
pub use schema::*;
pub use users::*;

use crate::{
	config::get_setting,
	login_throttle,
	server::{RequestExt, ResponseExt},
	utils::{read_body_limited, redirect, safe_local_redirect, template, FeedGroup, Post, Preferences},
};
use argon2::{
	password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
	Argon2,
};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cookie::{Cookie, SameSite};
use hyper::{header, Body, Method, Request, Response, StatusCode};
use percent_encoding::percent_decode_str;
use rand_core::OsRng;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::{
	collections::{HashMap, HashSet},
	path::PathBuf,
	sync::LazyLock,
};
use time::{Duration, OffsetDateTime};

const SECURE_SESSION_COOKIE: &str = "__Host-vale_session";
const DEVELOPMENT_SESSION_COOKIE: &str = "vale_session";
const MAX_FORM_BYTES: usize = 64 * 1024;
const HISTORY_RETENTION_SECONDS: i64 = 180 * 24 * 60 * 60;
const HISTORY_LIMIT: i64 = 5_000;
const HIDDEN_POST_LIMIT: i64 = 20_000;
const BROWSER_HIDDEN_POST_LIMIT: usize = 300;
const HIDDEN_POSTS_COOKIE: &str = "hidden_posts";

static DUMMY_PASSWORD_HASH: LazyLock<String> = LazyLock::new(|| {
	let salt = SaltString::encode_b64(b"vale-login-pad").expect("static login salt is valid");
	Argon2::default()
		.hash_password(b"this-password-never-matches", &salt)
		.expect("static dummy password hashes")
		.to_string()
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileMode {
	Browser,
	Shared,
	Accounts,
}

impl ProfileMode {
	pub fn label(self) -> &'static str {
		match self {
			Self::Browser => "browser",
			Self::Shared => "shared",
			Self::Accounts => "accounts",
		}
	}
}

#[derive(Clone, Debug)]
pub struct AuthContext {
	pub profile_id: i64,
	pub user_id: Option<i64>,
	pub username: String,
	pub display_name: String,
	pub is_admin: bool,
	pub session_hash: Option<Vec<u8>>,
	pub preferences: Preferences,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveBudgetSetting {
	pub mib: u64,
	pub revision: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveBudgetUpdate {
	Saved(ArchiveBudgetSetting),
	Conflict(ArchiveBudgetSetting),
}

#[derive(Clone, Debug)]
pub struct AccountView {
	pub username: String,
	pub display_name: String,
	pub is_admin: bool,
}

#[derive(Clone, Debug)]
pub struct AccountSummary {
	pub id: i64,
	pub username: String,
	pub display_name: String,
	pub is_admin: bool,
	pub is_disabled: bool,
	pub is_current: bool,
}

#[derive(Clone, Debug)]
pub struct HistoryEntry {
	pub title: String,
	pub community: String,
	pub permalink: String,
	pub viewed: String,
	pub view_count: i64,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
	prefs: Preferences,
	url: String,
	next: String,
	error: String,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
	prefs: Preferences,
	url: String,
	feed_groups: Vec<FeedGroup>,
	subscription_count: usize,
	error: String,
}

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryTemplate {
	prefs: Preferences,
	url: String,
	entries: Vec<HistoryEntry>,
}

#[derive(Debug)]
struct UserRecord {
	id: i64,
	password_hash: String,
	is_disabled: bool,
}

pub fn mode() -> ProfileMode {
	match get_setting("VALE_PROFILE_MODE")
		.unwrap_or_else(|| "browser".to_string())
		.trim()
		.to_ascii_lowercase()
		.as_str()
	{
		"shared" => ProfileMode::Shared,
		"accounts" => ProfileMode::Accounts,
		_ => ProfileMode::Browser,
	}
}

fn database_path() -> PathBuf {
	get_setting("VALE_PROFILE_DATABASE")
		.filter(|value| !value.trim().is_empty())
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("vale-profiles.sqlite3"))
}

fn cookie_secure() -> bool {
	get_setting("VALE_COOKIE_SECURE").is_none_or(|value| value != "off")
}

fn session_cookie_name() -> &'static str {
	if cookie_secure() {
		SECURE_SESSION_COOKIE
	} else {
		DEVELOPMENT_SESSION_COOKIE
	}
}

fn session_days() -> i64 {
	get_setting("VALE_SESSION_DAYS")
		.and_then(|value| value.parse::<i64>().ok())
		.filter(|days| (1..=365).contains(days))
		.unwrap_or(30)
}

pub(crate) fn now() -> i64 {
	OffsetDateTime::now_utc().unix_timestamp()
}

pub(crate) fn open_database() -> Result<crate::db::PooledConnection, String> {
	crate::db::pooled_at(&database_path())
}

#[cfg(test)]
fn open_database_at(path: &std::path::Path) -> Result<Connection, String> {
	crate::db::open_at(path)
}

fn default_preferences() -> Preferences {
	Preferences::from_browser(&Request::new(Body::empty()))
}

fn serialize_preferences(preferences: &Preferences) -> Result<String, String> {
	let mut stored = preferences.clone();
	stored.available_themes.clear();
	stored.active_feed.clear();
	serde_json::to_string(&stored).map_err(|error| format!("Unable to serialize Vale preferences: {error}"))
}

fn deserialize_preferences(value: &str) -> Result<Preferences, String> {
	let mut preferences: Preferences = serde_json::from_str(value).map_err(|error| format!("Unable to read saved Vale preferences: {error}"))?;
	preferences.available_themes.clear();
	preferences.active_feed.clear();
	preferences.apply_reader_defaults();
	Ok(preferences)
}

fn archive_budget_setting_in(connection: &Connection, profile_id: i64) -> Result<ArchiveBudgetSetting, String> {
	connection
		.query_row(
			"SELECT archive_budget_mib, revision FROM profile_archive_settings WHERE profile_id = ?1",
			params![profile_id],
			|row| {
				Ok(ArchiveBudgetSetting {
					mib: row.get::<_, i64>(0)?.max(0) as u64,
					revision: row.get::<_, i64>(1)?.max(0),
				})
			},
		)
		.optional()
		.map(|setting| setting.unwrap_or_default())
		.map_err(|error| format!("Unable to read the Vale archive setting: {error}"))
}

fn preferences_with_archive_budget(connection: &Connection, profile_id: i64, serialized: &str) -> Result<Preferences, String> {
	let mut preferences = deserialize_preferences(serialized)?;
	preferences.archive_budget_mib = archive_budget_setting_in(connection, profile_id)?.mib;
	Ok(preferences)
}

pub fn archive_budget_setting(request: &Request<Body>) -> Result<ArchiveBudgetSetting, String> {
	let Some(profile_id) = context(request).map(|context| context.profile_id) else {
		return Ok(ArchiveBudgetSetting::default());
	};
	archive_budget_setting_in(&*open_database()?, profile_id)
}

pub fn update_archive_budget(request: &Request<Body>, archive_budget_mib: u64, expected_revision: i64) -> Result<ArchiveBudgetUpdate, String> {
	crate::db::blocking(|| update_archive_budget_blocking(request, archive_budget_mib, expected_revision))
}

fn update_archive_budget_blocking(request: &Request<Body>, archive_budget_mib: u64, expected_revision: i64) -> Result<ArchiveBudgetUpdate, String> {
	let Some(profile_id) = context(request).map(|context| context.profile_id) else {
		return Err("A server-backed profile is required to change archive storage.".to_string());
	};
	let mut connection = open_database()?;
	update_archive_budget_in(&mut connection, profile_id, archive_budget_mib, expected_revision)
}

fn update_archive_budget_in(connection: &mut Connection, profile_id: i64, archive_budget_mib: u64, expected_revision: i64) -> Result<ArchiveBudgetUpdate, String> {
	if archive_budget_mib != 0 && (archive_budget_mib < 256 || !archive_budget_mib.is_multiple_of(256)) {
		return Err("Archive budgets must be 0 or whole 256 MiB steps.".to_string());
	}
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock the Vale archive setting: {error}"))?;
	let current = archive_budget_setting_in(&transaction, profile_id)?;
	if current.revision != expected_revision {
		return Ok(ArchiveBudgetUpdate::Conflict(current));
	}
	let updated = ArchiveBudgetSetting {
		mib: archive_budget_mib,
		revision: current.revision.saturating_add(1),
	};
	transaction
		.execute(
			"INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision, updated_at)
			 VALUES (?1, ?2, ?3, ?4)
			 ON CONFLICT(profile_id) DO UPDATE SET
				archive_budget_mib = excluded.archive_budget_mib,
				revision = excluded.revision,
				updated_at = excluded.updated_at",
			params![profile_id, updated.mib as i64, updated.revision, now()],
		)
		.map_err(|error| format!("Unable to persist the Vale archive setting: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish the Vale archive setting update: {error}"))?;
	Ok(ArchiveBudgetUpdate::Saved(updated))
}

fn ensure_shared_profile(connection: &Connection) -> Result<i64, String> {
	if let Some(id) = connection
		.query_row("SELECT id FROM profiles WHERE user_id IS NULL ORDER BY id LIMIT 1", [], |row| row.get(0))
		.optional()
		.map_err(|error| format!("Unable to inspect the shared Vale profile: {error}"))?
	{
		return Ok(id);
	}
	let timestamp = now();
	let preferences = serialize_preferences(&default_preferences())?;
	connection
		.execute(
			"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (NULL, 'Shared profile', ?1, ?2, ?2)",
			params![preferences, timestamp],
		)
		.map_err(|error| format!("Unable to create the shared Vale profile: {error}"))?;
	Ok(connection.last_insert_rowid())
}

fn user_count(connection: &Connection) -> Result<i64, String> {
	connection
		.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
		.map_err(|error| format!("Unable to count Vale accounts: {error}"))
}

pub fn has_accounts() -> Result<bool, String> {
	if mode() != ProfileMode::Accounts {
		return Ok(false);
	}
	let connection = open_database()?;
	Ok(user_count(&connection)? > 0)
}

fn is_public_asset(path: &str) -> bool {
	matches!(
		path,
		"/style.css"
			| "/manifest.json"
			| "/robots.txt"
			| "/favicon.ico"
			| "/vale-mark.svg"
			| "/logo.png"
			| "/touch-icon-iphone.png"
			| "/apple-touch-icon.png"
			| "/fonts/source-sans-3.woff2"
			| "/fonts/source-serif-4.woff2"
			| "/scenes/vale-dark.avif"
			| "/scenes/vale-dark.webp"
			| "/scenes/vale-light.avif"
			| "/scenes/vale-light.webp"
			| "/opensearch.xml"
			| "/playHLSVideo.js"
			| "/hls.min.js"
			| "/highlighted.js"
			| "/copy.js"
			| "/register-sw.js"
			| "/vale-interactions.js"
			| "/service-worker.js"
			| "/offline.html"
			| "/offline.js"
			| "/healthz"
	)
}

fn is_private_media_asset(path: &str) -> bool {
	[
		"/reaction/giphy/",
		"/img/",
		"/thumb/",
		"/emoji/",
		"/emote/",
		"/preview/",
		"/style/",
		"/static/",
		"/vid/",
		"/hls/",
	]
	.iter()
	.any(|prefix| path.starts_with(prefix))
}

pub fn response_cache_control(path: &str) -> Option<&'static str> {
	if mode() == ProfileMode::Browser || is_public_asset(path) {
		None
	} else if is_private_media_asset(path) {
		Some("private, max-age=1209600, immutable")
	} else {
		Some("private, no-store")
	}
}

fn parsed_origin(value: &str) -> Option<(String, String, u16)> {
	let parsed = url::Url::parse(value).ok()?;
	if !matches!(parsed.scheme(), "http" | "https")
		|| !parsed.username().is_empty()
		|| parsed.password().is_some()
		|| parsed.path() != "/"
		|| parsed.query().is_some()
		|| parsed.fragment().is_some()
	{
		return None;
	}
	Some((parsed.scheme().to_string(), parsed.host_str()?.to_ascii_lowercase(), parsed.port_or_known_default()?))
}

fn mutation_is_same_origin_with_expected(request: &Request<Body>, expected: Option<&str>) -> bool {
	if request.method() != Method::POST {
		return true;
	}
	if request
		.headers()
		.get("sec-fetch-site")
		.and_then(|value| value.to_str().ok())
		.is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
	{
		return false;
	}
	let Some(origin) = request.headers().get("origin").and_then(|value| value.to_str().ok()) else {
		// Current browsers send Origin with every POST. A request without it is
		// either a non-browser client (which holds no victim's cookies) or an
		// old browser; accept it only when fetch metadata, if present, agrees
		// that the request did not come from another site.
		return request
			.headers()
			.get("sec-fetch-site")
			.and_then(|value| value.to_str().ok())
			.is_none_or(|value| value.eq_ignore_ascii_case("same-origin") || value.eq_ignore_ascii_case("none"));
	};
	let Some(origin) = parsed_origin(origin) else {
		return false;
	};
	if expected.and_then(parsed_origin).is_some_and(|expected| expected == origin) {
		return true;
	}

	let Some(host) = request.headers().get("host").and_then(|value| value.to_str().ok()) else {
		return false;
	};
	let scheme = crate::net::request_scheme(request);
	if !matches!(scheme, "http" | "https") {
		return false;
	}
	parsed_origin(&format!("{scheme}://{host}")).is_some_and(|request_origin| request_origin == origin)
}

fn mutation_is_same_origin(request: &Request<Body>) -> bool {
	let expected = get_setting("REDLIB_FULL_URL");
	mutation_is_same_origin_with_expected(request, expected.as_deref())
}

fn plain_response(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(message.to_string()))
		.unwrap_or_default()
}

fn see_other(path: &str) -> Response<Body> {
	Response::builder()
		.status(StatusCode::SEE_OTHER)
		.header("location", path)
		.header("cache-control", "no-store")
		.body(Body::empty())
		.unwrap_or_default()
}

fn safe_next(value: &str) -> String {
	let value = safe_local_redirect(value, "/", 512);
	if value.starts_with("/login") {
		"/".to_string()
	} else {
		value
	}
}

fn login_redirect(request: &Request<Body>) -> Response<Body> {
	let next = safe_next(&request.uri().to_string());
	let query = url::form_urlencoded::Serializer::new(String::new()).append_pair("next", &next).finish();
	see_other(&format!("/login?{query}"))
}

pub fn prepare_request(request: &mut Request<Body>) -> Result<Option<Response<Body>>, String> {
	if !mutation_is_same_origin(request) {
		return Ok(Some(plain_response(StatusCode::FORBIDDEN, "Cross-site form submissions are not allowed.")));
	}

	match mode() {
		ProfileMode::Browser => Ok(None),
		ProfileMode::Shared => {
			if is_public_asset(request.uri().path()) {
				return Ok(None);
			}
			let connection = open_database()?;
			let profile_id = ensure_shared_profile(&connection)?;
			let preferences_json: String = connection
				.query_row("SELECT preferences_json FROM profiles WHERE id = ?1", params![profile_id], |row| row.get(0))
				.map_err(|error| format!("Unable to load the shared Vale profile: {error}"))?;
			request.extensions_mut().insert(AuthContext {
				profile_id,
				user_id: None,
				username: "shared".to_string(),
				display_name: "Shared profile".to_string(),
				is_admin: true,
				session_hash: None,
				preferences: preferences_with_archive_budget(&connection, profile_id, &preferences_json)?,
			});
			Ok(None)
		}
		ProfileMode::Accounts => {
			let path = request.uri().path();
			if is_public_asset(path) {
				return Ok(None);
			}
			let connection = open_database()?;
			let accounts_exist = user_count(&connection)? > 0;
			if !accounts_exist {
				if path == "/setup" {
					return Ok(None);
				}
				return Ok(Some(see_other("/setup")));
			}

			let context = session_context(&connection, request)?;
			if let Some(context) = context {
				if path == "/login" || path == "/setup" {
					return Ok(Some(see_other("/")));
				}
				request.extensions_mut().insert(context);
				return Ok(None);
			}

			if path == "/login" {
				return Ok(None);
			}
			if path == "/setup" {
				return Ok(Some(see_other("/login")));
			}
			Ok(Some(login_redirect(request)))
		}
	}
}

fn token_hash(token: &str) -> Vec<u8> {
	Sha256::digest(token.as_bytes()).to_vec()
}

fn random_token() -> String {
	let mut bytes = [0_u8; 32];
	getrandom::fill(&mut bytes).expect("the operating system provides secure randomness");
	URL_SAFE_NO_PAD.encode(bytes)
}

fn session_context(connection: &Connection, request: &Request<Body>) -> Result<Option<AuthContext>, String> {
	let Some(token) = request.cookie(session_cookie_name()).map(|cookie| cookie.value().to_string()) else {
		return Ok(None);
	};
	let hash = token_hash(&token);
	let timestamp = now();
	let row = connection
		.query_row(
			"SELECT u.id, u.username, u.display_name, u.is_admin, p.id, p.preferences_json, s.last_seen_at
			 FROM sessions s
			 JOIN users u ON u.id = s.user_id
			 JOIN profiles p ON p.user_id = u.id
			 WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND u.disabled_at IS NULL",
			params![hash, timestamp],
			|row| {
				Ok((
					row.get::<_, i64>(0)?,
					row.get::<_, String>(1)?,
					row.get::<_, String>(2)?,
					row.get::<_, i64>(3)? != 0,
					row.get::<_, i64>(4)?,
					row.get::<_, String>(5)?,
					row.get::<_, i64>(6)?,
				))
			},
		)
		.optional()
		.map_err(|error| format!("Unable to validate the Vale session: {error}"))?;
	let Some((user_id, username, display_name, is_admin, profile_id, preferences_json, last_seen_at)) = row else {
		return Ok(None);
	};
	if last_seen_at < timestamp - 3600 {
		connection
			.execute("UPDATE sessions SET last_seen_at = ?1 WHERE token_hash = ?2", params![timestamp, hash])
			.map_err(|error| format!("Unable to refresh the Vale session: {error}"))?;
	}
	Ok(Some(AuthContext {
		profile_id,
		user_id: Some(user_id),
		username,
		display_name,
		is_admin,
		session_hash: Some(hash),
		preferences: preferences_with_archive_budget(connection, profile_id, &preferences_json)?,
	}))
}

pub fn context(request: &Request<Body>) -> Option<&AuthContext> {
	request.extensions().get::<AuthContext>()
}

pub fn stored_preferences(request: &Request<Body>) -> Option<Preferences> {
	context(request).map(|context| context.preferences.clone())
}

pub fn server_backed(request: &Request<Body>) -> bool {
	context(request).is_some() && mode() != ProfileMode::Browser
}

pub fn save_preferences(request: &Request<Body>, preferences: &Preferences) -> Result<bool, String> {
	crate::db::blocking(|| save_preferences_blocking(request, preferences))
}

fn save_preferences_blocking(request: &Request<Body>, preferences: &Preferences) -> Result<bool, String> {
	let Some(context) = context(request) else {
		return Ok(false);
	};
	let connection = open_database()?;
	let serialized = serialize_preferences(preferences)?;
	let updated = connection
		.execute(
			"UPDATE profiles SET preferences_json = ?1, updated_at = ?2 WHERE id = ?3",
			params![serialized, now(), context.profile_id],
		)
		.map_err(|error| format!("Unable to save the Vale profile: {error}"))?;
	Ok(updated == 1)
}

pub fn restore_preferences(request: &Request<Body>, preferences: &Preferences) -> Result<bool, String> {
	crate::db::blocking(|| restore_preferences_blocking(request, preferences))
}

fn restore_preferences_blocking(request: &Request<Body>, preferences: &Preferences) -> Result<bool, String> {
	let Some(context) = context(request) else {
		return Ok(false);
	};
	preferences.validate_archive_budget()?;
	let serialized = serialize_preferences(preferences)?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock the Vale profile restore: {error}"))?;
	let current = archive_budget_setting_in(&transaction, context.profile_id)?;
	let timestamp = now();
	let updated = transaction
		.execute(
			"UPDATE profiles SET preferences_json = ?1, updated_at = ?2 WHERE id = ?3",
			params![serialized, timestamp, context.profile_id],
		)
		.map_err(|error| format!("Unable to restore the Vale profile preferences: {error}"))?;
	if updated == 0 {
		return Err("The active Vale profile no longer exists.".to_string());
	}
	transaction
		.execute(
			"INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision, updated_at)
			 VALUES (?1, ?2, ?3, ?4)
			 ON CONFLICT(profile_id) DO UPDATE SET archive_budget_mib = excluded.archive_budget_mib, revision = excluded.revision, updated_at = excluded.updated_at",
			params![context.profile_id, preferences.archive_budget_mib as i64, current.revision.saturating_add(1), timestamp],
		)
		.map_err(|error| format!("Unable to restore the Vale archive budget: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish the Vale profile restore: {error}"))?;
	Ok(true)
}

pub async fn health() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header("content-type", "text/plain; charset=utf-8")
			.header("cache-control", "no-store")
			.body(Body::from("ok\n"))
			.unwrap_or_default(),
	)
}
