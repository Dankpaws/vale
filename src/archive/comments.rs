//! Capturing a post's comment tree.

use super::*;

pub(super) fn issue(area: &str, message: impl Into<String>) -> ArchiveIssue {
	ArchiveIssue {
		area: area.to_string(),
		message: message.into(),
	}
}

pub(super) fn collect_comment_thing(
	thing: &Value,
	comments: &mut HashMap<String, Value>,
	order: &mut Vec<String>,
	more: &mut VecDeque<String>,
	seen_more: &mut HashSet<String>,
	issues: &mut Vec<ArchiveIssue>,
) {
	match thing["kind"].as_str().unwrap_or_default() {
		"t1" => {
			let id = val(thing, "id");
			if !id.is_empty() && comments.len() < MAX_ARCHIVE_COMMENTS && !comments.contains_key(&id) {
				order.push(id.clone());
				comments.insert(id, thing.clone());
			}
			if let Some(children) = thing["data"]["replies"]["data"]["children"].as_array() {
				for child in children {
					collect_comment_thing(child, comments, order, more, seen_more, issues);
				}
			}
		}
		"more" => {
			let children = thing["data"]["children"].as_array();
			let mut found = false;
			if let Some(children) = children {
				for child in children.iter().filter_map(Value::as_str) {
					if seen_more.insert(child.to_string()) {
						more.push_back(child.to_string());
					}
					found = true;
				}
			}
			if !found && thing["data"]["count"].as_i64().unwrap_or_default() > 0 {
				issues.push(issue(
					"comments",
					"Reddit reported additional comments without exposing identifiers that Vale could retrieve.",
				));
			}
		}
		_ => {}
	}
}

pub(super) fn comment_score(thing: &Value) -> i64 {
	thing["data"]["score"].as_i64().unwrap_or_default()
}

pub(super) fn archived_comment_from_thing(
	id: &str,
	comments: &HashMap<String, Value>,
	children: &HashMap<String, Vec<String>>,
	visiting: &mut HashSet<String>,
	issues: &mut Vec<ArchiveIssue>,
) -> Option<ArchivedComment> {
	if !visiting.insert(id.to_string()) {
		return None;
	}
	let thing = comments.get(id)?;
	let data = &thing["data"];
	let raw_body = val(thing, "body");
	let mut body_html = if data["body_html"].is_string() {
		rewrite_emotes(&data["media_metadata"], val(thing, "body_html"))
	} else {
		format!("<div class=\"md\"><p>{}</p></div>", htmlescape::encode_minimal(&raw_body))
	};
	body_html = rewrite_urls(&body_html);
	body_html = match normalize_archive_comment_headings(&body_html) {
		Ok(normalized) => normalized,
		Err(_) => {
			issues.push(issue(
				"comments",
				format!("Comment {id} contained ambiguous HTML; Vale stored its escaped raw Markdown instead."),
			));
			format!("<div class=\"md\"><p>{}</p></div>", htmlescape::encode_minimal(&raw_body))
		}
	};
	let timestamp = data["created_utc"].as_f64().unwrap_or_default().round() as i64;
	let replies = children
		.get(id)
		.into_iter()
		.flatten()
		.filter_map(|child| archived_comment_from_thing(child, comments, children, visiting, issues))
		.collect();
	visiting.remove(id);
	Some(ArchivedComment {
		id: id.to_string(),
		parent_id: val(thing, "parent_id"),
		author: val(thing, "author"),
		body_html,
		created: format_timestamp(timestamp),
		score: comment_score(thing),
		score_hidden: data["score_hidden"].as_bool().unwrap_or_default(),
		replies,
	})
}

pub(super) async fn capture_comments(initial: &Value, post_id: &str, expected: usize) -> CommentCapture {
	let mut comments = HashMap::new();
	let mut order = Vec::new();
	let mut more = VecDeque::new();
	let mut seen_more = HashSet::new();
	let mut issues = Vec::new();
	if let Some(children) = initial[1]["data"]["children"].as_array() {
		for thing in children {
			collect_comment_thing(thing, &mut comments, &mut order, &mut more, &mut seen_more, &mut issues);
		}
	} else {
		issues.push(issue("comments", "Reddit did not return a comment listing for this post."));
	}

	let mut fetched_things = Vec::new();
	let mut requests = 0usize;
	while !more.is_empty() && comments.len() < MAX_ARCHIVE_COMMENTS && requests < MAX_MORE_REQUESTS {
		let mut batch = Vec::new();
		while batch.len() < MORE_CHILDREN_BATCH {
			let Some(id) = more.pop_front() else {
				break;
			};
			if !comments.contains_key(&id) {
				batch.push(id);
			}
		}
		if batch.is_empty() {
			continue;
		}
		requests += 1;
		let query = url::form_urlencoded::Serializer::new(String::new())
			.append_pair("api_type", "json")
			.append_pair("link_id", &format!("t3_{post_id}"))
			.append_pair("children", &batch.join(","))
			.append_pair("sort", "top")
			.append_pair("raw_json", "1")
			.finish();
		let response = match json(format!("/api/morechildren.json?{query}"), true).await {
			Ok(response) => response,
			Err(_) => {
				issues.push(issue(
					"comments",
					format!(
						"Reddit stopped the continuation crawl after {} archived comments; the remaining branches are listed as omissions.",
						comments.len()
					),
				));
				break;
			}
		};
		let things = response["json"]["data"]["things"]
			.as_array()
			.or_else(|| response["data"]["things"].as_array())
			.cloned()
			.unwrap_or_default();
		if things.is_empty() {
			issues.push(issue(
				"comments",
				format!(
					"Reddit returned no data for {} queued comment identifiers; those branches could not be preserved.",
					batch.len()
				),
			));
			continue;
		}
		for thing in things {
			fetched_things.push(thing.clone());
			collect_comment_thing(&thing, &mut comments, &mut order, &mut more, &mut seen_more, &mut issues);
		}
	}

	if !more.is_empty() || comments.len() >= MAX_ARCHIVE_COMMENTS || requests >= MAX_MORE_REQUESTS {
		issues.push(issue(
			"comments",
			format!(
				"The safety boundary stopped comment expansion at {} comments and {} continuation requests.",
				comments.len(),
				requests
			),
		));
	}
	if expected > comments.len() {
		issues.push(issue(
			"comments",
			format!(
				"Reddit reported {expected} comments when capture began; {} distinct comments were retrievable.",
				comments.len()
			),
		));
	}

	let order_index = order.iter().enumerate().map(|(index, id)| (id.clone(), index)).collect::<HashMap<_, _>>();
	let mut children: HashMap<String, Vec<String>> = HashMap::new();
	let mut roots = Vec::new();
	for id in comments.keys() {
		let parent = val(comments.get(id).unwrap_or(&Value::Null), "parent_id");
		let parent_comment = parent.strip_prefix("t1_");
		if parent == format!("t3_{post_id}") || parent_comment.is_none() || parent_comment.is_some_and(|parent| !comments.contains_key(parent)) {
			roots.push(id.clone());
		} else {
			children.entry(parent_comment.unwrap_or_default().to_string()).or_default().push(id.clone());
		}
	}
	let sorter = |left: &String, right: &String| {
		let left_value = comments.get(left).unwrap_or(&Value::Null);
		let right_value = comments.get(right).unwrap_or(&Value::Null);
		comment_score(right_value)
			.cmp(&comment_score(left_value))
			.then_with(|| order_index.get(left).cmp(&order_index.get(right)))
	};
	roots.sort_by(sorter);
	for values in children.values_mut() {
		values.sort_by(sorter);
	}
	let mut visiting = HashSet::new();
	let archived = roots
		.iter()
		.filter_map(|id| archived_comment_from_thing(id, &comments, &children, &mut visiting, &mut issues))
		.collect();
	CommentCapture {
		comments: archived,
		things: fetched_things,
		count: comments.len(),
		issues,
	}
}
