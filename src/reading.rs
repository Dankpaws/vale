//! Explicit reading state. Opening a page never advances a checkpoint.
use crate::{
	account,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadingEntry {
	pub post_id: String,
	pub title: String,
	pub community: String,
	pub anchor: String,
	#[serde(default)]
	pub resume: String,
	pub bookmarked: bool,
	pub followed: bool,
	pub caught_up_at: i64,
	pub revision: i64,
	pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResumeState {
	pub sort: String,
	pub offset: i64,
	pub group_states: Vec<ResumeToggle>,
	pub comment_states: Vec<ResumeToggle>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeToggle {
	pub id: String,
	pub expanded: bool,
}
impl Default for ResumeState {
	fn default() -> Self {
		Self {
			sort: "confidence".into(),
			offset: 0,
			group_states: vec![],
			comment_states: vec![],
		}
	}
}
impl ResumeState {
	pub fn parse(raw: &str) -> Result<Self, WriteError> {
		if raw.is_empty() {
			return Ok(Self::default());
		}
		if raw.len() > 120_000 {
			return Err(WriteError::Invalid);
		}
		let state: Self = serde_json::from_str(raw).map_err(|_| WriteError::Invalid)?;
		if !["confidence", "top", "new", "controversial", "old"].contains(&state.sort.as_str())
			|| !(-100_000..=10_000).contains(&state.offset)
			|| state.group_states.len() > 200
			|| state.comment_states.len() > 1000
		{
			return Err(WriteError::Invalid);
		}
		for list in [&state.group_states, &state.comment_states] {
			let mut ids = std::collections::HashSet::new();
			for t in list {
				if !t.id.strip_prefix("t1_").is_some_and(account::valid_post_id) || !ids.insert(&t.id) {
					return Err(WriteError::Invalid);
				}
			}
		}
		Ok(state)
	}
}
impl ReadingEntry {
	pub fn resume_state(&self) -> ResumeState {
		ResumeState::parse(&self.resume).unwrap_or_default()
	}

	pub fn permalink(&self) -> String {
		if self.anchor.is_empty() {
			format!("/comments/{}#post-top", self.post_id)
		} else {
			format!("/comments/{}?resume=1&sort={}#{}", self.post_id, self.resume_state().sort, self.anchor)
		}
	}
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
	Invalid,
	Conflict,
	Full,
	Database(String),
}
impl From<rusqlite::Error> for WriteError {
	fn from(error: rusqlite::Error) -> Self {
		Self::Database(error.to_string())
	}
}

pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch(
		"CREATE TABLE IF NOT EXISTS reading_entries (
	 profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
	 post_id TEXT NOT NULL, title TEXT NOT NULL, community TEXT NOT NULL,
	 anchor TEXT NOT NULL DEFAULT '', bookmarked INTEGER NOT NULL DEFAULT 0,
	 followed INTEGER NOT NULL DEFAULT 0, caught_up_at INTEGER NOT NULL DEFAULT 0,
	 revision INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL,
	 PRIMARY KEY(profile_id, post_id));
	 CREATE INDEX IF NOT EXISTS reading_entries_recent ON reading_entries(profile_id,updated_at DESC);
     CREATE TABLE IF NOT EXISTS reading_clock(profile_id INTEGER PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE, revision INTEGER NOT NULL);
     INSERT INTO reading_clock(profile_id,revision) SELECT profile_id,max(revision) FROM reading_entries GROUP BY profile_id ON CONFLICT(profile_id) DO UPDATE SET revision=max(reading_clock.revision,excluded.revision);",
 )?;
	let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('reading_entries') WHERE name='resume')", [], |r| r.get(0))?;
	if !exists {
		db.execute_batch("ALTER TABLE reading_entries ADD COLUMN resume TEXT NOT NULL DEFAULT '';")?;
	}
	Ok(())
}

fn valid_anchor(value: &str) -> bool {
	value.is_empty() || (value.len() <= 64 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
}

pub fn get(db: &Connection, profile: i64, post: &str) -> Result<ReadingEntry, WriteError> {
	Ok(
		db.query_row(
			"SELECT post_id,title,community,anchor,bookmarked,followed,caught_up_at,revision,updated_at,resume FROM reading_entries WHERE profile_id=?1 AND post_id=?2",
			params![profile, post],
			row,
		)
		.optional()?
		.unwrap_or_else(|| ReadingEntry {
			post_id: post.to_string(),
			..Default::default()
		}),
	)
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReadingEntry> {
	Ok(ReadingEntry {
		post_id: row.get(0)?,
		title: row.get(1)?,
		community: row.get(2)?,
		anchor: row.get(3)?,
		bookmarked: row.get(4)?,
		followed: row.get(5)?,
		caught_up_at: row.get(6)?,
		revision: row.get(7)?,
		updated_at: row.get(8)?,
		resume: row.get(9)?,
	})
}

/// Expected revision zero creates a record. Each command changes only its own
/// field; stale tabs cannot overwrite a newer checkpoint or another command.
// Keep the explicit transaction fields together at this persistence boundary.
#[allow(clippy::too_many_arguments)]
pub fn command(
	db: &mut Connection,
	profile: i64,
	post: &str,
	title: &str,
	community: &str,
	expected: i64,
	action: &str,
	anchor: &str,
	now: i64,
) -> Result<ReadingEntry, WriteError> {
	command_with_resume(db, profile, post, title, community, expected, action, anchor, now, None)
}
// Keep the explicit transaction fields together at this persistence boundary.
#[allow(clippy::too_many_arguments)]
fn command_with_resume(
	db: &mut Connection,
	profile: i64,
	post: &str,
	title: &str,
	community: &str,
	expected: i64,
	action: &str,
	anchor: &str,
	now: i64,
	resume: Option<&str>,
) -> Result<ReadingEntry, WriteError> {
	let resume = resume.map(ResumeState::parse).transpose()?;

	if !account::valid_post_id(post)
		|| expected < 0
		|| now < 0
		|| title.trim().is_empty()
		|| title.chars().count() > 500
		|| community.is_empty()
		|| community.len() > 64
		|| !community.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
		|| !valid_anchor(anchor)
	{
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let mut entry = get(&tx, profile, post)?;
	if entry.revision != expected {
		return Err(WriteError::Conflict);
	}
	if expected == 0 && tx.query_row("SELECT count(*) FROM reading_entries WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 5000 {
		return Err(WriteError::Full);
	}
	match action {
		"forget" => {
			tx.execute("DELETE FROM reading_entries WHERE profile_id=?1 AND post_id=?2", params![profile, post])?;
			tx.commit()?;
			return Ok(ReadingEntry {
				post_id: post.into(),
				..Default::default()
			});
		}
		"bookmark" => entry.bookmarked = true,
		"unbookmark" => entry.bookmarked = false,
		"follow" => {
			if !entry.followed && tx.query_row("SELECT count(*) FROM reading_entries WHERE profile_id=?1 AND followed=1", [profile], |r| r.get::<_, i64>(0))? >= 100 {
				return Err(WriteError::Full);
			}
			entry.followed = true;
		}
		"unfollow" => entry.followed = false,
		"checkpoint" => entry.anchor = anchor.to_string(),
		"caught-up" => {
			crate::watch::acknowledge(&tx, profile, post)?;
			entry.caught_up_at = now.max(entry.caught_up_at);
			entry.anchor = anchor.to_string();
		}
		_ => return Err(WriteError::Invalid),
	}
	if matches!(action, "checkpoint" | "caught-up") {
		entry.resume = serde_json::to_string(&resume.unwrap_or_default()).map_err(|_| WriteError::Invalid)?;
	}
	entry.title = title.trim().to_string();
	entry.community = community.to_string();
	entry.revision = tx.query_row(
		"INSERT INTO reading_clock(profile_id,revision) VALUES(?1,1) ON CONFLICT(profile_id) DO UPDATE SET revision=reading_clock.revision+1 RETURNING revision",
		[profile],
		|r| r.get(0),
	)?;
	entry.updated_at = now.max(entry.updated_at);
	tx.execute("INSERT INTO reading_entries(profile_id,post_id,title,community,anchor,bookmarked,followed,caught_up_at,revision,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
	 ON CONFLICT(profile_id,post_id) DO UPDATE SET title=excluded.title,community=excluded.community,anchor=excluded.anchor,bookmarked=excluded.bookmarked,followed=excluded.followed,caught_up_at=excluded.caught_up_at,revision=excluded.revision,updated_at=excluded.updated_at",
	 params![profile,post,entry.title,entry.community,entry.anchor,entry.bookmarked,entry.followed,entry.caught_up_at,entry.revision,entry.updated_at])?;
	tx.execute(
		"UPDATE reading_entries SET resume=?3 WHERE profile_id=?1 AND post_id=?2",
		params![profile, post, entry.resume],
	)?;
	tx.commit()?;
	Ok(entry)
}

#[derive(Template)]
#[template(path = "reading.html")]
struct ReadingTemplate {
	prefs: Preferences,
	url: String,
	entries: Vec<ReadingEntry>,
	updates: Vec<crate::sources::FeedUpdate>,
}

pub async fn list_get(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(context) = account::context(&req) else {
		return Ok(response(StatusCode::UNAUTHORIZED, "Sign in to keep reading state."));
	};
	let db = account::open_database()?;
	let mut statement = db.prepare("SELECT post_id,title,community,anchor,bookmarked,followed,caught_up_at,revision,updated_at,resume FROM reading_entries WHERE profile_id=?1 ORDER BY updated_at DESC,post_id LIMIT 5000").map_err(|e| e.to_string())?;
	let entries = statement
		.query_map([context.profile_id], row)
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	Ok(template(&ReadingTemplate {
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		entries,
		updates: crate::sources::updates(&db, context.profile_id).map_err(|e| format!("{e:?}"))?,
	}))
}

pub async fn command_post(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(response(StatusCode::UNAUTHORIZED, "Sign in to keep reading state."));
	};
	let body = crate::utils::read_body_limited(req.body_mut(), 131072, "Reading command is too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
	let value = |key: &str| form.get(key).map(String::as_str).unwrap_or_default();
	let Ok(revision) = value("revision").parse::<i64>() else {
		return Ok(response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid revision."));
	};
	let mut db = account::open_database()?;
	match command_with_resume(
		&mut db,
		profile,
		value("post_id"),
		value("title"),
		value("community"),
		revision,
		value("action"),
		value("anchor"),
		account::now(),
		Some(value("resume_state")),
	) {
		Ok(entry) if req.headers().get("accept").and_then(|h| h.to_str().ok()) == Some("application/json") => Ok(
			Response::builder()
				.header("content-type", "application/json")
				.header("cache-control", "private, no-store")
				.body(Body::from(serde_json::json!({"revision":entry.revision,"url":entry.permalink()}).to_string()))
				.unwrap(),
		),
		Ok(_) => Ok(
			Response::builder()
				.status(StatusCode::SEE_OTHER)
				.header("location", crate::utils::safe_local_redirect(value("return_to"), "/reading", 1024))
				.header("cache-control", "private, no-store")
				.body(Body::empty())
				.unwrap(),
		),
		Err(WriteError::Conflict) => Ok(response(StatusCode::CONFLICT, "Reading state changed in another tab. Reload before trying again.")),
		Err(WriteError::Full) => Ok(response(StatusCode::CONFLICT, "Reading capacity reached (5,000 saved discussions or 100 active follows).")),
		Err(WriteError::Invalid) => Ok(response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid reading command.")),
		Err(WriteError::Database(error)) => Err(error),
	}
}

fn response(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "private, no-store")
		.body(Body::from(message.to_string()))
		.unwrap()
}

#[cfg(test)]
mod tests {
	use super::*;
	fn database() -> Connection {
		let db = Connection::open_in_memory().unwrap();
		db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE profiles(id INTEGER PRIMARY KEY); INSERT INTO profiles VALUES(1),(2);")
			.unwrap();
		initialize(&db).unwrap();
		crate::watch::initialize(&db).unwrap();
		db
	}
	#[test]
	fn resume_validation_twelve_edges() {
		for case in 0..12 {
			let mut state = serde_json::json!({"sort":"new","offset":-30,"groupStates":[{"id":"t1_abc","expanded":true}],"commentStates":[]});
			match case {
				1 => state["sort"] = "random".into(),
				2 => state["offset"] = 10001.into(),
				3 => state["offset"] = (-100001).into(),
				4 => state["groupStates"][0]["id"] = "bad/id".into(),
				5 => state["groupStates"] = serde_json::json!([{"id":"t1_abc","expanded":true},{"id":"t1_abc","expanded":false}]),
				6 => state["commentStates"] = serde_json::json!(vec![serde_json::json!({"id":"t1_abc","expanded":true}); 1001]),
				7 => state["extra"] = "no".into(),
				8 => state["offset"] = "NaN".into(),
				9 => state["groupStates"][0]["expanded"] = "true".into(),
				10 => state["sort"] = "old".into(),
				11 => state["offset"] = (-100000).into(),
				_ => {}
			}
			assert_eq!(ResumeState::parse(&state.to_string()).is_ok(), matches!(case, 0 | 10 | 11), "case {case}");
		}
	}
	#[test]
	fn resume_persistence_twelve_edges() {
		for case in 0..12 {
			let mut db = database();
			let state = serde_json::json!({"sort":"old","offset":-25,"groupStates":[],"commentStates":[]}).to_string();
			let state = serde_json::to_string(&ResumeState::parse(&state).unwrap()).unwrap();
			let entry = command_with_resume(&mut db, 1, "abc123", "Title", "rust", 0, "checkpoint", "comment123", 100, Some(&state)).unwrap();
			assert!(entry.permalink().contains("/comments/abc123?resume=1&sort=old#comment123"));
			assert!(!entry.permalink().contains("context="));
			match case {
				0 => assert_eq!(get(&db, 1, "abc123").unwrap().resume, state),
				1 => assert!(get(&db, 2, "abc123").unwrap().resume.is_empty()),
				2 => {
					let x = command(&mut db, 1, "abc123", "Title", "rust", entry.revision, "bookmark", "", 101).unwrap();
					assert_eq!(x.resume, state);
				}
				3 => {
					let x = command(&mut db, 1, "abc123", "Title", "rust", entry.revision, "follow", "", 101).unwrap();
					assert_eq!(x.resume, state);
				}
				4 => assert_eq!(
					command_with_resume(&mut db, 1, "abc123", "Title", "rust", 0, "checkpoint", "other", 101, Some(&state)),
					Err(WriteError::Conflict)
				),
				5 => {
					assert!(command_with_resume(&mut db, 1, "abc123", "Title", "rust", entry.revision, "checkpoint", "other", 101, Some("bad")).is_err());
					assert_eq!(get(&db, 1, "abc123").unwrap().anchor, "comment123");
				}
				6 => {
					command(&mut db, 1, "abc123", "Title", "rust", entry.revision, "forget", "", 101).unwrap();
					assert!(get(&db, 1, "abc123").unwrap().resume.is_empty());
				}
				7 => {
					initialize(&db).unwrap();
					initialize(&db).unwrap();
					assert_eq!(get(&db, 1, "abc123").unwrap().resume, state);
				}
				8 => {
					let x = command_with_resume(&mut db, 1, "abc123", "Title", "rust", entry.revision, "checkpoint", "post-top", 101, Some(&state)).unwrap();
					assert!(x.permalink().ends_with("?resume=1&sort=old#post-top"));
				}
				9 => {
					let x = command(&mut db, 1, "abc123", "Title", "rust", entry.revision, "checkpoint", "other", 101).unwrap();
					assert_eq!(x.resume_state().sort, "confidence");
				}
				10 => assert!(get(&db, 1, "different").unwrap().resume.is_empty()),
				11 => {
					assert!(command_with_resume(
						&mut db,
						1,
						"abc123",
						"Title",
						"rust",
						entry.revision,
						"checkpoint",
						"comment123",
						101,
						Some(&"x".repeat(120001))
					)
					.is_err());
				}
				_ => {}
			}
		}
	}

	fn apply(db: &mut Connection, profile: i64, revision: i64, action: &str) -> Result<ReadingEntry, WriteError> {
		command(db, profile, "abc123", "A useful discussion", "rust", revision, action, "comment123", 100)
	}
	// Each public reading command is exercised against the same twelve edge cases.
	// The loop label is part of assertion output, making failures attributable.
	fn command_edges(action: &str) {
		for case in 0..12 {
			let mut db = database();
			match case {
				0 => {
					let x = apply(&mut db, 1, 0, action).unwrap();
					assert_eq!(x.revision, 1, "{action}: first write");
				}
				1 => {
					apply(&mut db, 1, 0, action).unwrap();
					assert_eq!(apply(&mut db, 1, 0, action), Err(WriteError::Conflict), "{action}: stale replay");
				}
				2 => {
					apply(&mut db, 1, 0, action).unwrap();
					assert_eq!(get(&db, 2, "abc123").unwrap().revision, 0, "{action}: profile isolation");
					apply(&mut db, 2, 0, action).unwrap();
				}
				3 => {
					assert_eq!(apply(&mut db, 1, -1, action), Err(WriteError::Invalid), "{action}: negative revision");
				}
				4 => {
					assert_eq!(
						command(&mut db, 1, "../bad", "Title", "rust", 0, action, "", 100),
						Err(WriteError::Invalid),
						"{action}: invalid post"
					);
				}
				5 => {
					assert_eq!(
						command(&mut db, 1, "abc123", "  ", "rust", 0, action, "", 100),
						Err(WriteError::Invalid),
						"{action}: blank title"
					);
				}
				6 => {
					assert_eq!(
						command(&mut db, 1, "abc123", &"x".repeat(501), "rust", 0, action, "", 100),
						Err(WriteError::Invalid),
						"{action}: title limit"
					);
				}
				7 => {
					assert_eq!(
						command(&mut db, 1, "abc123", "Title", "../rust", 0, action, "", 100),
						Err(WriteError::Invalid),
						"{action}: invalid community"
					);
				}
				8 => {
					assert_eq!(
						command(&mut db, 1, "abc123", "Title", "rust", 0, action, "x\" onfocus=evil", 100),
						Err(WriteError::Invalid),
						"{action}: unsafe anchor"
					);
				}
				9 => {
					assert_eq!(
						command(&mut db, 1, "abc123", "Title", "rust", 0, action, "", -1),
						Err(WriteError::Invalid),
						"{action}: negative time"
					);
				}
				10 => {
					apply(&mut db, 1, 0, action).unwrap();
					let x = apply(&mut db, 1, 1, action).unwrap();
					assert_eq!(x.revision, 2, "{action}: repeated intent with fresh revision");
				}
				11 => {
					db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000) INSERT INTO reading_entries(profile_id,post_id,title,community,updated_at) SELECT 1,'p'||x,'Title','rust',1 FROM n;").unwrap();
					assert_eq!(apply(&mut db, 1, 0, action), Err(WriteError::Full), "{action}: bounded library");
					apply(&mut db, 2, 0, action).unwrap();
				}
				_ => unreachable!(),
			}
		}
	}
	#[test]
	fn bookmark_twelve_edges() {
		command_edges("bookmark");
	}
	#[test]
	fn remove_bookmark_twelve_edges() {
		command_edges("unbookmark");
	}
	#[test]
	fn follow_twelve_edges() {
		command_edges("follow");
	}
	#[test]
	fn unfollow_twelve_edges() {
		command_edges("unfollow");
	}
	#[test]
	fn checkpoint_twelve_edges() {
		command_edges("checkpoint");
	}
	#[test]
	fn caught_up_twelve_edges() {
		command_edges("caught-up");
	}
	#[test]
	fn forget_twelve_edges() {
		for case in 0..12 {
			let mut db = database();
			let entry = apply(&mut db, 1, 0, "bookmark").unwrap();
			match case {
				0 => {
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					assert_eq!(get(&db, 1, "abc123").unwrap().revision, 0);
				}
				1 => assert_eq!(apply(&mut db, 1, 0, "forget"), Err(WriteError::Conflict)),
				2 => {
					apply(&mut db, 2, 0, "bookmark").unwrap();
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					assert!(get(&db, 2, "abc123").unwrap().bookmarked);
				}
				3 => {
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					let fresh = apply(&mut db, 1, 0, "bookmark").unwrap();
					assert!(fresh.revision > entry.revision);
					assert_eq!(apply(&mut db, 1, entry.revision, "forget"), Err(WriteError::Conflict));
				}
				4 => assert_eq!(apply(&mut db, 1, -1, "forget"), Err(WriteError::Invalid)),
				5 => {
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					assert_eq!(apply(&mut db, 1, entry.revision, "forget"), Err(WriteError::Conflict));
				}
				6 => {
					command(&mut db, 1, "other", "Other", "rust", 0, "bookmark", "", 100).unwrap();
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					assert!(get(&db, 1, "other").unwrap().bookmarked);
				}
				7 => {
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					initialize(&db).unwrap();
					let fresh = apply(&mut db, 1, 0, "bookmark").unwrap();
					assert!(fresh.revision > entry.revision);
				}
				8 => {
					assert_eq!(command(&mut db, 1, "../bad", "Title", "rust", 0, "forget", "", 100), Err(WriteError::Invalid));
				}
				9 => {
					assert_eq!(
						command(&mut db, 1, "abc123", "Title", "rust", entry.revision, "forget", "bad/anchor", 100),
						Err(WriteError::Invalid)
					);
					assert!(get(&db, 1, "abc123").unwrap().bookmarked);
				}
				10 => {
					assert_eq!(command(&mut db, 1, "abc123", "Title", "rust", entry.revision, "forget", "", -1), Err(WriteError::Invalid));
				}
				11 => {
					apply(&mut db, 1, entry.revision, "forget").unwrap();
					assert_eq!(apply(&mut db, 1, 0, "forget").unwrap().revision, 0);
				}
				_ => unreachable!(),
			}
		}
	}

	#[test]
	fn reading_commands_preserve_independent_intent() {
		let mut db = database();
		apply(&mut db, 1, 0, "bookmark").unwrap();
		apply(&mut db, 1, 1, "follow").unwrap();
		let x = apply(&mut db, 1, 2, "checkpoint").unwrap();
		assert!(x.bookmarked && x.followed);
		assert_eq!(x.anchor, "comment123");
		assert_eq!(x.caught_up_at, 0);
		let x = apply(&mut db, 1, 3, "caught-up").unwrap();
		assert_eq!(x.caught_up_at, 100);
		let x = command(&mut db, 1, "abc123", "Title", "rust", 4, "caught-up", "", 90).unwrap();
		assert_eq!(x.caught_up_at, 100);
		assert_eq!(x.updated_at, 100);
	}
	#[test]
	fn unknown_command_rolls_back_without_creating_state() {
		let mut db = database();
		assert_eq!(apply(&mut db, 1, 0, "delete-everything"), Err(WriteError::Invalid));
		assert_eq!(get(&db, 1, "abc123").unwrap().revision, 0);
	}
	#[test]
	fn missing_profile_fails_foreign_key() {
		let mut db = database();
		assert!(matches!(apply(&mut db, 9, 0, "bookmark"), Err(WriteError::Database(_))));
	}
	#[test]
	fn migration_is_repeatable_and_preserves_records() {
		let mut db = database();
		let x = apply(&mut db, 1, 0, "bookmark").unwrap();
		initialize(&db).unwrap();
		assert_eq!(get(&db, 1, "abc123").unwrap(), x);
	}
}
