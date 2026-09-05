use crate::listing::{self, FragmentMode, ListingPolicy, ListingStatus, PostRenderKind};
use crate::utils::{self, catch_random, error, format_num, format_url, get_filters, param, see_other, template, val, Post, Preferences, LISTING_PAGE_SIZE};
use crate::{
	client::json,
	server::RequestExt,
	subreddit::{can_access_quarantine, quarantine},
};
use askama::Template;
use hyper::{Body, Request, Response};
use regex::Regex;
use std::sync::LazyLock;

const SEARCH_SORTS: [&str; 5] = ["relevance", "hot", "top", "new", "comments"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchScope {
	Feed,
	All,
}

impl SearchScope {
	fn as_str(self) -> &'static str {
		match self {
			Self::Feed => "feed",
			Self::All => "all",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchResultMode {
	Posts,
	Communities,
	Invalid,
}

struct SearchParams {
	q: String,
	sort: String,
	t: String,
	typed: String,
	scope: String,
	scope_label: String,
	feed: String,
	restrict_sr: String,
	has_query: bool,
}

struct Subreddit {
	name: String,
	url: String,
	icon: String,
	description: String,
	subscribers: (String, String),
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
	posts: Vec<Post>,
	subreddits: Vec<Subreddit>,
	sub: String,
	params: SearchParams,
	prefs: Preferences,
	url: String,
	previous_url: String,
	next_url: String,
	feed_scope_url: String,
	all_scope_url: String,
	active_feed: String,
	active_feed_name: String,
	feed_empty: bool,
	/// Whether the subreddit itself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	listing_status: String,
	visible_count: usize,
}

/// Regex matched against search queries to determine if they are Reddit URLs.
static REDDIT_URL_MATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://([^\./]+\.)*reddit.com/").unwrap());

pub async fn find(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	if req.param("sub").is_some() {
		find_community(req, prefs).await
	} else {
		find_named(req, prefs).await
	}
}

async fn find_named(req: Request<Body>, mut prefs: Preferences) -> Result<Response<Body>, String> {
	let fragment_mode = match listing::fragment_mode(&req) {
		Ok(mode) => mode,
		Err(response) => return Ok(response),
	};
	let url = req.uri().path_and_query().map_or("/search", |value| value.as_str()).to_string();
	let result_mode = search_result_mode(&url);
	if result_mode == SearchResultMode::Invalid {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("That search result type is not fragment-eligible."));
		}
		return Ok(listing::document_response(error(req, "That search result type does not exist.").await?));
	}
	if fragment_mode == FragmentMode::Posts && result_mode != SearchResultMode::Posts {
		return Ok(listing::fragment_route_rejection("Community search results do not provide post fragments."));
	}
	let query = normalized_query(&url);
	if let Some(target) = search_shortcut(&query) {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("Search shortcuts do not provide authoritative post fragments."));
		}
		return Ok(listing::document_response(see_other(&target)));
	}

	let scope = match param(&url, "scope").as_deref() {
		None | Some("") | Some("feed") => SearchScope::Feed,
		Some("all") => SearchScope::All,
		Some(_) => {
			if fragment_mode == FragmentMode::Posts {
				return Ok(listing::fragment_route_rejection("That search scope is not fragment-eligible."));
			}
			return Ok(listing::document_response(error(req, "That search scope does not exist.").await?));
		}
	};
	let sort = param(&url, "sort").unwrap_or_else(|| "relevance".to_string());
	if !SEARCH_SORTS.contains(&sort.as_str()) {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("That search sort is not fragment-eligible."));
		}
		return Ok(listing::document_response(error(req, "That search sort does not exist.").await?));
	}
	let timeframe = param(&url, "t").unwrap_or_else(|| "all".to_string());
	let typed = match result_mode {
		SearchResultMode::Posts if param(&url, "type").as_deref() == Some("link") => "link".to_string(),
		SearchResultMode::Communities if scope == SearchScope::All => "sr_user".to_string(),
		_ => String::new(),
	};

	let requested_feed = param(&url, "feed").unwrap_or_default();
	let feed_group = if scope == SearchScope::Feed {
		if requested_feed.is_empty() {
			prefs.active_feed_group()
		} else {
			prefs.feed_groups().into_iter().find(|group| group.slug == requested_feed)
		}
	} else {
		None
	};
	if scope == SearchScope::Feed && !requested_feed.is_empty() && feed_group.is_none() {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("That named feed no longer exists."));
		}
		return Ok(listing::document_response(error(req, "That named feed no longer exists.").await?));
	}
	if let Some(group) = feed_group.as_ref() {
		prefs.active_feed = group.slug.clone();
	}
	let active_group = prefs.active_feed_group();
	let active_feed = active_group.as_ref().map(|group| group.slug.clone()).unwrap_or_default();
	let active_feed_name = active_group.as_ref().map(|group| group.name.clone()).unwrap_or_else(|| "No active feed".to_string());
	let scoped_feed = feed_group.as_ref().map(|group| group.slug.as_str()).unwrap_or_default();

	let cursor = query_cursor(&url);
	let canonical_url = canonical_named_search_url(
		&query,
		scope,
		scoped_feed,
		&sort,
		param(&url, "t").as_deref(),
		&typed,
		cursor.as_ref().map(|(name, value)| (name.as_str(), value.as_str())),
	);
	if !query.is_empty() && url != canonical_url {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("Use the canonical search URL before requesting a fragment."));
		}
		return Ok(listing::document_response(see_other(&canonical_url)));
	}

	let feed_scope_url = canonical_named_search_url(&query, SearchScope::Feed, &active_feed, &sort, param(&url, "t").as_deref(), "", None);
	let all_scope_url = canonical_named_search_url(&query, SearchScope::All, "", &sort, param(&url, "t").as_deref(), "", None);
	let scope_label = match (scope, feed_group.as_ref()) {
		(SearchScope::Feed, Some(group)) => group.name.clone(),
		(SearchScope::Feed, None) => "No active feed".to_string(),
		(SearchScope::All, _) => "All Reddit".to_string(),
	};

	let base_template =
		|posts: Vec<Post>, subreddits, previous_url, next_url, feed_empty, is_filtered, all_posts_filtered, all_posts_hidden_nsfw, no_posts, listing_status: ListingStatus| {
			let visible_count = posts.len();
			SearchTemplate {
				posts,
				subreddits,
				sub: String::new(),
				params: SearchParams {
					q: query.clone(),
					sort: sort.clone(),
					t: timeframe.clone(),
					typed: typed.clone(),
					scope: scope.as_str().to_string(),
					scope_label: scope_label.clone(),
					feed: scoped_feed.to_string(),
					restrict_sr: String::new(),
					has_query: !query.is_empty(),
				},
				prefs: prefs.clone(),
				url: url.clone(),
				previous_url,
				next_url,
				feed_scope_url: feed_scope_url.clone(),
				all_scope_url: all_scope_url.clone(),
				active_feed: active_feed.clone(),
				active_feed_name: active_feed_name.clone(),
				feed_empty,
				is_filtered,
				all_posts_filtered,
				all_posts_hidden_nsfw,
				no_posts,
				listing_status: listing_status.as_str().to_string(),
				visible_count,
			}
		};

	if query.is_empty() {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				prefs.clone(),
				url.clone(),
				PostRenderKind::Search,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		return Ok(listing::document_response(template(&base_template(
			Vec::new(),
			Vec::new(),
			String::new(),
			String::new(),
			false,
			false,
			false,
			false,
			false,
			ListingStatus::End,
		))));
	}
	let communities = feed_group.as_ref().map(|group| group.communities.clone()).unwrap_or_default();
	if scope == SearchScope::Feed && communities.is_empty() {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				prefs.clone(),
				url.clone(),
				PostRenderKind::Search,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		return Ok(listing::document_response(template(&base_template(
			Vec::new(),
			Vec::new(),
			String::new(),
			String::new(),
			true,
			false,
			false,
			false,
			true,
			ListingStatus::End,
		))));
	}

	let filters = get_filters(&req);
	let all_requested_filtered = scope == SearchScope::Feed && communities.iter().all(|community| filters.contains(community));
	let mut subreddits = if scope == SearchScope::All && fragment_mode == FragmentMode::Document {
		let mut results = search_subreddits(&query, &typed).await;
		results.retain(|subreddit| !filters.contains(subreddit.name.as_str()));
		results
	} else {
		Vec::new()
	};
	if all_requested_filtered {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				prefs.clone(),
				url.clone(),
				PostRenderKind::Search,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		return Ok(listing::document_response(template(&base_template(
			Vec::new(),
			subreddits,
			String::new(),
			String::new(),
			false,
			true,
			false,
			false,
			false,
			ListingStatus::End,
		))));
	}

	let base_path = if scope == SearchScope::Feed {
		format!("/r/{}/search.json", communities.join("+").replace('+', "%2B"))
	} else {
		"/search.json".to_string()
	};
	let reddit_query = reddit_search_query(&query, &sort, &timeframe, &typed, cursor.as_ref(), scope == SearchScope::Feed, &prefs);
	let path = format!("{base_path}?{reddit_query}");
	let policy = match ListingPolicy::for_request(&req, (scope == SearchScope::Feed).then(|| communities.clone()), false, false) {
		Ok(policy) => policy,
		Err(_) => return listing::policy_unavailable_response(req, fragment_mode).await,
	};
	match listing::accumulate(&path, scope == SearchScope::Feed, policy).await {
		Ok(mut result) => {
			crate::activity::annotate(&req, &mut result.posts)?;
			let previous_url = result.previous_url(&canonical_url);
			let next_url = result.next_url(&canonical_url);
			let all_posts_filtered = result.all_posts_filtered();
			let all_posts_hidden_nsfw = result.all_posts_hidden_nsfw();
			let no_posts = result.no_posts();
			let status = result.status;
			if fragment_mode == FragmentMode::Posts {
				return listing::render_posts_fragment(
					result.posts,
					prefs.clone(),
					url.clone(),
					PostRenderKind::Search,
					active_feed.clone(),
					previous_url,
					next_url,
					status,
				);
			}
			Ok(listing::document_response(template(&base_template(
				result.posts,
				std::mem::take(&mut subreddits),
				previous_url,
				next_url,
				false,
				false,
				all_posts_filtered,
				all_posts_hidden_nsfw,
				no_posts,
				status,
			))))
		}
		Err(message) => {
			if fragment_mode == FragmentMode::Posts {
				Ok(listing::fragment_unavailable_response())
			} else {
				Ok(listing::document_response(error(req, &message).await?))
			}
		}
	}
}

async fn find_community(req: Request<Body>, prefs: Preferences) -> Result<Response<Body>, String> {
	let fragment_mode = match listing::fragment_mode(&req) {
		Ok(mode) => mode,
		Err(response) => return Ok(response),
	};
	let url = req.uri().path_and_query().map_or("/search", |value| value.as_str()).to_string();
	let result_mode = search_result_mode(&url);
	if result_mode == SearchResultMode::Invalid {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("That search result type is not fragment-eligible."));
		}
		return Ok(listing::document_response(error(req, "That search result type does not exist.").await?));
	}
	if fragment_mode == FragmentMode::Posts && result_mode != SearchResultMode::Posts {
		return Ok(listing::fragment_route_rejection("Community search results do not provide post fragments."));
	}
	let query = normalized_query(&url);
	if let Some(target) = search_shortcut(&query) {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("Search shortcuts do not provide authoritative post fragments."));
		}
		return Ok(listing::document_response(see_other(&target)));
	}
	let sub = req.param("sub").unwrap_or_default();
	if fragment_mode == FragmentMode::Posts && (sub.eq_ignore_ascii_case("random") || sub.eq_ignore_ascii_case("randnsfw")) {
		return Ok(listing::fragment_route_rejection("Random destinations do not provide authoritative post fragments."));
	}
	if let Ok(random) = catch_random(&sub, "/find").await {
		return Ok(listing::document_response(random));
	}
	let sort = param(&url, "sort").unwrap_or_else(|| "relevance".to_string());
	if !SEARCH_SORTS.contains(&sort.as_str()) {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("That search sort is not fragment-eligible."));
		}
		return Ok(listing::document_response(error(req, "That search sort does not exist.").await?));
	}
	let timeframe = param(&url, "t").unwrap_or_else(|| "all".to_string());
	let typed = match result_mode {
		SearchResultMode::Posts if param(&url, "type").as_deref() == Some("link") => "link".to_string(),
		SearchResultMode::Communities => "sr_user".to_string(),
		_ => String::new(),
	};
	let restrict_sr = param(&url, "restrict_sr").unwrap_or_default();
	let active_group = prefs.active_feed_group();
	let active_feed = active_group.as_ref().map(|group| group.slug.clone()).unwrap_or_default();
	let active_feed_name = active_group.as_ref().map(|group| group.name.clone()).unwrap_or_else(|| "No active feed".to_string());
	let feed_scope_url = canonical_named_search_url(&query, SearchScope::Feed, &active_feed, &sort, param(&url, "t").as_deref(), "", None);
	let all_scope_url = canonical_named_search_url(&query, SearchScope::All, "", &sort, param(&url, "t").as_deref(), "", None);
	let cursor = query_cursor(&url);

	let make_template =
		|posts: Vec<Post>, subreddits, previous_url, next_url, is_filtered, all_posts_filtered, all_posts_hidden_nsfw, no_posts, listing_status: ListingStatus| {
			let visible_count = posts.len();
			SearchTemplate {
				posts,
				subreddits,
				sub: sub.clone(),
				params: SearchParams {
					q: query.clone(),
					sort: sort.clone(),
					t: timeframe.clone(),
					typed: typed.clone(),
					scope: "community".to_string(),
					scope_label: format!("r/{sub}"),
					feed: String::new(),
					restrict_sr: restrict_sr.clone(),
					has_query: !query.is_empty(),
				},
				prefs: prefs.clone(),
				url: url.clone(),
				previous_url,
				next_url,
				feed_scope_url: feed_scope_url.clone(),
				all_scope_url: all_scope_url.clone(),
				active_feed: active_feed.clone(),
				active_feed_name: active_feed_name.clone(),
				feed_empty: false,
				is_filtered,
				all_posts_filtered,
				all_posts_hidden_nsfw,
				no_posts,
				listing_status: listing_status.as_str().to_string(),
				visible_count,
			}
		};
	if query.is_empty() {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				prefs.clone(),
				url.clone(),
				PostRenderKind::Search,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		return Ok(listing::document_response(template(&make_template(
			Vec::new(),
			Vec::new(),
			String::new(),
			String::new(),
			false,
			false,
			false,
			false,
			ListingStatus::End,
		))));
	}

	let filters = get_filters(&req);
	let mut subreddits = if restrict_sr.is_empty() && fragment_mode == FragmentMode::Document {
		let mut results = search_subreddits(&query, &typed).await;
		results.retain(|subreddit| !filters.contains(subreddit.name.as_str()));
		results
	} else {
		Vec::new()
	};
	if filters.contains(&sub) {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				prefs.clone(),
				url.clone(),
				PostRenderKind::Search,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		return Ok(listing::document_response(template(&make_template(
			Vec::new(),
			subreddits,
			String::new(),
			String::new(),
			true,
			false,
			false,
			false,
			ListingStatus::End,
		))));
	}

	let reddit_query = reddit_search_query(&query, &sort, &timeframe, &typed, cursor.as_ref(), !restrict_sr.is_empty(), &prefs);
	let path = format!("/r/{}/search.json?{reddit_query}", sub.replace('+', "%2B"));
	let quarantined = can_access_quarantine(&req, &sub);
	let allowed_communities = (!restrict_sr.is_empty()).then(|| sub.split('+').map(str::to_string).collect::<Vec<_>>());
	let policy = match ListingPolicy::for_request(&req, allowed_communities, false, false) {
		Ok(policy) => policy,
		Err(_) => return listing::policy_unavailable_response(req, fragment_mode).await,
	};
	match listing::accumulate(&path, quarantined, policy).await {
		Ok(mut result) => {
			crate::activity::annotate(&req, &mut result.posts)?;
			let previous_url = result.previous_url(&url);
			let next_url = result.next_url(&url);
			let all_posts_filtered = result.all_posts_filtered();
			let all_posts_hidden_nsfw = result.all_posts_hidden_nsfw();
			let no_posts = result.no_posts();
			let status = result.status;
			if fragment_mode == FragmentMode::Posts {
				return listing::render_posts_fragment(
					result.posts,
					prefs.clone(),
					url.clone(),
					PostRenderKind::Search,
					active_feed.clone(),
					previous_url,
					next_url,
					status,
				);
			}
			Ok(listing::document_response(template(&make_template(
				result.posts,
				std::mem::take(&mut subreddits),
				previous_url,
				next_url,
				false,
				all_posts_filtered,
				all_posts_hidden_nsfw,
				no_posts,
				status,
			))))
		}
		Err(message) => {
			if fragment_mode == FragmentMode::Posts {
				Ok(listing::fragment_unavailable_response())
			} else if matches!(message.as_str(), "quarantined" | "gated") {
				Ok(listing::document_response(quarantine(&req, sub, &message)))
			} else {
				Ok(listing::document_response(error(req, &message).await?))
			}
		}
	}
}

fn normalized_query(url: &str) -> String {
	let query = param(url, "q").unwrap_or_default();
	normalize_search_query(&query)
}

pub(crate) fn normalize_search_query(query: &str) -> String {
	REDDIT_URL_MATCH.replace(query.trim(), "").trim().to_string()
}

fn search_shortcut(query: &str) -> Option<String> {
	if query.starts_with("r/") || query.starts_with("user/") {
		Some(format!("/{query}"))
	} else if query.starts_with("R/") {
		Some(format!("/r{}", &query[1..]))
	} else if query.starts_with("u/") || query.starts_with("U/") {
		Some(format!("/user{}", &query[1..]))
	} else {
		None
	}
}

fn query_cursor(url: &str) -> Option<(String, String)> {
	param(url, "after")
		.filter(|value| !value.is_empty())
		.map(|value| ("after".to_string(), value))
		.or_else(|| param(url, "before").filter(|value| !value.is_empty()).map(|value| ("before".to_string(), value)))
}

/// Classify every raw `type` occurrence before rendering either a document or
/// fragment. This keeps enhanced document hooks and fragment eligibility in
/// lockstep, and prevents duplicate or future mixed modes from silently
/// falling back to link/post results.
fn search_result_mode(url: &str) -> SearchResultMode {
	let Ok(parsed) = url::Url::parse(&format!("https://vale.invalid{url}")) else {
		return SearchResultMode::Invalid;
	};
	let values = parsed
		.query_pairs()
		.filter_map(|(key, value)| (key == "type").then(|| value.into_owned()))
		.collect::<Vec<_>>();
	match values.as_slice() {
		[] => SearchResultMode::Posts,
		[value] if value.is_empty() || value == "link" => SearchResultMode::Posts,
		[value] if value == "sr_user" => SearchResultMode::Communities,
		_ => SearchResultMode::Invalid,
	}
}

fn canonical_named_search_url(query: &str, scope: SearchScope, feed: &str, sort: &str, timeframe: Option<&str>, typed: &str, cursor: Option<(&str, &str)>) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	if !query.is_empty() {
		serializer.append_pair("q", query);
	}
	serializer.append_pair("scope", scope.as_str());
	if scope == SearchScope::Feed && !feed.is_empty() {
		serializer.append_pair("feed", feed);
	}
	serializer.append_pair("sort", sort);
	if sort != "new" {
		if let Some(timeframe) = timeframe.filter(|value| !value.is_empty()) {
			serializer.append_pair("t", timeframe);
		}
	}
	if typed == "link" || (typed == "sr_user" && scope == SearchScope::All) {
		serializer.append_pair("type", typed);
	}
	if let Some((name, value)) = cursor.filter(|(name, value)| matches!(*name, "after" | "before") && !value.is_empty()) {
		serializer.append_pair(name, value);
	}
	format!("/search?{}", serializer.finish())
}

pub(crate) fn canonical_community_search_url(query: &str) -> String {
	canonical_named_search_url(query, SearchScope::All, "", "relevance", None, "sr_user", None)
}

fn reddit_search_query(query: &str, sort: &str, timeframe: &str, typed: &str, cursor: Option<&(String, String)>, restrict_sr: bool, prefs: &Preferences) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	serializer.append_pair("q", query);
	serializer.append_pair("sort", sort);
	if sort != "new" && !timeframe.is_empty() {
		serializer.append_pair("t", timeframe);
	}
	if !typed.is_empty() {
		serializer.append_pair("type", typed);
	}
	if let Some((name, value)) = cursor {
		serializer.append_pair(name, value);
	}
	if restrict_sr {
		serializer.append_pair("restrict_sr", "on");
	}
	if prefs.show_nsfw == "on" && !utils::sfw_only() {
		serializer.append_pair("include_over_18", "on");
	}
	serializer.append_pair("limit", &LISTING_PAGE_SIZE.to_string());
	serializer.append_pair("raw_json", "1");
	serializer.finish()
}

pub fn membership_label(prefs: &Preferences, active_feed: &str, community: &str) -> String {
	let groups = prefs.feed_groups();
	let membership = groups.iter().find(|group| group.communities.iter().any(|member| member.eq_ignore_ascii_case(community)));
	match membership {
		Some(group) if group.slug == active_feed => format!("In {}", group.name),
		Some(group) => format!("In {}", group.name),
		None => "Outside your feeds".to_string(),
	}
}

async fn search_subreddits(query: &str, typed: &str) -> Vec<Subreddit> {
	let limit = if typed == "sr_user" { "50" } else { "3" };
	let encoded = subreddit_search_query(query, limit);
	let subreddit_search_path = format!("/subreddits/search.json?{encoded}");

	json(subreddit_search_path, false).await.unwrap_or_default()["data"]["children"]
		.as_array()
		.map(ToOwned::to_owned)
		.unwrap_or_default()
		.iter()
		.map(|subreddit| {
			let icon = subreddit["data"]["community_icon"].as_str().map_or_else(|| val(subreddit, "icon_img"), ToString::to_string);
			Subreddit {
				name: val(subreddit, "display_name"),
				url: val(subreddit, "url"),
				icon: format_url(&icon),
				description: val(subreddit, "public_description"),
				subscribers: format_num(subreddit["data"]["subscribers"].as_f64().unwrap_or_default() as i64),
			}
		})
		.collect()
}

fn subreddit_search_query(query: &str, limit: &str) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	serializer.append_pair("q", query);
	serializer.append_pair("limit", limit);
	serializer.finish()
}

#[cfg(test)]
mod tests {
	use super::{canonical_named_search_url, find, reddit_search_query, search_result_mode, SearchResultMode, SearchScope};
	use crate::server::RequestExt;
	use crate::utils::Preferences;
	use hyper::{body::to_bytes, header, Body, Request, StatusCode};
	use route_recognizer::Params;

	fn route_request(path: &str, sub: Option<&str>, fragment: bool) -> Request<Body> {
		let mut builder = Request::builder().uri(path);
		if fragment {
			builder = builder.header("X-Vale-Fragment", "posts-v1");
		}
		let mut request = builder.body(Body::empty()).unwrap();
		if let Some(sub) = sub {
			let mut params = Params::new();
			params.insert("sub".to_string(), sub.to_string());
			request.set_params(params);
		}
		request
	}

	fn assert_private_variant(response: &hyper::Response<Body>) {
		assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store");
		assert_eq!(response.headers()[header::VARY], "X-Vale-Fragment");
	}

	#[test]
	fn named_search_urls_make_scope_and_feed_explicit() {
		assert_eq!(
			canonical_named_search_url("llama", SearchScope::Feed, "ai-homelab", "relevance", None, "", None),
			"/search?q=llama&scope=feed&feed=ai-homelab&sort=relevance"
		);
		assert_eq!(
			canonical_named_search_url("llama", SearchScope::All, "ignored", "relevance", None, "", None),
			"/search?q=llama&scope=all&sort=relevance"
		);
	}

	#[test]
	fn reddit_search_query_excludes_vale_route_state_and_caps_results() {
		let prefs = Preferences::default();
		let query = reddit_search_query("llama", "relevance", "all", "", None, true, &prefs);
		assert_eq!(query, "q=llama&sort=relevance&t=all&restrict_sr=on&limit=25&raw_json=1");
		assert!(!query.contains("scope="));
		assert!(!query.contains("feed="));
	}

	#[test]
	fn raw_search_types_are_classified_before_rendering() {
		for posts in ["/search?q=rust", "/search?q=rust&type=", "/search?q=rust&type=link", "/r/rust/search?q=async&type=link"] {
			assert_eq!(search_result_mode(posts), SearchResultMode::Posts, "{posts}");
		}
		assert_eq!(search_result_mode("/search?q=rust&type=sr_user"), SearchResultMode::Communities);
		for invalid in [
			"/search?q=rust&type=comment",
			"/search?q=rust&type=mixed",
			"/search?q=rust&type=link&type=comment",
			"/search?q=rust&type=link&type=link",
		] {
			assert_eq!(search_result_mode(invalid), SearchResultMode::Invalid, "{invalid}");
		}
	}

	#[tokio::test]
	async fn community_search_invalid_type_is_never_an_enhanced_document_or_fragment() {
		for path in ["/r/rust/search?q=async&type=comment", "/r/rust/search?q=async&type=link&type=link"] {
			let document = find(route_request(path, Some("rust"), false)).await.unwrap();
			assert_eq!(document.status(), StatusCode::NOT_FOUND, "{path}");
			assert_private_variant(&document);
			let body = String::from_utf8(to_bytes(document.into_body()).await.unwrap().to_vec()).unwrap();
			assert!(!body.contains("data-vale-listing=\"posts-v1\""), "{path}");

			let fragment = find(route_request(path, Some("rust"), true)).await.unwrap();
			assert_eq!(fragment.status(), StatusCode::BAD_REQUEST, "{path}");
			assert_private_variant(&fragment);
			assert!(fragment.headers().get("X-Vale-Fragment").is_none(), "{path}");
			let body = String::from_utf8(to_bytes(fragment.into_body()).await.unwrap().to_vec()).unwrap();
			assert!(!body.contains("<html"), "{path}");
			assert!(!body.contains("data-vale-posts-fragment"), "{path}");
		}
	}

	#[tokio::test]
	async fn subreddit_result_search_is_document_only_and_has_no_post_listing_hooks() {
		let path = "/r/rust/search?type=sr_user";
		let document = find(route_request(path, Some("rust"), false)).await.unwrap();
		assert_eq!(document.status(), StatusCode::OK);
		assert_private_variant(&document);
		let body = String::from_utf8(to_bytes(document.into_body()).await.unwrap().to_vec()).unwrap();
		assert!(!body.contains("data-vale-listing=\"posts-v1\""));

		let fragment = find(route_request(path, Some("rust"), true)).await.unwrap();
		assert_eq!(fragment.status(), StatusCode::BAD_REQUEST);
		assert_private_variant(&fragment);
		assert!(fragment.headers().get("X-Vale-Fragment").is_none());
	}

	#[tokio::test]
	async fn fragment_search_rejects_early_sort_and_canonical_redirect_outcomes() {
		let invalid_sort = find(route_request("/r/rust/search?q=async&sort=old", Some("rust"), true)).await.unwrap();
		assert_eq!(invalid_sort.status(), StatusCode::BAD_REQUEST);
		assert_private_variant(&invalid_sort);
		assert!(invalid_sort.headers().get("X-Vale-Fragment").is_none());

		for alias in ["random", "randnsfw", "RaNdOm"] {
			let path = format!("/r/{alias}/search?q=async");
			let response = find(route_request(&path, Some(alias), true)).await.unwrap();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{alias}");
			assert_private_variant(&response);
			assert!(response.headers().get("X-Vale-Fragment").is_none(), "{alias}");
		}

		let noncanonical = find(route_request("/search?q=async", None, true)).await.unwrap();
		assert_eq!(noncanonical.status(), StatusCode::BAD_REQUEST);
		assert_private_variant(&noncanonical);
		assert!(noncanonical.headers().get("X-Vale-Fragment").is_none());

		let document = find(route_request("/search?q=async", None, false)).await.unwrap();
		assert_eq!(document.status(), StatusCode::SEE_OTHER);
		assert_eq!(document.headers()[header::LOCATION], "/search?q=async&scope=feed&sort=relevance");
		assert_private_variant(&document);
	}
}
