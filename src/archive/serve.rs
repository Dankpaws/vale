//! Serving archived files and indexing published archives.

use super::*;

pub(super) fn safe_archive_relative(value: &str) -> Option<PathBuf> {
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

pub(super) fn archived_content_type(path: &Path) -> &'static str {
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

pub(super) async fn serve_archive_file(request: Request<Body>, relative: &str) -> Result<Response<Body>, String> {
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
		crate::library::index_archive(&mut *account::open_database()?, profile, &id, &manifest).map_err(|_| "Unable to index this archive within the search limits.".into())
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
