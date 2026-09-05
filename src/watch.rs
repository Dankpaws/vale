//! Bounded observations for explicitly followed discussions. Counts describe
//! retrieved identities, not Reddit's reported totals or inferred unread state.
use crate::{
	account,
	reading::WriteError,
	thread::ThreadModel,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const MAX_COMMENTS: i64 = 10_000;
const POLL_SECONDS: i64 = 900;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedComment {
	pub id: String,
	pub parent: String,
	pub ancestors: Vec<String>,
	pub author: String,
	pub body: String,
	pub created: i64,
	pub first_seen: i64,
}

#[derive(Clone, Debug, Default)]
pub struct WatchState {
	pub baseline: i64,
	pub checked: i64,
	pub snoozed_until: i64,
	pub complete: bool,
	pub error: String,
	pub new_count: i64,
}

impl WatchState {
	pub fn snoozed(&self) -> bool {
		self.snoozed_until > account::now()
	}

	pub fn checked_label(&self) -> String {
		crate::utils::time(self.checked as f64).0
	}
}

fn refresh_due(state: &WatchState, now: i64) -> bool {
	now >= 0 && state.snoozed_until <= now && (state.checked == 0 || now.saturating_sub(state.checked) >= POLL_SECONDS)
}

pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch(
		"CREATE TABLE IF NOT EXISTS watch_state (
	 profile_id INTEGER NOT NULL, post_id TEXT NOT NULL, baseline INTEGER NOT NULL DEFAULT -1,
	 checked INTEGER NOT NULL DEFAULT 0, snoozed_until INTEGER NOT NULL DEFAULT 0,
	 complete INTEGER NOT NULL DEFAULT 0, error TEXT NOT NULL DEFAULT '',
	 PRIMARY KEY(profile_id,post_id), FOREIGN KEY(profile_id,post_id) REFERENCES reading_entries(profile_id,post_id) ON DELETE CASCADE);
	 CREATE TABLE IF NOT EXISTS watch_comments (
	 sequence INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER NOT NULL, post_id TEXT NOT NULL,
	 comment_id TEXT NOT NULL, parent TEXT NOT NULL, ancestors TEXT NOT NULL, author TEXT NOT NULL,
	 body TEXT NOT NULL, created INTEGER NOT NULL, first_seen INTEGER NOT NULL,
	 UNIQUE(profile_id,post_id,comment_id), FOREIGN KEY(profile_id,post_id) REFERENCES reading_entries(profile_id,post_id) ON DELETE CASCADE);
	 CREATE TABLE IF NOT EXISTS watch_branches (
	 profile_id INTEGER NOT NULL, post_id TEXT NOT NULL, comment_id TEXT NOT NULL,
	 PRIMARY KEY(profile_id,post_id,comment_id), FOREIGN KEY(profile_id,post_id) REFERENCES reading_entries(profile_id,post_id) ON DELETE CASCADE);",
	)
}

const CHANGED: &str = "c.profile_id=?1 AND c.post_id=?2 AND c.sequence>?3 AND (
 NOT EXISTS(SELECT 1 FROM watch_branches b WHERE b.profile_id=c.profile_id AND b.post_id=c.post_id)
 OR EXISTS(SELECT 1 FROM watch_branches b WHERE b.profile_id=c.profile_id AND b.post_id=c.post_id AND
 (b.comment_id=c.comment_id OR instr(c.ancestors,char(34)||b.comment_id||char(34))>0)))";

pub fn state(db: &Connection, profile: i64, post: &str) -> Result<WatchState, WriteError> {
	let mut result = load_state(db, profile, post)?;
	if result.baseline >= 0 {
		result.new_count = db.query_row(
			&format!("SELECT count(*) FROM watch_comments c WHERE {CHANGED}"),
			params![profile, post, result.baseline],
			|r| r.get(0),
		)?;
	}
	Ok(result)
}
fn load_state(db: &Connection, profile: i64, post: &str) -> Result<WatchState, WriteError> {
	Ok(
		db.query_row(
			"SELECT baseline,checked,snoozed_until,complete,error FROM watch_state WHERE profile_id=?1 AND post_id=?2",
			params![profile, post],
			|r| {
				Ok(WatchState {
					baseline: r.get(0)?,
					checked: r.get(1)?,
					snoozed_until: r.get(2)?,
					complete: r.get(3)?,
					error: r.get(4)?,
					new_count: 0,
				})
			},
		)
		.optional()?
		.unwrap_or_default(),
	)
}

pub fn changes(db: &Connection, profile: i64, post: &str, baseline: i64) -> Result<Vec<ObservedComment>, WriteError> {
	changes_page(db, profile, post, baseline, 0)
}
fn changes_page(db: &Connection, profile: i64, post: &str, baseline: i64, offset: i64) -> Result<Vec<ObservedComment>, WriteError> {
	if !(0..=MAX_COMMENTS).contains(&offset) {
		return Err(WriteError::Invalid);
	}
	let mut stmt = db.prepare(&format!(
		"SELECT c.comment_id,c.parent,c.ancestors,c.author,c.body,c.created,c.first_seen FROM watch_comments c WHERE {CHANGED} ORDER BY c.sequence LIMIT 200 OFFSET ?4"
	))?;
	let rows = stmt.query_map(params![profile, post, baseline, offset], |r| {
		Ok(ObservedComment {
			id: r.get(0)?,
			parent: r.get(1)?,
			ancestors: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
			author: r.get(3)?,
			body: r.get(4)?,
			created: r.get(5)?,
			first_seen: r.get(6)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn acknowledge(db: &Connection, profile: i64, post: &str) -> rusqlite::Result<()> {
	db.execute(
		"UPDATE watch_state SET baseline=coalesce((SELECT max(sequence) FROM watch_comments WHERE profile_id=?1 AND post_id=?2),0) WHERE profile_id=?1 AND post_id=?2",
		params![profile, post],
	)?;
	Ok(())
}

pub fn observe(db: &mut Connection, profile: i64, post: &str, comments: &[ObservedComment], complete: bool, now: i64) -> Result<(), WriteError> {
	if now < 0
		|| comments.len() > 2000
		|| comments.iter().any(|c| {
			!account::valid_post_id(&c.id) || c.body.len() > 32768 || c.author.len() > 128 || c.ancestors.len() > 256 || c.ancestors.iter().any(|id| !account::valid_post_id(id))
		}) {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let followed = tx
		.query_row("SELECT followed FROM reading_entries WHERE profile_id=?1 AND post_id=?2", params![profile, post], |r| {
			r.get::<_, bool>(0)
		})
		.optional()?
		.unwrap_or(false);
	if !followed {
		return Ok(());
	}
	let initialized = tx.query_row(
		"SELECT EXISTS(SELECT 1 FROM watch_state WHERE profile_id=?1 AND post_id=?2 AND baseline>=0)",
		params![profile, post],
		|r| r.get::<_, bool>(0),
	)?;
	let mut remaining = MAX_COMMENTS - tx.query_row("SELECT count(*) FROM watch_comments WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))?;
	let mut limited = false;
	for comment in comments {
		let exists = tx.query_row(
			"SELECT EXISTS(SELECT 1 FROM watch_comments WHERE profile_id=?1 AND post_id=?2 AND comment_id=?3)",
			params![profile, post, comment.id],
			|r| r.get::<_, bool>(0),
		)?;
		if !exists && remaining <= 0 {
			limited = true;
			continue;
		}
		if !exists {
			remaining -= 1;
		}
		// Store inert text for evidence and search; active markup is never trusted.
		let body = ammonia::Builder::new()
			.tags(std::collections::HashSet::new())
			.clean(&comment.body.replace("</p>", "\n\n").replace("<br>", "\n"))
			.to_string();
		let body = htmlescape::decode_html(&body).unwrap_or(body).chars().take(8192).collect::<String>();
		tx.execute(
			"INSERT INTO watch_comments(profile_id,post_id,comment_id,parent,ancestors,author,body,created,first_seen) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
		 ON CONFLICT(profile_id,post_id,comment_id) DO UPDATE SET parent=excluded.parent,ancestors=excluded.ancestors,author=excluded.author,body=excluded.body",
			params![
				profile,
				post,
				comment.id,
				comment.parent,
				serde_json::to_string(&comment.ancestors).unwrap(),
				comment.author,
				body,
				comment.created,
				now
			],
		)?;
	}
	tx.execute("INSERT INTO watch_state(profile_id,post_id,checked,complete,error) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(profile_id,post_id) DO UPDATE SET checked=excluded.checked,complete=excluded.complete,error=excluded.error",params![profile,post,now,complete&&!limited,if limited {"Observation capacity reached; some replies were not retained."} else {""}])?;
	if !initialized {
		acknowledge(&tx, profile, post)?;
	}
	tx.commit()?;
	Ok(())
}

pub fn from_thread(thread: &ThreadModel) -> Vec<ObservedComment> {
	thread
		.observed_comments()
		.filter(|c| c.filter_state == crate::thread::CommentFilterState::Visible)
		.take(2000)
		.map(|c| ObservedComment {
			id: c.id.clone(),
			parent: c.parent_id.trim_start_matches("t1_").trim_start_matches("t3_").into(),
			ancestors: c
				.ancestor_path
				.iter()
				.filter(|a| !a.starts_with("t3_"))
				.map(|a| a.trim_start_matches("t1_").to_string())
				.collect(),
			author: c.author.name.clone(),
			body: c.body.chars().take(8192).collect(),
			created: c.created_ts,
			first_seen: 0,
		})
		.collect()
}

pub fn observe_request(req: &Request<Body>, post: &str, thread: &ThreadModel) -> Result<(), String> {
	let Some(profile) = account::context(req).map(|c| c.profile_id) else { return Ok(()) };
	observe(
		&mut account::open_database()?,
		profile,
		post,
		&from_thread(thread),
		thread.summary().complete && thread.summary().comment_count <= 2000,
		account::now(),
	)
	.map_err(|e| format!("Unable to observe followed discussion: {e:?}"))
}

fn claim(db: &mut Connection, profile: i64, post: &str, now: i64) -> Result<Option<(Preferences, String)>, WriteError> {
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let row=tx.query_row("SELECT p.preferences_json,r.community FROM reading_entries r JOIN profiles p ON p.id=r.profile_id LEFT JOIN users u ON u.id=p.user_id WHERE r.profile_id=?1 AND r.post_id=?2 AND r.followed=1 AND u.disabled_at IS NULL",params![profile,post],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
	let Some((prefs, community)) = row else { return Ok(None) };
	let prefs = serde_json::from_str::<Preferences>(&prefs).map_err(|_| WriteError::Invalid)?;
	if !refresh_due(&load_state(&tx, profile, post)?, now) {
		return Ok(None);
	}
	tx.execute("INSERT INTO watch_state(profile_id,post_id,checked,error) VALUES(?1,?2,?3,'Awaiting first capture') ON CONFLICT(profile_id,post_id) DO UPDATE SET checked=excluded.checked",params![profile,post,now])?;
	tx.commit()?;
	Ok(Some((prefs, community)))
}

fn due_watches(db: &Connection, now: i64) -> Result<Vec<(i64, String)>, WriteError> {
	let mut stmt=db.prepare("SELECT r.profile_id,r.post_id FROM reading_entries r JOIN profiles p ON p.id=r.profile_id LEFT JOIN users u ON u.id=p.user_id LEFT JOIN watch_state w ON w.profile_id=r.profile_id AND w.post_id=r.post_id WHERE r.followed=1 AND u.disabled_at IS NULL AND (coalesce(w.checked,0)=0 OR w.checked<=?1) AND coalesce(w.snoozed_until,0)<=?2 ORDER BY coalesce(w.checked,0),r.post_id LIMIT 4")?;
	let rows = stmt.query_map(params![now.saturating_sub(POLL_SECONDS), now], |r| Ok((r.get(0)?, r.get(1)?)))?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// One request per discussion per 15 minutes; claim before network work also
/// bounds failures and competing manual/background refreshes.
pub async fn refresh(profile: i64, post: &str) -> Result<(), String> {
	let Some((prefs, community)) = claim(&mut account::open_database()?, profile, post, account::now()).map_err(|e| format!("{e:?}"))? else {
		return Ok(());
	};
	let path = format!("/r/{community}/comments/{post}.json?raw_json=1&limit=100&sort=new");
	match tokio::time::timeout(std::time::Duration::from_secs(30), crate::client::json(path, false)).await {
		Ok(Ok(json)) => {
			let data = &json[0]["data"]["children"][0]["data"];
			if data["id"].as_str() != Some(post) {
				return record_error(profile, post, "Upstream returned a different discussion.");
			}
			if data["over_18"].as_bool().unwrap_or(false) && (prefs.show_nsfw != "on" || crate::utils::sfw_only()) {
				return record_error(profile, post, "Refresh paused by content preferences.");
			}
			let filters = prefs.filters.iter().cloned().collect();
			let thread = ThreadModel::from_listing(
				&json[1],
				post,
				data["num_comments"].as_u64().unwrap_or(0) as usize,
				&format!("/comments/{post}/"),
				data["author"].as_str().unwrap_or(""),
				"",
				&filters,
				&prefs.comment_keywords(),
				&prefs,
			);
			let mut db = account::open_database()?;
			observe(
				&mut db,
				profile,
				post,
				&from_thread(&thread),
				thread.summary().complete && thread.summary().comment_count <= 2000,
				account::now(),
			)
			.map_err(|e| format!("{e:?}"))?;
			Ok(())
		}
		_ => record_error(profile, post, "Reddit is unavailable. Retained observations remain readable."),
	}
}

fn record_error(profile: i64, post: &str, message: &str) -> Result<(), String> {
	account::open_database()?
		.execute(
			"UPDATE watch_state SET error=?3,complete=0 WHERE profile_id=?1 AND post_id=?2",
			params![profile, post, message],
		)
		.map_err(|e| e.to_string())?;
	Ok(())
}

pub async fn worker() {
	if account::mode() == account::ProfileMode::Browser {
		return;
	}
	loop {
		tokio::time::sleep(std::time::Duration::from_secs(60)).await;
		let candidates = account::open_database().map_err(WriteError::Database).and_then(|db| due_watches(&db, account::now()));
		if let Ok(candidates) = candidates {
			for (profile, post) in candidates {
				let _ = refresh(profile, &post).await;
			}
		}
	}
}

#[derive(Clone)]
struct Branch {
	id: String,
	author: String,
	excerpt: String,
}
fn visible_changes(db: &Connection, profile: i64, post: &str, baseline: i64, prefs: &Preferences, offset: i64) -> Result<(Vec<ObservedComment>, i64), WriteError> {
	if !(0..=MAX_COMMENTS).contains(&offset) {
		return Err(WriteError::Invalid);
	}
	let mut visible = Vec::new();
	let mut count = 0;
	let keywords = prefs.comment_keywords();
	for start in (0..MAX_COMMENTS).step_by(200) {
		let batch = changes_page(db, profile, post, baseline, start)?;
		let length = batch.len();
		for c in batch {
			if prefs.filters.contains(&format!("u_{}", c.author)) || crate::utils::comment_matches_keywords(&c.body, &keywords) {
				continue;
			}
			if count >= offset && visible.len() < 200 {
				visible.push(c)
			}
			count += 1;
		}
		if length < 200 {
			break;
		}
	}
	Ok((visible, count))
}

#[derive(Template)]
#[template(path = "watch.html")]
struct WatchTemplate {
	next: String,
	previous: String,
	prefs: Preferences,
	url: String,
	entry: crate::reading::ReadingEntry,
	state: WatchState,
	comments: Vec<ObservedComment>,
	branches: Vec<Branch>,
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to read followed discussions."));
	};
	let post = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
		.find_map(|(k, v)| (k == "post").then(|| v.into_owned()))
		.unwrap_or_default();
	let db = account::open_database()?;
	let entry = crate::reading::get(&db, profile, &post).map_err(|e| format!("{e:?}"))?;
	if entry.revision == 0 {
		return Ok(reply(StatusCode::NOT_FOUND, "Discussion not found in this profile."));
	}
	let mut state = state(&db, profile, &post).map_err(|e| format!("{e:?}"))?;
	let offset = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
		.find_map(|(k, v)| (k == "offset").then(|| v.parse::<i64>().ok()).flatten())
		.unwrap_or(0);
	if !(0..=MAX_COMMENTS).contains(&offset) {
		return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Invalid reply page."));
	}
	let prefs = Preferences::new(&req);
	let (comments, count) = if state.baseline < 0 || prefs.filters.contains(&entry.community) {
		(vec![], 0)
	} else {
		visible_changes(&db, profile, &post, state.baseline, &prefs, offset).map_err(|e| format!("{e:?}"))?
	};
	state.new_count = count;
	let next = if offset + 200 < state.new_count {
		format!("/reading/watch?post={post}&offset={}", offset + 200)
	} else {
		String::new()
	};
	let previous = if offset > 0 {
		format!("/reading/watch?post={post}&offset={}", offset.saturating_sub(200).max(0))
	} else {
		String::new()
	};
	let mut stmt = db
		.prepare("SELECT b.comment_id,c.author,c.body FROM watch_branches b LEFT JOIN watch_comments c ON c.profile_id=b.profile_id AND c.post_id=b.post_id AND c.comment_id=b.comment_id WHERE b.profile_id=?1 AND b.post_id=?2 ORDER BY b.comment_id")
		.map_err(|e| e.to_string())?;
	let mut branches: Vec<Branch> = stmt
		.query_map(params![profile, post], |r| {
			Ok(Branch {
				id: r.get(0)?,
				author: r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "unknown".into()),
				excerpt: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
			})
		})
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	for branch in &mut branches {
		if prefs.filters.contains(&format!("u_{}", branch.author)) || crate::utils::comment_matches_keywords(&branch.excerpt, &prefs.comment_keywords()) {
			branch.excerpt = "Hidden by your current filters".into();
		} else {
			branch.excerpt = branch.excerpt.chars().take(100).collect();
		}
	}
	Ok(template(&WatchTemplate {
		next,
		previous,
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		entry,
		state,
		comments,
		branches,
	}))
}

pub fn control(db: &mut Connection, profile: i64, post: &str, action: &str, branch: &str, now: i64) -> Result<(), WriteError> {
	if now < 0 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if !crate::reading::get(&tx, profile, post)?.followed {
		return Err(WriteError::Invalid);
	}
	match action {
		"snooze" | "resume" => {
			let until = if action == "snooze" { now.checked_add(86400).ok_or(WriteError::Invalid)? } else { 0 };
			tx.execute(
				"INSERT INTO watch_state(profile_id,post_id,snoozed_until) VALUES(?1,?2,?3) ON CONFLICT(profile_id,post_id) DO UPDATE SET snoozed_until=excluded.snoozed_until",
				params![profile, post, until],
			)?;
		}
		"branch" => {
			if !account::valid_post_id(branch) {
				return Err(WriteError::Invalid);
			}
			let known = tx.query_row(
				"SELECT EXISTS(SELECT 1 FROM watch_comments WHERE profile_id=?1 AND post_id=?2 AND comment_id=?3)",
				params![profile, post, branch],
				|r| r.get::<_, bool>(0),
			)?;
			if !known {
				return Err(WriteError::Invalid);
			}
			let existing = tx.query_row(
				"SELECT EXISTS(SELECT 1 FROM watch_branches WHERE profile_id=?1 AND post_id=?2 AND comment_id=?3)",
				params![profile, post, branch],
				|r| r.get::<_, bool>(0),
			)?;
			let count = tx.query_row("SELECT count(*) FROM watch_branches WHERE profile_id=?1 AND post_id=?2", params![profile, post], |r| {
				r.get::<_, i64>(0)
			})?;
			if !existing && count >= 32 {
				return Err(WriteError::Full);
			}
			tx.execute(
				"INSERT OR IGNORE INTO watch_branches(profile_id,post_id,comment_id) VALUES(?1,?2,?3)",
				params![profile, post, branch],
			)?;
		}
		"unbranch" => {
			if !account::valid_post_id(branch) {
				return Err(WriteError::Invalid);
			}
			tx.execute(
				"DELETE FROM watch_branches WHERE profile_id=?1 AND post_id=?2 AND comment_id=?3",
				params![profile, post, branch],
			)?;
		}
		"whole" => {
			tx.execute("DELETE FROM watch_branches WHERE profile_id=?1 AND post_id=?2", params![profile, post])?;
		}
		_ => return Err(WriteError::Invalid),
	}
	tx.commit()?;
	Ok(())
}

pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to manage follows."));
	};
	let bytes = crate::utils::read_body_limited(req.body_mut(), 2048, "Watch command is too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	let post = v("post");
	let db = account::open_database()?;
	let entry = crate::reading::get(&db, profile, post).map_err(|e| format!("{e:?}"))?;
	if !entry.followed {
		return Ok(reply(StatusCode::NOT_FOUND, "Follow this discussion first."));
	}
	if v("action") == "refresh" {
		drop(db);
		refresh(profile, post).await?;
	} else {
		let mut db = db;
		match control(&mut db, profile, post, v("action"), v("branch"), account::now()) {
			Ok(()) => {}
			Err(WriteError::Database(e)) => return Err(e),
			Err(WriteError::Full) => return Ok(reply(StatusCode::CONFLICT, "At most 32 branches may be followed in a discussion.")),
			Err(_) => {
				return Ok(reply(
					StatusCode::UNPROCESSABLE_ENTITY,
					"Invalid watch command or unknown branch. Open the branch in the discussion first.",
				))
			}
		}
	}
	Ok(
		Response::builder()
			.status(303)
			.header("location", format!("/reading/watch?post={post}"))
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
		let mut db = Connection::open_in_memory().unwrap();
		db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
		crate::account::initialize_schema(&db).unwrap();
		let prefs = serde_json::to_string(&Preferences::default()).unwrap();
		for profile in [1, 2] {
			db.execute(
				"INSERT INTO profiles(id,label,preferences_json,created_at,updated_at) VALUES(?1,'Test',?2,1,1)",
				params![profile, prefs],
			)
			.unwrap();
		}
		for profile in [1, 2] {
			crate::reading::command(&mut db, profile, "post1", "Discussion", "rust", 0, "follow", "", 1).unwrap();
		}
		db
	}
	fn comment(id: &str) -> ObservedComment {
		ObservedComment {
			id: id.into(),
			parent: "post1".into(),
			ancestors: vec![],
			author: "reader".into(),
			body: "A useful reply".into(),
			created: 1,
			first_seen: 999,
		}
	}

	#[test]
	fn cached_filter_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			observe(&mut db, 1, "post1", &[], false, 100).unwrap();
			let mut a = comment("a");
			let mut b = comment("b");
			b.author = "other".into();
			b.body = "A needle in the discussion".into();
			let mut prefs = Preferences::default();
			let mut offset = 0;
			let mut profile = 1;
			match case {
				0 => {}
				1 => prefs.filters = vec!["u_reader".into()],
				2 => prefs.filters = vec!["u_other".into()],
				3 => prefs.comment_filter_keywords = "needle".into(),
				4 => prefs.comment_filter_keywords = "USEFUL,needle".into(),
				5 => {
					a.body = "Café evidence".into();
					prefs.comment_filter_keywords = "café".into();
				}
				6 => offset = 1,
				7 => offset = 2,
				8 => offset = -1,
				9 => profile = 2,
				10 => {
					prefs.filters = vec!["u_reader".into()];
					prefs.comment_filter_keywords = "needle".into();
				}
				11 => prefs.comment_filter_keywords = "missing".into(),
				_ => {}
			}
			observe(&mut db, 1, "post1", &[a, b], false, 200).unwrap();
			let result = visible_changes(&db, profile, "post1", 0, &prefs, offset);
			if case == 8 {
				assert!(result.is_err());
				continue;
			}
			let (items, count) = result.unwrap();
			let expected = match case {
				1 | 2 | 3 | 5 => 1,
				4 | 9 | 10 => 0,
				_ => 2,
			};
			assert_eq!(count, expected, "case {case}");
			assert_eq!(items.len(), (expected - offset).max(0) as usize, "case {case}");
		}
	}
	fn control_edges(action: &str) {
		for case in 0..12 {
			let mut db = db();
			observe(&mut db, 1, "post1", &[comment("root")], false, 100).unwrap();
			match case {
				0 => control(&mut db, 1, "post1", action, "root", 200).unwrap(),
				1 => {
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
				}
				2 => assert_eq!(control(&mut db, 1, "post1", action, "root", -1), Err(WriteError::Invalid)),
				3 => assert_eq!(control(&mut db, 1, "missing", action, "root", 200), Err(WriteError::Invalid)),
				4 => assert_eq!(control(&mut db, 999, "post1", action, "root", 200), Err(WriteError::Invalid)),
				5 => {
					crate::reading::command(&mut db, 1, "post1", "Discussion", "rust", 1, "unfollow", "", 2).unwrap();
					assert_eq!(control(&mut db, 1, "post1", action, "root", 200), Err(WriteError::Invalid));
				}
				6 => {
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
					assert_eq!(state(&db, 1, "post1").unwrap().checked, 100);
				}
				7 => {
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
					assert_eq!(state(&db, 1, "post1").unwrap().new_count, 0);
				}
				8 => {
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
					assert_eq!(state(&db, 2, "post1").unwrap().snoozed_until, 0);
				}
				9 => {
					control(&mut db, 1, "post1", action, "root", 0).unwrap();
					assert_eq!(changes(&db, 1, "post1", 0).unwrap().len(), 1);
				}
				10 => {
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
					initialize(&db).unwrap();
					assert_eq!(changes(&db, 1, "post1", 0).unwrap().len(), 1);
				}
				11 => {
					control(&mut db, 1, "post1", action, "root", 200).unwrap();
					let saved = crate::reading::get(&db, 1, "post1").unwrap();
					assert!(saved.followed);
					assert_eq!(saved.caught_up_at, 0);
				}
				_ => unreachable!(),
			}
		}
	}
	#[test]
	fn snooze_twelve_edges() {
		control_edges("snooze");
	}
	#[test]
	fn resume_twelve_edges() {
		control_edges("resume");
	}
	#[test]
	fn follow_branch_twelve_edges() {
		control_edges("branch");
	}
	#[test]
	fn stop_branch_twelve_edges() {
		control_edges("unbranch");
	}
	#[test]
	fn follow_whole_twelve_edges() {
		control_edges("whole");
	}

	#[test]
	fn refresh_budget_twelve_edges() {
		for (checked, snoozed, now, expected) in [
			(0, 0, 100, true),
			(100, 0, 999, false),
			(100, 0, 1000, true),
			(100, 0, 1001, true),
			(100, 1001, 1000, false),
			(100, 1000, 1000, true),
			(2000, 0, 1000, false),
			(0, 0, -1, false),
			(100, 0, 100, false),
			(0, 2000, 1000, false),
			(i64::MAX, 0, 1000, false),
			(100, 0, i64::MAX, true),
		] {
			assert_eq!(
				refresh_due(
					&WatchState {
						checked,
						snoozed_until: snoozed,
						..Default::default()
					},
					now
				),
				expected
			);
		}
	}

	#[test]
	fn refresh_claim_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			match case {
				0 => assert!(claim(&mut db, 1, "post1", 1000).unwrap().is_some()),
				1 => {
					claim(&mut db, 1, "post1", 1000).unwrap();
					assert!(claim(&mut db, 1, "post1", 1001).unwrap().is_none());
				}
				2 => {
					claim(&mut db, 1, "post1", 1000).unwrap();
					assert!(claim(&mut db, 1, "post1", 1900).unwrap().is_some());
				}
				3 => assert!(claim(&mut db, 9, "post1", 1000).unwrap().is_none()),
				4 => assert!(claim(&mut db, 1, "missing", 1000).unwrap().is_none()),
				5 => {
					control(&mut db, 1, "post1", "snooze", "", 1000).unwrap();
					assert!(claim(&mut db, 1, "post1", 1001).unwrap().is_none());
				}
				6 => {
					control(&mut db, 1, "post1", "snooze", "", 1000).unwrap();
					control(&mut db, 1, "post1", "resume", "", 1001).unwrap();
					assert!(claim(&mut db, 1, "post1", 1001).unwrap().is_some());
				}
				7 => {
					db.execute("UPDATE reading_entries SET followed=0 WHERE profile_id=1", []).unwrap();
					assert!(claim(&mut db, 1, "post1", 1000).unwrap().is_none());
				}
				8 => {
					db.execute(
						"INSERT INTO users(id,username,display_name,password_hash,disabled_at,created_at,updated_at) VALUES(1,'disabled','Disabled','unused-test-hash',1,1,1)",
						[],
					)
					.unwrap();
					db.execute("UPDATE profiles SET user_id=1 WHERE id=1", []).unwrap();
					assert!(claim(&mut db, 1, "post1", 1000).unwrap().is_none());
				}
				9 => {
					db.execute("UPDATE profiles SET preferences_json='invalid' WHERE id=1", []).unwrap();
					assert!(matches!(claim(&mut db, 1, "post1", 1000), Err(WriteError::Invalid)));
				}
				10 => assert!(claim(&mut db, 1, "post1", -1).unwrap().is_none()),
				11 => {
					claim(&mut db, 1, "post1", 1000).unwrap();
					assert!(claim(&mut db, 2, "post1", 1001).unwrap().is_some());
				}
				_ => unreachable!(),
			}
		}
	}
	#[test]
	fn scheduler_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			match case {
				0 => assert_eq!(due_watches(&db, 1000).unwrap().len(), 2),
				1 => {
					claim(&mut db, 1, "post1", 1000).unwrap();
					assert_eq!(due_watches(&db, 1001).unwrap(), vec![(2, "post1".into())]);
				}
				2 => {
					claim(&mut db, 1, "post1", 1000).unwrap();
					assert_eq!(due_watches(&db, 1900).unwrap().len(), 2);
				}
				3 => {
					control(&mut db, 1, "post1", "snooze", "", 1000).unwrap();
					assert_eq!(due_watches(&db, 1001).unwrap().len(), 1);
				}
				4 => {
					control(&mut db, 1, "post1", "snooze", "", 1000).unwrap();
					assert_eq!(due_watches(&db, 87400).unwrap().len(), 2);
				}
				5 => {
					db.execute("UPDATE reading_entries SET followed=0", []).unwrap();
					assert!(due_watches(&db, 1000).unwrap().is_empty());
				}
				6 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
					assert_eq!(due_watches(&db, 1000).unwrap().len(), 1);
				}
				7 => {
					db.execute(
						"INSERT INTO users(id,username,display_name,password_hash,disabled_at,created_at,updated_at) VALUES(1,'disabled','Disabled','unused-test-hash',1,1,1)",
						[],
					)
					.unwrap();
					db.execute("UPDATE profiles SET user_id=1 WHERE id=1", []).unwrap();
					assert_eq!(due_watches(&db, 1000).unwrap().len(), 1);
				}
				8 => {
					for id in 0..8 {
						crate::reading::command(&mut db, 1, &format!("post{id}x"), "Title", "rust", 0, "follow", "", 100).unwrap();
					}
					assert_eq!(due_watches(&db, 1000).unwrap().len(), 4);
				}
				9 => {
					claim(&mut db, 1, "post1", 2000).unwrap();
					assert_eq!(due_watches(&db, 1000).unwrap().len(), 1);
				}
				10 => {
					claim(&mut db, 1, "post1", 1000).unwrap();
					assert_eq!(due_watches(&db, 1899).unwrap().len(), 1);
				}
				11 => {
					initialize(&db).unwrap();
					assert_eq!(due_watches(&db, 1000).unwrap().len(), 2);
				}
				_ => unreachable!(),
			}
		}
	}
	#[test]
	fn reply_pagination_twelve_edges() {
		let mut db = db();
		observe(&mut db, 1, "post1", &[], true, 100).unwrap();
		let comments = (0..405).map(|n| comment(&format!("c{n}"))).collect::<Vec<_>>();
		observe(&mut db, 1, "post1", &comments, false, 200).unwrap();
		for (offset, length) in [
			(0, 200),
			(1, 200),
			(199, 200),
			(200, 200),
			(201, 200),
			(204, 200),
			(205, 200),
			(399, 6),
			(400, 5),
			(404, 1),
			(405, 0),
			(10000, 0),
		] {
			assert_eq!(changes_page(&db, 1, "post1", 0, offset).unwrap().len(), length, "offset {offset}");
		}
		assert_eq!(state(&db, 1, "post1").unwrap().new_count, 405);
		assert!(matches!(changes_page(&db, 1, "post1", 0, -1), Err(WriteError::Invalid)));
	}

	#[test]
	fn observation_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			match case {
				0 => {
					observe(&mut db, 1, "post1", &[], true, 100).unwrap();
					let x = state(&db, 1, "post1").unwrap();
					assert_eq!(x.new_count, 0);
					assert!(x.complete);
				}
				1 => {
					observe(&mut db, 1, "post1", &[comment("a")], true, 100).unwrap();
					assert_eq!(state(&db, 1, "post1").unwrap().new_count, 0);
				}
				2 => {
					observe(&mut db, 1, "post1", &[], true, 100).unwrap();
					observe(&mut db, 1, "post1", &[comment("a")], true, 200).unwrap();
					assert_eq!(state(&db, 1, "post1").unwrap().new_count, 1, "old creation timestamp, newly observed identity");
				}
				3 => {
					observe(&mut db, 1, "post1", &[comment("a"), comment("a")], true, 100).unwrap();
					observe(&mut db, 1, "post1", &[comment("a")], true, 200).unwrap();
					assert_eq!(changes(&db, 1, "post1", 0).unwrap().len(), 1);
				}
				4 => {
					observe(&mut db, 1, "post1", &[comment("a")], true, 100).unwrap();
					let mut edited = comment("a");
					edited.body = "Correction".into();
					observe(&mut db, 1, "post1", &[edited], true, 200).unwrap();
					assert_eq!(state(&db, 1, "post1").unwrap().new_count, 0);
					assert_eq!(changes(&db, 1, "post1", 0).unwrap()[0].body, "Correction");
				}
				5 => {
					observe(&mut db, 1, "post1", &[comment("a")], true, 100).unwrap();
					observe(&mut db, 1, "post1", &[], false, 200).unwrap();
					assert_eq!(changes(&db, 1, "post1", 0).unwrap().len(), 1);
					assert!(!state(&db, 1, "post1").unwrap().complete);
				}
				6 => {
					let mut bad = comment("../a");
					bad.body = "unsafe".into();
					assert_eq!(observe(&mut db, 1, "post1", &[bad], true, 100), Err(WriteError::Invalid));
				}
				7 => {
					observe(&mut db, 1, "post1", &[comment("a")], true, 100).unwrap();
					assert!(changes(&db, 2, "post1", 0).unwrap().is_empty());
				}
				8 => {
					crate::reading::command(&mut db, 1, "post1", "Discussion", "rust", 1, "unfollow", "", 2).unwrap();
					observe(&mut db, 1, "post1", &[comment("a")], true, 100).unwrap();
					assert!(changes(&db, 1, "post1", 0).unwrap().is_empty());
				}
				9 => {
					observe(&mut db, 1, "post1", &[], false, 100).unwrap();
					observe(&mut db, 1, "post1", &[comment("a")], true, 200).unwrap();
					acknowledge(&db, 1, "post1").unwrap();
					assert_eq!(state(&db, 1, "post1").unwrap().new_count, 0);
				}
				10 => {
					let mut x = comment("a");
					x.body = "<script>bad()</script><b>Useful</b> &amp; safe".into();
					observe(&mut db, 1, "post1", &[x], true, 100).unwrap();
					let saved = changes(&db, 1, "post1", 0).unwrap().remove(0);
					assert_eq!(saved.body, "Useful & safe");
					assert_eq!(saved.first_seen, 100);
				}
				11 => {
					db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO watch_comments(profile_id,post_id,comment_id,parent,ancestors,author,body,created,first_seen) SELECT 1,'post1','p'||x,'post1','[]','a','b',1,1 FROM n;").unwrap();
					observe(&mut db, 1, "post1", &[comment("new")], true, 100).unwrap();
					let x = state(&db, 1, "post1").unwrap();
					assert!(!x.complete);
					assert!(!x.error.is_empty());
				}
				_ => unreachable!(),
			}
		}
	}
	#[test]
	fn branch_matching_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			observe(&mut db, 1, "post1", &[], true, 100).unwrap();
			let mut x = comment("reply");
			x.parent = "root".into();
			x.ancestors = vec!["root".into(), "middle".into()];
			observe(&mut db, 1, "post1", &[x], false, 200).unwrap();
			let (branch, expected) = match case {
				0 => ("root", 1),
				1 => ("middle", 1),
				2 => ("reply", 1),
				3 => ("roo", 0),
				4 => ("other", 0),
				5 => ("ROOT", 0),
				6 => ("rootmore", 0),
				7 => ("mid", 0),
				8 => ("post1", 0),
				9 => ("missing", 0),
				10 => ("repl", 0),
				_ => ("middle", 1),
			};
			db.execute("INSERT INTO watch_branches VALUES(1,'post1',?1)", [branch]).unwrap();
			assert_eq!(state(&db, 1, "post1").unwrap().new_count, expected, "branch case {case}");
		}
	}
	#[test]
	fn initial_claim_and_failure_do_not_turn_first_capture_into_unread() {
		let mut db = db();
		db.execute("INSERT INTO watch_state(profile_id,post_id,checked,error) VALUES(1,'post1',100,'Unavailable')", [])
			.unwrap();
		observe(&mut db, 1, "post1", &[comment("a")], false, 200).unwrap();
		assert_eq!(state(&db, 1, "post1").unwrap().new_count, 0);
	}
	#[test]
	fn forgetting_cascades_only_own_observations() {
		let mut db = db();
		for profile in [1, 2] {
			observe(&mut db, profile, "post1", &[comment("a")], true, 100).unwrap();
		}
		crate::reading::command(&mut db, 1, "post1", "Discussion", "rust", 1, "forget", "", 200).unwrap();
		assert!(changes(&db, 1, "post1", 0).unwrap().is_empty());
		assert_eq!(changes(&db, 2, "post1", 0).unwrap().len(), 1);
	}
}
