//! Capturing a post and its Reddit media into an archive.

use super::*;

pub(super) fn content_type_extension(content_type: &str, source: &str) -> &'static str {
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

pub(super) fn is_local_media_path(source: &str) -> bool {
	["/img/", "/preview/", "/vid/", "/hls/", "/thumb/", "/emoji/", "/emote/", "/style/", "/static/"]
		.iter()
		.any(|prefix| source.starts_with(prefix))
}

pub(super) async fn sha256_file(path: &Path) -> Result<String, String> {
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
	pub(super) fn remaining(&self) -> u64 {
		self.item_limit.saturating_sub(self.total_bytes)
	}

	pub(super) fn remaining_for_assets(&self) -> u64 {
		self.remaining().saturating_sub(FINALIZATION_RESERVE_BYTES)
	}

	pub(super) fn push_issue(&mut self, area: &str, message: impl Into<String>) {
		self.issues.push(issue(area, message));
	}

	pub(super) async fn store_response(&mut self, source_key: &str, original_url: &str, response: wreq::Response, maximum: u64) -> Result<String, String> {
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

	pub(super) async fn capture_reddit_asset(&mut self, source: &str, maximum: u64) -> Result<String, String> {
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

	pub(super) async fn capture_reddit_video(&mut self, hls_source: &str) -> Result<String, String> {
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

pub(super) async fn sync_directory(path: &Path) -> Result<(), String> {
	let path = path.to_path_buf();
	tokio::task::spawn_blocking(move || -> io::Result<()> { std::fs::File::open(path)?.sync_all() })
		.await
		.map_err(|_| "The archive directory durability check stopped unexpectedly.".to_string())?
		.map_err(|error| format!("Unable to make the archive directory durable: {error}"))
}

pub(super) async fn run_job(job: &ArchiveJob) -> Result<(), String> {
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
