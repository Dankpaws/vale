//! Hidden posts: server-backed and browser-cookie storage, and the hide/unhide form handlers.

use super::*;

pub(crate) fn valid_post_id(value: &str) -> bool {
	!value.is_empty() && value.len() <= 80 && value.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

pub(super) fn browser_hidden_post_ids(request: &Request<Body>) -> Vec<String> {
	let mut seen = HashSet::new();
	request
		.cookie(HIDDEN_POSTS_COOKIE)
		.map(|cookie| cookie.value().to_string())
		.unwrap_or_default()
		.split('.')
		.filter(|post_id| valid_post_id(post_id))
		.filter_map(|post_id| seen.insert(post_id.to_string()).then_some(post_id.to_string()))
		.take(BROWSER_HIDDEN_POST_LIMIT)
		.collect()
}

pub(super) fn hidden_posts_cookie(post_ids: &[String]) -> Cookie<'static> {
	Cookie::build((HIDDEN_POSTS_COOKIE, post_ids.join(".")))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
		.into()
}

pub(super) fn remove_hidden_posts_cookie(response: &mut Response<Body>) {
	let cookie = Cookie::build((HIDDEN_POSTS_COOKIE, ""))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::UNIX_EPOCH)
		.max_age(Duration::ZERO)
		.into();
	response.insert_cookie(cookie);
}

pub(super) fn empty_response(status: StatusCode) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("cache-control", "no-store")
		.body(Body::empty())
		.unwrap_or_default()
}

pub(super) fn hidden_post_ids(request: &Request<Body>) -> Result<HashSet<String>, String> {
	let Some(context) = context(request) else {
		return Ok(browser_hidden_post_ids(request).into_iter().collect());
	};
	let connection = open_database()?;
	let mut statement = connection
		.prepare("SELECT post_id FROM hidden_posts WHERE profile_id = ?1")
		.map_err(|error| format!("Unable to prepare hidden Vale posts: {error}"))?;
	let rows = statement
		.query_map(params![context.profile_id], |row| row.get::<_, String>(0))
		.map_err(|error| format!("Unable to query hidden Vale posts: {error}"))?;
	rows
		.collect::<Result<HashSet<_>, _>>()
		.map_err(|error| format!("Unable to read hidden Vale posts: {error}"))
}

pub(crate) fn hidden_post_ids_for_listing(request: &Request<Body>) -> Result<HashSet<String>, String> {
	hidden_post_ids(request)
}

pub fn post_is_hidden(request: &Request<Body>, post_id: &str) -> Result<bool, String> {
	let Some(context) = context(request) else {
		return Ok(browser_hidden_post_ids(request).iter().any(|hidden| hidden == post_id));
	};
	post_is_hidden_in(&*open_database()?, context.profile_id, post_id)
}

pub(super) fn post_is_hidden_in(connection: &Connection, profile_id: i64, post_id: &str) -> Result<bool, String> {
	connection
		.query_row(
			"SELECT EXISTS(SELECT 1 FROM hidden_posts WHERE profile_id = ?1 AND post_id = ?2)",
			params![profile_id, post_id],
			|row| row.get(0),
		)
		.map_err(|error| format!("Unable to inspect the hidden Vale post: {error}"))
}

/// Reconcile a restored/back-forward-cached listing against current profile
/// state without polling or exposing any records beyond the explicitly named,
/// bounded post identifiers already present in that document.
pub async fn hidden_state_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let requested = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
		.find_map(|(key, value)| (key == "ids").then_some(value.into_owned()))
		.unwrap_or_default();
	let mut seen = HashSet::new();
	let ids = requested
		.split(',')
		.filter(|id| valid_post_id(id))
		.filter_map(|id| seen.insert(id.to_string()).then_some(id.to_string()))
		.take(250)
		.collect::<Vec<_>>();
	let hidden = hidden_post_ids(&request)?;
	let hidden = ids.into_iter().filter(|id| hidden.contains(id)).collect::<Vec<_>>();
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header("content-type", "application/json; charset=utf-8")
			.header("cache-control", "private, no-store")
			.body(serde_json::to_string(&hidden).unwrap_or_else(|_| "[]".to_string()).into())
			.unwrap_or_default(),
	)
}

/// Remove posts hidden by the current profile while retaining user comments,
/// whose listing records intentionally have no post title.
pub fn filter_hidden_posts(request: &Request<Body>, posts: &mut Vec<Post>) -> Result<usize, String> {
	let hidden = hidden_post_ids(request)?;
	let before = posts.len();
	posts.retain(|post| post.title.is_empty() || !hidden.contains(&post.id));
	Ok(before - posts.len())
}

pub fn hidden_post_count(request: &Request<Body>) -> Result<usize, String> {
	if let Some(context) = context(request) {
		return open_database()?
			.query_row("SELECT COUNT(*) FROM hidden_posts WHERE profile_id = ?1", params![context.profile_id], |row| {
				row.get::<_, i64>(0)
			})
			.map(|count| count.max(0) as usize)
			.map_err(|error| format!("Unable to count hidden Vale posts: {error}"));
	}
	Ok(browser_hidden_post_ids(request).len())
}

pub(super) const MAX_HIDE_FORM_BYTES: usize = 8 * 1024;
pub(super) const MAX_RETURN_TO_BYTES: usize = 2_048;

pub(super) fn valid_percent_encoding(value: &str) -> bool {
	let bytes = value.as_bytes();
	let mut index = 0usize;
	while index < bytes.len() {
		if bytes[index] == b'%' {
			if index + 2 >= bytes.len() || !bytes[index + 1].is_ascii_hexdigit() || !bytes[index + 2].is_ascii_hexdigit() {
				return false;
			}
			index += 3;
		} else {
			index += 1;
		}
	}
	true
}

/// Strictly validate a decoded form redirect. Repeated decoding is used only
/// for validation so double-encoded traversal or network paths cannot become
/// dangerous after another browser/proxy normalization step.
pub(super) fn strict_return_to(value: &str) -> Option<String> {
	if value.is_empty() || value.len() > MAX_RETURN_TO_BYTES || !value.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) || !valid_percent_encoding(value) {
		return None;
	}
	// The redirect emitted in Location must already be root-relative. Recursive
	// decoding below is validation-only; accepting an encoded leading slash
	// would validate one spelling and return a different, non-relative target.
	if !value.starts_with('/') {
		return None;
	}
	let original = value.to_string();
	let mut decoded = original.clone();
	for _ in 0..3 {
		let next = percent_decode_str(&decoded).decode_utf8().ok()?.into_owned();
		if next == decoded {
			break;
		}
		if next.len() > MAX_RETURN_TO_BYTES || !valid_percent_encoding(&next) {
			return None;
		}
		decoded = next;
	}
	if decoded.contains('%')
		|| !decoded.starts_with('/')
		|| decoded.starts_with("//")
		|| decoded.contains('\u{5c}')
		|| decoded.contains('#')
		|| decoded.chars().any(char::is_control)
	{
		return None;
	}
	let path = decoded.split('?').next().unwrap_or_default();
	if path.split('/').any(|segment| matches!(segment, "." | "..")) {
		return None;
	}
	let parsed = url::Url::parse(&format!("https://vale.invalid{decoded}")).ok()?;
	if parsed.scheme() != "https" || parsed.host_str() != Some("vale.invalid") || !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
		return None;
	}
	Some(original)
}

pub(super) fn request_origins(request: &Request<Body>) -> Vec<(String, String, u16)> {
	let mut origins = Vec::with_capacity(2);
	if let Some(origin) = get_setting("REDLIB_FULL_URL").as_deref().and_then(parsed_origin) {
		origins.push(origin);
	}
	if let Some(host) = request.headers().get("host").and_then(|value| value.to_str().ok()) {
		let scheme = crate::net::request_scheme(request);
		if let Some(origin) = parsed_origin(&format!("{scheme}://{host}")) {
			if !origins.contains(&origin) {
				origins.push(origin);
			}
		}
	}
	origins
}

pub(super) fn validated_referer_target(request: &Request<Body>) -> Option<String> {
	let referer = request.headers().get(header::REFERER)?.to_str().ok()?;
	if let Some(relative) = strict_return_to(referer) {
		return Some(relative);
	}
	// Validate the exact path text before `Url` has a chance to normalize dot
	// segments. A Referer is only a fallback, so ambiguous spellings fail closed.
	if referer.contains('\\') || referer.contains('#') || referer.chars().any(char::is_control) || !valid_percent_encoding(referer) {
		return None;
	}
	let authority_start = referer.find("://")?.saturating_add(3);
	let suffix_start = referer[authority_start..].find(['/', '?']).map(|offset| authority_start.saturating_add(offset));
	let raw_target = match suffix_start.map(|index| &referer[index..]) {
		Some(suffix) if suffix.starts_with('/') => suffix.to_string(),
		Some(query) => format!("/{query}"),
		None => "/".to_string(),
	};
	let raw_target = strict_return_to(&raw_target)?;
	let parsed = url::Url::parse(referer).ok()?;
	if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
		return None;
	}
	let referer_origin = parsed_origin(&format!(
		"{}://{}{}",
		parsed.scheme(),
		parsed.host_str()?,
		parsed.port().map(|port| format!(":{port}")).unwrap_or_default()
	))?;
	if !request_origins(request).contains(&referer_origin) {
		return None;
	}
	Some(raw_target)
}

pub(super) fn is_enhanced_hide(request: &Request<Body>) -> bool {
	let values = request.headers().get_all("x-vale-enhanced").iter().collect::<Vec<_>>();
	values.len() == 1 && values[0].as_bytes() == b"hide-v1"
}

pub(super) fn decode_form_component(value: &[u8]) -> Option<String> {
	let value = std::str::from_utf8(value).ok()?;
	if !valid_percent_encoding(value) {
		return None;
	}
	let spaces = value.replace('+', " ");
	percent_decode_str(&spaces).decode_utf8().ok().map(|value| value.into_owned())
}

pub(super) fn form_return_to(body: &[u8]) -> Option<String> {
	for pair in body.split(|byte| *byte == b'&') {
		let mut parts = pair.splitn(2, |byte| *byte == b'=');
		let key = decode_form_component(parts.next().unwrap_or_default())?;
		if key == "return_to" {
			return decode_form_component(parts.next().unwrap_or_default()).and_then(|value| strict_return_to(&value));
		}
	}
	None
}

pub(super) async fn hide_redirect_target(request: &mut Request<Body>) -> String {
	let referer = validated_referer_target(request).unwrap_or_else(|| "/".to_string());
	let body = match read_body_limited(request.body_mut(), MAX_HIDE_FORM_BYTES, "The hide form is too large.").await {
		Ok(body) => body,
		Err(_) => return referer,
	};
	form_return_to(&body).unwrap_or(referer)
}

pub(super) fn hide_mutation_response(enhanced: bool, return_to: String) -> Response<Body> {
	if enhanced {
		empty_response(StatusCode::NO_CONTENT)
	} else {
		see_other(&return_to)
	}
}

pub async fn hide_post_post(mut request: Request<Body>) -> Result<Response<Body>, String> {
	let post_id = request.param("id").unwrap_or_default();
	if !valid_post_id(&post_id) {
		return Ok(plain_response(StatusCode::BAD_REQUEST, "The post identifier is invalid."));
	}
	let enhanced = is_enhanced_hide(&request);
	let return_to = if enhanced { String::new() } else { hide_redirect_target(&mut request).await };
	if let Some(profile_id) = context(&request).map(|context| context.profile_id) {
		let connection = open_database()?;
		connection
			.execute(
				"INSERT INTO hidden_posts (profile_id, post_id, hidden_at) VALUES (?1, ?2, ?3)
				 ON CONFLICT(profile_id, post_id) DO UPDATE SET hidden_at = excluded.hidden_at",
				params![profile_id, post_id, now()],
			)
			.map_err(|error| format!("Unable to hide the Vale post: {error}"))?;
		connection
			.execute(
				"DELETE FROM hidden_posts WHERE profile_id = ?1 AND post_id IN (
					SELECT post_id FROM hidden_posts WHERE profile_id = ?1 ORDER BY hidden_at DESC LIMIT -1 OFFSET ?2
				)",
				params![profile_id, HIDDEN_POST_LIMIT],
			)
			.map_err(|error| format!("Unable to bound hidden Vale posts: {error}"))?;
		return Ok(hide_mutation_response(enhanced, return_to));
	}

	let mut post_ids = browser_hidden_post_ids(&request);
	post_ids.retain(|hidden_id| hidden_id != &post_id);
	post_ids.push(post_id);
	if post_ids.len() > BROWSER_HIDDEN_POST_LIMIT {
		post_ids.drain(..post_ids.len() - BROWSER_HIDDEN_POST_LIMIT);
	}
	let mut response = hide_mutation_response(enhanced, return_to);
	response.insert_cookie(hidden_posts_cookie(&post_ids));
	Ok(response)
}

pub async fn unhide_post_post(mut request: Request<Body>) -> Result<Response<Body>, String> {
	let post_id = request.param("id").unwrap_or_default();
	if !valid_post_id(&post_id) {
		return Ok(plain_response(StatusCode::BAD_REQUEST, "The post identifier is invalid."));
	}
	let enhanced = is_enhanced_hide(&request);
	let return_to = if enhanced { String::new() } else { hide_redirect_target(&mut request).await };
	if let Some(profile_id) = context(&request).map(|context| context.profile_id) {
		open_database()?
			.execute("DELETE FROM hidden_posts WHERE profile_id = ?1 AND post_id = ?2", params![profile_id, post_id])
			.map_err(|error| format!("Unable to restore the Vale post: {error}"))?;
		return Ok(hide_mutation_response(enhanced, return_to));
	}

	let mut post_ids = browser_hidden_post_ids(&request);
	post_ids.retain(|hidden_id| hidden_id != &post_id);
	let mut response = hide_mutation_response(enhanced, return_to);
	if post_ids.is_empty() {
		remove_hidden_posts_cookie(&mut response);
	} else {
		response.insert_cookie(hidden_posts_cookie(&post_ids));
	}
	Ok(response)
}

pub async fn hidden_clear_post(request: Request<Body>) -> Result<Response<Body>, String> {
	if let Some(profile_id) = context(&request).map(|context| context.profile_id) {
		open_database()?
			.execute("DELETE FROM hidden_posts WHERE profile_id = ?1", params![profile_id])
			.map_err(|error| format!("Unable to clear hidden Vale posts: {error}"))?;
		return Ok(see_other("/settings"));
	}
	let mut response = see_other("/settings");
	remove_hidden_posts_cookie(&mut response);
	Ok(response)
}
