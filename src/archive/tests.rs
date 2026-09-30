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
