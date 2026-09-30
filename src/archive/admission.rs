//! Admitting, saving, deleting, and resuming archive captures.

use super::*;

pub(super) enum ArchiveAdmission {
	Existing { id: String },
	Admitted { id: String, reservation_bytes: u64, pruned: Vec<String> },
}

#[derive(Debug)]
pub(super) enum ArchiveAdmissionError {
	Storage(String),
	Database(String),
}

/// Reserve one durable archive row while holding SQLite's write lock. Keeping
/// this synchronous prevents a non-Send rusqlite transaction from living
/// across the request future's filesystem awaits.
pub(super) fn admit_archive_record(profile_id: i64, post_id: &str) -> Result<ArchiveAdmission, ArchiveAdmissionError> {
	let mut connection = account::open_database().map_err(ArchiveAdmissionError::Database)?;
	admit_archive_record_in(&mut connection, profile_id, post_id)
}

pub(super) fn admit_archive_record_in(connection: &mut rusqlite::Connection, profile_id: i64, post_id: &str) -> Result<ArchiveAdmission, ArchiveAdmissionError> {
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

pub(super) async fn cleanup_failed_capture(profile_id: i64, archive_id: &str) -> Result<(), String> {
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
