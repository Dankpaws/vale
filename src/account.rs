use crate::{
	config::get_setting,
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
	fs,
	path::{Path, PathBuf},
	sync::{LazyLock, Mutex},
	time::{Duration as StdDuration, Instant},
};
use time::{Duration, OffsetDateTime};

const SECURE_SESSION_COOKIE: &str = "__Host-vale_session";
const DEVELOPMENT_SESSION_COOKIE: &str = "vale_session";
const MAX_FORM_BYTES: usize = 64 * 1024;
const MAX_LOGIN_THROTTLE_KEY_CHARS: usize = 64;
const LOGIN_WINDOW: StdDuration = StdDuration::from_secs(15 * 60);
const LOGIN_ATTEMPTS: usize = 8;
const MAX_TRACKED_LOGIN_NAMES: usize = 1_024;
const HISTORY_RETENTION_SECONDS: i64 = 180 * 24 * 60 * 60;
const HISTORY_LIMIT: i64 = 5_000;
const HIDDEN_POST_LIMIT: i64 = 20_000;
const BROWSER_HIDDEN_POST_LIMIT: usize = 300;
const HIDDEN_POSTS_COOKIE: &str = "hidden_posts";

static LOGIN_FAILURES: LazyLock<Mutex<HashMap<String, Vec<Instant>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
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

pub(crate) fn open_database() -> Result<Connection, String> {
	open_database_at(&database_path())
}

fn open_database_at(path: &Path) -> Result<Connection, String> {
	if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
		fs::create_dir_all(parent).map_err(|error| format!("Unable to create the Vale profile directory: {error}"))?;
	}
	let connection = Connection::open(path).map_err(|error| format!("Unable to open the Vale profile database: {error}"))?;
	connection
		.busy_timeout(StdDuration::from_secs(5))
		.map_err(|error| format!("Unable to configure the Vale profile database: {error}"))?;
	connection
		.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
		.map_err(|error| format!("Unable to configure the Vale profile database: {error}"))?;
	Ok(connection)
}

fn initialize_schema(connection: &Connection) -> Result<(), String> {
	connection
		.execute_batch(
			"CREATE TABLE IF NOT EXISTS users (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				username TEXT NOT NULL COLLATE NOCASE UNIQUE,
				display_name TEXT NOT NULL,
				password_hash TEXT NOT NULL,
				is_admin INTEGER NOT NULL DEFAULT 0,
				disabled_at INTEGER,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS profiles (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				user_id INTEGER UNIQUE REFERENCES users(id) ON DELETE CASCADE,
				label TEXT NOT NULL,
				preferences_json TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS sessions (
				token_hash BLOB PRIMARY KEY,
				user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
				created_at INTEGER NOT NULL,
				expires_at INTEGER NOT NULL,
				last_seen_at INTEGER NOT NULL,
				user_agent TEXT NOT NULL DEFAULT ''
			);
			CREATE INDEX IF NOT EXISTS sessions_user_id ON sessions(user_id);
			CREATE INDEX IF NOT EXISTS sessions_expires_at ON sessions(expires_at);
			CREATE TABLE IF NOT EXISTS post_history (
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				post_id TEXT NOT NULL,
				title TEXT NOT NULL,
				community TEXT NOT NULL,
				permalink TEXT NOT NULL,
				first_viewed_at INTEGER NOT NULL,
				last_viewed_at INTEGER NOT NULL,
				view_count INTEGER NOT NULL DEFAULT 1,
				PRIMARY KEY (profile_id, post_id)
			);
			CREATE INDEX IF NOT EXISTS post_history_recent ON post_history(profile_id, last_viewed_at DESC);
			CREATE TABLE IF NOT EXISTS hidden_posts (
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				post_id TEXT NOT NULL,
				hidden_at INTEGER NOT NULL,
				PRIMARY KEY (profile_id, post_id)
			);
			CREATE INDEX IF NOT EXISTS hidden_posts_recent ON hidden_posts(profile_id, hidden_at DESC);
			CREATE TABLE IF NOT EXISTS post_archives (
				id TEXT PRIMARY KEY,
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				post_id TEXT NOT NULL,
				permalink TEXT NOT NULL DEFAULT '',
				title TEXT NOT NULL DEFAULT '',
				community TEXT NOT NULL DEFAULT '',
				source_url TEXT NOT NULL DEFAULT '',
				status TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL,
				captured_at INTEGER,
				comment_count INTEGER NOT NULL DEFAULT 0,
				asset_count INTEGER NOT NULL DEFAULT 0,
				generated_asset_count INTEGER NOT NULL DEFAULT 0,
				total_bytes INTEGER NOT NULL DEFAULT 0,
				issues_json TEXT NOT NULL DEFAULT '[]',
				error TEXT NOT NULL DEFAULT '',
				UNIQUE (profile_id, post_id)
			);
			CREATE INDEX IF NOT EXISTS post_archives_profile_recent ON post_archives(profile_id, created_at DESC);
			CREATE INDEX IF NOT EXISTS post_archives_status ON post_archives(status, updated_at);
			CREATE TABLE IF NOT EXISTS profile_archive_settings (
				profile_id INTEGER PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
				archive_budget_mib INTEGER NOT NULL DEFAULT 0,
				revision INTEGER NOT NULL DEFAULT 0,
				updated_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS archive_reservations (
				archive_id TEXT PRIMARY KEY REFERENCES post_archives(id) ON DELETE CASCADE,
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				state TEXT NOT NULL,
				reserved_bytes INTEGER NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			);
			CREATE INDEX IF NOT EXISTS archive_reservations_profile ON archive_reservations(profile_id, state);",
		)
		.map_err(|error| format!("Unable to migrate the Vale profile database: {error}"))?;
	ensure_column(connection, "post_archives", "generated_asset_count", "INTEGER NOT NULL DEFAULT 0")?;
	Ok(())
}

fn ensure_column(connection: &Connection, table: &str, column: &str, definition: &str) -> Result<(), String> {
	let mut statement = connection
		.prepare(&format!("PRAGMA table_info({table})"))
		.map_err(|error| format!("Unable to inspect the Vale database schema: {error}"))?;
	let columns = statement
		.query_map([], |row| row.get::<_, String>(1))
		.map_err(|error| format!("Unable to query the Vale database schema: {error}"))?
		.collect::<Result<HashSet<_>, _>>()
		.map_err(|error| format!("Unable to read the Vale database schema: {error}"))?;
	drop(statement);
	if !columns.contains(column) {
		connection
			.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
			.map_err(|error| format!("Unable to extend the Vale database schema: {error}"))?;
	}
	Ok(())
}

pub fn initialize() -> Result<(), String> {
	if mode() == ProfileMode::Browser {
		return Ok(());
	}
	let connection = open_database()?;
	initialize_schema(&connection)?;
	connection
		.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now()])
		.map_err(|error| format!("Unable to expire old Vale sessions: {error}"))?;
	if mode() == ProfileMode::Shared {
		ensure_shared_profile(&connection)?;
	}
	Ok(())
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
	archive_budget_setting_in(&open_database()?, profile_id)
}

pub fn update_archive_budget(request: &Request<Body>, archive_budget_mib: u64, expected_revision: i64) -> Result<ArchiveBudgetUpdate, String> {
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
			| "/healthz"
	)
}

fn is_private_media_asset(path: &str) -> bool {
	["/img/", "/thumb/", "/emoji/", "/emote/", "/preview/", "/style/", "/static/", "/vid/", "/hls/"]
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
		return true;
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
	let forwarded_scheme = request
		.headers()
		.get("x-forwarded-proto")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.split(',').next())
		.map(str::trim);
	let scheme = request.uri().scheme_str().or(forwarded_scheme).unwrap_or("http");
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

pub(crate) fn valid_post_id(value: &str) -> bool {
	!value.is_empty() && value.len() <= 80 && value.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn browser_hidden_post_ids(request: &Request<Body>) -> Vec<String> {
	let mut seen = HashSet::new();
	request
		.cookie(HIDDEN_POSTS_COOKIE)
		.map(|cookie| cookie.value().to_string())
		.unwrap_or_default()
		.split('.')
		.filter(|post_id| valid_post_id(post_id))
		.filter_map(|post_id| seen.insert(post_id.to_string()).then_some(post_id.to_string()))
		.take(BROWSER_HIDDEN_POST_LIMIT)
		.collect()
}

fn hidden_posts_cookie(post_ids: &[String]) -> Cookie<'static> {
	Cookie::build((HIDDEN_POSTS_COOKIE, post_ids.join(".")))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
		.into()
}

fn remove_hidden_posts_cookie(response: &mut Response<Body>) {
	let cookie = Cookie::build((HIDDEN_POSTS_COOKIE, ""))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::UNIX_EPOCH)
		.max_age(Duration::ZERO)
		.into();
	response.insert_cookie(cookie);
}

fn empty_response(status: StatusCode) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("cache-control", "no-store")
		.body(Body::empty())
		.unwrap_or_default()
}

fn hidden_post_ids(request: &Request<Body>) -> Result<HashSet<String>, String> {
	let Some(context) = context(request) else {
		return Ok(browser_hidden_post_ids(request).into_iter().collect());
	};
	let connection = open_database()?;
	let mut statement = connection
		.prepare("SELECT post_id FROM hidden_posts WHERE profile_id = ?1")
		.map_err(|error| format!("Unable to prepare hidden Vale posts: {error}"))?;
	let rows = statement
		.query_map(params![context.profile_id], |row| row.get::<_, String>(0))
		.map_err(|error| format!("Unable to query hidden Vale posts: {error}"))?;
	rows
		.collect::<Result<HashSet<_>, _>>()
		.map_err(|error| format!("Unable to read hidden Vale posts: {error}"))
}

pub(crate) fn hidden_post_ids_for_listing(request: &Request<Body>) -> Result<HashSet<String>, String> {
	hidden_post_ids(request)
}

pub fn post_is_hidden(request: &Request<Body>, post_id: &str) -> Result<bool, String> {
	Ok(hidden_post_ids(request)?.contains(post_id))
}

/// Reconcile a restored/back-forward-cached listing against current profile
/// state without polling or exposing any records beyond the explicitly named,
/// bounded post identifiers already present in that document.
pub async fn hidden_state_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let requested = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
		.find_map(|(key, value)| (key == "ids").then_some(value.into_owned()))
		.unwrap_or_default();
	let mut seen = HashSet::new();
	let ids = requested
		.split(',')
		.filter(|id| valid_post_id(id))
		.filter_map(|id| seen.insert(id.to_string()).then_some(id.to_string()))
		.take(250)
		.collect::<Vec<_>>();
	let hidden = hidden_post_ids(&request)?;
	let hidden = ids.into_iter().filter(|id| hidden.contains(id)).collect::<Vec<_>>();
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header("content-type", "application/json; charset=utf-8")
			.header("cache-control", "private, no-store")
			.body(serde_json::to_string(&hidden).unwrap_or_else(|_| "[]".to_string()).into())
			.unwrap_or_default(),
	)
}

/// Remove posts hidden by the current profile while retaining user comments,
/// whose listing records intentionally have no post title.
pub fn filter_hidden_posts(request: &Request<Body>, posts: &mut Vec<Post>) -> Result<usize, String> {
	let hidden = hidden_post_ids(request)?;
	let before = posts.len();
	posts.retain(|post| post.title.is_empty() || !hidden.contains(&post.id));
	Ok(before - posts.len())
}

pub fn hidden_post_count(request: &Request<Body>) -> Result<usize, String> {
	if let Some(context) = context(request) {
		return open_database()?
			.query_row("SELECT COUNT(*) FROM hidden_posts WHERE profile_id = ?1", params![context.profile_id], |row| {
				row.get::<_, i64>(0)
			})
			.map(|count| count.max(0) as usize)
			.map_err(|error| format!("Unable to count hidden Vale posts: {error}"));
	}
	Ok(browser_hidden_post_ids(request).len())
}

const MAX_HIDE_FORM_BYTES: usize = 8 * 1024;
const MAX_RETURN_TO_BYTES: usize = 2_048;

fn valid_percent_encoding(value: &str) -> bool {
	let bytes = value.as_bytes();
	let mut index = 0usize;
	while index < bytes.len() {
		if bytes[index] == b'%' {
			if index + 2 >= bytes.len() || !bytes[index + 1].is_ascii_hexdigit() || !bytes[index + 2].is_ascii_hexdigit() {
				return false;
			}
			index += 3;
		} else {
			index += 1;
		}
	}
	true
}

/// Strictly validate a decoded form redirect. Repeated decoding is used only
/// for validation so double-encoded traversal or network paths cannot become
/// dangerous after another browser/proxy normalization step.
fn strict_return_to(value: &str) -> Option<String> {
	if value.is_empty() || value.len() > MAX_RETURN_TO_BYTES || !value.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) || !valid_percent_encoding(value) {
		return None;
	}
	// The redirect emitted in Location must already be root-relative. Recursive
	// decoding below is validation-only; accepting an encoded leading slash
	// would validate one spelling and return a different, non-relative target.
	if !value.starts_with('/') {
		return None;
	}
	let original = value.to_string();
	let mut decoded = original.clone();
	for _ in 0..3 {
		let next = percent_decode_str(&decoded).decode_utf8().ok()?.into_owned();
		if next == decoded {
			break;
		}
		if next.len() > MAX_RETURN_TO_BYTES || !valid_percent_encoding(&next) {
			return None;
		}
		decoded = next;
	}
	if decoded.contains('%')
		|| !decoded.starts_with('/')
		|| decoded.starts_with("//")
		|| decoded.contains('\u{5c}')
		|| decoded.contains('#')
		|| decoded.chars().any(char::is_control)
	{
		return None;
	}
	let path = decoded.split('?').next().unwrap_or_default();
	if path.split('/').any(|segment| matches!(segment, "." | "..")) {
		return None;
	}
	let parsed = url::Url::parse(&format!("https://vale.invalid{decoded}")).ok()?;
	if parsed.scheme() != "https" || parsed.host_str() != Some("vale.invalid") || !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
		return None;
	}
	Some(original)
}

fn request_origins(request: &Request<Body>) -> Vec<(String, String, u16)> {
	let mut origins = Vec::with_capacity(2);
	if let Some(origin) = get_setting("REDLIB_FULL_URL").as_deref().and_then(parsed_origin) {
		origins.push(origin);
	}
	if let Some(host) = request.headers().get("host").and_then(|value| value.to_str().ok()) {
		let forwarded_scheme = request
			.headers()
			.get("x-forwarded-proto")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.split(',').next())
			.map(str::trim);
		let scheme = request.uri().scheme_str().or(forwarded_scheme).unwrap_or("http");
		if let Some(origin) = parsed_origin(&format!("{scheme}://{host}")) {
			if !origins.contains(&origin) {
				origins.push(origin);
			}
		}
	}
	origins
}

fn validated_referer_target(request: &Request<Body>) -> Option<String> {
	let referer = request.headers().get(header::REFERER)?.to_str().ok()?;
	if let Some(relative) = strict_return_to(referer) {
		return Some(relative);
	}
	// Validate the exact path text before `Url` has a chance to normalize dot
	// segments. A Referer is only a fallback, so ambiguous spellings fail closed.
	if referer.contains('\\') || referer.contains('#') || referer.chars().any(char::is_control) || !valid_percent_encoding(referer) {
		return None;
	}
	let authority_start = referer.find("://")?.saturating_add(3);
	let suffix_start = referer[authority_start..].find(['/', '?']).map(|offset| authority_start.saturating_add(offset));
	let raw_target = match suffix_start.map(|index| &referer[index..]) {
		Some(suffix) if suffix.starts_with('/') => suffix.to_string(),
		Some(query) => format!("/{query}"),
		None => "/".to_string(),
	};
	let raw_target = strict_return_to(&raw_target)?;
	let parsed = url::Url::parse(referer).ok()?;
	if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
		return None;
	}
	let referer_origin = parsed_origin(&format!(
		"{}://{}{}",
		parsed.scheme(),
		parsed.host_str()?,
		parsed.port().map(|port| format!(":{port}")).unwrap_or_default()
	))?;
	if !request_origins(request).contains(&referer_origin) {
		return None;
	}
	Some(raw_target)
}

fn is_enhanced_hide(request: &Request<Body>) -> bool {
	let values = request.headers().get_all("x-vale-enhanced").iter().collect::<Vec<_>>();
	values.len() == 1 && values[0].as_bytes() == b"hide-v1"
}

fn decode_form_component(value: &[u8]) -> Option<String> {
	let value = std::str::from_utf8(value).ok()?;
	if !valid_percent_encoding(value) {
		return None;
	}
	let spaces = value.replace('+', " ");
	percent_decode_str(&spaces).decode_utf8().ok().map(|value| value.into_owned())
}

fn form_return_to(body: &[u8]) -> Option<String> {
	for pair in body.split(|byte| *byte == b'&') {
		let mut parts = pair.splitn(2, |byte| *byte == b'=');
		let key = decode_form_component(parts.next().unwrap_or_default())?;
		if key == "return_to" {
			return decode_form_component(parts.next().unwrap_or_default()).and_then(|value| strict_return_to(&value));
		}
	}
	None
}

async fn hide_redirect_target(request: &mut Request<Body>) -> String {
	let referer = validated_referer_target(request).unwrap_or_else(|| "/".to_string());
	let body = match read_body_limited(request.body_mut(), MAX_HIDE_FORM_BYTES, "The hide form is too large.").await {
		Ok(body) => body,
		Err(_) => return referer,
	};
	form_return_to(&body).unwrap_or(referer)
}

fn hide_mutation_response(enhanced: bool, return_to: String) -> Response<Body> {
	if enhanced {
		empty_response(StatusCode::NO_CONTENT)
	} else {
		see_other(&return_to)
	}
}

pub async fn hide_post_post(mut request: Request<Body>) -> Result<Response<Body>, String> {
	let post_id = request.param("id").unwrap_or_default();
	if !valid_post_id(&post_id) {
		return Ok(plain_response(StatusCode::BAD_REQUEST, "The post identifier is invalid."));
	}
	let enhanced = is_enhanced_hide(&request);
	let return_to = if enhanced { String::new() } else { hide_redirect_target(&mut request).await };
	if let Some(profile_id) = context(&request).map(|context| context.profile_id) {
		let connection = open_database()?;
		connection
			.execute(
				"INSERT INTO hidden_posts (profile_id, post_id, hidden_at) VALUES (?1, ?2, ?3)
				 ON CONFLICT(profile_id, post_id) DO UPDATE SET hidden_at = excluded.hidden_at",
				params![profile_id, post_id, now()],
			)
			.map_err(|error| format!("Unable to hide the Vale post: {error}"))?;
		connection
			.execute(
				"DELETE FROM hidden_posts WHERE profile_id = ?1 AND post_id IN (
					SELECT post_id FROM hidden_posts WHERE profile_id = ?1 ORDER BY hidden_at DESC LIMIT -1 OFFSET ?2
				)",
				params![profile_id, HIDDEN_POST_LIMIT],
			)
			.map_err(|error| format!("Unable to bound hidden Vale posts: {error}"))?;
		return Ok(hide_mutation_response(enhanced, return_to));
	}

	let mut post_ids = browser_hidden_post_ids(&request);
	post_ids.retain(|hidden_id| hidden_id != &post_id);
	post_ids.push(post_id);
	if post_ids.len() > BROWSER_HIDDEN_POST_LIMIT {
		post_ids.drain(..post_ids.len() - BROWSER_HIDDEN_POST_LIMIT);
	}
	let mut response = hide_mutation_response(enhanced, return_to);
	response.insert_cookie(hidden_posts_cookie(&post_ids));
	Ok(response)
}

pub async fn unhide_post_post(mut request: Request<Body>) -> Result<Response<Body>, String> {
	let post_id = request.param("id").unwrap_or_default();
	if !valid_post_id(&post_id) {
		return Ok(plain_response(StatusCode::BAD_REQUEST, "The post identifier is invalid."));
	}
	let enhanced = is_enhanced_hide(&request);
	let return_to = if enhanced { String::new() } else { hide_redirect_target(&mut request).await };
	if let Some(profile_id) = context(&request).map(|context| context.profile_id) {
		open_database()?
			.execute("DELETE FROM hidden_posts WHERE profile_id = ?1 AND post_id = ?2", params![profile_id, post_id])
			.map_err(|error| format!("Unable to restore the Vale post: {error}"))?;
		return Ok(hide_mutation_response(enhanced, return_to));
	}

	let mut post_ids = browser_hidden_post_ids(&request);
	post_ids.retain(|hidden_id| hidden_id != &post_id);
	let mut response = hide_mutation_response(enhanced, return_to);
	if post_ids.is_empty() {
		remove_hidden_posts_cookie(&mut response);
	} else {
		response.insert_cookie(hidden_posts_cookie(&post_ids));
	}
	Ok(response)
}

pub async fn hidden_clear_post(request: Request<Body>) -> Result<Response<Body>, String> {
	if let Some(profile_id) = context(&request).map(|context| context.profile_id) {
		open_database()?
			.execute("DELETE FROM hidden_posts WHERE profile_id = ?1", params![profile_id])
			.map_err(|error| format!("Unable to clear hidden Vale posts: {error}"))?;
		return Ok(see_other("/settings"));
	}
	let mut response = see_other("/settings");
	remove_hidden_posts_cookie(&mut response);
	Ok(response)
}

pub fn active_feed_cookie(value: String) -> Cookie<'static> {
	Cookie::build(("active_feed", value))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
		.into()
}

fn session_cookie(token: String) -> Cookie<'static> {
	let days = session_days();
	Cookie::build((session_cookie_name(), token))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::days(days))
		.max_age(Duration::days(days))
		.into()
}

fn remove_session_cookie(response: &mut Response<Body>) {
	let cookie = Cookie::build((session_cookie_name(), ""))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::UNIX_EPOCH)
		.max_age(Duration::ZERO)
		.into();
	response.insert_cookie(cookie);
}

fn password_hash(password: &str) -> Result<String, String> {
	let salt = SaltString::generate(&mut OsRng);
	Argon2::default()
		.hash_password(password.as_bytes(), &salt)
		.map(|hash| hash.to_string())
		.map_err(|error| format!("Unable to secure the password: {error}"))
}

fn password_matches(password: &str, encoded: &str) -> bool {
	PasswordHash::new(encoded)
		.ok()
		.is_some_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
}

fn validate_username(value: &str) -> Result<String, String> {
	let username = value.trim();
	if !(3..=32).contains(&username.len()) || !username.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')) {
		return Err("Use 3–32 letters, numbers, periods, underscores, or hyphens for the username.".to_string());
	}
	Ok(username.to_ascii_lowercase())
}

fn validate_display_name(value: &str, username: &str) -> Result<String, String> {
	let display_name = if value.trim().is_empty() { username } else { value.trim() };
	if display_name.chars().count() > 64 || display_name.chars().any(char::is_control) {
		return Err("Display names must be 64 characters or fewer.".to_string());
	}
	Ok(display_name.to_string())
}

fn validate_password(password: &str, confirmation: &str) -> Result<(), String> {
	if password != confirmation {
		return Err("The passwords do not match.".to_string());
	}
	if !(12..=128).contains(&password.chars().count()) {
		return Err("Use a password between 12 and 128 characters.".to_string());
	}
	Ok(())
}

async fn read_form(request: Request<Body>) -> Result<HashMap<String, String>, String> {
	let mut body = request.into_body();
	let body = read_body_limited(&mut body, MAX_FORM_BYTES, "The submitted form is too large.")
		.await
		.map_err(|error| format!("Unable to read the form: {error}"))?;
	Ok(url::form_urlencoded::parse(&body).map(|(key, value)| (key.into_owned(), value.into_owned())).collect())
}

fn form_value<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
	form.get(key).map(String::as_str).unwrap_or_default()
}

fn login_throttle_key(username: &str) -> String {
	username.chars().take(MAX_LOGIN_THROTTLE_KEY_CHARS).collect::<String>().to_ascii_lowercase()
}

fn login_allowed(username: &str) -> bool {
	let mut attempts = LOGIN_FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
	let now = Instant::now();
	attempts.retain(|_, entries| {
		entries.retain(|attempt| now.duration_since(*attempt) < LOGIN_WINDOW);
		!entries.is_empty()
	});
	let username = login_throttle_key(username);
	if !attempts.contains_key(&username) && attempts.len() >= MAX_TRACKED_LOGIN_NAMES {
		return false;
	}
	let entries = attempts.entry(username).or_default();
	entries.len() < LOGIN_ATTEMPTS
}

fn record_login_failure(username: &str) {
	let mut attempts = LOGIN_FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
	attempts.entry(login_throttle_key(username)).or_default().push(Instant::now());
}

fn clear_login_failures(username: &str) {
	LOGIN_FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&login_throttle_key(username));
}

fn find_user(connection: &Connection, username: &str) -> Result<Option<UserRecord>, String> {
	connection
		.query_row(
			"SELECT id, password_hash, disabled_at IS NOT NULL FROM users WHERE username = ?1 COLLATE NOCASE",
			params![username],
			|row| {
				Ok(UserRecord {
					id: row.get(0)?,
					password_hash: row.get(1)?,
					is_disabled: row.get::<_, i64>(2)? != 0,
				})
			},
		)
		.optional()
		.map_err(|error| format!("Unable to read the Vale account: {error}"))
}

fn insert_session(connection: &Connection, user_id: i64) -> Result<String, String> {
	let token = random_token();
	let timestamp = now();
	connection
		.execute(
			"INSERT INTO sessions (token_hash, user_id, created_at, expires_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?3)",
			params![token_hash(&token), user_id, timestamp, timestamp + session_days() * 86_400],
		)
		.map_err(|error| format!("Unable to create the Vale session: {error}"))?;
	Ok(token)
}

/// Reset one account's password for a local operator tool.
///
/// The caller is responsible for obtaining the replacement password through a
/// protected prompt. This function does not log, include, or return the
/// password, and keeps a disabled account disabled while revoking its sessions.
pub fn reset_password_offline(username: &str, new_password: &str, confirmation: &str) -> Result<(), String> {
	if mode() != ProfileMode::Accounts {
		return Err("Offline password reset requires Vale account mode.".to_string());
	}
	let username = validate_username(username).map_err(|_| "The account username is invalid; use 3–32 letters, numbers, periods, underscores, or hyphens.".to_string())?;
	validate_password(new_password, confirmation).map_err(|error| format!("Replacement password rejected: {error}"))?;
	let encoded_password = password_hash(new_password).map_err(|_| "Unable to secure the replacement password.".to_string())?;
	let mut connection = open_database().map_err(|_| "Unable to open the configured Vale account database.".to_string())?;
	reset_password_offline_in_connection(&mut connection, &username, &encoded_password)
}

fn reset_password_offline_in_connection(connection: &mut Connection, username: &str, encoded_password: &str) -> Result<(), String> {
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|_| "Unable to lock the account database for offline password reset.".to_string())?;
	let user_id: Option<i64> = transaction
		.query_row("SELECT id FROM users WHERE username = ?1 COLLATE NOCASE", params![username], |row| row.get(0))
		.optional()
		.map_err(|_| "Unable to inspect the Vale account database.".to_string())?;
	let Some(user_id) = user_id else {
		return Err("No Vale account matched that username.".to_string());
	};
	let updated = transaction
		.execute(
			"UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3",
			params![encoded_password, now(), user_id],
		)
		.map_err(|_| "Unable to update the Vale account password.".to_string())?;
	if updated != 1 {
		return Err("Unable to update exactly one Vale account.".to_string());
	}
	transaction
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
		.map_err(|_| "Unable to revoke the Vale account sessions.".to_string())?;
	transaction.commit().map_err(|_| "Unable to commit the offline password reset.".to_string())
}

fn render_login(preferences: Preferences, next: String, error: String, status: StatusCode) -> Response<Body> {
	let body = LoginTemplate {
		prefs: preferences,
		url: "/login".to_string(),
		next,
		error,
	}
	.render()
	.unwrap_or_default();
	Response::builder()
		.status(status)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.unwrap_or_default()
}

pub async fn login_get(request: Request<Body>) -> Result<Response<Body>, String> {
	if mode() != ProfileMode::Accounts {
		return Ok(redirect("/"));
	}
	if !has_accounts()? {
		return Ok(see_other("/setup"));
	}
	let next = request
		.uri()
		.query()
		.and_then(|query| {
			url::form_urlencoded::parse(query.as_bytes())
				.find(|(key, _)| key == "next")
				.map(|(_, value)| value.into_owned())
		})
		.map_or_else(|| "/".to_string(), |value| safe_next(&value));
	Ok(render_login(Preferences::from_browser(&request), next, String::new(), StatusCode::OK))
}

pub async fn login_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let preferences = Preferences::from_browser(&request);
	let form = read_form(request).await?;
	let username = form_value(&form, "username").trim().to_ascii_lowercase();
	let password = form_value(&form, "password");
	let next = safe_next(form_value(&form, "next"));
	if !login_allowed(&username) {
		return Ok(render_login(
			preferences,
			next,
			"Too many sign-in attempts. Wait fifteen minutes and try again.".to_string(),
			StatusCode::TOO_MANY_REQUESTS,
		));
	}
	let connection = open_database()?;
	let user = find_user(&connection, &username)?;
	let encoded = user.as_ref().map(|user| user.password_hash.as_str()).unwrap_or(DUMMY_PASSWORD_HASH.as_str());
	let valid = password_matches(password, encoded) && user.as_ref().is_some_and(|user| !user.is_disabled);
	if !valid {
		record_login_failure(&username);
		return Ok(render_login(
			preferences,
			next,
			"The username or password was not accepted.".to_string(),
			StatusCode::UNAUTHORIZED,
		));
	}
	let user = user.expect("valid login has a user");
	clear_login_failures(&username);
	let token = insert_session(&connection, user.id)?;
	let mut response = see_other(&next);
	response.insert_cookie(session_cookie(token));
	Ok(response)
}

fn render_setup(preferences: Preferences, error: String, status: StatusCode) -> Response<Body> {
	let feed_groups = preferences.feed_groups();
	let subscription_count = preferences.subscriptions.len();
	let body = SetupTemplate {
		prefs: preferences,
		url: "/setup".to_string(),
		feed_groups,
		subscription_count,
		error,
	}
	.render()
	.unwrap_or_default();
	Response::builder()
		.status(status)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.unwrap_or_default()
}

pub async fn setup_get(request: Request<Body>) -> Result<Response<Body>, String> {
	if mode() != ProfileMode::Accounts {
		return Ok(redirect("/"));
	}
	if has_accounts()? {
		return Ok(see_other("/login"));
	}
	Ok(render_setup(Preferences::from_browser(&request), String::new(), StatusCode::OK))
}

pub async fn setup_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let browser_preferences = Preferences::from_browser(&request);
	let form = read_form(request).await?;
	let username = match validate_username(form_value(&form, "username")) {
		Ok(value) => value,
		Err(error) => return Ok(render_setup(browser_preferences, error, StatusCode::UNPROCESSABLE_ENTITY)),
	};
	let display_name = match validate_display_name(form_value(&form, "display_name"), &username) {
		Ok(value) => value,
		Err(error) => return Ok(render_setup(browser_preferences, error, StatusCode::UNPROCESSABLE_ENTITY)),
	};
	if let Err(error) = validate_password(form_value(&form, "password"), form_value(&form, "password_confirm")) {
		return Ok(render_setup(browser_preferences, error, StatusCode::UNPROCESSABLE_ENTITY));
	}
	let preferences = if form.contains_key("import_current") {
		browser_preferences.clone()
	} else {
		default_preferences()
	};
	let encoded_password = password_hash(form_value(&form, "password"))?;
	let serialized = serialize_preferences(&preferences)?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin owner setup: {error}"))?;
	if user_count(&transaction)? != 0 {
		return Ok(see_other("/login"));
	}
	let timestamp = now();
	transaction
		.execute(
			"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?4)",
			params![username, display_name, encoded_password, timestamp],
		)
		.map_err(|error| format!("Unable to create the owner account: {error}"))?;
	let user_id = transaction.last_insert_rowid();
	let shared_profile_id: Option<i64> = transaction
		.query_row("SELECT id FROM profiles WHERE user_id IS NULL ORDER BY id LIMIT 1", [], |row| row.get(0))
		.optional()
		.map_err(|error| format!("Unable to inspect the shared profile during setup: {error}"))?;
	if let Some(profile_id) = shared_profile_id {
		transaction
			.execute(
				"UPDATE profiles SET user_id = ?1, label = ?2, preferences_json = ?3, updated_at = ?4 WHERE id = ?5",
				params![user_id, display_name, serialized, timestamp, profile_id],
			)
			.map_err(|error| format!("Unable to promote the shared profile: {error}"))?;
	} else {
		transaction
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
				params![user_id, display_name, serialized, timestamp],
			)
			.map_err(|error| format!("Unable to create the owner profile: {error}"))?;
	}
	let token = random_token();
	transaction
		.execute(
			"INSERT INTO sessions (token_hash, user_id, created_at, expires_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?3)",
			params![token_hash(&token), user_id, timestamp, timestamp + session_days() * 86_400],
		)
		.map_err(|error| format!("Unable to create the owner session: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish owner setup: {error}"))?;
	let mut response = see_other("/");
	response.insert_cookie(session_cookie(token));
	if !preferences.active_feed.is_empty() {
		response.insert_cookie(active_feed_cookie(preferences.active_feed));
	}
	Ok(response)
}

pub async fn logout_post(request: Request<Body>) -> Result<Response<Body>, String> {
	if let Some(context) = context(&request) {
		if let Some(hash) = &context.session_hash {
			open_database()?
				.execute("DELETE FROM sessions WHERE token_hash = ?1", params![hash])
				.map_err(|error| format!("Unable to end the Vale session: {error}"))?;
		}
	}
	let mut response = see_other("/login");
	remove_session_cookie(&mut response);
	Ok(response)
}

pub async fn logout_all_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(user_id) = context(&request).and_then(|context| context.user_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	open_database()?
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
		.map_err(|error| format!("Unable to end the Vale sessions: {error}"))?;
	let mut response = see_other("/login");
	remove_session_cookie(&mut response);
	Ok(response)
}

pub async fn change_password_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(user_id) = context(&request).and_then(|context| context.user_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	let form = read_form(request).await?;
	if let Err(error) = validate_password(form_value(&form, "new_password"), form_value(&form, "new_password_confirm")) {
		return Ok(see_other(&format!("/settings?account={}", status_code(&error))));
	}
	let mut connection = open_database()?;
	let current_hash: String = connection
		.query_row("SELECT password_hash FROM users WHERE id = ?1", params![user_id], |row| row.get(0))
		.map_err(|error| format!("Unable to read the Vale account: {error}"))?;
	if !password_matches(form_value(&form, "current_password"), &current_hash) {
		return Ok(see_other("/settings?account=current-password"));
	}
	let encoded = password_hash(form_value(&form, "new_password"))?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin the password change: {error}"))?;
	transaction
		.execute("UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3", params![encoded, now(), user_id])
		.map_err(|error| format!("Unable to change the Vale password: {error}"))?;
	transaction
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
		.map_err(|error| format!("Unable to rotate the Vale sessions: {error}"))?;
	let token = insert_session(&transaction, user_id)?;
	transaction.commit().map_err(|error| format!("Unable to finish the password change: {error}"))?;
	let mut response = see_other("/settings?account=password-changed");
	response.insert_cookie(session_cookie(token));
	Ok(response)
}

fn status_code(error: &str) -> &'static str {
	if error.contains("match") {
		"password-mismatch"
	} else {
		"password-length"
	}
}

pub async fn create_user_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(admin) = context(&request).cloned().filter(|context| context.is_admin) else {
		return Ok(plain_response(StatusCode::FORBIDDEN, "Administrator access is required."));
	};
	let form = read_form(request).await?;
	let username = match validate_username(form_value(&form, "username")) {
		Ok(value) => value,
		Err(_) => return Ok(see_other("/settings?account=username-invalid")),
	};
	let display_name = match validate_display_name(form_value(&form, "display_name"), &username) {
		Ok(value) => value,
		Err(_) => return Ok(see_other("/settings?account=display-name-invalid")),
	};
	if let Err(error) = validate_password(form_value(&form, "password"), form_value(&form, "password_confirm")) {
		return Ok(see_other(&format!("/settings?account={}", status_code(&error))));
	}
	let encoded = password_hash(form_value(&form, "password"))?;
	let preferences = if form.contains_key("clone_profile") { admin.preferences } else { default_preferences() };
	let serialized = serialize_preferences(&preferences)?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin account creation: {error}"))?;
	let timestamp = now();
	let inserted = transaction.execute(
		"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
		params![username, display_name, encoded, i64::from(form.contains_key("is_admin")), timestamp],
	);
	if inserted.is_err() {
		return Ok(see_other("/settings?account=username-exists"));
	}
	let user_id = transaction.last_insert_rowid();
	transaction
		.execute(
			"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
			params![user_id, display_name, serialized, timestamp],
		)
		.map_err(|error| format!("Unable to create the account profile: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish account creation: {error}"))?;
	Ok(see_other("/settings?account=user-created"))
}

pub async fn toggle_user_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(admin) = context(&request).cloned().filter(|context| context.is_admin) else {
		return Ok(plain_response(StatusCode::FORBIDDEN, "Administrator access is required."));
	};
	let target_id = request.param("id").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default();
	if admin.user_id == Some(target_id) {
		return Ok(see_other("/settings?account=self-disable"));
	}
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin the account status change: {error}"))?;
	let target: Option<(bool, bool)> = transaction
		.query_row("SELECT is_admin != 0, disabled_at IS NOT NULL FROM users WHERE id = ?1", params![target_id], |row| {
			Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0))
		})
		.optional()
		.map_err(|error| format!("Unable to inspect the target account: {error}"))?;
	let Some((target_is_admin, target_disabled)) = target else {
		return Ok(see_other("/settings?account=user-missing"));
	};
	if target_is_admin && !target_disabled {
		let enabled_admins: i64 = transaction
			.query_row("SELECT COUNT(*) FROM users WHERE is_admin != 0 AND disabled_at IS NULL", [], |row| row.get(0))
			.map_err(|error| format!("Unable to count Vale administrators: {error}"))?;
		if enabled_admins <= 1 {
			return Ok(see_other("/settings?account=last-admin"));
		}
	}
	if target_disabled {
		transaction
			.execute("UPDATE users SET disabled_at = NULL, updated_at = ?1 WHERE id = ?2", params![now(), target_id])
			.map_err(|error| format!("Unable to enable the Vale account: {error}"))?;
		transaction.commit().map_err(|error| format!("Unable to finish the account status change: {error}"))?;
		Ok(see_other("/settings?account=user-enabled"))
	} else {
		transaction
			.execute("UPDATE users SET disabled_at = ?1, updated_at = ?1 WHERE id = ?2", params![now(), target_id])
			.map_err(|error| format!("Unable to disable the Vale account: {error}"))?;
		transaction
			.execute("DELETE FROM sessions WHERE user_id = ?1", params![target_id])
			.map_err(|error| format!("Unable to revoke the disabled account sessions: {error}"))?;
		transaction.commit().map_err(|error| format!("Unable to finish the account status change: {error}"))?;
		Ok(see_other("/settings?account=user-disabled"))
	}
}

pub async fn reset_user_password_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(admin) = context(&request).cloned().filter(|context| context.is_admin) else {
		return Ok(plain_response(StatusCode::FORBIDDEN, "Administrator access is required."));
	};
	let target_id = request.param("id").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default();
	if admin.user_id == Some(target_id) {
		return Ok(see_other("/settings?account=use-own-password-form"));
	}
	let form = read_form(request).await?;
	if let Err(error) = validate_password(form_value(&form, "password"), form_value(&form, "password_confirm")) {
		return Ok(see_other(&format!("/settings?account={}", status_code(&error))));
	}
	let encoded = password_hash(form_value(&form, "password"))?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin the password reset: {error}"))?;
	let changed = transaction
		.execute("UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3", params![encoded, now(), target_id])
		.map_err(|error| format!("Unable to reset the Vale password: {error}"))?;
	if changed == 0 {
		return Ok(see_other("/settings?account=user-missing"));
	}
	transaction
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![target_id])
		.map_err(|error| format!("Unable to revoke the reset account sessions: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish the password reset: {error}"))?;
	Ok(see_other("/settings?account=password-reset"))
}

pub fn current_account(request: &Request<Body>) -> Option<AccountView> {
	context(request).and_then(|context| {
		context.user_id.map(|_| AccountView {
			username: context.username.clone(),
			display_name: context.display_name.clone(),
			is_admin: context.is_admin,
		})
	})
}

pub fn accounts(request: &Request<Body>) -> Result<Vec<AccountSummary>, String> {
	let Some(context) = context(request).filter(|context| context.is_admin) else {
		return Ok(Vec::new());
	};
	let connection = open_database()?;
	let mut statement = connection
		.prepare("SELECT id, username, display_name, is_admin, disabled_at IS NOT NULL FROM users ORDER BY username COLLATE NOCASE")
		.map_err(|error| format!("Unable to prepare the account list: {error}"))?;
	let rows = statement
		.query_map([], |row| {
			let id = row.get::<_, i64>(0)?;
			Ok(AccountSummary {
				id,
				username: row.get(1)?,
				display_name: row.get(2)?,
				is_admin: row.get::<_, i64>(3)? != 0,
				is_disabled: row.get::<_, i64>(4)? != 0,
				is_current: context.user_id == Some(id),
			})
		})
		.map_err(|error| format!("Unable to list Vale accounts: {error}"))?;
	rows
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| format!("Unable to read the Vale account list: {error}"))
}

fn truncate(value: &str, max_chars: usize) -> String {
	value.chars().take(max_chars).collect()
}

pub fn record_post_view(request: &Request<Body>, post: &Post) -> Result<(), String> {
	let Some(context) = context(request) else {
		return Ok(());
	};
	let connection = open_database()?;
	let timestamp = now();
	connection
		.execute(
			"INSERT INTO post_history (profile_id, post_id, title, community, permalink, first_viewed_at, last_viewed_at, view_count)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
			 ON CONFLICT(profile_id, post_id) DO UPDATE SET
				title = excluded.title,
				community = excluded.community,
				permalink = excluded.permalink,
				last_viewed_at = excluded.last_viewed_at,
				view_count = post_history.view_count + 1",
			params![
				context.profile_id,
				truncate(&post.id, 80),
				truncate(&post.title, 500),
				truncate(&post.community, 120),
				truncate(&post.permalink, 1_000),
				timestamp
			],
		)
		.map_err(|error| format!("Unable to record Vale reading history: {error}"))?;
	connection
		.execute(
			"DELETE FROM post_history WHERE profile_id = ?1 AND last_viewed_at < ?2",
			params![context.profile_id, timestamp - HISTORY_RETENTION_SECONDS],
		)
		.map_err(|error| format!("Unable to expire Vale reading history: {error}"))?;
	connection
		.execute(
			"DELETE FROM post_history WHERE profile_id = ?1 AND post_id IN (
				SELECT post_id FROM post_history WHERE profile_id = ?1 ORDER BY last_viewed_at DESC LIMIT -1 OFFSET ?2
			)",
			params![context.profile_id, HISTORY_LIMIT],
		)
		.map_err(|error| format!("Unable to bound Vale reading history: {error}"))?;
	Ok(())
}

pub async fn history_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(context) = context(&request).cloned() else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	let connection = open_database()?;
	let mut statement = connection
		.prepare(
			"SELECT title, community, permalink, last_viewed_at, view_count
			 FROM post_history WHERE profile_id = ?1 ORDER BY last_viewed_at DESC LIMIT 250",
		)
		.map_err(|error| format!("Unable to prepare Vale history: {error}"))?;
	let entries = statement
		.query_map(params![context.profile_id], |row| {
			let timestamp = row.get::<_, i64>(3)?;
			Ok(HistoryEntry {
				title: row.get(0)?,
				community: row.get(1)?,
				permalink: row.get(2)?,
				viewed: crate::utils::time(timestamp as f64).0,
				view_count: row.get(4)?,
			})
		})
		.map_err(|error| format!("Unable to query Vale history: {error}"))?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| format!("Unable to read Vale history: {error}"))?;
	Ok(template(&HistoryTemplate {
		prefs: Preferences::new(&request),
		url: "/history".to_string(),
		entries,
	}))
}

pub async fn history_clear_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile_id) = context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	open_database()?
		.execute("DELETE FROM post_history WHERE profile_id = ?1", params![profile_id])
		.map_err(|error| format!("Unable to clear Vale history: {error}"))?;
	Ok(see_other("/history"))
}

pub async fn health() -> Result<Response<Body>, String> {
	if mode() != ProfileMode::Browser {
		let connection = open_database()?;
		initialize_schema(&connection)?;
	}
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header("content-type", "text/plain; charset=utf-8")
			.header("cache-control", "no-store")
			.body(Body::from("ok\n"))
			.unwrap_or_default(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use route_recognizer::Params;

	fn temporary_database() -> PathBuf {
		std::env::temp_dir().join(format!("vale-account-test-{}.sqlite3", uuid::Uuid::new_v4()))
	}

	fn remove_database(path: &Path) {
		let _ = fs::remove_file(path);
		let mut wal = path.as_os_str().to_os_string();
		wal.push("-wal");
		let _ = fs::remove_file(PathBuf::from(wal));
		let mut shm = path.as_os_str().to_os_string();
		shm.push("-shm");
		let _ = fs::remove_file(PathBuf::from(shm));
	}

	#[test]
	fn validates_account_credentials() {
		assert_eq!(validate_username("Reader.One").unwrap(), "reader.one");
		assert!(validate_username("two words").is_err());
		assert!(validate_password("a sufficiently long password", "a sufficiently long password").is_ok());
		assert!(validate_password("short", "short").is_err());
	}

	#[test]
	fn login_throttle_keys_are_bounded() {
		let username = "A".repeat(MAX_LOGIN_THROTTLE_KEY_CHARS + 32);
		assert_eq!(login_throttle_key(&username).len(), MAX_LOGIN_THROTTLE_KEY_CHARS);
	}

	#[test]
	fn password_hashes_are_salted_and_verifiable() {
		let first = password_hash("correct horse battery staple").unwrap();
		let second = password_hash("correct horse battery staple").unwrap();
		assert_ne!(first, second);
		assert!(password_matches("correct horse battery staple", &first));
		assert!(!password_matches("wrong password", &first));
	}

	#[test]
	fn new_sessions_leave_legacy_user_agent_empty() {
		let path = temporary_database();
		let connection = open_database_at(&path).unwrap();
		initialize_schema(&connection).unwrap();
		let timestamp = now();
		connection
			.execute(
				"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES ('owner', 'Owner', 'test-only-hash', 1, ?1, ?1)",
				params![timestamp],
			)
			.unwrap();
		let user_id = connection.last_insert_rowid();
		insert_session(&connection, user_id).unwrap();
		let user_agent: String = connection
			.query_row("SELECT user_agent FROM sessions WHERE user_id = ?1", params![user_id], |row| row.get(0))
			.unwrap();
		assert!(user_agent.is_empty());
		drop(connection);
		remove_database(&path);
	}

	#[test]
	fn offline_password_reset_rotates_hash_revokes_target_sessions_and_preserves_other_accounts() {
		let path = temporary_database();
		let mut connection = open_database_at(&path).unwrap();
		initialize_schema(&connection).unwrap();
		let timestamp = now();
		let old_hash = password_hash("old password for owner").unwrap();
		let other_hash = password_hash("old password for reader").unwrap();
		connection
			.execute(
				"INSERT INTO users (username, display_name, password_hash, is_admin, disabled_at, created_at, updated_at) VALUES ('owner', 'Owner', ?1, 1, ?2, ?3, ?4)",
				params![old_hash, timestamp - 100, timestamp - 100, timestamp - 100],
			)
			.unwrap();
		let owner_id = connection.last_insert_rowid();
		connection
			.execute(
				"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES ('reader', 'Reader', ?1, 0, ?2, ?2)",
				params![other_hash, timestamp - 100],
			)
			.unwrap();
		let reader_id = connection.last_insert_rowid();
		insert_session(&connection, owner_id).unwrap();
		insert_session(&connection, owner_id).unwrap();
		insert_session(&connection, reader_id).unwrap();

		let replacement_hash = password_hash("new password for owner").unwrap();
		reset_password_offline_in_connection(&mut connection, "owner", &replacement_hash).unwrap();

		let (stored_hash, updated_at, disabled_at): (String, i64, Option<i64>) = connection
			.query_row("SELECT password_hash, updated_at, disabled_at FROM users WHERE id = ?1", params![owner_id], |row| {
				Ok((row.get(0)?, row.get(1)?, row.get(2)?))
			})
			.unwrap();
		assert_ne!(stored_hash, old_hash);
		assert!(password_matches("new password for owner", &stored_hash));
		assert!(updated_at >= timestamp);
		assert_eq!(disabled_at, Some(timestamp - 100));
		let owner_sessions: i64 = connection
			.query_row("SELECT COUNT(*) FROM sessions WHERE user_id = ?1", params![owner_id], |row| row.get(0))
			.unwrap();
		let reader_sessions: i64 = connection
			.query_row("SELECT COUNT(*) FROM sessions WHERE user_id = ?1", params![reader_id], |row| row.get(0))
			.unwrap();
		assert_eq!(owner_sessions, 0);
		assert_eq!(reader_sessions, 1);
		let reader_stored_hash: String = connection
			.query_row("SELECT password_hash FROM users WHERE id = ?1", params![reader_id], |row| row.get(0))
			.unwrap();
		assert_eq!(reader_stored_hash, other_hash);
		drop(connection);
		remove_database(&path);
	}

	#[test]
	fn offline_password_reset_rolls_back_when_session_revocation_fails() {
		let path = temporary_database();
		let mut connection = open_database_at(&path).unwrap();
		initialize_schema(&connection).unwrap();
		let timestamp = now();
		let old_hash = password_hash("old password for rollback").unwrap();
		connection
			.execute(
				"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES ('owner', 'Owner', ?1, 1, ?2, ?3)",
				params![old_hash, timestamp - 100, timestamp - 100],
			)
			.unwrap();
		let owner_id = connection.last_insert_rowid();
		insert_session(&connection, owner_id).unwrap();
		connection
			.execute_batch("CREATE TRIGGER reject_offline_session_revoke BEFORE DELETE ON sessions BEGIN SELECT RAISE(ABORT, 'test-only failure'); END;")
			.unwrap();

		let replacement_hash = password_hash("new password for rollback").unwrap();
		let error = reset_password_offline_in_connection(&mut connection, "owner", &replacement_hash).unwrap_err();
		assert_eq!(error, "Unable to revoke the Vale account sessions.");
		let (stored_hash, updated_at): (String, i64) = connection
			.query_row("SELECT password_hash, updated_at FROM users WHERE id = ?1", params![owner_id], |row| {
				Ok((row.get(0)?, row.get(1)?))
			})
			.unwrap();
		assert_eq!(stored_hash, old_hash);
		assert_eq!(updated_at, timestamp - 100);
		let sessions: i64 = connection
			.query_row("SELECT COUNT(*) FROM sessions WHERE user_id = ?1", params![owner_id], |row| row.get(0))
			.unwrap();
		assert_eq!(sessions, 1);
		drop(connection);
		remove_database(&path);
	}

	#[test]
	fn private_media_assets_are_cacheable_without_exposing_dynamic_pages() {
		for path in [
			"/img/example.jpg",
			"/preview/pre/example.jpg",
			"/thumb/a/example.jpg",
			"/hls/example/HLSPlaylist.m3u8",
			"/vid/example/720.mp4",
		] {
			assert!(is_private_media_asset(path), "{path} should use the authenticated media cache policy");
		}
		for path in ["/", "/settings", "/r/homelab", "/download/gallery"] {
			assert!(!is_private_media_asset(path), "{path} must remain an uncached profile response");
		}
		for path in [
			"/fonts/source-sans-3.woff2",
			"/fonts/source-serif-4.woff2",
			"/scenes/vale-dark.avif",
			"/scenes/vale-light.webp",
		] {
			assert!(is_public_asset(path), "{path} must remain available to login and setup pages");
		}
	}

	fn post_request(origin: &str, host: &str) -> Request<Body> {
		Request::builder()
			.method(Method::POST)
			.uri("/settings/archive-storage")
			.header("origin", origin)
			.header("host", host)
			.body(Body::empty())
			.unwrap()
	}

	#[test]
	fn form_mutations_accept_the_configured_public_origin() {
		let request = post_request("https://vale.example.com", "127.0.0.1:8080");
		assert!(mutation_is_same_origin_with_expected(&request, Some("https://vale.example.com/")));
	}

	#[test]
	fn form_mutations_accept_the_current_request_origin() {
		let request = post_request("http://127.0.0.1:3101", "127.0.0.1:3101");
		assert!(mutation_is_same_origin_with_expected(&request, Some("https://vale.example.com")));
	}

	#[test]
	fn form_mutations_reject_hostile_or_malformed_origins() {
		let hostile = post_request("https://evil.example", "127.0.0.1:3101");
		assert!(!mutation_is_same_origin_with_expected(&hostile, Some("https://vale.example.com")));

		let opaque = post_request("null", "127.0.0.1:3101");
		assert!(!mutation_is_same_origin_with_expected(&opaque, Some("https://vale.example.com")));

		let wrong_scheme = Request::builder()
			.method(Method::POST)
			.uri("/settings/archive-storage")
			.header("origin", "http://vale.example.com")
			.header("host", "vale.example.com")
			.header("x-forwarded-proto", "https")
			.body(Body::empty())
			.unwrap();
		assert!(!mutation_is_same_origin_with_expected(&wrong_scheme, Some("https://other.example.com")));
	}

	#[test]
	fn hide_return_targets_reject_normalization_and_traversal_attacks() {
		for valid in [
			"/",
			"/f/quiet/new",
			"/search?q=rust%20reader&sort=new",
			"/caf%C3%A9",
			"/user/reader/submitted?after=t3_next",
		] {
			assert_eq!(strict_return_to(valid).as_deref(), Some(valid), "{valid}");
		}
		for invalid in [
			"",
			"relative/path",
			"//evil.example/path",
			"https://evil.example/path",
			"/\\evil.example/path",
			"/safe#fragment",
			"/a/../settings",
			"/a/%2e%2e/settings",
			"/a/%252e%252e/settings",
			"/%2f%2fevil.example/path",
			"%2Fsettings",
			"%252Fsettings",
			"/café",
			"/space here",
			"/bad%encoding",
			"/control%0aheader",
		] {
			assert!(strict_return_to(invalid).is_none(), "{invalid}");
		}
		assert!(strict_return_to(&format!("/{}", "a".repeat(MAX_RETURN_TO_BYTES))).is_none());
	}

	#[test]
	fn hide_referer_fallback_is_same_origin_and_relative() {
		let same_origin = Request::builder()
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://vale.example.test/f/news/new?after=t3_old")
			.body(Body::empty())
			.unwrap();
		assert_eq!(validated_referer_target(&same_origin).as_deref(), Some("/f/news/new?after=t3_old"));

		let cross_origin = Request::builder()
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://evil.example/f/news/new")
			.body(Body::empty())
			.unwrap();
		assert!(validated_referer_target(&cross_origin).is_none());

		let encoded_relative = Request::builder()
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "%2Fsettings")
			.body(Body::empty())
			.unwrap();
		assert!(validated_referer_target(&encoded_relative).is_none());

		for referer in [
			"https://vale.example.test/a/../settings",
			"https://vale.example.test/a/%2e%2e/settings",
			"https://vale.example.test\\@evil.example/settings",
		] {
			let request = Request::builder()
				.uri("/posts/abc/hide")
				.header("host", "vale.example.test")
				.header("x-forwarded-proto", "https")
				.header("referer", referer)
				.body(Body::empty())
				.unwrap();
			assert!(validated_referer_target(&request).is_none(), "{referer}");
		}
	}

	#[tokio::test]
	async fn ordinary_hide_prefers_valid_form_target_and_falls_back_to_referer() {
		let mut form_target = Request::builder()
			.method(Method::POST)
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://vale.example.test/f/fallback/new")
			.body(Body::from("return_to=%2Fsearch%3Fq%3Drust%26sort%3Dnew"))
			.unwrap();
		assert_eq!(hide_redirect_target(&mut form_target).await, "/search?q=rust&sort=new");

		let mut invalid_form_target = Request::builder()
			.method(Method::POST)
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://vale.example.test/f/fallback/new")
			.body(Body::from("return_to=%2F..%2Fsettings"))
			.unwrap();
		assert_eq!(hide_redirect_target(&mut invalid_form_target).await, "/f/fallback/new");

		let mut encoded_leading_slash = Request::builder()
			.method(Method::POST)
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://vale.example.test/f/fallback/new")
			.body(Body::from("return_to=%252Fsettings"))
			.unwrap();
		assert_eq!(hide_redirect_target(&mut encoded_leading_slash).await, "/f/fallback/new");

		let mut invalid_utf8_target = Request::builder()
			.method(Method::POST)
			.uri("/posts/abc/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://vale.example.test/f/fallback/new")
			.body(Body::from("return_to=%FF"))
			.unwrap();
		assert_eq!(hide_redirect_target(&mut invalid_utf8_target).await, "/f/fallback/new");
	}

	#[test]
	fn enhanced_hide_header_and_response_are_exact() {
		let enhanced = Request::builder().header("x-vale-enhanced", "hide-v1").body(Body::empty()).unwrap();
		assert!(is_enhanced_hide(&enhanced));
		let wrong = Request::builder().header("x-vale-enhanced", "hide-v2").body(Body::empty()).unwrap();
		assert!(!is_enhanced_hide(&wrong));
		let duplicate = Request::builder()
			.header("x-vale-enhanced", "hide-v1")
			.header("x-vale-enhanced", "hide-v1")
			.body(Body::empty())
			.unwrap();
		assert!(!is_enhanced_hide(&duplicate));

		let response = hide_mutation_response(true, String::new());
		assert_eq!(response.status(), StatusCode::NO_CONTENT);
		assert!(response.headers().get(header::LOCATION).is_none());
		let response = hide_mutation_response(false, "/f/news/new".to_string());
		assert_eq!(response.status(), StatusCode::SEE_OTHER);
		assert_eq!(response.headers()[header::LOCATION], "/f/news/new");
	}

	#[tokio::test]
	async fn hide_and_unhide_handlers_use_exact_enhanced_and_native_statuses() {
		let mut enhanced = Request::builder()
			.method(Method::POST)
			.uri("/posts/abc123/hide")
			.header("x-vale-enhanced", "hide-v1")
			.body(Body::from("return_to=%2Fignored"))
			.unwrap();
		let mut params = Params::new();
		params.insert("id".to_string(), "abc123".to_string());
		enhanced.set_params(params);
		let response = hide_post_post(enhanced).await.unwrap();
		assert_eq!(response.status(), StatusCode::NO_CONTENT);
		assert!(response.headers().get(header::LOCATION).is_none());

		let mut native = Request::builder()
			.method(Method::POST)
			.uri("/posts/abc123/unhide")
			.header("cookie", "hidden_posts=abc123.other")
			.body(Body::from("return_to=%2Fuser%2Freader%2Fsubmitted"))
			.unwrap();
		let mut params = Params::new();
		params.insert("id".to_string(), "abc123".to_string());
		native.set_params(params);
		let response = unhide_post_post(native).await.unwrap();
		assert_eq!(response.status(), StatusCode::SEE_OTHER);
		assert_eq!(response.headers()[header::LOCATION], "/user/reader/submitted");

		let mut unicode_target = Request::builder()
			.method(Method::POST)
			.uri("/posts/unicode/hide")
			.header("host", "vale.example.test")
			.header("x-forwarded-proto", "https")
			.header("referer", "https://vale.example.test/f/fallback/new")
			.body(Body::from("return_to=%2Fcaf%C3%A9"))
			.unwrap();
		let mut params = Params::new();
		params.insert("id".to_string(), "unicode".to_string());
		unicode_target.set_params(params);
		let response = hide_post_post(unicode_target).await.unwrap();
		assert_eq!(response.status(), StatusCode::SEE_OTHER);
		assert_eq!(response.headers()[header::LOCATION], "/f/fallback/new");
	}

	#[test]
	fn form_mutations_reject_cross_site_fetches_even_when_the_origin_matches() {
		let request = Request::builder()
			.method(Method::POST)
			.uri("/feeds")
			.header("origin", "http://127.0.0.1:3101")
			.header("host", "127.0.0.1:3101")
			.header("sec-fetch-site", "cross-site")
			.body(Body::empty())
			.unwrap();
		assert!(!mutation_is_same_origin_with_expected(&request, Some("http://127.0.0.1:3101")));
	}

	#[test]
	fn older_profile_json_receives_reader_defaults() {
		let mut value = serde_json::to_value(default_preferences()).unwrap();
		let object = value.as_object_mut().unwrap();
		for key in [
			"keyboard_navigation",
			"key_next_post",
			"key_previous_post",
			"key_open_post",
			"key_toggle_preview",
			"key_hide_post",
			"hide_post_behavior",
			"archive_budget_mib",
		] {
			object.remove(key);
		}
		let preferences = deserialize_preferences(&value.to_string()).unwrap();
		assert_eq!(preferences.keyboard_navigation, "on");
		assert_eq!(preferences.key_next_post, "j");
		assert_eq!(preferences.key_previous_post, "k");
		assert_eq!(preferences.key_open_post, "Enter");
		assert_eq!(preferences.key_toggle_preview, "e");
		assert_eq!(preferences.key_hide_post, "h");
		assert_eq!(preferences.hide_post_behavior, "instant");
		assert_eq!(preferences.archive_budget_mib, 0);
	}

	#[test]
	fn archive_budget_cas_is_profile_scoped_and_survives_old_json_rewrites() {
		let mut connection = Connection::open_in_memory().unwrap();
		initialize_schema(&connection).unwrap();
		let timestamp = now();
		let serialized = serialize_preferences(&default_preferences()).unwrap();
		connection
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (NULL, 'One', ?1, ?2, ?2)",
				params![serialized, timestamp],
			)
			.unwrap();
		let first_profile = connection.last_insert_rowid();
		connection
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (NULL, 'Two', ?1, ?2, ?2)",
				params![serialized, timestamp],
			)
			.unwrap();
		let second_profile = connection.last_insert_rowid();

		assert_eq!(
			update_archive_budget_in(&mut connection, first_profile, 512, 0).unwrap(),
			ArchiveBudgetUpdate::Saved(ArchiveBudgetSetting { mib: 512, revision: 1 })
		);
		assert_eq!(
			update_archive_budget_in(&mut connection, first_profile, 768, 0).unwrap(),
			ArchiveBudgetUpdate::Conflict(ArchiveBudgetSetting { mib: 512, revision: 1 })
		);
		assert_eq!(
			update_archive_budget_in(&mut connection, second_profile, 1_024, 0).unwrap(),
			ArchiveBudgetUpdate::Saved(ArchiveBudgetSetting { mib: 1_024, revision: 1 })
		);
		let old_binary_json = serialize_preferences(&default_preferences()).unwrap();
		connection
			.execute("UPDATE profiles SET preferences_json = ?1 WHERE id = ?2", params![old_binary_json, first_profile])
			.unwrap();
		let merged = preferences_with_archive_budget(&connection, first_profile, &old_binary_json).unwrap();
		assert_eq!(merged.archive_budget_mib, 512);
		assert_eq!(archive_budget_setting_in(&connection, second_profile).unwrap().mib, 1_024);
		assert!(update_archive_budget_in(&mut connection, first_profile, 300, 1).is_err());
		assert_eq!(archive_budget_setting_in(&connection, first_profile).unwrap().mib, 512);
	}

	#[test]
	fn concurrent_archive_budget_updates_return_one_save_and_one_stale_conflict() {
		use std::sync::{Arc, Barrier};

		let path = temporary_database();
		let connection = open_database_at(&path).unwrap();
		initialize_schema(&connection).unwrap();
		let timestamp = now();
		connection
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (NULL, 'Concurrent', ?1, ?2, ?2)",
				params![serialize_preferences(&default_preferences()).unwrap(), timestamp],
			)
			.unwrap();
		let profile_id = connection.last_insert_rowid();
		drop(connection);
		let barrier = Arc::new(Barrier::new(2));
		let handles = [512, 1_024].map(|budget| {
			let path = path.clone();
			let barrier = barrier.clone();
			std::thread::spawn(move || {
				let mut connection = open_database_at(&path).unwrap();
				barrier.wait();
				update_archive_budget_in(&mut connection, profile_id, budget, 0).unwrap()
			})
		});
		let results = handles.map(|handle| handle.join().unwrap());
		assert_eq!(results.iter().filter(|result| matches!(result, ArchiveBudgetUpdate::Saved(_))).count(), 1);
		assert_eq!(results.iter().filter(|result| matches!(result, ArchiveBudgetUpdate::Conflict(_))).count(), 1);
		let connection = open_database_at(&path).unwrap();
		let current = archive_budget_setting_in(&connection, profile_id).unwrap();
		assert_eq!(current.revision, 1);
		assert!(matches!(current.mib, 512 | 1_024));
		drop(connection);
		remove_database(&path);
	}

	#[test]
	fn new_profiles_default_to_instant_hide() {
		assert_eq!(default_preferences().hide_post_behavior, "instant");
	}

	#[test]
	fn database_schema_preserves_isolated_profiles() {
		let path = temporary_database();
		let connection = open_database_at(&path).unwrap();
		initialize_schema(&connection).unwrap();
		let timestamp = now();
		let password = password_hash("test-only password one").unwrap();
		connection
			.execute(
				"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES ('one', 'One', ?1, 1, ?2, ?2)",
				params![password, timestamp],
			)
			.unwrap();
		let user_one = connection.last_insert_rowid();
		let mut first_preferences = default_preferences();
		first_preferences.subscriptions = vec!["rust".to_string()];
		connection
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (?1, 'One', ?2, ?3, ?3)",
				params![user_one, serialize_preferences(&first_preferences).unwrap(), timestamp],
			)
			.unwrap();
		let profile_one = connection.last_insert_rowid();
		let password = password_hash("test-only password two").unwrap();
		connection
			.execute(
				"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES ('two', 'Two', ?1, 0, ?2, ?2)",
				params![password, timestamp],
			)
			.unwrap();
		let user_two = connection.last_insert_rowid();
		let mut second_preferences = default_preferences();
		second_preferences.subscriptions = vec!["anime".to_string()];
		connection
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (?1, 'Two', ?2, ?3, ?3)",
				params![user_two, serialize_preferences(&second_preferences).unwrap(), timestamp],
			)
			.unwrap();
		let profile_two = connection.last_insert_rowid();
		let first: String = connection
			.query_row("SELECT preferences_json FROM profiles WHERE id = ?1", params![profile_one], |row| row.get(0))
			.unwrap();
		let second: String = connection
			.query_row("SELECT preferences_json FROM profiles WHERE id = ?1", params![profile_two], |row| row.get(0))
			.unwrap();
		assert_eq!(deserialize_preferences(&first).unwrap().subscriptions, vec!["rust"]);
		assert_eq!(deserialize_preferences(&second).unwrap().subscriptions, vec!["anime"]);
		connection
			.execute(
				"INSERT INTO hidden_posts (profile_id, post_id, hidden_at) VALUES (?1, 'same-post', ?2)",
				params![profile_one, timestamp],
			)
			.unwrap();
		let first_hidden: i64 = connection
			.query_row("SELECT COUNT(*) FROM hidden_posts WHERE profile_id = ?1", params![profile_one], |row| row.get(0))
			.unwrap();
		let second_hidden: i64 = connection
			.query_row("SELECT COUNT(*) FROM hidden_posts WHERE profile_id = ?1", params![profile_two], |row| row.get(0))
			.unwrap();
		assert_eq!(first_hidden, 1);
		assert_eq!(second_hidden, 0);
		drop(connection);
		remove_database(&path);
	}
}
