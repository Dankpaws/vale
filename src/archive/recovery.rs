//! Startup reconciliation between archive records and files on disk.

use super::*;

pub(super) fn sync_directory_blocking(path: &Path) -> Result<(), String> {
	std::fs::File::open(path)
		.and_then(|file| file.sync_all())
		.map_err(|error| format!("Unable to make an archive directory change durable: {error}"))
}

pub(super) fn set_cleanup_failure(connection: &rusqlite::Connection, id: &str, profile_id: i64, message: &str, total_bytes: Option<u64>) -> Result<(), String> {
	connection
		.execute(
			"UPDATE post_archives SET status = 'cleanup_failed', total_bytes = COALESCE(?1, total_bytes), updated_at = ?2, error = ?3 WHERE id = ?4 AND profile_id = ?5",
			params![total_bytes.map(|value| value as i64), account::now(), bounded_error(message), id, profile_id],
		)
		.map_err(|error| format!("Unable to retain failed archive cleanup accounting: {error}"))?;
	Ok(())
}

pub(super) fn set_deletion_failure(connection: &rusqlite::Connection, id: &str, profile_id: i64, message: &str) -> Result<(), String> {
	connection
		.execute(
			"UPDATE post_archives SET status = 'deleting', updated_at = ?1, error = ?2 WHERE id = ?3 AND profile_id = ?4",
			params![account::now(), bounded_error(message), id, profile_id],
		)
		.map_err(|error| format!("Unable to retain interrupted archive deletion accounting: {error}"))?;
	Ok(())
}

pub(super) fn reconcile_published_archive(connection: &mut rusqlite::Connection, id: &str, profile_id: i64, current_status: &str, directory: &Path) -> Result<(), String> {
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

pub(super) fn remove_archive_directory_blocking(path: &Path, profile_directory: &Path) -> Result<(), String> {
	if path.exists() {
		std::fs::remove_dir_all(path).map_err(|error| format!("Unable to remove {}: {error}", path.display()))?;
	}
	if profile_directory.exists() {
		sync_directory_blocking(profile_directory)?;
	}
	Ok(())
}

pub(super) fn reconcile_database_archives(connection: &mut rusqlite::Connection, root: &Path) -> Result<(), String> {
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

pub(super) fn insert_orphan_record(connection: &rusqlite::Connection, profile_id: i64, archive_id: &str, bytes: u64, partial: bool) -> Result<(), String> {
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

pub(super) fn reconcile_orphan_archives(connection: &mut rusqlite::Connection, root: &Path) -> Result<(), String> {
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

pub(super) fn ensure_recovered_reservation(connection: &mut rusqlite::Connection, profile_id: i64, archive_id: &str) -> Result<Option<u64>, String> {
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
