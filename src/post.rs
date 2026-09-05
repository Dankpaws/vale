use crate::account;
use crate::client::json;
use crate::server::RequestExt;
use crate::subreddit::{can_access_quarantine, quarantine};
use crate::thread::{ThreadGroup, ThreadModel, ThreadSearch, ThreadSummary};
use crate::utils::{error, get_filters, nsfw_landing, param, parse_post, template, Post, Preferences};
use askama::Template;
use hyper::{header, Body, Request, Response, StatusCode};
use serde::Serialize;

// STRUCTS
#[derive(Template)]
#[template(path = "post.html")]
struct PostTemplate {
	reading: crate::reading::ReadingEntry,
	reading_enabled: bool,
	sources: Vec<crate::sources::SourceItem>,
	activity: crate::activity::Visit,
	comments: Vec<ThreadGroup>,
	post: Post,
	sort: String,
	prefs: Preferences,
	single_thread: bool,
	url: String,
	url_without_query: String,
	comment_query: String,
	filtered_comment_count: usize,
	thread_summary: ThreadSummary,
	thread_search: ThreadSearch,
	archive_id: String,
	archive_status: String,
	archive_status_label: String,
	post_hidden: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPatchRoot {
	node_id: String,
	parent_id: String,
	ancestor_path: Vec<String>,
	ancestor_path_complete: bool,
	depth: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPatchNode {
	node_id: String,
	parent_id: String,
	ancestor_path: Vec<String>,
	ancestor_path_complete: bool,
	depth: usize,
	kind: String,
	html: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPatchResponse {
	version: u8,
	post_id: String,
	requested_parent_id: String,
	continuation_id: String,
	sort: String,
	source_root: ThreadPatchRoot,
	nodes: Vec<ThreadPatchNode>,
	summary: ThreadSummary,
	search: ThreadSearch,
}

const MAX_COMMENT_SEARCH_CHARS: usize = 160;

fn forwarded_item_query(query: &str) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
		if !matches!(key.as_ref(), "resume" | "thread_patch" | "continuation" | "raw_json" | "q" | "type" | "activity_visit") {
			serializer.append_pair(&key, &value);
		}
	}
	serializer.append_pair("raw_json", "1");
	serializer.finish()
}

fn comment_search_query(query: &str) -> String {
	if item_query_param(query, "type").as_deref() != Some("comment") {
		return String::new();
	}
	item_query_param(query, "q").unwrap_or_default().trim().chars().take(MAX_COMMENT_SEARCH_CHARS).collect()
}

fn item_url_without_comment_search(path: &str, query: &str) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
		if !matches!(key.as_ref(), "q" | "type") {
			serializer.append_pair(&key, &value);
		}
	}
	let query = serializer.finish();
	if query.is_empty() {
		path.to_string()
	} else {
		format!("{path}?{query}")
	}
}

fn item_query_param(query: &str, name: &str) -> Option<String> {
	url::form_urlencoded::parse(query.as_bytes()).find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

fn valid_continuation_id(id: &str) -> bool {
	id.strip_prefix("more_")
		.is_some_and(|digest| digest.len() == 24 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn thread_patch_error(status: StatusCode, message: &str) -> Response<Body> {
	let body = serde_json::json!({ "error": message }).to_string();
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "application/json; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
		.body(Body::from(body))
		.unwrap_or_default()
}

fn thread_patch_response(
	post_id: &str,
	requested_parent_id: &str,
	continuation_id: &str,
	sort: &str,
	summary: ThreadSummary,
	search: ThreadSearch,
	groups: &[ThreadGroup],
) -> Result<Response<Body>, String> {
	if !valid_continuation_id(continuation_id) {
		return Ok(thread_patch_error(StatusCode::BAD_REQUEST, "The continuation identity is invalid."));
	}
	let canonical_parent_id = format!("t1_{}", requested_parent_id.trim_start_matches("t1_"));
	let Some(group) = groups.iter().find(|group| group.id == canonical_parent_id) else {
		return Ok(thread_patch_error(
			StatusCode::UNPROCESSABLE_ENTITY,
			"Reddit did not return the requested continuation parent.",
		));
	};
	let nodes = group
		.descendants
		.iter()
		.map(|comment| {
			Ok(ThreadPatchNode {
				node_id: comment.node_id.clone(),
				parent_id: comment.parent_node_id.clone(),
				ancestor_path: comment.ancestor_path.split_whitespace().map(str::to_string).collect(),
				ancestor_path_complete: comment.ancestor_path_complete,
				depth: comment.depth,
				kind: comment.kind.clone(),
				html: comment.render().map_err(|error| format!("Could not render a continuation node: {error}"))?,
			})
		})
		.collect::<Result<Vec<_>, String>>()?;
	let payload = ThreadPatchResponse {
		version: 1,
		post_id: format!("t3_{}", post_id.trim_start_matches("t3_")),
		requested_parent_id: canonical_parent_id,
		continuation_id: continuation_id.to_string(),
		sort: sort.to_string(),
		source_root: ThreadPatchRoot {
			node_id: group.root.node_id.clone(),
			parent_id: group.root.parent_node_id.clone(),
			ancestor_path: group.root.ancestor_path.split_whitespace().map(str::to_string).collect(),
			ancestor_path_complete: group.root.ancestor_path_complete,
			depth: group.root.depth,
		},
		nodes,
		summary,
		search,
	};
	let body = serde_json::to_string(&payload).map_err(|error| format!("Could not serialize the continuation patch: {error}"))?;
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/json; charset=utf-8")
			.header(header::CACHE_CONTROL, "private, no-store")
			.header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
			.body(Body::from(body))
			.unwrap_or_default(),
	)
}

fn listing_has_comment(listing: &serde_json::Value, id: &str) -> bool {
	listing["data"]["children"].as_array().is_some_and(|children| {
		children
			.iter()
			.any(|c| c["kind"] == "t1" && (c["data"]["id"].as_str() == Some(id) || listing_has_comment(&c["data"]["replies"], id)))
	})
}
fn merge_resume_listing(listing: &mut serde_json::Value, saved: &serde_json::Value) {
	let Some(incoming) = saved["data"]["children"].as_array() else { return };
	if listing["data"]["children"].as_array().is_none() {
		*listing = serde_json::json!({"kind":"Listing","data":{"children":[]}});
	}
	let children = listing["data"]["children"].as_array_mut().unwrap();
	for node in incoming.iter().filter(|n| n["kind"] == "t1").take(500) {
		let Some(id) = node["data"]["id"].as_str().filter(|id| account::valid_post_id(id)) else {
			continue;
		};
		if let Some(existing) = children.iter_mut().find(|n| n["kind"] == "t1" && n["data"]["id"].as_str() == Some(id)) {
			merge_resume_listing(&mut existing["data"]["replies"], &node["data"]["replies"]);
		} else {
			children.push(node.clone());
		}
	}
}

pub async fn item(req: Request<Body>) -> Result<Response<Body>, String> {
	// Build Reddit API path
	let thread_patch = item_query_param(req.uri().query().unwrap_or_default(), "thread_patch").as_deref() == Some("1");
	let continuation_id = item_query_param(req.uri().query().unwrap_or_default(), "continuation").unwrap_or_default();
	let query = comment_search_query(req.uri().query().unwrap_or_default());
	let mut path: String = format!("{}.json?{}", req.uri().path(), forwarded_item_query(req.uri().query().unwrap_or_default()));
	let sub = req.param("sub").unwrap_or_default();
	let quarantined = can_access_quarantine(&req, &sub);
	let prefs = Preferences::new(&req);
	let comment_keywords = prefs.comment_keywords();

	// Set sort to sort query parameter
	let sort = param(&path, "sort").unwrap_or_else(|| {
		// Grab default comment sort method from Cookies
		let default_sort = prefs.comment_sort.clone();

		// If there's no sort query but there's a default sort, set sort to default_sort
		if default_sort.is_empty() {
			String::new()
		} else {
			path.push_str(&format!("&sort={default_sort}"));
			default_sort
		}
	});

	// Log the post ID being fetched in debug mode
	#[cfg(debug_assertions)]
	req.param("id").unwrap_or_default();

	let single_thread = req.param("comment_id").is_some();
	let highlighted_comment = &req.param("comment_id").unwrap_or_default();

	// Send a request to the url, receive JSON in response
	match json(path, quarantined).await {
		// Otherwise, grab the JSON output from the request
		Ok(mut response) => {
			// Parse the JSON into Post and Comment structs
			let mut post = parse_post(&response[0]["data"]["children"][0]).await;

			let req_url = req.uri().to_string();
			// Return landing page if this post if this Reddit deems this post
			// NSFW, but we have also disabled the display of NSFW content
			// or if the instance is SFW-only.
			if post.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
				return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
			}
			if !thread_patch {
				crate::account::record_post_view(&req, &post)?;
			}

			let reading = if let Some(context) = account::context(&req) {
				crate::reading::get(&account::open_database()?, context.profile_id, &post.id).map_err(|e| format!("{e:?}"))?
			} else {
				Default::default()
			};
			// Fetch only the saved path when it is outside the initial full-thread batch.
			if !single_thread
				&& !thread_patch
				&& item_query_param(req.uri().query().unwrap_or_default(), "resume").as_deref() == Some("1")
				&& account::valid_post_id(&reading.anchor)
				&& !listing_has_comment(&response[1], &reading.anchor)
			{
				let saved_path = format!(
					"/comments/{}/comments/{}.json?context=100&limit=500&sort={}&raw_json=1",
					post.id,
					reading.anchor,
					reading.resume_state().sort
				);
				if let Ok(Ok(saved)) = tokio::time::timeout(std::time::Duration::from_secs(20), json(saved_path, quarantined)).await {
					if saved[0]["data"]["children"][0]["data"]["id"].as_str() == Some(post.id.as_str()) {
						merge_resume_listing(&mut response[1], &saved[1]);
					}
				}
			}

			let thread = ThreadModel::from_listing(
				&response[1],
				&post.id,
				response[0]["data"]["children"][0]["data"]["num_comments"].as_u64().unwrap_or_default() as usize,
				&post.permalink,
				&post.author.name,
				highlighted_comment,
				&get_filters(&req),
				&comment_keywords,
				&prefs,
			);
			crate::watch::observe_request(&req, &post.id, &thread)?;
			let thread_summary = thread.summary();
			let filtered_comment_count = thread.filtered_comment_count();
			let thread_search = thread.search(&query);
			let mut comments = thread.into_search_projection(&thread_search);
			let archive = if thread_patch { None } else { crate::archive::archive_for_post(&req, &post.id)? };
			let post_hidden = !thread_patch && crate::account::post_is_hidden(&req, &post.id)?;
			let activity = crate::activity::for_post(&req, &post, thread_patch)?;
			activity.highlight(&mut comments);
			post.new_comments = activity.new_comments;
			if thread_patch {
				if !single_thread {
					return Ok(thread_patch_error(StatusCode::BAD_REQUEST, "A continuation patch requires a parent comment."));
				}
				return thread_patch_response(&post.id, highlighted_comment, &continuation_id, &sort, thread_summary, thread_search, &comments);
			}
			let mut sources = Vec::new();
			if let (Some(context), Some(out)) = (account::context(&req), post.out_url.as_deref()) {
				let db = account::open_database()?;
				for feed in prefs.feed_groups().iter().filter(|f| f.communities.iter().any(|c| c.eq_ignore_ascii_case(&post.community))) {
					sources.extend(crate::sources::matching_entries(&db, context.profile_id, &feed.slug, out).map_err(|e| format!("{e:?}"))?);
				}
				sources.sort_by_key(|i| i.id);
				sources.dedup_by_key(|i| i.id);
			}
			let url_without_query = activity.url(&item_url_without_comment_search(req.uri().path(), req.uri().query().unwrap_or_default()));

			// Use the Post and Comment structs to generate a website to show users
			Ok(template(&PostTemplate {
				reading,
				reading_enabled: crate::account::context(&req).is_some(),
				sources,
				activity,
				comments,
				post,
				url_without_query,
				sort,
				prefs,
				single_thread,
				url: req_url,
				comment_query: thread_search.query.clone(),
				filtered_comment_count,
				thread_summary,
				thread_search,
				archive_id: archive.as_ref().map(|entry| entry.id.clone()).unwrap_or_default(),
				archive_status: archive.as_ref().map(|entry| entry.status.clone()).unwrap_or_default(),
				archive_status_label: archive.map(|entry| entry.status_label).unwrap_or_default(),
				post_hidden,
			}))
		}
		// If the Reddit API returns an error, exit and send error page to user
		Err(msg) => {
			if msg == "quarantined" || msg == "gated" {
				let sub = req.param("sub").unwrap_or_default();
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::thread::ThreadModel;
	use serde_json::{json, Value};
	use std::collections::HashSet;

	#[test]
	fn resume_merge_twelve_edges() {
		fn listing(nodes: Vec<serde_json::Value>) -> serde_json::Value {
			json!({"kind":"Listing","data":{"children":nodes}})
		}
		fn c(id: &str, replies: serde_json::Value) -> serde_json::Value {
			json!({"kind":"t1","data":{"id":id,"replies":replies}})
		}
		for case in 0..12 {
			let mut base = listing(vec![c("root", listing(vec![c("first", json!(""))])), c("sibling", json!(""))]);
			let mut extra = listing(vec![c("root", listing(vec![c("target", json!(""))]))]);
			match case {
				1 => extra = listing(vec![]),
				2 => extra = json!(null),
				3 => extra = listing(vec![json!({"kind":"more","data":{"id":"target"}})]),
				4 => extra = listing(vec![c("newroot", listing(vec![c("target", json!(""))]))]),
				5 => extra = listing(vec![c("root", listing(vec![c("first", listing(vec![c("target", json!(""))]))]))]),
				6 => base = json!(""),
				7 => extra = listing(vec![c("bad/id", json!(""))]),
				8 => extra = listing(vec![c("root", listing(vec![c("target", json!("")), c("target", json!(""))]))]),
				9 => extra = listing(vec![c("root", json!(""))]),
				10 => extra = listing(vec![c("root", listing(vec![c("target", json!(""))])), c("sibling", json!(""))]),
				11 => {
					assert_eq!(forwarded_item_query("resume=1&sort=old"), "sort=old&raw_json=1");
				}
				_ => {}
			}
			merge_resume_listing(&mut base, &extra);
			let once = base.clone();
			merge_resume_listing(&mut base, &extra);
			assert_eq!(base, once, "idempotent case {case}");
			assert_eq!(listing_has_comment(&base, "target"), ![1, 2, 3, 7, 9].contains(&case), "case {case}");
			if case != 6 {
				assert!(listing_has_comment(&base, "first"));
				assert!(listing_has_comment(&base, "sibling"));
			}
		}
	}

	#[test]
	fn thread_patch_state_never_reaches_reddit() {
		assert_eq!(forwarded_item_query("sort=new&activity_visit=private-local-state"), "sort=new&raw_json=1");
		assert_eq!(
			forwarded_item_query("sort=top&q=needle&type=comment&thread_patch=1&continuation=more_0123456789abcdef01234567&raw_json=0&context=3"),
			"sort=top&context=3&raw_json=1"
		);
		assert_eq!(item_query_param("thread_patch=1&sort=top", "thread_patch").as_deref(), Some("1"));
	}

	#[test]
	fn comment_search_query_is_explicit_bounded_and_removable() {
		assert_eq!(comment_search_query("sort=top&q=deep+reply&type=comment"), "deep reply");
		assert!(comment_search_query("q=deep+reply").is_empty());
		assert_eq!(
			comment_search_query(&format!("q={}&type=comment", "x".repeat(200))).chars().count(),
			MAX_COMMENT_SEARCH_CHARS
		);
		assert_eq!(
			item_url_without_comment_search("/r/test/comments/post/thread/", "sort=top&q=deep+reply&type=comment&context=3"),
			"/r/test/comments/post/thread/?sort=top&context=3"
		);
	}

	#[test]
	fn continuation_identity_is_bounded_and_canonical() {
		assert!(valid_continuation_id("more_0123456789abcdef01234567"));
		assert!(!valid_continuation_id("more_0123456789abcdef0123456"));
		assert!(!valid_continuation_id("more_0123456789abcdef0123456z"));
		assert!(!valid_continuation_id("t1_0123456789abcdef01234567"));
	}

	#[tokio::test]
	async fn thread_patch_response_contains_only_normalized_descendants() {
		let listing = json!({
			"data": {"children": [{
				"kind": "t1",
				"data": {
					"id": "parent",
					"name": "t1_parent",
					"parent_id": "t1_ancestor",
					"depth": 3,
					"author": "parent-user",
					"body": "Parent",
					"body_html": "<p>Parent</p>",
					"replies": {"data": {"children": [{
						"kind": "t1",
						"data": {
							"id": "child",
							"name": "t1_child",
							"parent_id": "t1_parent",
							"author": "child-user",
							"body": "Child",
							"body_html": "<p>Child</p>",
							"replies": ""
						}
					}]}}
				}
			}]}
		});
		let thread = ThreadModel::from_listing(
			&listing,
			"post",
			2,
			"/comments/post/thread/",
			"post-user",
			"parent",
			&HashSet::new(),
			&[],
			&Preferences::default(),
		);
		let summary = thread.summary();
		let search = thread.search("child");
		let groups = thread.into_search_projection(&search);
		let response = thread_patch_response("post", "parent", "more_0123456789abcdef01234567", "confidence", summary, search, &groups).unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
		let payload: Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(payload["sourceRoot"]["nodeId"], "t1_parent");
		assert_eq!(payload["nodes"].as_array().unwrap().len(), 1);
		assert_eq!(payload["nodes"][0]["nodeId"], "t1_child");
		assert_eq!(payload["nodes"][0]["parentId"], "t1_parent");
		assert!(payload["nodes"][0]["html"].as_str().unwrap().contains("data-thread-node-id=\"t1_child\""));
		assert_eq!(payload["search"]["matchIds"][0], "t1_child");
		assert!(payload["nodes"][0]["html"].as_str().unwrap().contains("data-comment-search-match=\"true\""));
	}
}

#[cfg(test)]
mod reading_fixture_tests {
	use super::*;
	#[tokio::test]
	async fn reading_discussion_fixture() {
		for theme in ["dark", "light"] {
			let prefs = crate::reading_fixtures::preferences(theme);
			let post = crate::reading_fixtures::posts().await.remove(0);
			let model = ThreadModel::from_listing(
				&crate::reading_fixtures::comments(),
				"post0",
				20,
				&post.permalink,
				"field_reader",
				"",
				&std::collections::HashSet::new(),
				&[],
				&prefs,
			);
			let summary = model.summary();
			let search = model.search("");
			let html = PostTemplate {
				reading: Default::default(),
				reading_enabled: false,
				sources: vec![],
				activity: crate::activity::Visit::default(),
				comments: model.into_search_projection(&search),
				url: post.permalink.clone(),
				url_without_query: post.permalink.clone(),
				post,
				sort: "confidence".into(),
				prefs,
				single_thread: false,
				comment_query: String::new(),
				filtered_comment_count: 0,
				thread_summary: summary,
				thread_search: search,
				archive_id: String::new(),
				archive_status: String::new(),
				archive_status_label: String::new(),
				post_hidden: false,
			}
			.render()
			.unwrap();
			assert!(html.contains("data-thread-depth=\"8\""));
			assert!(html.contains("Show 8 replies"));
			assert!(html.contains("class=\"disclosure-chevron\""));
			assert!(!html.contains("class=\"sr-only\" data-replies-label"));
			assert!(html.contains("Best</option>") || html.contains("Best\n"));
			crate::reading_fixtures::export(theme, "discussion.html", &html);
		}
	}
}
