use crate::{
	account,
	client::json,
	thread::{ThreadGroup, ThreadModel},
	utils::{get_filters, parse_post, template, Preferences},
};
use askama::Template;
use hyper::{header, Body, Request, Response, StatusCode};
use std::collections::{HashMap, HashSet};
use std::future::Future;

const MAX_COMBINED_POSTS: usize = 12;
const MAX_COMBINED_ROOT_COMMENTS: usize = 500;
const COMBINED_FETCH_CONCURRENCY: usize = 3;

pub struct CombinedDiscussionView {
	pub activity: crate::activity::Visit,
	pub title: String,
	pub community: String,
	pub permalink: String,
	pub score: String,
	pub comments: String,
}

pub struct CombinedCommentView {
	pub activity: crate::activity::Visit,
	pub post_id: String,
	pub community: String,
	pub post_title: String,
	pub post_permalink: String,
	pub raw_score: i64,
	pub group: ThreadGroup,
}

#[derive(Template)]
#[template(path = "combined.html")]
struct CombinedTemplate {
	prefs: Preferences,
	url: String,
	source_url: String,
	discussions: Vec<CombinedDiscussionView>,
	comments: Vec<CombinedCommentView>,
	filtered_comment_count: usize,
}

fn plain_response(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(Body::from(message.to_string()))
		.unwrap_or_default()
}

fn requested_post_ids(request: &Request<Body>) -> Result<Vec<String>, &'static str> {
	let encoded = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
		.find_map(|(key, value)| (key == "posts").then_some(value.into_owned()))
		.unwrap_or_default();
	let mut seen = HashSet::new();
	let ids = encoded
		.split(',')
		.filter(|id| account::valid_post_id(id))
		.filter_map(|id| seen.insert(id.to_string()).then_some(id.to_string()))
		.take(MAX_COMBINED_POSTS + 1)
		.collect::<Vec<_>>();
	if ids.len() < 2 || ids.len() > MAX_COMBINED_POSTS {
		return Err("A combined discussion requires between 2 and 12 distinct post identifiers.");
	}
	Ok(ids)
}

async fn fetch_source_batch<F, Fut>(ids: &[String], fetch: F) -> Result<Vec<serde_json::Value>, String>
where
	F: Fn(String) -> Fut,
	Fut: Future<Output = Result<serde_json::Value, String>>,
{
	debug_assert!(ids.len() <= COMBINED_FETCH_CONCURRENCY);
	let fetch = &fetch;
	let fetch_at = |index: usize| async move {
		match ids.get(index) {
			Some(id) => fetch(id.clone()).await.map(Some),
			None => Ok(None),
		}
	};
	// Keep input order, and cancel sibling requests if a source fails.
	let (first, second, third) = tokio::try_join!(fetch_at(0), fetch_at(1), fetch_at(2))?;
	Ok([first, second, third].into_iter().flatten().collect())
}

pub async fn item(request: Request<Body>) -> Result<Response<Body>, String> {
	let ids = match requested_post_ids(&request) {
		Ok(ids) => ids,
		Err(message) => return Ok(plain_response(StatusCode::BAD_REQUEST, message)),
	};
	let prefs = Preferences::new(&request);
	let filters = get_filters(&request);
	let keywords = prefs.comment_keywords();
	let mut identity = String::new();
	let mut source_url = String::new();
	let mut discussions = Vec::new();
	let mut combined_comments = Vec::new();
	let mut sources = Vec::new();

	for batch in ids.chunks(COMBINED_FETCH_CONCURRENCY) {
		let responses = fetch_source_batch(batch, |id| json(format!("/comments/{id}.json?sort=top&limit=500&depth=10&raw_json=1"), true))
			.await
			.map_err(|message| format!("Reddit could not provide one of the grouped discussions: {message}"))?;
		for response in responses {
			let post_thing = &response[0]["data"]["children"][0];
			let post = parse_post(post_thing).await;
			if post.id.is_empty() || post.content_key.is_empty() {
				return Ok(plain_response(
					StatusCode::BAD_REQUEST,
					"One of those submissions has no strong content identity, so Vale will not merge it.",
				));
			}
			if identity.is_empty() {
				identity = post.content_key.clone();
				source_url = post.out_url.clone().unwrap_or_default();
			} else if identity != post.content_key {
				return Ok(plain_response(
					StatusCode::BAD_REQUEST,
					"Those submissions do not share an exact URL or Reddit crosspost identity. Vale will not combine unrelated discussions.",
				));
			}

			sources.push((post, response));
		}
	}

	// Validate every source before recording any visits to this combined page.
	for (post, response) in sources {
		let root_scores = response[1]["data"]["children"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|thing| thing["kind"].as_str() == Some("t1"))
			.filter_map(|thing| Some((thing["data"]["id"].as_str()?.to_string(), thing["data"]["score"].as_i64().unwrap_or_default())))
			.collect::<HashMap<_, _>>();
		let mut groups = ThreadModel::from_listing(
			&response[1],
			&post.id,
			response[0]["data"]["children"][0]["data"]["num_comments"].as_u64().unwrap_or_default() as usize,
			&post.permalink,
			&post.author.name,
			"",
			&filters,
			&keywords,
			&prefs,
		)
		.into_projection();
		let activity = crate::activity::for_post(&request, &post, false)?;
		account::record_post_view(&request, &post)?;
		activity.highlight(&mut groups);
		crate::library::decorate_comments(&request, &post.id, &mut groups)?;
		for group in groups.into_iter().filter(|group| group.root.kind == "t1") {
			combined_comments.push(CombinedCommentView {
				activity: activity.clone(),
				post_id: post.id.clone(),
				community: post.community.clone(),
				post_title: post.title.clone(),
				post_permalink: post.permalink.clone(),
				raw_score: root_scores.get(&group.root.id).copied().unwrap_or_default(),
				group,
			});
		}
		discussions.push(CombinedDiscussionView {
			activity,
			title: post.title,
			community: post.community,
			permalink: post.permalink,
			score: post.score.0,
			comments: post.comments.0,
		});
	}

	combined_comments.sort_by(|left, right| {
		right
			.raw_score
			.cmp(&left.raw_score)
			.then_with(|| left.community.cmp(&right.community))
			.then_with(|| left.group.root.id.cmp(&right.group.root.id))
	});
	combined_comments.truncate(MAX_COMBINED_ROOT_COMMENTS);
	let filtered_comment_count = combined_comments
		.iter()
		.map(|entry| usize::from(entry.group.root.is_keyword_filtered) + entry.group.descendants.iter().filter(|comment| comment.is_keyword_filtered).count())
		.sum();

	Ok(template(&CombinedTemplate {
		prefs,
		url: request.uri().to_string(),
		source_url,
		discussions,
		comments: combined_comments,
		filtered_comment_count,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use std::sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	};

	#[tokio::test]
	async fn source_batches_overlap_three_requests_and_preserve_input_order() {
		let active = AtomicUsize::new(0);
		let peak = AtomicUsize::new(0);
		let ids = (0..8).map(|index| index.to_string()).collect::<Vec<_>>();
		let mut responses = Vec::new();
		for batch in ids.chunks(COMBINED_FETCH_CONCURRENCY) {
			let barrier = tokio::sync::Barrier::new(batch.len());
			let fetched = tokio::time::timeout(
				std::time::Duration::from_secs(2),
				fetch_source_batch(batch, |id| {
					let (active, peak, barrier) = (&active, &peak, &barrier);
					async move {
						peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
						// This cannot pass if the batch silently becomes serial.
						barrier.wait().await;
						tokio::time::sleep(std::time::Duration::from_millis(3 - id.parse::<u64>().unwrap() % 3)).await;
						active.fetch_sub(1, Ordering::SeqCst);
						Ok(json!(id))
					}
				}),
			)
			.await
			.unwrap()
			.unwrap();
			responses.extend(fetched);
		}
		assert_eq!(responses, ids.into_iter().map(|id| json!(id)).collect::<Vec<_>>());
		assert_eq!(peak.load(Ordering::SeqCst), COMBINED_FETCH_CONCURRENCY);
		assert_eq!(active.load(Ordering::SeqCst), 0);
		assert!(fetch_source_batch(&[], |_| async { panic!("an empty batch must not fetch") }).await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn a_failed_source_cancels_pending_batch_requests() {
		struct ActiveRequest(Arc<AtomicUsize>);
		impl Drop for ActiveRequest {
			fn drop(&mut self) {
				self.0.fetch_sub(1, Ordering::SeqCst);
			}
		}
		let active = Arc::new(AtomicUsize::new(0));
		let ids = vec!["pending-a".to_string(), "failure".to_string(), "pending-b".to_string()];
		let result = fetch_source_batch(&ids, |id| {
			let active = Arc::clone(&active);
			async move {
				active.fetch_add(1, Ordering::SeqCst);
				let _request = ActiveRequest(active);
				if id == "failure" {
					tokio::task::yield_now().await;
					Err("synthetic retrieval failure".to_string())
				} else {
					std::future::pending().await
				}
			}
		})
		.await;
		assert_eq!(result.unwrap_err(), "synthetic retrieval failure");
		assert_eq!(active.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn rejects_single_or_unbounded_combined_requests() {
		let single = Request::builder().uri("/combined?posts=abc").body(Body::empty()).unwrap();
		assert!(requested_post_ids(&single).is_err());
		let many = (0..13).map(|index| format!("id{index}")).collect::<Vec<_>>().join(",");
		let request = Request::builder().uri(format!("/combined?posts={many}")).body(Body::empty()).unwrap();
		assert!(requested_post_ids(&request).is_err());
	}

	#[test]
	fn combined_groups_retain_their_source_post_and_sort() {
		let listing = json!({
			"data": {"children": [{
				"kind": "t1",
				"data": {
					"id": "root",
					"name": "t1_root",
					"parent_id": "t3_source",
					"author": "reader",
					"body": "Root",
					"body_html": "<p>Root</p>",
					"replies": ""
				}
			}]}
		});
		let group = ThreadModel::from_listing(
			&listing,
			"source",
			1,
			"/r/test/comments/source/thread/",
			"poster",
			"",
			&HashSet::new(),
			&[],
			&Preferences::default(),
		)
		.into_projection()
		.remove(0);
		let rendered = CombinedTemplate {
			prefs: Preferences::default(),
			url: "/combined?posts=source,copy".to_string(),
			source_url: "https://example.com/story".to_string(),
			discussions: vec![CombinedDiscussionView {
				activity: crate::activity::Visit::default(),
				title: "Source".to_string(),
				community: "test".to_string(),
				permalink: "/r/test/comments/source/thread/".to_string(),
				score: "1".to_string(),
				comments: "1".to_string(),
			}],
			comments: vec![CombinedCommentView {
				activity: crate::activity::Visit::default(),
				post_id: "source".to_string(),
				community: "test".to_string(),
				post_title: "Source".to_string(),
				post_permalink: "/r/test/comments/source/thread/".to_string(),
				raw_score: 1,
				group,
			}],
			filtered_comment_count: 0,
		}
		.render()
		.unwrap();
		assert!(rendered.contains("data-thread-post-id=\"t3_source\""));
		assert!(rendered.contains("data-thread-sort=\"top\""));
		crate::reading_fixtures::export("light", "combined.html", &rendered);
	}
}
