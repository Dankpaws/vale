//! Feed-scoped reading choices, rendered from a bounded local snapshot.
use crate::{
	account, reading,
	utils::{template, Preferences},
	watch,
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
const RECENT_PLACE_SECONDS: i64 = 7 * 86400;
const STALE_CHECK_SECONDS: i64 = 3600;

pub struct Update {
	pub entry: reading::ReadingEntry,
	pub state: watch::WatchState,
	pub through: i64,
	pub first: i64,
	pub feed: String,
}
impl Update {
	pub fn replies_url(&self) -> String {
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query
			.append_pair("post", &self.entry.post_id)
			.append_pair("through", &self.through.to_string())
			.append_pair("baseline", &self.state.baseline.to_string());
		if !self.feed.is_empty() {
			query.append_pair("feed", &self.feed);
		}
		format!("/reading/watch?{}", query.finish())
	}
}
pub struct EditionLink {
	pub id: i64,
	pub name: String,
	pub position: i64,
}
#[derive(Template)]
#[template(path = "reading_home.html")]
struct Home {
	prefs: Preferences,
	url: String,
	feed: String,
	overview: bool,
	resume: Option<reading::ReadingEntry>,
	rows: Vec<Update>,
	more: bool,
	empty_label: String,
	notice: String,
	edition: Option<EditionLink>,
	previous: String,
	next: String,
}
fn visible(entry: &reading::ReadingEntry, prefs: &Preferences, communities: Option<&[String]>) -> bool {
	!prefs.filters.iter().any(|f| f.eq_ignore_ascii_case(&entry.community)) && communities.is_none_or(|list| list.iter().any(|c| c.eq_ignore_ascii_case(&entry.community)))
}
fn recent_place(entries: &[reading::ReadingEntry], prefs: &Preferences, communities: &[String], now: i64) -> Option<reading::ReadingEntry> {
	entries
		.iter()
		.filter(|entry| {
			!entry.anchor.is_empty()
				&& entry.place_kept_at > 0
				&& entry.place_kept_at <= now
				&& now.saturating_sub(entry.place_kept_at) <= RECENT_PLACE_SECONDS
				&& visible(entry, prefs, Some(communities))
		})
		.max_by(|a, b| a.place_kept_at.cmp(&b.place_kept_at).then_with(|| b.post_id.cmp(&a.post_id)))
		.cloned()
}
fn edition(db: &Connection, profile: i64, feed: &str, now: i64) -> rusqlite::Result<Option<EditionLink>> {
	db.query_row("SELECT id,name,position FROM reading_editions WHERE profile_id=?1 AND feed=?2 AND complete=0 AND created>=?3 AND json_array_length(items)>0 ORDER BY created DESC,id DESC LIMIT 1",params![profile,feed,now.saturating_sub(RECENT_PLACE_SECONDS)],|r|Ok(EditionLink{id:r.get(0)?,name:r.get(1)?,position:r.get(2)?})).optional()
}
pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::from("Sign in to use Reading.")).unwrap());
	};
	let args: HashMap<String, String> = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
	let overview = args.get("list").is_none_or(|v| v.is_empty());
	let prefs = Preferences::new(&req);
	let feeds = prefs.feed_groups();
	let feed = args.get("feed").cloned().unwrap_or_else(|| {
		if overview {
			feeds
				.iter()
				.find(|f| f.slug == prefs.active_feed)
				.or_else(|| feeds.first())
				.map(|f| f.slug.clone())
				.unwrap_or_default()
		} else {
			String::new()
		}
	});
	let selected = feeds.iter().find(|f| f.slug == feed);
	if !feed.is_empty() && selected.is_none() {
		return Ok(Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("Feed not found.")).unwrap());
	}
	let offset = args.get("offset").map(|s| s.parse::<usize>().unwrap_or(usize::MAX)).unwrap_or(0);
	if offset > 5000 {
		return Ok(
			Response::builder()
				.status(StatusCode::UNPROCESSABLE_ENTITY)
				.body(Body::from("Invalid reading page."))
				.unwrap(),
		);
	}
	let now = account::now();
	let connection = account::open_database()?;
	let db = connection.unchecked_transaction().map_err(|e| e.to_string())?;
	let entries = reading::list(&db, profile)?;
	let communities = selected.map(|f| f.communities.as_slice());
	let resume = if overview {
		communities.and_then(|c| recent_place(&entries, &prefs, c, now))
	} else {
		None
	};
	let edition = if overview && !feed.is_empty() {
		edition(&db, profile, &feed, now).map_err(|e| e.to_string())?
	} else {
		None
	};
	let mut rows = Vec::new();
	let mut failed = 0;
	let mut partial = 0;
	let mut snoozed = 0;
	let mut pending = 0;
	for entry in entries.into_iter().filter(|e| e.followed && visible(e, &prefs, communities)) {
		if overview && communities.is_none() {
			continue;
		}
		let (state, through, first) = watch::summary(&db, profile, &entry.post_id, &prefs).map_err(|e| format!("{e:?}"))?;
		if state.snoozed() {
			snoozed += 1;
		} else if !state.error.is_empty() && state.error != "Awaiting first capture" {
			failed += 1;
		} else if state.last_success == 0 || now.saturating_sub(state.last_success) > STALE_CHECK_SECONDS {
			pending += 1;
		} else if !state.complete {
			partial += 1;
		}
		rows.push(Update {
			entry,
			state,
			through,
			first,
			feed: feed.clone(),
		});
	}
	let total = rows.len();
	let empty_label = if overview && communities.is_none() {
		"Choose a feed to start reading."
	} else if total == 0 {
		"No followed discussions in this view."
	} else if snoozed == total {
		"Your followed discussions are snoozed."
	} else if failed + pending > 0 {
		"No confirmed updates to show."
	} else if partial > 0 {
		"No changes found in the replies retrieved."
	} else {
		"No changes found in the latest checks."
	}
	.to_string();
	let mut notices = Vec::new();
	if failed > 0 {
		notices.push(format!("{failed} {} could not be updated.", if failed == 1 { "discussion" } else { "discussions" }));
	}
	if pending > 0 {
		notices.push(format!(
			"{pending} {} a successful recent check.",
			if pending == 1 { "discussion needs" } else { "discussions need" }
		));
	}
	if partial > 0 {
		notices.push(format!("{partial} {} partial coverage.", if partial == 1 { "discussion has" } else { "discussions have" }));
	}
	if snoozed > 0 {
		notices.push(format!("{snoozed} snoozed."));
	}
	if overview {
		rows.retain(|row| row.state.new_count > 0 && !row.state.snoozed());
		rows.sort_by(|a, b| a.first.cmp(&b.first).then_with(|| a.entry.post_id.cmp(&b.entry.post_id)));
	} else {
		rows.sort_by(|a, b| {
			a.entry
				.title
				.to_lowercase()
				.cmp(&b.entry.title.to_lowercase())
				.then_with(|| a.entry.post_id.cmp(&b.entry.post_id))
		});
	}
	let page_url = |offset: usize| {
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query.append_pair("list", "following").append_pair("feed", &feed).append_pair("offset", &offset.to_string());
		format!("/reading?{}", query.finish())
	};
	let more = overview && rows.len() > 6;
	let next = if !overview && rows.len() > offset + 25 { page_url(offset + 25) } else { String::new() };
	let previous = if !overview && offset > 0 { page_url(offset.saturating_sub(25)) } else { String::new() };
	let rows = if overview {
		rows.into_iter().take(6).collect()
	} else {
		rows.into_iter().skip(offset).take(25).collect()
	};
	Ok(template(&Home {
		prefs,
		url: req.uri().to_string(),
		feed,
		overview,
		resume,
		rows,
		more,
		empty_label,
		notice: notices.join(" "),
		edition,
		previous,
		next,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn recent_place_uses_explicit_time_and_current_membership() {
		let prefs = Preferences::default();
		let now = 1_000_000;
		let mut old = reading::ReadingEntry {
			post_id: "old".into(),
			community: "rust".into(),
			anchor: "a".into(),
			place_kept_at: 1,
			updated_at: now,
			..Default::default()
		};
		let current = reading::ReadingEntry {
			post_id: "current".into(),
			place_kept_at: now - 100,
			..old.clone()
		};
		let other = reading::ReadingEntry {
			post_id: "other".into(),
			community: "cooking".into(),
			place_kept_at: now,
			..old.clone()
		};
		let entries = vec![old.clone(), current.clone(), other];
		assert_eq!(recent_place(&entries, &prefs, &["RUST".into()], now).unwrap().post_id, "current");
		assert!(recent_place(&entries, &prefs, &["missing".into()], now).is_none());
		old.place_kept_at = 0;
		assert!(recent_place(&[old], &prefs, &["rust".into()], now).is_none());
		let mut hidden = prefs;
		hidden.filters = vec!["rust".into()];
		assert!(recent_place(&[current], &hidden, &["rust".into()], now).is_none());
	}
}
