use crate::config::get_setting;
use crate::utils::{comment_matches_keywords, format_num, rewrite_emotes, time, val, Author, Awards, Comment, Flair, FlairPart, Preferences};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentFilterState {
	Visible,
	AuthorFiltered,
	KeywordFiltered,
}

impl CommentFilterState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Visible => "visible",
			Self::AuthorFiltered => "author-filtered",
			Self::KeywordFiltered => "keyword-filtered",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationState {
	Pending,
	Unavailable,
}

impl ContinuationState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Unavailable => "unavailable",
		}
	}
}

pub struct NormalizedComment {
	pub node_id: String,
	pub id: String,
	pub parent_id: String,
	pub ancestor_path: Vec<String>,
	pub ancestor_path_complete: bool,
	pub depth: usize,
	pub preorder: usize,
	pub child_ids: Vec<String>,
	pub raw_body: String,
	pub body: String,
	pub author: Author,
	pub score: (String, String),
	pub rel_time: String,
	pub created: String,
	pub edited: (String, String),
	pub highlighted: bool,
	pub awards: Awards,
	pub collapsed: bool,
	pub filter_state: CommentFilterState,
}

pub struct CommentContinuation {
	pub node_id: String,
	pub parent_id: String,
	pub ancestor_path: Vec<String>,
	pub ancestor_path_complete: bool,
	pub depth: usize,
	pub preorder: usize,
	pub count: usize,
	pub child_ids: Vec<String>,
	pub state: ContinuationState,
}

pub enum ThreadNode {
	Comment(Box<NormalizedComment>),
	Continuation(CommentContinuation),
}

impl ThreadNode {
	pub fn node_id(&self) -> &str {
		match self {
			Self::Comment(comment) => &comment.node_id,
			Self::Continuation(continuation) => &continuation.node_id,
		}
	}

	pub fn parent_id(&self) -> &str {
		match self {
			Self::Comment(comment) => &comment.parent_id,
			Self::Continuation(continuation) => &continuation.parent_id,
		}
	}

	pub fn ancestor_path(&self) -> &[String] {
		match self {
			Self::Comment(comment) => &comment.ancestor_path,
			Self::Continuation(continuation) => &continuation.ancestor_path,
		}
	}

	pub fn ancestor_path_complete(&self) -> bool {
		match self {
			Self::Comment(comment) => comment.ancestor_path_complete,
			Self::Continuation(continuation) => continuation.ancestor_path_complete,
		}
	}

	pub fn depth(&self) -> usize {
		match self {
			Self::Comment(comment) => comment.depth,
			Self::Continuation(continuation) => continuation.depth,
		}
	}

	pub fn preorder(&self) -> usize {
		match self {
			Self::Comment(comment) => comment.preorder,
			Self::Continuation(continuation) => continuation.preorder,
		}
	}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
	pub comment_count: usize,
	pub reported_comment_count: usize,
	pub continuation_count: usize,
	pub pending_continuation_count: usize,
	pub unavailable_continuation_count: usize,
	pub estimated_remaining: usize,
	pub unaccounted_comment_count: usize,
	pub incomplete_ancestry_count: usize,
	pub complete: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSearch {
	pub query: String,
	pub match_count: usize,
	pub match_ids: Vec<String>,
	pub context_ids: Vec<String>,
	pub searched_comment_count: usize,
	pub excluded_filtered_comment_count: usize,
	pub coverage_complete: bool,
}

pub struct ThreadGroup {
	pub id: String,
	pub root: Comment,
	pub descendants: Vec<Comment>,
}

struct ProjectionContext<'a> {
	root_id: &'a str,
	root_depth: usize,
	reply_region_id: &'a str,
	projected_reply_count: usize,
	projected_replies_complete: bool,
	is_group_root: bool,
	parent_authors: &'a HashMap<String, String>,
	projected_ids: &'a HashSet<String>,
	search_match_ids: &'a HashSet<String>,
	search_context_ids: &'a HashSet<String>,
}

impl ThreadSummary {
	pub fn complete(self) -> bool {
		self.complete
	}
}

struct ThreadContext {
	post_link: String,
	post_author: String,
	prefs: Preferences,
}

#[derive(Clone)]
struct ParentContext {
	node_id: String,
	ancestor_path: Vec<String>,
	ancestor_path_complete: bool,
	depth: usize,
}

pub struct ThreadModel {
	post_id: String,
	reported_comment_count: usize,
	roots: Vec<String>,
	preorder: Vec<String>,
	nodes: HashMap<String, ThreadNode>,
	context: ThreadContext,
}

impl ThreadModel {
	#[allow(clippy::too_many_arguments)]
	pub fn from_listing(
		json: &Value,
		post_id: &str,
		reported_comment_count: usize,
		post_link: &str,
		post_author: &str,
		highlighted_comment: &str,
		filters: &HashSet<String>,
		keywords: &[String],
		prefs: &Preferences,
	) -> Self {
		let mut model = Self {
			post_id: canonical_post_id(post_id),
			reported_comment_count,
			roots: Vec::new(),
			preorder: Vec::new(),
			nodes: HashMap::new(),
			context: ThreadContext {
				post_link: post_link.to_string(),
				post_author: post_author.to_string(),
				prefs: prefs.clone(),
			},
		};
		model.roots = model.ingest_listing(json, None, highlighted_comment, filters, keywords);
		model
	}

	pub fn post_id(&self) -> &str {
		&self.post_id
	}

	pub fn roots(&self) -> &[String] {
		&self.roots
	}

	pub fn preorder_ids(&self) -> &[String] {
		&self.preorder
	}

	pub fn node(&self, id: &str) -> Option<&ThreadNode> {
		self.nodes.get(id)
	}

	pub fn summary(&self) -> ThreadSummary {
		let mut summary = ThreadSummary {
			reported_comment_count: self.reported_comment_count,
			..ThreadSummary::default()
		};
		for node in self.nodes.values() {
			match node {
				ThreadNode::Comment(comment) => {
					summary.comment_count += 1;
					if !comment.ancestor_path_complete {
						summary.incomplete_ancestry_count += 1;
					}
				}
				ThreadNode::Continuation(continuation) => {
					summary.continuation_count += 1;
					summary.estimated_remaining += continuation.count.max(continuation.child_ids.len());
					match continuation.state {
						ContinuationState::Pending => summary.pending_continuation_count += 1,
						ContinuationState::Unavailable => summary.unavailable_continuation_count += 1,
					}
				}
			}
		}
		let reported_gap = summary.reported_comment_count.saturating_sub(summary.comment_count);
		summary.unaccounted_comment_count = reported_gap.saturating_sub(summary.estimated_remaining);
		summary.estimated_remaining = summary.estimated_remaining.max(reported_gap);
		summary.complete = summary.continuation_count == 0 && summary.unaccounted_comment_count == 0 && summary.incomplete_ancestry_count == 0;
		summary
	}

	pub fn filtered_comment_count(&self) -> usize {
		self
			.nodes
			.values()
			.filter(|node| matches!(node, ThreadNode::Comment(comment) if comment.filter_state == CommentFilterState::KeywordFiltered))
			.count()
	}

	pub fn search(&self, query: &str) -> ThreadSearch {
		let query = query.trim();
		let normalized_query = query.to_lowercase();
		let mut result = ThreadSearch {
			query: query.to_string(),
			coverage_complete: self.summary().complete,
			..ThreadSearch::default()
		};
		if normalized_query.is_empty() {
			return result;
		}

		let mut context_ids = HashSet::new();
		for id in &self.preorder {
			let Some(ThreadNode::Comment(comment)) = self.nodes.get(id) else {
				continue;
			};
			if comment.filter_state != CommentFilterState::Visible {
				result.excluded_filtered_comment_count += 1;
				continue;
			}
			result.searched_comment_count += 1;
			if !comment.raw_body.to_lowercase().contains(&normalized_query) {
				continue;
			}
			result.match_ids.push(id.clone());
			for ancestor_id in &comment.ancestor_path {
				if matches!(self.nodes.get(ancestor_id), Some(ThreadNode::Comment(_))) {
					context_ids.insert(ancestor_id.clone());
				}
			}
		}
		let matches = result.match_ids.iter().collect::<HashSet<_>>();
		result.context_ids = self.preorder.iter().filter(|id| context_ids.contains(*id) && !matches.contains(id)).cloned().collect();
		result.match_count = result.match_ids.len();
		result
	}

	pub fn validate(&self) -> Result<(), String> {
		if self.preorder.len() != self.nodes.len() {
			return Err("thread preorder and node map have different lengths".to_string());
		}
		let unique = self.preorder.iter().collect::<HashSet<_>>();
		if unique.len() != self.preorder.len() {
			return Err("thread preorder contains a duplicate node".to_string());
		}
		for (position, id) in self.preorder.iter().enumerate() {
			let Some(node) = self.nodes.get(id) else {
				return Err(format!("thread preorder references missing node {id}"));
			};
			if node.node_id() != id || node.preorder() != position {
				return Err(format!("thread node {id} has inconsistent identity or preorder"));
			}
			if node.ancestor_path_complete() && node.depth() != node.ancestor_path().len() {
				return Err(format!("thread node {id} has inconsistent complete ancestry"));
			}
		}
		for root in &self.roots {
			if !self.nodes.contains_key(root) {
				return Err(format!("thread root {root} is missing"));
			}
		}
		for node in self.nodes.values() {
			let ThreadNode::Comment(comment) = node else {
				continue;
			};
			for child_id in &comment.child_ids {
				let Some(child) = self.nodes.get(child_id) else {
					return Err(format!("thread node {} references missing child {child_id}", comment.node_id));
				};
				if child.parent_id() != comment.node_id {
					return Err(format!("thread child {child_id} has the wrong parent"));
				}
			}
		}
		Ok(())
	}

	pub fn into_projection(self) -> Vec<ThreadGroup> {
		self.into_search_projection(&ThreadSearch::default())
	}

	pub fn into_search_projection(mut self, search: &ThreadSearch) -> Vec<ThreadGroup> {
		let parent_authors = self.comment_authors();
		let search_match_ids = search.match_ids.iter().cloned().collect::<HashSet<_>>();
		let search_context_ids = search.context_ids.iter().cloned().collect::<HashSet<_>>();
		let roots = std::mem::take(&mut self.roots);
		roots
			.into_iter()
			.filter_map(|root_id| {
				let node_ids = self.subtree_preorder(&root_id);
				let root_depth = self.nodes.get(&root_id).map_or(0, ThreadNode::depth);
				let projected_ids = node_ids.iter().cloned().collect::<HashSet<_>>();
				let reply_region_id = format!("comment-replies-{}", base_id(&root_id));
				let projected_reply_count = node_ids.iter().skip(1).filter(|id| matches!(self.nodes.get(*id), Some(ThreadNode::Comment(_)))).count();
				let projected_replies_complete = node_ids
					.iter()
					.skip(1)
					.all(|id| matches!(self.nodes.get(id), Some(ThreadNode::Comment(comment)) if comment.ancestor_path_complete));
				let root = self.take_view(
					&root_id,
					&ProjectionContext {
						root_id: &root_id,
						root_depth,
						reply_region_id: &reply_region_id,
						projected_reply_count,
						projected_replies_complete,
						is_group_root: true,
						parent_authors: &parent_authors,
						projected_ids: &projected_ids,
						search_match_ids: &search_match_ids,
						search_context_ids: &search_context_ids,
					},
				)?;
				let descendants = node_ids
					.into_iter()
					.skip(1)
					.filter_map(|id| {
						self.take_view(
							&id,
							&ProjectionContext {
								root_id: &root_id,
								root_depth,
								reply_region_id: &reply_region_id,
								projected_reply_count: 0,
								projected_replies_complete: true,
								is_group_root: false,
								parent_authors: &parent_authors,
								projected_ids: &projected_ids,
								search_match_ids: &search_match_ids,
								search_context_ids: &search_context_ids,
							},
						)
					})
					.collect();
				Some(ThreadGroup { id: root_id, root, descendants })
			})
			.collect()
	}

	fn comment_authors(&self) -> HashMap<String, String> {
		self
			.nodes
			.iter()
			.filter_map(|(id, node)| match node {
				ThreadNode::Comment(comment) => Some((id.clone(), comment.author.name.clone())),
				ThreadNode::Continuation(_) => None,
			})
			.collect()
	}

	fn subtree_preorder(&self, root_id: &str) -> Vec<String> {
		let mut ids = Vec::new();
		self.collect_subtree_preorder(root_id, &mut ids);
		ids
	}

	fn collect_subtree_preorder(&self, id: &str, ids: &mut Vec<String>) {
		let Some(node) = self.nodes.get(id) else {
			return;
		};
		ids.push(id.to_string());
		if let ThreadNode::Comment(comment) = node {
			for child_id in &comment.child_ids {
				self.collect_subtree_preorder(child_id, ids);
			}
		}
	}

	fn ingest_listing(&mut self, json: &Value, parent: Option<&ParentContext>, highlighted_comment: &str, filters: &HashSet<String>, keywords: &[String]) -> Vec<String> {
		json["data"]["children"]
			.as_array()
			.into_iter()
			.flatten()
			.filter_map(|thing| self.ingest_thing(thing, parent, highlighted_comment, filters, keywords))
			.collect()
	}

	fn ingest_thing(&mut self, thing: &Value, parent: Option<&ParentContext>, highlighted_comment: &str, filters: &HashSet<String>, keywords: &[String]) -> Option<String> {
		match thing["kind"].as_str().unwrap_or_default() {
			"t1" => self.ingest_comment(thing, parent, highlighted_comment, filters, keywords),
			"more" => self.ingest_continuation(thing, parent),
			_ => None,
		}
	}

	fn ingest_comment(&mut self, thing: &Value, parent: Option<&ParentContext>, highlighted_comment: &str, filters: &HashSet<String>, keywords: &[String]) -> Option<String> {
		let data = &thing["data"];
		let id = val(thing, "id");
		if id.is_empty() {
			return None;
		}
		let node_id = canonical_comment_id(data["name"].as_str().unwrap_or_default(), &id);
		if self.nodes.contains_key(&node_id) {
			return None;
		}
		let topology = self.topology(data, parent);
		let preorder = self.preorder.len();
		let raw_body = val(thing, "body");
		let body = if (val(thing, "author") == "[deleted]" && raw_body == "[removed]") || raw_body == "[ Removed by Reddit ]" {
			let frontend = get_setting("REDLIB_PUSHSHIFT_FRONTEND").unwrap_or_else(|| String::from(crate::config::DEFAULT_PUSHSHIFT_FRONTEND));
			format!(
				"<div class=\"md\"><p>[removed] — <a href=\"https://{frontend}{}{id}\">view removed comment</a></p></div>",
				self.context.post_link,
			)
		} else {
			rewrite_emotes(&data["media_metadata"], val(thing, "body_html"))
		};
		let unix_time = data["created_utc"].as_f64().unwrap_or_default();
		let (rel_time, created) = time(unix_time);
		let edited = data["edited"].as_f64().map_or((String::new(), String::new()), time);
		let score = data["score"].as_i64().unwrap_or_default();
		let author = Author {
			name: val(thing, "author"),
			flair: Flair {
				flair_parts: FlairPart::parse(
					data["author_flair_type"].as_str().unwrap_or_default(),
					data["author_flair_richtext"].as_array(),
					data["author_flair_text"].as_str(),
				),
				text: val(thing, "link_flair_text"),
				background_color: val(thing, "author_flair_background_color"),
				foreground_color: val(thing, "author_flair_text_color"),
			},
			distinguished: val(thing, "distinguished"),
		};
		let author_filtered = filters.contains(&format!("u_{}", author.name));
		let filter_state = if author_filtered {
			CommentFilterState::AuthorFiltered
		} else if comment_matches_keywords(&raw_body, keywords) {
			CommentFilterState::KeywordFiltered
		} else {
			CommentFilterState::Visible
		};
		let is_moderator_comment = data["distinguished"].as_str().unwrap_or_default() == "moderator";
		let is_stickied = data["stickied"].as_bool().unwrap_or_default();
		let collapsed = (is_moderator_comment && is_stickied) || author_filtered;
		let parent_context = ParentContext {
			node_id: node_id.clone(),
			ancestor_path: topology.ancestor_path.iter().cloned().chain(std::iter::once(node_id.clone())).collect(),
			ancestor_path_complete: topology.ancestor_path_complete,
			depth: topology.depth + 1,
		};
		self.preorder.push(node_id.clone());
		self.nodes.insert(
			node_id.clone(),
			ThreadNode::Comment(Box::new(NormalizedComment {
				node_id: node_id.clone(),
				id,
				parent_id: topology.parent_id,
				ancestor_path: topology.ancestor_path,
				ancestor_path_complete: topology.ancestor_path_complete,
				depth: topology.depth,
				preorder,
				child_ids: Vec::new(),
				raw_body,
				body,
				author,
				score: if data["score_hidden"].as_bool().unwrap_or_default() {
					("\u{2022}".to_string(), "Hidden".to_string())
				} else {
					format_num(score)
				},
				rel_time,
				created,
				edited,
				highlighted: val(thing, "id") == highlighted_comment,
				awards: Awards::parse(&data["all_awardings"]),
				collapsed,
				filter_state,
			})),
		);
		let child_ids = if data["replies"].is_object() {
			self.ingest_listing(&data["replies"], Some(&parent_context), highlighted_comment, filters, keywords)
		} else {
			Vec::new()
		};
		if let Some(ThreadNode::Comment(comment)) = self.nodes.get_mut(&node_id) {
			comment.child_ids = child_ids;
		}
		Some(node_id)
	}

	fn ingest_continuation(&mut self, thing: &Value, parent: Option<&ParentContext>) -> Option<String> {
		let data = &thing["data"];
		let count = data["count"].as_u64().unwrap_or_default() as usize;
		let child_ids = data["children"]
			.as_array()
			.into_iter()
			.flatten()
			.filter_map(Value::as_str)
			.filter(|id| !id.is_empty())
			.map(|id| canonical_comment_id(id, id))
			.collect::<Vec<_>>();
		if count == 0 && child_ids.is_empty() {
			return None;
		}
		let topology = self.topology(data, parent);
		let node_id = stable_continuation_id(&topology.parent_id, &child_ids, count, val(thing, "id").as_str());
		if self.nodes.contains_key(&node_id) {
			return None;
		}
		let preorder = self.preorder.len();
		let state = if child_ids.is_empty() {
			ContinuationState::Unavailable
		} else {
			ContinuationState::Pending
		};
		self.preorder.push(node_id.clone());
		self.nodes.insert(
			node_id.clone(),
			ThreadNode::Continuation(CommentContinuation {
				node_id: node_id.clone(),
				parent_id: topology.parent_id,
				ancestor_path: topology.ancestor_path,
				ancestor_path_complete: topology.ancestor_path_complete,
				depth: topology.depth,
				preorder,
				count,
				child_ids,
				state,
			}),
		);
		Some(node_id)
	}

	fn topology(&self, data: &Value, parent: Option<&ParentContext>) -> NodeTopology {
		if let Some(parent) = parent {
			return NodeTopology {
				parent_id: parent.node_id.clone(),
				ancestor_path: parent.ancestor_path.clone(),
				ancestor_path_complete: parent.ancestor_path_complete,
				depth: parent.depth,
			};
		}
		let source_parent = data["parent_id"].as_str().unwrap_or_default();
		if source_parent.starts_with("t1_") {
			NodeTopology {
				parent_id: source_parent.to_string(),
				ancestor_path: vec![source_parent.to_string()],
				ancestor_path_complete: false,
				depth: data["depth"].as_u64().unwrap_or(1) as usize,
			}
		} else {
			NodeTopology {
				parent_id: self.post_id.clone(),
				ancestor_path: Vec::new(),
				ancestor_path_complete: true,
				depth: 0,
			}
		}
	}

	fn take_view(&mut self, id: &str, projection: &ProjectionContext<'_>) -> Option<Comment> {
		let node = self.nodes.remove(id)?;
		let prefs = self.context.prefs.clone();
		match node {
			ThreadNode::Comment(comment) => {
				let relative_depth = comment.depth.saturating_sub(projection.root_depth);
				let parent_available = projection.projected_ids.contains(&comment.parent_id);
				let parent_author = projection.parent_authors.get(&comment.parent_id).cloned().unwrap_or_default();
				let search_match = projection.search_match_ids.contains(&comment.node_id);
				let search_context = projection.search_context_ids.contains(&comment.node_id);
				Some(Comment {
					id: comment.id,
					kind: "t1".to_string(),
					parent_id: base_id(&comment.parent_id),
					parent_kind: thing_kind(&comment.parent_id),
					post_link: self.context.post_link.clone(),
					post_author: self.context.post_author.clone(),
					body: comment.body,
					author: comment.author,
					score: comment.score,
					rel_time: comment.rel_time,
					created: comment.created,
					edited: comment.edited,
					replies: Vec::new(),
					highlighted: comment.highlighted,
					awards: comment.awards,
					collapsed: comment.collapsed,
					is_filtered: comment.filter_state == CommentFilterState::AuthorFiltered,
					is_keyword_filtered: comment.filter_state == CommentFilterState::KeywordFiltered,
					more_count: 0,
					prefs,
					node_id: comment.node_id,
					parent_node_id: comment.parent_id,
					ancestor_path: comment.ancestor_path.join(" "),
					ancestor_path_complete: comment.ancestor_path_complete,
					depth: comment.depth,
					preorder: comment.preorder,
					filter_state: comment.filter_state.as_str().to_string(),
					continuation_state: String::new(),
					continuation_children: String::new(),
					thread_root_id: projection.root_id.to_string(),
					parent_author,
					parent_available,
					wide_indent: relative_depth.min(6) * 14,
					narrow_indent: relative_depth.min(2) * 8,
					reply_region_id: projection.reply_region_id.to_string(),
					projected_reply_count: projection.projected_reply_count,
					projected_replies_complete: projection.projected_replies_complete,
					is_group_root: projection.is_group_root,
					search_match,
					search_context,
				})
			}
			ThreadNode::Continuation(continuation) => {
				let relative_depth = continuation.depth.saturating_sub(projection.root_depth);
				let parent_available = projection.projected_ids.contains(&continuation.parent_id);
				let parent_author = projection.parent_authors.get(&continuation.parent_id).cloned().unwrap_or_default();
				Some(Comment {
					id: continuation.node_id.clone(),
					kind: "more".to_string(),
					parent_id: base_id(&continuation.parent_id),
					parent_kind: thing_kind(&continuation.parent_id),
					post_link: self.context.post_link.clone(),
					post_author: self.context.post_author.clone(),
					body: String::new(),
					author: empty_author(),
					score: (String::new(), String::new()),
					rel_time: String::new(),
					created: String::new(),
					edited: (String::new(), String::new()),
					replies: Vec::new(),
					highlighted: false,
					awards: Awards::default(),
					collapsed: false,
					is_filtered: false,
					is_keyword_filtered: false,
					more_count: continuation.count as i64,
					prefs,
					node_id: continuation.node_id,
					parent_node_id: continuation.parent_id,
					ancestor_path: continuation.ancestor_path.join(" "),
					ancestor_path_complete: continuation.ancestor_path_complete,
					depth: continuation.depth,
					preorder: continuation.preorder,
					filter_state: String::new(),
					continuation_state: continuation.state.as_str().to_string(),
					continuation_children: continuation.child_ids.join(","),
					thread_root_id: projection.root_id.to_string(),
					parent_author,
					parent_available,
					wide_indent: relative_depth.min(6) * 14,
					narrow_indent: relative_depth.min(2) * 8,
					reply_region_id: projection.reply_region_id.to_string(),
					projected_reply_count: projection.projected_reply_count,
					projected_replies_complete: projection.projected_replies_complete,
					is_group_root: projection.is_group_root,
					search_match: false,
					search_context: false,
				})
			}
		}
	}
}

struct NodeTopology {
	parent_id: String,
	ancestor_path: Vec<String>,
	ancestor_path_complete: bool,
	depth: usize,
}

fn canonical_post_id(id: &str) -> String {
	if id.starts_with("t3_") {
		id.to_string()
	} else {
		format!("t3_{id}")
	}
}

fn canonical_comment_id(name: &str, id: &str) -> String {
	if name.starts_with("t1_") {
		name.to_string()
	} else if id.starts_with("t1_") {
		id.to_string()
	} else {
		format!("t1_{id}")
	}
}

fn thing_kind(id: &str) -> String {
	id.split_once('_').map(|(kind, _)| kind.to_string()).unwrap_or_default()
}

fn base_id(id: &str) -> String {
	id.split_once('_').map(|(_, id)| id.to_string()).unwrap_or_else(|| id.to_string())
}

fn stable_continuation_id(parent_id: &str, child_ids: &[String], count: usize, source_id: &str) -> String {
	let mut hasher = Sha256::new();
	hasher.update(b"vale-comment-continuation-v1\0");
	hasher.update(parent_id.as_bytes());
	hasher.update(b"\0");
	hasher.update(count.to_string().as_bytes());
	hasher.update(b"\0");
	hasher.update(source_id.as_bytes());
	for child_id in child_ids {
		hasher.update(b"\0");
		hasher.update(child_id.as_bytes());
	}
	let digest = hasher.finalize();
	let mut encoded = String::with_capacity(24);
	for byte in &digest[..12] {
		let _ = write!(encoded, "{byte:02x}");
	}
	format!("more_{encoded}")
}

fn empty_author() -> Author {
	Author {
		name: String::new(),
		flair: Flair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		},
		distinguished: String::new(),
	}
}

pub fn count_keyword_filtered(groups: &[ThreadGroup]) -> usize {
	groups
		.iter()
		.map(|group| usize::from(group.root.is_keyword_filtered) + group.descendants.iter().filter(|comment| comment.is_keyword_filtered).count())
		.sum()
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn fixture() -> Value {
		serde_json::from_str(include_str!("../tests/fixtures/thread/normalized-listing.json")).unwrap()
	}

	fn model() -> ThreadModel {
		ThreadModel::from_listing(
			&fixture(),
			"post",
			9,
			"/r/test/comments/post/thread/",
			"post-author",
			"child-a",
			&HashSet::from(["u_filtered-user".to_string()]),
			&["spoiler phrase".to_string()],
			&Preferences::default(),
		)
	}

	#[test]
	fn recursive_listing_normalizes_identity_topology_filters_and_coverage() {
		let model = model();
		assert_eq!(model.post_id(), "t3_post");
		assert_eq!(model.roots().len(), 4);
		assert_eq!(model.preorder_ids().len(), 6);
		assert!(model.validate().is_ok());

		let ThreadNode::Comment(root) = model.node("t1_root-a").unwrap() else {
			panic!("root-a should be a comment");
		};
		assert_eq!(root.parent_id, "t3_post");
		assert!(root.ancestor_path.is_empty());
		assert!(root.ancestor_path_complete);
		assert_eq!(root.depth, 0);
		assert_eq!(root.preorder, 0);
		assert_eq!(root.child_ids.len(), 1);

		let ThreadNode::Comment(child) = model.node("t1_child-a").unwrap() else {
			panic!("child-a should be a comment");
		};
		assert_eq!(child.parent_id, "t1_root-a");
		assert_eq!(child.ancestor_path, ["t1_root-a"]);
		assert_eq!(child.depth, 1);
		assert_eq!(child.preorder, 1);
		assert!(child.highlighted);

		let continuation_id = child.child_ids.first().unwrap();
		let ThreadNode::Continuation(continuation) = model.node(continuation_id).unwrap() else {
			panic!("child continuation should be explicit");
		};
		assert_eq!(continuation.parent_id, "t1_child-a");
		assert_eq!(continuation.ancestor_path, ["t1_root-a", "t1_child-a"]);
		assert_eq!(continuation.depth, 2);
		assert_eq!(continuation.child_ids, ["t1_deep-a", "t1_deep-b"]);
		assert_eq!(continuation.state, ContinuationState::Pending);

		let ThreadNode::Comment(filtered) = model.node("t1_root-b").unwrap() else {
			panic!("root-b should be a comment");
		};
		assert_eq!(filtered.filter_state, CommentFilterState::AuthorFiltered);
		let ThreadNode::Comment(keyword) = model.node("t1_root-c").unwrap() else {
			panic!("root-c should be a comment");
		};
		assert_eq!(keyword.filter_state, CommentFilterState::KeywordFiltered);

		assert_eq!(
			model.summary(),
			ThreadSummary {
				comment_count: 4,
				reported_comment_count: 9,
				continuation_count: 2,
				pending_continuation_count: 1,
				unavailable_continuation_count: 1,
				estimated_remaining: 5,
				unaccounted_comment_count: 0,
				incomplete_ancestry_count: 0,
				complete: false,
			}
		);
		assert!(!model.summary().complete());
		assert_eq!(model.filtered_comment_count(), 1);
	}

	#[test]
	fn continuation_identity_and_projection_are_deterministic() {
		let first = model();
		let second = model();
		assert_eq!(first.preorder_ids(), second.preorder_ids());
		let groups = first.into_projection();
		assert_eq!(groups.len(), 4);
		assert_eq!(groups[0].root.node_id, "t1_root-a");
		assert_eq!(groups[0].descendants[0].node_id, "t1_child-a");
		assert_eq!(groups[0].descendants[0].parent_author, "root-user");
		assert_eq!(groups[0].descendants[0].wide_indent, 14);
		assert_eq!(groups[0].descendants[0].narrow_indent, 8);
		let continuation = &groups[0].descendants[1];
		assert_eq!(continuation.kind, "more");
		assert_eq!(continuation.parent_node_id, "t1_child-a");
		assert_eq!(continuation.depth, 2);
		assert_eq!(continuation.continuation_state, "pending");
		assert_eq!(continuation.continuation_children, "t1_deep-a,t1_deep-b");
	}

	#[test]
	fn search_preserves_ancestors_and_excludes_filtered_bodies() {
		let model = model();
		let search = model.search("child body");
		assert_eq!(search.match_ids, ["t1_child-a"]);
		assert_eq!(search.context_ids, ["t1_root-a"]);
		assert_eq!(search.match_count, 1);
		assert_eq!(search.searched_comment_count, 2);
		assert_eq!(search.excluded_filtered_comment_count, 2);
		assert!(!search.coverage_complete);

		let groups = model.into_search_projection(&search);
		assert_eq!(groups.len(), 4, "search must not replace the thread with flattened result roots");
		assert!(groups[0].root.search_context);
		assert!(groups[0].descendants[0].search_match);
		assert_eq!(groups[0].descendants[0].parent_node_id, "t1_root-a");
	}

	#[test]
	fn search_never_matches_author_or_keyword_filtered_content() {
		let model = model();
		assert!(model.search("filtered author body").match_ids.is_empty());
		assert!(model.search("spoiler phrase").match_ids.is_empty());
	}

	#[test]
	fn duplicate_comment_ids_are_ignored_without_rewriting_the_first_node() {
		let duplicate = json!({
			"data": {
				"children": [
					{"kind": "t1", "data": {"id": "same", "name": "t1_same", "parent_id": "t3_post", "author": "first", "body": "first", "body_html": "<p>first</p>", "replies": ""}},
					{"kind": "t1", "data": {"id": "same", "name": "t1_same", "parent_id": "t3_post", "author": "second", "body": "second", "body_html": "<p>second</p>", "replies": ""}}
				]
			}
		});
		let model = ThreadModel::from_listing(&duplicate, "post", 1, "/comments/post/", "author", "", &HashSet::new(), &[], &Preferences::default());
		assert_eq!(model.roots(), ["t1_same"]);
		assert_eq!(model.preorder_ids(), ["t1_same"]);
		assert!(model.validate().is_ok());
		assert!(model.summary().complete());
		let ThreadNode::Comment(comment) = model.node("t1_same").unwrap() else {
			panic!("same should be a comment");
		};
		assert_eq!(comment.author.name, "first");
	}

	#[test]
	fn single_thread_root_records_partial_ancestry_without_inventing_nodes() {
		let partial = json!({
			"data": {"children": [{
				"kind": "t1",
				"data": {"id": "focus", "name": "t1_focus", "parent_id": "t1_missing-parent", "depth": 4, "author": "reader", "body": "focus", "body_html": "<p>focus</p>", "replies": ""}
			}]}
		});
		let model = ThreadModel::from_listing(&partial, "post", 1, "/comments/post/", "author", "focus", &HashSet::new(), &[], &Preferences::default());
		let ThreadNode::Comment(comment) = model.node("t1_focus").unwrap() else {
			panic!("focus should be a comment");
		};
		assert_eq!(comment.parent_id, "t1_missing-parent");
		assert_eq!(comment.ancestor_path, ["t1_missing-parent"]);
		assert!(!comment.ancestor_path_complete);
		assert_eq!(comment.depth, 4);
		assert!(model.validate().is_ok());
		assert!(!model.summary().complete());
	}

	#[test]
	fn reported_count_gap_keeps_coverage_honestly_incomplete() {
		let model = ThreadModel::from_listing(
			&fixture(),
			"post",
			12,
			"/r/test/comments/post/thread/",
			"post-author",
			"",
			&HashSet::new(),
			&[],
			&Preferences::default(),
		);
		let summary = model.summary();
		assert_eq!(summary.comment_count, 4);
		assert_eq!(summary.estimated_remaining, 8);
		assert_eq!(summary.unaccounted_comment_count, 3);
		assert!(!summary.complete());
	}
}
