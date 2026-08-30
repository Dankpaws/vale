use crate::{
	account,
	client::json,
	thread::{ThreadGroup, ThreadModel},
	utils::{get_filters, parse_post, template, Preferences},
};
use askama::Template;
use hyper::{header, Body, Request, Response, StatusCode};
use std::collections::{HashMap, HashSet};

const MAX_COMBINED_POSTS: usize = 12;
const MAX_COMBINED_ROOT_COMMENTS: usize = 500;

pub struct CombinedDiscussionView {
	pub title: String,
	pub community: String,
	pub permalink: String,
	pub score: String,
	pub comments: String,
}

pub struct CombinedCommentView {
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

	for id in ids {
		let response = json(format!("/comments/{id}.json?sort=top&limit=500&depth=10&raw_json=1"), true)
			.await
			.map_err(|message| format!("Reddit could not provide one of the grouped discussions: {message}"))?;
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

		let root_scores = response[1]["data"]["children"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|thing| thing["kind"].as_str() == Some("t1"))
			.filter_map(|thing| Some((thing["data"]["id"].as_str()?.to_string(), thing["data"]["score"].as_i64().unwrap_or_default())))
			.collect::<HashMap<_, _>>();
		let groups = ThreadModel::from_listing(
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
		for group in groups.into_iter().filter(|group| group.root.kind == "t1") {
			combined_comments.push(CombinedCommentView {
				post_id: post.id.clone(),
				community: post.community.clone(),
				post_title: post.title.clone(),
				post_permalink: post.permalink.clone(),
				raw_score: root_scores.get(&group.root.id).copied().unwrap_or_default(),
				group,
			});
		}
		discussions.push(CombinedDiscussionView {
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
				title: "Source".to_string(),
				community: "test".to_string(),
				permalink: "/r/test/comments/source/thread/".to_string(),
				score: "1".to_string(),
				comments: "1".to_string(),
			}],
			comments: vec![CombinedCommentView {
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
	}
}
