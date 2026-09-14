//! One presentation over bookmarks, retained excerpts, and archives.
use crate::{
	account, archive, library, reading,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use std::collections::{BTreeMap, HashMap};

pub struct Item {
	pub post: String,
	pub title: String,
	pub community: String,
	pub reading: reading::ReadingEntry,
	pub excerpt: Option<library::Saved>,
	pub copy: Option<archive::ArchiveEntryView>,
	pub order: i64,
}
impl Item {
	pub fn is_comment(&self) -> bool {
		self.excerpt.as_ref().is_some_and(|x| !x.comment.is_empty())
	}
	pub fn collection(&self) -> &str {
		self.excerpt.as_ref().map_or("", |x| x.collection.as_str())
	}
	pub fn link(&self) -> String {
		self.excerpt.as_ref().map_or_else(|| format!("/comments/{}#post-top", self.post), |x| x.link())
	}
}

pub fn combine(entries: Vec<reading::ReadingEntry>, copies: Vec<archive::ArchiveEntryView>, excerpts: Vec<library::Saved>) -> Vec<Item> {
	let mut posts = BTreeMap::new();
	for entry in entries {
		if entry.bookmarked {
			posts.insert(
				entry.post_id.clone(),
				Item {
					post: entry.post_id.clone(),
					title: entry.title.clone(),
					community: entry.community.clone(),
					order: entry.updated_at,
					reading: entry,
					copy: None,
					excerpt: None,
				},
			);
		}
	}
	for copy in copies {
		let item = posts.entry(copy.post_id.clone()).or_insert_with(|| Item {
			post: copy.post_id.clone(),
			title: copy.title.clone(),
			community: copy.community.clone(),
			reading: reading::ReadingEntry::default(),
			order: 0,
			copy: None,
			excerpt: None,
		});
		item.copy = Some(copy);
	}
	let mut comments = Vec::new();
	for excerpt in excerpts {
		let item = Item {
			post: excerpt.post.clone(),
			title: excerpt.title.clone(),
			community: excerpt.community.clone(),
			order: excerpt.captured,
			reading: reading::ReadingEntry::default(),
			copy: None,
			excerpt: None,
		};
		if excerpt.comment.is_empty() {
			if excerpt.body.is_empty() && excerpt.note.is_empty() && excerpt.collection.is_empty() && !posts.contains_key(&excerpt.post) {
				continue;
			}
			let post = posts.entry(excerpt.post.clone()).or_insert(item);
			post.order = post.order.max(excerpt.captured);
			post.excerpt = Some(excerpt);
		} else {
			comments.push(Item { excerpt: Some(excerpt), ..item });
		}
	}
	let mut items: Vec<_> = posts.into_values().chain(comments).collect();
	items.sort_by(|a, b| b.order.cmp(&a.order).then_with(|| a.post.cmp(&b.post)));
	items
}

#[derive(Template)]
#[template(path = "saved_unified.html")]
struct Page {
	prefs: Preferences,
	url: String,
	view: String,
	items: Vec<Item>,
	quota: archive::ArchiveQuotaSnapshot,
	q: String,
	collection: String,
	collections: Vec<String>,
	previous: String,
	next: String,
}

// Select only this page before loading retained bodies; the library may hold
// thousands of large excerpts. UNION identifies one post across all stores.
fn page_keys(db: &rusqlite::Connection, profile: i64, view: &str, q: &str, collection: &str, offset: usize) -> rusqlite::Result<Vec<(String, bool, i64)>> {
	let mut stmt = db.prepare(
		"WITH posts AS (
        SELECT post_id AS post FROM reading_entries WHERE profile_id=?1 AND bookmarked=1
        UNION SELECT post_id FROM post_archives WHERE profile_id=?1
        UNION SELECT post FROM reading_library WHERE profile_id=?1 AND comment='' AND (body<>'' OR note<>'' OR collection<>'')
    ), items AS (
        SELECT p.post,0 AS is_comment,coalesce(l.id,0) AS id,
        coalesce(r.bookmarked,0) AS bookmarked,coalesce(r.finished,0) AS finished,
        a.id IS NOT NULL AS has_copy,coalesce(l.collection,'') AS collection,
        coalesce(nullif(r.title,''),a.title,l.title,'') AS title,
        coalesce(nullif(r.community,''),a.community,l.community,'') AS community,
        coalesce(l.body,'') AS body,coalesce(l.context,'') AS context,coalesce(l.note,'') AS note,
        max(coalesce(r.updated_at,0),coalesce(a.created_at,0),coalesce(l.captured,0)) AS recent
        FROM posts p
        LEFT JOIN reading_entries r ON r.profile_id=?1 AND r.post_id=p.post
        LEFT JOIN post_archives a ON a.profile_id=?1 AND a.post_id=p.post
        LEFT JOIN reading_library l ON l.profile_id=?1 AND l.post=p.post AND l.comment=''
        UNION ALL SELECT post,1,id,0,0,0,collection,title,community,body,context,note,captured FROM reading_library WHERE profile_id=?1 AND comment<>''
    ) SELECT post,is_comment,id FROM items
    WHERE (?2='all' OR (?2='later' AND bookmarked=1 AND finished=0) OR (?2='comments' AND is_comment=1) OR (?2='copies' AND has_copy=1))
    AND (?3='' OR instr(lower(title || ' ' || community || ' ' || body || ' ' || context || ' ' || note || ' ' || collection),lower(?3))>0)
    AND (?4='' OR collection=?4)
    ORDER BY recent DESC,post,is_comment,id LIMIT 51 OFFSET ?5",
	)?;
	let rows = stmt.query_map(rusqlite::params![profile, view, q, collection, offset as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
	rows.collect()
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::from("Sign in to view Saved.")).unwrap());
	};
	let params: HashMap<String, String> = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
	let v = |key: &str| params.get(key).map(String::as_str).unwrap_or("");
	let view = if v("view").is_empty() { "all" } else { v("view") };
	let offset = v("offset").parse::<usize>().unwrap_or(0);
	if !["all", "later", "comments", "copies"].contains(&view) || v("q").len() > 256 || v("collection").chars().count() > 80 || offset > 10000 {
		return Ok(
			Response::builder()
				.status(StatusCode::UNPROCESSABLE_ENTITY)
				.body(Body::from("Invalid Saved view."))
				.unwrap(),
		);
	}
	let db = account::open_database()?;
	let mut collection_stmt = db
		.prepare("SELECT DISTINCT collection FROM reading_library WHERE profile_id=?1 AND collection<>'' ORDER BY collection COLLATE NOCASE")
		.map_err(|e| e.to_string())?;
	let collections = collection_stmt
		.query_map([profile], |r| r.get::<_, String>(0))
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let keys = page_keys(&db, profile, view, v("q"), v("collection"), offset).map_err(|e| e.to_string())?;
	let has_more = keys.len() > 50;
	let mut items = Vec::new();
	for (post, comment, id) in keys.into_iter().take(50) {
		let excerpt = if id > 0 { library::get(&db, profile, id).map_err(|e| format!("{e:?}"))? } else { None };
		if comment {
			if let Some(excerpt) = excerpt {
				items.extend(combine(vec![], vec![], vec![excerpt]));
			}
		} else {
			let entry = reading::get(&db, profile, &post).map_err(|e| format!("{e:?}"))?;
			let copy = archive::entry_for_post(&db, profile, &post).map_err(|e| e.to_string())?;
			items.extend(combine(vec![entry], copy.into_iter().collect(), excerpt.into_iter().collect()));
		}
	}
	let page_url = |offset: usize| {
		let mut s = url::form_urlencoded::Serializer::new(String::new());
		s.append_pair("view", view)
			.append_pair("q", v("q"))
			.append_pair("collection", v("collection"))
			.append_pair("offset", &offset.to_string());
		format!("/saved?{}", s.finish())
	};
	let next = if has_more { page_url(offset + 50) } else { String::new() };
	let previous = if offset > 0 { page_url(offset.saturating_sub(50)) } else { String::new() };
	Ok(template(&Page {
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		view: view.into(),
		items,
		quota: archive::quota_snapshot(&req)?,
		q: v("q").into(),
		collection: v("collection").into(),
		collections,
		previous,
		next,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;
	fn bookmark() -> reading::ReadingEntry {
		reading::ReadingEntry {
			post_id: "abc123".into(),
			title: "A retained discussion".into(),
			community: "rust".into(),
			bookmarked: true,
			updated_at: 12,
			..Default::default()
		}
	}
	fn excerpt(comment: &str) -> library::Saved {
		library::Saved {
			id: 7,
			post: "abc123".into(),
			comment: comment.into(),
			title: "A retained discussion".into(),
			community: "rust".into(),
			author: "reader".into(),
			body: "Useful text".into(),
			context: "Parent context".into(),
			captured: 10,
			note: "Keep this".into(),
			collection: "References".into(),
			revision: 1,
		}
	}
	#[test]
	fn saved_pages_are_bounded_deduplicated_and_profile_scoped() {
		let mut db = rusqlite::Connection::open_in_memory().unwrap();
		db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE profiles(id INTEGER PRIMARY KEY); INSERT INTO profiles VALUES(1),(2); CREATE TABLE post_archives(id TEXT PRIMARY KEY,profile_id INTEGER,post_id TEXT,title TEXT,community TEXT,created_at INTEGER);").unwrap();
		reading::initialize(&db).unwrap();
		library::initialize(&db).unwrap();
		for n in 0..55 {
			reading::command(&mut db, 1, &format!("post{n}"), &format!("Title {n}"), "rust", 0, "bookmark", "", n).unwrap();
		}
		let mut note = excerpt("");
		note.post = "post54".into();
		let id = library::save(&mut db, 1, &note).unwrap();
		library::annotate(&mut db, 1, id, 1, "needle in a note", "References", false).unwrap();
		db.execute("INSERT INTO post_archives VALUES('copy',1,'post54','Title 54','rust',54)", []).unwrap();
		assert_eq!(page_keys(&db, 1, "all", "", "", 0).unwrap().len(), 51);
		assert_eq!(page_keys(&db, 1, "all", "", "", 50).unwrap().len(), 5);
		assert_eq!(page_keys(&db, 1, "copies", "", "", 0).unwrap(), vec![("post54".into(), false, id)]);
		assert_eq!(page_keys(&db, 1, "all", "needle", "References", 0).unwrap().len(), 1);
		assert!(page_keys(&db, 2, "all", "", "", 0).unwrap().is_empty());
		let saved = reading::get(&db, 1, "post54").unwrap();
		reading::command(&mut db, 1, "post54", "Title 54", "rust", saved.revision, "finish", "", 60).unwrap();
		assert_eq!(page_keys(&db, 1, "later", "Title 54", "", 0).unwrap().len(), 0);
		assert_eq!(page_keys(&db, 1, "all", "Title 54", "", 0).unwrap().len(), 1);
		let comment = excerpt("def456");
		library::save(&mut db, 1, &comment).unwrap();
		assert_eq!(page_keys(&db, 1, "comments", "", "", 0).unwrap().len(), 1);
	}

	#[test]
	fn merges_post_notes_but_keeps_comments_distinct() {
		let items = combine(vec![bookmark()], vec![], vec![excerpt(""), excerpt("def456")]);
		assert_eq!(items.len(), 2);
		assert!(items[0].reading.bookmarked);
		assert_eq!(items[0].collection(), "References");
		assert!(items[1].is_comment());
		assert!(items[1].link().contains("def456"));
	}
	#[test]
	fn unsaving_does_not_discard_retained_notes() {
		let mut entry = bookmark();
		entry.bookmarked = false;
		let items = combine(vec![entry], vec![], vec![excerpt("")]);
		assert_eq!(items.len(), 1);
		assert!(!items[0].reading.bookmarked);
		assert_eq!(items[0].excerpt.as_ref().unwrap().note, "Keep this");
	}
	#[test]
	fn link_only_unsave_disappears() {
		let mut entry = bookmark();
		entry.bookmarked = false;
		entry.followed = true;
		assert!(combine(vec![entry], vec![], vec![]).is_empty());
	}
}
