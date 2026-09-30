//! Saved-post archives (permanent copies): shared types, paths, and quota
//! configuration, plus submodules for archive records and quotas (`records`),
//! admission and deletion (`admission`), startup reconciliation
//! (`recovery`), the capture worker (`worker`), comment capture
//! (`comments`), post and media capture (`capture`), external page capture
//! (`external`), and serving archived files (`serve`).

mod admission;
mod capture;
mod comments;
mod external;
mod records;
mod recovery;
mod serve;
#[cfg(test)]
mod tests;
mod worker;

pub use admission::*;
use capture::*;
use comments::*;
pub(crate) use external::*;
pub use records::*;
use recovery::*;
pub use serve::*;
use worker::*;

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
		include_bytes!("../../static/fonts/SourceSans3VF-Upright.ttf.woff2"),
	),
	(
		"files/reader/source-serif-4.woff2",
		"font/woff2",
		include_bytes!("../../static/fonts/SourceSerif4-Regular.ttf.woff2"),
	),
	("files/reader/vale-mark.svg", "image/svg+xml", include_bytes!("../../static/vale-mark-flat.svg")),
	("files/reader/OFL.txt", "text/plain; charset=utf-8", include_bytes!("../../static/fonts/OFL.txt")),
	("files/reader/AGPL-3.0.txt", "text/plain; charset=utf-8", include_bytes!("../../LICENSE")),
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
		css: include_str!("../../static/archive.css"),
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
