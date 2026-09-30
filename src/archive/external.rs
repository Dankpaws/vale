//! Capturing linked external pages with public-address checks.

use super::*;

pub(super) fn html_media_sources(html: &str, into: &mut HashSet<String>) {
	for captures in HTML_MEDIA_URL.captures_iter(html) {
		if let Some(source) = captures.name("url").map(|value| value.as_str()) {
			if is_local_media_path(source) {
				into.insert(source.to_string());
			}
		}
	}
}

pub(super) fn comment_media_sources(comments: &[ArchivedComment], into: &mut HashSet<String>) {
	for comment in comments {
		html_media_sources(&comment.body_html, into);
		comment_media_sources(&comment.replies, into);
	}
}

pub(super) fn rewrite_archived_html(html: &str, paths: &HashMap<String, String>) -> String {
	let rewritten = rewrite_str(
		html,
		RewriteStrSettings::new().append_element_content_handler(element!("*", |element| {
			for attribute in ["src", "poster", "href"] {
				let Some(value) = element.get_attribute(attribute) else {
					continue;
				};
				let encoded = value.replace('&', "&amp;");
				if let Some(path) = paths.get(&value).or_else(|| paths.get(&encoded)) {
					element.set_attribute(attribute, path)?;
				} else if attribute == "href" && value.starts_with('/') {
					element.set_attribute(attribute, &format!("https://www.reddit.com{value}"))?;
				}
			}
			Ok(())
		})),
	);
	rewritten.unwrap_or_else(|_| html.to_string())
}

pub(super) fn rewrite_comment_assets(comments: &mut [ArchivedComment], paths: &HashMap<String, String>) {
	for comment in comments {
		comment.body_html = rewrite_archived_html(&comment.body_html, paths);
		rewrite_comment_assets(&mut comment.replies, paths);
	}
}

pub(super) fn is_public_ipv4(address: Ipv4Addr) -> bool {
	let [a, b, c, _] = address.octets();
	!matches!(
		(a, b, c),
		(0, _, _)
			| (10, _, _)
			| (100, 64..=127, _)
			| (127, _, _)
			| (169, 254, _)
			| (172, 16..=31, _)
			| (192, 0, 0)
			| (192, 0, 2)
			| (192, 88, 99)
			| (192, 168, _)
			| (198, 18..=19, _)
			| (198, 51, 100)
			| (203, 0, 113)
			| (224..=255, _, _)
	)
}

pub(super) async fn public_addresses(url: &Url) -> Result<Vec<SocketAddr>, String> {
	if !matches!(url.scheme(), "http" | "https") {
		return Err("Only HTTP and HTTPS source pages can be archived.".to_string());
	}
	if !url.username().is_empty() || url.password().is_some() {
		return Err("Source-page addresses cannot contain credentials.".to_string());
	}
	let host = url.host_str().ok_or_else(|| "The source page has no host.".to_string())?;
	let port = url.port_or_known_default().ok_or_else(|| "The source page has no valid port.".to_string())?;
	if !matches!(port, 80 | 443) {
		return Err("Source-page capture is restricted to standard web ports.".to_string());
	}
	let resolved = lookup_host((host, port)).await.map_err(|_| "The source page host could not be resolved.".to_string())?;
	let mut addresses = resolved
		.filter_map(|address| match address.ip() {
			IpAddr::V4(ip) if is_public_ipv4(ip) => Some(SocketAddr::new(IpAddr::V4(ip), port)),
			_ => None,
		})
		.collect::<Vec<_>>();
	addresses.sort_unstable();
	addresses.dedup();
	if addresses.is_empty() {
		return Err("The source page did not resolve to a permitted public IPv4 address.".to_string());
	}
	Ok(addresses)
}

pub(crate) async fn public_response(source: &str) -> Result<(Url, wreq::Response), String> {
	let mut current = Url::parse(source).map_err(|_| "The source page address is invalid.".to_string())?;
	for _ in 0..=MAX_EXTERNAL_REDIRECTS {
		let host = current.host_str().ok_or_else(|| "The source page has no host.".to_string())?.to_string();
		let addresses = public_addresses(&current).await?;
		let client = wreq::Client::builder()
			.redirect(Policy::none())
			.resolve_to_addrs(host, addresses)
			.build()
			.map_err(|_| "Vale could not initialize its isolated source-page client.".to_string())?;
		let request = client
			.get(current.as_str())
			.header("User-Agent", "Vale local archive")
			.header("Accept", "text/html,application/xhtml+xml,application/pdf,image/*,video/*;q=0.8,*/*;q=0.2")
			.send();
		let response = timeout(EXTERNAL_FETCH_TIMEOUT, request)
			.await
			.map_err(|_| "The source page timed out.".to_string())?
			.map_err(|_| "The source page could not be retrieved.".to_string())?;
		if !response.status().is_redirection() {
			return Ok((current, response));
		}
		let location = response
			.headers()
			.get(header::LOCATION.as_str())
			.and_then(|value| value.to_str().ok())
			.ok_or_else(|| "The source page redirected without a usable destination.".to_string())?;
		if location.len() > MAX_EXTERNAL_REDIRECT_LOCATION_BYTES {
			return Err("The source page returned an oversized redirect destination.".to_string());
		}
		current = current.join(location).map_err(|_| "The source page redirected to an invalid destination.".to_string())?;
	}
	Err("The source page exceeded the redirect boundary.".to_string())
}

pub(super) async fn response_bytes(response: wreq::Response, maximum: u64) -> Result<(String, Vec<u8>), String> {
	if !response.status().is_success() {
		return Err(format!("The source returned HTTP {}.", response.status().as_u16()));
	}
	if response.content_length().is_some_and(|length| length > maximum) {
		return Err(format!("The source response is larger than {}.", format_bytes(maximum)));
	}
	let content_type = response
		.headers()
		.get(header::CONTENT_TYPE.as_str())
		.and_then(|value| value.to_str().ok())
		.unwrap_or("application/octet-stream")
		.to_string();
	let mut bytes = Vec::new();
	let mut stream = response.bytes_stream();
	loop {
		let next = timeout(TRANSFER_IDLE_TIMEOUT, stream.next())
			.await
			.map_err(|_| "The source transfer stopped making progress.".to_string())?;
		let Some(chunk) = next else {
			break;
		};
		let chunk = chunk.map_err(|_| "The source transfer ended before it completed.".to_string())?;
		if (bytes.len() as u64).saturating_add(chunk.len() as u64) > maximum {
			return Err(format!("The source response exceeded {}.", format_bytes(maximum)));
		}
		bytes.extend_from_slice(&chunk);
	}
	if bytes.is_empty() {
		return Err("The source returned an empty response.".to_string());
	}
	Ok((content_type, bytes))
}

pub(super) fn prepare_external_html(bytes: Vec<u8>) -> (String, Vec<String>, String) {
	let sha256 = format!("{:x}", Sha256::digest(&bytes));
	let mut html = String::from_utf8_lossy(&bytes).into_owned();
	html = ACTIVE_HTML.replace_all(&html, "").into_owned();
	html = REFRESH_META.replace_all(&html, "").into_owned();
	html = BASE_ELEMENT.replace_all(&html, "").into_owned();
	html = EVENT_ATTRIBUTE.replace_all(&html, "").into_owned();
	html = JAVASCRIPT_URL.replace_all(&html, "$1=\"#\"").into_owned();
	let resources = PAGE_RESOURCE_URL
		.captures_iter(&html)
		.filter_map(|captures| {
			let attribute = captures.name("attribute")?.as_str().to_ascii_lowercase();
			let raw = captures.name("url")?.as_str().to_string();
			if raw.starts_with("data:") || raw.starts_with("blob:") || raw.starts_with('#') {
				return None;
			}
			let likely_stylesheet = raw.split('?').next().is_some_and(|path| path.to_ascii_lowercase().ends_with(".css"));
			(attribute != "href" || likely_stylesheet).then_some(raw)
		})
		.collect::<HashSet<_>>()
		.into_iter()
		.take(MAX_EXTERNAL_REQUISITES + 1)
		.collect();
	(html, resources, sha256)
}

pub(super) fn finish_external_html(mut html: String, final_url: &str, raw_path: &str) -> (String, String) {
	let policy = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{ARCHIVE_CSP}\"><meta name=\"referrer\" content=\"no-referrer\">");
	if let Some(index) = html.to_ascii_lowercase().find("<head>") {
		html.insert_str(index + "<head>".len(), &policy);
	} else {
		html = format!("<!doctype html><html><head>{policy}</head><body>{html}</body></html>");
	}
	let provenance = format!(
		"<aside style=\"padding:12px;background:#111;color:#ddd;font:14px system-ui\">Locally captured by Vale from <a style=\"color:#7adac8\" href=\"{}\">{}</a>. Active content was removed. <a style=\"color:#7adac8\" href=\"../../{}\">Original response</a>.</aside>",
		htmlescape::encode_attribute(final_url),
		htmlescape::encode_minimal(final_url),
		raw_path
	);
	if let Some(index) = html.to_ascii_lowercase().find("<body") {
		if let Some(close) = html[index..].find('>') {
			html.insert_str(index + close + 1, &provenance);
		}
	}
	let sha256 = format!("{:x}", Sha256::digest(html.as_bytes()));
	(html, sha256)
}

impl CaptureContext {
	pub(super) async fn store_bytes_asset(&mut self, source_key: &str, original_url: &str, content_type: &str, bytes: Vec<u8>) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(source_key).or_else(|| self.asset_paths.get(original_url)).cloned() {
			self.asset_paths.insert(source_key.to_string(), path.clone());
			return Ok(path);
		}
		if bytes.len() as u64 > self.remaining_for_assets() {
			return Err("The archive reached its configured size boundary.".to_string());
		}
		let extension = content_type_extension(content_type, original_url);
		let url_digest = format!("{:x}", Sha256::digest(original_url.as_bytes()));
		let filename = format!("{url_digest}.{extension}");
		let relative = format!("files/assets/{filename}");
		let directory = self.directory.join("files/assets");
		fs::create_dir_all(&directory)
			.await
			.map_err(|error| format!("Unable to prepare the archive asset directory: {error}"))?;
		let destination = directory.join(&filename);
		let mut file = fs::File::create(&destination)
			.await
			.map_err(|error| format!("Unable to create an archived source file: {error}"))?;
		file.write_all(&bytes).await.map_err(|error| format!("Unable to write an archived source file: {error}"))?;
		file.sync_all().await.map_err(|error| format!("Unable to make an archived source file durable: {error}"))?;
		let length = bytes.len() as u64;
		let sha256 = tokio::task::spawn_blocking(move || format!("{:x}", Sha256::digest(bytes)))
			.await
			.map_err(|_| "The archive checksum task stopped unexpectedly.".to_string())?;
		self.total_bytes = self.total_bytes.saturating_add(length);
		self.assets.push(ArchiveAsset {
			path: relative.clone(),
			original_url: original_url.to_string(),
			content_type: content_type.to_string(),
			bytes: length,
			sha256,
		});
		self.asset_paths.insert(source_key.to_string(), relative.clone());
		self.asset_paths.insert(original_url.to_string(), relative.clone());
		Ok(relative)
	}

	pub(super) async fn write_generated(&mut self, relative: &str, bytes: &[u8]) -> Result<(), String> {
		if bytes.len() as u64 > self.remaining() {
			return Err("The archive reached its configured size boundary while writing its reader.".to_string());
		}
		let destination = self.directory.join(relative);
		if let Some(parent) = destination.parent() {
			fs::create_dir_all(parent)
				.await
				.map_err(|error| format!("Unable to prepare an archive directory: {error}"))?;
		}
		let mut file = fs::File::create(&destination)
			.await
			.map_err(|error| format!("Unable to create an archive document: {error}"))?;
		file.write_all(bytes).await.map_err(|error| format!("Unable to write an archive document: {error}"))?;
		file.sync_all().await.map_err(|error| format!("Unable to make an archive document durable: {error}"))?;
		self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
		Ok(())
	}

	pub(super) async fn write_reader_asset(&mut self, relative: &str, content_type: &str, bytes: &[u8]) -> Result<(), String> {
		self.write_generated(relative, bytes).await?;
		self.generated_assets.push(GeneratedArchiveAsset {
			path: relative.to_string(),
			content_type: content_type.to_string(),
			bytes: bytes.len() as u64,
			sha256: format!("{:x}", Sha256::digest(bytes)),
		});
		Ok(())
	}

	pub(super) async fn capture_external_source(&mut self, source: &str) -> Result<String, String> {
		if let Some(path) = self.asset_paths.get(source).cloned() {
			return Ok(path);
		}
		let (final_url, response) = public_response(source).await?;
		let maximum = MAX_EXTERNAL_DOCUMENT_BYTES.min(self.remaining_for_assets() / 2);
		let (content_type, bytes) = response_bytes(response, maximum).await?;
		let media_type = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
		if media_type != "text/html" && media_type != "application/xhtml+xml" {
			return self.store_bytes_asset(source, final_url.as_str(), &content_type, bytes).await;
		}

		let raw_path = "files/source/original-response.bin".to_string();
		self.write_generated(&raw_path, &bytes).await?;
		let raw_bytes = bytes.len() as u64;
		let (mut html, resources, raw_sha256) = tokio::task::spawn_blocking(move || prepare_external_html(bytes))
			.await
			.map_err(|_| "The source-page sanitizing task stopped unexpectedly.".to_string())?;
		self.assets.push(ArchiveAsset {
			path: raw_path.clone(),
			original_url: final_url.to_string(),
			content_type: "application/octet-stream".to_string(),
			bytes: raw_bytes,
			sha256: raw_sha256,
		});
		self.push_issue(
			"source page",
			"The linked HTML page was preserved as a sanitized, best-effort snapshot. Active code and resources discoverable only after script execution were deliberately not stored.",
		);
		let sanitized_document_reserve = (html.len() as u64).saturating_add(128 * 1024);
		if resources.len() > MAX_EXTERNAL_REQUISITES {
			self.push_issue(
				"source page",
				format!("The linked page referenced more than {MAX_EXTERNAL_REQUISITES} direct assets; additional assets were left as omissions."),
			);
		}
		for raw in resources.into_iter().take(MAX_EXTERNAL_REQUISITES) {
			let resource_allowance = self.remaining_for_assets().saturating_sub(sanitized_document_reserve);
			if resource_allowance == 0 {
				self.push_issue(
					"source page",
					"The remaining capture allowance was reserved for the sanitized offline document; later page assets were omitted.",
				);
				break;
			}
			let Ok(resource_url) = final_url.join(&raw) else {
				self.push_issue("source page", format!("A linked-page asset used an invalid address: {raw}"));
				continue;
			};
			if !matches!(resource_url.scheme(), "http" | "https") {
				continue;
			}
			let result = async {
				let (resolved_url, response) = public_response(resource_url.as_str()).await?;
				let path = self
					.store_response(&raw, resolved_url.as_str(), response, MAX_EXTERNAL_REQUISITE_BYTES.min(resource_allowance))
					.await?;
				Ok::<String, String>(path)
			}
			.await;
			match result {
				Ok(path) => {
					let source_relative = path.strip_prefix("files/").map(|path| format!("../{path}")).unwrap_or(path);
					html = tokio::task::spawn_blocking(move || html.replace(&raw, &source_relative))
						.await
						.map_err(|_| "The source-page rewriting task stopped unexpectedly.".to_string())?;
				}
				Err(message) => self.push_issue("source page", format!("A linked-page asset could not be stored ({raw}): {message}")),
			}
		}
		let final_url_string = final_url.to_string();
		let raw_path_for_reader = raw_path.clone();
		let (html, html_sha256) = tokio::task::spawn_blocking(move || finish_external_html(html, &final_url_string, &raw_path_for_reader))
			.await
			.map_err(|_| "The source-page finalization task stopped unexpectedly.".to_string())?;
		let sanitized_path = "files/source/index.html";
		if html.len() as u64 > self.remaining_for_assets() {
			return Err("The sanitized source page no longer fit inside the capture allowance reserved for it.".to_string());
		}
		self.write_generated(sanitized_path, html.as_bytes()).await?;
		self.assets.push(ArchiveAsset {
			path: sanitized_path.to_string(),
			original_url: final_url.to_string(),
			content_type: "text/html; charset=utf-8".to_string(),
			bytes: html.len() as u64,
			sha256: html_sha256,
		});
		self.asset_paths.insert(source.to_string(), sanitized_path.to_string());
		Ok(sanitized_path.to_string())
	}
}
