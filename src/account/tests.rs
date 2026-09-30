use super::*;
use route_recognizer::Params;
use std::{fs, path::Path};

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
fn wal_is_enabled_explicitly_once_while_connection_safety_stays_local() {
	let path = temporary_database();
	let connection = open_database_at(&path).unwrap();
	let initial: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
	let foreign_keys: i64 = connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
	let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
	assert_eq!(initial, "delete");
	assert_eq!(foreign_keys, 1);
	assert_eq!(synchronous, 2);
	enable_wal(&connection).unwrap();
	let enabled: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
	assert_eq!(enabled, "wal");
	drop(connection);
	remove_database(&path);
}

#[test]
fn single_post_hidden_lookup_uses_the_profile_primary_key() {
	let connection = Connection::open_in_memory().unwrap();
	initialize_schema(&connection).unwrap();
	connection
		.execute(
			"INSERT INTO profiles (id, label, preferences_json, created_at, updated_at) VALUES (7, 'Reader', '{}', 1, 1)",
			[],
		)
		.unwrap();
	connection
		.execute("INSERT INTO hidden_posts (profile_id, post_id, hidden_at) VALUES (7, 'hidden', 1)", [])
		.unwrap();
	assert!(post_is_hidden_in(&connection, 7, "hidden").unwrap());
	assert!(!post_is_hidden_in(&connection, 7, "visible").unwrap());
	assert!(!post_is_hidden_in(&connection, 8, "hidden").unwrap());
}

#[test]
fn validates_account_credentials() {
	assert_eq!(validate_username("Reader.One").unwrap(), "reader.one");
	assert!(validate_username("two words").is_err());
	assert!(validate_password("a sufficiently long password", "a sufficiently long password").is_ok());
	assert!(validate_password("short", "short").is_err());
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
fn reading_forms_keep_twelve_origin_boundaries() {
	for (origin, host, expected) in [
		("http://127.0.0.1:3117", "127.0.0.1:3117", true),
		("http://localhost:3117", "localhost:3117", true),
		("http://127.0.0.1:3118", "127.0.0.1:3117", false),
		("http://localhost:3117", "127.0.0.1:3117", false),
		("https://evil.example", "127.0.0.1:3117", false),
		("null", "127.0.0.1:3117", false),
		("", "127.0.0.1:3117", false),
		("file:///tmp/x", "127.0.0.1:3117", false),
		("https://vale.example.com", "127.0.0.1:3117", true),
		("https://vale.example.com.evil.test", "127.0.0.1:3117", false),
		("https://vale.example.com:444", "127.0.0.1:3117", false),
		("http://vale.example.com", "127.0.0.1:3117", false),
	] {
		let request = post_request(origin, host);
		assert_eq!(mutation_is_same_origin_with_expected(&request, Some("https://vale.example.com")), expected, "{origin}");
	}
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
fn form_mutations_without_origin_follow_fetch_metadata() {
	let post = |fetch_site: Option<&str>| {
		let mut builder = Request::builder().method(Method::POST).uri("/feeds").header("host", "127.0.0.1:3101");
		if let Some(fetch_site) = fetch_site {
			builder = builder.header("sec-fetch-site", fetch_site);
		}
		builder.body(Body::empty()).unwrap()
	};
	assert!(mutation_is_same_origin_with_expected(&post(None), None));
	assert!(mutation_is_same_origin_with_expected(&post(Some("same-origin")), None));
	assert!(mutation_is_same_origin_with_expected(&post(Some("none")), None));
	assert!(!mutation_is_same_origin_with_expected(&post(Some("same-site")), None));
	assert!(!mutation_is_same_origin_with_expected(&post(Some("cross-site")), None));
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

#[cfg(test)]
mod surface_fixture_tests {
	use super::*;
	#[test]
	fn account_surface_fixtures() {
		for theme in ["dark", "light"] {
			let history = HistoryTemplate {
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/history".into(),
				entries: vec![HistoryEntry {
					title: "A small workshop, one good workbench".into(),
					community: "woodworking".into(),
					permalink: "/r/woodworking/comments/post0/discussion/".into(),
					viewed: "Today".into(),
					view_count: 2,
				}],
			}
			.render()
			.unwrap();
			assert!(history.contains("Reading history"));
			crate::reading_fixtures::export(theme, "history.html", &history);
			let login = LoginTemplate {
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/login".into(),
				next: "/".into(),
				error: String::new(),
			}
			.render()
			.unwrap();
			crate::reading_fixtures::export(theme, "login.html", &login);
			let setup = SetupTemplate {
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/setup".into(),
				feed_groups: crate::reading_fixtures::feeds(),
				subscription_count: 3,
				error: String::new(),
			}
			.render()
			.unwrap();
			crate::reading_fixtures::export(theme, "setup.html", &setup);
		}
	}
}

#[cfg(test)]
#[test]
#[ignore = "Explicit local copied-state migration rehearsal; never targets live state"]
fn copied_state_schema_rehearsal() {
	let source = std::env::var_os("VALE_MIGRATION_FIXTURE").expect("Set VALE_MIGRATION_FIXTURE to a consistent, standalone database backup");
	struct RehearsalDirectory(std::path::PathBuf);
	impl Drop for RehearsalDirectory {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}
	let candidate = RehearsalDirectory(std::env::temp_dir().join(format!("vale-migration-{}", uuid::Uuid::new_v4())));
	std::fs::create_dir(&candidate.0).unwrap();
	let path = candidate.0.join("candidate.sqlite3");
	std::fs::copy(source, &path).unwrap();
	let db = Connection::open(path).unwrap();
	initialize_schema(&db).unwrap();
	initialize_schema(&db).unwrap();
	assert_eq!(db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)).unwrap(), "ok");
	assert!(db.prepare("PRAGMA foreign_key_check").unwrap().query([]).unwrap().next().unwrap().is_none());
}
