//! Durable, profile-owned source excerpts and personal annotations.
use crate::{
	account,
	reading::WriteError,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Saved {
	pub id: i64,
	pub post: String,
	pub comment: String,
	pub title: String,
	pub community: String,
	pub author: String,
	pub body: String,
	pub context: String,
	pub captured: i64,
	pub note: String,
	pub collection: String,
	pub revision: i64,
}
impl Saved {
	pub fn link(&self) -> String {
		if self.comment.is_empty() {
			format!("/comments/{}", self.post)
		} else {
			format!("/comments/{}/comments/{}?context=8#{}", self.post, self.comment, self.comment)
		}
	}
	pub fn date(&self) -> String {
		crate::utils::time(self.captured as f64).0
	}
}
pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch("CREATE TABLE IF NOT EXISTS reading_library(
 id INTEGER PRIMARY KEY AUTOINCREMENT,profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
 post TEXT NOT NULL,comment TEXT NOT NULL,title TEXT NOT NULL,community TEXT NOT NULL,author TEXT NOT NULL,
 body TEXT NOT NULL,context TEXT NOT NULL,captured INTEGER NOT NULL,note TEXT NOT NULL DEFAULT '',collection TEXT NOT NULL DEFAULT '',revision INTEGER NOT NULL DEFAULT 1,
 UNIQUE(profile_id,post,comment));
 CREATE VIRTUAL TABLE IF NOT EXISTS reading_archive_search USING fts5(profile UNINDEXED,archive UNINDEXED,source UNINDEXED,title,body);
 CREATE TRIGGER IF NOT EXISTS reading_archive_delete AFTER DELETE ON post_archives BEGIN DELETE FROM reading_archive_search WHERE archive=old.id AND profile=old.profile_id;END;
 CREATE VIRTUAL TABLE IF NOT EXISTS reading_library_search USING fts5(title,body,context,note,collection,content=reading_library,content_rowid=id);
 CREATE TRIGGER IF NOT EXISTS library_insert AFTER INSERT ON reading_library BEGIN INSERT INTO reading_library_search(rowid,title,body,context,note,collection) VALUES(new.id,new.title,new.body,new.context,new.note,new.collection);END;
 CREATE TRIGGER IF NOT EXISTS library_delete AFTER DELETE ON reading_library BEGIN INSERT INTO reading_library_search(reading_library_search,rowid,title,body,context,note,collection) VALUES('delete',old.id,old.title,old.body,old.context,old.note,old.collection);END;
 CREATE TRIGGER IF NOT EXISTS library_update AFTER UPDATE ON reading_library BEGIN INSERT INTO reading_library_search(reading_library_search,rowid,title,body,context,note,collection) VALUES('delete',old.id,old.title,old.body,old.context,old.note,old.collection);INSERT INTO reading_library_search(rowid,title,body,context,note,collection) VALUES(new.id,new.title,new.body,new.context,new.note,new.collection);END;")
}
pub(crate) fn plain(text: &str) -> String {
	let text = ammonia::Builder::new()
		.tags(std::collections::HashSet::new())
		.clean(&text.replace("</p>", "\n\n"))
		.to_string();
	htmlescape::decode_html(&text).unwrap_or(text)
}
pub fn save(db: &mut Connection, profile: i64, item: &Saved) -> Result<i64, WriteError> {
	if !account::valid_post_id(&item.post)
		|| (!item.comment.is_empty() && !account::valid_post_id(&item.comment))
		|| item.title.trim().is_empty()
		|| item.title.len() > 4000
		|| item.body.len() > 65536
		|| item.context.len() > 65536
		|| item.author.len() > 128
		|| item.community.len() > 80
		|| item.captured < 0
	{
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	// Re-saving retains the original evidence and its personal notes.
	if let Some(id) = tx
		.query_row(
			"SELECT id FROM reading_library WHERE profile_id=?1 AND post=?2 AND comment=?3",
			params![profile, item.post, item.comment],
			|r| r.get(0),
		)
		.optional()?
	{
		return Ok(id);
	}
	if tx.query_row("SELECT count(*) FROM reading_library WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 2000 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT INTO reading_library(profile_id,post,comment,title,community,author,body,context,captured) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
		params![
			profile,
			item.post,
			item.comment,
			item.title,
			item.community,
			item.author,
			plain(&item.body),
			plain(&item.context),
			item.captured
		],
	)?;
	let id = tx.last_insert_rowid();
	tx.commit()?;
	Ok(id)
}
const COLS: &str = "id,post,comment,title,community,author,body,context,captured,note,collection,revision";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Saved> {
	Ok(Saved {
		id: r.get(0)?,
		post: r.get(1)?,
		comment: r.get(2)?,
		title: r.get(3)?,
		community: r.get(4)?,
		author: r.get(5)?,
		body: r.get(6)?,
		context: r.get(7)?,
		captured: r.get(8)?,
		note: r.get(9)?,
		collection: r.get(10)?,
		revision: r.get(11)?,
	})
}
pub fn get(db: &Connection, profile: i64, id: i64) -> Result<Option<Saved>, WriteError> {
	Ok(
		db.query_row(&format!("SELECT {COLS} FROM reading_library WHERE profile_id=?1 AND id=?2"), params![profile, id], row)
			.optional()?,
	)
}
pub fn annotate(db: &mut Connection, profile: i64, id: i64, revision: i64, note: &str, collection: &str, remove: bool) -> Result<(), WriteError> {
	if note.chars().count() > 8192 || collection.trim().chars().count() > 80 || collection.contains(['\n', '\r', '\0']) {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let item = get(&tx, profile, id)?.ok_or(WriteError::Invalid)?;
	if item.revision != revision {
		return Err(WriteError::Conflict);
	}
	if remove {
		tx.execute("DELETE FROM reading_library WHERE profile_id=?1 AND id=?2", params![profile, id])?;
	} else {
		tx.execute(
			"UPDATE reading_library SET note=?3,collection=?4,revision=revision+1 WHERE profile_id=?1 AND id=?2",
			params![profile, id, note, collection.trim()],
		)?;
	}
	tx.commit()?;
	Ok(())
}
fn query(value: &str) -> Result<String, WriteError> {
	if value.len() > 256 {
		return Err(WriteError::Invalid);
	}
	Ok(
		value
			.split_whitespace()
			.take(20)
			.map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
			.collect::<Vec<_>>()
			.join(" AND "),
	)
}
pub fn search(db: &Connection, profile: i64, text: &str, collection: &str, offset: i64) -> Result<Vec<Saved>, WriteError> {
	if !(0..=2000).contains(&offset) || collection.chars().count() > 80 {
		return Err(WriteError::Invalid);
	}
	let q = query(text)?;
	let sql=format!("SELECT {COLS} FROM reading_library WHERE profile_id=?1 AND (?2='' OR collection=?2) AND (?3='' OR id IN(SELECT rowid FROM reading_library_search WHERE reading_library_search MATCH ?3)) ORDER BY id DESC LIMIT 100 OFFSET ?4");
	let mut stmt = db.prepare(&sql)?;
	let rows = stmt.query_map(params![profile, collection, q, offset], row)?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
#[derive(Template)]
#[template(path = "library.html")]
struct Page {
	collections: Vec<String>,
	archive_hits: Vec<ArchiveHit>,
	prefs: Preferences,
	url: String,
	items: Vec<Saved>,
	selected: Option<Saved>,
	q: String,
	collection: String,
	next: String,
	previous: String,
}
pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to use your library."));
	};
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	let db = account::open_database()?;
	let selected = if v("id").is_empty() {
		None
	} else {
		let item = get(&db, profile, v("id").parse().unwrap_or(0)).map_err(|e| format!("{e:?}"))?;
		if item.is_none() {
			return Ok(reply(StatusCode::NOT_FOUND, "Saved item not found."));
		}
		item
	};
	let offset = v("offset").parse().unwrap_or(0);
	let items = match search(&db, profile, v("q"), v("collection"), offset) {
		Ok(items) => items,
		Err(_) => return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Search is too long or its page is invalid.")),
	};
	if v("export") == "json" {
		let mut all = Vec::new();
		for offset in (0..2000).step_by(100) {
			let batch = search(&db, profile, v("q"), v("collection"), offset).map_err(|e| format!("{e:?}"))?;
			let len = batch.len();
			all.extend(batch);
			if len < 100 {
				break;
			}
		}
		let json = serde_json::to_vec_pretty(&serde_json::json!({"format":"vale-library-v1","items":all})).map_err(|e| e.to_string())?;
		return Ok(
			Response::builder()
				.header("content-type", "application/json")
				.header("content-disposition", "attachment; filename=vale-library.json")
				.header("cache-control", "private, no-store")
				.body(Body::from(json))
				.unwrap(),
		);
	}
	let page_url = |offset: i64| {
		let mut s = url::form_urlencoded::Serializer::new(String::new());
		s.append_pair("q", v("q"))
			.append_pair("collection", v("collection"))
			.append_pair("offset", &offset.to_string());
		format!("/reading/library?{}", s.finish())
	};
	let next = if items.len() == 100 { page_url(offset + 100) } else { String::new() };
	let previous = if offset > 0 { page_url((offset - 100).max(0)) } else { String::new() };
	let mut collection_stmt = db
		.prepare("SELECT DISTINCT collection FROM reading_library WHERE profile_id=?1 AND collection<>'' ORDER BY collection COLLATE NOCASE")
		.map_err(|e| e.to_string())?;
	let collections = collection_stmt
		.query_map([profile], |r| r.get::<_, String>(0))
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let archive_hits = archive_search(&db, profile, v("q")).map_err(|e| format!("{e:?}"))?;
	Ok(template(&Page {
		collections,
		archive_hits,
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		items,
		selected,
		q: v("q").into(),
		collection: v("collection").into(),
		next,
		previous,
	}))
}
pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to save comments."));
	};
	let prefs = Preferences::new(&req);
	let bytes = crate::utils::read_body_limited(req.body_mut(), 131072, "Note is too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	let id = if v("action") == "capture" {
		let parts = v("source").split('/').collect::<Vec<_>>();
		let post = parts.iter().position(|s| *s == "comments").and_then(|i| parts.get(i + 1)).copied().unwrap_or("");
		let comment = v("comment");
		if !account::valid_post_id(post) || !account::valid_post_id(comment) {
			return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Invalid comment reference."));
		}
		let path = format!("/comments/{post}.json?raw_json=1&comment={comment}&context=8&limit=100");
		let json = match tokio::time::timeout(std::time::Duration::from_secs(30), crate::client::json(path, false)).await {
			Ok(Ok(json)) => json,
			_ => return Ok(reply(StatusCode::BAD_GATEWAY, "The comment could not be retrieved. Your saved library is unchanged.")),
		};
		let data = &json[0]["data"]["children"][0]["data"];
		if data["id"].as_str() != Some(post) || data["over_18"].as_bool().unwrap_or(false) && (prefs.show_nsfw != "on" || crate::utils::sfw_only()) {
			return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "The source is unavailable under your content preferences."));
		}
		let filters = prefs.filters.iter().cloned().collect();
		let thread = crate::thread::ThreadModel::from_listing(
			&json[1],
			post,
			0,
			&format!("/comments/{post}/"),
			data["author"].as_str().unwrap_or(""),
			"",
			&filters,
			&prefs.comment_keywords(),
			&prefs,
		);
		let Some(c) = thread
			.observed_comments()
			.find(|c| c.id == comment && c.filter_state == crate::thread::CommentFilterState::Visible)
		else {
			return Ok(reply(StatusCode::NOT_FOUND, "That comment was not returned or is hidden by your filters."));
		};
		let mut context = String::new();
		for ancestor in &c.ancestor_path {
			if let Some(crate::thread::ThreadNode::Comment(parent)) = thread.node(ancestor) {
				if parent.filter_state == crate::thread::CommentFilterState::Visible {
					context.push_str(&format!("u/{}: {}\n\n", parent.author.name, plain(&parent.body)));
				}
			}
		}
		context = context.chars().take(16000).collect();
		if context.is_empty() {
			context = "No parent text was returned in this capture.".into()
		}
		let item = Saved {
			id: 0,
			post: post.into(),
			comment: comment.into(),
			title: data["title"].as_str().unwrap_or("Saved discussion").into(),
			community: data["subreddit"].as_str().unwrap_or("").into(),
			author: c.author.name.clone(),
			body: c.body.clone(),
			context,
			captured: account::now(),
			note: String::new(),
			collection: String::new(),
			revision: 0,
		};
		match save(&mut account::open_database()?, profile, &item) {
			Ok(id) => id,
			Err(_) => {
				return Ok(reply(
					StatusCode::CONFLICT,
					"Unable to retain this comment. Check its size or your library capacity (2,000 items).",
				))
			}
		}
	} else {
		if !matches!(v("action"), "annotate" | "remove") {
			return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Invalid library action."));
		}
		let id = v("id").parse().unwrap_or(0);
		if let Err(e) = annotate(
			&mut account::open_database()?,
			profile,
			id,
			v("revision").parse().unwrap_or(-1),
			v("note"),
			v("collection"),
			v("action") == "remove",
		) {
			return Ok(reply(
				if e == WriteError::Conflict {
					StatusCode::CONFLICT
				} else {
					StatusCode::UNPROCESSABLE_ENTITY
				},
				"The saved item changed or the note is invalid. Use Back to preserve your draft, then reload the item to review changes.",
			));
		}
		if v("action") == "remove" {
			0
		} else {
			id
		}
	};
	Ok(
		Response::builder()
			.status(StatusCode::SEE_OTHER)
			.header("location", if id == 0 { "/reading/library".into() } else { format!("/reading/library?id={id}") })
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
		db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE profiles(id INTEGER PRIMARY KEY);CREATE TABLE post_archives(id TEXT PRIMARY KEY,profile_id INTEGER,status TEXT);INSERT INTO profiles VALUES(1),(2);")
			.unwrap();
		initialize(&db).unwrap();
		db
	}
	fn item() -> Saved {
		Saved {
			id: 0,
			post: "post1".into(),
			comment: "reply1".into(),
			title: "Workshop evidence".into(),
			community: "woodworking".into(),
			author: "reader".into(),
			body: "A dovetail joint".into(),
			context: "Parent about walnut".into(),
			captured: 1000,
			note: String::new(),
			collection: String::new(),
			revision: 0,
		}
	}
	#[test]
	fn save_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut i = item();
			let mut profile = 1;
			match case {
				0 => i.comment.clear(),
				1 => i.post = "../bad".into(),
				2 => i.comment = "a/b".into(),
				3 => i.title = " ".into(),
				4 => i.title = "x".repeat(4001),
				5 => i.body = "x".repeat(65537),
				6 => i.context = "x".repeat(65537),
				7 => i.captured = -1,
				8 => profile = 99,
				9 => i.body = "<script>bad()</script><p>Safe &amp; readable</p>".into(),
				10 => initialize(&db).unwrap(),
				11 => {
					let id = save(&mut db, 1, &i).unwrap();
					annotate(&mut db, 1, id, 1, "My note", "Craft", false).unwrap();
					i.body = "Edited upstream".into();
				}
				_ => {}
			}
			let result = save(&mut db, profile, &i);
			if matches!(case, 1..=8) {
				assert!(result.is_err(), "case {case}")
			} else {
				let id = result.unwrap();
				assert!(get(&db, 2, id).unwrap().is_none());
				let saved = get(&db, 1, id).unwrap().unwrap();
				match case {
					9 => {
						assert!(saved.body.contains("Safe & readable"));
						assert!(!saved.body.contains("bad()"));
					}
					11 => {
						assert_eq!(saved.body, "A dovetail joint");
						assert_eq!(saved.note, "My note");
					}
					_ => assert_eq!(saved.body, i.body),
				}
			}
		}
	}
	fn annotation_edges(remove: bool) {
		for case in 0..12 {
			let mut db = db();
			let id = save(&mut db, 1, &item()).unwrap();
			let mut profile = 1;
			let mut target = id;
			let mut revision = 1;
			let mut note = "Personal observation".to_string();
			let mut collection = "Workshop".to_string();
			match case {
				0 => note.clear(),
				1 => collection.clear(),
				2 => note = "x".repeat(32769),
				3 => collection = "x".repeat(81),
				4 => collection = "bad\nname".into(),
				5 => revision = 0,
				6 => profile = 2,
				7 => target = 999,
				8 => {
					annotate(&mut db, 1, id, 1, "Earlier", "", false).unwrap();
				}
				9 => note = "<script>literal private note</script>".into(),
				10 => initialize(&db).unwrap(),
				11 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				_ => {}
			}
			let result = annotate(&mut db, profile, target, revision, &note, &collection, remove);
			if matches!(case, 2..=8 | 11) {
				assert!(result.is_err(), "remove={remove} case {case}")
			} else {
				result.unwrap();
				let saved = get(&db, 1, id).unwrap();
				if remove {
					assert!(saved.is_none());
					assert!(search(&db, 1, "dovetail", "", 0).unwrap().is_empty())
				} else {
					let saved = saved.unwrap();
					assert_eq!(saved.note, note);
					assert_eq!(saved.collection, collection);
					assert_eq!(saved.revision, 2);
					assert_eq!(saved.body, item().body)
				}
			}
		}
	}
	#[test]
	fn notes_collections_twelve_edges() {
		annotation_edges(false)
	}
	#[test]
	fn remove_twelve_edges() {
		annotation_edges(true)
	}
	#[test]
	fn search_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = save(&mut db, 1, &item()).unwrap();
			annotate(&mut db, 1, id, 1, "Café repair", "Craft", false).unwrap();
			let (profile, q, collection, offset, expected) = match case {
				0 => (1, "dovetail", "", 0, 1),
				1 => (1, "walnut", "", 0, 1),
				2 => (1, "cafe", "", 0, 1),
				3 => (1, "WORKSHOP", "", 0, 1),
				4 => (2, "dovetail", "", 0, 0),
				5 => (1, "missing", "", 0, 0),
				6 => (1, "dovetail", "Other", 0, 0),
				7 => (1, "dovetail", "Craft", 0, 1),
				8 => (1, "dovetail OR missing", "", 0, 0),
				9 => (1, "\" OR *", "", 0, 0),
				10 => (1, "", "", 100, 0),
				_ => (1, "dovetail walnut", "", 0, 1),
			};
			let results = search(&db, profile, q, collection, offset).unwrap();
			assert_eq!(results.len(), expected, "case {case}");
		}
	}
	#[test]
	fn query_twelve_boundaries() {
		for (input, ok) in [
			("", true),
			(" ", true),
			("word", true),
			("two words", true),
			("OR", true),
			("*", true),
			("\"", true),
			("(x)", true),
			("a:b", true),
			("日本語", true),
			("\n", true),
			(&"x".repeat(257), false),
		] {
			assert_eq!(query(input).is_ok(), ok, "{input:?}");
		}
	}
	#[test]
	fn export_and_lookup_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut i = item();
			match case {
				0 => i.body = "Unicode 日本語".into(),
				1 => i.body = "Line one\nLine two".into(),
				2 => i.body = "\"quote\"".into(),
				3 => i.body = "\\backslash".into(),
				4 => i.comment.clear(),
				5 => i.context.clear(),
				6 => i.author = "[deleted]".into(),
				7 => i.body = "[removed]".into(),
				8 => i.title = "A & B".into(),
				9 => i.body = "\ttext".into(),
				10 => i.body = "🪵".into(),
				_ => i.captured = 0,
			}
			let id = save(&mut db, 1, &i).unwrap();
			let saved = get(&db, 1, id).unwrap().unwrap();
			let json = serde_json::to_vec(&saved).unwrap();
			assert_eq!(serde_json::from_slice::<Saved>(&json).unwrap(), saved);
			assert!(get(&db, 2, id).unwrap().is_none());
			assert!(get(&db, 1, id + 1).unwrap().is_none());
		}
	}
}

#[derive(Clone, Debug)]
struct ArchiveHit {
	archive: String,
	source: String,
	title: String,
	excerpt: String,
}
fn archive_search(db: &Connection, profile: i64, text: &str) -> Result<Vec<ArchiveHit>, WriteError> {
	let q = query(text)?;
	if q.is_empty() {
		return Ok(vec![]);
	}
	let mut stmt=db.prepare("SELECT archive,source,title,snippet(reading_archive_search,4,'[',']','…',32) FROM reading_archive_search WHERE reading_archive_search MATCH ?1 AND profile=?2 AND EXISTS(SELECT 1 FROM post_archives a WHERE a.id=archive AND a.profile_id=?2 AND a.status IN('ready','partial')) ORDER BY rank LIMIT 50")?;
	let rows = stmt.query_map(params![q, profile], |r| {
		Ok(ArchiveHit {
			archive: r.get(0)?,
			source: r.get(1)?,
			title: r.get(2)?,
			excerpt: r.get(3)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
pub(crate) fn index_archive(db: &mut Connection, profile: i64, archive: &str, manifest: &crate::archive::ArchiveManifest) -> Result<usize, WriteError> {
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if !tx.query_row(
		"SELECT EXISTS(SELECT 1 FROM post_archives WHERE id=?1 AND profile_id=?2 AND status IN('ready','partial'))",
		params![archive, profile],
		|r| r.get::<_, bool>(0),
	)? {
		return Err(WriteError::Invalid);
	}
	let mut rows = vec![(manifest.post.id.clone(), plain(&manifest.post.body_html))];
	let mut stack = manifest.comments.iter().collect::<Vec<_>>();
	let mut bytes = rows[0].1.len();
	while let Some(c) = stack.pop() {
		let body = plain(&c.body_html);
		bytes = bytes.saturating_add(body.len());
		if rows.len() >= 20000 || bytes > 64 * 1024 * 1024 {
			return Err(WriteError::Full);
		}
		rows.push((format!("comment-{}", c.id), body));
		stack.extend(c.replies.iter());
	}
	let (used, count) = tx.query_row(
		"SELECT coalesce(sum(length(cast(title AS BLOB))+length(cast(body AS BLOB))),0),count(*) FROM reading_archive_search WHERE profile=?1 AND archive<>?2",
		params![profile, archive],
		|r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
	)?;
	if used as usize + bytes + manifest.post.title.len() * rows.len() > 64 * 1024 * 1024 || count as usize + rows.len() > 100000 {
		return Err(WriteError::Full);
	}
	tx.execute("DELETE FROM reading_archive_search WHERE profile=?1 AND archive=?2", params![profile, archive])?;
	for (source, body) in &rows {
		tx.execute(
			"INSERT INTO reading_archive_search(profile,archive,source,title,body) VALUES(?1,?2,?3,?4,?5)",
			params![profile, archive, source, manifest.post.title, body],
		)?;
	}
	tx.commit()?;
	Ok(rows.len())
}

#[cfg(test)]
mod archive_index_tests {
	use super::*;
	fn manifest() -> crate::archive::ArchiveManifest {
		serde_json::from_value(serde_json::json!({"format":"VALE_ARCHIVE_1","captured_at":1000,"comment_count":0,"post":{"id":"post1","title":"Workshop","community":"rust","author":"reader","permalink":"/comments/post1","source_url":"","body_html":"<p>Walnut evidence</p>","post_type":"text","created":"","score":0,"upvote_ratio":0,"media":[],"source_snapshot":""},"comments":[],"assets":[],"issues":[],"initial_reddit_json":{},"additional_comment_things":[]})).unwrap()
	}
	#[test]
	fn indexing_twelve_edges() {
		for case in 0..12 {
			let mut db = Connection::open_in_memory().unwrap();
			db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE profiles(id INTEGER PRIMARY KEY);CREATE TABLE post_archives(id TEXT PRIMARY KEY,profile_id INTEGER,status TEXT);INSERT INTO profiles VALUES(1),(2);INSERT INTO post_archives VALUES('archive1',1,'ready');").unwrap();
			initialize(&db).unwrap();
			let mut m = manifest();
			let mut profile = 1;
			let mut id = "archive1";
			match case {
				0 => {}
				1 => profile = 2,
				2 => id = "missing",
				3 => {
					db.execute("UPDATE post_archives SET status='pending'", []).unwrap();
				}
				4 => {
					db.execute("UPDATE post_archives SET status='partial'", []).unwrap();
				}
				5 => m.post.body_html = "<script>unsafe</script><p>Walnut</p>".into(),
				6 => m.post.body_html = "Unicode café walnut".into(),
				7 => {
					index_archive(&mut db, 1, id, &m).unwrap();
				}
				8 => {
					initialize(&db).unwrap();
				}
				9 => m.comments.push(crate::archive::ArchivedComment {
					id: "reply1".into(),
					parent_id: "post1".into(),
					author: "reader".into(),
					body_html: "Walnut comment".into(),
					created: String::new(),
					score: 0,
					score_hidden: false,
					replies: vec![],
				}),
				10 => m.post.body_html.clear(),
				11 => m.post.title = "<literal>".into(),
				_ => {}
			}
			let result = index_archive(&mut db, profile, id, &m);
			if matches!(case, 1..=3) {
				assert!(result.is_err(), "case {case}")
			} else {
				result.unwrap();
				let matches = archive_search(&db, 1, "walnut").unwrap();
				assert_eq!(
					matches.len(),
					if case == 9 {
						2
					} else if case == 10 {
						0
					} else {
						1
					},
					"case {case}"
				);
				assert!(archive_search(&db, 2, "walnut").unwrap().is_empty());
				db.execute("DELETE FROM post_archives WHERE id='archive1'", []).unwrap();
				assert!(archive_search(&db, 1, "walnut").unwrap().is_empty());
				assert_eq!(db.query_row("SELECT count(*) FROM reading_archive_search", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
			}
		}
	}
}
