use std::collections::HashMap;

// CRATES
use crate::account::{self, AccountSummary, AccountView, ArchiveBudgetSetting, ArchiveBudgetUpdate};
use crate::archive::{self, ArchiveQuotaSnapshot};
use crate::search;
use crate::server::ResponseExt;
use crate::subreddit::join_until_size_limit;
use crate::utils::{
	canonical_comment_keywords, canonical_theme, cookie_is_secure, deflate_decompress, encode_cookie_text, normalize_community_name, normalize_feed_name, parse_feed_groups,
	read_body_limited, redirect, safe_local_redirect, sanitize_feed_groups, see_other, serialize_feed_groups, template, FeedGroup, Preferences,
};
use askama::Template;
use cookie::{Cookie, SameSite};
use hyper::{Body, Request, Response, StatusCode};
use time::{Duration, OffsetDateTime};
use tokio::time::timeout;
use url::form_urlencoded;

// STRUCTS
#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
	prefs: Preferences,
	url: String,
	profile_mode: String,
	account: Option<AccountView>,
	accounts: Vec<AccountSummary>,
	account_status: String,
	hidden_post_count: usize,
	archive_quota: Option<ArchiveQuotaSnapshot>,
	archive_setting: ArchiveBudgetSetting,
	archive_budget_mode: String,
	archive_budget_value: String,
	archive_error: String,
	archive_notice: String,
	settings_saved: bool,
}

#[derive(Template)]
#[template(path = "subscriptions.html")]
struct SubscriptionsTemplate {
	prefs: Preferences,
	feed_groups: Vec<FeedGroup>,
	active_feed: String,
	unassigned: Vec<String>,
	url: String,
}

// CONSTANTS

const PREFS: [&str; 30] = [
	"theme",
	"front_page",
	"layout",
	"wide",
	"comment_sort",
	"collapse_child_comments",
	"post_sort",
	"blur_spoiler",
	"show_nsfw",
	"blur_nsfw",
	"use_hls",
	"hide_hls_notification",
	"autoplay_videos",
	"hide_sidebar_and_summary",
	"fixed_navbar",
	"hide_awards",
	"hide_score",
	"disable_visit_reddit_confirmation",
	"video_quality",
	"remove_default_feeds",
	"comment_filter_keywords",
	"feed_groups",
	"active_feed",
	"keyboard_navigation",
	"key_next_post",
	"key_previous_post",
	"key_open_post",
	"key_toggle_preview",
	"key_hide_post",
	"hide_post_behavior",
];

fn stored_preference_value(name: &str, value: &str) -> String {
	match name {
		"theme" => canonical_theme(value),
		"front_page" => "default".to_string(),
		"layout" => "compact".to_string(),
		"wide" | "fixed_navbar" | "remove_default_feeds" => "on".to_string(),
		"hide_sidebar_and_summary" => "off".to_string(),
		"comment_filter_keywords" => encode_cookie_text(&canonical_comment_keywords(value)),
		"feed_groups" => encode_cookie_text(&serialize_feed_groups(&parse_feed_groups(value))),
		"keyboard_navigation" => if value == "on" { "on" } else { "off" }.to_string(),
		"key_next_post" => canonical_shortcut(value, "j"),
		"key_previous_post" => canonical_shortcut(value, "k"),
		"key_open_post" => canonical_shortcut(value, "Enter"),
		"key_toggle_preview" => canonical_shortcut(value, "e"),
		"key_hide_post" => canonical_shortcut(value, "h"),
		"hide_post_behavior" => "instant".to_string(),
		_ => value.to_string(),
	}
}

fn persistent_cookie(name: String, value: String) -> Cookie<'static> {
	Cookie::build((name, value))
		.path("/")
		.http_only(true)
		.secure(cookie_is_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
		.into()
}

fn persist_subscriptions(response: &mut Response<Body>, cookies: &str, subscriptions: &[String]) {
	if subscriptions.is_empty() {
		response.remove_cookie("subscriptions".to_string());
		let mut number = 1;
		while cookies.contains(&format!("subscriptions{number}=")) {
			response.remove_cookie(format!("subscriptions{number}"));
			number += 1;
		}
		return;
	}

	let mut next_cookie = 0;
	for (number, list) in join_until_size_limit(subscriptions).into_iter().enumerate() {
		let name = if number == 0 { "subscriptions".to_string() } else { format!("subscriptions{number}") };
		response.insert_cookie(persistent_cookie(name, list));
		next_cookie += 1;
	}
	while cookies.contains(&format!("subscriptions{next_cookie}=")) {
		response.remove_cookie(format!("subscriptions{next_cookie}"));
		next_cookie += 1;
	}
}

// FUNCTIONS

/// Retrieve cookies from request "Cookie" header
pub async fn get(req: Request<Body>) -> Result<Response<Body>, String> {
	render_settings(&req, None, String::new(), StatusCode::OK)
}

fn render_settings(req: &Request<Body>, archive_draft: Option<(String, String)>, archive_error: String, status: StatusCode) -> Result<Response<Body>, String> {
	let url = req.uri().to_string();
	let settings_saved = settings_saved_status(req.uri().query());
	let account_status = req
		.uri()
		.query()
		.and_then(|query| {
			form_urlencoded::parse(query.as_bytes())
				.find(|(key, _)| key == "account")
				.map(|(_, value)| account_status_message(&value))
		})
		.unwrap_or_default();
	let archive_notice = req
		.uri()
		.query()
		.and_then(|query| {
			form_urlencoded::parse(query.as_bytes())
				.any(|(key, value)| key == "archive" && value == "updated")
				.then(|| "Archive storage budget updated.".to_string())
		})
		.unwrap_or_default();
	let archive_quota = account::server_backed(req).then(|| archive::quota_snapshot(req)).transpose()?;
	let archive_setting = if archive_quota.is_some() {
		account::archive_budget_setting(req)?
	} else {
		ArchiveBudgetSetting::default()
	};
	let default_custom = archive_budget_form_value(archive_setting, archive_quota.as_ref());
	let (archive_budget_mode, archive_budget_value) =
		archive_draft.unwrap_or_else(|| (if archive_setting.mib == 0 { "instance" } else { "custom" }.to_string(), default_custom.to_string()));
	let mut response = template(&SettingsTemplate {
		prefs: Preferences::new(req),
		url,
		profile_mode: account::mode().label().to_string(),
		account: account::current_account(req),
		accounts: account::accounts(req)?,
		account_status,
		hidden_post_count: account::hidden_post_count(req)?,
		archive_quota,
		archive_setting,
		archive_budget_mode,
		archive_budget_value,
		archive_error,
		archive_notice,
		settings_saved,
	});
	*response.status_mut() = status;
	Ok(response)
}

fn settings_saved_status(query: Option<&str>) -> bool {
	query
		.map(|query| form_urlencoded::parse(query.as_bytes()).any(|(key, value)| key == "saved" && value == "1"))
		.unwrap_or(false)
}

fn archive_budget_form_value(setting: ArchiveBudgetSetting, quota: Option<&ArchiveQuotaSnapshot>) -> u64 {
	let maximum = quota.map(|quota| quota.maximum_custom_mib).unwrap_or(2_048);
	if setting.mib == 0 {
		maximum.clamp(256, 2_048)
	} else if maximum >= 256 {
		setting.mib.min(maximum).max(256)
	} else {
		256
	}
}

pub async fn archive_storage(mut req: Request<Body>) -> Result<Response<Body>, String> {
	if !account::server_backed(&req) {
		return Ok(
			Response::builder()
				.status(StatusCode::BAD_REQUEST)
				.header("content-type", "text/plain; charset=utf-8")
				.body(Body::from("Archive storage budgets require a server-backed profile."))
				.unwrap_or_default(),
		);
	}
	let body = read_body_limited(req.body_mut(), 4 * 1024, "Archive storage form is too large").await?;
	let form = form_urlencoded::parse(&body)
		.map(|(key, value)| (key.into_owned(), value.into_owned()))
		.collect::<HashMap<_, _>>();
	let mode = form.get("archive_budget_mode").cloned().unwrap_or_default();
	let entered = form.get("archive_budget_mib").cloned().unwrap_or_default();
	let revision = form.get("archive_budget_revision").and_then(|value| value.parse::<i64>().ok());
	let quota = archive::quota_snapshot(&req)?;
	let budget = validate_archive_budget_form(&mode, &entered, &quota);
	let budget = match budget {
		Ok(budget) => budget,
		Err(error) => return render_settings(&req, Some((mode, entered)), error, StatusCode::UNPROCESSABLE_ENTITY),
	};
	let Some(revision) = revision.filter(|revision| *revision >= 0) else {
		return render_settings(
			&req,
			Some((mode, entered)),
			"The archive setting changed or the form expired. Review the current value and try again.".to_string(),
			StatusCode::CONFLICT,
		);
	};
	match account::update_archive_budget(&req, budget, revision)? {
		ArchiveBudgetUpdate::Saved(_) => Ok(see_other("/settings?archive=updated#archive-storage")),
		ArchiveBudgetUpdate::Conflict(_) => render_settings(
			&req,
			Some((mode, entered)),
			"Archive storage changed in another request. No value was overwritten; review the current metrics and submit again.".to_string(),
			StatusCode::CONFLICT,
		),
	}
}

fn validate_archive_budget_form(mode: &str, entered: &str, quota: &ArchiveQuotaSnapshot) -> Result<u64, String> {
	match mode {
		"instance" => Ok(0),
		"custom" if !quota.custom_budget_available => Err("This instance maximum is below 256 MiB, so a separate profile budget is unavailable.".to_string()),
		"custom" => match entered.parse::<u64>() {
			Ok(value) if value >= 256 && value % 256 == 0 && value <= quota.maximum_custom_mib => Ok(value),
			_ => Err(format!("Enter a whole 256 MiB step from 256 through {}.", quota.maximum_custom_mib)),
		},
		_ => Err("Choose whether to use the instance maximum or set a profile budget.".to_string()),
	}
}

fn account_status_message(code: &str) -> String {
	match code {
		"password-changed" => "Password changed and existing sessions were rotated.",
		"current-password" => "The current password was not accepted.",
		"password-mismatch" => "The password confirmation did not match.",
		"password-length" => "Passwords must contain between 12 and 128 characters.",
		"username-invalid" => "Use 3–32 letters, numbers, periods, underscores, or hyphens for the username.",
		"display-name-invalid" => "Display names must be 64 characters or fewer.",
		"username-exists" => "That username is already in use.",
		"user-created" => "The account and its independent profile were created.",
		"user-enabled" => "The account was enabled.",
		"user-disabled" => "The account was disabled and its sessions were revoked.",
		"password-reset" => "The account password was reset and its sessions were revoked.",
		"self-disable" => "You cannot disable the account you are currently using.",
		"last-admin" => "The final enabled administrator cannot be disabled.",
		"user-missing" => "That account no longer exists.",
		"use-own-password-form" => "Use the personal password form to change your own password.",
		_ => "",
	}
	.to_string()
}

/// Render the focused community-management page used by the Vale shell.
pub async fn subscriptions(req: Request<Body>) -> Result<Response<Body>, String> {
	let url = req.uri().to_string();
	let prefs = Preferences::new(&req);
	let feed_groups = prefs.feed_groups();
	let grouped = feed_groups
		.iter()
		.flat_map(|group| group.communities.iter().map(|community| community.to_lowercase()))
		.collect::<std::collections::HashSet<_>>();
	let unassigned = prefs
		.subscriptions
		.iter()
		.filter(|community| !grouped.contains(&community.to_lowercase()))
		.cloned()
		.collect();
	let active_feed = prefs.active_feed_group().map(|group| group.slug).unwrap_or_default();
	Ok(template(&SubscriptionsTemplate {
		prefs,
		feed_groups,
		active_feed,
		unassigned,
		url,
	}))
}

/// Preserve old bookmarks while keeping community discovery in Search.
pub async fn legacy_discover(req: Request<Body>) -> Result<Response<Body>, String> {
	Ok(see_other(&legacy_discover_target(req.uri().query())))
}

fn legacy_discover_target(query: Option<&str>) -> String {
	let query = strict_last_discover_query(query).unwrap_or_default();
	search::canonical_community_search_url(&query)
}

fn strict_last_discover_query(query: Option<&str>) -> Option<String> {
	let mut candidate = None;
	for pair in query.unwrap_or_default().split('&') {
		let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
		if strict_form_component(key).ok().as_deref() == Some("q") {
			candidate = Some(strict_form_component(value));
		}
	}
	let decoded = candidate?.ok()?;
	if decoded.contains('\u{fffd}') || decoded.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
		return None;
	}
	let normalized = search::normalize_search_query(&decoded);
	if normalized.is_empty() || normalized.len() > 512 {
		return None;
	}
	Some(normalized)
}

fn strict_form_component(value: &str) -> Result<String, ()> {
	fn hex(byte: u8) -> Option<u8> {
		match byte {
			b'0'..=b'9' => Some(byte - b'0'),
			b'a'..=b'f' => Some(byte - b'a' + 10),
			b'A'..=b'F' => Some(byte - b'A' + 10),
			_ => None,
		}
	}

	let input = value.as_bytes();
	let mut decoded = Vec::with_capacity(input.len());
	let mut index = 0;
	while index < input.len() {
		match input[index] {
			b'+' => decoded.push(b' '),
			b'%' => {
				let high = input.get(index + 1).copied().and_then(hex).ok_or(())?;
				let low = input.get(index + 2).copied().and_then(hex).ok_or(())?;
				decoded.push((high << 4) | low);
				index += 2;
			}
			byte => decoded.push(byte),
		}
		index += 1;
	}
	String::from_utf8(decoded).map_err(|_| ())
}

/// Set cookies using response "Set-Cookie" header
pub async fn set(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let mut prefs = Preferences::new(&req);
	let server_backed = account::server_backed(&req);
	let body_bytes = read_body_limited(req.body_mut(), 64 * 1024, "Settings form is too large").await?;
	let form = url::form_urlencoded::parse(&body_bytes)
		.map(|(key, value)| (key.into_owned(), value.into_owned()))
		.collect::<HashMap<_, _>>();

	let mut response = see_other("/settings?saved=1#preferences");
	if server_backed {
		apply_profile_form(&mut prefs, &form);
		account::save_preferences(&req, &prefs)?;
		if let Some(active_feed) = form.get("active_feed").filter(|value| prefs.feed_groups().iter().any(|group| &group.slug == *value)) {
			response.insert_cookie(account::active_feed_cookie(active_feed.clone()));
		}
		return Ok(response);
	}

	for &name in &PREFS {
		match form.get(name) {
			Some(value) => response.insert_cookie(persistent_cookie(name.to_owned(), stored_preference_value(name, value))),
			None => response.remove_cookie(name.to_string()),
		};
	}

	Ok(response)
}

fn apply_profile_form(prefs: &mut Preferences, form: &HashMap<String, String>) {
	let value = |name: &str| form.get(name).cloned().unwrap_or_default();
	prefs.theme = canonical_theme(&value("theme"));
	prefs.comment_sort = value("comment_sort");
	prefs.collapse_child_comments = value("collapse_child_comments");
	prefs.post_sort = value("post_sort");
	prefs.blur_spoiler = value("blur_spoiler");
	prefs.show_nsfw = value("show_nsfw");
	prefs.blur_nsfw = value("blur_nsfw");
	prefs.use_hls = value("use_hls");
	prefs.hide_hls_notification = value("hide_hls_notification");
	prefs.autoplay_videos = value("autoplay_videos");
	prefs.hide_awards = value("hide_awards");
	prefs.hide_score = value("hide_score");
	prefs.disable_visit_reddit_confirmation = value("disable_visit_reddit_confirmation");
	prefs.video_quality = value("video_quality");
	prefs.comment_filter_keywords = canonical_comment_keywords(&value("comment_filter_keywords"));
	prefs.keyboard_navigation = if value("keyboard_navigation") == "on" { "on".to_string() } else { "off".to_string() };
	prefs.key_next_post = canonical_shortcut(&value("key_next_post"), "j");
	prefs.key_previous_post = canonical_shortcut(&value("key_previous_post"), "k");
	prefs.key_open_post = canonical_shortcut(&value("key_open_post"), "Enter");
	prefs.key_toggle_preview = canonical_shortcut(&value("key_toggle_preview"), "e");
	prefs.key_hide_post = canonical_shortcut(&value("key_hide_post"), "h");
	prefs.hide_post_behavior = "instant".to_string();
	prefs.apply_reader_defaults();
}

fn canonical_shortcut(value: &str, fallback: &str) -> String {
	let value = value.trim();
	if value.is_empty() || value.chars().count() > 32 || value.chars().any(char::is_control) {
		fallback.to_string()
	} else {
		value.to_string()
	}
}

pub async fn manage_feeds(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let mut prefs = Preferences::new(&req);
	let server_backed = account::server_backed(&req);
	let cookies = req.headers().get("cookie").and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
	let body = read_body_limited(req.body_mut(), 64 * 1024, "Feed form is too large")
		.await
		.map_err(|error| format!("Failed to read feed form: {error}"))?;
	let form = url::form_urlencoded::parse(&body).collect::<HashMap<_, _>>();
	let intent = form.get("intent").map_or("", |value| value.as_ref());
	let feed_slug = form.get("feed_slug").map_or("", |value| value.as_ref());
	let return_to = safe_local_redirect(form.get("return_to").map_or("", |value| value.as_ref()), "/feeds", 2_048);

	let mut groups = prefs.feed_groups();
	let mut active_feed = prefs.active_feed_group().map(|group| group.slug).unwrap_or_default();
	let mut subscriptions = prefs.subscriptions.clone();

	match intent {
		"create" => {
			if let Some(name) = form.get("feed_name").and_then(|value| normalize_feed_name(value)) {
				let communities = if groups.is_empty() { subscriptions.clone() } else { Vec::new() };
				groups.push(FeedGroup {
					name,
					slug: String::new(),
					communities,
				});
				groups = sanitize_feed_groups(&groups);
				if let Some(created) = groups.last() {
					active_feed = created.slug.clone();
				}
			}
		}
		"rename" => {
			if let Some(name) = form.get("feed_name").and_then(|value| normalize_feed_name(value)) {
				if let Some(index) = groups.iter().position(|group| group.slug == feed_slug) {
					let was_active = active_feed == groups[index].slug;
					groups[index].name = name;
					groups = sanitize_feed_groups(&groups);
					if was_active {
						active_feed = groups.get(index).map(|group| group.slug.clone()).unwrap_or_default();
					}
				}
			}
		}
		"delete" => {
			groups.retain(|group| group.slug != feed_slug);
			groups = sanitize_feed_groups(&groups);
			if active_feed == feed_slug {
				active_feed = groups.first().map(|group| group.slug.clone()).unwrap_or_default();
			}
		}
		"add" => {
			if let Some(community) = form.get("community").and_then(|value| normalize_community_name(value)) {
				if let Some(target) = groups.iter().position(|group| group.slug == feed_slug) {
					for group in &mut groups {
						group.communities.retain(|existing| !existing.eq_ignore_ascii_case(&community));
					}
					groups[target].communities.push(community.clone());
					if !subscriptions.iter().any(|existing| existing.eq_ignore_ascii_case(&community)) {
						subscriptions.push(community);
						subscriptions.sort_by_key(|item| item.to_lowercase());
					}
				}
				groups = sanitize_feed_groups(&groups);
			}
		}
		"remove" => {
			if let Some(community) = form.get("community").and_then(|value| normalize_community_name(value)) {
				if let Some(group) = groups.iter_mut().find(|group| group.slug == feed_slug) {
					group.communities.retain(|existing| !existing.eq_ignore_ascii_case(&community));
				}
			}
		}
		"activate" if groups.iter().any(|group| group.slug == feed_slug) => {
			active_feed = feed_slug.to_string();
		}
		_ => {}
	}

	groups = sanitize_feed_groups(&groups);
	if !groups.iter().any(|group| group.slug == active_feed) {
		active_feed = groups.first().map(|group| group.slug.clone()).unwrap_or_default();
	}

	let mut response = redirect(&return_to);
	if server_backed {
		prefs.feed_groups = serialize_feed_groups(&groups);
		prefs.subscriptions = subscriptions;
		account::save_preferences(&req, &prefs)?;
		if groups.is_empty() {
			response.remove_cookie("active_feed".to_string());
		} else {
			response.insert_cookie(account::active_feed_cookie(active_feed));
		}
		return Ok(response);
	}
	if groups.is_empty() {
		response.remove_cookie("feed_groups".to_string());
		response.remove_cookie("active_feed".to_string());
	} else {
		response.insert_cookie(persistent_cookie("feed_groups".to_string(), encode_cookie_text(&serialize_feed_groups(&groups))));
		response.insert_cookie(persistent_cookie("active_feed".to_string(), active_feed));
	}
	persist_subscriptions(&mut response, &cookies, &subscriptions);
	Ok(response)
}

fn set_cookies_method(req: Request<Body>, remove_cookies: bool) -> Response<Body> {
	// Split the body into parts
	let (parts, _) = req.into_parts();

	// Grab existing cookies
	let _cookies: Vec<Cookie<'_>> = parts
		.headers
		.get_all("Cookie")
		.iter()
		.filter_map(|header| Cookie::parse(header.to_str().unwrap_or_default()).ok())
		.collect();

	let query = parts.uri.query().unwrap_or_default().as_bytes();

	let form = url::form_urlencoded::parse(query).collect::<HashMap<_, _>>();

	let candidate = match form.get("redirect") {
		Some(value) => {
			let value = value.replace("%26", "&").replace("%23", "#");
			if value.starts_with('/') {
				value
			} else {
				format!("/{value}")
			}
		}
		None => "/".to_string(),
	};
	let path = safe_local_redirect(&candidate, "/", 2_048);

	let mut response = redirect(&path);

	for name in PREFS {
		match form.get(name) {
			Some(value) => response.insert_cookie(persistent_cookie(name.to_owned(), stored_preference_value(name, value))),
			None => {
				if remove_cookies {
					response.remove_cookie(name.to_string());
				}
			}
		};
	}

	// Get subscriptions/filters to restore from query string
	let subscriptions = form.get("subscriptions");
	let filters = form.get("filters");

	// We can't search through the cookies directly like in subreddit.rs, so instead we have to make a string out of the request's headers to search through
	let cookies_string = parts
		.headers
		.get("cookie")
		.map(|hv| hv.to_str().unwrap_or("").to_string()) // Return String
		.unwrap_or_else(String::new); // Return an empty string if None

	// If there are subscriptions to restore set them and delete any old subscriptions cookies, otherwise delete them all
	if let Some(subscriptions) = subscriptions {
		let sub_list: Vec<String> = subscriptions.split('+').map(str::to_string).collect();

		// Start at 0 to keep track of what number we need to start deleting old subscription cookies from
		let mut subscriptions_number_to_delete_from = 0;

		// Starting at 0 so we handle the subscription cookie without a number first
		for (subscriptions_number, list) in join_until_size_limit(&sub_list).into_iter().enumerate() {
			let subscriptions_cookie = if subscriptions_number == 0 {
				"subscriptions".to_string()
			} else {
				format!("subscriptions{subscriptions_number}")
			};

			response.insert_cookie(
				Cookie::build((subscriptions_cookie, list))
					.path("/")
					.http_only(true)
					.secure(cookie_is_secure())
					.same_site(SameSite::Lax)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			);

			subscriptions_number_to_delete_from += 1;
		}

		// While subscriptionsNUMBER= is in the string of cookies add a response removing that cookie
		while cookies_string.contains(&format!("subscriptions{subscriptions_number_to_delete_from}=")) {
			// Remove that subscriptions cookie
			response.remove_cookie(format!("subscriptions{subscriptions_number_to_delete_from}"));

			// Increment subscriptions cookie number
			subscriptions_number_to_delete_from += 1;
		}
	} else {
		// Remove unnumbered subscriptions cookie
		response.remove_cookie("subscriptions".to_string());

		// Starts at one to deal with the first numbered subscription cookie and onwards
		let mut subscriptions_number_to_delete_from = 1;

		// While subscriptionsNUMBER= is in the string of cookies add a response removing that cookie
		while cookies_string.contains(&format!("subscriptions{subscriptions_number_to_delete_from}=")) {
			// Remove that subscriptions cookie
			response.remove_cookie(format!("subscriptions{subscriptions_number_to_delete_from}"));

			// Increment subscriptions cookie number
			subscriptions_number_to_delete_from += 1;
		}
	}

	// If there are filters to restore set them and delete any old filters cookies, otherwise delete them all
	if let Some(filters) = filters {
		let filters_list: Vec<String> = filters.split('+').map(str::to_string).collect();

		// Start at 0 to keep track of what number we need to start deleting old subscription cookies from
		let mut filters_number_to_delete_from = 0;

		// Starting at 0 so we handle the subscription cookie without a number first
		for (filters_number, list) in join_until_size_limit(&filters_list).into_iter().enumerate() {
			let filters_cookie = if filters_number == 0 {
				"filters".to_string()
			} else {
				format!("filters{filters_number}")
			};

			response.insert_cookie(
				Cookie::build((filters_cookie, list))
					.path("/")
					.http_only(true)
					.secure(cookie_is_secure())
					.same_site(SameSite::Lax)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			);

			filters_number_to_delete_from += 1;
		}

		// While filtersNUMBER= is in the string of cookies add a response removing that cookie
		while cookies_string.contains(&format!("filters{filters_number_to_delete_from}=")) {
			// Remove that filters cookie
			response.remove_cookie(format!("filters{filters_number_to_delete_from}"));

			// Increment filters cookie number
			filters_number_to_delete_from += 1;
		}
	} else {
		// Remove unnumbered filters cookie
		response.remove_cookie("filters".to_string());

		// Starts at one to deal with the first numbered subscription cookie and onwards
		let mut filters_number_to_delete_from = 1;

		// While filtersNUMBER= is in the string of cookies add a response removing that cookie
		while cookies_string.contains(&format!("filters{filters_number_to_delete_from}=")) {
			// Remove that sfilters cookie
			response.remove_cookie(format!("filters{filters_number_to_delete_from}"));

			// Increment filters cookie number
			filters_number_to_delete_from += 1;
		}
	}

	response
}

/// Set cookies using response "Set-Cookie" header
pub async fn restore(req: Request<Body>) -> Result<Response<Body>, String> {
	if account::server_backed(&req) {
		return Ok(redirect("/settings"));
	}
	Ok(set_cookies_method(req, true))
}

pub async fn update(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let mut prefs = Preferences::new(&req);
	let server_backed = account::server_backed(&req);
	let body = read_body_limited(req.body_mut(), 8 * 1024, "Quick settings form is too large")
		.await
		.map_err(|error| format!("Failed to read quick settings form: {error}"))?;
	let form = form_urlencoded::parse(&body)
		.map(|(key, value)| (key.into_owned(), value.into_owned()))
		.collect::<HashMap<_, _>>();
	let redirect_to = safe_local_redirect(form.get("redirect").map_or("", String::as_str), "/", 2_048);
	let mut response = redirect(&redirect_to);
	for name in ["use_hls", "hide_hls_notification", "show_nsfw"] {
		if let Some(value) = form.get(name) {
			if !matches!(value.as_str(), "on" | "off") {
				continue;
			}
			if server_backed {
				match name {
					"use_hls" => prefs.use_hls = value.clone(),
					"hide_hls_notification" => prefs.hide_hls_notification = value.clone(),
					"show_nsfw" => prefs.show_nsfw = value.clone(),
					_ => {}
				}
			} else {
				response.insert_cookie(persistent_cookie(name.to_string(), value.clone()));
			}
		}
	}
	if server_backed {
		account::save_preferences(&req, &prefs)?;
	}
	Ok(response)
}

pub async fn encoded_restore(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let server_backed = account::server_backed(&req);
	let body = read_body_limited(req.body_mut(), 1024 * 1024, "Request body too large").await?;

	let encoded_prefs = form_urlencoded::parse(&body)
		.find(|(key, _)| key == "encoded_prefs")
		.map(|(_, value)| value)
		.ok_or_else(|| "encoded_prefs parameter not found in request body".to_string())?;

	let bytes = base2048::decode(&encoded_prefs).ok_or_else(|| "Failed to decode base2048 encoded preferences".to_string())?;

	let out = timeout(std::time::Duration::from_secs(1), async { deflate_decompress(bytes) })
		.await
		.map_err(|e| format!("Failed to decompress bytes: {e}"))??;

	let mut prefs: Preferences = timeout(std::time::Duration::from_secs(1), async { Preferences::from_bincode(&out) })
		.await
		.map_err(|e| format!("Failed to deserialize preferences: {e}"))?
		.map_err(|e| format!("Failed to deserialize bytes into Preferences struct: {e}"))?;

	prefs.available_themes = vec![];
	prefs.feed_groups = serialize_feed_groups(&prefs.feed_groups());
	prefs.comment_filter_keywords = canonical_comment_keywords(&prefs.comment_filter_keywords);
	prefs.apply_reader_defaults();
	if server_backed {
		let quota = archive::quota_snapshot(&req)?;
		if prefs.archive_budget_mib != 0 && prefs.archive_budget_mib > quota.maximum_custom_mib {
			return Err(format!(
				"The imported archive budget exceeds this instance's {} MiB custom maximum. No settings were changed.",
				quota.maximum_custom_mib
			));
		}
		account::restore_preferences(&req, &prefs)?;
		let mut response = redirect("/settings");
		if !prefs.active_feed.is_empty() && prefs.feed_groups().iter().any(|group| group.slug == prefs.active_feed) {
			response.insert_cookie(account::active_feed_cookie(prefs.active_feed));
		}
		return Ok(response);
	}

	let url = format!("/settings/restore/?{}", prefs.to_urlencoded()?);

	Ok(redirect(&url))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn archive_budget_form_validates_native_numeric_contract() {
		let quota = ArchiveQuotaSnapshot {
			maximum_custom_mib: 2_048,
			custom_budget_available: true,
			..ArchiveQuotaSnapshot::default()
		};
		assert_eq!(validate_archive_budget_form("instance", "not-a-number", &quota).unwrap(), 0);
		assert_eq!(validate_archive_budget_form("custom", "256", &quota).unwrap(), 256);
		assert_eq!(validate_archive_budget_form("custom", "2048", &quota).unwrap(), 2_048);
		for invalid in ["", "0", "255", "300", "2304", "-256", "256.0"] {
			assert!(validate_archive_budget_form("custom", invalid, &quota).is_err(), "{invalid} should be rejected");
		}
		assert!(validate_archive_budget_form("unknown", "256", &quota).is_err());
	}

	#[test]
	fn archive_budget_form_disables_custom_mode_below_one_step() {
		let quota = ArchiveQuotaSnapshot {
			maximum_custom_mib: 0,
			custom_budget_available: false,
			..ArchiveQuotaSnapshot::default()
		};
		assert_eq!(validate_archive_budget_form("instance", "", &quota).unwrap(), 0);
		assert!(validate_archive_budget_form("custom", "256", &quota).unwrap_err().contains("below 256 MiB"));
	}

	#[test]
	fn archive_budget_form_reflects_the_stored_custom_value() {
		let quota = ArchiveQuotaSnapshot {
			maximum_custom_mib: 2_048,
			custom_budget_available: true,
			..ArchiveQuotaSnapshot::default()
		};
		assert_eq!(archive_budget_form_value(ArchiveBudgetSetting { mib: 512, revision: 1 }, Some(&quota)), 512);
		assert_eq!(archive_budget_form_value(ArchiveBudgetSetting::default(), Some(&quota)), 2_048);
		let lowered = ArchiveQuotaSnapshot {
			maximum_custom_mib: 256,
			custom_budget_available: true,
			..ArchiveQuotaSnapshot::default()
		};
		assert_eq!(archive_budget_form_value(ArchiveBudgetSetting { mib: 1_024, revision: 2 }, Some(&lowered)), 256);
	}

	#[test]
	fn archive_budget_success_uses_prg_with_the_storage_anchor() {
		let response = see_other("/settings?archive=updated#archive-storage");
		assert_eq!(response.status(), StatusCode::SEE_OTHER);
		assert_eq!(response.headers().get("location").unwrap(), "/settings?archive=updated#archive-storage");
	}

	#[test]
	fn main_settings_success_status_requires_exact_saved_value() {
		assert!(settings_saved_status(Some("saved=1")));
		assert!(settings_saved_status(Some("other=value&saved=1")));
		for query in [None, Some(""), Some("saved"), Some("saved=01"), Some("saved=true"), Some("Saved=1")] {
			assert!(!settings_saved_status(query));
		}
		let response = see_other("/settings?saved=1#preferences");
		assert_eq!(response.status(), StatusCode::SEE_OTHER);
		assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
		assert_eq!(response.headers().get("location").unwrap(), "/settings?saved=1#preferences");
	}

	#[test]
	fn legacy_discover_uses_the_canonical_community_search_builder() {
		let bare = "/search?scope=all&sort=relevance&type=sr_user";
		assert_eq!(legacy_discover_target(None), bare);
		assert_eq!(legacy_discover_target(Some("")), bare);
		assert_eq!(legacy_discover_target(Some("not_q=ignored")), bare);
		assert_eq!(
			legacy_discover_target(Some("q=home+automation")),
			"/search?q=home+automation&scope=all&sort=relevance&type=sr_user"
		);
		assert_eq!(
			legacy_discover_target(Some("q=first&ignored=%ZZ&%71=last")),
			"/search?q=last&scope=all&sort=relevance&type=sr_user"
		);
		assert_eq!(legacy_discover_target(Some("q=valid&q=%ZZ")), bare);
	}

	#[test]
	fn legacy_discover_encodes_search_text_without_treating_it_as_a_destination() {
		for (query, encoded) in [
			("q=C%2B%2B", "C%2B%2B"),
			("q=%2F%2Fevil.example", "%2F%2Fevil.example"),
			("q=https%3A%2F%2Fexample.com%2Fpath", "https%3A%2F%2Fexample.com%2Fpath"),
			("q=folder%5Cname", "folder%5Cname"),
			("q=%E2%98%83+weather", "%E2%98%83+weather"),
		] {
			assert_eq!(legacy_discover_target(Some(query)), format!("/search?q={encoded}&scope=all&sort=relevance&type=sr_user"));
		}
		assert_eq!(
			legacy_discover_target(Some("q=https%3A%2F%2Fwww.reddit.com%2Fr%2Fselfhosted")),
			"/search?q=r%2Fselfhosted&scope=all&sort=relevance&type=sr_user"
		);
	}

	#[test]
	fn legacy_discover_rejects_invalid_or_oversized_queries_without_truncating() {
		let bare = "/search?scope=all&sort=relevance&type=sr_user";
		for query in ["q=", "q=+++", "q=%", "q=%0", "q=%GG", "q=%FF", "q=%EF%BF%BD", "q=%00topic", "q=topic%7F"] {
			assert_eq!(legacy_discover_target(Some(query)), bare, "{query}");
		}
		let maximum = format!("q={}", "a".repeat(512));
		let oversized = format!("q={}", "a".repeat(513));
		assert!(legacy_discover_target(Some(&maximum)).contains(&format!("q={}", "a".repeat(512))));
		assert_eq!(legacy_discover_target(Some(&oversized)), bare);
	}

	#[tokio::test]
	async fn legacy_discover_handler_is_a_no_store_see_other() {
		let request = Request::builder().uri("/discover?q=rust&extra=ignored").body(Body::empty()).unwrap();
		let response = legacy_discover(request).await.unwrap();
		assert_eq!(response.status(), StatusCode::SEE_OTHER);
		assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
		assert_eq!(response.headers().get("location").unwrap(), "/search?q=rust&scope=all&sort=relevance&type=sr_user");
	}

	#[test]
	fn reviewed_ui_static_and_template_contracts_remain_exact() {
		let style = include_str!("../static/style.css");
		let interactions = include_str!("../static/vale-interactions.js");
		let service_worker = include_str!("../static/service-worker.js");
		let base = include_str!("../templates/base.html");
		let post = include_str!("../templates/post.html");
		let subreddit = include_str!("../templates/subreddit.html");
		let history = include_str!("../templates/history.html");
		let settings = include_str!("../templates/settings.html");
		let subscriptions = include_str!("../templates/subscriptions.html");

		assert!(style.contains("#main-content:focus-visible { outline: 0; }"));
		assert!(style.contains("outline: 2px solid color-mix(in srgb, var(--focus) 35%, transparent)"));
		assert!(style.contains(".mobile-feed-context.is-pinned:not([hidden]) { background: var(--shell-solid); -webkit-backdrop-filter: none; backdrop-filter: none; }"));
		assert!(style.contains(".feed-page.community-page > .community-rail"));
		assert!(!style.contains("feed-switcher-compact"));
		assert!(!style.contains("discover-page"));
		assert!(!style.contains("discover-card"));

		assert_eq!(subreddit.matches("<h1>").count(), 1);
		assert_eq!(subreddit.matches("class=\"feed-switcher feed-switcher-wide\"").count(), 1);
		assert!(subreddit.contains("class=\"feed-switcher feed-switcher-wide\" aria-label=\"Feed selection\""));
		assert!(!subreddit.contains("Choose a feed"));
		assert!(!style.contains("feed-switcher-label"));
		assert!(subreddit.contains("<div class=\"mobile-feed-context\" hidden aria-hidden=\"true\">"));
		assert!(subreddit.contains("<aside class=\"community-rail\""));
		assert!(subreddit.contains("<details class=\"community-about\">"));
		assert!(!subreddit.contains("feed-switcher-compact"));

		let exact_history =
			"\t\t<article class=\"history-item\">\n\t\t\t<div class=\"history-item-copy\">\n\t\t\t\t<h2 class=\"history-heading\">\n\t\t\t\t\t<a class=\"history-title\"";
		assert!(history.contains(exact_history));
		assert!(!history.contains(">Open</a>"));

		assert!(settings.contains("<form id=\"preferences-form\" action=\"/settings\" method=\"POST\">"));
		assert!(settings.contains("<div class=\"prefs\" id=\"preferences\">"));
		assert!(settings.contains("<p class=\"settings-saved-status\" role=\"status\">Settings saved.</p>"));
		assert!(settings.contains("formaction=\"/hidden/clear\" formmethod=\"POST\""));
		assert!(settings.contains("onclick=\"return confirm('Return every hidden post to your feeds?')\""));
		let native_save = settings.find("<input id=\"save\" type=\"submit\"").unwrap();
		let bar = settings.find("data-settings-save-bar hidden").unwrap();
		assert!(native_save < bar);
		assert!(settings.contains("</section>\n</div>\n\n<div class=\"settings-save-bar\" data-settings-save-bar hidden>"));
		assert!(interactions.contains("const bar = document.querySelector(\"[data-settings-save-bar]\");"));

		assert!(post.contains("id=\"commentSortSelect\" data-comment-sort"));
		assert!(!post.contains("aria-label=\"Apply comment sort\""));
		assert!(post.contains("<form id=\"comment-search-form\" role=\"search\" aria-label=\"Search comments\">"));
		assert!(post.contains("maxlength=\"160\" enterkeyhint=\"search\" aria-describedby=\"comment-search-help\""));
		assert!(!post.contains("<button type=\"submit\" class=\"quiet-button\">Search</button>"));
		assert!(interactions.contains("const commentSortSelect = event.target.closest(\"[data-comment-sort]\");"));
		assert!(interactions.contains("if (commentSortSelect) commentSortSelect.form?.requestSubmit();"));
		assert!(interactions.contains("if (event.key !== \"Enter\" || event.isComposing) return;"));
		assert!(interactions.contains("event.currentTarget.form?.requestSubmit();"));
		assert!(!style.contains("#comment-search-form .quiet-button"));

		assert!(subscriptions.contains("href=\"/search?scope=all&amp;sort=relevance&amp;type=sr_user\">Find a community</a>"));
		assert!(interactions.contains("const NAVIGATION_STATE_VERSION = 3;"));
		assert!(service_worker.contains("const CACHE = \"vale-v97-static\";"));
		assert!(service_worker.contains("request.headers.get(\"X-Vale-Fragment\") === \"posts-v1\""));
		assert!(base.contains("-vale-v78"));
		assert!(base.contains("-v55"));
	}
}
