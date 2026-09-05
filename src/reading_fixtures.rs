//! Synthetic reading fixtures rendered by the real templates; never compiled into the service.
use crate::utils::{parse_post, FeedGroup, Post, Preferences};
use serde_json::json;

pub(crate) fn preferences(theme: &str) -> Preferences {
	let mut prefs = Preferences {
		theme: theme.into(),
		collapse_child_comments: "on".into(),
		active_feed: "field-notes".into(),
		..Preferences::default()
	};
	prefs.apply_reader_defaults();
	prefs
}

pub(crate) fn feeds() -> Vec<FeedGroup> {
	vec![
		FeedGroup {
			name: "Field notes".into(),
			slug: "field-notes".into(),
			communities: vec!["woodworking".into(), "gardening".into(), "photography".into()],
		},
		FeedGroup {
			name: "Science & technology".into(),
			slug: "science".into(),
			communities: vec!["science".into()],
		},
	]
}

pub(crate) async fn posts() -> Vec<Post> {
	let titles = [
		"A small workshop, one good workbench",
		"What a year of restoring a neglected garden taught me about patience, soil, and knowing when to leave things alone",
		"The light changes everything",
		"A field guide to repairing the things you already own",
		"This weekend’s reading",
		"A simple shelf, built to last",
		"Notes from the first harvest",
		"Working with the seasons",
	];
	let mut posts = Vec::new();
	for (i, title) in titles.iter().enumerate() {
		let id = format!("post{i}");
		let body = "<p>The best changes are the ones that make an ordinary day a little easier. I started with a clear surface, good light, and enough room to work.</p><p>There is no perfect setup. Start with what you have, notice what gets in your way, and improve one thing at a time.</p>";
		let mut post = parse_post(&json!({"data": {"id": id, "name": format!("t3_{id}"), "title": title, "subreddit": "woodworking", "author": "field_reader", "is_self": true, "selftext_html": body, "permalink": format!("/r/woodworking/comments/{id}/discussion/"), "num_comments": 24, "score": 128, "created_utc": 1700000000.0}})).await;
		post.rel_time = "2h ago".into();
		post.new_comments = if i == 0 { 3 } else { 0 };
		if i == 2 {
			post.post_type = "image".into();
			post.thumbnail.url = "/scenes/vale-light.webp".into();
			post.thumbnail.width = 144;
			post.thumbnail.height = 90;
			post.media.display_url = "/scenes/vale-light.webp".into();
			post.media.url = post.media.display_url.clone();
			post.media.width = 1536;
			post.media.height = 1024;
		}
		posts.push(post);
	}
	posts
}

pub(crate) fn comments() -> serde_json::Value {
	let mut child = json!({"kind": "t1", "data": {"id":"reply8", "name":"t1_reply8", "parent_id":"t1_reply7", "author":"patient_maker", "body_html":"<p>A deeper reply should remain just as comfortable to read on a small screen.</p>", "score": 7, "created_utc":1700000000.0, "replies":""}});
	for depth in (1..8).rev() {
		child = json!({"kind":"t1", "data": {"id":format!("reply{depth}"), "name":format!("t1_reply{depth}"), "parent_id":if depth == 1 { "t1_root0".into() } else {format!("t1_reply{}", depth - 1)}, "author":format!("maker_{depth}"), "body_html":"<p>Good point. I found that leaving a little space makes the whole project easier to use.</p>", "score":18, "created_utc":1700000000.0, "replies":{"data":{"children":[child]}}}});
	}
	let mut roots = Vec::new();
	for i in 0..12 {
		roots.push(json!({"kind":"t1", "data": {"id":format!("root{i}"), "name":format!("t1_root{i}"), "parent_id":"t3_post0", "author":format!("reader_{i}"), "body_html":"<p>Make the things you touch every day work well. Good tools disappear into the task, and the result feels natural.</p><p>I kept a short list of what slowed me down. That was more useful than starting over.</p>", "score":42 - i, "created_utc":1700000000.0, "replies": if i == 0 {json!({"data":{"children":[child.clone()]}})} else {json!("")}}}));
	}
	json!({"data":{"children":roots}})
}

pub(crate) fn export(theme: &str, file: &str, html: &str) {
	if let Some(path) = std::env::var_os("VALE_READING_FIXTURE_DIR") {
		let path = std::path::PathBuf::from(path).join(theme);
		std::fs::create_dir_all(&path).unwrap();
		std::fs::write(path.join(file), html).unwrap();
	}
}

#[test]
fn state_surface_fixtures() {
	use crate::utils::{ErrorTemplate, InfoTemplate, NSFWLandingTemplate, ResourceType};
	use askama::Template;
	for theme in ["dark", "light"] {
		let error = ErrorTemplate {
			msg: "The upstream request could not be completed.".into(),
			prefs: preferences(theme),
			url: "/r/woodworking".into(),
		}
		.render()
		.unwrap();
		assert!(error.contains("Try again"));
		export(theme, "error.html", &error);
		let info = InfoTemplate {
			msg: "Choose a community to start reading.".into(),
			prefs: preferences(theme),
			url: "/".into(),
		}
		.render()
		.unwrap();
		export(theme, "info.html", &info);
		let gate = NSFWLandingTemplate {
			res: "review-community".into(),
			res_type: ResourceType::Subreddit,
			prefs: preferences(theme),
			url: "/r/review-community".into(),
		}
		.render()
		.unwrap();
		export(theme, "gate.html", &gate);
	}
}
