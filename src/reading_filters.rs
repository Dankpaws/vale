//! Explicit, bounded, profile-owned post filters. No regex or implicit feed mixing.
use crate::{
	account,
	reading::WriteError,
	utils::{template, FeedGroup, Post, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

#[derive(Clone, Debug)]
pub struct Rule {
	pub id: i64,
	pub feed: String,
	pub kind: String,
	pub value: String,
	pub until: i64,
	pub effect: String,
}

impl Rule {
	pub fn matches(&self, domain: &str, flair: &str, title: &str, kind: &str) -> bool {
		match self.kind.as_str() {
			"domain" => domain.trim_end_matches('.').eq_ignore_ascii_case(&self.value) || domain.to_ascii_lowercase().trim_end_matches('.').ends_with(&format!(".{}", self.value)),
			"flair" => flair.trim().to_lowercase() == self.value,
			"phrase" => title.to_lowercase().contains(&self.value),
			"type" => kind == self.value,
			"episode" => episode_hidden(&self.value, title, flair),
			_ => false,
		}
	}
	pub fn hides(&self, post: &Post) -> bool {
		let matches = if self.kind == "spoiler" {
			post.flags.spoiler
		} else {
			self.matches(&post.domain, &post.flair.text, &post.title, &post.post_type)
		};
		if self.effect == "only" {
			!matches
		} else {
			matches
		}
	}
}

pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch(
		"CREATE TABLE IF NOT EXISTS reading_filters (
	 id INTEGER PRIMARY KEY, profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
	 feed TEXT NOT NULL, kind TEXT NOT NULL, value TEXT NOT NULL, until_at INTEGER NOT NULL, effect TEXT NOT NULL,
	 UNIQUE(profile_id,feed,kind,value,effect));
 CREATE TABLE IF NOT EXISTS reading_filter_clock(id INTEGER PRIMARY KEY CHECK(id=1),value INTEGER NOT NULL);
 INSERT INTO reading_filter_clock(id,value) SELECT 1,coalesce(max(id),0) FROM reading_filters WHERE 1 ON CONFLICT(id) DO UPDATE SET value=max(reading_filter_clock.value,excluded.value);",
	)
}

// Keep the explicit transaction fields together at this persistence boundary.
#[allow(clippy::too_many_arguments)]
pub fn add(db: &mut Connection, profile: i64, feeds: &[FeedGroup], feed: &str, kind: &str, value: &str, duration: i64, effect: &str, now: i64) -> Result<(), WriteError> {
	let value = value.trim().to_lowercase();
	if (!feed.is_empty() && !feeds.iter().any(|f| f.slug == feed))
		|| value.is_empty()
		|| value.chars().count() > 160
		|| !matches!(effect, "hide" | "only")
		|| !matches!(duration, 0 | 86400 | 604800 | 2592000)
		|| now < 0
	{
		return Err(WriteError::Invalid);
	}
	match kind {
		"domain"
			if value.contains('.')
				&& value.len() <= 253
				&& value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
				&& !value.starts_with('.')
				&& !value.ends_with('.')
				&& !value.contains("..") => {}
		"flair" | "phrase" => {}
		"spoiler" if value == "tagged" && effect == "hide" => {}
		"episode" if episode_boundary(&value).is_some() && effect == "hide" => {}
		"type" if matches!(value.as_str(), "self" | "link" | "image" | "video" | "gif" | "gallery") => {}
		_ => return Err(WriteError::Invalid),
	}
	let until = if duration == 0 { 0 } else { now.checked_add(duration).ok_or(WriteError::Invalid)? };
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let exists = tx.query_row(
		"SELECT EXISTS(SELECT 1 FROM reading_filters WHERE profile_id=?1 AND feed=?2 AND kind=?3 AND value=?4 AND effect=?5)",
		params![profile, feed, kind, value, effect],
		|r| r.get::<_, bool>(0),
	)?;
	if !exists && tx.query_row("SELECT count(*) FROM reading_filters WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 200 {
		return Err(WriteError::Full);
	}
	let existing_id: Option<i64> = tx
		.query_row(
			"SELECT id FROM reading_filters WHERE profile_id=?1 AND feed=?2 AND kind=?3 AND value=?4 AND effect=?5",
			params![profile, feed, kind, value, effect],
			|r| r.get(0),
		)
		.optional()?;
	let id = match existing_id {
		Some(id) => id,
		None => tx.query_row("UPDATE reading_filter_clock SET value=value+1 WHERE id=1 RETURNING value", [], |r| r.get::<_, i64>(0))?,
	};
	tx.execute("INSERT INTO reading_filters(id,profile_id,feed,kind,value,until_at,effect) VALUES(?7,?1,?2,?3,?4,?5,?6) ON CONFLICT(profile_id,feed,kind,value,effect) DO UPDATE SET until_at=excluded.until_at",params![profile,feed,kind,value,until,effect,id])?;
	tx.commit()?;
	Ok(())
}

pub fn list(db: &Connection, profile: i64) -> Result<Vec<Rule>, WriteError> {
	let mut stmt = db.prepare("SELECT id,feed,kind,value,until_at,effect FROM reading_filters WHERE profile_id=?1 ORDER BY id DESC")?;
	let rows = stmt.query_map([profile], |r| {
		Ok(Rule {
			id: r.get(0)?,
			feed: r.get(1)?,
			kind: r.get(2)?,
			value: r.get(3)?,
			until: r.get(4)?,
			effect: r.get(5)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn for_listing(req: &Request<Body>, communities: Option<&[String]>) -> Result<Vec<Rule>, String> {
	if url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).any(|(k, v)| k == "vale_filters" && v == "show") {
		return Ok(vec![]);
	}
	let Some(context) = account::context(req) else { return Ok(vec![]) };
	let prefs = Preferences::new(req);
	let feed = communities
		.and_then(|members| {
			prefs
				.feed_groups()
				.into_iter()
				.find(|f| f.communities.len() == members.len() && f.communities.iter().all(|c| members.iter().any(|m| m.eq_ignore_ascii_case(c))))
		})
		.map(|f| f.slug)
		.unwrap_or_default();
	let now = account::now();
	Ok(
		list(&account::open_database()?, context.profile_id)
			.map_err(|e| format!("Unable to load filters: {e:?}"))?
			.into_iter()
			.filter(|r| (r.feed.is_empty() || r.feed == feed) && (r.until == 0 || r.until > now))
			.collect(),
	)
}

#[derive(Template)]
#[template(path = "reading_filters.html")]
struct FilterTemplate {
	prefs: Preferences,
	url: String,
	rules: Vec<Rule>,
	feeds: Vec<FeedGroup>,
	now: i64,
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(context) = account::context(&req) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to manage filters."));
	};
	let prefs = Preferences::new(&req);
	let feeds = prefs.feed_groups();
	let rules = list(&account::open_database()?, context.profile_id).map_err(|e| format!("Unable to load filters: {e:?}"))?;
	Ok(template(&FilterTemplate {
		prefs,
		url: req.uri().to_string(),
		rules,
		feeds,
		now: account::now(),
	}))
}

pub fn remove(db: &Connection, profile: i64, id: i64, until: i64) -> Result<(), WriteError> {
	let changed = db.execute("DELETE FROM reading_filters WHERE profile_id=?1 AND id=?2 AND until_at=?3", params![profile, id, until])?;
	if changed == 1 {
		Ok(())
	} else {
		Err(WriteError::Conflict)
	}
}
pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to manage filters."));
	};
	let feeds = Preferences::new(&req).feed_groups();
	let bytes = crate::utils::read_body_limited(req.body_mut(), 4096, "Filter command is too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |key: &str| form.get(key).map(String::as_str).unwrap_or_default();
	let mut db = account::open_database()?;
	let result = if v("action") == "remove" {
		match (v("id").parse::<i64>(), v("until").parse::<i64>()) {
			(Ok(id), Ok(until)) => remove(&db, profile, id, until),
			_ => Err(WriteError::Invalid),
		}
	} else if v("action") == "add" {
		match v("duration").parse::<i64>() {
			Ok(duration) => add(&mut db, profile, &feeds, v("feed"), v("kind"), v("value"), duration, v("effect"), account::now()),
			Err(_) => Err(WriteError::Invalid),
		}
	} else {
		Err(WriteError::Invalid)
	};
	match result {
		Ok(()) => Ok(
			Response::builder()
				.status(303)
				.header("location", "/reading/filters")
				.header("cache-control", "private, no-store")
				.body(Body::empty())
				.unwrap(),
		),
		Err(WriteError::Database(e)) => Err(e),
		Err(WriteError::Full) => Ok(reply(StatusCode::CONFLICT, "The 200-rule limit has been reached. Remove a rule first.")),
		Err(_) => Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Invalid filter. Check the scope, value, and duration.")),
	}
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
	#[test]
	fn removal_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			add(&mut db, 1, &[], "", "phrase", "release", 0, "hide", 1000).unwrap();
			let rule = list(&db, 1).unwrap().remove(0);
			let (mut profile, mut id, mut until) = (1, rule.id, 0);
			match case {
				0 => {}
				1 => profile = 2,
				2 => id = 0,
				3 => id = -1,
				4 => until = -1,
				5 => until = 1,
				6 => {
					add(&mut db, 1, &[], "", "phrase", "release", 86400, "hide", 1000).unwrap();
				}
				7 => {
					remove(&db, 1, id, 0).unwrap();
				}
				8 => {
					remove(&db, 1, id, 0).unwrap();
					add(&mut db, 1, &[], "", "phrase", "new", 0, "hide", 1000).unwrap();
				}
				9 => {
					initialize(&db).unwrap();
				}
				10 => {
					add(&mut db, 2, &[], "", "phrase", "release", 0, "hide", 1000).unwrap();
				}
				11 => id = i64::MAX,
				_ => {}
			}
			assert_eq!(remove(&db, profile, id, until).is_ok(), matches!(case, 0 | 9 | 10), "case {case}");
		}
	}
	fn db() -> Connection {
		let db = Connection::open_in_memory().unwrap();
		db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE profiles(id INTEGER PRIMARY KEY); INSERT INTO profiles VALUES(1),(2);")
			.unwrap();
		initialize(&db).unwrap();
		db
	}
	fn rule(kind: &str, value: &str) -> Rule {
		Rule {
			id: 1,
			feed: String::new(),
			kind: kind.into(),
			value: value.into(),
			until: 0,
			effect: "hide".into(),
		}
	}
	#[test]
	fn domain_twelve_edges() {
		let r = rule("domain", "example.com");
		for (value, expected) in [
			("example.com", true),
			("EXAMPLE.COM", true),
			("www.example.com", true),
			("a.b.example.com", true),
			("example.com.", true),
			("www.example.com.", true),
			("badexample.com", false),
			("example.com.evil.org", false),
			("", false),
			("https://example.com", false),
			("example.org", false),
			("example.com:443", false),
		] {
			assert_eq!(r.matches(value, "", "", ""), expected, "{value}");
		}
	}
	#[test]
	fn phrase_twelve_edges() {
		let r = rule("phrase", "release");
		for (value, expected) in [
			("release", true),
			("RELEASE", true),
			("New release today", true),
			("prerelease", true),
			(" release ", true),
			("release\nnotes", true),
			("", false),
			("re lease", false),
			("releases", true),
			("releas", false),
			("notes", false),
			("🎉 release", true),
		] {
			assert_eq!(r.matches("", "", value, ""), expected, "{value}");
		}
	}
	#[test]
	fn flair_twelve_edges() {
		let r = rule("flair", "news");
		for (value, expected) in [
			("news", true),
			("NEWS", true),
			(" News ", true),
			("news\n", true),
			("newspaper", false),
			("old news", false),
			("", false),
			("new", false),
			("news!", false),
			("ニュース", false),
			("n e w s", false),
			("\tnews", true),
		] {
			assert_eq!(r.matches("", value, "", ""), expected, "{value}");
		}
	}
	#[test]
	fn content_type_twelve_edges() {
		let r = rule("type", "image");
		for (value, expected) in [
			("image", true),
			("IMAGE", false),
			("video", false),
			("self", false),
			("link", false),
			("gif", false),
			("gallery", false),
			("", false),
			(" image", false),
			("image ", false),
			("images", false),
			("unknown", false),
		] {
			assert_eq!(r.matches("", "", "", value), expected, "{value}");
		}
	}
	#[test]
	fn adding_rules_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			match case {
				0 => {
					add(&mut db, 1, &[], "", "phrase", " Release ", 0, "hide", 100).unwrap();
					assert_eq!(list(&db, 1).unwrap()[0].value, "release");
				}
				1 => {
					assert_eq!(add(&mut db, 1, &[], "missing", "phrase", "x", 0, "hide", 100), Err(WriteError::Invalid));
				}
				2 => {
					assert_eq!(add(&mut db, 1, &[], "", "regex", ".*", 0, "hide", 100), Err(WriteError::Invalid));
				}
				3 => {
					assert_eq!(add(&mut db, 1, &[], "", "phrase", "", 0, "hide", 100), Err(WriteError::Invalid));
				}
				4 => {
					assert_eq!(add(&mut db, 1, &[], "", "phrase", &"x".repeat(161), 0, "hide", 100), Err(WriteError::Invalid));
				}
				5 => {
					assert_eq!(add(&mut db, 1, &[], "", "domain", "https://example.com", 0, "hide", 100), Err(WriteError::Invalid));
				}
				6 => {
					assert_eq!(add(&mut db, 1, &[], "", "type", "invalid", 0, "hide", 100), Err(WriteError::Invalid));
				}
				7 => {
					assert_eq!(add(&mut db, 1, &[], "", "phrase", "x", -1, "hide", 100), Err(WriteError::Invalid));
				}
				8 => {
					assert_eq!(add(&mut db, 1, &[], "", "phrase", "x", 0, "delete", 100), Err(WriteError::Invalid));
				}
				9 => {
					add(&mut db, 1, &[], "", "phrase", "x", 86400, "hide", 100).unwrap();
					assert_eq!(list(&db, 1).unwrap()[0].until, 86500);
					assert!(list(&db, 2).unwrap().is_empty());
				}
				10 => {
					add(&mut db, 1, &[], "", "phrase", "x", 0, "hide", 100).unwrap();
					add(&mut db, 1, &[], "", "phrase", "x", 604800, "hide", 100).unwrap();
					assert_eq!(list(&db, 1).unwrap().len(), 1);
				}
				11 => {
					db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<200) INSERT INTO reading_filters(profile_id,feed,kind,value,until_at,effect) SELECT 1,'','phrase','p'||x,0,'hide' FROM n;").unwrap();
					assert_eq!(add(&mut db, 1, &[], "", "phrase", "x", 0, "hide", 100), Err(WriteError::Full));
				}
				_ => unreachable!(),
			}
		}
	}
}

// Exact SxxExx markers only. Unknown or ambiguous numbering stays hidden for
// the chosen series; no inferred episode order or guessed spoiler labels.
fn episode_boundary(value: &str) -> Option<(&str, u32, u32)> {
	let parts = value.split('|').map(str::trim).collect::<Vec<_>>();
	if parts.len() != 3 || parts[0].is_empty() {
		return None;
	}
	let season = parts[1].parse::<u32>().ok()?;
	let episode = parts[2].parse::<u32>().ok()?;
	if season > 999 || episode > 999 {
		return None;
	}
	Some((parts[0], season, episode))
}
fn episode_hidden(value: &str, title: &str, flair: &str) -> bool {
	let Some((series, season, episode)) = episode_boundary(value) else { return false };
	let text = format!("{} {}", title.to_lowercase(), flair.to_lowercase());
	if !text.contains(series) {
		return false;
	}
	static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\bs([0-9]{1,3})e([0-9]{1,3})\b").unwrap());
	let mut found = false;
	for c in MARKER.captures_iter(&text) {
		found = true;
		let s = c[1].parse::<u32>().unwrap();
		let e = c[2].parse::<u32>().unwrap();
		if (s, e) > (season, episode) {
			return true;
		}
	}
	!found
}
#[cfg(test)]
mod episode_tests {
	use super::*;
	#[test]
	fn twelve_spoiler_boundaries() {
		for (title, flair, hidden) in [
			("Show S01E01", "", false),
			("Show S01E02", "", false),
			("Show S01E03", "", true),
			("Show S02E01", "", true),
			("Other S02E01", "", false),
			("Show discussion", "", true),
			("SHOW s01e01", "", false),
			("Show S01E01 S01E03", "", true),
			("Show", "S01E02", false),
			("Show Episode 1", "", true),
			("Show S001E002", "", false),
			("Show S01E2000", "", true),
		] {
			assert_eq!(episode_hidden("show|1|2", title, flair), hidden, "{title}")
		}
	}
	#[test]
	fn twelve_boundary_validation() {
		for (value, ok) in [
			("show|1|2", true),
			("show|0|0", true),
			("show|999|999", true),
			("show|1000|1", false),
			("show|1|1000", false),
			("|1|1", false),
			("show|x|1", false),
			("show|1|x", false),
			("show|1", false),
			("show|-1|1", false),
			("show|1|2|3", false),
			(" show | 1 | 2 ", true),
		] {
			assert_eq!(episode_boundary(value).is_some(), ok, "{value}")
		}
	}
}
