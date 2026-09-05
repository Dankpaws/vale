//! Explicit primary-source feeds. Isolated public-address fetching, no Reddit
//! credentials, no automatic subscriptions, and no generated interpretation.
use crate::{
	account,
	reading::WriteError,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Source {
	pub id: i64,
	pub feed: String,
	pub title: String,
	pub url: String,
	pub checked: i64,
	pub error: String,
	pub revision: i64,
	pub unread: i64,
	pub through: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceItem {
	pub id: i64,
	pub source: i64,
	pub title: String,
	pub url: String,
	pub body: String,
	pub published: String,
}
impl Source {
	pub fn checked_label(&self) -> String {
		if self.checked == 0 {
			"Never".into()
		} else {
			format!("{} ago", crate::utils::time(self.checked as f64).0)
		}
	}
}
pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch("CREATE TABLE IF NOT EXISTS reading_sources(id INTEGER PRIMARY KEY AUTOINCREMENT,profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,feed TEXT NOT NULL,title TEXT NOT NULL,url TEXT NOT NULL,checked INTEGER NOT NULL DEFAULT 0,error TEXT NOT NULL DEFAULT '',revision INTEGER NOT NULL DEFAULT 1,UNIQUE(profile_id,feed,url));CREATE TABLE IF NOT EXISTS reading_source_items(id INTEGER PRIMARY KEY AUTOINCREMENT,source INTEGER NOT NULL REFERENCES reading_sources(id) ON DELETE CASCADE,guid TEXT NOT NULL,title TEXT NOT NULL,url TEXT NOT NULL,body TEXT NOT NULL,published TEXT NOT NULL,UNIQUE(source,guid));
 CREATE TABLE IF NOT EXISTS reading_source_seen(source INTEGER PRIMARY KEY REFERENCES reading_sources(id) ON DELETE CASCADE,through_id INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS reading_source_discussions(profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,feed TEXT NOT NULL,post_id TEXT NOT NULL,title TEXT NOT NULL,community TEXT NOT NULL,url TEXT NOT NULL,observed INTEGER NOT NULL,PRIMARY KEY(profile_id,feed,post_id));")
}
fn safe_url(value: &str) -> Option<String> {
	if value.len() > 2048 {
		return None;
	}
	let u = url::Url::parse(value).ok()?;
	if !matches!(u.scheme(), "http" | "https")
		|| u.host_str().is_none()
		|| !u.username().is_empty()
		|| u.password().is_some()
		|| !matches!(u.port_or_known_default(), Some(80 | 443))
	{
		return None;
	}
	Some(u.to_string())
}
pub fn add(db: &mut Connection, profile: i64, feeds: &[crate::utils::FeedGroup], feed: &str, title: &str, url: &str) -> Result<i64, WriteError> {
	let url = safe_url(url).ok_or(WriteError::Invalid)?;
	if title.trim().is_empty() || title.len() > 200 || !feeds.iter().any(|f| f.slug == feed) {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if let Some(id) = tx
		.query_row(
			"SELECT id FROM reading_sources WHERE profile_id=?1 AND feed=?2 AND url=?3",
			params![profile, feed, url],
			|r| r.get(0),
		)
		.optional()?
	{
		return Ok(id);
	}
	if tx.query_row("SELECT count(*) FROM reading_sources WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 32 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT INTO reading_sources(profile_id,feed,title,url) VALUES(?1,?2,?3,?4)",
		params![profile, feed, title.trim(), url],
	)?;
	let id = tx.last_insert_rowid();
	tx.commit()?;
	Ok(id)
}
pub fn entries(db: &Connection, profile: i64, feed: &str) -> Result<Vec<SourceItem>, WriteError> {
	let mut stmt=db.prepare("SELECT i.id,i.source,i.title,i.url,i.body,i.published FROM reading_source_items i JOIN reading_sources s ON s.id=i.source WHERE s.profile_id=?1 AND (?2='' OR s.feed=?2) ORDER BY i.id DESC LIMIT 200")?;
	let rows = stmt.query_map(params![profile, feed], |r| {
		Ok(SourceItem {
			id: r.get(0)?,
			source: r.get(1)?,
			title: r.get(2)?,
			url: r.get(3)?,
			body: r.get(4)?,
			published: r.get(5)?,
		})
	})?;
	let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
	entries.sort_by_cached_key(|i| {
		std::cmp::Reverse((
			chrono::DateTime::parse_from_rfc3339(&i.published)
				.or_else(|_| chrono::DateTime::parse_from_rfc2822(&i.published))
				.map(|d| d.timestamp())
				.unwrap_or(0),
			i.id,
		))
	});
	Ok(entries)
}
fn parse(bytes: &[u8]) -> Result<Vec<(String, SourceItem)>, WriteError> {
	if bytes.len() > 2 * 1024 * 1024 {
		return Err(WriteError::Full);
	}
	let mut result = Vec::new();
	if let Ok(channel) = rss::Channel::read_from(bytes) {
		for i in channel.items.iter().take(100) {
			let Some(url) = i.link.as_deref().and_then(safe_url) else { continue };
			let title = i.title.clone().unwrap_or_else(|| url.clone());
			let guid = i.guid.as_ref().map(|g| g.value.clone()).unwrap_or_else(|| url.clone());
			result.push((
				guid,
				SourceItem {
					id: 0,
					source: 0,
					title,
					url,
					body: crate::library::plain(i.content.as_deref().or(i.description.as_deref()).unwrap_or("")),
					published: i.pub_date.clone().unwrap_or_default(),
				},
			));
		}
	} else if let Ok(feed) = atom_syndication::Feed::read_from(bytes) {
		for i in feed.entries.iter().take(100) {
			let Some(url) = i.links.iter().find(|l| l.rel == "alternate" || l.rel.is_empty()).and_then(|l| safe_url(&l.href)) else {
				continue;
			};
			result.push((
				i.id.clone(),
				SourceItem {
					id: 0,
					source: 0,
					title: i.title.value.clone(),
					url,
					body: crate::library::plain(
						i.content
							.as_ref()
							.and_then(|c| c.value.as_deref())
							.or(i.summary.as_ref().map(|s| s.value.as_str()))
							.unwrap_or(""),
					),
					published: i.published.as_ref().map(|d| d.to_rfc3339()).unwrap_or_else(|| i.updated.to_rfc3339()),
				},
			));
		}
	} else {
		return Err(WriteError::Invalid);
	}
	for (guid, item) in &mut result {
		*guid = guid.chars().take(2048).collect();
		item.title = item.title.chars().take(1000).collect();
		item.body = item.body.chars().take(16000).collect();
		item.published = item.published.chars().take(100).collect();
	}
	Ok(result)
}
fn retain(db: &mut Connection, profile: i64, id: i64, items: &[(String, SourceItem)]) -> Result<(), WriteError> {
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if !tx.query_row("SELECT EXISTS(SELECT 1 FROM reading_sources WHERE id=?1 AND profile_id=?2)", params![id, profile], |r| {
		r.get::<_, bool>(0)
	})? {
		return Err(WriteError::Invalid);
	}
	if items.len() > 100 {
		return Err(WriteError::Invalid);
	}
	for (guid, i) in items.iter().rev() {
		tx.execute("INSERT INTO reading_source_items(source,guid,title,url,body,published) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(source,guid) DO UPDATE SET title=excluded.title,url=excluded.url,body=excluded.body,published=excluded.published",params![id,guid,i.title,i.url,i.body,i.published])?;
	}
	tx.execute(
		"DELETE FROM reading_source_items WHERE source=?1 AND id NOT IN(SELECT id FROM reading_source_items WHERE source=?1 ORDER BY id DESC LIMIT 200)",
		[id],
	)?;
	tx.execute("UPDATE reading_sources SET error='' WHERE id=?1", [id])?;
	tx.commit()?;
	Ok(())
}
fn claim(db: &mut Connection, profile: i64, id: i64, now: i64) -> Result<Option<String>, WriteError> {
	if now < 0 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let row = tx
		.query_row("SELECT url,checked FROM reading_sources WHERE profile_id=?1 AND id=?2", params![profile, id], |r| {
			Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
		})
		.optional()?;
	let Some((url, checked)) = row else { return Err(WriteError::Invalid) };
	if checked > 0 && now.saturating_sub(checked) < 3600 {
		return Ok(None);
	}
	tx.execute("UPDATE reading_sources SET checked=?3 WHERE profile_id=?1 AND id=?2", params![profile, id, now])?;
	tx.commit()?;
	Ok(Some(url))
}
async fn refresh(profile: i64, id: i64) -> Result<(), String> {
	let Some(url) = claim(&mut account::open_database()?, profile, id, account::now()).map_err(|e| format!("{e:?}"))? else {
		return Ok(());
	};
	let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
		let (_, response) = crate::archive::public_response(&url).await?;
		if !response.status().is_success() {
			return Err("Source returned an error".to_string());
		}
		use futures_lite::StreamExt;
		let mut stream = response.bytes_stream();
		let mut bytes = Vec::new();
		while let Some(chunk) = stream.next().await {
			let chunk = chunk.map_err(|_| "Unable to read feed")?;
			if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
				return Err("Feed is too large".into());
			}
			bytes.extend_from_slice(&chunk)
		}
		parse(&bytes).map_err(|_| "Not a supported RSS or Atom feed".into())
	})
	.await;
	match result {
		Ok(Ok(items)) => retain(&mut account::open_database()?, profile, id, &items).map_err(|e| format!("{e:?}")),
		_ => {
			account::open_database()?
				.execute(
					"UPDATE reading_sources SET error='Refresh unavailable. Retained entries remain readable.' WHERE profile_id=?1 AND id=?2",
					params![profile, id],
				)
				.map_err(|e| e.to_string())?;
			Ok(())
		}
	}
}

#[derive(Clone, Debug)]
pub struct Discussion {
	pub id: String,
	pub title: String,
	pub community: String,
}
pub struct SourceEntry {
	pub item: SourceItem,
	pub source_name: String,
	pub unread: bool,
	pub discussions: Vec<Discussion>,
}
pub struct StoryChoice {
	pub id: i64,
	pub revision: i64,
	pub title: String,
}
pub struct FeedUpdate {
	pub feed: String,
	pub count: i64,
}
/// Conservative identity: fragments are page locations; query strings and schemes
/// remain significant. No guessed title/topic similarity or cross-feed joins.
pub fn exact_url(value: &str) -> Option<String> {
	let mut u = url::Url::parse(&safe_url(value)?).ok()?;
	u.set_fragment(None);
	Some(u.to_string())
}
pub fn observe(db: &Connection, profile: i64, feed: &str, posts: &[crate::utils::Post], now: i64) -> Result<(), WriteError> {
	if feed.is_empty() || feed.len() > 80 || posts.len() > 100 || now < 0 {
		return Err(WriteError::Invalid);
	}
	// The caller supplies only the current policy-filtered named-feed listing.
	let tx = db.unchecked_transaction()?;
	for p in posts {
		let Some(url) = p.out_url.as_deref().and_then(exact_url) else { continue };
		tx.execute("INSERT INTO reading_source_discussions(profile_id,feed,post_id,title,community,url,observed) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile_id,feed,post_id) DO UPDATE SET title=excluded.title,community=excluded.community,url=excluded.url,observed=excluded.observed",params![profile,feed,p.id,p.title,p.community,url,now])?;
	}
	tx.execute("DELETE FROM reading_source_discussions WHERE profile_id=?1 AND rowid NOT IN(SELECT rowid FROM reading_source_discussions WHERE profile_id=?1 ORDER BY observed DESC,rowid DESC LIMIT 1000)",[profile])?;
	tx.commit()?;
	Ok(())
}
pub fn acknowledge(db: &Connection, profile: i64, source: i64, through: i64) -> Result<(), WriteError> {
	let tx = db.unchecked_transaction()?;
	let max = tx
		.query_row(
			"SELECT coalesce(max(i.id),0) FROM reading_sources s LEFT JOIN reading_source_items i ON i.source=s.id WHERE s.id=?1 AND s.profile_id=?2 GROUP BY s.id",
			params![source, profile],
			|r| r.get::<_, i64>(0),
		)
		.optional()?
		.ok_or(WriteError::Invalid)?;
	if through < 0 || through > max {
		return Err(WriteError::Invalid);
	}
	tx.execute(
		"INSERT INTO reading_source_seen(source,through_id) VALUES(?1,?2) ON CONFLICT(source) DO UPDATE SET through_id=max(through_id,excluded.through_id)",
		params![source, through],
	)?;
	tx.commit()?;
	Ok(())
}
pub fn updates(db: &Connection, profile: i64) -> Result<Vec<FeedUpdate>, WriteError> {
	let mut q=db.prepare("SELECT s.feed,count(*) FROM reading_sources s JOIN reading_source_items i ON i.source=s.id LEFT JOIN reading_source_seen v ON v.source=s.id WHERE s.profile_id=?1 AND i.id>coalesce(v.through_id,0) GROUP BY s.feed ORDER BY s.feed")?;
	let rows = q.query_map([profile], |r| {
		Ok(FeedUpdate {
			feed: r.get(0)?,
			count: r.get(1)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
pub fn matching_entries(db: &Connection, profile: i64, feed: &str, url: &str) -> Result<Vec<SourceItem>, WriteError> {
	let Some(key) = exact_url(url) else { return Ok(vec![]) };
	Ok(entries(db, profile, feed)?.into_iter().filter(|i| exact_url(&i.url).as_deref() == Some(&key)).collect())
}
fn attach(db: &mut Connection, profile: i64, item: i64, story: i64, revision: i64, date: i64, note: &str) -> Result<(), WriteError> {
	let selected = crate::stories::get(db, profile, story)?.ok_or(WriteError::Invalid)?;
	let entry = db
		.query_row(
			"SELECT i.title,i.url,i.body,i.published FROM reading_source_items i JOIN reading_sources s ON i.source=s.id WHERE i.id=?1 AND s.profile_id=?2 AND s.feed=?3",
			params![item, profile, selected.feed],
			|r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
		)
		.optional()?
		.ok_or(WriteError::Invalid)?;
	crate::stories::add_event(
		db,
		profile,
		story,
		revision,
		&crate::stories::Event {
			id: 0,
			date,
			title: entry.0,
			url: entry.1,
			body: entry.2,
			community: String::new(),
			note: note.into(),
			provenance: format!("Selected source entry; published {}", entry.3),
		},
	)
}
#[derive(Template)]
#[template(path = "sources.html")]
struct Page {
	prefs: Preferences,
	url: String,
	sources: Vec<Source>,
	items: Vec<SourceEntry>,
	stories: Vec<StoryChoice>,
	feed: String,
}
pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to choose sources."));
	};
	let feed = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
		.find_map(|(k, v)| (k == "feed").then(|| v.into_owned()))
		.unwrap_or_default();
	let db = account::open_database()?;
	let mut stmt = db
		.prepare("SELECT s.id,s.feed,s.title,s.url,s.checked,s.error,s.revision,(SELECT count(*) FROM reading_source_items i WHERE i.source=s.id AND i.id>coalesce((SELECT through_id FROM reading_source_seen WHERE source=s.id),0)),(SELECT coalesce(max(id),0) FROM reading_source_items WHERE source=s.id) FROM reading_sources s WHERE s.profile_id=?1 AND (?2='' OR s.feed=?2) ORDER BY s.id")
		.map_err(|e| e.to_string())?;
	let sources: Vec<Source> = stmt
		.query_map(params![profile, feed], |r| {
			Ok(Source {
				id: r.get(0)?,
				feed: r.get(1)?,
				title: r.get(2)?,
				url: r.get(3)?,
				checked: r.get(4)?,
				error: r.get(5)?,
				revision: r.get(6)?,
				unread: r.get(7)?,
				through: r.get(8)?,
			})
		})
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let mut items = Vec::new();
	for item in entries(&db, profile, &feed).map_err(|e| format!("{e:?}"))? {
		let source = sources.iter().find(|s| s.id == item.source).ok_or("Source missing")?;
		let seen = db
			.query_row("SELECT coalesce((SELECT through_id FROM reading_source_seen WHERE source=?1),0)", [item.source], |r| {
				r.get::<_, i64>(0)
			})
			.map_err(|e| e.to_string())?;
		let key = exact_url(&item.url).unwrap_or_default();
		let mut q = db
			.prepare("SELECT post_id,title,community FROM reading_source_discussions WHERE profile_id=?1 AND feed=?2 AND url=?3 AND NOT EXISTS(SELECT 1 FROM hidden_posts h WHERE h.profile_id=?1 AND h.post_id=reading_source_discussions.post_id) ORDER BY observed DESC LIMIT 20")
			.map_err(|e| e.to_string())?;
		let discussions = q
			.query_map(params![profile, feed, key], |r| {
				Ok(Discussion {
					id: r.get(0)?,
					title: r.get(1)?,
					community: r.get(2)?,
				})
			})
			.map_err(|e| e.to_string())?
			.collect::<Result<Vec<_>, _>>()
			.map_err(|e| e.to_string())?;
		items.push(SourceEntry {
			unread: item.id > seen,
			item,
			source_name: source.title.clone(),
			discussions,
		});
	}
	let mut q = db
		.prepare("SELECT id,revision,title FROM reading_stories WHERE profile_id=?1 AND feed=?2 ORDER BY id DESC")
		.map_err(|e| e.to_string())?;
	let stories = q
		.query_map(params![profile, feed], |r| {
			Ok(StoryChoice {
				id: r.get(0)?,
				revision: r.get(1)?,
				title: r.get(2)?,
			})
		})
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	Ok(template(&Page {
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		sources,
		items,
		stories,
		feed,
	}))
}
pub fn remove(db: &Connection, profile: i64, id: i64, revision: i64) -> Result<(), WriteError> {
	let changed = db.execute("DELETE FROM reading_sources WHERE profile_id=?1 AND id=?2 AND revision=?3", params![profile, id, revision])?;
	if changed == 1 {
		Ok(())
	} else {
		Err(WriteError::Conflict)
	}
}
pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to choose sources."));
	};
	let prefs = Preferences::new(&req);
	let bytes = crate::utils::read_body_limited(req.body_mut(), 131072, "Source form too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	match v("action") {
		"add" => {
			let added = add(&mut account::open_database()?, profile, &prefs.feed_groups(), v("feed"), v("title"), v("source"));
			if added.is_err() {
				return Ok(reply(
					StatusCode::UNPROCESSABLE_ENTITY,
					"Choose a named feed, title, and public RSS or Atom URL. Up to 32 sources are supported.",
				));
			}
			refresh(profile, added.unwrap()).await?;
		}
		"refresh" => refresh(profile, v("id").parse().unwrap_or(0)).await?,
		"seen" => {
			if acknowledge(&account::open_database()?, profile, v("id").parse().unwrap_or(0), v("through").parse().unwrap_or(-1)).is_err() {
				return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Source changed. Reload before marking entries read."));
			}
		}
		"story" => {
			let (story, revision) = v("story").split_once(':').unwrap_or(("0", "0"));
			let date = chrono::NaiveDate::parse_from_str(v("date"), "%Y-%m-%d")
				.ok()
				.and_then(|d| d.and_hms_opt(0, 0, 0))
				.map(|d| d.and_utc().timestamp())
				.unwrap_or(-1);
			let result = attach(
				&mut account::open_database()?,
				profile,
				v("item").parse().unwrap_or(0),
				story.parse().unwrap_or(0),
				revision.parse().unwrap_or(0),
				date,
				v("note"),
			);
			if let Err(e) = result {
				return Ok(reply(
					if e == WriteError::Conflict {
						StatusCode::CONFLICT
					} else {
						StatusCode::UNPROCESSABLE_ENTITY
					},
					"Unable to attach entry. Check the date and selected story, then reload if it changed.",
				));
			}
			return Ok(
				Response::builder()
					.status(StatusCode::SEE_OTHER)
					.header("location", format!("/reading/stories?id={}", story.parse::<i64>().unwrap_or(0)))
					.body(Body::empty())
					.unwrap(),
			);
		}
		"remove" => {
			if remove(&account::open_database()?, profile, v("id").parse().unwrap_or(0), v("revision").parse().unwrap_or(-1)).is_err() {
				return Ok(reply(StatusCode::CONFLICT, "Source changed or was already removed. Reload to review it."));
			}
		}
		_ => return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "Unknown source action.")),
	}
	Ok(
		Response::builder()
			.status(StatusCode::SEE_OTHER)
			.header(
				"location",
				format!("/reading/sources?feed={}", url::form_urlencoded::byte_serialize(v("feed").as_bytes()).collect::<String>()),
			)
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
	#[test]
	fn exact_matching_twelve_edges() {
		for (input, expected) in [
			("https://EXAMPLE.org/a#part", Some("https://example.org/a")),
			("https://example.org:443/a", Some("https://example.org/a")),
			("https://example.org/a?q=1", Some("https://example.org/a?q=1")),
			("https://example.org/a?q=2", Some("https://example.org/a?q=2")),
			("http://example.org/a", Some("http://example.org/a")),
			("https://example.org/A", Some("https://example.org/A")),
			("https://example.org/a/", Some("https://example.org/a/")),
			("https://user@example.org/a", None),
			("javascript:alert(1)", None),
			("/a", None),
			("https://example.org:8443/a", None),
			("https://example.org/a#other", Some("https://example.org/a")),
		] {
			assert_eq!(exact_url(input).as_deref(), expected);
		}
	}
	#[test]
	fn unread_snapshot_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = add(&mut db, 1, &feeds(), "rust", "Source", "https://example.org/feed").unwrap();
			let data = parse(rss("Title", "https://example.org/a", "Text").as_bytes()).unwrap();
			retain(&mut db, 1, id, &data).unwrap();
			let through = entries(&db, 1, "rust").unwrap()[0].id;
			let mut profile = 1;
			let mut target = id;
			let mut marker = through;
			match case {
				1 => profile = 2,
				2 => target = 999,
				3 => marker = -1,
				4 => marker = through + 1,
				5 => {
					acknowledge(&db, 1, id, through).unwrap();
				}
				6 => marker = 0,
				7 => {
					let more = parse(rss("New", "https://example.org/b", "More").as_bytes()).unwrap();
					retain(&mut db, 1, id, &more).unwrap();
				}
				8 => retain(&mut db, 1, id, &data).unwrap(),
				9 => initialize(&db).unwrap(),
				10 => {
					remove(&db, 1, id, 1).unwrap();
				}
				11 => {
					acknowledge(&db, 1, id, through).unwrap();
					marker = 0;
				}
				_ => {}
			}
			let result = acknowledge(&db, profile, target, marker);
			assert_eq!(result.is_ok(), !matches!(case, 1 | 2 | 3 | 4 | 10), "case {case}");
			let count = updates(&db, 1).unwrap().iter().map(|u| u.count).sum::<i64>();
			assert_eq!(count, if matches!(case, 1 | 2 | 3 | 4 | 6 | 7) { 1 } else { 0 }, "case {case}");
			assert!(updates(&db, 2).unwrap().is_empty());
		}
	}
	#[test]
	fn source_story_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			crate::stories::initialize(&db).unwrap();
			let source = add(&mut db, 1, &feeds(), "rust", "Source", "https://example.org/feed").unwrap();
			let data = parse(rss("Title", "https://example.org/a", "Text").as_bytes()).unwrap();
			retain(&mut db, 1, source, &data).unwrap();
			let mut item = entries(&db, 1, "rust").unwrap()[0].id;
			let mut story = crate::stories::create(&mut db, 1, if case == 3 { "other" } else { "rust" }, "Story").unwrap();
			let mut profile = 1;
			let mut revision = 1;
			let mut date = 100;
			let mut note = String::new();
			match case {
				1 => profile = 2,
				2 => item = 999,
				4 => story = 999,
				5 => revision = 0,
				6 => date = -1,
				7 => note = "x".repeat(8193),
				8 => {
					remove(&db, 1, source, 1).unwrap();
				}
				9 => note = "é".repeat(8192),
				10 => {
					initialize(&db).unwrap();
				}
				11 => {
					crate::stories::edit(&mut db, 1, story, 1, "Changed", "released", "", false).unwrap();
				}
				_ => {}
			}
			let result = attach(&mut db, profile, item, story, revision, date, &note);
			assert_eq!(result.is_ok(), matches!(case, 0 | 9 | 10), "case {case}");
			if result.is_ok() {
				let ev = crate::stories::events(&db, 1, story).unwrap();
				assert_eq!(ev.len(), 1);
				assert_eq!(ev[0].body, "Text");
				assert_eq!(ev[0].url, "https://example.org/a");
			}
		}
	}
	#[test]
	fn entry_matching_scope_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let source = add(&mut db, 1, &feeds(), "rust", "Source", "https://example.org/feed").unwrap();
			let data = parse(rss("Title", "https://example.org/a?version=1", "Text").as_bytes()).unwrap();
			retain(&mut db, 1, source, &data).unwrap();
			let mut profile = 1;
			let mut feed = "rust";
			let mut url = "https://example.org/a?version=1";
			match case {
				1 => profile = 2,
				2 => feed = "other",
				3 => url = "https://example.org/a?version=2",
				4 => url = "https://example.org/a?version=1#notes",
				5 => url = "http://example.org/a?version=1",
				6 => url = "https://other.org/a?version=1",
				7 => url = "https://example.org/A?version=1",
				8 => url = "javascript:alert(1)",
				9 => {
					remove(&db, 1, source, 1).unwrap();
				}
				10 => url = "https://EXAMPLE.org:443/a?version=1",
				11 => {
					retain(&mut db, 1, source, &data).unwrap();
				}
				_ => {}
			}
			assert_eq!(
				matching_entries(&db, profile, feed, url).unwrap().len(),
				if matches!(case, 0 | 4 | 10 | 11) { 1 } else { 0 },
				"case {case}"
			);
		}
	}

	#[tokio::test]
	async fn observed_discussions_twelve_edges() {
		for case in 0..12 {
			let db = db();
			let mut posts = crate::reading_fixtures::posts().await;
			posts.truncate(1);
			posts[0].out_url = Some("https://example.org/a#details".into());
			let mut profile = 1;
			let mut feed = "rust";
			let mut now = 100;
			match case {
				1 => profile = 2,
				2 => feed = "other",
				3 => feed = "",
				4 => now = -1,
				5 => posts.clear(),
				6 => posts[0].out_url = None,
				7 => posts[0].out_url = Some("javascript:alert(1)".into()),
				8 => {
					observe(&db, 1, "rust", &posts, 99).unwrap();
				}
				9 => {
					initialize(&db).unwrap();
				}
				10 => posts[0].out_url = Some("https://example.org/a?x=1".into()),
				11 => {
					for n in 0..1005 {
						db.execute(
							"INSERT INTO reading_source_discussions VALUES(1,'rust',?1,'old','rust','https://example.org/old',1)",
							[format!("old{n}")],
						)
						.unwrap();
					}
				}
				_ => {}
			}
			let result = observe(&db, profile, feed, &posts, now);
			assert_eq!(result.is_ok(), !matches!(case, 3 | 4), "case {case}");
			let count = db
				.query_row(
					"SELECT count(*) FROM reading_source_discussions WHERE profile_id=?1 AND feed=?2 AND url=?3",
					params![profile, feed, if case == 10 { "https://example.org/a?x=1" } else { "https://example.org/a" }],
					|r| r.get::<_, i64>(0),
				)
				.unwrap();
			assert_eq!(count, if matches!(case, 3..=7) { 0 } else { 1 }, "case {case}");
			assert!(
				db.query_row("SELECT count(*) FROM reading_source_discussions WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))
					.unwrap()
					<= 1000
			);
		}
	}

	#[test]
	fn entry_order_twelve_dates() {
		for (published, newer) in [
			("2026-09-04T12:00:00Z", true),
			("2026-09-04T07:00:00-05:00", true),
			("Fri, 04 Sep 2026 12:00:00 +0000", true),
			("2026-09-03T23:59:59Z", false),
			("2025-01-01T00:00:00Z", false),
			("", false),
			("invalid", false),
			("2026-99-99T00:00:00Z", false),
			("2026-09-04T12:00:00.123Z", true),
			("2026-09-04T12:00:00+01:00", true),
			("1970-01-01T00:00:00Z", false),
			("2027-01-01T00:00:00Z", true),
		] {
			let mut db = db();
			let id = add(&mut db, 1, &feeds(), "rust", "Source", "https://example.org/feed").unwrap();
			let mut first = parse(rss("Baseline", "https://example.org/a", "text").as_bytes()).unwrap();
			first[0].1.published = "2026-09-04T00:00:00Z".into();
			retain(&mut db, 1, id, &first).unwrap();
			let mut second = parse(rss("Candidate", "https://example.org/b", "text").as_bytes()).unwrap();
			second[0].1.published = published.into();
			retain(&mut db, 1, id, &second).unwrap();
			assert_eq!(entries(&db, 1, "rust").unwrap()[0].title == "Candidate", newer, "{published}");
		}
	}

	fn feeds() -> Vec<crate::utils::FeedGroup> {
		vec![crate::utils::FeedGroup {
			name: "Rust".into(),
			slug: "rust".into(),
			communities: vec!["rust".into()],
		}]
	}
	fn rss(title: &str, link: &str, body: &str) -> String {
		format!("<rss version=\"2.0\"><channel><title>Feed</title><link>https://example.org</link><description>Source</description><item><title>{title}</title><link>{link}</link><description>{body}</description></item></channel></rss>")
	}
	#[test]
	fn removal_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = add(&mut db, 1, &feeds(), "rust", "Release", "https://example.org/rss").unwrap();
			let (mut profile, mut target, mut rev) = (1, id, 1);
			match case {
				0 => {}
				1 => profile = 2,
				2 => profile = 99,
				3 => target = 0,
				4 => target = -1,
				5 => target = i64::MAX,
				6 => rev = 0,
				7 => rev = -1,
				8 => rev = 2,
				9 => {
					remove(&db, 1, id, 1).unwrap();
				}
				10 => {
					initialize(&db).unwrap();
				}
				11 => {
					let items = parse(rss("Title", "https://example.org/r", "Text").as_bytes()).unwrap();
					retain(&mut db, 1, id, &items).unwrap();
				}
				_ => {}
			}
			assert_eq!(remove(&db, profile, target, rev).is_ok(), matches!(case, 0 | 10 | 11), "case {case}");
			if matches!(case, 0 | 10 | 11) {
				assert!(entries(&db, 1, "").unwrap().is_empty())
			}
		}
	}
	#[test]
	fn url_twelve_edges() {
		for (url, ok) in [
			("https://example.org/feed.xml", true),
			("http://example.org/rss", true),
			("https://example.org:443/rss", true),
			("https://example.org:8443/rss", false),
			("file:///etc/passwd", false),
			("ftp://example.org/feed", false),
			("javascript:alert(1)", false),
			("https://user:password@example.org/rss", false),
			("https://user@example.org/rss", false),
			("//example.org/rss", false),
			("invalid", false),
			("https://example.org/a?b=c", true),
		] {
			assert_eq!(safe_url(url).is_some(), ok, "{url}")
		}
	}
	#[test]
	fn parser_twelve_edges() {
		for case in 0..12 {
			let mut xml = rss("Release", "https://example.org/release", "Original evidence");
			match case{0=>{},1=>xml=rss("Unicode 日本語","https://example.org/r","Café"),2=>xml=rss("Title","javascript:alert(1)","Text"),3=>xml=rss("Title","https://user:pw@example.org/r","Text"),4=>xml=rss("Title","https://example.org/r","&lt;script&gt;bad()&lt;/script&gt;&lt;p&gt;Safe&lt;/p&gt;"),5=>xml="not xml".into(),6=>xml="x".repeat(2*1024*1024+1),7=>xml="<rss version=\"2.0\"><channel><title>Empty</title><link>https://example.org</link><description>Empty</description></channel></rss>".into(),8=>xml="<feed xmlns=\"http://www.w3.org/2005/Atom\"><id>feed</id><title>Releases</title><updated>2026-09-04T00:00:00Z</updated><entry><id>one</id><title>Release</title><updated>2026-09-04T00:00:00Z</updated><link href=\"https://example.org/r\"/><summary>Evidence</summary></entry></feed>".into(),9=>xml=rss("Title","https://example.org/r",&"x".repeat(17000)),10=>xml=rss("Title","https://example.org/r?x=1&amp;y=2","Text"),11=>xml=rss("Title","","Text"),_=>{}}
			let result = parse(xml.as_bytes());
			if matches!(case, 5 | 6) {
				assert!(result.is_err(), "case {case}");
				continue;
			}
			let items = result.unwrap();
			assert_eq!(items.len(), if matches!(case, 2 | 3 | 7 | 11) { 0 } else { 1 }, "case {case}");
			if case == 4 {
				assert!(!items[0].1.body.contains("bad()"));
				assert!(items[0].1.body.contains("Safe"))
			}
			if case == 9 {
				assert_eq!(items[0].1.body.chars().count(), 16000)
			}
		}
	}
	#[test]
	fn add_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut profile = 1;
			let mut feed = "rust";
			let mut title = "Releases".to_string();
			let mut url = "https://example.org/rss";
			match case {
				0 => {}
				1 => profile = 99,
				2 => feed = "other",
				3 => title.clear(),
				4 => title = "x".repeat(201),
				5 => url = "file:///bad",
				6 => {
					add(&mut db, 1, &feeds(), feed, &title, url).unwrap();
				}
				7 => {
					initialize(&db).unwrap();
				}
				8 => profile = 2,
				9 => {
					for i in 0..32 {
						add(&mut db, 1, &feeds(), feed, &title, &format!("https://example.org/{i}")).unwrap();
					}
				}
				10 => title = "日本語".into(),
				11 => url = "https://user:pw@example.org/rss",
				_ => {}
			}
			assert_eq!(
				add(&mut db, profile, &feeds(), feed, &title, url).is_ok(),
				matches!(case, 0 | 6 | 7 | 8 | 10),
				"case {case}"
			);
		}
	}
	#[test]
	fn capture_retention_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = add(&mut db, 1, &feeds(), "rust", "Release", "https://example.org/rss").unwrap();
			let mut items = parse(rss("Release", "https://example.org/r", "Walnut").as_bytes()).unwrap();
			let mut profile = 1;
			let mut target = id;
			match case {
				0 => {}
				1 => profile = 2,
				2 => target = 999,
				3 => items.clear(),
				4 => items = vec![items[0].clone(); 101],
				5 => {
					retain(&mut db, 1, id, &items).unwrap();
				}
				6 => {
					initialize(&db).unwrap();
				}
				7 => {
					items[0].1.body = "Edited source".into();
				}
				8 => {
					db.execute("DELETE FROM reading_sources WHERE id=?1", [id]).unwrap();
				}
				9 => {
					for i in 0..210 {
						items[0].0 = i.to_string();
						retain(&mut db, 1, id, &items).unwrap();
					}
				}
				10 => items[0].1.title = "<literal>".into(),
				11 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				_ => {}
			}
			let result = retain(&mut db, profile, target, &items);
			assert_eq!(result.is_ok(), !matches!(case, 1 | 2 | 4 | 8 | 11), "case {case}");
			assert!(entries(&db, 2, "").unwrap().is_empty());
			assert!(entries(&db, 1, "other").unwrap().is_empty());
			assert!(entries(&db, 1, "rust").unwrap().len() <= 200);
		}
	}
	#[test]
	fn refresh_budget_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let id = add(&mut db, 1, &feeds(), "rust", "Release", "https://example.org/rss").unwrap();
			assert!(claim(&mut db, 1, id, 1000).unwrap().is_some());
			let (profile, target, now, expected) = match case {
				0 => (1, id, 1000, 0),
				1 => (1, id, 4599, 0),
				2 => (1, id, 4600, 1),
				3 => (1, id, 4601, 1),
				4 => (2, id, 4600, -1),
				5 => (1, 999, 4600, -1),
				6 => (1, id, -1, -1),
				7 => (1, id, 0, 0),
				8 => (1, id, 999, 0),
				9 => (1, id, i64::MAX, 1),
				10 => {
					initialize(&db).unwrap();
					(1, id, 1000, 0)
				}
				_ => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
					(1, id, 4600, -1)
				}
			};
			let result = claim(&mut db, profile, target, now);
			assert_eq!(
				match result {
					Err(_) => -1,
					Ok(None) => 0,
					Ok(Some(_)) => 1,
				},
				expected,
				"case {case}"
			);
		}
	}
}
