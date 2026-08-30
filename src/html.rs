//! HTML policies shared by Vale surfaces that render upstream markup.
//!
//! Community sidebar HTML is deliberately much narrower than Reddit's normal
//! Markdown output. Ammonia owns the explicit structural allowlist and active
//! content removal; an HTML5-aware rewrite pass then applies the contextual URL,
//! numeric-attribute, generated-attribute, and heading policies that cannot be
//! expressed as a static allowlist.

use ammonia::{Builder, UrlRelative};
use lol_html::{element, rewrite_str, RewriteStrSettings};
use std::{
	cell::Cell,
	collections::{HashMap, HashSet},
	rc::Rc,
};
use url::Url;

const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_TABLE_SPAN: u32 = 100;
const MAX_ORDERED_LIST_START: i64 = 100_000;
const VALE_URL_BASE: &str = "https://vale.invalid";

const APPROVED_STATIC_IMAGE_PREFIXES: &[&str] = &["award_images/", "gold/awards/", "marketplace-assets/"];

/// Sanitize the HTML from Reddit's `subreddit.info` field for the community
/// sidebar. A rewrite failure fails closed instead of returning partially
/// processed upstream markup.
pub fn sanitize_subreddit_info(input: &str) -> String {
	let allowlisted = community_sidebar_builder().clean(input).to_string();
	rewrite_community_sidebar(&allowlisted).unwrap_or_default()
}

/// Name-explicit alias for callers that prefer to identify the returned value
/// as HTML.
pub fn sanitize_subreddit_info_html(input: &str) -> String {
	sanitize_subreddit_info(input)
}

/// Return a same-origin proxy path for a community icon, or no source so the
/// app-owned initial fallback is used. This intentionally shares the sidebar
/// image policy and never passes an unrecognized upstream URL to the browser.
pub fn sanitize_subreddit_image_source(input: &str) -> String {
	approved_image_source(input).unwrap_or_default()
}

/// Shift headings in one archived-comment fragment below the reader's h2
/// discussion heading. This policy changes heading tag names only: every other
/// token and attribute remains under the archive reader's existing CSP and
/// asset-rewrite contract.
pub fn normalize_archive_comment_headings(input: &str) -> Result<String, lol_html::errors::RewritingError> {
	let previous = Rc::new(Cell::new(2_u8));
	let h1_previous = Rc::clone(&previous);
	let h2_previous = Rc::clone(&previous);
	let h3_previous = Rc::clone(&previous);
	let h4_previous = Rc::clone(&previous);
	let h5_previous = Rc::clone(&previous);
	let h6_previous = Rc::clone(&previous);

	rewrite_str(
		input,
		RewriteStrSettings::new()
			.with_strict(true)
			.with_enable_esi_tags(false)
			.append_element_content_handler(element!("h1", move |element| normalize_archive_heading(element, 1, &h1_previous)))
			.append_element_content_handler(element!("h2", move |element| normalize_archive_heading(element, 2, &h2_previous)))
			.append_element_content_handler(element!("h3", move |element| normalize_archive_heading(element, 3, &h3_previous)))
			.append_element_content_handler(element!("h4", move |element| normalize_archive_heading(element, 4, &h4_previous)))
			.append_element_content_handler(element!("h5", move |element| normalize_archive_heading(element, 5, &h5_previous)))
			.append_element_content_handler(element!("h6", move |element| normalize_archive_heading(element, 6, &h6_previous))),
	)
}

fn normalize_archive_heading(element: &mut lol_html::html_content::Element<'_, '_>, source_level: u8, previous: &Cell<u8>) -> lol_html::HandlerResult {
	let requested = source_level.saturating_add(2).clamp(3, 6);
	let emitted = requested.min(previous.get().saturating_add(1));
	element.set_tag_name(&format!("h{emitted}"))?;
	previous.set(emitted);
	Ok(())
}

fn community_sidebar_builder() -> Builder<'static> {
	let tags = [
		"a",
		"blockquote",
		"br",
		"caption",
		"code",
		"dd",
		"del",
		"dl",
		"dt",
		"em",
		// h1 is an input-only allowance. The rewrite pass always maps it to h2.
		"h1",
		"h2",
		"h3",
		"h4",
		"h5",
		"h6",
		"hr",
		"img",
		"li",
		"ol",
		"p",
		"pre",
		"strong",
		"sub",
		"sup",
		"table",
		"tbody",
		"td",
		"tfoot",
		"th",
		"thead",
		"tr",
		"ul",
	]
	.into_iter()
	.collect::<HashSet<_>>();

	let clean_content_tags = [
		"button", "datalist", "embed", "fieldset", "form", "iframe", "input", "keygen", "label", "legend", "math", "meter", "object", "optgroup", "option", "output", "progress",
		"script", "select", "style", "svg", "template", "textarea",
	]
	.into_iter()
	.collect::<HashSet<_>>();

	let tag_attributes = HashMap::from([
		("a", ["href", "title"].into_iter().collect::<HashSet<_>>()),
		("img", ["alt", "height", "src", "width"].into_iter().collect::<HashSet<_>>()),
		("ol", ["start"].into_iter().collect::<HashSet<_>>()),
		("td", ["colspan", "rowspan"].into_iter().collect::<HashSet<_>>()),
		("th", ["colspan", "rowspan", "scope"].into_iter().collect::<HashSet<_>>()),
	]);

	let mut builder = Builder::new();
	builder
		.tags(tags)
		.clean_content_tags(clean_content_tags)
		.tag_attributes(tag_attributes)
		.tag_attribute_values(HashMap::<&str, HashMap<&str, HashSet<&str>>>::new())
		.set_tag_attribute_values(HashMap::<&str, HashMap<&str, &str>>::new())
		.generic_attributes(HashSet::new())
		.generic_attribute_prefixes(HashSet::new())
		.allowed_classes(HashMap::new())
		.url_schemes(["http", "https"].into_iter().collect())
		.url_relative(UrlRelative::PassThrough)
		.link_rel(None)
		.id_prefix(None)
		.strip_comments(true);
	builder
}

fn rewrite_community_sidebar(input: &str) -> Result<String, lol_html::errors::RewritingError> {
	let mut previous_heading = None;
	rewrite_str(
		input,
		RewriteStrSettings::new().append_element_content_handler(element!("*", move |element| {
			match element.tag_name().as_str() {
				"h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
					let requested = match element.tag_name().as_bytes().get(1) {
						Some(b'1') => 2,
						Some(level @ b'2'..=b'6') => level - b'0',
						_ => 2,
					};
					let emitted = previous_heading.map_or(2, |previous: u8| requested.min(previous.saturating_add(1)));
					previous_heading = Some(emitted);
					element.set_tag_name(&format!("h{emitted}"))?;
				}
				"a" => rewrite_anchor(element)?,
				"img" => rewrite_image(element)?,
				"ol" => canonicalize_signed_attribute(element, "start", MAX_ORDERED_LIST_START),
				"td" | "th" => {
					canonicalize_unsigned_attribute(element, "colspan", MAX_TABLE_SPAN);
					canonicalize_unsigned_attribute(element, "rowspan", MAX_TABLE_SPAN);
					if element.tag_name() == "th" {
						canonicalize_scope(element);
					}
				}
				_ => {}
			}
			Ok(())
		})),
	)
}

enum AnchorDestination {
	Local(String),
	External(String),
}

fn rewrite_anchor(element: &mut lol_html::html_content::Element<'_, '_>) -> lol_html::HandlerResult {
	let Some(href) = element.get_attribute("href") else {
		element.remove_and_keep_content();
		return Ok(());
	};

	match anchor_destination(&href) {
		Some(AnchorDestination::Local(href)) => {
			element.set_attribute("href", &href)?;
			element.remove_attribute("target");
			element.remove_attribute("rel");
		}
		Some(AnchorDestination::External(href)) => {
			element.set_attribute("href", &href)?;
			element.set_attribute("target", "_blank")?;
			element.set_attribute("rel", "nofollow noopener noreferrer")?;
		}
		None => element.remove_and_keep_content(),
	}
	Ok(())
}

fn rewrite_image(element: &mut lol_html::html_content::Element<'_, '_>) -> lol_html::HandlerResult {
	let Some(src) = element.get_attribute("src").and_then(|src| approved_image_source(&src)) else {
		element.remove();
		return Ok(());
	};

	element.set_attribute("src", &src)?;
	canonicalize_unsigned_attribute(element, "width", MAX_IMAGE_DIMENSION);
	canonicalize_unsigned_attribute(element, "height", MAX_IMAGE_DIMENSION);
	element.set_attribute("loading", "lazy")?;
	element.set_attribute("decoding", "async")?;
	Ok(())
}

fn anchor_destination(value: &str) -> Option<AnchorDestination> {
	if !url_text_is_safe(value) {
		return None;
	}
	if value.starts_with('/') {
		return safe_local_path(value).map(AnchorDestination::Local);
	}
	// Protocol-relative links do not assert the required http/https scheme.
	if value.starts_with("//") {
		return None;
	}

	let parsed = Url::parse(value).ok()?;
	if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() || !url_has_no_credentials(&parsed) || !raw_path_is_safe(value) {
		return None;
	}

	if is_reddit_navigation_host(parsed.host_str()?) {
		if parsed.port().is_some() {
			return None;
		}
		let mut local = parsed.path().to_string();
		if let Some(query) = parsed.query() {
			local.push('?');
			local.push_str(query);
		}
		if let Some(fragment) = parsed.fragment() {
			local.push('#');
			local.push_str(fragment);
		}
		return safe_local_path(&local).map(AnchorDestination::Local);
	}

	Some(AnchorDestination::External(parsed.to_string()))
}

fn approved_image_source(value: &str) -> Option<String> {
	if !url_text_is_safe(value) {
		return None;
	}
	if value.starts_with('/') && !value.starts_with("//") {
		return safe_local_image_path(value);
	}

	let absolute = if value.starts_with("//") { format!("https:{value}") } else { value.to_string() };
	let parsed = Url::parse(&absolute).ok()?;
	if !matches!(parsed.scheme(), "http" | "https")
		|| parsed.host_str().is_none()
		|| parsed.port().is_some()
		|| parsed.fragment().is_some()
		|| !url_has_no_credentials(&parsed)
		|| !raw_path_is_safe(value)
	{
		return None;
	}

	let path = parsed.path().strip_prefix('/')?;
	let prefix = match parsed.host_str()? {
		"i.redd.it" => "/img/",
		"preview.redd.it" => "/preview/pre/",
		"external-preview.redd.it" => "/preview/external-pre/",
		"a.thumbs.redditmedia.com" => "/thumb/a/",
		"b.thumbs.redditmedia.com" => "/thumb/b/",
		"emoji.redditmedia.com" => "/emoji/",
		"www.redditstatic.com" => "/static/",
		_ => return None,
	};
	let mut local = format!("{prefix}{path}");
	if let Some(query) = parsed.query() {
		local.push('?');
		local.push_str(query);
	}
	safe_local_image_path(&local)
}

fn safe_local_path(value: &str) -> Option<String> {
	if !value.starts_with('/') || value.starts_with("//") || !url_text_is_safe(value) || !raw_path_is_safe(value) {
		return None;
	}
	let parsed = Url::parse(&format!("{VALE_URL_BASE}{value}")).ok()?;
	(parsed.origin().ascii_serialization() == VALE_URL_BASE && url_has_no_credentials(&parsed)).then(|| value.to_string())
}

fn safe_local_image_path(value: &str) -> Option<String> {
	if value.contains('#') {
		return None;
	}
	let value = safe_local_path(value)?;
	let parsed = Url::parse(&format!("{VALE_URL_BASE}{value}")).ok()?;
	let path = parsed.path();

	let approved = if let Some(rest) = path.strip_prefix("/img/") {
		media_path_remainder_is_safe(rest)
	} else if let Some(rest) = path.strip_prefix("/preview/") {
		media_path_remainder_is_safe(rest)
	} else if let Some(rest) = path.strip_prefix("/thumb/") {
		let mut segments = rest.split('/');
		matches!(segments.next(), Some("a" | "b")) && segments.next().is_some_and(safe_media_segment) && segments.next().is_none()
	} else if let Some(rest) = path.strip_prefix("/emoji/") {
		let mut segments = rest.split('/');
		segments.next().is_some_and(safe_media_segment) && segments.next().is_some_and(safe_media_segment) && segments.next().is_none()
	} else if let Some(rest) = path.strip_prefix("/static/") {
		APPROVED_STATIC_IMAGE_PREFIXES.iter().any(|prefix| rest.starts_with(prefix)) && media_path_remainder_is_safe(rest)
	} else {
		false
	};
	approved.then_some(value)
}

fn media_path_remainder_is_safe(value: &str) -> bool {
	!value.is_empty() && value.split('/').all(safe_media_segment)
}

fn safe_media_segment(value: &str) -> bool {
	!value.is_empty() && !matches!(value, "." | "..")
}

fn is_reddit_navigation_host(host: &str) -> bool {
	host == "reddit.com" || host.ends_with(".reddit.com") || host == "redd.it" || host.ends_with(".redd.it")
}

fn url_has_no_credentials(url: &Url) -> bool {
	url.username().is_empty() && url.password().is_none()
}

fn url_text_is_safe(value: &str) -> bool {
	if value.is_empty() || value.trim() != value || value.chars().any(|character| character.is_control() || character == '\\' || character.is_whitespace()) {
		return false;
	}
	let Some(decoded) = percent_decode_strict(value) else {
		return false;
	};
	decoded.chars().all(|character| !character.is_control() && character != '\\')
}

fn raw_path_is_safe(value: &str) -> bool {
	let Some(path) = raw_path(value) else {
		return false;
	};
	let Some(decoded) = percent_decode_strict(path) else {
		return false;
	};
	if decoded.contains('\\') || decoded.contains('%') || decoded.chars().any(char::is_control) {
		return false;
	}
	!decoded.split('/').any(|segment| {
		let segment = segment.split(';').next().unwrap_or_default();
		matches!(segment, "." | "..")
	})
}

fn raw_path(value: &str) -> Option<&str> {
	let end = value.find(['?', '#']).unwrap_or(value.len());
	let address = &value[..end];
	if address.starts_with('/') && !address.starts_with("//") {
		return Some(address);
	}

	let authority_start = if let Some(rest) = address.strip_prefix("//") {
		address.len() - rest.len()
	} else {
		address.find("://")?.saturating_add(3)
	};
	address[authority_start..].find('/').map_or(Some("/"), |slash| Some(&address[authority_start + slash..]))
}

fn percent_decode_strict(value: &str) -> Option<String> {
	let bytes = value.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == b'%' {
			let high = bytes.get(index + 1).and_then(|byte| hex_value(*byte))?;
			let low = bytes.get(index + 2).and_then(|byte| hex_value(*byte))?;
			decoded.push((high << 4) | low);
			index += 3;
		} else {
			decoded.push(bytes[index]);
			index += 1;
		}
	}
	String::from_utf8(decoded).ok()
}

fn hex_value(value: u8) -> Option<u8> {
	match value {
		b'0'..=b'9' => Some(value - b'0'),
		b'a'..=b'f' => Some(value - b'a' + 10),
		b'A'..=b'F' => Some(value - b'A' + 10),
		_ => None,
	}
}

fn canonicalize_unsigned_attribute(element: &mut lol_html::html_content::Element<'_, '_>, name: &str, maximum: u32) {
	let Some(value) = element.get_attribute(name) else {
		return;
	};
	let canonical = value
		.chars()
		.all(|character| character.is_ascii_digit())
		.then(|| value.parse::<u32>().ok())
		.flatten()
		.filter(|number| (1..=maximum).contains(number))
		.map(|number| number.to_string());
	if let Some(canonical) = canonical {
		let _ = element.set_attribute(name, &canonical);
	} else {
		element.remove_attribute(name);
	}
}

fn canonicalize_signed_attribute(element: &mut lol_html::html_content::Element<'_, '_>, name: &str, maximum_absolute: i64) {
	let Some(value) = element.get_attribute(name) else {
		return;
	};
	let digits = value.strip_prefix(['-', '+']).unwrap_or(&value);
	let canonical = (!digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit()))
		.then(|| value.parse::<i64>().ok())
		.flatten()
		.filter(|number| number.unsigned_abs() <= maximum_absolute as u64)
		.map(|number| number.to_string());
	if let Some(canonical) = canonical {
		let _ = element.set_attribute(name, &canonical);
	} else {
		element.remove_attribute(name);
	}
}

fn canonicalize_scope(element: &mut lol_html::html_content::Element<'_, '_>) {
	let Some(scope) = element.get_attribute("scope") else {
		return;
	};
	let scope = scope.to_ascii_lowercase();
	if matches!(scope.as_str(), "row" | "col" | "rowgroup" | "colgroup") {
		let _ = element.set_attribute("scope", &scope);
	} else {
		element.remove_attribute("scope");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hostile_active_content_is_removed_with_its_contents() {
		let input = r#"
			<script>script leak</script><style>style leak</style><template>template leak</template>
			<iframe>frame leak</iframe><object>object leak</object><embed src="/img/x">
			<form>form leak<button>button leak</button><input value="input leak"><meter>meter leak</meter><progress>progress leak</progress></form>
			<svg><text>svg leak</text></svg><math><mi>math leak</mi></math>
			<div><span><p id="source-id" class="source-class" style="color:red" data-x="1" onclick="bad()">kept text</p></span></div>
		"#;
		let output = sanitize_subreddit_info(input);
		for forbidden in [
			"script leak",
			"style leak",
			"template leak",
			"frame leak",
			"object leak",
			"form leak",
			"button leak",
			"input leak",
			"meter leak",
			"progress leak",
			"svg leak",
			"math leak",
		] {
			assert!(!output.contains(forbidden), "{forbidden} survived in {output}");
		}
		assert!(output.contains("<p>kept text</p>"));
		for attribute in ["source-id", "source-class", "style=", "data-x", "onclick"] {
			assert!(!output.contains(attribute), "{attribute} survived in {output}");
		}
	}

	#[test]
	fn presentation_wrappers_are_unwrapped_and_safe_structure_remains() {
		let input = "<article><div><span>plain <b>bold wrapper</b></span></div><blockquote><strong>strong</strong> <em>em</em> <del>gone</del> H<sub>2</sub>O<sup>+</sup></blockquote><dl><dt>Term</dt><dd>Definition</dd></dl></article>";
		let output = sanitize_subreddit_info(input);
		assert_eq!(
			output,
			"plain bold wrapper<blockquote><strong>strong</strong> <em>em</em> <del>gone</del> H<sub>2</sub>O<sup>+</sup></blockquote><dl><dt>Term</dt><dd>Definition</dd></dl>"
		);
	}

	#[test]
	fn headings_have_an_h2_baseline_and_advance_one_level_at_most() {
		let output = sanitize_subreddit_info("<h1>One</h1><h4>Two</h4><h2>Three</h2><h6>Four</h6><h5>Five</h5><h2>Six</h2>");
		assert_eq!(output, "<h2>One</h2><h3>Two</h3><h2>Three</h2><h3>Four</h3><h4>Five</h4><h2>Six</h2>");
		assert!(!output.contains("<h1"));

		assert_eq!(
			sanitize_subreddit_info("<h6>Deep first</h6><h6>Deep second</h6>"),
			"<h2>Deep first</h2><h3>Deep second</h3>"
		);
	}

	#[test]
	fn anchors_rewrite_reddit_and_generate_external_safety_attributes() {
		let output = sanitize_subreddit_info(
			r#"<p>
			<a href="/r/rust?sort=top&amp;t=week#rules" title="Local">local</a>
			<a href="https://old.reddit.com/r/selfhosted/comments/abc/post/?context=3#reply">reddit</a>
			<a href="https://example.org/read?q=vale&amp;lang=en" target="same" rel="opener">external</a>
			</p>"#,
		);
		assert!(output.contains(r#"href="/r/rust?sort=top&amp;t=week#rules" title="Local""#));
		assert!(output.contains(r#"href="/r/selfhosted/comments/abc/post/?context=3#reply""#));
		assert!(output.contains(r#"href="https://example.org/read?q=vale&amp;lang=en" target="_blank" rel="nofollow noopener noreferrer""#));
	}

	#[test]
	fn invalid_anchor_destinations_unwrap_without_losing_text() {
		let input = r#"
			<a href="javascript:alert(1)">script</a>
			<a href="data:text/html,bad">data</a>
			<a href="blob:https://example.org/id">blob</a>
			<a href="mailto:test@example.org">mail</a>
			<a href="//example.org/path">protocol</a>
			<a href="https://user:secret@example.org/path">credentials</a>
			<a href="/r/test/%2e%2e/account">traversal</a>
			<a href="/r/test/%ZZ">encoding</a>
			<a href="relative/path">relative</a>
		"#;
		let output = sanitize_subreddit_info(input);
		assert!(!output.contains("<a"), "invalid anchor survived: {output}");
		for text in ["script", "data", "blob", "mail", "protocol", "credentials", "traversal", "encoding", "relative"] {
			assert!(output.contains(text), "anchor text {text} was lost in {output}");
		}
	}

	#[test]
	fn images_are_same_origin_lazy_and_strictly_bounded() {
		let output = sanitize_subreddit_info(
			r#"
			<img src="/img/local.png" alt="local" width="0032" height="20" onerror="bad()">
			<img src="https://preview.redd.it/photo.jpg?width=640&amp;auto=webp" alt="preview" width="99999" height="-1">
			<img src="//emoji.redditmedia.com/id/name.png" alt="emoji">
			<img src="https://www.redditstatic.com/marketplace-assets/v1/emote.gif" alt="static">
		"#,
		);
		assert!(output.contains(r#"src="/img/local.png" alt="local" width="32" height="20" loading="lazy" decoding="async""#));
		assert!(output.contains(r#"src="/preview/pre/photo.jpg?width=640&amp;auto=webp" alt="preview" loading="lazy" decoding="async""#));
		assert!(output.contains(r#"src="/emoji/id/name.png" alt="emoji" loading="lazy" decoding="async""#));
		assert!(output.contains(r#"src="/static/marketplace-assets/v1/emote.gif" alt="static" loading="lazy" decoding="async""#));
		assert!(!output.contains("onerror"));
		assert!(!output.contains("99999"));
	}

	#[test]
	fn unapproved_or_broken_images_are_removed_completely() {
		let input = r#"
			before<img alt="missing">after
			<img src="https://tracker.example/pixel.gif" alt="tracker">
			<img src="//tracker.example/pixel.gif" alt="protocol tracker">
			<img src="data:image/png;base64,AAAA" alt="data">
			<img src="/vid/not-an-image.mp4" alt="video">
			<img src="/img/%2e%2e/private" alt="traversal">
			<img src="/img/%ZZ" alt="encoding">
			<img src="/static/desktop2x/img/renderTimingPixel.png" alt="unapproved static">
		"#;
		let output = sanitize_subreddit_info(input);
		assert_eq!(output.split_whitespace().collect::<String>(), "beforeafter");
		assert!(!output.contains("<img"));
	}

	#[test]
	fn community_icon_sources_never_leave_the_same_origin() {
		assert_eq!(sanitize_subreddit_image_source("https://i.redd.it/community.png"), "/img/community.png");
		assert_eq!(sanitize_subreddit_image_source("https://tracker.example/community.png"), "");
		assert_eq!(sanitize_subreddit_image_source("https://styles.redditmedia.com/community.png"), "");
		assert_eq!(sanitize_subreddit_image_source(""), "");
	}

	#[test]
	fn table_list_and_pre_attributes_are_canonical_and_bounded() {
		let long_code = "x".repeat(8192);
		let input = format!(
			r#"<ol start="+0007"><li>seven</li></ol><ol start="100001"><li>too far</li></ol>
			<table class="wide"><caption>Long table</caption><thead><tr><th scope="COL" colspan="02">Head</th><th scope="invalid">Bad</th></tr></thead><tbody><tr><td rowspan="3" colspan="101">Cell</td></tr></tbody></table>
			<pre style="width:99999px"><code data-language="x">{long_code}</code></pre>"#
		);
		let output = sanitize_subreddit_info(&input);
		assert!(output.contains(r#"<ol start="7">"#));
		assert!(output.contains("<ol><li>too far</li></ol>"));
		assert!(output.contains(r#"<th scope="col" colspan="2">Head</th>"#));
		assert!(output.contains("<th>Bad</th>"));
		assert!(output.contains(r#"<td rowspan="3">Cell</td>"#));
		assert!(output.contains(&long_code));
		for forbidden in ["class=", "style=", "data-language", "colspan=\"101\""] {
			assert!(!output.contains(forbidden));
		}
	}

	#[test]
	fn malformed_html_is_serialized_deterministically_and_empty_input_stays_empty() {
		let input = "<h3>Rules<h6>Deep<table><tr><td>one<td>two</table><pre>&lt;tag&gt;&amp;</pre><!-- comment -->";
		let first = sanitize_subreddit_info(input);
		let second = sanitize_subreddit_info(input);
		assert_eq!(first, second);
		assert!(!first.contains("<!--"));
		assert!(first.contains("<h2>Rules"));
		assert!(first.contains("<h3>Deep"));
		assert!(first.contains("<td>one</td><td>two</td>"));
		assert!(first.contains("<pre>&lt;tag&gt;&amp;</pre>"));
		assert_eq!(sanitize_subreddit_info(""), "");
	}

	#[test]
	fn long_text_and_source_titles_survive_without_source_presentation_attributes() {
		let long_title = "A community title ".repeat(400);
		let input = format!(r#"<div class="card"><p>{long_title}</p><a href="https://example.com" title="{long_title}" class="cta">Read</a></div>"#);
		let output = sanitize_subreddit_info(&input);
		assert!(output.contains(&format!("<p>{long_title}</p>")));
		assert!(output.contains(&format!("title=\"{long_title}\"")));
		assert!(!output.contains("class="));
	}

	#[test]
	fn archive_headings_shift_below_the_reader_outline_with_one_level_steps() {
		for (input, expected) in [
			("<h6>Only</h6>", "<h3>Only</h3>"),
			("<h1>One</h1><h6>Two</h6>", "<h3>One</h3><h4>Two</h4>"),
			("<h6>One</h6><h1>Two</h1>", "<h3>One</h3><h3>Two</h3>"),
			("<h1>One</h1><h4>Two</h4><h2>Three</h2><h6>Four</h6>", "<h3>One</h3><h4>Two</h4><h4>Three</h4><h5>Four</h5>"),
		] {
			assert_eq!(normalize_archive_comment_headings(input).unwrap(), expected);
		}
	}

	#[test]
	fn archive_heading_order_includes_lists_and_blockquotes() {
		let input = "<ul><li><h6>List</h6></li></ul><blockquote><h6>Quote</h6><h1>Up</h1></blockquote>";
		assert_eq!(
			normalize_archive_comment_headings(input).unwrap(),
			"<ul><li><h3>List</h3></li></ul><blockquote><h4>Quote</h4><h3>Up</h3></blockquote>"
		);
	}

	#[test]
	fn archive_heading_policy_preserves_every_non_heading_token() {
		let input = r#"<div class="md" style="color:red" data-x="1" onclick="kept()"><p title="<h1>">&lt;h2&gt;</p><!-- <h3> --><script>const sample = "<h4>";</script><img src="/img/a.png" onerror="kept()"><br></div>"#;
		assert_eq!(normalize_archive_comment_headings(input).unwrap(), input);
	}

	#[test]
	fn archive_heading_policy_fails_closed_on_ambiguous_markup() {
		let input = r#"<select><xmp><script>"use strict";</script></select>"#;
		assert!(normalize_archive_comment_headings(input).is_err());
	}
}
