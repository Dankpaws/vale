//! Archive records, listings, and quota accounting.

use super::*;

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

pub(super) fn status_label(status: &str) -> String {
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

pub(super) fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveEntryView> {
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

pub(super) const ENTRY_COLUMNS: &str =
	"id, post_id, permalink, title, community, source_url, status, created_at, captured_at, comment_count, asset_count, generated_asset_count, total_bytes, issues_json, error";

pub(super) fn entry_for_profile(profile_id: i64, archive_id: &str) -> Result<Option<ArchiveEntryView>, String> {
	account::open_database()?
		.query_row(
			&format!("SELECT {ENTRY_COLUMNS} FROM post_archives WHERE profile_id = ?1 AND id = ?2"),
			params![profile_id, archive_id],
			row_to_entry,
		)
		.optional()
		.map_err(|error| format!("Unable to read the saved Vale post: {error}"))
}

pub(crate) fn entry_for_post(connection: &rusqlite::Connection, profile_id: i64, post_id: &str) -> rusqlite::Result<Option<ArchiveEntryView>> {
	connection
		.query_row(
			&format!("SELECT {ENTRY_COLUMNS} FROM post_archives WHERE profile_id = ?1 AND post_id = ?2"),
			params![profile_id, post_id],
			row_to_entry,
		)
		.optional()
}

pub(super) fn archive_record_count(connection: &rusqlite::Connection, profile_id: Option<i64>) -> Result<i64, String> {
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
pub(super) fn prune_failed_records(connection: &rusqlite::Connection, profile_id: i64, limit: i64) -> Result<Vec<String>, String> {
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
pub(super) enum ArchiveCapacityError {
	Limit(String),
	Database(String),
}

pub(super) fn ensure_archive_record_capacity_with_limits(
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

pub(super) fn ensure_archive_record_capacity(connection: &rusqlite::Connection, profile_id: i64) -> Result<Vec<String>, ArchiveCapacityError> {
	ensure_archive_record_capacity_with_limits(connection, profile_id, MAX_ARCHIVE_RECORDS_PER_PROFILE, MAX_ARCHIVE_RECORDS_GLOBAL)
}

pub(super) async fn remove_pruned_archive_files(profile_id: i64, archive_ids: &[String]) {
	for archive_id in archive_ids {
		let _ = fs::remove_dir_all(archive_directory(profile_id, archive_id)).await;
		let _ = fs::remove_dir_all(partial_directory(profile_id, archive_id)).await;
	}
}

pub(super) fn bounded_archive_list_limit(limit: usize) -> i64 {
	limit.clamp(1, MAX_ARCHIVE_LIST_ENTRIES as usize) as i64
}

pub(super) fn visible_entries_for_profile(connection: &rusqlite::Connection, profile_id: i64, limit: usize) -> Result<Vec<ArchiveEntryView>, String> {
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

pub(super) fn summed_bytes(connection: &rusqlite::Connection, sql: &str, profile_id: Option<i64>) -> Result<u64, String> {
	let value = match profile_id {
		Some(profile_id) => connection.query_row(sql, params![profile_id], |row| row.get::<_, i64>(0)),
		None => connection.query_row(sql, [], |row| row.get::<_, i64>(0)),
	};
	value
		.map(|value| value.max(0) as u64)
		.map_err(|error| format!("Unable to measure the Vale archive library: {error}"))
}

pub(super) fn actual_archive_bytes_in(connection: &rusqlite::Connection, profile_id: Option<i64>) -> Result<u64, String> {
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

pub(super) fn reserved_archive_bytes_in(connection: &rusqlite::Connection, profile_id: Option<i64>) -> Result<u64, String> {
	match profile_id {
		Some(_) => summed_bytes(
			connection,
			"SELECT COALESCE(SUM(reserved_bytes), 0) FROM archive_reservations WHERE profile_id = ?1",
			profile_id,
		),
		None => summed_bytes(connection, "SELECT COALESCE(SUM(reserved_bytes), 0) FROM archive_reservations", None),
	}
}

pub(super) fn configured_profile_budget_mib(connection: &rusqlite::Connection, profile_id: i64) -> Result<u64, String> {
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

pub(super) fn quota_snapshot_in(connection: &rusqlite::Connection, profile_id: i64) -> Result<ArchiveQuotaSnapshot, String> {
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
	quota_snapshot_in(&*account::open_database()?, profile_id)
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
