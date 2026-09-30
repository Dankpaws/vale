//! The background capture worker, its queue, and job completion.

use super::*;

pub(super) fn try_reserve_pending(counter: &AtomicUsize) -> bool {
	counter
		.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| (pending < MAX_PENDING_ARCHIVES).then_some(pending + 1))
		.is_ok()
}

pub(super) fn release_pending(counter: &AtomicUsize) {
	let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| pending.checked_sub(1));
}

pub(super) struct ArchiveReservation;

impl Drop for ArchiveReservation {
	fn drop(&mut self) {
		release_pending(&PENDING_ARCHIVES);
	}
}

pub(super) fn reserve_archive_job() -> Option<ArchiveReservation> {
	try_reserve_pending(&PENDING_ARCHIVES).then_some(ArchiveReservation)
}

pub(super) struct QueuedArchiveJob {
	pub(super) job: ArchiveJob,
	pub(super) reservation: ArchiveReservation,
}

#[cfg(target_os = "linux")]
pub(super) fn lower_archive_worker_priority() -> Result<(), String> {
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
pub(super) fn lower_archive_worker_priority() -> Result<(), String> {
	Ok(())
}

pub(super) fn archive_worker_loop(receiver: Receiver<QueuedArchiveJob>, ready: SyncSender<Result<(), String>>) {
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

pub(super) fn start_archive_worker() -> Result<SyncSender<QueuedArchiveJob>, String> {
	let (sender, receiver) = sync_channel(MAX_PENDING_ARCHIVES);
	let (ready_sender, ready_receiver) = sync_channel(1);
	std::thread::Builder::new()
		.name("vale-archive".to_string())
		.spawn(move || archive_worker_loop(receiver, ready_sender))
		.map_err(|error| format!("Unable to start the isolated archive worker: {error}"))?;
	ready_receiver.recv().map_err(|_| "The isolated archive worker stopped during startup.".to_string())??;
	Ok(sender)
}

pub(super) fn archive_worker_sender() -> Result<&'static SyncSender<QueuedArchiveJob>, String> {
	ARCHIVE_WORKER.as_ref().map_err(Clone::clone)
}

pub(super) fn spawn_job(job: ArchiveJob, reservation: ArchiveReservation) -> Result<(), String> {
	let queued = QueuedArchiveJob { job, reservation };
	match archive_worker_sender()?.try_send(queued) {
		Ok(()) => Ok(()),
		Err(TrySendError::Full(_)) => Err("The isolated archive queue reached its concurrency boundary.".to_string()),
		Err(TrySendError::Disconnected(_)) => Err("The isolated archive worker stopped before this capture began.".to_string()),
	}
}

pub(super) fn bounded_error(message: &str) -> String {
	message.chars().take(500).collect()
}

pub(super) fn directory_usage(path: &Path) -> io::Result<(u64, u64)> {
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

pub(super) async fn finalize_published_job(job: &ArchiveJob, failure: &str) -> Result<(), String> {
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

pub(super) async fn retain_cleanup_reservation(job: &ArchiveJob, message: &str) {
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

pub(super) async fn fail_job_after_cleanup(job: &ArchiveJob, message: &str) {
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
