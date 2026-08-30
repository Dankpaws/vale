//! Bounded, vacancy-replacing post listings.
//!
//! A render may inspect at most four upstream pages of 25 raw records. The
//! resulting snapshot is authoritative for at most 25 visible top-level post
//! representatives; it is deliberately not an infinite-scroll protocol.

use crate::{
	account,
	utils::{error, get_filters, grouped_post, listing_cursor_url, refresh_group_metadata, ListingPage, Post, Preferences, LISTING_PAGE_SIZE},
};
use askama::Template;
use hyper::{header, Body, Request, Response, StatusCode};
use std::collections::{HashMap, HashSet};
use url::Url;

pub const MAX_LISTING_REQUESTS: usize = 4;
pub const MAX_LISTING_RAW_RECORDS: usize = MAX_LISTING_REQUESTS * LISTING_PAGE_SIZE;
pub const POSTS_FRAGMENT_VERSION: &str = "posts-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListingStatus {
	Complete,
	End,
	Retry,
}

impl ListingStatus {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Complete => "complete",
			Self::End => "end",
			Self::Retry => "retry",
		}
	}
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListingDiagnostics {
	pub raw_records: usize,
	pub post_records: usize,
	pub membership_filtered: usize,
	pub profile_filtered: usize,
	pub hidden: usize,
	pub nsfw_filtered: usize,
	pub invalid_post_ids: usize,
	pub duplicate_post_ids: usize,
}

#[derive(Clone, Debug)]
pub struct ListingPolicy {
	filters: HashSet<String>,
	hidden_ids: HashSet<String>,
	allowed_communities: Option<HashSet<String>>,
	show_nsfw: bool,
	group_exact_content: bool,
	sort_new_with_stickies: bool,
}

impl ListingPolicy {
	pub fn for_request(request: &Request<Body>, allowed_communities: Option<Vec<String>>, group_exact_content: bool, sort_new_with_stickies: bool) -> Result<Self, String> {
		Ok(Self {
			filters: get_filters(request),
			hidden_ids: account::hidden_post_ids_for_listing(request)?,
			allowed_communities: allowed_communities.map(|communities| communities.into_iter().map(|community| community.to_ascii_lowercase()).collect::<HashSet<_>>()),
			show_nsfw: Preferences::new(request).show_nsfw == "on" && !crate::utils::sfw_only(),
			group_exact_content,
			sort_new_with_stickies,
		})
	}

	#[cfg(test)]
	fn test_default() -> Self {
		Self {
			filters: HashSet::new(),
			hidden_ids: HashSet::new(),
			allowed_communities: None,
			show_nsfw: true,
			group_exact_content: false,
			sort_new_with_stickies: false,
		}
	}
}

pub struct ListingResult {
	pub posts: Vec<Post>,
	pub previous_cursor: String,
	pub next_cursor: String,
	pub status: ListingStatus,
	pub diagnostics: ListingDiagnostics,
	pub raw_fullnames: Vec<String>,
	pub page_fingerprints: Vec<String>,
	pub requests: usize,
}

impl ListingResult {
	pub fn previous_url(&self, current_url: &str) -> String {
		listing_cursor_url(current_url, "before", &self.previous_cursor)
	}

	pub fn next_url(&self, current_url: &str) -> String {
		listing_cursor_url(current_url, "after", &self.next_cursor)
	}

	pub fn all_posts_filtered(&self) -> bool {
		self.diagnostics.post_records > 0 && self.diagnostics.profile_filtered == self.diagnostics.post_records.saturating_sub(self.diagnostics.membership_filtered)
	}

	pub fn all_posts_hidden_nsfw(&self) -> bool {
		let after_profile = self
			.diagnostics
			.post_records
			.saturating_sub(self.diagnostics.membership_filtered)
			.saturating_sub(self.diagnostics.profile_filtered)
			.saturating_sub(self.diagnostics.hidden)
			.saturating_sub(self.diagnostics.invalid_post_ids)
			.saturating_sub(self.diagnostics.duplicate_post_ids);
		after_profile > 0 && self.diagnostics.nsfw_filtered == after_profile
	}

	pub fn no_posts(&self) -> bool {
		self.posts.is_empty() && !self.all_posts_filtered() && !self.all_posts_hidden_nsfw()
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageDecision {
	Continue,
	Complete,
	End,
	Retry,
}

struct ListingAccumulator {
	policy: ListingPolicy,
	posts: Vec<Post>,
	by_content: HashMap<String, usize>,
	seen_fullnames: HashSet<String>,
	seen_post_ids: HashSet<String>,
	seen_fingerprints: HashSet<String>,
	seen_after_cursors: HashSet<String>,
	previous_cursor: String,
	next_cursor: String,
	continuation_cursor: String,
	target_boundary: String,
	target_has_trailing_raw: bool,
	diagnostics: ListingDiagnostics,
	raw_fullnames: Vec<String>,
	page_fingerprints: Vec<String>,
	requests: usize,
}

impl ListingAccumulator {
	fn new(policy: ListingPolicy) -> Self {
		Self {
			policy,
			posts: Vec::with_capacity(LISTING_PAGE_SIZE),
			by_content: HashMap::new(),
			seen_fullnames: HashSet::with_capacity(MAX_LISTING_RAW_RECORDS),
			seen_post_ids: HashSet::with_capacity(MAX_LISTING_RAW_RECORDS),
			seen_fingerprints: HashSet::with_capacity(MAX_LISTING_REQUESTS),
			seen_after_cursors: HashSet::with_capacity(MAX_LISTING_REQUESTS),
			previous_cursor: String::new(),
			next_cursor: String::new(),
			continuation_cursor: String::new(),
			target_boundary: String::new(),
			target_has_trailing_raw: false,
			diagnostics: ListingDiagnostics::default(),
			raw_fullnames: Vec::with_capacity(MAX_LISTING_RAW_RECORDS),
			page_fingerprints: Vec::with_capacity(MAX_LISTING_REQUESTS),
			requests: 0,
		}
	}

	fn accept_page(&mut self, mut page: ListingPage, requested_after: &str) -> PageDecision {
		self.requests = self.requests.saturating_add(1);
		if self.requests == 1 {
			self.previous_cursor = page.cursors.before.clone();
		}

		if page.posts.len() != page.raw_fullnames.len()
			|| page.posts.len() > LISTING_PAGE_SIZE
			|| page.fingerprint.is_empty()
			|| !self.seen_fingerprints.insert(page.fingerprint.clone())
		{
			return PageDecision::Retry;
		}
		self.page_fingerprints.push(page.fingerprint);

		if !requested_after.is_empty() && page.cursors.after == requested_after {
			return PageDecision::Retry;
		}
		if !page.cursors.after.is_empty() && !self.seen_after_cursors.insert(page.cursors.after.clone()) {
			return PageDecision::Retry;
		}
		self.continuation_cursor = page.cursors.after.clone();

		let mut new_fullnames = 0usize;
		for (mut post, raw_fullname) in page.posts.drain(..).zip(page.raw_fullnames.drain(..)) {
			self.diagnostics.raw_records = self.diagnostics.raw_records.saturating_add(1);
			self.raw_fullnames.push(raw_fullname.clone());
			if !self.target_boundary.is_empty() {
				self.target_has_trailing_raw = true;
			}
			if raw_fullname.is_empty() || !self.seen_fullnames.insert(raw_fullname.clone()) {
				continue;
			}
			new_fullnames = new_fullnames.saturating_add(1);
			post.fullname = raw_fullname;

			// Every enhanced route is a homogeneous top-level post listing. A
			// comment or subreddit record therefore cannot become a card.
			if post.title.is_empty() || !post.fullname.starts_with("t3_") {
				continue;
			}
			self.diagnostics.post_records = self.diagnostics.post_records.saturating_add(1);

			if self
				.policy
				.allowed_communities
				.as_ref()
				.is_some_and(|communities| !communities.contains(&post.community.to_ascii_lowercase()))
			{
				self.diagnostics.membership_filtered = self.diagnostics.membership_filtered.saturating_add(1);
				continue;
			}
			if self.policy.filters.contains(&post.community) || self.policy.filters.contains(&format!("u_{}", post.author.name)) {
				self.diagnostics.profile_filtered = self.diagnostics.profile_filtered.saturating_add(1);
				continue;
			}
			if !valid_post_id(&post.id) {
				self.diagnostics.invalid_post_ids = self.diagnostics.invalid_post_ids.saturating_add(1);
				continue;
			}
			if self.policy.hidden_ids.contains(&post.id) {
				self.diagnostics.hidden = self.diagnostics.hidden.saturating_add(1);
				continue;
			}
			if post.flags.nsfw && !self.policy.show_nsfw {
				self.diagnostics.nsfw_filtered = self.diagnostics.nsfw_filtered.saturating_add(1);
				continue;
			}
			if !self.seen_post_ids.insert(post.id.clone()) {
				self.diagnostics.duplicate_post_ids = self.diagnostics.duplicate_post_ids.saturating_add(1);
				continue;
			}

			if self.policy.group_exact_content && !post.content_key.is_empty() {
				if let Some(index) = self.by_content.get(&post.content_key).copied() {
					let representative = &mut self.posts[index];
					merge_group_member(representative, post, self.policy.sort_new_with_stickies);
					continue;
				}
			}

			// Once the target is full, later unique records remain unconsumed.
			// We still scan the already-fetched page so later exact duplicates can
			// patch the authoritative group membership without taking a slot.
			if self.posts.len() >= LISTING_PAGE_SIZE {
				continue;
			}
			if self.policy.group_exact_content && !post.content_key.is_empty() {
				self.by_content.insert(post.content_key.clone(), self.posts.len());
			}
			let boundary = post.fullname.clone();
			self.posts.push(post);
			if self.posts.len() == LISTING_PAGE_SIZE {
				self.target_boundary = boundary;
			}
		}

		if new_fullnames == 0 && (self.diagnostics.raw_records > 0 || !page.cursors.after.is_empty()) {
			return PageDecision::Retry;
		}
		if !self.target_boundary.is_empty() {
			let has_more_raw = self.target_has_trailing_raw || !page.cursors.after.is_empty();
			if has_more_raw {
				self.next_cursor = self.target_boundary.clone();
			}
			if !self.policy.group_exact_content {
				return if has_more_raw {
					PageDecision::Complete
				} else {
					self.next_cursor.clear();
					PageDecision::End
				};
			}
			// Continue through the bounded raw-record budget even after card 25
			// is elected. Unique tail posts stay unconsumed, while later exact
			// duplicates can still complete the representative's group.
			if !page.cursors.after.is_empty() {
				return PageDecision::Continue;
			}
			return if self.target_has_trailing_raw {
				PageDecision::Complete
			} else {
				self.next_cursor.clear();
				PageDecision::End
			};
		}
		if page.cursors.after.is_empty() {
			self.next_cursor.clear();
			return PageDecision::End;
		}
		self.next_cursor = page.cursors.after;
		PageDecision::Continue
	}

	fn finish_budget(mut self) -> ListingResult {
		if self.target_boundary.is_empty() {
			self.next_cursor.clear();
			self.finish(ListingStatus::Retry)
		} else {
			self.next_cursor = self.target_boundary.clone();
			self.finish(ListingStatus::Complete)
		}
	}

	fn finish_retry(mut self) -> ListingResult {
		self.next_cursor.clear();
		self.finish(ListingStatus::Retry)
	}

	fn finish(mut self, status: ListingStatus) -> ListingResult {
		if self.policy.sort_new_with_stickies {
			// Keep the existing stable New behavior: newest first within the
			// stickied and ordinary partitions, with stickied representatives first.
			self.posts.sort_by_key(|post| std::cmp::Reverse(post.created_ts));
			self.posts.sort_by_key(|post| std::cmp::Reverse(post.flags.stickied));
		}
		ListingResult {
			posts: self.posts,
			previous_cursor: self.previous_cursor,
			next_cursor: self.next_cursor,
			status,
			diagnostics: self.diagnostics,
			raw_fullnames: self.raw_fullnames,
			page_fingerprints: self.page_fingerprints,
			requests: self.requests,
		}
	}
}

/// Fetch and accumulate a complete bounded listing snapshot.
pub async fn accumulate(path: &str, quarantine: bool, policy: ListingPolicy) -> Result<ListingResult, String> {
	let mut accumulator = ListingAccumulator::new(policy);
	let mut request_path = path.to_string();
	let mut requested_after = query_value(path, "after").unwrap_or_default();

	for request_index in 0..MAX_LISTING_REQUESTS {
		let page = match Post::fetch_page(&request_path, quarantine).await {
			Ok(page) => page,
			Err(message) if request_index == 0 => return Err(message),
			Err(_) => return Ok(accumulator.finish_retry()),
		};
		match accumulator.accept_page(page, &requested_after) {
			PageDecision::Complete => return Ok(accumulator.finish(ListingStatus::Complete)),
			PageDecision::End => return Ok(accumulator.finish(ListingStatus::End)),
			PageDecision::Retry => return Ok(accumulator.finish_retry()),
			PageDecision::Continue if request_index + 1 == MAX_LISTING_REQUESTS => {
				return Ok(accumulator.finish_budget());
			}
			PageDecision::Continue => {
				requested_after = accumulator.continuation_cursor.clone();
				request_path = upstream_cursor_path(path, &requested_after);
			}
		}
	}

	Ok(accumulator.finish_retry())
}

fn valid_post_id(value: &str) -> bool {
	!value.is_empty() && value.len() <= 80 && value.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn new_post_precedes(candidate: &Post, current: &Post) -> bool {
	(candidate.flags.stickied && !current.flags.stickied) || (candidate.flags.stickied == current.flags.stickied && candidate.created_ts > current.created_ts)
}

fn sort_group_members_new(members: &mut [crate::utils::GroupedPost]) {
	members.sort_by_key(|post| std::cmp::Reverse(post.created_ts));
	members.sort_by_key(|post| std::cmp::Reverse(post.stickied));
}

/// Match the prior New behavior (stable New/stickied sort, then grouping)
/// without changing the raw item that consumed the representative slot.
fn merge_group_member(representative: &mut Post, candidate: Post, sort_new_with_stickies: bool) {
	if sort_new_with_stickies && new_post_precedes(&candidate, representative) {
		let mut members = std::mem::take(&mut representative.grouped_posts);
		let previous_representative = std::mem::replace(representative, candidate);
		members.insert(0, grouped_post(previous_representative));
		representative.grouped_posts = members;
	} else {
		representative.grouped_posts.push(grouped_post(candidate));
	}
	if sort_new_with_stickies {
		sort_group_members_new(&mut representative.grouped_posts);
	}
	refresh_group_metadata(representative);
}

fn query_value(path: &str, key: &str) -> Option<String> {
	Url::parse(&format!("https://vale.invalid{path}"))
		.ok()?
		.query_pairs()
		.find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

/// Replace upstream pagination with the next raw Reddit fullname while
/// retaining only route/search semantics and the fixed page size.
fn upstream_cursor_path(path: &str, after: &str) -> String {
	let Ok(parsed) = Url::parse(&format!("https://vale.invalid{path}")) else {
		return path.to_string();
	};
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	for (key, value) in parsed.query_pairs() {
		if !is_internal_listing_parameter(&key) && !matches!(key.as_ref(), "after" | "before" | "limit") {
			serializer.append_pair(&key, &value);
		}
	}
	serializer.append_pair("after", after);
	serializer.append_pair("limit", &LISTING_PAGE_SIZE.to_string());
	format!("{}?{}", parsed.path(), serializer.finish())
}

pub fn is_internal_listing_parameter(name: &str) -> bool {
	matches!(
		name,
		"fragment" | "seen" | "seen_ids" | "group" | "group_state" | "target" | "target_count" | "profile" | "profile_state"
	) || name.starts_with("vale_")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FragmentMode {
	Document,
	Posts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostRenderKind {
	Direct,
	Search,
}

impl PostRenderKind {
	const fn as_str(self) -> &'static str {
		match self {
			Self::Direct => "direct",
			Self::Search => "search",
		}
	}
}

/// Listing fragments are enabled only by the exact versioned request header.
/// A present but unknown version is rejected instead of falling back to a
/// document response that client code could accidentally parse as a fragment.
// Keep the response error concrete so callers can return the rejection verbatim.
#[allow(clippy::result_large_err)]
pub fn fragment_mode(request: &Request<Body>) -> Result<FragmentMode, Response<Body>> {
	let values = request.headers().get_all("x-vale-fragment").iter().collect::<Vec<_>>();
	if values.is_empty() {
		return Ok(FragmentMode::Document);
	}
	if values.len() == 1 && values[0].as_bytes() == POSTS_FRAGMENT_VERSION.as_bytes() {
		return Ok(FragmentMode::Posts);
	}
	Err(fragment_error(StatusCode::BAD_REQUEST, "Unsupported Vale posts fragment version."))
}

// Keep the response error concrete so callers can return the rejection verbatim.
#[allow(clippy::result_large_err)]
pub fn reject_fragment_request(request: &Request<Body>) -> Result<(), Response<Body>> {
	match fragment_mode(request)? {
		FragmentMode::Document => Ok(()),
		FragmentMode::Posts => Err(fragment_route_rejection("This route does not provide post fragments.")),
	}
}

pub fn fragment_route_rejection(message: &str) -> Response<Body> {
	fragment_error(StatusCode::BAD_REQUEST, message)
}

fn fragment_error(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.header(header::VARY, "X-Vale-Fragment")
		.body(message.to_string().into())
		.unwrap_or_default()
}

/// A first upstream failure has no authoritative listing snapshot. Return a
/// non-fragment error so enhanced clients retain the settled collection.
pub fn fragment_unavailable_response() -> Response<Body> {
	fragment_error(
		StatusCode::SERVICE_UNAVAILABLE,
		"Vale could not refresh this listing. Retry without replacing the current posts.",
	)
}

/// Convert a pre-fetch profile-state failure into the correct response variant.
/// Fragment callers must not receive a parseable empty snapshot, while native
/// navigation receives an explicit private 503 document instead of bubbling a
/// generic server error through the router.
pub async fn policy_unavailable_response(request: Request<Body>, mode: FragmentMode) -> Result<Response<Body>, String> {
	if mode == FragmentMode::Posts {
		return Ok(fragment_unavailable_response());
	}
	let mut response = error(request, "Vale could not load this listing's private profile state. Please retry.").await?;
	*response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
	Ok(document_response(response))
}

#[derive(Template)]
#[template(path = "posts_fragment.html")]
struct PostsFragmentTemplate {
	posts: Vec<Post>,
	prefs: Preferences,
	url: String,
	render_kind: String,
	active_feed: String,
	previous_url: String,
	next_url: String,
	listing_status: String,
	visible_count: usize,
}

// These fields mirror the fragment template boundary; grouping them would
// broaden a stable rendering API without reducing runtime complexity.
#[allow(clippy::too_many_arguments)]
pub fn render_posts_fragment(
	posts: Vec<Post>,
	prefs: Preferences,
	url: String,
	render_kind: PostRenderKind,
	active_feed: String,
	previous_url: String,
	next_url: String,
	status: ListingStatus,
) -> Result<Response<Body>, String> {
	let visible_count = posts.len();
	let body = PostsFragmentTemplate {
		posts,
		prefs,
		url,
		render_kind: render_kind.as_str().to_string(),
		active_feed,
		previous_url,
		next_url,
		listing_status: status.as_str().to_string(),
		visible_count,
	}
	.render()
	.map_err(|error| format!("Unable to render the Vale posts fragment: {error}"))?;
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, "text/html; charset=utf-8")
			.header(header::CACHE_CONTROL, "private, no-store")
			.header(header::VARY, "X-Vale-Fragment")
			.header("X-Vale-Fragment", POSTS_FRAGMENT_VERSION)
			.body(body.into())
			.unwrap_or_default(),
	)
}

pub fn document_response(mut response: Response<Body>) -> Response<Body> {
	response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("private, no-store"));
	response.headers_mut().insert(header::VARY, header::HeaderValue::from_static("X-Vale-Fragment"));
	response
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::utils::{parse_post, ListingCursors};
	use hyper::header::HeaderValue;
	use serde_json::json;

	async fn post(id: &str, content: &str) -> Post {
		parse_post(&json!({
			"kind": "t3",
			"data": {
				"name": format!("t3_{id}"),
				"id": id,
				"title": format!("Post {id}"),
				"subreddit": "alpha",
				"author": "reader",
				"permalink": format!("/r/alpha/comments/{id}/post/"),
				"created_utc": 1_700_000_000.0,
				"score": 1,
				"num_comments": 1,
				"url": content,
				"url_overridden_by_dest": content,
				"domain": "example.test"
			}
		}))
		.await
	}

	async fn page(ids: &[&str], after: &str, fingerprint: &str) -> ListingPage {
		let mut posts = Vec::new();
		for id in ids {
			posts.push(post(id, &format!("https://example.test/{id}")).await);
		}
		ListingPage {
			raw_fullnames: posts.iter().map(|post| post.fullname.clone()).collect(),
			posts,
			cursors: ListingCursors {
				before: "t3_previous".to_string(),
				after: after.to_string(),
			},
			fingerprint: fingerprint.to_string(),
		}
	}

	#[tokio::test]
	async fn target_cursor_uses_consumed_record_and_later_duplicate_patches_group() {
		let first_ids = (0..20).map(|index| format!("a{index:02}")).collect::<Vec<_>>();
		let first_refs = first_ids.iter().map(String::as_str).collect::<Vec<_>>();
		let mut accumulator = ListingAccumulator::new(ListingPolicy {
			group_exact_content: true,
			..ListingPolicy::test_default()
		});
		assert_eq!(accumulator.accept_page(page(&first_refs, "t3_a19", "page-one").await, ""), PageDecision::Continue);

		let second_ids = (0..25).map(|index| format!("b{index:02}")).collect::<Vec<_>>();
		let second_refs = second_ids.iter().map(String::as_str).collect::<Vec<_>>();
		// The fifth record fills the 25th representative. Unique records after
		// that boundary cannot consume a 26th card.
		assert_eq!(accumulator.accept_page(page(&second_refs, "t3_b24", "page-two").await, "t3_a19"), PageDecision::Continue);
		assert_eq!(accumulator.next_cursor, "t3_b04");
		assert_eq!(accumulator.continuation_cursor, "t3_b24");

		let third_ids = (0..25).map(|index| format!("c{index:02}")).collect::<Vec<_>>();
		let third_refs = third_ids.iter().map(String::as_str).collect::<Vec<_>>();
		assert_eq!(accumulator.accept_page(page(&third_refs, "t3_c24", "page-three").await, "t3_b24"), PageDecision::Continue);

		let fourth_ids = (0..25).map(|index| format!("d{index:02}")).collect::<Vec<_>>();
		let fourth_refs = fourth_ids.iter().map(String::as_str).collect::<Vec<_>>();
		let mut fourth = page(&fourth_refs, "t3_d24", "page-four").await;
		// Exact membership is completed through the fourth page even though the
		// representative boundary was reached on page two.
		fourth.posts[24].content_key = accumulator.posts[0].content_key.clone();
		assert_eq!(accumulator.accept_page(fourth, "t3_c24"), PageDecision::Continue);
		let result = accumulator.finish_budget();
		assert_eq!(result.posts.len(), 25);
		assert_eq!(result.next_cursor, "t3_b04");
		assert_eq!(result.previous_cursor, "t3_previous");
		assert_eq!(result.posts[0].grouped_posts.len(), 1);
		assert_eq!(result.requests, MAX_LISTING_REQUESTS);
		assert_eq!(result.diagnostics.raw_records, 95);
		assert_eq!(result.status, ListingStatus::Complete);
	}

	#[tokio::test]
	async fn ungrouped_target_completes_without_unnecessary_tail_requests() {
		let ids = (0..LISTING_PAGE_SIZE).map(|index| format!("post{index:02}")).collect::<Vec<_>>();
		let refs = ids.iter().map(String::as_str).collect::<Vec<_>>();
		let mut accumulator = ListingAccumulator::new(ListingPolicy::test_default());
		assert_eq!(accumulator.accept_page(page(&refs, "t3_post24", "one-full-page").await, ""), PageDecision::Complete);
		let result = accumulator.finish(ListingStatus::Complete);
		assert_eq!(result.posts.len(), LISTING_PAGE_SIZE);
		assert_eq!(result.requests, 1);
		assert_eq!(result.next_cursor, "t3_post24");
	}

	#[tokio::test]
	async fn hidden_representative_re_elects_earliest_visible_group_sibling() {
		let mut policy = ListingPolicy {
			group_exact_content: true,
			..ListingPolicy::test_default()
		};
		policy.hidden_ids.insert("one".to_string());
		let mut first = post("one", "https://example.test/shared").await;
		let second = post("two", "https://example.test/shared").await;
		first.content_key = second.content_key.clone();
		let listing_page = ListingPage {
			raw_fullnames: vec![first.fullname.clone(), second.fullname.clone()],
			posts: vec![first, second],
			cursors: ListingCursors::default(),
			fingerprint: "hidden-election".to_string(),
		};
		let mut accumulator = ListingAccumulator::new(policy);
		assert_eq!(accumulator.accept_page(listing_page, ""), PageDecision::End);
		let result = accumulator.finish(ListingStatus::End);
		assert_eq!(result.posts.len(), 1);
		assert_eq!(result.posts[0].id, "two");
		assert!(result.posts[0].grouped_posts.is_empty());
	}

	#[tokio::test]
	async fn new_groups_elect_and_order_stickied_newer_members_before_raw_earlier_members() {
		let mut policy = ListingPolicy {
			group_exact_content: true,
			sort_new_with_stickies: true,
			..ListingPolicy::test_default()
		};
		policy.show_nsfw = true;
		let mut raw_earlier = post("raw-earlier", "https://example.test/shared-new").await;
		raw_earlier.created_ts = 300;
		let mut sticky_older = post("sticky-older", "https://example.test/shared-new").await;
		sticky_older.created_ts = 100;
		sticky_older.flags.stickied = true;
		let mut sticky_newer = post("sticky-newer", "https://example.test/shared-new").await;
		sticky_newer.created_ts = 200;
		sticky_newer.flags.stickied = true;
		let posts = vec![raw_earlier, sticky_older, sticky_newer];
		let raw_fullnames = posts.iter().map(|post| post.fullname.clone()).collect();
		let mut accumulator = ListingAccumulator::new(policy);
		assert_eq!(
			accumulator.accept_page(
				ListingPage {
					posts,
					cursors: ListingCursors::default(),
					fingerprint: "new-group-election".to_string(),
					raw_fullnames,
				},
				"",
			),
			PageDecision::End
		);
		let result = accumulator.finish(ListingStatus::End);
		assert_eq!(result.posts.len(), 1);
		assert_eq!(result.posts[0].id, "sticky-newer");
		assert_eq!(
			result.posts[0].grouped_posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(),
			["sticky-older", "raw-earlier"]
		);
		assert_eq!(result.posts[0].combined_url, "/combined?posts=sticky-newer,sticky-older,raw-earlier");
	}

	#[tokio::test]
	async fn overlap_can_progress_but_repeated_fingerprint_and_no_progress_retry() {
		let mut accumulator = ListingAccumulator::new(ListingPolicy::test_default());
		assert_eq!(accumulator.accept_page(page(&["one", "two"], "t3_two", "first").await, ""), PageDecision::Continue);
		assert_eq!(
			accumulator.accept_page(page(&["two", "three"], "t3_three", "second").await, "t3_two"),
			PageDecision::Continue
		);
		assert_eq!(accumulator.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), ["one", "two", "three"]);
		assert_eq!(accumulator.accept_page(page(&["two", "three"], "t3_four", "second").await, "t3_three"), PageDecision::Retry);

		let mut no_progress = ListingAccumulator::new(ListingPolicy::test_default());
		assert_eq!(no_progress.accept_page(page(&["one"], "t3_one", "one").await, ""), PageDecision::Continue);
		assert_eq!(no_progress.accept_page(page(&["one"], "t3_two", "overlap-only").await, "t3_one"), PageDecision::Retry);

		let mut repeated_cursor = ListingAccumulator::new(ListingPolicy::test_default());
		assert_eq!(repeated_cursor.accept_page(page(&["one"], "t3_one", "cursor-one").await, ""), PageDecision::Continue);
		assert_eq!(repeated_cursor.accept_page(page(&["two"], "t3_one", "cursor-two").await, "t3_one"), PageDecision::Retry);
	}

	#[tokio::test]
	async fn an_oversized_page_without_any_safe_fullname_is_retryable() {
		let mut malformed = page(&["one", "two"], "", "oversized-without-cursor").await;
		malformed.raw_fullnames.fill(String::new());
		let mut accumulator = ListingAccumulator::new(ListingPolicy::test_default());
		assert_eq!(accumulator.accept_page(malformed, ""), PageDecision::Retry);
		let result = accumulator.finish_retry();
		assert_eq!(result.status, ListingStatus::Retry);
		assert!(result.next_cursor.is_empty());
		assert!(result.posts.is_empty());
	}

	#[tokio::test]
	async fn eligibility_filters_are_exact_and_run_before_deduplication() {
		let mut policy = ListingPolicy::test_default();
		policy.allowed_communities = Some(HashSet::from(["alpha".to_string(), "blocked".to_string()]));
		policy.filters = HashSet::from(["blocked".to_string(), "u_muted".to_string()]);
		policy.hidden_ids = HashSet::from(["hidden".to_string()]);
		policy.show_nsfw = false;

		let good = post("good", "https://example.test/good").await;
		let mut membership = post("membership", "https://example.test/membership").await;
		membership.community = "beta".to_string();
		let mut blocked = post("blocked", "https://example.test/blocked").await;
		blocked.community = "blocked".to_string();
		let mut muted = post("muted", "https://example.test/muted").await;
		muted.author.name = "muted".to_string();
		let hidden = post("hidden", "https://example.test/hidden").await;
		let mut nsfw = post("nsfw", "https://example.test/nsfw").await;
		nsfw.flags.nsfw = true;
		let invalid = post("bad/id", "https://example.test/invalid").await;
		let mut duplicate = post("good", "https://example.test/duplicate").await;
		duplicate.fullname = "t3_good_alias".to_string();
		let posts = vec![good, membership, blocked, muted, hidden, nsfw, invalid, duplicate];
		let raw_fullnames = posts.iter().map(|post| post.fullname.clone()).collect();
		let mut accumulator = ListingAccumulator::new(policy);
		assert_eq!(
			accumulator.accept_page(
				ListingPage {
					posts,
					cursors: ListingCursors::default(),
					fingerprint: "eligibility".to_string(),
					raw_fullnames,
				},
				"",
			),
			PageDecision::End
		);
		let result = accumulator.finish(ListingStatus::End);
		assert_eq!(result.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), ["good"]);
		assert_eq!(result.diagnostics.membership_filtered, 1);
		assert_eq!(result.diagnostics.profile_filtered, 2);
		assert_eq!(result.diagnostics.hidden, 1);
		assert_eq!(result.diagnostics.nsfw_filtered, 1);
		assert_eq!(result.diagnostics.invalid_post_ids, 1);
		assert_eq!(result.diagnostics.duplicate_post_ids, 1);
	}

	#[tokio::test]
	async fn an_unfilled_four_page_budget_is_retryable_and_keeps_partial_cards() {
		let mut accumulator = ListingAccumulator::new(ListingPolicy::test_default());
		for index in 0..MAX_LISTING_REQUESTS {
			let id = format!("partial{index}");
			let after = format!("t3_{id}");
			let requested_after = if index == 0 { String::new() } else { format!("t3_partial{}", index - 1) };
			assert_eq!(
				accumulator.accept_page(page(&[id.as_str()], &after, &format!("partial-page-{index}")).await, &requested_after),
				PageDecision::Continue
			);
		}
		let result = accumulator.finish_budget();
		assert_eq!(result.status, ListingStatus::Retry);
		assert_eq!(result.posts.len(), MAX_LISTING_REQUESTS);
		assert_eq!(result.requests, MAX_LISTING_REQUESTS);
		assert!(result.next_cursor.is_empty());
	}

	#[test]
	fn upstream_cursor_removes_caller_state_and_enforces_twenty_five() {
		assert_eq!(
			upstream_cursor_path("/r/a/search.json?q=rust&before=t3_old&limit=999&seen_ids=a,b&target_count=1&raw_json=1", "t3_next"),
			"/r/a/search.json?q=rust&raw_json=1&after=t3_next&limit=25"
		);
	}

	#[test]
	fn fragment_header_is_exact_and_versioned() {
		let document = Request::new(Body::empty());
		assert_eq!(fragment_mode(&document).unwrap(), FragmentMode::Document);
		let valid = Request::builder()
			.header("x-vale-fragment", HeaderValue::from_static("posts-v1"))
			.body(Body::empty())
			.unwrap();
		assert_eq!(fragment_mode(&valid).unwrap(), FragmentMode::Posts);
		let invalid = Request::builder().header("x-vale-fragment", "posts-v2").body(Body::empty()).unwrap();
		assert_eq!(fragment_mode(&invalid).unwrap_err().status(), StatusCode::BAD_REQUEST);
		let duplicate = Request::builder()
			.header("x-vale-fragment", POSTS_FRAGMENT_VERSION)
			.header("x-vale-fragment", POSTS_FRAGMENT_VERSION)
			.body(Body::empty())
			.unwrap();
		assert_eq!(fragment_mode(&duplicate).unwrap_err().status(), StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn first_fetch_unavailable_response_cannot_be_parsed_as_an_empty_snapshot() {
		let response = fragment_unavailable_response();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store");
		assert_eq!(response.headers()[header::VARY], "X-Vale-Fragment");
		assert!(response.headers().get("x-vale-fragment").is_none());
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		assert!(!body.windows(b"data-vale-posts-fragment".len()).any(|window| window == b"data-vale-posts-fragment"));
	}

	#[tokio::test]
	async fn policy_failures_map_to_explicit_private_fragment_and_document_variants() {
		let fragment = policy_unavailable_response(Request::new(Body::empty()), FragmentMode::Posts).await.unwrap();
		assert_eq!(fragment.status(), StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(fragment.headers()[header::CACHE_CONTROL], "private, no-store");
		assert_eq!(fragment.headers()[header::VARY], "X-Vale-Fragment");
		assert_eq!(fragment.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
		assert!(fragment.headers().get("x-vale-fragment").is_none());
		let fragment_body = hyper::body::to_bytes(fragment.into_body()).await.unwrap();
		assert!(!fragment_body.windows(b"<html".len()).any(|window| window == b"<html"));
		assert!(!fragment_body.windows(b"data-vale-posts-fragment".len()).any(|window| window == b"data-vale-posts-fragment"));

		let document = policy_unavailable_response(Request::builder().uri("/r/rust/hot").body(Body::empty()).unwrap(), FragmentMode::Document)
			.await
			.unwrap();
		assert_eq!(document.status(), StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(document.headers()[header::CACHE_CONTROL], "private, no-store");
		assert_eq!(document.headers()[header::VARY], "X-Vale-Fragment");
		assert_eq!(document.headers()[header::CONTENT_TYPE], "text/html");
		assert!(document.headers().get("x-vale-fragment").is_none());
		let document_body = hyper::body::to_bytes(document.into_body()).await.unwrap();
		assert!(document_body.windows(b"<html".len()).any(|window| window == b"<html"));
		assert!(!document_body.windows(b"data-vale-posts-fragment".len()).any(|window| window == b"data-vale-posts-fragment"));
	}

	#[tokio::test]
	async fn search_fragment_has_one_wrapper_and_one_keyed_card_per_result() {
		let response = render_posts_fragment(
			vec![post("one", "https://example.test/one").await],
			Preferences::default(),
			"/search?q=one&scope=all".to_string(),
			PostRenderKind::Search,
			String::new(),
			String::new(),
			"/search?q=one&scope=all&after=t3_one".to_string(),
			ListingStatus::Complete,
		)
		.unwrap();
		assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store");
		assert_eq!(response.headers()[header::VARY], "X-Vale-Fragment");
		assert_eq!(response.headers()["x-vale-fragment"], POSTS_FRAGMENT_VERSION);
		let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
		let body = String::from_utf8(bytes.to_vec()).unwrap();
		assert_eq!(body.matches("data-vale-posts-fragment=\"1\"").count(), 1);
		assert_eq!(body.matches("data-vale-render-kind=\"search\"").count(), 1);
		assert_eq!(body.matches("data-vale-search-result=\"1\"").count(), 1);
		assert_eq!(body.matches("data-post-id=\"one\"").count(), 1);
		assert!(body.contains("data-visible-count=\"1\""));
	}
}
