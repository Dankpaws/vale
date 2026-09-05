use crate::{
	account,
	client::{json, CLIENT, OAUTH_CLIENT},
	config::get_setting,
	html::normalize_archive_comment_headings,
	media,
	server::RequestExt,
	utils::{parse_post, rewrite_emotes, rewrite_urls, template, val, Preferences},
};
use askama::Template;
use futures_lite::StreamExt;
use hyper::{header, Body, Request, Response, StatusCode};
use lol_html::{element, rewrite_str, RewriteStrSettings};
use regex::Regex;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
	collections::{HashMap, HashSet, VecDeque},
	env, io,
	net::{IpAddr, Ipv4Addr, SocketAddr},
	path::{Component, Path, PathBuf},
	sync::{
		atomic::{AtomicUsize, Ordering},
		mpsc::{sync_channel, Receiver, SyncSender, TrySendError},
		LazyLock,
	},
};
use tokio::{
	fs,
	io::AsyncWriteExt,
	net::lookup_host,
	time::{timeout, Duration},
};
use url::Url;
use wreq::redirect::Policy;

const MAX_ARCHIVE_COMMENTS: usize = 5_000;
const ARCHIVE_READER_VERSION: u16 = 3;
const MAX_MORE_REQUESTS: usize = 30;
const MORE_CHILDREN_BATCH: usize = 100;
const MAX_INLINE_MEDIA: usize = 160;
const MAX_REDDIT_ASSET_BYTES: u64 = 768 * 1024 * 1024;
const MAX_EXTERNAL_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXTERNAL_REQUISITE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_EXTERNAL_REQUISITES: usize = 64;
const MAX_EXTERNAL_REDIRECTS: usize = 5;
const MAX_EXTERNAL_REDIRECT_LOCATION_BYTES: usize = 8 * 1024;
const MAX_PENDING_ARCHIVES: usize = 32;
// Archive metadata is durable account state even when a capture fails. Keep
// both a per-profile and instance-wide ceiling so retries cannot grow the
// SQLite table without bound. Failed rows remain outside byte-quota accounting
// and are pruned first when a profile needs another metadata slot.
const MAX_ARCHIVE_RECORDS_PER_PROFILE: i64 = 500;
const MAX_ARCHIVE_RECORDS_GLOBAL: i64 = 5_000;
const MAX_ARCHIVE_LIST_ENTRIES: i64 = MAX_ARCHIVE_RECORDS_PER_PROFILE;
const EXTERNAL_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
const TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const MIB: u64 = 1024 * 1024;
const ARCHIVE_BUDGET_STEP_MIB: u64 = 256;
const MIN_CAPTURE_RESERVATION_BYTES: u64 = 64 * MIB;
// Keep enough of every reservation for the manifest, standalone reader, fonts,
// mark, and notices. Captured/source assets may not consume this allowance.
const FINALIZATION_RESERVE_BYTES: u64 = 64 * MIB;
const DEFAULT_ITEM_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_TOTAL_QUOTA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[cfg(target_os = "linux")]
const ARCHIVE_WORKER_NICE: i32 = 10;
pub const ARCHIVE_CSP: &str = "default-src 'none'; script-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; connect-src 'none'; frame-src 'none'; frame-ancestors 'none'; object-src 'none'; form-action 'none'; base-uri 'none'";
const READER_SUPPORT_ASSETS: [(&str, &str, &[u8]); 5] = [
	(
		"files/reader/source-sans-3.woff2",
		"font/woff2",
		include_bytes!("../static/fonts/SourceSans3VF-Upright.ttf.woff2"),
	),
	(
		"files/reader/source-serif-4.woff2",
		"font/woff2",
		include_bytes!("../static/fonts/SourceSerif4-Regular.ttf.woff2"),
	),
	("files/reader/vale-mark.svg", "image/svg+xml", include_bytes!("../static/vale-mark-flat.svg")),
	("files/reader/OFL.txt", "text/plain; charset=utf-8", include_bytes!("../static/fonts/OFL.txt")),
	("files/reader/AGPL-3.0.txt", "text/plain; charset=utf-8", include_bytes!("../LICENSE")),
];

static PENDING_ARCHIVES: AtomicUsize = AtomicUsize::new(0);
static ARCHIVE_WORKER: LazyLock<Result<SyncSender<QueuedArchiveJob>, String>> = LazyLock::new(start_archive_worker);
static HTML_MEDIA_URL: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r#"(?i)\b(?:src|poster|href)\s*=\s*["'](?P<url>/[A-Za-z0-9._~!$&'()*+,;=:@%/?#-]+)["']"#).expect("archive media URL regex is valid"));
static PAGE_RESOURCE_URL: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r#"(?i)\b(?P<attribute>src|poster|href)\s*=\s*(?P<quote>["'])(?P<url>[^"'<> \t\r\n]+)["']"#).expect("external page resource regex is valid"));
static ACTIVE_HTML: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r"(?is)<(?:script|iframe|object|embed|form)\b[^>]*>.*?</(?:script|iframe|object|embed|form)\s*>|<(?:script|iframe|object|embed|form)\b[^>]*/?>")
		.expect("archive active HTML regex is valid")
});
static REFRESH_META: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r#"(?is)<meta\b[^>]*http-equiv\s*=\s*["']?refresh["']?[^>]*>"#).expect("archive refresh meta regex is valid"));
static BASE_ELEMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<base\b[^>]*>").expect("archive base element regex is valid"));
static EVENT_ATTRIBUTE: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r#"(?is)\s+on[a-z0-9_-]+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)"#).expect("archive event attribute regex is valid"));
static JAVASCRIPT_URL: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r#"(?is)\b(href|src)\s*=\s*(?:"\s*javascript:[^"]*"|'\s*javascript:[^']*'|\s*javascript:[^\s>]*)"#).expect("archive javascript URL regex is valid")
});

#[derive(Clone, Debug)]
struct ArchiveJob {
	id: String,
	profile_id: i64,
	post_id: String,
	reservation_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveIssue {
	pub area: String,
	pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveAsset {
	pub path: String,
	pub original_url: String,
	pub content_type: String,
	pub bytes: u64,
	pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct GeneratedArchiveAsset {
	pub path: String,
	pub content_type: String,
	pub bytes: u64,
	pub sha256: String,
}

fn default_reader_version() -> u16 {
	1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchivedMedia {
	pub kind: String,
	pub path: String,
	pub caption: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchivedPost {
	pub id: String,
	pub title: String,
	pub community: String,
	pub author: String,
	pub permalink: String,
	pub source_url: String,
	pub body_html: String,
	pub post_type: String,
	pub created: String,
	pub score: i64,
	pub upvote_ratio: i64,
	pub media: Vec<ArchivedMedia>,
	pub source_snapshot: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Template)]
#[template(path = "archive_comment.html")]
pub struct ArchivedComment {
	pub id: String,
	pub parent_id: String,
	pub author: String,
	pub body_html: String,
	pub created: String,
	pub score: i64,
	pub score_hidden: bool,
	pub replies: Vec<ArchivedComment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveManifest {
	pub format: String,
	#[serde(default = "default_reader_version")]
	pub reader_version: u16,
	pub captured_at: i64,
	pub comment_count: usize,
	pub post: ArchivedPost,
	pub comments: Vec<ArchivedComment>,
	pub assets: Vec<ArchiveAsset>,
	#[serde(default)]
	pub generated_assets: Vec<GeneratedArchiveAsset>,
	pub issues: Vec<ArchiveIssue>,
	pub initial_reddit_json: Value,
	pub additional_comment_things: Vec<Value>,
}

#[derive(Template)]
#[template(path = "archive_snapshot.html")]
struct ArchiveSnapshotTemplate<'a> {
	manifest: &'a ArchiveManifest,
	css: &'static str,
	csp: &'static str,
}

fn render_archive_reader(manifest: &ArchiveManifest) -> Result<String, String> {
	if manifest.reader_version > ARCHIVE_READER_VERSION {
		return Err(format!(
			"Archive reader version {} is newer than this Vale build supports; regeneration was refused.",
			manifest.reader_version
		));
	}
	ArchiveSnapshotTemplate {
		manifest,
		css: include_str!("../static/archive.css"),
		csp: ARCHIVE_CSP,
	}
	.render()
	.map_err(|error| format!("Unable to render the standalone saved-post reader: {error}"))
}

fn build_archive_documents(manifest: ArchiveManifest) -> Result<(ArchiveManifest, Vec<u8>, String, String), String> {
	let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(|error| format!("Unable to serialize the saved-post manifest: {error}"))?;
	let index = render_archive_reader(&manifest)?;
	let issues_json = serde_json::to_string(&manifest.issues).map_err(|error| format!("Unable to serialize the capture report: {error}"))?;
	Ok((manifest, manifest_json, index, issues_json))
}

#[derive(Clone, Debug)]
pub struct ArchiveEntryView {
	pub id: String,
	pub post_id: String,
	pub permalink: String,
	pub title: String,
	pub community: String,
	pub source_url: String,
	pub status: String,
	pub status_label: String,
	pub created: String,
	pub captured: String,
	pub comment_count: i64,
	pub asset_count: i64,
	pub generated_asset_count: i64,
	pub local_file_count: i64,
	pub total_bytes: u64,
	pub total_size: String,
	pub issues: Vec<ArchiveIssue>,
	pub error: String,
}

impl ArchiveEntryView {
	fn is_pending(&self) -> bool {
		matches!(self.status.as_str(), "queued" | "capturing")
	}

	pub(crate) fn is_viewable(&self) -> bool {
		matches!(self.status.as_str(), "ready" | "partial")
	}
}

#[derive(Template)]
#[template(path = "saved.html")]
struct SavedTemplate {
	prefs: Preferences,
	url: String,
	entries: Vec<ArchiveEntryView>,
	quota: ArchiveQuotaSnapshot,
}

#[derive(Template)]
#[template(path = "saved_detail.html")]
struct SavedDetailTemplate {
	prefs: Preferences,
	url: String,
	entry: ArchiveEntryView,
}

#[derive(Clone, Debug, Default)]
pub struct ArchiveQuotaSnapshot {
	pub profile_used_bytes: u64,
	pub profile_reserved_bytes: u64,
	pub instance_used_bytes: u64,
	pub instance_reserved_bytes: u64,
	pub effective_limit_bytes: u64,
	pub instance_limit_bytes: u64,
	pub configured_budget_mib: u64,
	pub maximum_custom_mib: u64,
	pub used_size: String,
	pub reserved_size: String,
	pub effective_limit_size: String,
	pub instance_limit_size: String,
	pub over_by_size: String,
	pub instance_over_by_size: String,
	pub is_over_budget: bool,
	pub instance_exhausted: bool,
	pub custom_budget_available: bool,
}

struct CaptureContext {
	directory: PathBuf,
	item_limit: u64,
	total_bytes: u64,
	assets: Vec<ArchiveAsset>,
	generated_assets: Vec<GeneratedArchiveAsset>,
	asset_paths: HashMap<String, String>,
	issues: Vec<ArchiveIssue>,
}

struct CommentCapture {
	comments: Vec<ArchivedComment>,
	things: Vec<Value>,
	count: usize,
	issues: Vec<ArchiveIssue>,
}

fn archive_root() -> PathBuf {
	get_setting("VALE_ARCHIVE_DIR")
		.filter(|value| !value.trim().is_empty())
		.map(PathBuf::from)
		.or_else(|| env::var_os("VALE_ARCHIVE_DIR").map(PathBuf::from))
		.unwrap_or_else(|| PathBuf::from("vale-archives"))
}

fn configured_bytes(name: &str, default: u64, minimum: u64, maximum: u64) -> u64 {
	get_setting(name)
		.or_else(|| env::var(name).ok())
		.and_then(|value| value.parse::<u64>().ok())
		.filter(|value| (*value >= minimum) && (*value <= maximum))
		.unwrap_or(default)
}

fn item_quota() -> u64 {
	configured_bytes("VALE_ARCHIVE_ITEM_MAX_BYTES", DEFAULT_ITEM_QUOTA_BYTES, 64 * 1024 * 1024, 4 * 1024 * 1024 * 1024)
}

fn total_quota() -> u64 {
	configured_bytes("VALE_ARCHIVE_TOTAL_MAX_BYTES", DEFAULT_TOTAL_QUOTA_BYTES, MIB, 64 * 1024 * 1024 * 1024)
}

fn archive_directory(profile_id: i64, archive_id: &str) -> PathBuf {
	archive_root().join(profile_id.to_string()).join(archive_id)
}

fn partial_directory(profile_id: i64, archive_id: &str) -> PathBuf {
	archive_root().join(profile_id.to_string()).join(format!(".{archive_id}.partial"))
}

fn plain_response(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(Body::from(message.to_string()))
		.unwrap_or_default()
}

fn see_other(path: &str) -> Response<Body> {
	Response::builder()
		.status(StatusCode::SEE_OTHER)
		.header(header::LOCATION, path)
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(Body::empty())
		.unwrap_or_default()
}

pub(crate) fn format_timestamp(timestamp: impl std::borrow::Borrow<i64>) -> String {
	chrono::DateTime::from_timestamp(*timestamp.borrow(), 0)
		.map(|value| value.format("%Y-%m-%d %H:%M UTC").to_string())
		.unwrap_or_else(|| "Unknown time".to_string())
}

pub fn format_bytes(bytes: u64) -> String {
	const KIB: f64 = 1024.0;
	const MIB: f64 = KIB * 1024.0;
	const GIB: f64 = MIB * 1024.0;
	let bytes = bytes as f64;
	if bytes >= GIB {
		format!("{:.2} GiB", bytes / GIB)
	} else if bytes >= MIB {
		format!("{:.1} MiB", bytes / MIB)
	} else if bytes >= KIB {
		format!("{:.1} KiB", bytes / KIB)
	} else {
		format!("{} B", bytes as u64)
	}
}

fn status_label(status: &str) -> String {
	match status {
		"queued" => "Waiting to capture".to_string(),
		"capturing" => "Capturing locally".to_string(),
		"ready" => "Complete".to_string(),
		"partial" => "Saved with omissions".to_string(),
		"failed" => "Capture failed".to_string(),
		"cleanup_failed" => "Cleanup needs attention".to_string(),
		"deleting" => "Removal needs attention".to_string(),
		_ => "Unknown".to_string(),
	}
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveEntryView> {
	let status = row.get::<_, String>(6)?;
	let created_at = row.get::<_, i64>(7)?;
	let captured_at = row.get::<_, Option<i64>>(8)?;
	let generated_asset_count = row.get::<_, i64>(11)?.max(0);
	let total_bytes = row.get::<_, i64>(12)?.max(0) as u64;
	let issues_json = row.get::<_, String>(13)?;
	Ok(ArchiveEntryView {
		id: row.get(0)?,
		post_id: row.get(1)?,
		permalink: row.get(2)?,
		title: row.get(3)?,
		community: row.get(4)?,
		source_url: row.get(5)?,
		status_label: status_label(&status),
		status,
		created: format_timestamp(created_at),
		captured: captured_at.map(format_timestamp).unwrap_or_default(),
		comment_count: row.get(9)?,
		asset_count: row.get(10)?,
		generated_asset_count,
		local_file_count: row.get::<_, i64>(10)?.max(0) + generated_asset_count + 2,
		total_bytes,
		total_size: format_bytes(total_bytes),
		issues: serde_json::from_str(&issues_json).unwrap_or_default(),
		error: row.get(14)?,
	})
}

const ENTRY_COLUMNS: &str =
	"id, post_id, permalink, title, community, source_url, status, created_at, captured_at, comment_count, asset_count, generated_asset_count, total_bytes, issues_json, error";

fn entry_for_profile(profile_id: i64, archive_id: &str) -> Result<Option<ArchiveEntryView>, String> {
	account::open_database()?
		.query_row(
			&format!("SELECT {ENTRY_COLUMNS} FROM post_archives WHERE profile_id = ?1 AND id = ?2"),
			params![profile_id, archive_id],
			row_to_entry,
		)
		.optional()
		.map_err(|error| format!("Unable to read the saved Vale post: {error}"))
}

fn entry_for_post(connection: &rusqlite::Connection, profile_id: i64, post_id: &str) -> rusqlite::Result<Option<ArchiveEntryView>> {
	connection
		.query_row(
			&format!("SELECT {ENTRY_COLUMNS} FROM post_archives WHERE profile_id = ?1 AND post_id = ?2"),
			params![profile_id, post_id],
			row_to_entry,
		)
		.optional()
}

fn archive_record_count(connection: &rusqlite::Connection, profile_id: Option<i64>) -> Result<i64, String> {
	let count = match profile_id {
		Some(profile_id) => connection.query_row("SELECT COUNT(*) FROM post_archives WHERE profile_id = ?1", params![profile_id], |row| row.get(0)),
		None => connection.query_row("SELECT COUNT(*) FROM post_archives", [], |row| row.get(0)),
	};
	count.map_err(|error| format!("Unable to count the Vale archive records: {error}"))
}

/// Remove only failed records, oldest first, while the caller holds the write
/// transaction used to admit a new record. Failed captures are never viewable
/// archives, so their retry metadata is the safe eviction tier. Returning ids
/// lets the caller remove any stale on-disk directories after commit.
fn prune_failed_records(connection: &rusqlite::Connection, profile_id: i64, limit: i64) -> Result<Vec<String>, String> {
	if limit <= 0 {
		return Ok(Vec::new());
	}
	let mut statement = connection
		.prepare(
			"SELECT id FROM post_archives \
			 WHERE profile_id = ?1 AND status = 'failed' \
			 ORDER BY updated_at ASC, created_at ASC, id ASC LIMIT ?2",
		)
		.map_err(|error| format!("Unable to prepare stale Vale archive cleanup: {error}"))?;
	let ids = statement
		.query_map(params![profile_id, limit], |row| row.get::<_, String>(0))
		.map_err(|error| format!("Unable to query stale Vale archive records: {error}"))?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| format!("Unable to read stale Vale archive records: {error}"))?;
	drop(statement);
	for id in &ids {
		connection
			.execute("DELETE FROM post_archives WHERE profile_id = ?1 AND id = ?2 AND status = 'failed'", params![profile_id, id])
			.map_err(|error| format!("Unable to prune stale Vale archive records: {error}"))?;
	}
	Ok(ids)
}

#[derive(Debug)]
enum ArchiveCapacityError {
	Limit(String),
	Database(String),
}

fn ensure_archive_record_capacity_with_limits(
	connection: &rusqlite::Connection,
	profile_id: i64,
	per_profile_limit: i64,
	global_limit: i64,
) -> Result<Vec<String>, ArchiveCapacityError> {
	if per_profile_limit < 1 || global_limit < 1 {
		return Err(ArchiveCapacityError::Limit("The Vale archive record limits must be positive.".to_string()));
	}
	let mut pruned = Vec::new();
	let profile_count = archive_record_count(connection, Some(profile_id)).map_err(ArchiveCapacityError::Database)?;
	if profile_count >= per_profile_limit {
		let needed = profile_count.saturating_sub(per_profile_limit - 1);
		pruned.extend(prune_failed_records(connection, profile_id, needed).map_err(ArchiveCapacityError::Database)?);
		if archive_record_count(connection, Some(profile_id)).map_err(ArchiveCapacityError::Database)? >= per_profile_limit {
			return Err(ArchiveCapacityError::Limit(format!(
				"This profile has reached the {per_profile_limit} saved-post record limit. Remove an archive before saving another post."
			)));
		}
	}

	let global_count = archive_record_count(connection, None).map_err(ArchiveCapacityError::Database)?;
	if global_count >= global_limit {
		// Keep account isolation intact: an admission attempt may evict only
		// failed retry metadata owned by the requesting profile. If another
		// profile fills the instance-wide bound, the caller must make room there.
		let needed = global_count.saturating_sub(global_limit - 1);
		pruned.extend(prune_failed_records(connection, profile_id, needed).map_err(ArchiveCapacityError::Database)?);
		if archive_record_count(connection, None).map_err(ArchiveCapacityError::Database)? >= global_limit {
			return Err(ArchiveCapacityError::Limit(format!(
				"This Vale instance has reached the {global_limit} saved-post record limit. Remove an archive before saving another post."
			)));
		}
	}
	Ok(pruned)
}

fn ensure_archive_record_capacity(connection: &rusqlite::Connection, profile_id: i64) -> Result<Vec<String>, ArchiveCapacityError> {
	ensure_archive_record_capacity_with_limits(connection, profile_id, MAX_ARCHIVE_RECORDS_PER_PROFILE, MAX_ARCHIVE_RECORDS_GLOBAL)
}

async fn remove_pruned_archive_files(profile_id: i64, archive_ids: &[String]) {
	for archive_id in archive_ids {
		let _ = fs::remove_dir_all(archive_directory(profile_id, archive_id)).await;
		let _ = fs::remove_dir_all(partial_directory(profile_id, archive_id)).await;
	}
}

fn bounded_archive_list_limit(limit: usize) -> i64 {
	limit.clamp(1, MAX_ARCHIVE_LIST_ENTRIES as usize) as i64
}

fn visible_entries_for_profile(connection: &rusqlite::Connection, profile_id: i64, limit: usize) -> Result<Vec<ArchiveEntryView>, String> {
	let limit = bounded_archive_list_limit(limit);
	let mut statement = connection
		.prepare(&format!(
			"SELECT {ENTRY_COLUMNS} FROM post_archives \
			 WHERE profile_id = ?1 AND status IN ('queued', 'capturing', 'ready', 'partial', 'failed', 'cleanup_failed', 'deleting') \
			 ORDER BY created_at DESC, id DESC LIMIT ?2"
		))
		.map_err(|error| format!("Unable to prepare the saved-post library: {error}"))?;
	let entries = statement
		.query_map(params![profile_id, limit], row_to_entry)
		.map_err(|error| format!("Unable to query the saved-post library: {error}"))?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| format!("Unable to read the saved-post library: {error}"))?;
	Ok(entries)
}

pub fn archive_for_post(request: &Request<Body>, post_id: &str) -> Result<Option<ArchiveEntryView>, String> {
	let Some(profile_id) = account::context(request).map(|context| context.profile_id) else {
		return Ok(None);
	};
	let connection = account::open_database()?;
	entry_for_post(&connection, profile_id, post_id).map_err(|error| format!("Unable to inspect the saved Vale post: {error}"))
}

/// Return only true profile-owned archive records for the home context rail.
/// Browser-only mode has no durable archive identity and therefore returns an
/// empty list rather than inventing saved highlights.
pub fn recent_for_profile(request: &Request<Body>, limit: usize) -> Result<Vec<ArchiveEntryView>, String> {
	let Some(profile_id) = account::context(request).map(|context| context.profile_id) else {
		return Ok(Vec::new());
	};
	let limit = limit.clamp(1, 3);
	let connection = account::open_database()?;
	visible_entries_for_profile(&connection, profile_id, limit)
}

fn summed_bytes(connection: &rusqlite::Connection, sql: &str, profile_id: Option<i64>) -> Result<u64, String> {
	let value = match profile_id {
		Some(profile_id) => connection.query_row(sql, params![profile_id], |row| row.get::<_, i64>(0)),
		None => connection.query_row(sql, [], |row| row.get::<_, i64>(0)),
	};
	value
		.map(|value| value.max(0) as u64)
		.map_err(|error| format!("Unable to measure the Vale archive library: {error}"))
}

fn actual_archive_bytes_in(connection: &rusqlite::Connection, profile_id: Option<i64>) -> Result<u64, String> {
	match profile_id {
		Some(_) => summed_bytes(
			connection,
			"SELECT COALESCE(SUM(total_bytes), 0) FROM post_archives WHERE profile_id = ?1 AND status IN ('ready', 'partial', 'capturing', 'cleanup_failed', 'deleting')",
			profile_id,
		),
		None => summed_bytes(
			connection,
			"SELECT COALESCE(SUM(total_bytes), 0) FROM post_archives WHERE status IN ('ready', 'partial', 'capturing', 'cleanup_failed', 'deleting')",
			None,
		),
	}
}

fn reserved_archive_bytes_in(connection: &rusqlite::Connection, profile_id: Option<i64>) -> Result<u64, String> {
	match profile_id {
		Some(_) => summed_bytes(
			connection,
			"SELECT COALESCE(SUM(reserved_bytes), 0) FROM archive_reservations WHERE profile_id = ?1",
			profile_id,
		),
		None => summed_bytes(connection, "SELECT COALESCE(SUM(reserved_bytes), 0) FROM archive_reservations", None),
	}
}

fn configured_profile_budget_mib(connection: &rusqlite::Connection, profile_id: i64) -> Result<u64, String> {
	connection
		.query_row(
			"SELECT archive_budget_mib FROM profile_archive_settings WHERE profile_id = ?1",
			params![profile_id],
			|row| row.get::<_, i64>(0),
		)
		.optional()
		.map(|value| value.unwrap_or_default().max(0) as u64)
		.map_err(|error| format!("Unable to read the profile archive budget: {error}"))
}

fn quota_snapshot_in(connection: &rusqlite::Connection, profile_id: i64) -> Result<ArchiveQuotaSnapshot, String> {
	let instance_limit_bytes = total_quota();
	let configured_budget_mib = configured_profile_budget_mib(connection, profile_id)?;
	let selected_bytes = configured_budget_mib.saturating_mul(MIB);
	let effective_limit_bytes = if configured_budget_mib == 0 {
		instance_limit_bytes
	} else {
		selected_bytes.min(instance_limit_bytes)
	};
	let profile_used_bytes = actual_archive_bytes_in(connection, Some(profile_id))?;
	let profile_reserved_bytes = reserved_archive_bytes_in(connection, Some(profile_id))?;
	let instance_used_bytes = actual_archive_bytes_in(connection, None)?;
	let instance_reserved_bytes = reserved_archive_bytes_in(connection, None)?;
	let maximum_custom_mib = (instance_limit_bytes / (ARCHIVE_BUDGET_STEP_MIB * MIB)) * ARCHIVE_BUDGET_STEP_MIB;
	let over_by = profile_used_bytes.saturating_sub(effective_limit_bytes);
	let instance_over_by = instance_used_bytes.saturating_sub(instance_limit_bytes);
	Ok(ArchiveQuotaSnapshot {
		profile_used_bytes,
		profile_reserved_bytes,
		instance_used_bytes,
		instance_reserved_bytes,
		effective_limit_bytes,
		instance_limit_bytes,
		configured_budget_mib,
		maximum_custom_mib,
		used_size: format_bytes(profile_used_bytes),
		reserved_size: format_bytes(profile_reserved_bytes),
		effective_limit_size: format_bytes(effective_limit_bytes),
		instance_limit_size: format_bytes(instance_limit_bytes),
		over_by_size: format_bytes(over_by),
		instance_over_by_size: format_bytes(instance_over_by),
		is_over_budget: over_by > 0,
		instance_exhausted: instance_used_bytes.saturating_add(instance_reserved_bytes) >= instance_limit_bytes,
		custom_budget_available: maximum_custom_mib >= ARCHIVE_BUDGET_STEP_MIB,
	})
}

pub fn quota_snapshot(request: &Request<Body>) -> Result<ArchiveQuotaSnapshot, String> {
	let Some(profile_id) = account::context(request).map(|context| context.profile_id) else {
		return Ok(ArchiveQuotaSnapshot::default());
	};
	quota_snapshot_in(&account::open_database()?, profile_id)
}

pub async fn list_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile_id) = account::context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required to view saved posts."));
	};
	let connection = account::open_database()?;
	let entries = visible_entries_for_profile(&connection, profile_id, MAX_ARCHIVE_LIST_ENTRIES as usize)?;
	let quota = quota_snapshot_in(&connection, profile_id)?;
	Ok(template(&SavedTemplate {
		prefs: Preferences::new(&request),
		url: "/saved".to_string(),
		entries,
		quota,
	}))
}

pub async fn detail_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile_id) = account::context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required to view that saved post."));
	};
	let archive_id = request.param("archive_id").unwrap_or_default();
	let Some(entry) = entry_for_profile(profile_id, &archive_id)? else {
		return Ok(plain_response(StatusCode::NOT_FOUND, "That saved post does not exist in this profile."));
	};
	let pending = entry.is_pending();
	let mut response = template(&SavedDetailTemplate {
		prefs: Preferences::new(&request),
		url: format!("/saved/{archive_id}"),
		entry,
	});
	if response.status() == StatusCode::OK && pending {
		response.headers_mut().insert(header::REFRESH, header::HeaderValue::from_static("3"));
	}
	Ok(response)
}

enum ArchiveAdmission {
	Existing { id: String },
	Admitted { id: String, reservation_bytes: u64, pruned: Vec<String> },
}

#[derive(Debug)]
enum ArchiveAdmissionError {
	Storage(String),
	Database(String),
}

/// Reserve one durable archive row while holding SQLite's write lock. Keeping
/// this synchronous prevents a non-Send rusqlite transaction from living
/// across the request future's filesystem awaits.
fn admit_archive_record(profile_id: i64, post_id: &str) -> Result<ArchiveAdmission, ArchiveAdmissionError> {
	let mut connection = account::open_database().map_err(ArchiveAdmissionError::Database)?;
	admit_archive_record_in(&mut connection, profile_id, post_id)
}

fn admit_archive_record_in(connection: &mut rusqlite::Connection, profile_id: i64, post_id: &str) -> Result<ArchiveAdmission, ArchiveAdmissionError> {
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| ArchiveAdmissionError::Database(format!("Unable to lock the Vale archive library: {error}")))?;
	// Recheck under the write lock so simultaneous requests cannot bypass the
	// per-profile or global record ceilings.
	let current =
		entry_for_post(&transaction, profile_id, post_id).map_err(|error| ArchiveAdmissionError::Database(format!("Unable to inspect the saved Vale post: {error}")))?;
	if let Some(current) = current.as_ref().filter(|entry| entry.status != "failed") {
		let archive_id = current.id.clone();
		transaction
			.commit()
			.map_err(|error| ArchiveAdmissionError::Database(format!("Unable to finish the saved-post request: {error}")))?;
		return Ok(ArchiveAdmission::Existing { id: archive_id });
	}
	let quota = quota_snapshot_in(&transaction, profile_id).map_err(ArchiveAdmissionError::Database)?;
	let profile_remainder = quota
		.effective_limit_bytes
		.saturating_sub(quota.profile_used_bytes.saturating_add(quota.profile_reserved_bytes));
	let instance_remainder = quota
		.instance_limit_bytes
		.saturating_sub(quota.instance_used_bytes.saturating_add(quota.instance_reserved_bytes));
	let reservation_bytes = item_quota().min(profile_remainder).min(instance_remainder);
	if reservation_bytes < MIN_CAPTURE_RESERVATION_BYTES {
		let reason = if profile_remainder < MIN_CAPTURE_RESERVATION_BYTES {
			format!(
				"This profile has less than the required {} capture allowance available. Delete saved archives or raise its budget in Settings.",
				format_bytes(MIN_CAPTURE_RESERVATION_BYTES)
			)
		} else {
			format!(
				"The shared Vale archive pool has less than the required {} capture allowance available. Delete saved archives before retrying.",
				format_bytes(MIN_CAPTURE_RESERVATION_BYTES)
			)
		};
		return Err(ArchiveAdmissionError::Storage(reason));
	}
	let pruned = if current.is_some() {
		Vec::new()
	} else {
		match ensure_archive_record_capacity(&transaction, profile_id) {
			Ok(pruned) => pruned,
			Err(ArchiveCapacityError::Limit(message)) => return Err(ArchiveAdmissionError::Storage(message)),
			Err(ArchiveCapacityError::Database(message)) => return Err(ArchiveAdmissionError::Database(message)),
		}
	};
	let archive_id = current.map(|entry| entry.id).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
	let timestamp = account::now();
	if transaction
		.execute(
			"UPDATE post_archives SET status = 'queued', updated_at = ?1, error = '', issues_json = '[]', captured_at = NULL, comment_count = 0, asset_count = 0, generated_asset_count = 0, total_bytes = 0 WHERE profile_id = ?2 AND id = ?3 AND status = 'failed'",
			params![timestamp, profile_id, archive_id],
		)
		.map_err(|error| ArchiveAdmissionError::Database(format!("Unable to retry the saved-post capture: {error}")))?
		== 0
	{
		transaction
			.execute(
				"INSERT INTO post_archives (id, profile_id, post_id, status, created_at, updated_at) VALUES (?1, ?2, ?3, 'queued', ?4, ?4)",
				params![archive_id, profile_id, post_id, timestamp],
			)
			.map_err(|error| ArchiveAdmissionError::Database(format!("Unable to queue the saved-post capture: {error}")))?;
	}
	transaction
		.execute(
			"INSERT INTO archive_reservations (archive_id, profile_id, state, reserved_bytes, created_at, updated_at)
			 VALUES (?1, ?2, 'reserved', ?3, ?4, ?4)
			 ON CONFLICT(archive_id) DO UPDATE SET profile_id = excluded.profile_id, state = 'reserved', reserved_bytes = excluded.reserved_bytes, updated_at = excluded.updated_at",
			params![archive_id, profile_id, reservation_bytes as i64, timestamp],
		)
		.map_err(|error| ArchiveAdmissionError::Database(format!("Unable to reserve archive storage: {error}")))?;
	transaction
		.commit()
		.map_err(|error| ArchiveAdmissionError::Database(format!("Unable to finish the saved-post request: {error}")))?;
	Ok(ArchiveAdmission::Admitted {
		id: archive_id,
		reservation_bytes,
		pruned,
	})
}

async fn cleanup_failed_capture(profile_id: i64, archive_id: &str) -> Result<(), String> {
	let profile_directory = archive_root().join(profile_id.to_string());
	for directory in [partial_directory(profile_id, archive_id), archive_directory(profile_id, archive_id)] {
		if fs::try_exists(&directory).await.unwrap_or(false) {
			fs::remove_dir_all(&directory)
				.await
				.map_err(|error| format!("Unable to remove the incomplete archive before retrying: {error}"))?;
		}
	}
	if fs::try_exists(&profile_directory).await.unwrap_or(false) {
		sync_directory(&profile_directory).await?;
	}
	let mut connection = account::open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock the failed archive cleanup: {error}"))?;
	transaction
		.execute(
			"DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2",
			params![archive_id, profile_id],
		)
		.map_err(|error| format!("Unable to release the failed archive reservation: {error}"))?;
	transaction
		.execute(
			"UPDATE post_archives SET total_bytes = 0, updated_at = ?1 WHERE id = ?2 AND profile_id = ?3 AND status = 'failed'",
			params![account::now(), archive_id, profile_id],
		)
		.map_err(|error| format!("Unable to reconcile the failed archive: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish the failed archive cleanup: {error}"))
}

pub async fn save_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile_id) = account::context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required to save a permanent post archive."));
	};
	let post_id = request.param("id").unwrap_or_default();
	if !account::valid_post_id(&post_id) {
		return Ok(plain_response(StatusCode::BAD_REQUEST, "That post identifier is invalid."));
	}
	let existing = archive_for_post(&request, &post_id)?;
	if let Some(existing) = existing.as_ref().filter(|entry| entry.status != "failed") {
		return Ok(see_other(&format!("/saved/{}", existing.id)));
	}
	if let Some(existing) = existing.as_ref().filter(|entry| entry.status == "failed") {
		if let Err(message) = cleanup_failed_capture(profile_id, &existing.id).await {
			let connection = account::open_database()?;
			let _ = connection.execute(
				"UPDATE post_archives SET status = 'cleanup_failed', updated_at = ?1, error = ?2 WHERE id = ?3 AND profile_id = ?4",
				params![account::now(), message, existing.id, profile_id],
			);
			return Ok(plain_response(
				StatusCode::CONFLICT,
				"Vale could not safely clean the prior capture. Its storage remains accounted for; review the saved-post status.",
			));
		}
	}
	let Some(reservation) = reserve_archive_job() else {
		return Ok(plain_response(
			StatusCode::TOO_MANY_REQUESTS,
			"The local archive queue is full. Retry after a capture finishes.",
		));
	};
	let admission = match admit_archive_record(profile_id, &post_id) {
		Ok(admission) => admission,
		Err(ArchiveAdmissionError::Storage(message)) => return Ok(plain_response(StatusCode::INSUFFICIENT_STORAGE, &message)),
		Err(ArchiveAdmissionError::Database(message)) => return Err(message),
	};
	match admission {
		ArchiveAdmission::Existing { id } => Ok(see_other(&format!("/saved/{id}"))),
		ArchiveAdmission::Admitted { id, reservation_bytes, pruned } => {
			remove_pruned_archive_files(profile_id, &pruned).await;
			let job = ArchiveJob {
				id: id.clone(),
				profile_id,
				post_id,
				reservation_bytes,
			};
			if let Err(message) = spawn_job(job.clone(), reservation) {
				fail_job_after_cleanup(&job, &message).await;
				return Ok(plain_response(
					StatusCode::SERVICE_UNAVAILABLE,
					"The local archive worker is unavailable. Retry this capture.",
				));
			}
			Ok(see_other(&format!("/saved/{id}")))
		}
	}
}

pub async fn delete_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile_id) = account::context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required to remove that saved post."));
	};
	let archive_id = request.param("archive_id").unwrap_or_default();
	let Some(entry) = entry_for_profile(profile_id, &archive_id)? else {
		return Ok(plain_response(StatusCode::NOT_FOUND, "That saved post does not exist in this profile."));
	};
	if entry.is_pending() {
		return Ok(plain_response(StatusCode::CONFLICT, "Wait for the active capture to finish before removing it."));
	}
	account::open_database()?
		.execute(
			"UPDATE post_archives SET status = 'deleting', updated_at = ?1, error = '' WHERE profile_id = ?2 AND id = ?3",
			params![account::now(), profile_id, archive_id],
		)
		.map_err(|error| format!("Unable to mark the saved post for removal: {error}"))?;
	let directory = archive_directory(profile_id, &archive_id);
	let partial = partial_directory(profile_id, &archive_id);
	for path in [&directory, &partial] {
		if fs::try_exists(path).await.unwrap_or(false) {
			if let Err(error) = fs::remove_dir_all(path).await {
				let message = format!("Vale could not remove the archive files. Their bytes remain counted: {error}");
				let _ = account::open_database()?.execute(
					"UPDATE post_archives SET status = 'deleting', updated_at = ?1, error = ?2 WHERE profile_id = ?3 AND id = ?4",
					params![account::now(), bounded_error(&message), profile_id, archive_id],
				);
				return Ok(plain_response(StatusCode::CONFLICT, &message));
			}
		}
	}
	let profile_directory = archive_root().join(profile_id.to_string());
	if fs::try_exists(&profile_directory).await.unwrap_or(false) {
		if let Err(error) = sync_directory(&profile_directory).await {
			let message = format!("Vale removed the archive directory but could not make that removal durable. Accounting remains reserved: {error}");
			let _ = account::open_database()?.execute(
				"UPDATE post_archives SET status = 'deleting', updated_at = ?1, error = ?2 WHERE profile_id = ?3 AND id = ?4",
				params![account::now(), bounded_error(&message), profile_id, archive_id],
			);
			return Ok(plain_response(StatusCode::CONFLICT, &message));
		}
	}
	account::open_database()?
		.execute(
			"DELETE FROM post_archives WHERE profile_id = ?1 AND id = ?2 AND status = 'deleting'",
			params![profile_id, archive_id],
		)
		.map_err(|error| format!("Unable to release the removed archive accounting: {error}"))?;
	Ok(see_other("/saved"))
}

pub fn resume_pending() -> Result<(), String> {
	if account::mode() == account::ProfileMode::Browser {
		return Ok(());
	}
	let root = archive_root();
	std::fs::create_dir_all(&root).map_err(|error| format!("Unable to create the Vale archive directory: {error}"))?;
	archive_worker_sender()?;
	let mut connection = account::open_database()?;
	reconcile_database_archives(&mut connection, &root)?;
	reconcile_orphan_archives(&mut connection, &root)?;
	let pending = {
		let mut statement = connection
			.prepare("SELECT id, profile_id, post_id FROM post_archives WHERE status IN ('queued', 'capturing') ORDER BY created_at, id")
			.map_err(|error| format!("Unable to prepare pending saved-post captures: {error}"))?;
		let rows = statement
			.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)))
			.map_err(|error| format!("Unable to query pending saved-post captures: {error}"))?
			.collect::<Result<Vec<_>, _>>()
			.map_err(|error| format!("Unable to read pending saved-post captures: {error}"))?;
		rows
	};
	let mut jobs = Vec::new();
	for (position, (id, profile_id, post_id)) in pending.into_iter().enumerate() {
		if position >= MAX_PENDING_ARCHIVES {
			let transaction = connection
				.transaction_with_behavior(TransactionBehavior::Immediate)
				.map_err(|error| format!("Unable to lock the recovered archive queue: {error}"))?;
			transaction
				.execute("DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2", params![id, profile_id])
				.map_err(|error| format!("Unable to release an overflowed recovered reservation: {error}"))?;
			transaction
				.execute(
					"UPDATE post_archives SET status = 'failed', total_bytes = 0, updated_at = ?1, error = ?2 WHERE id = ?3 AND profile_id = ?4",
					params![
						account::now(),
						"Vale recovered the bounded archive queue first; retry this capture when capacity is available.",
						id,
						profile_id,
					],
				)
				.map_err(|error| format!("Unable to bound a recovered saved-post capture: {error}"))?;
			transaction
				.commit()
				.map_err(|error| format!("Unable to finish bounding the recovered archive queue: {error}"))?;
			continue;
		}
		if let Some(reservation_bytes) = ensure_recovered_reservation(&mut connection, profile_id, &id)? {
			jobs.push(ArchiveJob {
				id,
				profile_id,
				post_id,
				reservation_bytes,
			});
		}
	}
	drop(connection);
	for job in jobs {
		if let Some(reservation) = reserve_archive_job() {
			spawn_job(job, reservation)?;
		} else {
			return Err("The recovered archive queue exceeded its in-memory concurrency boundary.".to_string());
		}
	}
	Ok(())
}

fn sync_directory_blocking(path: &Path) -> Result<(), String> {
	std::fs::File::open(path)
		.and_then(|file| file.sync_all())
		.map_err(|error| format!("Unable to make an archive directory change durable: {error}"))
}

fn set_cleanup_failure(connection: &rusqlite::Connection, id: &str, profile_id: i64, message: &str, total_bytes: Option<u64>) -> Result<(), String> {
	connection
		.execute(
			"UPDATE post_archives SET status = 'cleanup_failed', total_bytes = COALESCE(?1, total_bytes), updated_at = ?2, error = ?3 WHERE id = ?4 AND profile_id = ?5",
			params![total_bytes.map(|value| value as i64), account::now(), bounded_error(message), id, profile_id],
		)
		.map_err(|error| format!("Unable to retain failed archive cleanup accounting: {error}"))?;
	Ok(())
}

fn set_deletion_failure(connection: &rusqlite::Connection, id: &str, profile_id: i64, message: &str) -> Result<(), String> {
	connection
		.execute(
			"UPDATE post_archives SET status = 'deleting', updated_at = ?1, error = ?2 WHERE id = ?3 AND profile_id = ?4",
			params![account::now(), bounded_error(message), id, profile_id],
		)
		.map_err(|error| format!("Unable to retain interrupted archive deletion accounting: {error}"))?;
	Ok(())
}

fn reconcile_published_archive(connection: &mut rusqlite::Connection, id: &str, profile_id: i64, current_status: &str, directory: &Path) -> Result<(), String> {
	let (total_bytes, _) = directory_usage(directory).map_err(|error| format!("Unable to measure published archive {id}: {error}"))?;
	let manifest = std::fs::read(directory.join("manifest.json"))
		.map_err(|error| format!("Unable to read published archive {id}: {error}"))
		.and_then(|bytes| serde_json::from_slice::<ArchiveManifest>(&bytes).map_err(|error| format!("Unable to validate published archive {id}: {error}")));
	let Ok(manifest) = manifest else {
		let transaction = connection
			.transaction_with_behavior(TransactionBehavior::Immediate)
			.map_err(|error| format!("Unable to lock published archive recovery: {error}"))?;
		transaction
			.execute("DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2", params![id, profile_id])
			.map_err(|error| format!("Unable to replace an invalid published archive reservation: {error}"))?;
		set_cleanup_failure(
			&transaction,
			id,
			profile_id,
			"The published archive is present but its manifest cannot be validated. Its exact files remain counted and can be deleted safely.",
			Some(total_bytes),
		)?;
		return transaction
			.commit()
			.map_err(|error| format!("Unable to finish invalid published archive recovery: {error}"));
	};
	if !directory.join("index.html").is_file() {
		set_cleanup_failure(
			connection,
			id,
			profile_id,
			"The published archive has no standalone reader. Its exact files remain counted and can be deleted safely.",
			Some(total_bytes),
		)?;
		return Ok(());
	}
	let recovered_status = if matches!(current_status, "ready" | "partial") {
		current_status
	} else if manifest.issues.is_empty() {
		"ready"
	} else {
		"partial"
	};
	let issues_json = serde_json::to_string(&manifest.issues).map_err(|error| format!("Unable to serialize a recovered archive report: {error}"))?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock published archive reconciliation: {error}"))?;
	transaction
		.execute(
			"UPDATE post_archives SET permalink = ?1, title = ?2, community = ?3, source_url = ?4, status = ?5, captured_at = ?6, updated_at = ?7, comment_count = ?8, asset_count = ?9, generated_asset_count = ?10, total_bytes = ?11, issues_json = ?12, error = '' WHERE id = ?13 AND profile_id = ?14",
			params![
				manifest.post.permalink,
				manifest.post.title,
				manifest.post.community,
				manifest.post.source_url,
				recovered_status,
				manifest.captured_at,
				account::now(),
				manifest.comment_count as i64,
				manifest.assets.len() as i64,
				manifest.generated_assets.len() as i64,
				total_bytes as i64,
				issues_json,
				id,
				profile_id,
			],
		)
		.map_err(|error| format!("Unable to reconcile published archive metadata: {error}"))?;
	transaction
		.execute("DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2", params![id, profile_id])
		.map_err(|error| format!("Unable to replace a published archive reservation: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish published archive reconciliation: {error}"))
}

fn remove_archive_directory_blocking(path: &Path, profile_directory: &Path) -> Result<(), String> {
	if path.exists() {
		std::fs::remove_dir_all(path).map_err(|error| format!("Unable to remove {}: {error}", path.display()))?;
	}
	if profile_directory.exists() {
		sync_directory_blocking(profile_directory)?;
	}
	Ok(())
}

fn reconcile_database_archives(connection: &mut rusqlite::Connection, root: &Path) -> Result<(), String> {
	let records = {
		let mut statement = connection
			.prepare("SELECT id, profile_id, status FROM post_archives ORDER BY profile_id, id")
			.map_err(|error| format!("Unable to prepare archive reconciliation: {error}"))?;
		let rows = statement
			.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)))
			.map_err(|error| format!("Unable to query archive reconciliation records: {error}"))?
			.collect::<Result<Vec<_>, _>>()
			.map_err(|error| format!("Unable to read archive reconciliation records: {error}"))?;
		rows
	};
	for (id, profile_id, status) in records {
		let profile_directory = root.join(profile_id.to_string());
		let final_directory = profile_directory.join(&id);
		let partial = profile_directory.join(format!(".{id}.partial"));
		if status == "deleting" {
			if let Err(message) =
				remove_archive_directory_blocking(&final_directory, &profile_directory).and_then(|_| remove_archive_directory_blocking(&partial, &profile_directory))
			{
				set_deletion_failure(connection, &id, profile_id, &message)?;
				continue;
			}
			connection
				.execute("DELETE FROM post_archives WHERE id = ?1 AND profile_id = ?2", params![id, profile_id])
				.map_err(|error| format!("Unable to finish an interrupted archive deletion: {error}"))?;
			continue;
		}
		if final_directory.exists() {
			reconcile_published_archive(connection, &id, profile_id, &status, &final_directory)?;
			continue;
		}
		if partial.exists() {
			if let Err(message) = remove_archive_directory_blocking(&partial, &profile_directory) {
				let measured = directory_usage(&partial).ok().map(|(bytes, _)| bytes);
				// The retained ledger, not total_bytes, owns incomplete-file
				// accounting so the same bytes are never counted twice.
				set_cleanup_failure(connection, &id, profile_id, &message, Some(0))?;
				connection
					.execute(
						"INSERT INTO archive_reservations (archive_id, profile_id, state, reserved_bytes, created_at, updated_at)
						 VALUES (?1, ?2, 'cleanup_pending', ?3, ?4, ?4)
						 ON CONFLICT(archive_id) DO UPDATE SET state = 'cleanup_pending', reserved_bytes = MAX(reserved_bytes, excluded.reserved_bytes), updated_at = excluded.updated_at",
						params![id, profile_id, measured.unwrap_or(MIN_CAPTURE_RESERVATION_BYTES) as i64, account::now()],
					)
					.map_err(|error| format!("Unable to retain an interrupted archive reservation: {error}"))?;
				continue;
			}
		}
		match status.as_str() {
			"ready" | "partial" => set_cleanup_failure(
				connection,
				&id,
				profile_id,
				"The archive record exists but its durable directory is missing. Accounting remains retained until the record is removed.",
				None,
			)?,
			"failed" | "cleanup_failed" => {
				let transaction = connection
					.transaction_with_behavior(TransactionBehavior::Immediate)
					.map_err(|error| format!("Unable to lock completed archive cleanup: {error}"))?;
				transaction
					.execute("DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2", params![id, profile_id])
					.map_err(|error| format!("Unable to release recovered cleanup accounting: {error}"))?;
				transaction
					.execute(
						"UPDATE post_archives SET status = 'failed', total_bytes = 0, updated_at = ?1 WHERE id = ?2 AND profile_id = ?3",
						params![account::now(), id, profile_id],
					)
					.map_err(|error| format!("Unable to finish recovered archive cleanup: {error}"))?;
				transaction.commit().map_err(|error| format!("Unable to commit recovered archive cleanup: {error}"))?;
			}
			_ => {}
		}
	}
	Ok(())
}

fn insert_orphan_record(connection: &rusqlite::Connection, profile_id: i64, archive_id: &str, bytes: u64, partial: bool) -> Result<(), String> {
	let timestamp = account::now();
	let message = if partial {
		"Vale found an untracked partial archive and could not remove it. Its exact bytes are counted until deletion succeeds."
	} else {
		"Vale found an untracked durable archive. Its exact bytes are counted; inspect or delete it before continuing normal archive work."
	};
	connection
		.execute(
			"INSERT INTO post_archives (id, profile_id, post_id, status, created_at, updated_at, total_bytes, error)
			 VALUES (?1, ?2, ?3, 'cleanup_failed', ?4, ?4, ?5, ?6)",
			params![archive_id, profile_id, format!("orphan-{archive_id}"), timestamp, bytes as i64, message],
		)
		.map_err(|error| format!("Unable to account for orphan archive {archive_id}: {error}"))?;
	Ok(())
}

fn reconcile_orphan_archives(connection: &mut rusqlite::Connection, root: &Path) -> Result<(), String> {
	for profile_entry in std::fs::read_dir(root).map_err(|error| format!("Unable to inspect the Vale archive root: {error}"))? {
		let profile_entry = profile_entry.map_err(|error| format!("Unable to inspect a Vale archive profile directory: {error}"))?;
		if !profile_entry
			.file_type()
			.map_err(|error| format!("Unable to inspect an archive-root entry: {error}"))?
			.is_dir()
		{
			return Err(format!("Unexpected file in the Vale archive root: {}", profile_entry.path().display()));
		}
		let profile_name = profile_entry.file_name().to_string_lossy().into_owned();
		let profile_id = profile_name
			.parse::<i64>()
			.map_err(|_| format!("Unexpected directory in the Vale archive root: {}", profile_entry.path().display()))?;
		let profile_exists = connection
			.query_row("SELECT EXISTS(SELECT 1 FROM profiles WHERE id = ?1)", params![profile_id], |row| row.get::<_, i64>(0))
			.map_err(|error| format!("Unable to validate an archive profile directory: {error}"))?
			!= 0;
		if !profile_exists {
			return Err(format!(
				"Archive files exist for missing profile {profile_id}; startup stopped to preserve their accounting."
			));
		}
		for entry in std::fs::read_dir(profile_entry.path()).map_err(|error| format!("Unable to inspect archives for profile {profile_id}: {error}"))? {
			let entry = entry.map_err(|error| format!("Unable to inspect an archive directory: {error}"))?;
			if !entry.file_type().map_err(|error| format!("Unable to inspect an archive entry: {error}"))?.is_dir() {
				return Err(format!("Unexpected file outside archive accounting: {}", entry.path().display()));
			}
			let name = entry.file_name().to_string_lossy().into_owned();
			let (archive_id, is_partial) = if let Some(id) = name.strip_prefix('.').and_then(|value| value.strip_suffix(".partial")) {
				(id.to_string(), true)
			} else {
				(name, false)
			};
			if uuid::Uuid::parse_str(&archive_id).is_err() {
				return Err(format!("Unexpected directory outside archive accounting: {}", entry.path().display()));
			}
			let exists = connection
				.query_row(
					"SELECT EXISTS(SELECT 1 FROM post_archives WHERE id = ?1 AND profile_id = ?2)",
					params![archive_id, profile_id],
					|row| row.get::<_, i64>(0),
				)
				.map_err(|error| format!("Unable to inspect orphan archive accounting: {error}"))?
				!= 0;
			if exists {
				continue;
			}
			if is_partial && remove_archive_directory_blocking(&entry.path(), &profile_entry.path()).is_ok() {
				continue;
			}
			let (bytes, _) = directory_usage(&entry.path()).map_err(|error| format!("Unable to measure orphan archive {archive_id}: {error}"))?;
			insert_orphan_record(connection, profile_id, &archive_id, bytes, is_partial)?;
		}
	}
	Ok(())
}

fn ensure_recovered_reservation(connection: &mut rusqlite::Connection, profile_id: i64, archive_id: &str) -> Result<Option<u64>, String> {
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock a recovered archive reservation: {error}"))?;
	if let Some(existing) = transaction
		.query_row(
			"SELECT reserved_bytes FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2",
			params![archive_id, profile_id],
			|row| row.get::<_, i64>(0),
		)
		.optional()
		.map_err(|error| format!("Unable to inspect a recovered archive reservation: {error}"))?
	{
		let existing = existing.max(0) as u64;
		if existing >= MIN_CAPTURE_RESERVATION_BYTES {
			transaction
				.commit()
				.map_err(|error| format!("Unable to finish recovered reservation inspection: {error}"))?;
			return Ok(Some(existing));
		}
	}
	let quota = quota_snapshot_in(&transaction, profile_id)?;
	let profile_remainder = quota
		.effective_limit_bytes
		.saturating_sub(quota.profile_used_bytes.saturating_add(quota.profile_reserved_bytes));
	let instance_remainder = quota
		.instance_limit_bytes
		.saturating_sub(quota.instance_used_bytes.saturating_add(quota.instance_reserved_bytes));
	let reservation_bytes = item_quota().min(profile_remainder).min(instance_remainder);
	if reservation_bytes < MIN_CAPTURE_RESERVATION_BYTES {
		transaction
			.execute(
				"DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2",
				params![archive_id, profile_id],
			)
			.map_err(|error| format!("Unable to release an unusable recovered reservation: {error}"))?;
		transaction
			.execute(
				"UPDATE post_archives SET status = 'failed', total_bytes = 0, updated_at = ?1, error = ?2 WHERE id = ?3 AND profile_id = ?4",
				params![
					account::now(),
					"The recovered capture no longer fits the current archive limits; raise the budget or delete archives before retrying.",
					archive_id,
					profile_id
				],
			)
			.map_err(|error| format!("Unable to pause a recovered archive over budget: {error}"))?;
		transaction.commit().map_err(|error| format!("Unable to finish pausing a recovered archive: {error}"))?;
		return Ok(None);
	}
	let timestamp = account::now();
	transaction
		.execute(
			"INSERT INTO archive_reservations (archive_id, profile_id, state, reserved_bytes, created_at, updated_at)
			 VALUES (?1, ?2, 'reserved', ?3, ?4, ?4)
			 ON CONFLICT(archive_id) DO UPDATE SET state = 'reserved', reserved_bytes = excluded.reserved_bytes, updated_at = excluded.updated_at",
			params![archive_id, profile_id, reservation_bytes as i64, timestamp],
		)
		.map_err(|error| format!("Unable to create a recovered archive reservation: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish recovered archive reservation: {error}"))?;
	Ok(Some(reservation_bytes))
}

fn try_reserve_pending(counter: &AtomicUsize) -> bool {
	counter
		.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| (pending < MAX_PENDING_ARCHIVES).then_some(pending + 1))
		.is_ok()
}

fn release_pending(counter: &AtomicUsize) {
	let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| pending.checked_sub(1));
}

struct ArchiveReservation;

impl Drop for ArchiveReservation {
	fn drop(&mut self) {
		release_pending(&PENDING_ARCHIVES);
	}
}

fn reserve_archive_job() -> Option<ArchiveReservation> {
	try_reserve_pending(&PENDING_ARCHIVES).then_some(ArchiveReservation)
}

struct QueuedArchiveJob {
	job: ArchiveJob,
	reservation: ArchiveReservation,
}

#[cfg(target_os = "linux")]
fn lower_archive_worker_priority() -> Result<(), String> {
	const PRIO_PROCESS: i32 = 0;

	unsafe extern "C" {
		fn getpriority(which: i32, who: u32) -> i32;
		fn setpriority(which: i32, who: u32, priority: i32) -> i32;
	}

	// Linux schedules each native thread independently for PRIO_PROCESS. The
	// dedicated runtime's blocking thread inherits this value when it starts.
	let current = unsafe { getpriority(PRIO_PROCESS, 0) };
	if current >= ARCHIVE_WORKER_NICE {
		return Ok(());
	}
	if unsafe { setpriority(PRIO_PROCESS, 0, ARCHIVE_WORKER_NICE) } == 0 {
		Ok(())
	} else {
		Err(format!("Unable to lower the archive worker's CPU priority: {}", io::Error::last_os_error()))
	}
}

#[cfg(not(target_os = "linux"))]
fn lower_archive_worker_priority() -> Result<(), String> {
	Ok(())
}

fn archive_worker_loop(receiver: Receiver<QueuedArchiveJob>, ready: SyncSender<Result<(), String>>) {
	if let Err(message) = lower_archive_worker_priority() {
		let _ = ready.send(Err(message));
		return;
	}
	let runtime = match tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.max_blocking_threads(1)
		.thread_name("vale-archive-blocking")
		.build()
	{
		Ok(runtime) => runtime,
		Err(error) => {
			let _ = ready.send(Err(format!("Unable to start the isolated archive runtime: {error}")));
			return;
		}
	};
	if ready.send(Ok(())).is_err() {
		return;
	}
	while let Ok(queued) = receiver.recv() {
		let _reservation = queued.reservation;
		runtime.block_on(async {
			if let Err(message) = run_job(&queued.job).await {
				fail_job_after_cleanup(&queued.job, &message).await;
			}
		});
	}
}

fn start_archive_worker() -> Result<SyncSender<QueuedArchiveJob>, String> {
	let (sender, receiver) = sync_channel(MAX_PENDING_ARCHIVES);
	let (ready_sender, ready_receiver) = sync_channel(1);
	std::thread::Builder::new()
		.name("vale-archive".to_string())
		.spawn(move || archive_worker_loop(receiver, ready_sender))
		.map_err(|error| format!("Unable to start the isolated archive worker: {error}"))?;
	ready_receiver.recv().map_err(|_| "The isolated archive worker stopped during startup.".to_string())??;
	Ok(sender)
}

fn archive_worker_sender() -> Result<&'static SyncSender<QueuedArchiveJob>, String> {
	ARCHIVE_WORKER.as_ref().map_err(Clone::clone)
}

fn spawn_job(job: ArchiveJob, reservation: ArchiveReservation) -> Result<(), String> {
	let queued = QueuedArchiveJob { job, reservation };
	match archive_worker_sender()?.try_send(queued) {
		Ok(()) => Ok(()),
		Err(TrySendError::Full(_)) => Err("The isolated archive queue reached its concurrency boundary.".to_string()),
		Err(TrySendError::Disconnected(_)) => Err("The isolated archive worker stopped before this capture began.".to_string()),
	}
}

fn bounded_error(message: &str) -> String {
	message.chars().take(500).collect()
}

fn directory_usage(path: &Path) -> io::Result<(u64, u64)> {
	fn visit(path: &Path, bytes: &mut u64, files: &mut u64) -> io::Result<()> {
		for entry in std::fs::read_dir(path)? {
			let entry = entry?;
			let file_type = entry.file_type()?;
			if file_type.is_symlink() {
				return Err(io::Error::new(io::ErrorKind::InvalidData, "archive directories cannot contain symbolic links"));
			}
			if file_type.is_dir() {
				visit(&entry.path(), bytes, files)?;
			} else if file_type.is_file() {
				*bytes = bytes.saturating_add(entry.metadata()?.len());
				*files = files.saturating_add(1);
			}
		}
		Ok(())
	}
	let mut bytes = 0;
	let mut files = 0;
	visit(path, &mut bytes, &mut files)?;
	Ok((bytes, files))
}

async fn finalize_published_job(job: &ArchiveJob, failure: &str) -> Result<(), String> {
	let directory = archive_directory(job.profile_id, &job.id);
	let manifest_bytes = fs::read(directory.join("manifest.json"))
		.await
		.map_err(|error| format!("Unable to read the published archive manifest during recovery: {error}"))?;
	let manifest: ArchiveManifest =
		serde_json::from_slice(&manifest_bytes).map_err(|error| format!("Unable to validate the published archive manifest during recovery: {error}"))?;
	let usage_path = directory.clone();
	let (total_bytes, _) = tokio::task::spawn_blocking(move || directory_usage(&usage_path))
		.await
		.map_err(|_| "The published archive accounting task stopped unexpectedly.".to_string())?
		.map_err(|error| format!("Unable to measure the published archive during recovery: {error}"))?;
	let status = if manifest.issues.is_empty() { "ready" } else { "partial" };
	let issues_json = serde_json::to_string(&manifest.issues).map_err(|error| format!("Unable to serialize the recovered capture report: {error}"))?;
	let mut connection = account::open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock the published archive recovery: {error}"))?;
	transaction
		.execute(
			"UPDATE post_archives SET permalink = ?1, title = ?2, community = ?3, source_url = ?4, status = ?5, updated_at = ?6, captured_at = ?7, comment_count = ?8, asset_count = ?9, generated_asset_count = ?10, total_bytes = ?11, issues_json = ?12, error = ?13 WHERE id = ?14 AND profile_id = ?15",
			params![
				manifest.post.permalink,
				manifest.post.title,
				manifest.post.community,
				manifest.post.source_url,
				status,
				account::now(),
				manifest.captured_at,
				manifest.comment_count as i64,
				manifest.assets.len() as i64,
				manifest.generated_assets.len() as i64,
				total_bytes as i64,
				issues_json,
				bounded_error(failure),
				job.id,
				job.profile_id,
			],
		)
		.map_err(|error| format!("Unable to reconcile the published archive record: {error}"))?;
	transaction
		.execute(
			"DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2",
			params![job.id, job.profile_id],
		)
		.map_err(|error| format!("Unable to replace the published archive reservation: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish the published archive recovery: {error}"))
}

async fn retain_cleanup_reservation(job: &ArchiveJob, message: &str) {
	if let Ok(mut connection) = account::open_database() {
		if let Ok(transaction) = connection.transaction_with_behavior(TransactionBehavior::Immediate) {
			let timestamp = account::now();
			let _ = transaction.execute(
				"UPDATE archive_reservations SET state = 'cleanup_pending', updated_at = ?1 WHERE archive_id = ?2 AND profile_id = ?3",
				params![timestamp, job.id, job.profile_id],
			);
			let _ = transaction.execute(
				"UPDATE post_archives SET status = 'cleanup_failed', updated_at = ?1, error = ?2 WHERE id = ?3 AND profile_id = ?4",
				params![timestamp, bounded_error(message), job.id, job.profile_id],
			);
			let _ = transaction.commit();
		}
	}
}

async fn fail_job_after_cleanup(job: &ArchiveJob, message: &str) {
	let final_directory = archive_directory(job.profile_id, &job.id);
	if fs::try_exists(&final_directory).await.unwrap_or(false) {
		if let Err(recovery_error) = finalize_published_job(job, message).await {
			retain_cleanup_reservation(job, &format!("{message} Published-file reconciliation also failed: {recovery_error}")).await;
		}
		return;
	}
	let partial = partial_directory(job.profile_id, &job.id);
	let cleanup = async {
		if fs::try_exists(&partial).await.unwrap_or(false) {
			fs::remove_dir_all(&partial)
				.await
				.map_err(|error| format!("Unable to remove the incomplete capture: {error}"))?;
		}
		let profile_directory = archive_root().join(job.profile_id.to_string());
		if fs::try_exists(&profile_directory).await.unwrap_or(false) {
			sync_directory(&profile_directory).await?;
		}
		Ok::<(), String>(())
	}
	.await;
	if let Err(cleanup_error) = cleanup {
		retain_cleanup_reservation(job, &format!("{message} {cleanup_error}")).await;
		return;
	}
	if let Ok(mut connection) = account::open_database() {
		if let Ok(transaction) = connection.transaction_with_behavior(TransactionBehavior::Immediate) {
			let timestamp = account::now();
			let _ = transaction.execute(
				"DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2",
				params![job.id, job.profile_id],
			);
			let _ = transaction.execute(
				"UPDATE post_archives SET status = 'failed', updated_at = ?1, total_bytes = 0, error = ?2 WHERE id = ?3 AND profile_id = ?4",
				params![timestamp, bounded_error(message), job.id, job.profile_id],
			);
			let _ = transaction.commit();
		}
	}
}

fn issue(area: &str, message: impl Into<String>) -> ArchiveIssue {
	ArchiveIssue {
		area: area.to_string(),
		message: message.into(),
	}
}

fn collect_comment_thing(
	thing: &Value,
	comments: &mut HashMap<String, Value>,
	order: &mut Vec<String>,
	more: &mut VecDeque<String>,
	seen_more: &mut HashSet<String>,
	issues: &mut Vec<ArchiveIssue>,
) {
	match thing["kind"].as_str().unwrap_or_default() {
		"t1" => {
			let id = val(thing, "id");
			if !id.is_empty() && comments.len() < MAX_ARCHIVE_COMMENTS && !comments.contains_key(&id) {
				order.push(id.clone());
				comments.insert(id, thing.clone());
			}
			if let Some(children) = thing["data"]["replies"]["data"]["children"].as_array() {
				for child in children {
					collect_comment_thing(child, comments, order, more, seen_more, issues);
				}
			}
		}
		"more" => {
			let children = thing["data"]["children"].as_array();
			let mut found = false;
			if let Some(children) = children {
				for child in children.iter().filter_map(Value::as_str) {
					if seen_more.insert(child.to_string()) {
						more.push_back(child.to_string());
					}
					found = true;
				}
			}
			if !found && thing["data"]["count"].as_i64().unwrap_or_default() > 0 {
				issues.push(issue(
					"comments",
					"Reddit reported additional comments without exposing identifiers that Vale could retrieve.",
				));
			}
		}
		_ => {}
	}
}

fn comment_score(thing: &Value) -> i64 {
	thing["data"]["score"].as_i64().unwrap_or_default()
}

fn archived_comment_from_thing(
	id: &str,
	comments: &HashMap<String, Value>,
	children: &HashMap<String, Vec<String>>,
	visiting: &mut HashSet<String>,
	issues: &mut Vec<ArchiveIssue>,
) -> Option<ArchivedComment> {
	if !visiting.insert(id.to_string()) {
		return None;
	}
	let thing = comments.get(id)?;
	let data = &thing["data"];
	let raw_body = val(thing, "body");
	let mut body_html = if data["body_html"].is_string() {
		rewrite_emotes(&data["media_metadata"], val(thing, "body_html"))
	} else {
		format!("<div class=\"md\"><p>{}</p></div>", htmlescape::encode_minimal(&raw_body))
	};
	body_html = rewrite_urls(&body_html);
	body_html = match normalize_archive_comment_headings(&body_html) {
		Ok(normalized) => normalized,
		Err(_) => {
			issues.push(issue(
				"comments",
				format!("Comment {id} contained ambiguous HTML; Vale stored its escaped raw Markdown instead."),
			));
			format!("<div class=\"md\"><p>{}</p></div>", htmlescape::encode_minimal(&raw_body))
		}
	};
	let timestamp = data["created_utc"].as_f64().unwrap_or_default().round() as i64;
	let replies = children
		.get(id)
		.into_iter()
		.flatten()
		.filter_map(|child| archived_comment_from_thing(child, comments, children, visiting, issues))
		.collect();
	visiting.remove(id);
	Some(ArchivedComment {
		id: id.to_string(),
		parent_id: val(thing, "parent_id"),
		author: val(thing, "author"),
		body_html,
		created: format_timestamp(timestamp),
		score: comment_score(thing),
		score_hidden: data["score_hidden"].as_bool().unwrap_or_default(),
		replies,
	})
}

async fn capture_comments(initial: &Value, post_id: &str, expected: usize) -> CommentCapture {
	let mut comments = HashMap::new();
	let mut order = Vec::new();
	let mut more = VecDeque::new();
	let mut seen_more = HashSet::new();
	let mut issues = Vec::new();
	if let Some(children) = initial[1]["data"]["children"].as_array() {
		for thing in children {
			collect_comment_thing(thing, &mut comments, &mut order, &mut more, &mut seen_more, &mut issues);
		}
	} else {
		issues.push(issue("comments", "Reddit did not return a comment listing for this post."));
	}

	let mut fetched_things = Vec::new();
	let mut requests = 0usize;
	while !more.is_empty() && comments.len() < MAX_ARCHIVE_COMMENTS && requests < MAX_MORE_REQUESTS {
		let mut batch = Vec::new();
		while batch.len() < MORE_CHILDREN_BATCH {
			let Some(id) = more.pop_front() else {
				break;
			};
			if !comments.contains_key(&id) {
				batch.push(id);
			}
		}
		if batch.is_empty() {
			continue;
		}
		requests += 1;
		let query = url::form_urlencoded::Serializer::new(String::new())
			.append_pair("api_type", "json")
			.append_pair("link_id", &format!("t3_{post_id}"))
			.append_pair("children", &batch.join(","))
			.append_pair("sort", "top")
			.append_pair("raw_json", "1")
			.finish();
		let response = match json(format!("/api/morechildren.json?{query}"), true).await {
			Ok(response) => response,
			Err(_) => {
				issues.push(issue(
					"comments",
					format!(
						"Reddit stopped the continuation crawl after {} archived comments; the remaining branches are listed as omissions.",
						comments.len()
					),
				));
				break;
			}
		};
		let things = response["json"]["data"]["things"]
			.as_array()
			.or_else(|| response["data"]["things"].as_array())
			.cloned()
			.unwrap_or_default();
		if things.is_empty() {
			issues.push(issue(
				"comments",
				format!(
					"Reddit returned no data for {} queued comment identifiers; those branches could not be preserved.",
					batch.len()
				),
			));
			continue;
		}
		for thing in things {
			fetched_things.push(thing.clone());
			collect_comment_thing(&thing, &mut comments, &mut order, &mut more, &mut seen_more, &mut issues);
		}
	}

	if !more.is_empty() || comments.len() >= MAX_ARCHIVE_COMMENTS || requests >= MAX_MORE_REQUESTS {
		issues.push(issue(
			"comments",
			format!(
				"The safety boundary stopped comment expansion at {} comments and {} continuation requests.",
				comments.len(),
				requests
			),
		));
	}
	if expected > comments.len() {
		issues.push(issue(
			"comments",
			format!(
				"Reddit reported {expected} comments when capture began; {} distinct comments were retrievable.",
				comments.len()
			),
		));
	}

	let order_index = order.iter().enumerate().map(|(index, id)| (id.clone(), index)).collect::<HashMap<_, _>>();
	let mut children: HashMap<String, Vec<String>> = HashMap::new();
	let mut roots = Vec::new();
	for id in comments.keys() {
		let parent = val(comments.get(id).unwrap_or(&Value::Null), "parent_id");
		let parent_comment = parent.strip_prefix("t1_");
		if parent == format!("t3_{post_id}") || parent_comment.is_none() || parent_comment.is_some_and(|parent| !comments.contains_key(parent)) {
			roots.push(id.clone());
		} else {
			children.entry(parent_comment.unwrap_or_default().to_string()).or_default().push(id.clone());
		}
	}
	let sorter = |left: &String, right: &String| {
		let left_value = comments.get(left).unwrap_or(&Value::Null);
		let right_value = comments.get(right).unwrap_or(&Value::Null);
		comment_score(right_value)
			.cmp(&comment_score(left_value))
			.then_with(|| order_index.get(left).cmp(&order_index.get(right)))
	};
	roots.sort_by(sorter);
	for values in children.values_mut() {
		values.sort_by(sorter);
	}
	let mut visiting = HashSet::new();
	let archived = roots
		.iter()
		.filter_map(|id| archived_comment_from_thing(id, &comments, &children, &mut visiting, &mut issues))
		.collect();
	CommentCapture {
		comments: archived,
		things: fetched_things,
		count: comments.len(),
		issues,
	}
}

fn content_type_extension(content_type: &str, source: &str) -> &'static str {
	match content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase().as_str() {
		"image/jpeg" => "jpg",
		"image/png" => "png",
		"image/gif" => "gif",
		"image/webp" => "webp",
		"image/avif" => "avif",
		"image/svg+xml" => "svg",
		"video/mp4" => "mp4",
		"video/webm" => "webm",
		"audio/mpeg" => "mp3",
		"audio/mp4" => "m4a",
		"application/pdf" => "pdf",
		"text/css" => "css",
		"text/html" | "application/xhtml+xml" => "html",
		"application/json" => "json",
		_ => {
			let extension = source
				.split('?')
				.next()
				.and_then(|path| path.rsplit('/').next())
				.and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
				.unwrap_or_default()
				.to_ascii_lowercase();
			match extension.as_str() {
				"jpg" | "jpeg" => "jpg",
				"png" => "png",
				"gif" => "gif",
				"webp" => "webp",
				"avif" => "avif",
				"svg" => "svg",
				"mp4" | "m4v" => "mp4",
				"webm" => "webm",
				"mp3" => "mp3",
				"m4a" => "m4a",
				"pdf" => "pdf",
				"css" => "css",
				"html" | "htm" => "html",
				"json" => "json",
				_ => "bin",
			}
		}
	}
}

fn is_local_media_path(source: &str) -> bool {
	["/img/", "/preview/", "/vid/", "/hls/", "/thumb/", "/emoji/", "/emote/", "/style/", "/static/"]
		.iter()
		.any(|prefix| source.starts_with(prefix))
}

async fn sha256_file(path: &Path) -> Result<String, String> {
	let path = path.to_path_buf();
	tokio::task::spawn_blocking(move || {
		use std::io::Read as _;

		let mut file = std::fs::File::open(path).map_err(|error| format!("Unable to checksum an archived file: {error}"))?;
		let mut digest = Sha256::new();
		let mut buffer = vec![0_u8; 128 * 1024];
		loop {
			let read = file.read(&mut buffer).map_err(|error| format!("Unable to checksum an archived file: {error}"))?;
			if read == 0 {
				break;
			}
			digest.update(&buffer[..read]);
		}
		Ok(format!("{:x}", digest.finalize()))
	})
	.await
	.map_err(|_| "The archive checksum task stopped unexpectedly.".to_string())?
}

impl CaptureContext {
	fn remaining(&self) -> u64 {
		self.item_limit.saturating_sub(self.total_bytes)
	}

	fn remaining_for_assets(&self) -> u64 {
		self.remaining().saturating_sub(FINALIZATION_RESERVE_BYTES)
	}

	fn push_issue(&mut self, area: &str, message: impl Into<String>) {
		self.issues.push(issue(area, message));
	}

	async fn store_response(&mut self, source_key: &str, original_url: &str, response: wreq::Response, maximum: u64) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(source_key).or_else(|| self.asset_paths.get(original_url)).cloned() {
			self.asset_paths.insert(source_key.to_string(), path.clone());
			return Ok(path);
		}
		if !response.status().is_success() {
			return Err(format!("The source returned HTTP {}.", response.status().as_u16()));
		}
		let limit = maximum.min(self.remaining_for_assets());
		if limit == 0 {
			return Err("The archive reached its configured size limit.".to_string());
		}
		if response.content_length().is_some_and(|length| length > limit) {
			return Err(format!("The asset is larger than the remaining {} archive allowance.", format_bytes(limit)));
		}
		let content_type = response
			.headers()
			.get(header::CONTENT_TYPE.as_str())
			.and_then(|value| value.to_str().ok())
			.unwrap_or("application/octet-stream")
			.to_string();
		let extension = content_type_extension(&content_type, original_url);
		let url_digest = format!("{:x}", Sha256::digest(original_url.as_bytes()));
		let filename = format!("{url_digest}.{extension}");
		let relative = format!("files/assets/{filename}");
		let directory = self.directory.join("files/assets");
		fs::create_dir_all(&directory)
			.await
			.map_err(|error| format!("Unable to prepare the archive asset directory: {error}"))?;
		let final_path = directory.join(&filename);
		let temporary = directory.join(format!(".{filename}.partial"));
		let mut file = fs::File::create(&temporary).await.map_err(|error| format!("Unable to create an archived asset: {error}"))?;
		let mut stream = response.bytes_stream();
		let mut bytes = 0_u64;
		let mut digest = Sha256::new();
		loop {
			let next = timeout(TRANSFER_IDLE_TIMEOUT, stream.next())
				.await
				.map_err(|_| "The asset transfer stopped making progress.".to_string())?;
			let Some(chunk) = next else {
				break;
			};
			let chunk = chunk.map_err(|_| "The asset transfer ended before it completed.".to_string())?;
			bytes = bytes.saturating_add(chunk.len() as u64);
			if bytes > limit {
				let _ = fs::remove_file(&temporary).await;
				return Err(format!("The asset exceeded the remaining {} archive allowance.", format_bytes(limit)));
			}
			digest.update(&chunk);
			file.write_all(&chunk).await.map_err(|error| format!("Unable to write an archived asset: {error}"))?;
		}
		if bytes == 0 {
			let _ = fs::remove_file(&temporary).await;
			return Err("The source returned an empty asset.".to_string());
		}
		file.sync_all().await.map_err(|error| format!("Unable to make an archived asset durable: {error}"))?;
		drop(file);
		fs::rename(&temporary, &final_path)
			.await
			.map_err(|error| format!("Unable to finish an archived asset: {error}"))?;
		self.total_bytes = self.total_bytes.saturating_add(bytes);
		self.assets.push(ArchiveAsset {
			path: relative.clone(),
			original_url: original_url.to_string(),
			content_type,
			bytes,
			sha256: format!("{:x}", digest.finalize()),
		});
		self.asset_paths.insert(source_key.to_string(), relative.clone());
		self.asset_paths.insert(original_url.to_string(), relative.clone());
		Ok(relative)
	}

	async fn capture_reddit_asset(&mut self, source: &str, maximum: u64) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(source) {
			return Ok(path.clone());
		}
		let normalized = source.replace("&amp;", "&");
		if let Some(path) = self.asset_paths.get(&normalized).cloned() {
			self.asset_paths.insert(source.to_string(), path.clone());
			return Ok(path);
		}
		let upstream = media::upstream_media_url(&normalized)?;
		let uri = wreq::Uri::try_from(upstream.clone()).map_err(|_| "The Reddit media address is invalid.".to_string())?;
		let oauth = OAUTH_CLIENT.load_full();
		let request = CLIENT.get(uri).header("User-Agent", oauth.user_agent()).header("Accept", "*/*").send();
		let response = timeout(TRANSFER_IDLE_TIMEOUT, request)
			.await
			.map_err(|_| "Reddit did not begin the archived media transfer in time.".to_string())?
			.map_err(|_| "Reddit did not return the archived media item.".to_string())?;
		let path = self.store_response(&normalized, &upstream, response, maximum).await?;
		self.asset_paths.insert(source.to_string(), path.clone());
		Ok(path)
	}

	async fn capture_reddit_video(&mut self, hls_source: &str) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(hls_source) {
			return Ok(path.clone());
		}
		let upstream = media::upstream_media_url(hls_source)?;
		let digest = format!("{:x}", Sha256::digest(upstream.as_bytes()));
		let filename = format!("{digest}.mp4");
		let relative = format!("files/assets/{filename}");
		let directory = self.directory.join("files/assets");
		fs::create_dir_all(&directory)
			.await
			.map_err(|error| format!("Unable to prepare the archive asset directory: {error}"))?;
		let final_path = directory.join(&filename);
		let temporary = directory.join(format!(".{filename}.partial"));
		let maximum = MAX_REDDIT_ASSET_BYTES.min(self.remaining_for_assets());
		if maximum < 1024 {
			return Err("The archive reached its configured size limit before the video capture began.".to_string());
		}
		media::remux_video_to(hls_source, &temporary, maximum).await?;
		let bytes = fs::metadata(&temporary).await.map_err(|_| "Vale could not inspect the archived video.".to_string())?.len();
		if bytes == 0 || bytes > maximum {
			let _ = fs::remove_file(&temporary).await;
			return Err("The archived video was empty or exceeded its size boundary.".to_string());
		}
		let sha256 = sha256_file(&temporary).await?;
		fs::rename(&temporary, &final_path)
			.await
			.map_err(|error| format!("Unable to finish the archived video: {error}"))?;
		self.total_bytes = self.total_bytes.saturating_add(bytes);
		self.assets.push(ArchiveAsset {
			path: relative.clone(),
			original_url: upstream,
			content_type: "video/mp4".to_string(),
			bytes,
			sha256,
		});
		self.asset_paths.insert(hls_source.to_string(), relative.clone());
		Ok(relative)
	}
}

async fn sync_directory(path: &Path) -> Result<(), String> {
	let path = path.to_path_buf();
	tokio::task::spawn_blocking(move || -> io::Result<()> { std::fs::File::open(path)?.sync_all() })
		.await
		.map_err(|_| "The archive directory durability check stopped unexpectedly.".to_string())?
		.map_err(|error| format!("Unable to make the archive directory durable: {error}"))
}

async fn run_job(job: &ArchiveJob) -> Result<(), String> {
	// Keep restart-resumed captures queued while Reddit is unavailable. The
	// token daemon retries independently, so an upstream outage does not turn a
	// durable local queue into failed records or block Vale startup.
	while !crate::client::oauth_ready() {
		tokio::time::sleep(Duration::from_secs(5)).await;
	}
	let durable_reservation = {
		let mut connection = account::open_database()?;
		let transaction = connection
			.transaction_with_behavior(TransactionBehavior::Immediate)
			.map_err(|error| format!("Unable to lock the saved-post capture: {error}"))?;
		let durable_reservation = transaction
			.query_row(
				"SELECT reserved_bytes FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2 AND state IN ('reserved', 'capturing')",
				params![job.id, job.profile_id],
				|row| row.get::<_, i64>(0),
			)
			.optional()
			.map_err(|error| format!("Unable to inspect the saved-post reservation: {error}"))?
			.ok_or_else(|| "The saved-post capture has no durable storage reservation.".to_string())?
			.max(0) as u64;
		if durable_reservation != job.reservation_bytes || durable_reservation < MIN_CAPTURE_RESERVATION_BYTES {
			return Err("The saved-post storage reservation changed before capture began.".to_string());
		}
		let timestamp = account::now();
		transaction
			.execute(
				"UPDATE post_archives SET status = 'capturing', updated_at = ?1, error = '' WHERE id = ?2 AND profile_id = ?3 AND status IN ('queued', 'capturing')",
				params![timestamp, job.id, job.profile_id],
			)
			.map_err(|error| format!("Unable to start the saved-post capture: {error}"))?;
		transaction
			.execute(
				"UPDATE archive_reservations SET state = 'capturing', updated_at = ?1 WHERE archive_id = ?2 AND profile_id = ?3",
				params![timestamp, job.id, job.profile_id],
			)
			.map_err(|error| format!("Unable to activate the saved-post reservation: {error}"))?;
		transaction.commit().map_err(|error| format!("Unable to commit the saved-post capture start: {error}"))?;
		durable_reservation
	};

	let partial = partial_directory(job.profile_id, &job.id);
	let final_directory = archive_directory(job.profile_id, &job.id);
	if fs::try_exists(&final_directory).await.unwrap_or(false) {
		return Err("A published archive already exists for this capture and must be reconciled before writing.".to_string());
	}
	if fs::try_exists(&partial).await.unwrap_or(false) {
		fs::remove_dir_all(&partial)
			.await
			.map_err(|error| format!("Unable to clear the prior incomplete archive: {error}"))?;
	}
	fs::create_dir_all(partial.join("files/assets"))
		.await
		.map_err(|error| format!("Unable to create the saved-post directory: {error}"))?;

	let capture_limit = durable_reservation;

	let initial = json(format!("/comments/{}.json?sort=top&limit=500&depth=10&raw_json=1", job.post_id), true)
		.await
		.map_err(|message| format!("Reddit could not provide the post for local capture: {message}"))?;
	let post_thing = initial[0]["data"]["children"][0].clone();
	if post_thing.is_null() {
		return Err("Reddit did not return a post record for this identifier.".to_string());
	}
	let parsed = parse_post(&post_thing).await;
	if parsed.id.is_empty() {
		return Err("Reddit returned an invalid post record.".to_string());
	}
	let expected_comments = post_thing["data"]["num_comments"].as_u64().unwrap_or_default() as usize;
	let mut captured_comments = capture_comments(&initial, &job.post_id, expected_comments).await;
	let mut context = CaptureContext {
		directory: partial.clone(),
		item_limit: capture_limit,
		total_bytes: 0,
		assets: Vec::new(),
		generated_assets: Vec::new(),
		asset_paths: HashMap::new(),
		issues: std::mem::take(&mut captured_comments.issues),
	};

	let mut archived_media = Vec::new();
	match parsed.post_type.as_str() {
		"image" | "gif" => {
			if parsed.media.url.is_empty() {
				context.push_issue("post media", "Reddit identified media for the post without returning a usable source.");
			} else {
				match context.capture_reddit_asset(&parsed.media.url, MAX_REDDIT_ASSET_BYTES).await {
					Ok(path) => archived_media.push(ArchivedMedia {
						kind: parsed.post_type.clone(),
						path,
						caption: String::new(),
					}),
					Err(message) => context.push_issue("post media", format!("The main {} could not be stored: {message}", parsed.post_type)),
				}
			}
		}
		"video" => {
			let result = if !parsed.media.alt_url.is_empty() {
				context.capture_reddit_video(&parsed.media.alt_url).await
			} else {
				context.capture_reddit_asset(&parsed.media.url, MAX_REDDIT_ASSET_BYTES).await
			};
			match result {
				Ok(path) => archived_media.push(ArchivedMedia {
					kind: "video".to_string(),
					path,
					caption: String::new(),
				}),
				Err(message) => context.push_issue("post media", format!("The post video could not be stored: {message}")),
			}
		}
		"gallery" => {
			for (index, image) in parsed.gallery.iter().enumerate() {
				match context.capture_reddit_asset(&image.original_url, MAX_REDDIT_ASSET_BYTES).await {
					Ok(path) => archived_media.push(ArchivedMedia {
						kind: "image".to_string(),
						path,
						caption: image.caption.clone(),
					}),
					Err(message) => context.push_issue("post media", format!("Gallery item {} could not be stored: {message}", index + 1)),
				}
			}
		}
		_ => {}
	}

	let post_body = parsed.body.clone();
	let comments = std::mem::take(&mut captured_comments.comments);
	let (mut post_body, comments, inline_sources) = tokio::task::spawn_blocking(move || {
		let mut inline_sources = HashSet::new();
		html_media_sources(&post_body, &mut inline_sources);
		comment_media_sources(&comments, &mut inline_sources);
		(post_body, comments, inline_sources)
	})
	.await
	.map_err(|_| "The archive media scan stopped unexpectedly.".to_string())?;
	captured_comments.comments = comments;
	if inline_sources.len() > MAX_INLINE_MEDIA {
		context.push_issue(
			"embedded media",
			format!("The post and comments referenced more than {MAX_INLINE_MEDIA} embedded media files; later references were not fetched."),
		);
	}
	for source in inline_sources.into_iter().take(MAX_INLINE_MEDIA) {
		if let Err(message) = context.capture_reddit_asset(&source, MAX_EXTERNAL_REQUISITE_BYTES).await {
			context.push_issue("embedded media", format!("An embedded Reddit asset could not be stored ({source}): {message}"));
		}
	}
	let asset_paths = context.asset_paths.clone();
	let comments = std::mem::take(&mut captured_comments.comments);
	let (rewritten_body, rewritten_comments) = tokio::task::spawn_blocking(move || {
		let rewritten_body = rewrite_archived_html(&post_body, &asset_paths);
		let mut rewritten_comments = comments;
		rewrite_comment_assets(&mut rewritten_comments, &asset_paths);
		(rewritten_body, rewritten_comments)
	})
	.await
	.map_err(|_| "The archive HTML rewriting task stopped unexpectedly.".to_string())?;
	post_body = rewritten_body;
	captured_comments.comments = rewritten_comments;

	let source_url = parsed.out_url.clone().unwrap_or_default();
	let source_snapshot = if source_url.is_empty() || !matches!(parsed.post_type.as_str(), "link" | "image") {
		String::new()
	} else {
		match context.capture_external_source(&source_url).await {
			Ok(path) => path,
			Err(message) => {
				context.push_issue("source page", format!("The outbound source could not be captured safely: {message}"));
				String::new()
			}
		}
	};
	let captured_at = account::now();
	let archived_post = ArchivedPost {
		id: parsed.id.clone(),
		title: parsed.title.clone(),
		community: parsed.community.clone(),
		author: parsed.author.name.clone(),
		permalink: parsed.permalink.clone(),
		source_url: source_url.clone(),
		body_html: post_body,
		post_type: parsed.post_type.clone(),
		created: parsed.created.clone(),
		score: post_thing["data"]["score"].as_i64().unwrap_or_default(),
		upvote_ratio: parsed.upvote_ratio,
		media: archived_media,
		source_snapshot,
	};

	if context.issues.len() > 100 {
		let omitted = context.issues.len() - 99;
		context.issues.truncate(99);
		context.issues.push(issue(
			"capture report",
			format!("{omitted} additional repetitive omissions were suppressed from this report."),
		));
	}
	for (path, content_type, bytes) in READER_SUPPORT_ASSETS {
		context.write_reader_asset(path, content_type, bytes).await?;
	}
	let manifest = ArchiveManifest {
		format: "VALE_ARCHIVE_1".to_string(),
		reader_version: ARCHIVE_READER_VERSION,
		captured_at,
		comment_count: captured_comments.count,
		post: archived_post,
		comments: captured_comments.comments,
		assets: context.assets.clone(),
		generated_assets: context.generated_assets.clone(),
		issues: context.issues.clone(),
		initial_reddit_json: initial,
		additional_comment_things: captured_comments.things,
	};
	let (manifest, manifest_json, index, issues_json) = tokio::task::spawn_blocking(move || build_archive_documents(manifest))
		.await
		.map_err(|_| "The archive document rendering task stopped unexpectedly.".to_string())??;
	context.write_generated("manifest.json", &manifest_json).await?;
	context.write_generated("index.html", index.as_bytes()).await?;
	sync_directory(&partial).await?;
	let usage_path = partial.clone();
	let (exact_total_bytes, _) = tokio::task::spawn_blocking(move || directory_usage(&usage_path))
		.await
		.map_err(|_| "The archive accounting task stopped unexpectedly.".to_string())?
		.map_err(|error| format!("Unable to measure the completed archive: {error}"))?;
	if exact_total_bytes > durable_reservation {
		return Err(format!(
			"The completed archive exceeded its durable reservation by {}.",
			format_bytes(exact_total_bytes - durable_reservation)
		));
	}
	fs::rename(&partial, &final_directory)
		.await
		.map_err(|error| format!("Unable to publish the completed saved-post directory: {error}"))?;
	if let Some(parent) = final_directory.parent() {
		sync_directory(parent).await?;
	}

	let status = if manifest.issues.is_empty() { "ready" } else { "partial" };
	let mut connection = account::open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to lock the saved-post finalization: {error}"))?;
	transaction
		.execute(
			"UPDATE post_archives SET permalink = ?1, title = ?2, community = ?3, source_url = ?4, status = ?5, updated_at = ?6, captured_at = ?6, comment_count = ?7, asset_count = ?8, generated_asset_count = ?9, total_bytes = ?10, issues_json = ?11, error = '' WHERE id = ?12 AND profile_id = ?13",
			params![
				manifest.post.permalink,
				manifest.post.title,
				manifest.post.community,
				manifest.post.source_url,
				status,
				captured_at,
				captured_comments.count as i64,
				manifest.assets.len() as i64,
				manifest.generated_assets.len() as i64,
				exact_total_bytes as i64,
				issues_json,
				job.id,
				job.profile_id
			],
		)
		.map_err(|error| format!("Unable to finalize the saved-post database record: {error}"))?;
	transaction
		.execute(
			"DELETE FROM archive_reservations WHERE archive_id = ?1 AND profile_id = ?2",
			params![job.id, job.profile_id],
		)
		.map_err(|error| format!("Unable to replace the archive reservation with exact usage: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to commit the saved-post finalization: {error}"))?;
	Ok(())
}

fn safe_archive_relative(value: &str) -> Option<PathBuf> {
	let path = Path::new(value);
	if path.as_os_str().is_empty()
		|| path.is_absolute()
		|| value.split('/').any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
		|| path.components().any(|component| !matches!(component, Component::Normal(_)))
	{
		return None;
	}
	Some(path.to_path_buf())
}

fn archived_content_type(path: &Path) -> &'static str {
	match path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
		"html" | "htm" => "text/html; charset=utf-8",
		"json" => "application/json; charset=utf-8",
		"css" => "text/css; charset=utf-8",
		"txt" => "text/plain; charset=utf-8",
		"woff2" => "font/woff2",
		"jpg" | "jpeg" => "image/jpeg",
		"png" => "image/png",
		"gif" => "image/gif",
		"webp" => "image/webp",
		"avif" => "image/avif",
		"svg" => "image/svg+xml",
		"mp4" | "m4v" => "video/mp4",
		"webm" => "video/webm",
		"mp3" => "audio/mpeg",
		"m4a" => "audio/mp4",
		"pdf" => "application/pdf",
		_ => "application/octet-stream",
	}
}

async fn serve_archive_file(request: Request<Body>, relative: &str) -> Result<Response<Body>, String> {
	let Some(profile_id) = account::context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required to view that saved file."));
	};
	let archive_id = request.param("archive_id").unwrap_or_default();
	let Some(entry) = entry_for_profile(profile_id, &archive_id)? else {
		return Ok(plain_response(StatusCode::NOT_FOUND, "That saved post does not exist in this profile."));
	};
	if !entry.is_viewable() {
		return Ok(plain_response(StatusCode::CONFLICT, "That saved post is not ready to view."));
	}
	let Some(relative) = safe_archive_relative(relative) else {
		return Ok(plain_response(StatusCode::BAD_REQUEST, "That saved-file path is invalid."));
	};
	let root = archive_directory(profile_id, &archive_id);
	let root_canonical = fs::canonicalize(&root).await.map_err(|_| "The saved-post directory is unavailable.".to_string())?;
	let path = root.join(relative);
	let canonical = match fs::canonicalize(&path).await {
		Ok(path) if path.starts_with(&root_canonical) => path,
		_ => return Ok(plain_response(StatusCode::NOT_FOUND, "That saved file does not exist.")),
	};
	let metadata = fs::metadata(&canonical).await.map_err(|_| "The saved file is unavailable.".to_string())?;
	if !metadata.is_file() {
		return Ok(plain_response(StatusCode::NOT_FOUND, "That saved file does not exist."));
	}
	let content_type = archived_content_type(&canonical);
	let file = fs::File::open(&canonical).await.map_err(|_| "The saved file could not be opened.".to_string())?;
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, content_type)
			.header(header::CONTENT_LENGTH, metadata.len())
			.header(header::CACHE_CONTROL, "private, no-store")
			.header("Content-Security-Policy", ARCHIVE_CSP)
			.body(Body::wrap_stream(tokio_util::io::ReaderStream::new(file)))
			.unwrap_or_default(),
	)
}

pub async fn view_get(request: Request<Body>) -> Result<Response<Body>, String> {
	serve_archive_file(request, "index.html").await
}

pub async fn manifest_get(request: Request<Body>) -> Result<Response<Body>, String> {
	serve_archive_file(request, "manifest.json").await
}

pub async fn file_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let relative = request.param("path").unwrap_or_default();
	serve_archive_file(request, &format!("files/{relative}")).await
}

fn html_media_sources(html: &str, into: &mut HashSet<String>) {
	for captures in HTML_MEDIA_URL.captures_iter(html) {
		if let Some(source) = captures.name("url").map(|value| value.as_str()) {
			if is_local_media_path(source) {
				into.insert(source.to_string());
			}
		}
	}
}

fn comment_media_sources(comments: &[ArchivedComment], into: &mut HashSet<String>) {
	for comment in comments {
		html_media_sources(&comment.body_html, into);
		comment_media_sources(&comment.replies, into);
	}
}

fn rewrite_archived_html(html: &str, paths: &HashMap<String, String>) -> String {
	let rewritten = rewrite_str(
		html,
		RewriteStrSettings::new().append_element_content_handler(element!("*", |element| {
			for attribute in ["src", "poster", "href"] {
				let Some(value) = element.get_attribute(attribute) else {
					continue;
				};
				let encoded = value.replace('&', "&amp;");
				if let Some(path) = paths.get(&value).or_else(|| paths.get(&encoded)) {
					element.set_attribute(attribute, path)?;
				} else if attribute == "href" && value.starts_with('/') {
					element.set_attribute(attribute, &format!("https://www.reddit.com{value}"))?;
				}
			}
			Ok(())
		})),
	);
	rewritten.unwrap_or_else(|_| html.to_string())
}

fn rewrite_comment_assets(comments: &mut [ArchivedComment], paths: &HashMap<String, String>) {
	for comment in comments {
		comment.body_html = rewrite_archived_html(&comment.body_html, paths);
		rewrite_comment_assets(&mut comment.replies, paths);
	}
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
	let [a, b, c, _] = address.octets();
	!matches!(
		(a, b, c),
		(0, _, _)
			| (10, _, _)
			| (100, 64..=127, _)
			| (127, _, _)
			| (169, 254, _)
			| (172, 16..=31, _)
			| (192, 0, 0)
			| (192, 0, 2)
			| (192, 88, 99)
			| (192, 168, _)
			| (198, 18..=19, _)
			| (198, 51, 100)
			| (203, 0, 113)
			| (224..=255, _, _)
	)
}

async fn public_addresses(url: &Url) -> Result<Vec<SocketAddr>, String> {
	if !matches!(url.scheme(), "http" | "https") {
		return Err("Only HTTP and HTTPS source pages can be archived.".to_string());
	}
	if !url.username().is_empty() || url.password().is_some() {
		return Err("Source-page addresses cannot contain credentials.".to_string());
	}
	let host = url.host_str().ok_or_else(|| "The source page has no host.".to_string())?;
	let port = url.port_or_known_default().ok_or_else(|| "The source page has no valid port.".to_string())?;
	if !matches!(port, 80 | 443) {
		return Err("Source-page capture is restricted to standard web ports.".to_string());
	}
	let resolved = lookup_host((host, port)).await.map_err(|_| "The source page host could not be resolved.".to_string())?;
	let mut addresses = resolved
		.filter_map(|address| match address.ip() {
			IpAddr::V4(ip) if is_public_ipv4(ip) => Some(SocketAddr::new(IpAddr::V4(ip), port)),
			_ => None,
		})
		.collect::<Vec<_>>();
	addresses.sort_unstable();
	addresses.dedup();
	if addresses.is_empty() {
		return Err("The source page did not resolve to a permitted public IPv4 address.".to_string());
	}
	Ok(addresses)
}

pub(crate) async fn public_response(source: &str) -> Result<(Url, wreq::Response), String> {
	let mut current = Url::parse(source).map_err(|_| "The source page address is invalid.".to_string())?;
	for _ in 0..=MAX_EXTERNAL_REDIRECTS {
		let host = current.host_str().ok_or_else(|| "The source page has no host.".to_string())?.to_string();
		let addresses = public_addresses(&current).await?;
		let client = wreq::Client::builder()
			.redirect(Policy::none())
			.resolve_to_addrs(host, addresses)
			.build()
			.map_err(|_| "Vale could not initialize its isolated source-page client.".to_string())?;
		let request = client
			.get(current.as_str())
			.header("User-Agent", "Vale local archive")
			.header("Accept", "text/html,application/xhtml+xml,application/pdf,image/*,video/*;q=0.8,*/*;q=0.2")
			.send();
		let response = timeout(EXTERNAL_FETCH_TIMEOUT, request)
			.await
			.map_err(|_| "The source page timed out.".to_string())?
			.map_err(|_| "The source page could not be retrieved.".to_string())?;
		if !response.status().is_redirection() {
			return Ok((current, response));
		}
		let location = response
			.headers()
			.get(header::LOCATION.as_str())
			.and_then(|value| value.to_str().ok())
			.ok_or_else(|| "The source page redirected without a usable destination.".to_string())?;
		if location.len() > MAX_EXTERNAL_REDIRECT_LOCATION_BYTES {
			return Err("The source page returned an oversized redirect destination.".to_string());
		}
		current = current.join(location).map_err(|_| "The source page redirected to an invalid destination.".to_string())?;
	}
	Err("The source page exceeded the redirect boundary.".to_string())
}

async fn response_bytes(response: wreq::Response, maximum: u64) -> Result<(String, Vec<u8>), String> {
	if !response.status().is_success() {
		return Err(format!("The source returned HTTP {}.", response.status().as_u16()));
	}
	if response.content_length().is_some_and(|length| length > maximum) {
		return Err(format!("The source response is larger than {}.", format_bytes(maximum)));
	}
	let content_type = response
		.headers()
		.get(header::CONTENT_TYPE.as_str())
		.and_then(|value| value.to_str().ok())
		.unwrap_or("application/octet-stream")
		.to_string();
	let mut bytes = Vec::new();
	let mut stream = response.bytes_stream();
	loop {
		let next = timeout(TRANSFER_IDLE_TIMEOUT, stream.next())
			.await
			.map_err(|_| "The source transfer stopped making progress.".to_string())?;
		let Some(chunk) = next else {
			break;
		};
		let chunk = chunk.map_err(|_| "The source transfer ended before it completed.".to_string())?;
		if (bytes.len() as u64).saturating_add(chunk.len() as u64) > maximum {
			return Err(format!("The source response exceeded {}.", format_bytes(maximum)));
		}
		bytes.extend_from_slice(&chunk);
	}
	if bytes.is_empty() {
		return Err("The source returned an empty response.".to_string());
	}
	Ok((content_type, bytes))
}

fn prepare_external_html(bytes: Vec<u8>) -> (String, Vec<String>, String) {
	let sha256 = format!("{:x}", Sha256::digest(&bytes));
	let mut html = String::from_utf8_lossy(&bytes).into_owned();
	html = ACTIVE_HTML.replace_all(&html, "").into_owned();
	html = REFRESH_META.replace_all(&html, "").into_owned();
	html = BASE_ELEMENT.replace_all(&html, "").into_owned();
	html = EVENT_ATTRIBUTE.replace_all(&html, "").into_owned();
	html = JAVASCRIPT_URL.replace_all(&html, "$1=\"#\"").into_owned();
	let resources = PAGE_RESOURCE_URL
		.captures_iter(&html)
		.filter_map(|captures| {
			let attribute = captures.name("attribute")?.as_str().to_ascii_lowercase();
			let raw = captures.name("url")?.as_str().to_string();
			if raw.starts_with("data:") || raw.starts_with("blob:") || raw.starts_with('#') {
				return None;
			}
			let likely_stylesheet = raw.split('?').next().is_some_and(|path| path.to_ascii_lowercase().ends_with(".css"));
			(attribute != "href" || likely_stylesheet).then_some(raw)
		})
		.collect::<HashSet<_>>()
		.into_iter()
		.take(MAX_EXTERNAL_REQUISITES + 1)
		.collect();
	(html, resources, sha256)
}

fn finish_external_html(mut html: String, final_url: &str, raw_path: &str) -> (String, String) {
	let policy = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{ARCHIVE_CSP}\"><meta name=\"referrer\" content=\"no-referrer\">");
	if let Some(index) = html.to_ascii_lowercase().find("<head>") {
		html.insert_str(index + "<head>".len(), &policy);
	} else {
		html = format!("<!doctype html><html><head>{policy}</head><body>{html}</body></html>");
	}
	let provenance = format!(
		"<aside style=\"padding:12px;background:#111;color:#ddd;font:14px system-ui\">Locally captured by Vale from <a style=\"color:#7adac8\" href=\"{}\">{}</a>. Active content was removed. <a style=\"color:#7adac8\" href=\"../../{}\">Original response</a>.</aside>",
		htmlescape::encode_attribute(final_url),
		htmlescape::encode_minimal(final_url),
		raw_path
	);
	if let Some(index) = html.to_ascii_lowercase().find("<body") {
		if let Some(close) = html[index..].find('>') {
			html.insert_str(index + close + 1, &provenance);
		}
	}
	let sha256 = format!("{:x}", Sha256::digest(html.as_bytes()));
	(html, sha256)
}

impl CaptureContext {
	async fn store_bytes_asset(&mut self, source_key: &str, original_url: &str, content_type: &str, bytes: Vec<u8>) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(source_key).or_else(|| self.asset_paths.get(original_url)).cloned() {
			self.asset_paths.insert(source_key.to_string(), path.clone());
			return Ok(path);
		}
		if bytes.len() as u64 > self.remaining_for_assets() {
			return Err("The archive reached its configured size boundary.".to_string());
		}
		let extension = content_type_extension(content_type, original_url);
		let url_digest = format!("{:x}", Sha256::digest(original_url.as_bytes()));
		let filename = format!("{url_digest}.{extension}");
		let relative = format!("files/assets/{filename}");
		let directory = self.directory.join("files/assets");
		fs::create_dir_all(&directory)
			.await
			.map_err(|error| format!("Unable to prepare the archive asset directory: {error}"))?;
		let destination = directory.join(&filename);
		let mut file = fs::File::create(&destination)
			.await
			.map_err(|error| format!("Unable to create an archived source file: {error}"))?;
		file.write_all(&bytes).await.map_err(|error| format!("Unable to write an archived source file: {error}"))?;
		file.sync_all().await.map_err(|error| format!("Unable to make an archived source file durable: {error}"))?;
		let length = bytes.len() as u64;
		let sha256 = tokio::task::spawn_blocking(move || format!("{:x}", Sha256::digest(bytes)))
			.await
			.map_err(|_| "The archive checksum task stopped unexpectedly.".to_string())?;
		self.total_bytes = self.total_bytes.saturating_add(length);
		self.assets.push(ArchiveAsset {
			path: relative.clone(),
			original_url: original_url.to_string(),
			content_type: content_type.to_string(),
			bytes: length,
			sha256,
		});
		self.asset_paths.insert(source_key.to_string(), relative.clone());
		self.asset_paths.insert(original_url.to_string(), relative.clone());
		Ok(relative)
	}

	async fn write_generated(&mut self, relative: &str, bytes: &[u8]) -> Result<(), String> {
		if bytes.len() as u64 > self.remaining() {
			return Err("The archive reached its configured size boundary while writing its reader.".to_string());
		}
		let destination = self.directory.join(relative);
		if let Some(parent) = destination.parent() {
			fs::create_dir_all(parent)
				.await
				.map_err(|error| format!("Unable to prepare an archive directory: {error}"))?;
		}
		let mut file = fs::File::create(&destination)
			.await
			.map_err(|error| format!("Unable to create an archive document: {error}"))?;
		file.write_all(bytes).await.map_err(|error| format!("Unable to write an archive document: {error}"))?;
		file.sync_all().await.map_err(|error| format!("Unable to make an archive document durable: {error}"))?;
		self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
		Ok(())
	}

	async fn write_reader_asset(&mut self, relative: &str, content_type: &str, bytes: &[u8]) -> Result<(), String> {
		self.write_generated(relative, bytes).await?;
		self.generated_assets.push(GeneratedArchiveAsset {
			path: relative.to_string(),
			content_type: content_type.to_string(),
			bytes: bytes.len() as u64,
			sha256: format!("{:x}", Sha256::digest(bytes)),
		});
		Ok(())
	}

	async fn capture_external_source(&mut self, source: &str) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(source).cloned() {
			return Ok(path);
		}
		let (final_url, response) = public_response(source).await?;
		let maximum = MAX_EXTERNAL_DOCUMENT_BYTES.min(self.remaining_for_assets() / 2);
		let (content_type, bytes) = response_bytes(response, maximum).await?;
		let media_type = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
		if media_type != "text/html" && media_type != "application/xhtml+xml" {
			return self.store_bytes_asset(source, final_url.as_str(), &content_type, bytes).await;
		}

		let raw_path = "files/source/original-response.bin".to_string();
		self.write_generated(&raw_path, &bytes).await?;
		let raw_bytes = bytes.len() as u64;
		let (mut html, resources, raw_sha256) = tokio::task::spawn_blocking(move || prepare_external_html(bytes))
			.await
			.map_err(|_| "The source-page sanitizing task stopped unexpectedly.".to_string())?;
		self.assets.push(ArchiveAsset {
			path: raw_path.clone(),
			original_url: final_url.to_string(),
			content_type: "application/octet-stream".to_string(),
			bytes: raw_bytes,
			sha256: raw_sha256,
		});
		self.push_issue(
			"source page",
			"The linked HTML page was preserved as a sanitized, best-effort snapshot. Active code and resources discoverable only after script execution were deliberately not stored.",
		);
		let sanitized_document_reserve = (html.len() as u64).saturating_add(128 * 1024);
		if resources.len() > MAX_EXTERNAL_REQUISITES {
			self.push_issue(
				"source page",
				format!("The linked page referenced more than {MAX_EXTERNAL_REQUISITES} direct assets; additional assets were left as omissions."),
			);
		}
		for raw in resources.into_iter().take(MAX_EXTERNAL_REQUISITES) {
			let resource_allowance = self.remaining_for_assets().saturating_sub(sanitized_document_reserve);
			if resource_allowance == 0 {
				self.push_issue(
					"source page",
					"The remaining capture allowance was reserved for the sanitized offline document; later page assets were omitted.",
				);
				break;
			}
			let Ok(resource_url) = final_url.join(&raw) else {
				self.push_issue("source page", format!("A linked-page asset used an invalid address: {raw}"));
				continue;
			};
			if !matches!(resource_url.scheme(), "http" | "https") {
				continue;
			}
			let result = async {
				let (resolved_url, response) = public_response(resource_url.as_str()).await?;
				let path = self
					.store_response(&raw, resolved_url.as_str(), response, MAX_EXTERNAL_REQUISITE_BYTES.min(resource_allowance))
					.await?;
				Ok::<String, String>(path)
			}
			.await;
			match result {
				Ok(path) => {
					let source_relative = path.strip_prefix("files/").map(|path| format!("../{path}")).unwrap_or(path);
					html = tokio::task::spawn_blocking(move || html.replace(&raw, &source_relative))
						.await
						.map_err(|_| "The source-page rewriting task stopped unexpectedly.".to_string())?;
				}
				Err(message) => self.push_issue("source page", format!("A linked-page asset could not be stored ({raw}): {message}")),
			}
		}
		let final_url_string = final_url.to_string();
		let raw_path_for_reader = raw_path.clone();
		let (html, html_sha256) = tokio::task::spawn_blocking(move || finish_external_html(html, &final_url_string, &raw_path_for_reader))
			.await
			.map_err(|_| "The source-page finalization task stopped unexpectedly.".to_string())?;
		let sanitized_path = "files/source/index.html";
		if html.len() as u64 > self.remaining_for_assets() {
			return Err("The sanitized source page no longer fit inside the capture allowance reserved for it.".to_string());
		}
		self.write_generated(sanitized_path, html.as_bytes()).await?;
		self.assets.push(ArchiveAsset {
			path: sanitized_path.to_string(),
			original_url: final_url.to_string(),
			content_type: "text/html; charset=utf-8".to_string(),
			bytes: html.len() as u64,
			sha256: html_sha256,
		});
		self.asset_paths.insert(source.to_string(), sanitized_path.to_string());
		Ok(sanitized_path.to_string())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rusqlite::Connection;
	use serde_json::json;
	use std::sync::atomic::AtomicUsize;

	fn initialize_archive_database(connection: &Connection) {
		connection
			.execute_batch(
				"CREATE TABLE profiles (id INTEGER PRIMARY KEY);
				 INSERT INTO profiles (id) VALUES (7), (8), (9);
				 CREATE TABLE post_archives (
					id TEXT PRIMARY KEY,
					profile_id INTEGER NOT NULL,
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
					error TEXT NOT NULL DEFAULT ''
				);
				 CREATE TABLE profile_archive_settings (
					profile_id INTEGER PRIMARY KEY,
					archive_budget_mib INTEGER NOT NULL DEFAULT 0,
					revision INTEGER NOT NULL DEFAULT 0,
					updated_at INTEGER NOT NULL DEFAULT 0
				 );
				 CREATE TABLE archive_reservations (
					archive_id TEXT PRIMARY KEY,
					profile_id INTEGER NOT NULL,
					state TEXT NOT NULL,
					reserved_bytes INTEGER NOT NULL,
					created_at INTEGER NOT NULL,
					updated_at INTEGER NOT NULL
				 );",
			)
			.unwrap();
	}

	fn archive_database() -> Connection {
		let connection = Connection::open_in_memory().unwrap();
		initialize_archive_database(&connection);
		connection
	}

	fn insert_archive(connection: &Connection, id: &str, profile_id: i64, status: &str, timestamp: i64) {
		connection
			.execute(
				"INSERT INTO post_archives (id, profile_id, post_id, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
				params![id, profile_id, format!("post-{id}"), status, timestamp],
			)
			.unwrap();
	}

	fn manifest_fixture() -> ArchiveManifest {
		serde_json::from_value(json!({
			"format": "VALE_ARCHIVE_1",
			"captured_at": 1_700_000_000,
			"comment_count": 0,
			"post": {
				"id": "fixture",
				"title": "A deterministic Vale archive title",
				"community": "homelab",
				"author": "reader",
				"permalink": "/r/homelab/comments/fixture/title/",
				"source_url": "",
				"body_html": "<p>Offline prose.</p>",
				"post_type": "text",
				"created": "2023-11-14 22:13 UTC",
				"score": 4,
				"upvote_ratio": 91,
				"media": [],
				"source_snapshot": ""
			},
			"comments": [],
			"assets": [],
			"issues": [],
			"initial_reddit_json": {},
			"additional_comment_things": []
		}))
		.unwrap()
	}

	#[test]
	fn version_one_manifest_defaults_are_backward_compatible_without_rewrite() {
		let fixture = serde_json::to_vec(&json!({
			"format": "VALE_ARCHIVE_1",
			"captured_at": 1_700_000_000,
			"comment_count": 0,
			"post": {
				"id": "fixture", "title": "Fixture", "community": "homelab", "author": "reader",
				"permalink": "/r/homelab/comments/fixture/title/", "source_url": "", "body_html": "",
				"post_type": "text", "created": "now", "score": 0, "upvote_ratio": 100, "media": [], "source_snapshot": ""
			},
			"comments": [], "assets": [], "issues": [], "initial_reddit_json": {}, "additional_comment_things": []
		}))
		.unwrap();
		let original = fixture.clone();
		let manifest: ArchiveManifest = serde_json::from_slice(&fixture).unwrap();
		assert_eq!(manifest.reader_version, 1);
		assert!(manifest.generated_assets.is_empty());
		assert_eq!(manifest.format, "VALE_ARCHIVE_1");
		assert_eq!(fixture, original);
	}

	#[test]
	fn reader_v2_render_is_deterministic_closed_and_uses_one_csp() {
		let mut manifest = manifest_fixture();
		manifest.reader_version = 2;
		let first = render_archive_reader(&manifest).unwrap();
		let second = render_archive_reader(&manifest).unwrap();
		assert_eq!(first, second);
		assert!(first.contains("<html lang=\"en\" data-vale-reader-version=\"2\">"));
		assert!(first.contains(&format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{ARCHIVE_CSP}\">")));
		assert!(first.contains("prefers-color-scheme: light"));
		assert!(first.contains("@media print"));
		assert!(first.contains("source-sans-3.woff2"));
		assert!(first.contains("source-serif-4.woff2"));
		assert!(first.contains("vale-mark.svg"));
		for forbidden in ["<script", "linear-gradient", "box-shadow", "text-shadow", "drop-shadow"] {
			assert!(!first.to_ascii_lowercase().contains(forbidden), "reader contains forbidden token {forbidden}");
		}
	}

	#[test]
	fn archive_document_build_preserves_manifest_and_reader() {
		let manifest = manifest_fixture();
		let expected = serde_json::to_vec_pretty(&manifest).unwrap();
		let (manifest, bytes, reader, issues) = build_archive_documents(manifest).unwrap();
		assert_eq!(bytes, expected);
		assert!(reader.contains("A deterministic Vale archive title"));
		assert_eq!(issues, "[]");
		assert_eq!(manifest.format, "VALE_ARCHIVE_1");
	}

	#[test]
	fn reader_v3_has_a_diagnostic_marker_and_scoped_comment_heading_styles() {
		assert_eq!(ARCHIVE_READER_VERSION, 3);
		let mut manifest = manifest_fixture();
		manifest.reader_version = ARCHIVE_READER_VERSION;
		let reader = render_archive_reader(&manifest).unwrap();
		assert!(reader.contains("<html lang=\"en\" data-vale-reader-version=\"3\">"));
		for expected in [
			".archive-comment-body :is(h3, h4, h5, h6)",
			".archive-comment-body h3 { font-size: 1.15rem; font-weight: 750; }",
			".archive-comment-body h4 { font-size: 1.08rem; font-weight: 730; }",
			".archive-comment-body h5 { font-size: 1rem; font-weight: 700; }",
			".archive-comment-body h6 { font-size: .94rem; font-weight: 700; }",
			"break-after: avoid",
		] {
			assert!(reader.contains(expected), "Reader v3 omitted {expected}");
		}
	}

	#[test]
	fn future_manifest_deserializes_but_reader_regeneration_fails_closed() {
		let mut manifest = manifest_fixture();
		manifest.reader_version = ARCHIVE_READER_VERSION + 1;
		let encoded = serde_json::to_vec(&manifest).unwrap();
		let inspected: ArchiveManifest = serde_json::from_slice(&encoded).unwrap();
		assert_eq!(inspected.reader_version, 4);
		let error = render_archive_reader(&inspected).unwrap_err();
		assert!(error.contains("newer than this Vale build supports"));
		assert_eq!(encoded, serde_json::to_vec(&manifest).unwrap());
	}

	#[test]
	fn legacy_v1_and_v2_manifest_index_and_embedded_css_bytes_remain_immutable() {
		let temporary = std::env::temp_dir().join(format!("vale-legacy-reader-test-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&temporary).unwrap();
		for version in [1_u16, 2_u16] {
			let directory = temporary.join(version.to_string());
			std::fs::create_dir_all(&directory).unwrap();
			let mut manifest = manifest_fixture();
			manifest.reader_version = version;
			let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
			let index_bytes = format!("<!doctype html><style>legacy-reader-{version}</style><p>reader {version}</p>").into_bytes();
			std::fs::write(directory.join("manifest.json"), &manifest_bytes).unwrap();
			std::fs::write(directory.join("index.html"), &index_bytes).unwrap();

			let inspected: ArchiveManifest = serde_json::from_slice(&std::fs::read(directory.join("manifest.json")).unwrap()).unwrap();
			assert_eq!(inspected.reader_version, version);
			assert!(render_archive_reader(&inspected).is_ok());
			assert_eq!(std::fs::read(directory.join("manifest.json")).unwrap(), manifest_bytes);
			assert_eq!(std::fs::read(directory.join("index.html")).unwrap(), index_bytes);
		}
		std::fs::remove_dir_all(&temporary).unwrap();
	}

	#[tokio::test]
	async fn generated_reader_assets_have_exact_metadata_and_disk_accounting() {
		let temporary = std::env::temp_dir().join(format!("vale-reader-assets-test-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&temporary).unwrap();
		let mut context = CaptureContext {
			directory: temporary.clone(),
			item_limit: FINALIZATION_RESERVE_BYTES,
			total_bytes: 0,
			assets: Vec::new(),
			generated_assets: Vec::new(),
			asset_paths: HashMap::new(),
			issues: Vec::new(),
		};
		for (path, content_type, bytes) in READER_SUPPORT_ASSETS {
			context.write_reader_asset(path, content_type, bytes).await.unwrap();
		}
		assert_eq!(context.generated_assets.len(), READER_SUPPORT_ASSETS.len());
		for generated in &context.generated_assets {
			let bytes = std::fs::read(temporary.join(&generated.path)).unwrap();
			assert_eq!(generated.bytes, bytes.len() as u64);
			assert_eq!(generated.sha256, format!("{:x}", Sha256::digest(&bytes)));
			assert_eq!(archived_content_type(Path::new(&generated.path)), generated.content_type);
		}
		let (disk_bytes, disk_files) = directory_usage(&temporary).unwrap();
		assert_eq!(disk_bytes, context.total_bytes);
		assert_eq!(disk_files, context.generated_assets.len() as u64);
		std::fs::remove_dir_all(&temporary).unwrap();
	}

	#[test]
	fn deletion_failure_remains_tombstoned_for_startup_retry() {
		let connection = archive_database();
		let id = uuid::Uuid::new_v4().to_string();
		insert_archive(&connection, &id, 7, "deleting", 1);

		set_deletion_failure(&connection, &id, 7, "test removal failure").unwrap();

		let (status, error): (String, String) = connection
			.query_row("SELECT status, error FROM post_archives WHERE id = ?1", params![id], |row| Ok((row.get(0)?, row.get(1)?)))
			.unwrap();
		assert_eq!(status, "deleting");
		assert!(error.contains("test removal failure"));
	}

	#[test]
	fn startup_reconciliation_accounts_published_partial_deleting_and_orphan_states() {
		let root = std::env::temp_dir().join(format!("vale-archive-reconcile-test-{}", uuid::Uuid::new_v4()));
		let profile_directory = root.join("7");
		std::fs::create_dir_all(&profile_directory).unwrap();
		let mut connection = archive_database();
		let published_id = uuid::Uuid::new_v4().to_string();
		let partial_id = uuid::Uuid::new_v4().to_string();
		let deleting_id = uuid::Uuid::new_v4().to_string();
		let failed_id = uuid::Uuid::new_v4().to_string();
		for (id, status, timestamp) in [
			(&published_id, "capturing", 1),
			(&partial_id, "queued", 2),
			(&deleting_id, "deleting", 3),
			(&failed_id, "failed", 4),
		] {
			insert_archive(&connection, id, 7, status, timestamp);
		}
		for id in [&published_id, &partial_id, &failed_id] {
			connection
				.execute(
					"INSERT INTO archive_reservations (archive_id, profile_id, state, reserved_bytes, created_at, updated_at) VALUES (?1, 7, 'capturing', ?2, 1, 1)",
					params![id, (256 * MIB) as i64],
				)
				.unwrap();
		}
		let published = profile_directory.join(&published_id);
		std::fs::create_dir_all(&published).unwrap();
		std::fs::write(published.join("manifest.json"), serde_json::to_vec(&manifest_fixture()).unwrap()).unwrap();
		std::fs::write(published.join("index.html"), b"reader").unwrap();
		std::fs::create_dir_all(profile_directory.join(format!(".{partial_id}.partial"))).unwrap();
		std::fs::write(profile_directory.join(format!(".{partial_id}.partial/chunk")), b"partial").unwrap();
		std::fs::create_dir_all(profile_directory.join(&deleting_id)).unwrap();
		std::fs::write(profile_directory.join(&deleting_id).join("index.html"), b"delete me").unwrap();
		std::fs::create_dir_all(profile_directory.join(format!(".{failed_id}.partial"))).unwrap();
		std::fs::write(profile_directory.join(format!(".{failed_id}.partial/chunk")), b"failed").unwrap();

		reconcile_database_archives(&mut connection, &root).unwrap();
		let (published_status, published_total): (String, i64) = connection
			.query_row("SELECT status, total_bytes FROM post_archives WHERE id = ?1", params![published_id], |row| {
				Ok((row.get(0)?, row.get(1)?))
			})
			.unwrap();
		assert_eq!(published_status, "ready");
		assert_eq!(published_total as u64, directory_usage(&published).unwrap().0);
		assert_eq!(
			connection
				.query_row("SELECT COUNT(*) FROM archive_reservations WHERE archive_id = ?1", params![published_id], |row| row
					.get::<_, i64>(0))
				.unwrap(),
			0
		);
		assert!(!profile_directory.join(format!(".{partial_id}.partial")).exists());
		assert_eq!(
			connection
				.query_row("SELECT status FROM post_archives WHERE id = ?1", params![partial_id], |row| row.get::<_, String>(0))
				.unwrap(),
			"queued"
		);
		assert_eq!(
			connection
				.query_row("SELECT COUNT(*) FROM archive_reservations WHERE archive_id = ?1", params![partial_id], |row| row
					.get::<_, i64>(0))
				.unwrap(),
			1
		);
		assert_eq!(
			connection
				.query_row("SELECT COUNT(*) FROM post_archives WHERE id = ?1", params![deleting_id], |row| row.get::<_, i64>(0))
				.unwrap(),
			0
		);
		assert!(!profile_directory.join(&deleting_id).exists());
		assert_eq!(
			connection
				.query_row("SELECT status FROM post_archives WHERE id = ?1", params![failed_id], |row| row.get::<_, String>(0))
				.unwrap(),
			"failed"
		);
		assert_eq!(
			connection
				.query_row("SELECT COUNT(*) FROM archive_reservations WHERE archive_id = ?1", params![failed_id], |row| row
					.get::<_, i64>(0))
				.unwrap(),
			0
		);

		let orphan_id = uuid::Uuid::new_v4().to_string();
		let orphan = profile_directory.join(&orphan_id);
		std::fs::create_dir_all(&orphan).unwrap();
		std::fs::write(orphan.join("orphan.bin"), b"orphan bytes").unwrap();
		reconcile_orphan_archives(&mut connection, &root).unwrap();
		let (orphan_status, orphan_total): (String, i64) = connection
			.query_row("SELECT status, total_bytes FROM post_archives WHERE id = ?1", params![orphan_id], |row| {
				Ok((row.get(0)?, row.get(1)?))
			})
			.unwrap();
		assert_eq!(orphan_status, "cleanup_failed");
		assert_eq!(orphan_total, b"orphan bytes".len() as i64);
		std::fs::remove_dir_all(&root).unwrap();
	}

	fn comment(id: &str, parent: &str, score: i64, replies: Value) -> Value {
		json!({
			"kind": "t1",
			"data": {
				"id": id,
				"parent_id": parent,
				"author": "archived-user",
				"body": format!("body {id}"),
				"body_html": format!("<div class=\"md\"><p>body {id}</p></div>"),
				"created_utc": 1_700_000_000.0,
				"score": score,
				"replies": replies
			}
		})
	}

	#[tokio::test]
	async fn comment_capture_sorts_roots_and_keeps_reply_relationships() {
		let reply = comment("reply", "t1_low", 90, Value::String(String::new()));
		let low = comment("low", "t3_post", 2, json!({"data": {"children": [reply]}}));
		let high = comment("high", "t3_post", 50, Value::String(String::new()));
		let initial = json!([
			{"data": {"children": [{"data": {"num_comments": 3}}]}},
			{"data": {"children": [low, high]}}
		]);
		let captured = capture_comments(&initial, "post", 3).await;
		assert_eq!(captured.count, 3);
		assert!(captured.issues.is_empty());
		assert_eq!(captured.comments[0].id, "high");
		assert_eq!(captured.comments[1].id, "low");
		assert_eq!(captured.comments[1].replies[0].id, "reply");
	}

	#[tokio::test]
	async fn archived_comment_headings_reset_for_each_root_and_reply_without_mutating_raw_snapshots() {
		let mut reply = comment("reply", "t1_root", 9, Value::String(String::new()));
		reply["data"]["body_html"] = Value::String(r#"<div class="md"><h6>Reply first</h6><img src="/img/reply.png"></div>"#.to_string());
		let mut root = comment("root", "t3_post", 10, json!({"data": {"children": [reply]}}));
		root["data"]["body_html"] = Value::String(r#"<div class="md"><h1>Root one</h1><h6>Root two</h6><img src="/img/root.png"></div>"#.to_string());
		let initial = json!([
			{"data": {"children": [{"data": {"num_comments": 2}}]}},
			{"data": {"children": [root]}}
		]);
		let raw_snapshot = initial.clone();

		let mut captured = capture_comments(&initial, "post", 2).await;
		assert!(captured.issues.is_empty());
		assert_eq!(
			captured.comments[0].body_html,
			r#"<div class="md"><h3>Root one</h3><h4>Root two</h4><img src="/img/root.png"></div>"#
		);
		assert_eq!(
			captured.comments[0].replies[0].body_html,
			r#"<div class="md"><h3>Reply first</h3><img src="/img/reply.png"></div>"#
		);

		let paths = HashMap::from([
			("/img/root.png".to_string(), "files/assets/root.png".to_string()),
			("/img/reply.png".to_string(), "files/assets/reply.png".to_string()),
		]);
		rewrite_comment_assets(&mut captured.comments, &paths);
		assert!(captured.comments[0].body_html.contains("<h3>Root one</h3><h4>Root two</h4>"));
		assert!(captured.comments[0].replies[0].body_html.contains("<h3>Reply first</h3>"));
		assert!(!captured.comments[0].body_html.contains("<h1"));
		assert!(!captured.comments[0].body_html.contains("<h2"));
		assert_eq!(initial, raw_snapshot);
		assert!(initial.to_string().contains("<h1>Root one</h1><h6>Root two</h6>"));
	}

	#[test]
	fn archived_asset_rewrite_changes_url_attributes_only() {
		let html = r#"<div class="md" data-note="/img/root.png"><h3 title="/img/root.png">Heading</h3><p>Text /img/root.png</p><!-- /img/root.png --><img src="/img/root.png" alt="/img/root.png"><a href="/r/vale">Local link</a></div>"#;
		let paths = HashMap::from([("/img/root.png".to_string(), "files/assets/root.png".to_string())]);
		let rewritten = rewrite_archived_html(html, &paths);
		assert!(rewritten.contains(r#"data-note="/img/root.png""#));
		assert!(rewritten.contains(r#"title="/img/root.png""#));
		assert!(rewritten.contains("Text /img/root.png"));
		assert!(rewritten.contains("<!-- /img/root.png -->"));
		assert!(rewritten.contains(r#"src="files/assets/root.png" alt="/img/root.png""#));
		assert!(rewritten.contains(r#"href="https://www.reddit.com/r/vale""#));
	}

	#[tokio::test]
	async fn ambiguous_archived_comment_html_falls_back_to_escaped_markdown_and_records_an_issue() {
		let mut root = comment("ambiguous", "t3_post", 1, Value::String(String::new()));
		root["data"]["body"] = Value::String("# Raw <heading> & prose".to_string());
		root["data"]["body_html"] = Value::String(r#"<select><xmp><script>"use strict";</script></select>"#.to_string());
		let initial = json!([
			{"data": {"children": [{"data": {"num_comments": 1}}]}},
			{"data": {"children": [root]}}
		]);
		let raw_snapshot = initial.clone();

		let captured = capture_comments(&initial, "post", 1).await;
		assert_eq!(captured.comments.len(), 1);
		assert_eq!(captured.comments[0].body_html, r#"<div class="md"><p># Raw &lt;heading&gt; &amp; prose</p></div>"#);
		assert_eq!(captured.issues.len(), 1);
		assert_eq!(captured.issues[0].area, "comments");
		assert!(captured.issues[0].message.contains("Comment ambiguous contained ambiguous HTML"));
		assert!(!captured.comments[0].body_html.contains("<select"));
		assert_eq!(initial, raw_snapshot);
		assert!(initial.to_string().contains("<select><xmp><script>"));
	}

	#[test]
	fn archive_file_paths_cannot_escape_the_owned_directory() {
		assert_eq!(safe_archive_relative("files/assets/image.png"), Some(PathBuf::from("files/assets/image.png")));
		assert!(safe_archive_relative("../profiles.sqlite3").is_none());
		assert!(safe_archive_relative("/var/lib/vale/profiles.sqlite3").is_none());
		assert!(safe_archive_relative("files/./asset").is_none());
	}

	#[test]
	fn source_capture_rejects_non_public_ipv4_ranges() {
		assert!(!is_public_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
		assert!(!is_public_ipv4(Ipv4Addr::new(10, 0, 0, 2)));
		assert!(!is_public_ipv4(Ipv4Addr::new(100, 64, 1, 1)));
		assert!(!is_public_ipv4(Ipv4Addr::new(192, 168, 1, 10)));
		assert!(!is_public_ipv4(Ipv4Addr::new(169, 254, 169, 254)));
		assert!(is_public_ipv4(Ipv4Addr::new(93, 184, 216, 34)));
	}

	#[tokio::test]
	async fn source_capture_rejects_embedded_credentials_and_nonstandard_ports() {
		let credentials = Url::parse("https://reader:secret@example.com/").unwrap();
		assert_eq!(public_addresses(&credentials).await.unwrap_err(), "Source-page addresses cannot contain credentials.");
		let port = Url::parse("https://example.com:8443/").unwrap();
		assert_eq!(public_addresses(&port).await.unwrap_err(), "Source-page capture is restricted to standard web ports.");
	}

	#[test]
	fn source_sanitizers_remove_active_elements_and_handlers() {
		let html = r#"<script>alert(1)</script><form action="/logout"><button>go</button></form><img src="x" onerror="alert(2)"><a href="javascript:alert(3)">x</a><img src=javascript:alert(4)>"#;
		let html = ACTIVE_HTML.replace_all(html, "");
		let html = EVENT_ATTRIBUTE.replace_all(&html, "");
		let html = JAVASCRIPT_URL.replace_all(&html, "$1=\"#\"");
		assert!(!html.contains("<script"));
		assert!(!html.contains("<form"));
		assert!(!html.contains("onerror"));
		assert!(!html.contains("javascript:"));
	}

	#[test]
	fn archive_queue_reservation_has_a_hard_global_bound() {
		let pending = AtomicUsize::new(0);
		for _ in 0..MAX_PENDING_ARCHIVES {
			assert!(try_reserve_pending(&pending));
		}
		assert!(!try_reserve_pending(&pending));
		release_pending(&pending);
		assert!(try_reserve_pending(&pending));
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn archive_worker_runs_below_interactive_cpu_priority() {
		let (sender, receiver) = sync_channel(1);
		std::thread::spawn(move || {
			lower_archive_worker_priority().unwrap();
			unsafe extern "C" {
				fn getpriority(which: i32, who: u32) -> i32;
			}
			let priority = unsafe { getpriority(0, 0) };
			sender.send(priority).unwrap();
		});
		assert!(receiver.recv().unwrap() >= ARCHIVE_WORKER_NICE);
	}

	#[test]
	fn profile_quota_snapshot_and_admission_share_durable_reservations() {
		let mut connection = archive_database();
		connection
			.execute(
				"INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision) VALUES (7, 512, 1), (8, 256, 1)",
				[],
			)
			.unwrap();
		let first = admit_archive_record_in(&mut connection, 7, "first").unwrap();
		let ArchiveAdmission::Admitted {
			id: first_id,
			reservation_bytes: first_bytes,
			..
		} = first
		else {
			panic!("new capture should be admitted");
		};
		assert_eq!(first_bytes, 512 * MIB);
		let first_snapshot = quota_snapshot_in(&connection, 7).unwrap();
		assert_eq!(first_snapshot.profile_used_bytes, 0);
		assert_eq!(first_snapshot.profile_reserved_bytes, 512 * MIB);
		assert_eq!(first_snapshot.effective_limit_bytes, 512 * MIB);

		connection
			.execute("UPDATE profile_archive_settings SET archive_budget_mib = 256, revision = 2 WHERE profile_id = 7", [])
			.unwrap();
		let lowered = quota_snapshot_in(&connection, 7).unwrap();
		assert_eq!(lowered.profile_reserved_bytes, 512 * MIB);
		assert_eq!(lowered.effective_limit_bytes, 256 * MIB);
		assert!(matches!(admit_archive_record_in(&mut connection, 7, "blocked"), Err(ArchiveAdmissionError::Storage(_))));

		let second = admit_archive_record_in(&mut connection, 8, "other-profile").unwrap();
		assert!(matches!(
			second,
			ArchiveAdmission::Admitted {
				reservation_bytes,
				..
			} if reservation_bytes == 256 * MIB
		));
		let stored: i64 = connection
			.query_row("SELECT reserved_bytes FROM archive_reservations WHERE archive_id = ?1", params![first_id], |row| row.get(0))
			.unwrap();
		assert_eq!(stored as u64, first_bytes);
	}

	#[test]
	fn retry_admission_reuses_the_failed_record_and_is_atomic() {
		let mut connection = archive_database();
		connection
			.execute("INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision) VALUES (7, 256, 1)", [])
			.unwrap();
		insert_archive(&connection, "failed-id", 7, "failed", 1);
		let admitted = admit_archive_record_in(&mut connection, 7, "post-failed-id").unwrap();
		assert!(matches!(
			admitted,
			ArchiveAdmission::Admitted {
				ref id,
				reservation_bytes,
				..
			} if id == "failed-id" && reservation_bytes == 256 * MIB
		));
		let (status, reservations): (String, i64) = connection
			.query_row(
				"SELECT p.status, COUNT(r.archive_id) FROM post_archives p LEFT JOIN archive_reservations r ON r.archive_id = p.id WHERE p.id = 'failed-id' GROUP BY p.id",
				[],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert_eq!(status, "queued");
		assert_eq!(reservations, 1);
	}

	#[test]
	fn admission_honors_exact_floor_below_floor_and_shared_pool_exhaustion() {
		let mut exact = archive_database();
		exact
			.execute("INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision) VALUES (7, 256, 1)", [])
			.unwrap();
		insert_archive(&exact, "existing", 7, "ready", 1);
		exact
			.execute("UPDATE post_archives SET total_bytes = ?1 WHERE id = 'existing'", params![(192 * MIB) as i64])
			.unwrap();
		assert!(matches!(
			admit_archive_record_in(&mut exact, 7, "at-floor").unwrap(),
			ArchiveAdmission::Admitted {
				reservation_bytes,
				..
			} if reservation_bytes == MIN_CAPTURE_RESERVATION_BYTES
		));

		let mut below = archive_database();
		below
			.execute("INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision) VALUES (7, 256, 1)", [])
			.unwrap();
		insert_archive(&below, "existing", 7, "ready", 1);
		below
			.execute("UPDATE post_archives SET total_bytes = ?1 WHERE id = 'existing'", params![(192 * MIB + 1) as i64])
			.unwrap();
		assert!(matches!(admit_archive_record_in(&mut below, 7, "below-floor"), Err(ArchiveAdmissionError::Storage(_))));

		let mut shared = archive_database();
		insert_archive(&shared, "global", 8, "ready", 1);
		shared
			.execute(
				"UPDATE post_archives SET total_bytes = ?1 WHERE id = 'global'",
				params![(total_quota() - MIN_CAPTURE_RESERVATION_BYTES + 1) as i64],
			)
			.unwrap();
		assert!(matches!(
			admit_archive_record_in(&mut shared, 7, "global-blocked"),
			Err(ArchiveAdmissionError::Storage(message)) if message.contains("shared Vale archive pool")
		));
	}

	#[test]
	fn concurrent_admissions_and_retries_cannot_duplicate_reservations() {
		use std::sync::{Arc, Barrier};

		let path = std::env::temp_dir().join(format!("vale-archive-admission-test-{}.sqlite3", uuid::Uuid::new_v4()));
		let connection = Connection::open(&path).unwrap();
		initialize_archive_database(&connection);
		connection
			.execute("INSERT INTO profile_archive_settings (profile_id, archive_budget_mib, revision) VALUES (7, 256, 1)", [])
			.unwrap();
		drop(connection);
		let barrier = Arc::new(Barrier::new(2));
		let handles = ["one", "two"].map(|post_id| {
			let path = path.clone();
			let barrier = barrier.clone();
			std::thread::spawn(move || {
				let mut connection = Connection::open(path).unwrap();
				connection.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
				barrier.wait();
				admit_archive_record_in(&mut connection, 7, post_id)
			})
		});
		let results = handles.map(|handle| handle.join().unwrap());
		assert_eq!(results.iter().filter(|result| matches!(result, Ok(ArchiveAdmission::Admitted { .. }))).count(), 1);
		assert_eq!(results.iter().filter(|result| matches!(result, Err(ArchiveAdmissionError::Storage(_)))).count(), 1);
		let connection = Connection::open(&path).unwrap();
		assert_eq!(
			connection.query_row("SELECT COUNT(*) FROM archive_reservations", [], |row| row.get::<_, i64>(0)).unwrap(),
			1
		);

		connection.execute("DELETE FROM archive_reservations", []).unwrap();
		connection.execute("DELETE FROM post_archives", []).unwrap();
		insert_archive(&connection, "failed-id", 7, "failed", 1);
		drop(connection);
		let barrier = Arc::new(Barrier::new(2));
		let handles = [0, 1].map(|_| {
			let path = path.clone();
			let barrier = barrier.clone();
			std::thread::spawn(move || {
				let mut connection = Connection::open(path).unwrap();
				connection.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
				barrier.wait();
				admit_archive_record_in(&mut connection, 7, "post-failed-id")
			})
		});
		let results = handles.map(|handle| handle.join().unwrap());
		assert_eq!(results.iter().filter(|result| matches!(result, Ok(ArchiveAdmission::Admitted { .. }))).count(), 1);
		assert_eq!(results.iter().filter(|result| matches!(result, Ok(ArchiveAdmission::Existing { .. }))).count(), 1);
		let connection = Connection::open(&path).unwrap();
		assert_eq!(
			connection.query_row("SELECT COUNT(*) FROM archive_reservations", [], |row| row.get::<_, i64>(0)).unwrap(),
			1
		);
		drop(connection);
		std::fs::remove_file(&path).unwrap();
	}

	#[test]
	fn archive_capacity_prunes_old_failed_rows_without_cross_profile_deletion() {
		let connection = archive_database();
		insert_archive(&connection, "failed-old", 7, "failed", 1);
		insert_archive(&connection, "ready", 7, "ready", 2);
		insert_archive(&connection, "failed-new", 7, "failed", 3);
		insert_archive(&connection, "other-profile", 8, "failed", 4);

		let pruned = ensure_archive_record_capacity_with_limits(&connection, 7, 3, 10).unwrap();
		assert_eq!(pruned, vec![String::from("failed-old")]);
		assert_eq!(archive_record_count(&connection, Some(7)).unwrap(), 2);
		assert_eq!(archive_record_count(&connection, Some(8)).unwrap(), 1);
		insert_archive(&connection, "new", 7, "queued", 5);
		assert_eq!(archive_record_count(&connection, Some(7)).unwrap(), 3);
	}

	#[test]
	fn archive_capacity_prunes_profile_failed_rows_for_global_bound() {
		let connection = archive_database();
		insert_archive(&connection, "profile-failed", 7, "failed", 1);
		insert_archive(&connection, "other-ready", 8, "ready", 2);
		insert_archive(&connection, "other-partial", 8, "partial", 3);

		let pruned = ensure_archive_record_capacity_with_limits(&connection, 7, 10, 3).unwrap();
		assert_eq!(pruned, vec![String::from("profile-failed")]);
		assert_eq!(archive_record_count(&connection, None).unwrap(), 2);
		assert_eq!(archive_record_count(&connection, Some(8)).unwrap(), 2);
	}

	#[test]
	fn archive_capacity_rejects_profile_cap_without_failed_rows() {
		let connection = archive_database();
		insert_archive(&connection, "ready-old", 7, "ready", 1);
		insert_archive(&connection, "partial", 7, "partial", 2);
		insert_archive(&connection, "queued", 7, "queued", 3);

		let error = ensure_archive_record_capacity_with_limits(&connection, 7, 3, 10).unwrap_err();
		assert!(matches!(error, ArchiveCapacityError::Limit(message) if message.contains("profile has reached the 3 saved-post record limit")));
		assert_eq!(archive_record_count(&connection, Some(7)).unwrap(), 3);
	}

	#[test]
	fn archive_capacity_rejects_global_cap_when_only_other_profile_has_records() {
		let connection = archive_database();
		insert_archive(&connection, "requester-ready", 7, "ready", 1);
		insert_archive(&connection, "other-ready", 8, "ready", 2);
		insert_archive(&connection, "other-partial", 8, "partial", 3);

		let error = ensure_archive_record_capacity_with_limits(&connection, 7, 10, 3).unwrap_err();
		assert!(matches!(error, ArchiveCapacityError::Limit(message) if message.contains("Vale instance has reached the 3 saved-post record limit")));
		assert_eq!(archive_record_count(&connection, None).unwrap(), 3);
		assert_eq!(archive_record_count(&connection, Some(8)).unwrap(), 2);
	}

	#[test]
	fn failed_archive_bytes_do_not_consume_storage_quota() {
		let connection = archive_database();
		insert_archive(&connection, "failed", 7, "failed", 1);
		insert_archive(&connection, "ready", 7, "ready", 2);
		insert_archive(&connection, "partial", 7, "partial", 3);
		insert_archive(&connection, "capturing", 7, "capturing", 4);
		connection
			.execute(
				"UPDATE post_archives SET total_bytes = CASE id WHEN 'failed' THEN 10_000 WHEN 'ready' THEN 20 WHEN 'partial' THEN 30 WHEN 'capturing' THEN 40 END",
				[],
			)
			.unwrap();

		assert_eq!(actual_archive_bytes_in(&connection, None).unwrap(), 90);
	}

	#[test]
	fn archive_listing_keeps_actionable_failures_and_has_a_hard_bound() {
		let connection = archive_database();
		for index in 0..=MAX_ARCHIVE_LIST_ENTRIES {
			insert_archive(&connection, &format!("ready-{index}"), 7, "ready", index);
		}
		insert_archive(&connection, "failed", 7, "failed", MAX_ARCHIVE_LIST_ENTRIES + 1);

		let entries = visible_entries_for_profile(&connection, 7, usize::MAX).unwrap();
		assert_eq!(entries.len(), MAX_ARCHIVE_LIST_ENTRIES as usize);
		assert_eq!(entries.first().map(|entry| entry.id.as_str()), Some("failed"));
		assert!(entries.iter().any(|entry| entry.status == "failed"));
		assert_eq!(bounded_archive_list_limit(usize::MAX), MAX_ARCHIVE_LIST_ENTRIES);
	}
}

#[cfg(test)]
mod surface_fixture_tests {
	use super::*;
	fn entry() -> ArchiveEntryView {
		ArchiveEntryView {
			id: "review-save".into(),
			post_id: "post0".into(),
			permalink: "/r/woodworking/comments/post0/discussion/".into(),
			title: "A small workshop, one good workbench".into(),
			community: "woodworking".into(),
			source_url: String::new(),
			status: "ready".into(),
			status_label: "Saved".into(),
			created: "Today".into(),
			captured: "Today".into(),
			comment_count: 24,
			asset_count: 0,
			generated_asset_count: 4,
			local_file_count: 4,
			total_bytes: 524288,
			total_size: "512 KiB".into(),
			issues: Vec::new(),
			error: String::new(),
		}
	}
	#[test]
	fn saved_surface_fixtures() {
		for theme in ["dark", "light"] {
			let saved = SavedTemplate {
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/saved".into(),
				entries: vec![entry()],
				quota: ArchiveQuotaSnapshot {
					used_size: "512 KiB".into(),
					reserved_size: "0 B".into(),
					effective_limit_size: "2 GiB".into(),
					instance_limit_size: "2 GiB".into(),
					..ArchiveQuotaSnapshot::default()
				},
			}
			.render()
			.unwrap();
			assert!(saved.contains("A small workshop, one good workbench"));
			crate::reading_fixtures::export(theme, "saved.html", &saved);
			let detail = SavedDetailTemplate {
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/saved/review-save".into(),
				entry: entry(),
			}
			.render()
			.unwrap();
			assert!(detail.contains("Read local snapshot"));
			crate::reading_fixtures::export(theme, "saved-detail.html", &detail);
		}
	}
}

/// Explicit indexing of a published, owned archive. Never fetches Reddit.
pub async fn index_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&request).map(|c| c.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in to index an archive."));
	};
	let id = request.param("archive_id").unwrap_or_default();
	if !account::valid_post_id(&id) {
		return Ok(plain_response(StatusCode::NOT_FOUND, "Archive not found."));
	}
	let Some(entry) = entry_for_profile(profile, &id)? else {
		return Ok(plain_response(StatusCode::NOT_FOUND, "Archive not found."));
	};
	if !entry.is_viewable() {
		return Ok(plain_response(StatusCode::CONFLICT, "Wait for the archive capture to finish."));
	}
	let path = archive_directory(profile, &id).join("manifest.json");
	let result = tokio::task::spawn_blocking(move || -> Result<usize, String> {
		use std::io::Read;
		let file = std::fs::File::open(path).map_err(|_| "Archive manifest is unavailable.")?;
		let mut bytes = Vec::new();
		file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| "Archive could not be read.")?;
		if bytes.len() > 128 * 1024 * 1024 {
			return Err("Archive exceeds the search indexing size limit; its reader remains available.".into());
		}
		let manifest: ArchiveManifest = serde_json::from_slice(&bytes).map_err(|_| "Archive manifest is invalid.")?;
		crate::library::index_archive(&mut account::open_database()?, profile, &id, &manifest).map_err(|_| "Unable to index this archive within the search limits.".into())
	})
	.await
	.map_err(|_| "Indexing stopped unexpectedly.")?;
	match result {
		Ok(_) => Ok(
			Response::builder()
				.status(StatusCode::SEE_OTHER)
				.header("location", "/reading/library")
				.header("cache-control", "private, no-store")
				.body(Body::empty())
				.unwrap(),
		),
		Err(message) => Ok(plain_response(StatusCode::UNPROCESSABLE_ENTITY, &message)),
	}
}
