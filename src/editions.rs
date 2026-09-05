//! Finite, immutable feed windows with explicit, revisioned progress.
use crate::{
	account,
	reading::WriteError,
	utils::{template, Post, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Item {
	pub id: String,
	pub title: String,
	pub community: String,
	pub author: String,
	pub excerpt: String,
	pub key: String,
	pub created: i64,
}
impl Item {
	pub(crate) fn from_post(p: &Post) -> Self {
		let plain = ammonia::Builder::new().tags(HashSet::new()).clean(&p.body).to_string();
		Self {
			id: p.id.clone(),
			title: p.title.clone(),
			community: p.community.clone(),
			author: p.author.name.clone(),
			excerpt: htmlescape::decode_html(&plain).unwrap_or(plain).chars().take(800).collect(),
			key: p.content_key.clone(),
			created: p.created_ts.min(i64::MAX as u64) as i64,
		}
	}
}
#[derive(Clone, Debug)]
pub struct Edition {
	pub id: i64,
	pub feed: String,
	pub name: String,
	pub created: i64,
	pub minutes: i64,
	pub items: Vec<Item>,
	pub position: i64,
	pub complete: bool,
	pub revision: i64,
	pub coverage: String,
}
impl Edition {
	pub fn date(&self) -> String {
		crate::utils::time(self.created as f64).0
	}
}
pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch("CREATE TABLE IF NOT EXISTS reading_editions(
 id INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
 feed TEXT NOT NULL,name TEXT NOT NULL,created INTEGER NOT NULL,minutes INTEGER NOT NULL,items TEXT NOT NULL,
 position INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,revision INTEGER NOT NULL DEFAULT 1,coverage TEXT NOT NULL);
 CREATE INDEX IF NOT EXISTS edition_owner ON reading_editions(profile_id,feed,created);
 CREATE TABLE IF NOT EXISTS edition_builds(profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,feed TEXT NOT NULL,claimed INTEGER NOT NULL,PRIMARY KEY(profile_id,feed));")
}
/// Round-robin sampling starts with the quietest community. Time bounds are
/// fixed at request start; duplicate content consumes one slot, not two.
pub fn select(mut buckets: Vec<Vec<Item>>, cutoff: i64, minutes: i64) -> Result<Vec<Item>, WriteError> {
	if cutoff < 0 || !matches!(minutes, 5 | 10 | 20) || buckets.len() > 32 {
		return Err(WriteError::Invalid);
	}
	for b in &mut buckets {
		b.retain(|p| p.created <= cutoff && p.created >= cutoff.saturating_sub(86400) && account::valid_post_id(&p.id));
		b.sort_by_key(|p| std::cmp::Reverse(p.created));
	}
	buckets.sort_by_key(Vec::len);
	let mut queues: Vec<VecDeque<Item>> = buckets.into_iter().map(VecDeque::from).collect();
	let mut ids = HashSet::new();
	let mut keys = HashSet::new();
	let mut items = Vec::new();
	let target = (minutes * 2).min(25) as usize;
	loop {
		let mut progressed = false;
		for q in &mut queues {
			while let Some(p) = q.pop_front() {
				progressed = true;
				if !ids.insert(p.id.clone()) {
					continue;
				}
				if !p.key.is_empty() && !keys.insert(p.key.clone()) {
					continue;
				}
				items.push(p);
				break;
			}
			if items.len() >= target {
				return Ok(items);
			}
		}
		if !progressed {
			return Ok(items);
		}
	}
}
// Keep the explicit transaction fields together at this persistence boundary.
#[allow(clippy::too_many_arguments)]
pub fn store(db: &mut Connection, profile: i64, feed: &str, name: &str, minutes: i64, items: &[Item], coverage: &str, now: i64) -> Result<i64, WriteError> {
	if feed.is_empty()
		|| feed.len() > 80
		|| name.trim().is_empty()
		|| name.len() > 200
		|| now < 0
		|| !matches!(minutes, 5 | 10 | 20)
		|| items.len() > 25
		|| coverage.len() > 4000
		|| items
			.iter()
			.any(|i| !account::valid_post_id(&i.id) || i.title.len() > 4000 || i.excerpt.len() > 4000 || i.community.len() > 80 || i.author.len() > 128 || i.key.len() > 8192)
	{
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if tx.query_row("SELECT count(*) FROM reading_editions WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 100 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT INTO reading_editions(profile_id,feed,name,created,minutes,items,coverage) VALUES(?1,?2,?3,?4,?5,?6,?7)",
		params![profile, feed, name, now, minutes, serde_json::to_string(items).map_err(|_| WriteError::Invalid)?, coverage],
	)?;
	let id = tx.last_insert_rowid();
	crate::agenda::observe(&tx, profile, feed, items, now)?;
	tx.commit()?;
	Ok(id)
}
fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Edition> {
	let json: String = r.get(5)?;
	let items = serde_json::from_str(&json).map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e)))?;
	Ok(Edition {
		id: r.get(0)?,
		feed: r.get(1)?,
		name: r.get(2)?,
		created: r.get(3)?,
		minutes: r.get(4)?,
		items,
		position: r.get(6)?,
		complete: r.get(7)?,
		revision: r.get(8)?,
		coverage: r.get(9)?,
	})
}
const COLUMNS: &str = "id,feed,name,created,minutes,items,position,complete,revision,coverage";
pub fn get(db: &Connection, profile: i64, id: i64) -> Result<Option<Edition>, WriteError> {
	Ok(
		db.query_row(
			&format!("SELECT {COLUMNS} FROM reading_editions WHERE profile_id=?1 AND id=?2"),
			params![profile, id],
			read_row,
		)
		.optional()?,
	)
}
pub fn progress(db: &mut Connection, profile: i64, id: i64, revision: i64, position: i64, action: &str) -> Result<(), WriteError> {
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let e = get(&tx, profile, id)?.ok_or(WriteError::Invalid)?;
	if revision != e.revision {
		return Err(WriteError::Conflict);
	}
	if position < 0 || position as usize > e.items.len() {
		return Err(WriteError::Invalid);
	}
	match action {
		"place" | "complete" | "reopen" => {
			tx.execute(
				"UPDATE reading_editions SET position=?3,complete=?4,revision=revision+1 WHERE profile_id=?1 AND id=?2",
				params![profile, id, position, action == "complete"],
			)?;
		}
		"forget" => {
			tx.execute("DELETE FROM reading_editions WHERE profile_id=?1 AND id=?2", params![profile, id])?;
		}
		_ => return Err(WriteError::Invalid),
	}
	tx.commit()?;
	Ok(())
}
fn claim(db: &mut Connection, profile: i64, feed: &str, now: i64) -> Result<(), WriteError> {
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let previous: Option<i64> = tx
		.query_row("SELECT claimed FROM edition_builds WHERE profile_id=?1 AND feed=?2", params![profile, feed], |r| r.get(0))
		.optional()?;
	if now < 0 || previous.is_some_and(|p| now.saturating_sub(p) < 900) {
		return Err(WriteError::Conflict);
	}
	tx.execute(
		"INSERT INTO edition_builds(profile_id,feed,claimed) VALUES(?1,?2,?3) ON CONFLICT(profile_id,feed) DO UPDATE SET claimed=excluded.claimed",
		params![profile, feed, now],
	)?;
	tx.commit()?;
	Ok(())
}
#[derive(Template)]
#[template(path = "editions.html")]
struct Page {
	prefs: Preferences,
	url: String,
	editions: Vec<Edition>,
	selected: Option<Edition>,
}
pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to read editions."));
	};
	let db = account::open_database()?;
	let id = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).find_map(|(k, v)| (k == "id").then(|| v.parse::<i64>().ok()).flatten());
	let selected = match id {
		Some(id) => {
			let e = get(&db, profile, id).map_err(|e| format!("{e:?}"))?;
			if e.is_none() {
				return Ok(reply(StatusCode::NOT_FOUND, "Edition not found."));
			}
			e
		}
		None => None,
	};
	let mut stmt = db
		.prepare(&format!("SELECT {COLUMNS} FROM reading_editions WHERE profile_id=?1 ORDER BY id DESC LIMIT 100"))
		.map_err(|e| e.to_string())?;
	let editions = stmt
		.query_map([profile], read_row)
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	Ok(template(&Page {
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		editions,
		selected,
	}))
}
pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to save editions."));
	};
	let bytes = crate::utils::read_body_limited(req.body_mut(), 2 * 1024 * 1024, "Edition command is too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	let id = if v("action") == "window" {
		let prefs = Preferences::new(&req);
		let Some(feed) = prefs.feed_groups().into_iter().find(|f| f.slug == v("feed")) else {
			return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Choose a named feed."));
		};
		let items = match serde_json::from_str::<Vec<Item>>(v("items")) {
			Ok(items) => items,
			Err(_) => return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Invalid reading window.")),
		};
		match save_window(&mut account::open_database()?, profile, &feed, &items, account::now()) {
			Ok(id) => id,
			Err(_) => {
				return Ok(reply(
					StatusCode::UNPROCESSABLE_ENTITY,
					"Unable to save this window. It must contain at most 25 posts from this feed, within your edition capacity.",
				))
			}
		}
	} else if v("action") == "build" {
		let prefs = Preferences::new(&req);
		let Some(feed) = prefs.feed_groups().into_iter().find(|f| f.slug == v("feed")) else {
			return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Choose a named feed."));
		};
		let minutes = v("minutes").parse::<i64>().unwrap_or(0);
		if !matches!(minutes, 5 | 10 | 20) || feed.communities.len() > 32 {
			return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Choose a 5, 10 or 20 minute edition with at most 32 communities."));
		}
		let now = account::now();
		if claim(&mut account::open_database()?, profile, &feed.slug, now).is_err() {
			return Ok(reply(StatusCode::CONFLICT, "An edition was requested recently. Try again after 15 minutes."));
		}
		let policy = crate::listing::ListingPolicy::for_request(&req, Some(feed.communities.clone()), false, false)?;
		let mut buckets = Vec::new();
		let mut failures = Vec::new();
		// Four requests in flight, one batch of 25 per selected community, no crawling.
		for chunk in feed.communities.chunks(4) {
			let mut jobs = Vec::new();
			for community in chunk {
				let community = community.clone();
				let policy = policy.clone();
				jobs.push(tokio::spawn(async move {
					let path = format!("/r/{community}/new.json?raw_json=1&limit=25");
					let result = tokio::time::timeout(std::time::Duration::from_secs(30), crate::listing::edition_batch(&path, policy)).await;
					(community, result)
				}));
			}
			for job in jobs {
				match job.await {
					Ok((_, Ok(Ok(posts)))) => buckets.push(posts.into_iter().map(|p| Item::from_post(&p)).collect()),
					Ok((community, _)) => failures.push(community),
					Err(_) => failures.push("A selected community".into()),
				}
			}
		}
		let items = select(buckets, now, minutes).map_err(|e| format!("{e:?}"))?;
		let coverage = format!(
			"Sampled up to 25 recent posts per community from the preceding 24 hours. Membership is fixed. This is a reading selection, not every post. {}",
			if failures.is_empty() {
				"All selected communities responded.".into()
			} else {
				format!("Unavailable: {}.", failures.join(", "))
			}
		);
		match store(&mut account::open_database()?, profile, &feed.slug, &feed.name, minutes, &items, &coverage, now) {
			Ok(id) => id,
			Err(WriteError::Full) => return Ok(reply(StatusCode::CONFLICT, "Edition capacity reached. Forget an old edition first.")),
			Err(e) => return Err(format!("{e:?}")),
		}
	} else {
		let id = v("id").parse().unwrap_or(0);
		let revision = v("revision").parse().unwrap_or(-1);
		let position = v("position").parse().unwrap_or(-1);
		if let Err(e) = progress(&mut account::open_database()?, profile, id, revision, position, v("action")) {
			return Ok(reply(
				if e == WriteError::Conflict {
					StatusCode::CONFLICT
				} else {
					StatusCode::UNPROCESSABLE_ENTITY
				},
				"Edition changed in another tab or the command is invalid. Reload to review it.",
			));
		}
		if v("action") == "forget" {
			0
		} else {
			id
		}
	};
	let location = if id == 0 { "/reading/editions".into() } else { format!("/reading/editions?id={id}") };
	Ok(
		Response::builder()
			.status(StatusCode::SEE_OTHER)
			.header("location", location)
			.header("cache-control", "private, no-store")
			.body(Body::empty())
			.unwrap(),
	)
}
fn reply(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("cache-control", "private, no-store")
		.header("content-type", "text/plain; charset=utf-8")
		.body(Body::from(message.to_string()))
		.unwrap()
}

#[cfg(test)]
mod tests {
	use super::*;
	fn db() -> Connection {
		let db = Connection::open_in_memory().unwrap();
		db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE profiles(id INTEGER PRIMARY KEY);INSERT INTO profiles VALUES(1),(2);")
			.unwrap();
		initialize(&db).unwrap();
		crate::agenda::initialize(&db).unwrap();
		db
	}
	fn item(id: usize) -> Item {
		Item {
			id: format!("post{id}"),
			title: format!("Title {id}"),
			community: "rust".into(),
			author: "reader".into(),
			excerpt: "Evidence".into(),
			key: String::new(),
			created: 100000,
		}
	}
	#[test]
	fn selection_twelve_edges() {
		for case in 0..12 {
			let mut buckets = vec![vec![item(1)], (2..42).map(item).collect()];
			let mut cutoff = 100000;
			let mut minutes = 5;
			match case {
				0 => buckets.clear(),
				1 => buckets = vec![vec![], vec![]],
				2 => buckets[0][0].created = 100001,
				3 => buckets[0][0].created = 13599,
				4 => buckets[0][0].created = 13600,
				5 => buckets[1][0].id = "post1".into(),
				6 => {
					buckets[0][0].key = "same".into();
					buckets[1][0].key = "same".into()
				}
				7 => minutes = 20,
				8 => minutes = 6,
				9 => cutoff = -1,
				10 => buckets = vec![vec![]; 33],
				11 => buckets[0][0].id = "../bad".into(),
				_ => {}
			}
			let result = select(buckets, cutoff, minutes);
			if matches!(case, 8..=10) {
				assert!(result.is_err(), "case {case}");
				continue;
			}
			let items = result.unwrap();
			assert!(items.len() <= 25, "case {case}");
			let ids: HashSet<_> = items.iter().map(|i| &i.id).collect();
			assert_eq!(ids.len(), items.len(), "case {case}");
			match case {
				0 | 1 => assert!(items.is_empty()),
				2 | 3 | 11 => assert!(!items.iter().any(|i| i.id == "post1")),
				4 => assert_eq!(items[0].id, "post1"),
				5 => assert_eq!(items.iter().filter(|i| i.id == "post1").count(), 1),
				6 => assert_eq!(items.iter().filter(|i| i.key == "same").count(), 1),
				7 => assert_eq!(items.len(), 25),
				_ => {}
			}
		}
	}
	#[test]
	fn persistence_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut feed = "rust";
			let mut name = "Rust";
			let mut minutes = 5;
			let mut items = vec![item(1)];
			let mut now = 100000;
			let mut profile = 1;
			match case {
				0 => items.clear(),
				1 => feed = "",
				2 => name = " ",
				3 => minutes = 0,
				4 => items = vec![item(1); 26],
				5 => now = -1,
				6 => items[0].id = "/bad".into(),
				7 => items[0].excerpt = "x".repeat(4001),
				8 => profile = 99,
				9 => {
					for _ in 0..100 {
						store(&mut db, 1, "rust", "Rust", 5, &[], "", now).unwrap();
					}
				}
				10 => initialize(&db).unwrap(),
				11 => items[0].title = "<script>literal</script>".into(),
				_ => {}
			}
			let result = store(&mut db, profile, feed, name, minutes, &items, "Captured", now);
			if matches!(case, 1..=9) {
				assert!(result.is_err(), "case {case}")
			} else {
				let id = result.unwrap();
				assert_eq!(get(&db, 1, id).unwrap().unwrap().items, items);
				assert!(get(&db, 2, id).unwrap().is_none());
			}
		}
	}
	fn progress_edges(action: &str) {
		for case in 0..12 {
			let mut db = db();
			let id = store(&mut db, 1, "rust", "Rust", 5, &[item(1), item(2)], "", 100000).unwrap();
			let mut profile = 1;
			let mut target = id;
			let mut rev = 1;
			let mut pos = 1;
			let mut act = action;
			match case {
				0 => pos = 0,
				1 => pos = 2,
				2 => pos = -1,
				3 => pos = 3,
				4 => rev = 0,
				5 => rev = -1,
				6 => profile = 2,
				7 => target = 999,
				8 => act = "bogus",
				9 => {
					progress(&mut db, 1, id, 1, 0, "place").unwrap();
				}
				10 => initialize(&db).unwrap(),
				11 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				_ => {}
			}
			let result = progress(&mut db, profile, target, rev, pos, act);
			if matches!(case, 2..=9 | 11) {
				assert!(result.is_err(), "{action} case {case}")
			} else {
				result.unwrap();
				let current = get(&db, 1, id).unwrap();
				if action == "forget" {
					assert!(current.is_none())
				} else {
					let e = current.unwrap();
					assert_eq!(e.revision, 2);
					assert_eq!(e.position, pos);
					assert_eq!(e.complete, action == "complete");
					assert_eq!(e.items, vec![item(1), item(2)]);
				}
			}
		}
	}
	#[test]
	fn checkpoint_twelve_edges() {
		progress_edges("place")
	}
	#[test]
	fn complete_twelve_edges() {
		progress_edges("complete")
	}
	#[test]
	fn reopen_twelve_edges() {
		progress_edges("reopen")
	}
	#[test]
	fn forget_twelve_edges() {
		progress_edges("forget")
	}
	#[test]
	fn build_claim_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			claim(&mut db, 1, "rust", 1000).unwrap();
			let (profile, feed, now, ok) = match case {
				0 => (1, "rust", 1000, false),
				1 => (1, "rust", 1899, false),
				2 => (1, "rust", 1900, true),
				3 => (1, "rust", 1901, true),
				4 => (2, "rust", 1000, true),
				5 => (1, "other", 1000, true),
				6 => (1, "rust", -1, false),
				7 => (1, "rust", 0, false),
				8 => (99, "rust", 1000, false),
				9 => (1, "rust", i64::MAX, true),
				10 => {
					initialize(&db).unwrap();
					(1, "rust", 1000, false)
				}
				_ => (1, "rust", 999, false),
			};
			assert_eq!(claim(&mut db, profile, feed, now).is_ok(), ok, "case {case}");
		}
	}
}

/// Inert snapshots of rendered cards, also carried by enhanced listing fragments.
pub fn item_json(post: &Post) -> String {
	serde_json::to_string(&Item::from_post(post)).unwrap_or_default()
}
pub fn window_json(posts: &[Post]) -> String {
	serde_json::to_string(&posts.iter().take(25).map(Item::from_post).collect::<Vec<_>>()).unwrap_or_default()
}
pub fn save_window(db: &mut Connection, profile: i64, feed: &crate::utils::FeedGroup, items: &[Item], now: i64) -> Result<i64, WriteError> {
	let mut ids = HashSet::new();
	if items.len() > 25
		|| items
			.iter()
			.any(|i| !feed.communities.iter().any(|c| c.eq_ignore_ascii_case(&i.community)) || !ids.insert(&i.id))
	{
		return Err(WriteError::Invalid);
	}
	store(
		db,
		profile,
		&feed.slug,
		&feed.name,
		5,
		items,
		"A fixed copy of the posts you chose to keep from your feed page. Membership and order do not change when the live feed updates.",
		now,
	)
}
#[cfg(test)]
mod window_tests {
	use super::*;
	#[test]
	fn window_twelve_edges() {
		for case in 0..12 {
			let mut db = Connection::open_in_memory().unwrap();
			db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE profiles(id INTEGER PRIMARY KEY);INSERT INTO profiles VALUES(1),(2);")
				.unwrap();
			initialize(&db).unwrap();
			crate::agenda::initialize(&db).unwrap();
			let feed = crate::utils::FeedGroup {
				name: "Rust".into(),
				slug: "rust".into(),
				communities: vec!["rust".into()],
			};
			let mut items = vec![Item {
				id: "post1".into(),
				title: "Title".into(),
				community: "rust".into(),
				author: "reader".into(),
				excerpt: "Source".into(),
				key: String::new(),
				created: 1000,
			}];
			let mut profile = 1;
			let mut now = 1000;
			match case {
				0 => items.clear(),
				1 => items[0].community = "other".into(),
				2 => items.push(items[0].clone()),
				3 => items[0].community = "RUST".into(),
				4 => items[0].id = "../bad".into(),
				5 => items[0].excerpt = "x".repeat(4001),
				6 => profile = 99,
				7 => now = -1,
				8 => {
					initialize(&db).unwrap();
				}
				9 => items[0].title = "日本語".into(),
				10 => {
					items = (0..26)
						.map(|i| {
							let mut p = items[0].clone();
							p.id = format!("post{i}");
							p
						})
						.collect()
				}
				11 => items[0].key = "<literal>".into(),
				_ => {}
			}
			let result = save_window(&mut db, profile, &feed, &items, now);
			assert_eq!(result.is_ok(), matches!(case, 0 | 3 | 8 | 9 | 11), "case {case}");
			if let Ok(id) = result {
				assert_eq!(get(&db, 1, id).unwrap().unwrap().items, items);
				assert!(get(&db, 2, id).unwrap().is_none());
			}
		}
	}
}
