use crate::{
	client::{CLIENT, OAUTH_CLIENT},
	utils::{read_body_limited, safe_download_filename},
};
use futures_lite::StreamExt;
use hyper::{header, Body, Request, Response, StatusCode};
use sha2::{Digest, Sha256};
use std::{
	collections::HashSet,
	env,
	io::{Cursor, Write},
	path::{Path, PathBuf},
	sync::LazyLock,
	time::SystemTime,
};
use tokio::{
	fs,
	process::Command,
	sync::Semaphore,
	time::{timeout, Duration},
};
use tokio_util::io::ReaderStream;
use url::Url;
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

const MAX_FORM_BYTES: usize = 128 * 1024;
const FORM_TOO_LARGE: &str = "That download request is too large.";
const MAX_GALLERY_ITEMS: usize = 20;
const MAX_GALLERY_ITEM_BYTES: usize = 64 * 1024 * 1024;
const MAX_GALLERY_ARCHIVE_BYTES: usize = 192 * 1024 * 1024;
const MAX_VIDEO_DOWNLOAD_BYTES: u64 = 768 * 1024 * 1024;
const VIDEO_CACHE_TARGET_BYTES: u64 = 768 * 1024 * 1024;
const VIDEO_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const VIDEO_REMUX_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MEDIA_TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

static VIDEO_REMUX_SLOT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));

fn download_error(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(Body::from(message.to_string()))
		.unwrap_or_default()
}

fn attachment_response(body: Body, content_type: &str, filename: &str, length: u64) -> Response<Body> {
	let filename = safe_download_filename(filename, "vale-download");
	Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, content_type)
		.header(header::CONTENT_LENGTH, length)
		.header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(body)
		.unwrap_or_default()
}

async fn read_form(mut request: Request<Body>) -> Result<Vec<(String, String)>, (StatusCode, &'static str)> {
	let bytes = read_body_limited(request.body_mut(), MAX_FORM_BYTES, FORM_TOO_LARGE).await.map_err(|message| {
		if message == FORM_TOO_LARGE {
			(StatusCode::PAYLOAD_TOO_LARGE, FORM_TOO_LARGE)
		} else {
			(StatusCode::BAD_REQUEST, "Vale could not read that download request.")
		}
	})?;
	Ok(url::form_urlencoded::parse(&bytes).into_owned().collect())
}

fn form_value<'a>(form: &'a [(String, String)], key: &str) -> &'a str {
	form.iter().find_map(|(name, value)| (name == key).then_some(value.as_str())).unwrap_or_default()
}

fn local_media_url(source: &str) -> Result<Url, String> {
	if !source.starts_with('/') || source.starts_with("//") {
		return Err("Only same-origin Vale media can be downloaded.".to_string());
	}
	Url::parse(&format!("https://vale.invalid{source}")).map_err(|_| "That media address is invalid.".to_string())
}

pub fn upstream_media_url(source: &str) -> Result<String, String> {
	let parsed = local_media_url(source)?;
	let path = parsed.path();
	let safe_path = |value: &str| !value.is_empty() && !value.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..");
	let upstream = if let Some(rest) = path.strip_prefix("/img/") {
		format!("https://i.redd.it/{rest}")
	} else if let Some(rest) = path.strip_prefix("/preview/pre/") {
		format!("https://preview.redd.it/{rest}")
	} else if let Some(rest) = path.strip_prefix("/preview/external-pre/") {
		format!("https://external-preview.redd.it/{rest}")
	} else if let Some(rest) = path.strip_prefix("/hls/") {
		format!("https://v.redd.it/{rest}")
	} else if let Some(rest) = path.strip_prefix("/vid/") {
		let (id, size) = rest.split_once('/').ok_or_else(|| "That video address is invalid.".to_string())?;
		if id.is_empty() || size.is_empty() || size.contains('/') {
			return Err("That video address is invalid.".to_string());
		}
		format!("https://v.redd.it/{id}/DASH_{size}")
	} else if let Some(rest) = path.strip_prefix("/thumb/") {
		let (point, id) = rest.split_once('/').ok_or_else(|| "That thumbnail address is invalid.".to_string())?;
		if !matches!(point, "a" | "b") || !safe_path(id) {
			return Err("That thumbnail address is invalid.".to_string());
		}
		format!("https://{point}.thumbs.redditmedia.com/{id}")
	} else if let Some(rest) = path.strip_prefix("/emoji/") {
		if !safe_path(rest) {
			return Err("That emoji address is invalid.".to_string());
		}
		format!("https://emoji.redditmedia.com/{rest}")
	} else if let Some(rest) = path.strip_prefix("/emote/") {
		if !safe_path(rest) {
			return Err("That emote address is invalid.".to_string());
		}
		format!("https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/{rest}")
	} else if let Some(rest) = path.strip_prefix("/style/") {
		if !safe_path(rest) {
			return Err("That style asset address is invalid.".to_string());
		}
		format!("https://styles.redditmedia.com/{rest}")
	} else if let Some(rest) = path.strip_prefix("/static/") {
		if !safe_path(rest) {
			return Err("That static asset address is invalid.".to_string());
		}
		format!("https://www.redditstatic.com/{rest}")
	} else {
		return Err("That is not a downloadable Vale media address.".to_string());
	};

	let query = parsed
		.query_pairs()
		.filter(|(key, _)| key != "download")
		.fold(url::form_urlencoded::Serializer::new(String::new()), |mut serializer, (key, value)| {
			serializer.append_pair(&key, &value);
			serializer
		})
		.finish();
	Ok(if query.is_empty() { upstream } else { format!("{upstream}?{query}") })
}

async fn fetch_media(source: &str, maximum: usize) -> Result<Vec<u8>, String> {
	let upstream = upstream_media_url(source)?;
	let uri = wreq::Uri::try_from(upstream).map_err(|_| "That media address is invalid.".to_string())?;
	let client = OAUTH_CLIENT.load_full();
	let request = CLIENT.get(uri).header("User-Agent", client.user_agent()).header("Accept", "*/*").send();
	let response = timeout(MEDIA_TRANSFER_IDLE_TIMEOUT, request)
		.await
		.map_err(|_| "Reddit did not begin that media transfer in time.".to_string())?
		.map_err(|_| "Reddit did not return that media item.".to_string())?;
	if !response.status().is_success() {
		return Err("Reddit did not return that media item.".to_string());
	}
	if response.content_length().is_some_and(|length| length > maximum as u64) {
		return Err("That media item is too large for a bulk archive.".to_string());
	}

	let mut bytes = Vec::new();
	let mut stream = response.bytes_stream();
	loop {
		let next = timeout(MEDIA_TRANSFER_IDLE_TIMEOUT, stream.next())
			.await
			.map_err(|_| "The media transfer stopped making progress.".to_string())?;
		let Some(chunk) = next else {
			break;
		};
		let chunk = chunk.map_err(|_| "The media transfer ended early.".to_string())?;
		if bytes.len().saturating_add(chunk.len()) > maximum {
			return Err("That media item is too large for a bulk archive.".to_string());
		}
		bytes.extend_from_slice(&chunk);
	}
	Ok(bytes)
}

pub async fn gallery_download(request: Request<Body>) -> Result<Response<Body>, String> {
	let form = match read_form(request).await {
		Ok(form) => form,
		Err((status, message)) => return Ok(download_error(status, message)),
	};
	let filename = safe_download_filename(form_value(&form, "filename"), "vale-gallery.zip");
	let mut seen = HashSet::new();
	let sources = form
		.iter()
		.filter_map(|(key, value)| (key == "media" && seen.insert(value.clone())).then_some(value.clone()))
		.take(MAX_GALLERY_ITEMS + 1)
		.collect::<Vec<_>>();
	if sources.is_empty() || sources.len() > MAX_GALLERY_ITEMS {
		return Ok(download_error(StatusCode::BAD_REQUEST, "That gallery does not contain a valid set of images."));
	}

	let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
	let options = FileOptions::default().compression_method(CompressionMethod::Stored).unix_permissions(0o600);
	let mut total = 0usize;
	for (index, source) in sources.iter().enumerate() {
		let bytes = match fetch_media(source, MAX_GALLERY_ITEM_BYTES).await {
			Ok(bytes) => bytes,
			Err(message) => return Ok(download_error(StatusCode::BAD_GATEWAY, &message)),
		};
		total = total.saturating_add(bytes.len());
		if total > MAX_GALLERY_ARCHIVE_BYTES {
			return Ok(download_error(StatusCode::PAYLOAD_TOO_LARGE, "That gallery is too large to package safely."));
		}
		let basename = source.split('?').next().and_then(|path| path.rsplit('/').next()).unwrap_or("image.jpg");
		let entry_name = safe_download_filename(&format!("{:02}_{basename}", index + 1), "gallery-image.jpg");
		archive
			.start_file(entry_name, options)
			.map_err(|error| format!("Unable to start gallery archive: {error}"))?;
		archive.write_all(&bytes).map_err(|error| format!("Unable to write gallery archive: {error}"))?;
	}
	let body = archive.finish().map_err(|error| format!("Unable to finish gallery archive: {error}"))?.into_inner();
	let length = body.len() as u64;
	Ok(attachment_response(Body::from(body), "application/zip", &filename, length))
}

fn video_cache_directory() -> PathBuf {
	env::var_os("VALE_MEDIA_CACHE_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("/var/cache/vale"))
		.join("video-downloads")
}

async fn cached_video(path: &Path) -> Option<u64> {
	fs::metadata(path)
		.await
		.ok()
		.filter(|metadata| metadata.is_file() && metadata.len() > 0)
		.map(|metadata| metadata.len())
}

async fn prune_video_cache(directory: &Path, keep: &Path) {
	let Ok(mut entries) = fs::read_dir(directory).await else {
		return;
	};
	let mut files = Vec::new();
	let mut total = 0u64;
	while let Ok(Some(entry)) = entries.next_entry().await {
		let path = entry.path();
		if path.extension().and_then(|value| value.to_str()) != Some("mp4") {
			continue;
		}
		if let Ok(metadata) = entry.metadata().await {
			total = total.saturating_add(metadata.len());
			files.push((metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH), metadata.len(), path));
		}
	}
	if total <= VIDEO_CACHE_MAX_BYTES {
		return;
	}
	files.sort_by_key(|(modified, _, _)| *modified);
	for (_, length, path) in files {
		if total <= VIDEO_CACHE_TARGET_BYTES {
			break;
		}
		if path != keep && fs::remove_file(&path).await.is_ok() {
			total = total.saturating_sub(length);
		}
	}
}

fn ffmpeg_remux_arguments(user_agent: &str, upstream: &str, maximum: u64) -> Vec<String> {
	[
		"-hide_banner",
		"-loglevel",
		"error",
		"-nostdin",
		"-user_agent",
		user_agent,
		"-i",
		upstream,
		"-map",
		"0:v:0",
		"-map",
		"0:a:0",
		"-c",
		"copy",
		"-map_metadata",
		"-1",
		"-movflags",
		"+faststart",
		"-fs",
	]
	.into_iter()
	.map(str::to_string)
	.chain(std::iter::once(maximum.to_string()))
	.chain(["-y".to_string()])
	.collect()
}

async fn remux_upstream_to(upstream: &str, output: &Path, maximum: u64) -> Result<u64, String> {
	let user_agent = OAUTH_CLIENT.load_full().user_agent().to_string();
	let mut command = Command::new("ffmpeg");
	command.args(ffmpeg_remux_arguments(&user_agent, upstream, maximum));
	command.arg(output);
	let status = timeout(VIDEO_REMUX_TIMEOUT, command.status()).await;
	let successful = matches!(status, Ok(Ok(status)) if status.success());
	if !successful {
		let _ = fs::remove_file(output).await;
		return Err("Vale could not package that video with audio. Please try again.".to_string());
	}
	cached_video(output).await.ok_or_else(|| "Reddit returned an empty video.".to_string())
}

pub(crate) async fn remux_video_to(hls_source: &str, output: &Path, maximum: u64) -> Result<u64, String> {
	let upstream = upstream_media_url(hls_source)?;
	let _permit = VIDEO_REMUX_SLOT.acquire().await.map_err(|_| "The video download worker is unavailable.".to_string())?;
	remux_upstream_to(&upstream, output, maximum.min(MAX_VIDEO_DOWNLOAD_BYTES)).await
}

async fn remux_video(hls_source: &str) -> Result<(PathBuf, u64), String> {
	let upstream = upstream_media_url(hls_source)?;
	let digest = format!("{:x}", Sha256::digest(upstream.as_bytes()));
	let directory = video_cache_directory();
	fs::create_dir_all(&directory).await.map_err(|_| "Vale could not prepare its video cache.".to_string())?;
	let output = directory.join(format!("{digest}.mp4"));
	if let Some(length) = cached_video(&output).await {
		return Ok((output, length));
	}

	let _permit = VIDEO_REMUX_SLOT.acquire().await.map_err(|_| "The video download worker is unavailable.".to_string())?;
	if let Some(length) = cached_video(&output).await {
		return Ok((output, length));
	}

	let temporary = directory.join(format!(".{digest}-{}.mp4", uuid::Uuid::new_v4()));
	let length = remux_upstream_to(&upstream, &temporary, MAX_VIDEO_DOWNLOAD_BYTES).await?;
	fs::rename(&temporary, &output)
		.await
		.map_err(|_| "Vale could not finish that video download.".to_string())?;
	prune_video_cache(&directory, &output).await;
	Ok((output, length))
}

pub async fn video_download(request: Request<Body>) -> Result<Response<Body>, String> {
	let form = match read_form(request).await {
		Ok(form) => form,
		Err((status, message)) => return Ok(download_error(status, message)),
	};
	let hls_source = form_value(&form, "hls");
	if hls_source.is_empty() {
		return Ok(download_error(StatusCode::BAD_REQUEST, "That post does not include an audio-capable video stream."));
	}
	let filename = safe_download_filename(form_value(&form, "filename"), "vale-video.mp4");
	let (path, length) = match remux_video(hls_source).await {
		Ok(result) => result,
		Err(message) => return Ok(download_error(StatusCode::BAD_GATEWAY, &message)),
	};
	let file = fs::File::open(path).await.map_err(|_| "Vale could not open the completed video download.".to_string())?;
	Ok(attachment_response(Body::wrap_stream(ReaderStream::new(file)), "video/mp4", &filename, length))
}

#[cfg(test)]
mod tests {
	use super::*;
	use hyper::body::Bytes;

	#[test]
	fn only_known_same_origin_media_maps_to_reddit_cdns() {
		assert_eq!(upstream_media_url("/img/example.jpg?download=example.jpg").unwrap(), "https://i.redd.it/example.jpg");
		assert_eq!(
			upstream_media_url("/preview/pre/example.jpg?width=1080&auto=webp").unwrap(),
			"https://preview.redd.it/example.jpg?width=1080&auto=webp"
		);
		assert_eq!(
			upstream_media_url("/vid/abc123/720.mp4?source=fallback").unwrap(),
			"https://v.redd.it/abc123/DASH_720.mp4?source=fallback"
		);
		assert!(upstream_media_url("https://example.com/image.jpg").is_err());
		assert!(upstream_media_url("/settings").is_err());
	}

	#[test]
	fn video_remux_requires_both_video_and_audio_streams() {
		let arguments = ffmpeg_remux_arguments("Vale test", "https://v.redd.it/example/HLSPlaylist.m3u8", 1024);
		assert!(arguments.windows(2).any(|pair| pair == ["-map", "0:v:0"]));
		assert!(arguments.windows(2).any(|pair| pair == ["-map", "0:a:0"]));
		assert!(!arguments.iter().any(|argument| argument == "0:a:0?"));
		assert!(!arguments.iter().any(|argument| argument.starts_with("-c:") || argument == "libx264" || argument == "aac"));
	}

	#[tokio::test]
	async fn chunked_download_forms_are_bounded_while_streaming() {
		let (mut sender, body) = Body::channel();
		let send = tokio::spawn(async move {
			sender.send_data(Bytes::from(vec![b'x'; MAX_FORM_BYTES])).await.unwrap();
			sender.send_data(Bytes::from_static(b"x")).await.unwrap();
		});
		let error = read_form(Request::new(body)).await.unwrap_err();
		send.await.unwrap();
		assert_eq!(error, (StatusCode::PAYLOAD_TOO_LARGE, FORM_TOO_LARGE));
	}
}
