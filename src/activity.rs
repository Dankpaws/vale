//! Profile-owned activity baselines, separate from read/unread and hidden state.
use crate::{account, thread::ThreadGroup, utils::Post};
use hyper::{Body, Request};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;

const RETENTION: i64 = 180 * 24 * 60 * 60;
const VISIT_RETENTION: i64 = 24 * 60 * 60;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Visit {
	pub id: String,
	pub previous_at: Option<i64>,
	pub new_comments: u64,
}

impl Visit {
	pub fn url(&self, path: &str) -> String {
		if self.id.is_empty() {
			return path.to_string();
		}
		let Ok(mut parsed) = url::Url::parse(&format!("https://vale.invalid{path}")) else {
			return path.to_string();
		};
		let pairs = parsed
			.query_pairs()
			.filter(|(key, _)| key != "activity_visit")
			.map(|(key, value)| (key.into_owned(), value.into_owned()))
			.collect::<Vec<_>>();
		parsed.set_query(None);
		parsed.query_pairs_mut().extend_pairs(pairs).append_pair("activity_visit", &self.id);
		format!(
			"{}?{}{}",
			parsed.path(),
			parsed.query().unwrap_or_default(),
			parsed.fragment().map(|hash| format!("#{hash}")).unwrap_or_default()
		)
	}

	pub fn has_previous(&self) -> bool {
		self.previous_at.is_some()
	}

	pub fn previous_label(&self) -> String {
		self.previous_at.map(|at| crate::utils::time(at as f64).0).unwrap_or_default()
	}

	pub fn highlight(&self, groups: &mut [ThreadGroup]) {
		for group in groups {
			for comment in std::iter::once(&mut group.root).chain(group.descendants.iter_mut()) {
				comment.is_new = comment.kind == "t1" && self.previous_at.is_some_and(|at| comment.created_ts > at);
				comment.activity_visit = self.id.clone();
			}
		}
	}
}

pub(crate) fn initialize(connection: &Connection) -> rusqlite::Result<()> {
	connection.execute_batch(
		"CREATE TABLE IF NOT EXISTS post_activity (
			profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
			post_id TEXT NOT NULL,
			comment_count INTEGER NOT NULL CHECK(comment_count >= 0),
			viewed_at INTEGER NOT NULL,
			PRIMARY KEY (profile_id, post_id)
		);
		CREATE INDEX IF NOT EXISTS post_activity_recent ON post_activity(profile_id, viewed_at DESC);
		CREATE TABLE IF NOT EXISTS post_activity_visits (
			id TEXT PRIMARY KEY,
			profile_id INTEGER NOT NULL,
			post_id TEXT NOT NULL,
			started_at INTEGER NOT NULL,
			previous_at INTEGER,
			new_comments INTEGER NOT NULL CHECK(new_comments >= 0),
			FOREIGN KEY (profile_id, post_id) REFERENCES post_activity(profile_id, post_id) ON DELETE CASCADE
		);
		CREATE INDEX IF NOT EXISTS post_activity_visits_recent ON post_activity_visits(profile_id, started_at DESC);",
	)
}

fn delta(current: u64, previous: u64) -> u64 {
	current.saturating_sub(previous)
}

fn resume(connection: &Connection, profile: i64, post: &str, id: &str, now: i64) -> rusqlite::Result<Visit> {
	if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		return Ok(Visit::default());
	}
	connection
		.query_row(
			"SELECT previous_at, new_comments FROM post_activity_visits
		 WHERE id = ?1 AND profile_id = ?2 AND post_id = ?3 AND started_at >= ?4",
			params![id, profile, post, now - VISIT_RETENTION],
			|row| {
				Ok(Visit {
					id: id.to_string(),
					previous_at: row.get(0)?,
					new_comments: row.get(1)?,
				})
			},
		)
		.optional()
		.map(Option::unwrap_or_default)
}

fn begin(connection: &mut Connection, profile: i64, post: &str, count: u64, now: i64) -> rusqlite::Result<Visit> {
	let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let previous = transaction
		.query_row(
			"SELECT viewed_at, comment_count FROM post_activity WHERE profile_id = ?1 AND post_id = ?2 AND viewed_at >= ?3",
			params![profile, post, now - RETENTION],
			|row| Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?)),
		)
		.optional()?;
	let visit = Visit {
		id: uuid::Uuid::new_v4().simple().to_string(),
		previous_at: previous.map(|(at, _)| at),
		new_comments: previous.map_or(0, |(_, before)| delta(count, before)),
	};
	transaction.execute(
		"INSERT INTO post_activity (profile_id, post_id, comment_count, viewed_at) VALUES (?1, ?2, ?3, ?4)
		 ON CONFLICT(profile_id, post_id) DO UPDATE SET comment_count = excluded.comment_count, viewed_at = excluded.viewed_at",
		params![profile, post, count, now],
	)?;
	transaction.execute(
		"INSERT INTO post_activity_visits (id, profile_id, post_id, started_at, previous_at, new_comments) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
		params![visit.id, profile, post, now, visit.previous_at, visit.new_comments],
	)?;
	transaction.execute("DELETE FROM post_activity WHERE profile_id = ?1 AND viewed_at < ?2", params![profile, now - RETENTION])?;
	transaction.execute(
		"DELETE FROM post_activity WHERE profile_id = ?1 AND post_id IN (
		 SELECT post_id FROM post_activity WHERE profile_id = ?1 ORDER BY viewed_at DESC, rowid DESC LIMIT -1 OFFSET 5000)",
		[profile],
	)?;
	transaction.execute(
		"DELETE FROM post_activity_visits WHERE profile_id = ?1 AND started_at < ?2",
		params![profile, now - VISIT_RETENTION],
	)?;
	transaction.execute(
		"DELETE FROM post_activity_visits WHERE profile_id = ?1 AND id IN (
		 SELECT id FROM post_activity_visits WHERE profile_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT -1 OFFSET 512)",
		[profile],
	)?;
	transaction.commit()?;
	Ok(visit)
}

/// Internal visit IDs are presentation context, never authentication credentials.
/// A supplied/expired ID or a continuation can only resume, never advance state.
pub fn for_post(request: &Request<Body>, post: &Post, continuation: bool) -> Result<Visit, String> {
	let Some(context) = account::context(request) else { return Ok(Visit::default()) };
	if !account::valid_post_id(&post.id)
		|| ["purpose", "sec-purpose"].iter().any(|name| {
			request
				.headers()
				.get(*name)
				.and_then(|value| value.to_str().ok())
				.is_some_and(|value| value.contains("prefetch"))
		}) {
		return Ok(Visit::default());
	}
	let id = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes()).find_map(|(key, value)| (key == "activity_visit").then(|| value.into_owned()));
	let mut connection = account::open_database()?;
	let now = account::now();
	let reload = request
		.headers()
		.get(hyper::header::CACHE_CONTROL)
		.and_then(|value| value.to_str().ok())
		.is_some_and(|value| value.split(',').any(|directive| matches!(directive.trim(), "max-age=0" | "no-cache")));
	if continuation || (id.is_some() && !reload) {
		return resume(&connection, context.profile_id, &post.id, id.as_deref().unwrap_or_default(), now)
			.map_err(|error| format!("Unable to load Vale comment activity: {error}"));
	}
	let Some(count) = post.comment_count.filter(|count| *count <= i64::MAX as u64) else {
		return Ok(Visit::default());
	};
	begin(&mut connection, context.profile_id, &post.id, count, now).map_err(|error| format!("Unable to record Vale comment activity: {error}"))
}

fn baselines(connection: &Connection, profile: i64, ids: &[&str], now: i64) -> rusqlite::Result<HashMap<String, u64>> {
	let mut result = HashMap::new();
	// Respect SQLite's parameter bound even for older, unbounded overview routes.
	for chunk in ids.chunks(200) {
		let placeholders = vec!["?"; chunk.len()].join(",");
		let sql = format!("SELECT post_id, comment_count FROM post_activity WHERE profile_id = ? AND viewed_at >= ? AND post_id IN ({placeholders})");
		let mut parameters = vec![rusqlite::types::Value::Integer(profile), rusqlite::types::Value::Integer(now - RETENTION)];
		parameters.extend(chunk.iter().map(|id| rusqlite::types::Value::Text((*id).to_string())));
		let mut statement = connection.prepare(&sql)?;
		let rows = statement.query_map(params_from_iter(parameters), |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)))?;
		for row in rows {
			let (id, count) = row?;
			result.insert(id, count);
		}
	}
	Ok(result)
}

pub fn annotate(request: &Request<Body>, posts: &mut [Post]) -> Result<(), String> {
	let Some(context) = account::context(request) else { return Ok(()) };
	let ids = posts
		.iter()
		.flat_map(|post| std::iter::once(post.id.as_str()).chain(post.grouped_posts.iter().map(|post| post.id.as_str())))
		.collect::<Vec<_>>();
	if ids.is_empty() {
		return Ok(());
	}
	let counts = baselines(&account::open_database()?, context.profile_id, &ids, account::now()).map_err(|error| format!("Unable to load Vale comment activity: {error}"))?;
	for post in posts {
		post.new_comments = post.comment_count.zip(counts.get(&post.id)).map_or(0, |(current, previous)| delta(current, *previous));
		for grouped in &mut post.grouped_posts {
			grouped.new_comments = grouped
				.comment_count
				.zip(counts.get(&grouped.id))
				.map_or(0, |(current, previous)| delta(current, *previous));
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn database() -> Connection {
		let connection = Connection::open_in_memory().unwrap();
		connection
			.execute_batch("PRAGMA foreign_keys = ON; CREATE TABLE profiles(id INTEGER PRIMARY KEY); INSERT INTO profiles VALUES (1), (2);")
			.unwrap();
		initialize(&connection).unwrap();
		initialize(&connection).unwrap();
		connection
	}

	#[test]
	fn first_visit_growth_deletion_and_rebased_growth() {
		let mut db = database();
		let first = begin(&mut db, 1, "post", 100, 1000).unwrap();
		assert!(!first.has_previous());
		assert_eq!(first.new_comments, 0);
		let growth = begin(&mut db, 1, "post", 116, 1100).unwrap();
		assert_eq!(growth.new_comments, 16);
		let deletion = begin(&mut db, 1, "post", 66, 1200).unwrap();
		assert_eq!(deletion.new_comments, 0);
		assert_eq!(begin(&mut db, 1, "post", 70, 1300).unwrap().new_comments, 4);
		assert_eq!(delta(0, u64::MAX), 0);
		assert_eq!(resume(&db, 1, "post", &growth.id, 1400).unwrap(), growth, "other tabs do not replace this visit's snapshot");
		assert_eq!(baselines(&db, 1, &["post"], 1400).unwrap()["post"], 70, "resuming cannot rewind the account baseline");
	}

	#[test]
	fn visits_are_owned_bounded_and_cleared_with_baselines() {
		let mut db = database();
		let visit = begin(&mut db, 1, "one", 100, 1000).unwrap();
		assert_eq!(resume(&db, 2, "one", &visit.id, 1100).unwrap(), Visit::default());
		assert_eq!(resume(&db, 1, "two", &visit.id, 1100).unwrap(), Visit::default());
		assert_eq!(resume(&db, 1, "one", "invalid", 1100).unwrap(), Visit::default());
		assert_eq!(resume(&db, 1, "one", &visit.id, 1001 + VISIT_RETENTION).unwrap(), Visit::default());
		assert!(!begin(&mut db, 2, "one", 150, 1100).unwrap().has_previous());
		for now in 2000..2513 {
			begin(&mut db, 1, "one", 100, now).unwrap();
		}
		assert_eq!(
			db.query_row("SELECT count(*) FROM post_activity_visits WHERE profile_id = 1", [], |row| row.get::<_, i64>(0))
				.unwrap(),
			512
		);
		db.execute("DELETE FROM post_activity WHERE profile_id = 1", []).unwrap();
		assert_eq!(
			db.query_row("SELECT count(*) FROM post_activity_visits WHERE profile_id = 1", [], |row| row.get::<_, i64>(0))
				.unwrap(),
			0
		);
		assert_eq!(baselines(&db, 2, &["one"], 3000).unwrap()["one"], 150);
	}

	#[test]
	fn expired_baselines_are_unknown_and_listing_reads_never_advance_them() {
		let mut db = database();
		begin(&mut db, 1, "one", 100, 1000).unwrap();
		assert!(baselines(&db, 2, &["one"], 1100).unwrap().is_empty());
		assert!(baselines(&db, 1, &["one"], 1001 + RETENTION).unwrap().is_empty());
		assert_eq!(baselines(&db, 1, &["one"], 1100).unwrap()["one"], 100);
		assert_eq!(baselines(&db, 1, &["one"], 1200).unwrap()["one"], 100);
		assert!(!begin(&mut db, 1, "one", 120, 1001 + RETENTION).unwrap().has_previous());
	}

	#[test]
	fn highlight_is_creation_based_even_with_zero_net_growth_and_partial_replies() {
		let listing = serde_json::json!({"data":{"children":[
			{"kind":"t1","data":{"id":"old","parent_id":"t3_post","created_utc":99,"author":"reader","body":"Old","body_html":"<p>Old</p>","replies":""}},
			{"kind":"t1","data":{"id":"equal","parent_id":"t3_post","created_utc":100,"author":"reader","body":"Same second","body_html":"<p>Same second</p>","replies":""}},
			{"kind":"t1","data":{"id":"new","parent_id":"t3_post","created_utc":101,"author":"reader","body":"New","body_html":"<p>New</p>","replies":{"data":{"children":[
				{"kind":"t1","data":{"id":"child","parent_id":"t1_new","created_utc":102,"author":"reader","body":"Reply","body_html":"<p>Reply</p>","replies":""}},
				{"kind":"more","data":{"parent_id":"t1_new","count":2,"children":["later"]}}
			]}}}}
		]}});
		let mut groups =
			crate::thread::ThreadModel::from_listing(&listing, "post", 6, "/comments/post/", "poster", "", &Default::default(), &[], &Default::default()).into_projection();
		Visit::default().highlight(&mut groups);
		assert!(groups.iter().all(|group| !group.root.is_new));
		let visit = Visit {
			id: "0123456789abcdef0123456789abcdef".to_string(),
			previous_at: Some(100),
			new_comments: 0,
		};
		visit.highlight(&mut groups);
		assert!(!groups[0].root.is_new);
		assert!(!groups[1].root.is_new);
		assert!(groups[2].root.is_new);
		assert!(groups[2].descendants[0].is_new);
		assert!(!groups[2].descendants[1].is_new);
		assert_eq!(groups[2].descendants[0].activity_visit, visit.id);
		let html = askama::Template::render(&groups[2].root).unwrap();
		assert!(html.contains("is-comment-new"));
		assert!(html.contains("New<span class=\"sr-only\"> since last visit</span>"));
		assert_eq!(
			visit.url("/comments/post/?sort=new&activity_visit=old#new"),
			format!("/comments/post/?sort=new&activity_visit={}#new", visit.id)
		);
	}
}
