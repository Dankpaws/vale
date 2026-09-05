//! Manually curated story lifecycles. Every event retains selected evidence;
//! comparisons describe the selection, never inferred community consensus.
use crate::{
	account,
	reading::WriteError,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Story {
	pub id: i64,
	pub feed: String,
	pub title: String,
	pub stage: String,
	pub note: String,
	pub revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
	pub id: i64,
	pub date: i64,
	pub title: String,
	pub url: String,
	pub community: String,
	pub body: String,
	pub note: String,
	pub provenance: String,
}
impl Event {
	pub fn excerpt(&self) -> String {
		let mut text = self.body.chars().take(500).collect::<String>();
		if self.body.chars().count() > 500 {
			text.push('…')
		}
		text
	}
	pub fn date_label(&self) -> String {
		chrono::DateTime::from_timestamp(self.date, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
	}
}
#[derive(Clone, Debug)]
struct Related {
	id: i64,
	title: String,
	note: String,
}
#[derive(Clone, Debug)]
struct Comparison {
	community: String,
	count: usize,
}
pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch("CREATE TABLE IF NOT EXISTS reading_stories(id INTEGER PRIMARY KEY AUTOINCREMENT,profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,feed TEXT NOT NULL,title TEXT NOT NULL,stage TEXT NOT NULL,note TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 1);CREATE TABLE IF NOT EXISTS reading_story_events(id INTEGER PRIMARY KEY AUTOINCREMENT,story INTEGER NOT NULL REFERENCES reading_stories(id) ON DELETE CASCADE,date INTEGER NOT NULL,title TEXT NOT NULL,url TEXT NOT NULL,community TEXT NOT NULL,body TEXT NOT NULL,note TEXT NOT NULL,provenance TEXT NOT NULL);CREATE TABLE IF NOT EXISTS reading_story_links(left_id INTEGER NOT NULL REFERENCES reading_stories(id) ON DELETE CASCADE,right_id INTEGER NOT NULL REFERENCES reading_stories(id) ON DELETE CASCADE,note TEXT NOT NULL,PRIMARY KEY(left_id,right_id),CHECK(left_id<right_id));")
}
fn valid_stage(stage: &str) -> bool {
	matches!(stage, "watching" | "announced" | "released" | "follow-up" | "resolved")
}
pub fn create(db: &mut Connection, profile: i64, feed: &str, title: &str) -> Result<i64, WriteError> {
	if feed.is_empty() || feed.len() > 80 || title.trim().is_empty() || title.len() > 500 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if tx.query_row("SELECT count(*) FROM reading_stories WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 200 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT INTO reading_stories(profile_id,feed,title,stage,note) VALUES(?1,?2,?3,'watching','')",
		params![profile, feed, title.trim()],
	)?;
	let id = tx.last_insert_rowid();
	tx.commit()?;
	Ok(id)
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Story> {
	Ok(Story {
		id: r.get(0)?,
		feed: r.get(1)?,
		title: r.get(2)?,
		stage: r.get(3)?,
		note: r.get(4)?,
		revision: r.get(5)?,
	})
}
pub fn get(db: &Connection, profile: i64, id: i64) -> Result<Option<Story>, WriteError> {
	Ok(
		db.query_row(
			"SELECT id,feed,title,stage,note,revision FROM reading_stories WHERE profile_id=?1 AND id=?2",
			params![profile, id],
			row,
		)
		.optional()?,
	)
}
pub fn events(db: &Connection, profile: i64, id: i64) -> Result<Vec<Event>, WriteError> {
	if get(db, profile, id)?.is_none() {
		return Err(WriteError::Invalid);
	}
	let mut stmt = db.prepare("SELECT id,date,title,url,community,body,note,provenance FROM reading_story_events WHERE story=?1 ORDER BY date,id")?;
	let rows = stmt.query_map([id], |r| {
		Ok(Event {
			id: r.get(0)?,
			date: r.get(1)?,
			title: r.get(2)?,
			url: r.get(3)?,
			community: r.get(4)?,
			body: r.get(5)?,
			note: r.get(6)?,
			provenance: r.get(7)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
// Keep the explicit transaction fields together at this persistence boundary.
#[allow(clippy::too_many_arguments)]
pub fn edit(db: &mut Connection, profile: i64, id: i64, revision: i64, title: &str, stage: &str, note: &str, remove: bool) -> Result<(), WriteError> {
	if title.trim().is_empty() || title.len() > 500 || !valid_stage(stage) || note.chars().count() > 8192 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let story = get(&tx, profile, id)?.ok_or(WriteError::Invalid)?;
	if story.revision != revision {
		return Err(WriteError::Conflict);
	}
	if remove {
		tx.execute("DELETE FROM reading_stories WHERE id=?1", [id])?;
	} else {
		tx.execute(
			"UPDATE reading_stories SET title=?2,stage=?3,note=?4,revision=revision+1 WHERE id=?1",
			params![id, title.trim(), stage, note],
		)?;
	}
	tx.commit()?;
	Ok(())
}
fn source_url(url: &str) -> bool {
	if url.starts_with("/comments/") {
		return !url.contains(['<', '>', '\n', '\r', '\\']) && url.len() <= 2048;
	}
	url::Url::parse(url)
		.ok()
		.is_some_and(|u| matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none() && u.host_str().is_some() && url.len() <= 2048)
}
pub fn add_event(db: &mut Connection, profile: i64, id: i64, revision: i64, event: &Event) -> Result<(), WriteError> {
	if event.date < 0
		|| event.title.trim().is_empty()
		|| event.title.len() > 4000
		|| !source_url(&event.url)
		|| event.community.len() > 80
		|| event.body.len() > 65536
		|| event.note.chars().count() > 8192
		|| event.provenance.len() > 2048
	{
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let story = get(&tx, profile, id)?.ok_or(WriteError::Invalid)?;
	if story.revision != revision {
		return Err(WriteError::Conflict);
	}
	if tx.query_row("SELECT count(*) FROM reading_story_events WHERE story=?1", [id], |r| r.get::<_, i64>(0))? >= 200 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT INTO reading_story_events(story,date,title,url,community,body,note,provenance) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
		params![id, event.date, event.title, event.url, event.community, event.body, event.note, event.provenance],
	)?;
	tx.execute("UPDATE reading_stories SET revision=revision+1 WHERE id=?1", [id])?;
	tx.commit()?;
	Ok(())
}
pub fn remove_event(db: &mut Connection, profile: i64, id: i64, revision: i64, event: i64) -> Result<(), WriteError> {
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let story = get(&tx, profile, id)?.ok_or(WriteError::Invalid)?;
	if story.revision != revision {
		return Err(WriteError::Conflict);
	}
	let changed = tx.execute("DELETE FROM reading_story_events WHERE story=?1 AND id=?2", params![id, event])?;
	if changed == 0 {
		return Err(WriteError::Invalid);
	}
	tx.execute("UPDATE reading_stories SET revision=revision+1 WHERE id=?1", [id])?;
	tx.commit()?;
	Ok(())
}
pub fn relate(db: &mut Connection, profile: i64, id: i64, revision: i64, other: i64, note: &str, remove: bool) -> Result<(), WriteError> {
	if id == other || note.len() > 4000 || (!remove && note.trim().is_empty()) {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let story = get(&tx, profile, id)?.ok_or(WriteError::Invalid)?;
	let target = get(&tx, profile, other)?.ok_or(WriteError::Invalid)?;
	if story.feed != target.feed {
		return Err(WriteError::Invalid);
	}
	if story.revision != revision {
		return Err(WriteError::Conflict);
	}
	let (left, right) = (id.min(other), id.max(other));
	if remove {
		tx.execute("DELETE FROM reading_story_links WHERE left_id=?1 AND right_id=?2", params![left, right])?;
	} else {
		tx.execute(
			"INSERT INTO reading_story_links(left_id,right_id,note) VALUES(?1,?2,?3) ON CONFLICT(left_id,right_id) DO UPDATE SET note=excluded.note",
			params![left, right, note],
		)?;
	}
	tx.execute("UPDATE reading_stories SET revision=revision+1 WHERE id IN(?1,?2)", params![id, other])?;
	tx.commit()?;
	Ok(())
}
fn compare(events: &[Event]) -> Vec<Comparison> {
	let mut groups = std::collections::BTreeMap::new();
	for e in events {
		if !e.community.is_empty() {
			let ids = groups.entry(e.community.to_lowercase()).or_insert_with(std::collections::HashSet::new);
			ids.insert(e.url.clone());
		}
	}
	groups.into_iter().map(|(community, ids)| Comparison { community, count: ids.len() }).collect()
}
#[derive(Template)]
#[template(path = "stories.html")]
struct Page {
	prefs: Preferences,
	url: String,
	stories: Vec<Story>,
	selected: Option<Story>,
	events: Vec<Event>,
	related: Vec<Related>,
	comparison: Vec<Comparison>,
	library: Vec<crate::library::Saved>,
	sources: Vec<crate::sources::SourceItem>,
}
pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to curate stories."));
	};
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
	let db = account::open_database()?;
	let selected = if let Some(id) = form.get("id") {
		let s = get(&db, profile, id.parse().unwrap_or(0)).map_err(|e| format!("{e:?}"))?;
		if s.is_none() {
			return Ok(reply(StatusCode::NOT_FOUND, "Story not found."));
		}
		s
	} else {
		None
	};
	let feed = selected.as_ref().map(|s| s.feed.clone()).unwrap_or_else(|| form.get("feed").cloned().unwrap_or_default());
	let mut stmt = db
		.prepare("SELECT id,feed,title,stage,note,revision FROM reading_stories WHERE profile_id=?1 AND (?2='' OR feed=?2) ORDER BY id DESC")
		.map_err(|e| e.to_string())?;
	let stories = stmt
		.query_map(params![profile, feed], row)
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let events = if let Some(s) = &selected {
		events(&db, profile, s.id).map_err(|e| format!("{e:?}"))?
	} else {
		vec![]
	};
	let comparison = compare(&events);
	let mut related = Vec::new();
	if let Some(s) = &selected {
		let mut stmt=db.prepare("SELECT s.id,s.title,l.note FROM reading_story_links l JOIN reading_stories s ON s.id=CASE WHEN l.left_id=?1 THEN l.right_id ELSE l.left_id END WHERE (l.left_id=?1 OR l.right_id=?1) AND s.profile_id=?2").map_err(|e|e.to_string())?;
		related = stmt
			.query_map(params![s.id, profile], |r| {
				Ok(Related {
					id: r.get(0)?,
					title: r.get(1)?,
					note: r.get(2)?,
				})
			})
			.map_err(|e| e.to_string())?
			.collect::<Result<Vec<_>, _>>()
			.map_err(|e| e.to_string())?;
	}
	let prefs = Preferences::new(&req);
	let members = prefs.feed_groups().into_iter().find(|f| f.slug == feed).map(|f| f.communities).unwrap_or_default();
	let library = crate::library::search(&db, profile, "", "", 0)
		.map_err(|e| format!("{e:?}"))?
		.into_iter()
		.filter(|i| members.iter().any(|m| m.eq_ignore_ascii_case(&i.community)))
		.collect();
	let sources = crate::sources::entries(&db, profile, &feed).map_err(|e| format!("{e:?}"))?;
	if form.get("export").is_some_and(|v| v == "json") {
		return Ok(
			Response::builder()
				.header("content-type", "application/json")
				.header("content-disposition", "attachment; filename=vale-story.json")
				.header("cache-control", "private, no-store")
				.body(Body::from(serde_json::json!({"format":"vale-story-v1","story":selected,"events":events}).to_string()))
				.unwrap(),
		);
	}
	Ok(template(&Page {
		prefs,
		url: req.uri().to_string(),
		stories,
		selected,
		events,
		related,
		comparison,
		library,
		sources,
	}))
}
pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to curate stories."));
	};
	let prefs = Preferences::new(&req);
	let bytes = crate::utils::read_body_limited(req.body_mut(), 131072, "Story form too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	let mut db = account::open_database()?;
	let mut id = v("id").parse().unwrap_or(0);
	let revision = v("revision").parse().unwrap_or(-1);
	let result = match v("action") {
		"create" => {
			if !prefs.feed_groups().iter().any(|f| f.slug == v("feed")) {
				Err(WriteError::Invalid)
			} else {
				create(&mut db, profile, v("feed"), v("title")).map(|new| id = new)
			}
		}
		"edit" | "remove" => {
			let result = edit(&mut db, profile, id, revision, v("title"), v("stage"), v("note"), v("action") == "remove");
			if result.is_ok() && v("action") == "remove" {
				id = 0
			}
			result
		}
		"relate" | "unrelate" => relate(&mut db, profile, id, revision, v("other").parse().unwrap_or(0), v("note"), v("action") == "unrelate"),
		"remove-event" => remove_event(&mut db, profile, id, revision, v("event").parse().unwrap_or(0)),
		"event" => {
			let Some(story) = get(&db, profile, id).map_err(|e| format!("{e:?}"))? else {
				return Ok(reply(StatusCode::NOT_FOUND, "Story not found."));
			};
			let date = chrono::NaiveDate::parse_from_str(v("date"), "%Y-%m-%d")
				.ok()
				.and_then(|d| d.and_hms_opt(0, 0, 0))
				.map(|d| d.and_utc().timestamp())
				.unwrap_or(-1);
			let evidence = v("evidence").split_once(':');
			let event = match evidence {
				Some(("library", ref_id)) => {
					let item = crate::library::get(&db, profile, ref_id.parse().unwrap_or(0)).map_err(|e| format!("{e:?}"))?;
					item
						.filter(|i| {
							prefs
								.feed_groups()
								.iter()
								.any(|f| f.slug == story.feed && f.communities.iter().any(|c| c.eq_ignore_ascii_case(&i.community)))
						})
						.map(|i| Event {
							id: 0,
							date,
							url: i.link(),
							title: i.title,
							community: i.community,
							body: i.body,
							note: v("note").into(),
							provenance: format!("Saved comment capture at {}", i.captured),
						})
				}
				Some(("source", ref_id)) => crate::sources::entries(&db, profile, &story.feed)
					.map_err(|e| format!("{e:?}"))?
					.into_iter()
					.find(|i| i.id == ref_id.parse::<i64>().unwrap_or(0))
					.map(|i| Event {
						id: 0,
						date,
						title: i.title,
						url: i.url,
						community: String::new(),
						body: i.body,
						note: v("note").into(),
						provenance: format!("Selected source entry; published {}", i.published),
					}),
				_ => None,
			};
			match event {
				Some(event) => add_event(&mut db, profile, id, revision, &event),
				None => Err(WriteError::Invalid),
			}
		}
		_ => Err(WriteError::Invalid),
	};
	if let Err(e) = result {
		return Ok(reply(
			if e == WriteError::Conflict {
				StatusCode::CONFLICT
			} else {
				StatusCode::UNPROCESSABLE_ENTITY
			},
			"The story changed or the selection is invalid. Use Back to preserve your draft, then reload to review it.",
		));
	}
	Ok(
		Response::builder()
			.status(StatusCode::SEE_OTHER)
			.header("location", if id == 0 { "/reading/stories".into() } else { format!("/reading/stories?id={id}") })
			.header("cache-control", "private, no-store")
			.body(Body::empty())
			.unwrap(),
	)
}
fn reply(status: StatusCode, text: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "private, no-store")
		.body(Body::from(text.to_string()))
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
		db
	}
	fn event() -> Event {
		Event {
			id: 0,
			date: 100000,
			title: "Original evidence".into(),
			url: "/comments/post1/comments/reply1".into(),
			community: "rust".into(),
			body: "Captured passage".into(),
			note: "My assessment".into(),
			provenance: "Saved capture".into(),
		}
	}
	#[test]
	fn creation_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut profile = 1;
			let mut feed = "rust".to_string();
			let mut title = "Release story".to_string();
			match case {
				0 => {}
				1 => profile = 99,
				2 => feed.clear(),
				3 => feed = "x".repeat(81),
				4 => title.clear(),
				5 => title = " ".into(),
				6 => title = "x".repeat(501),
				7 => {
					initialize(&db).unwrap();
				}
				8 => profile = 2,
				9 => title = "Unicode 日本語".into(),
				10 => {
					for _ in 0..200 {
						create(&mut db, 1, "rust", "Old").unwrap();
					}
				}
				11 => title = "<literal>".into(),
				_ => {}
			}
			assert_eq!(create(&mut db, profile, &feed, &title).is_ok(), matches!(case, 0 | 7 | 8 | 9 | 11), "case {case}");
		}
	}
	fn edit_edges(remove: bool) {
		for case in 0..12 {
			let mut db = db();
			let id = create(&mut db, 1, "rust", "Story").unwrap();
			let mut profile = 1;
			let mut target = id;
			let mut revision = 1;
			let mut title = "Edited".to_string();
			let mut stage = "released";
			let mut note = "My judgment".to_string();
			match case {
				0 => {}
				1 => profile = 2,
				2 => target = 999,
				3 => revision = 0,
				4 => title.clear(),
				5 => title = "x".repeat(501),
				6 => stage = "imagined",
				7 => note = "x".repeat(32769),
				8 => {
					edit(&mut db, 1, id, 1, "Earlier", "watching", "", false).unwrap();
				}
				9 => note.clear(),
				10 => {
					initialize(&db).unwrap();
				}
				11 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				_ => {}
			}
			assert_eq!(
				edit(&mut db, profile, target, revision, &title, stage, &note, remove).is_ok(),
				matches!(case, 0 | 9 | 10),
				"remove={remove} case {case}"
			);
		}
	}
	#[test]
	fn lifecycle_edit_twelve_edges() {
		edit_edges(false)
	}
	#[test]
	fn story_removal_twelve_edges() {
		edit_edges(true)
	}
	#[test]
	fn evidence_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = create(&mut db, 1, "rust", "Story").unwrap();
			let mut e = event();
			let mut profile = 1;
			let mut revision = 1;
			match case {
				0 => {}
				1 => profile = 2,
				2 => revision = 0,
				3 => e.date = -1,
				4 => e.title.clear(),
				5 => e.url = "javascript:alert(1)".into(),
				6 => e.body = "x".repeat(65537),
				7 => e.note = "x".repeat(32769),
				8 => e.url = "https://example.org/release".into(),
				9 => {
					initialize(&db).unwrap();
				}
				10 => {
					for rev in 1..=200 {
						add_event(&mut db, 1, id, rev, &event()).unwrap();
					}
					revision = 201;
				}
				11 => e.body = "日本語".into(),
				_ => {}
			}
			assert_eq!(add_event(&mut db, profile, id, revision, &e).is_ok(), matches!(case, 0 | 8 | 9 | 11), "case {case}");
		}
	}
	#[test]
	fn event_removal_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = create(&mut db, 1, "rust", "Story").unwrap();
			add_event(&mut db, 1, id, 1, &event()).unwrap();
			let event_id = events(&db, 1, id).unwrap()[0].id;
			let mut profile = 1;
			let mut target = id;
			let mut revision = 2;
			let mut selected = event_id;
			match case {
				0 => {}
				1 => profile = 2,
				2 => target = 999,
				3 => revision = 0,
				4 => revision = 1,
				5 => selected = 999,
				6 => {
					remove_event(&mut db, 1, id, 2, event_id).unwrap();
				}
				7 => {
					initialize(&db).unwrap();
				}
				8 => {
					let other = create(&mut db, 1, "rust", "Other").unwrap();
					target = other;
					revision = 1;
				}
				9 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				10 => selected = -1,
				11 => revision = 3,
				_ => {}
			}
			assert_eq!(remove_event(&mut db, profile, target, revision, selected).is_ok(), matches!(case, 0 | 7), "case {case}");
		}
	}
	fn relate_edges(remove: bool) {
		for case in 0..12 {
			let mut db = db();
			let id = create(&mut db, 1, "rust", "First").unwrap();
			let other = create(&mut db, 1, "rust", "Second").unwrap();
			let mut profile = 1;
			let mut target = other;
			let mut revision = 1;
			let mut note = "Shared source".to_string();
			match case {
				0 => {}
				1 => profile = 2,
				2 => target = id,
				3 => target = 999,
				4 => revision = 0,
				5 => note = "x".repeat(4001),
				6 => {
					db.execute("UPDATE reading_stories SET feed='other' WHERE id=?1", [other]).unwrap();
				}
				7 => {
					db.execute("UPDATE reading_stories SET profile_id=2 WHERE id=?1", [other]).unwrap();
				}
				8 => {
					initialize(&db).unwrap();
				}
				9 => {
					relate(&mut db, 1, id, 1, other, "Earlier", false).unwrap();
				}
				10 => note.clear(),
				11 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				_ => {}
			}
			let ok = matches!(case, 0 | 8) || (case == 10 && remove);
			assert_eq!(relate(&mut db, profile, id, revision, target, &note, remove).is_ok(), ok, "remove={remove} case {case}");
		}
	}
	#[test]
	fn association_twelve_edges() {
		relate_edges(false)
	}
	#[test]
	fn unlink_twelve_edges() {
		relate_edges(true)
	}
	#[test]
	fn comparison_twelve_edges() {
		for case in 0..12 {
			let mut events = vec![event()];
			match case {
				0 => events.clear(),
				1 => events.push(event()),
				2 => {
					let mut second = event();
					second.url = "/comments/other".into();
					events.push(second);
				}
				3 => events[0].community.clear(),
				4 => events[0].community = "RUST".into(),
				5 => {
					let mut second = event();
					second.community = "homelab".into();
					events.push(second);
				}
				6 => events[0].body = "Disagree".into(),
				7 => events[0].note = "Agree".into(),
				8 => events[0].date = 0,
				9 => events[0].body.clear(),
				10 => events[0].title = "Another title".into(),
				11 => events[0].provenance.clear(),
				_ => {}
			}
			let result = compare(&events);
			assert_eq!(
				result.iter().map(|g| g.count).sum::<usize>(),
				match case {
					0 | 3 => 0,
					2 | 5 => 2,
					_ => 1,
				},
				"case {case}"
			);
		}
	}
}
