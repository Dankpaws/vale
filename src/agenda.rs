//! Explicit topic watches and opt-in finite-edition schedules.
use crate::{
	account,
	reading::WriteError,
	utils::{template, Preferences},
};
use askama::Template;
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
#[derive(Clone, Debug)]
pub struct Topic {
	pub id: i64,
	pub feed: String,
	pub phrase: String,
	pub baseline: i64,
	pub until: i64,
	pub revision: i64,
}
#[derive(Clone, Debug)]
pub struct Match {
	pub topic: i64,
	pub title: String,
	pub community: String,
	pub post: String,
}
#[derive(Clone, Debug)]
pub struct Schedule {
	pub id: i64,
	pub feed: String,
	pub minutes: i64,
	pub interval: i64,
	pub next: i64,
	pub error: String,
	pub revision: i64,
}
impl Schedule {
	pub fn next_label(&self) -> String {
		chrono::DateTime::from_timestamp(self.next, 0)
			.map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
			.unwrap_or_default()
	}
}
pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch("CREATE TABLE IF NOT EXISTS reading_topics(id INTEGER PRIMARY KEY AUTOINCREMENT,profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,feed TEXT NOT NULL,phrase TEXT NOT NULL,baseline INTEGER NOT NULL DEFAULT 0,until_at INTEGER NOT NULL DEFAULT 0,revision INTEGER NOT NULL DEFAULT 1,UNIQUE(profile_id,feed,phrase));CREATE TABLE IF NOT EXISTS reading_topic_matches(id INTEGER PRIMARY KEY AUTOINCREMENT,topic INTEGER NOT NULL REFERENCES reading_topics(id) ON DELETE CASCADE,post TEXT NOT NULL,title TEXT NOT NULL,community TEXT NOT NULL,UNIQUE(topic,post));CREATE TABLE IF NOT EXISTS reading_schedules(id INTEGER PRIMARY KEY AUTOINCREMENT,profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,feed TEXT NOT NULL,minutes INTEGER NOT NULL,interval_seconds INTEGER NOT NULL,next_at INTEGER NOT NULL,error TEXT NOT NULL DEFAULT '',revision INTEGER NOT NULL DEFAULT 1,UNIQUE(profile_id,feed));")
}
pub fn add_topic(db: &mut Connection, profile: i64, feed: &str, phrase: &str) -> Result<(), WriteError> {
	let phrase = phrase.trim().to_lowercase();
	if feed.is_empty() || feed.len() > 80 || phrase.is_empty() || phrase.chars().count() > 160 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if tx.query_row("SELECT count(*) FROM reading_topics WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 50 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT OR IGNORE INTO reading_topics(profile_id,feed,phrase) VALUES(?1,?2,?3)",
		params![profile, feed, phrase],
	)?;
	tx.commit()?;
	Ok(())
}
pub fn topics(db: &Connection, profile: i64) -> Result<Vec<Topic>, WriteError> {
	let mut stmt = db.prepare("SELECT id,feed,phrase,baseline,until_at,revision FROM reading_topics WHERE profile_id=?1 ORDER BY id")?;
	let rows = stmt.query_map([profile], |r| {
		Ok(Topic {
			id: r.get(0)?,
			feed: r.get(1)?,
			phrase: r.get(2)?,
			baseline: r.get(3)?,
			until: r.get(4)?,
			revision: r.get(5)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
pub fn observe(db: &Connection, profile: i64, feed: &str, items: &[crate::editions::Item], now: i64) -> Result<(), WriteError> {
	if items.len() > 25 || now < 0 {
		return Err(WriteError::Invalid);
	}
	for topic in topics(db, profile)?.into_iter().filter(|t| t.feed == feed && t.until <= now) {
		for item in items {
			if item.title.to_lowercase().contains(&topic.phrase) && account::valid_post_id(&item.id) && item.title.len() <= 4000 && item.community.len() <= 80 {
				db.execute(
					"INSERT OR IGNORE INTO reading_topic_matches(topic,post,title,community) VALUES(?1,?2,?3,?4)",
					params![topic.id, item.id, item.title, item.community],
				)?;
			}
		}
		db.execute(
			"DELETE FROM reading_topic_matches WHERE topic=?1 AND id NOT IN(SELECT id FROM reading_topic_matches WHERE topic=?1 ORDER BY id DESC LIMIT 500)",
			[topic.id],
		)?;
	}
	Ok(())
}
pub fn topic_command(db: &mut Connection, profile: i64, id: i64, revision: i64, action: &str, now: i64) -> Result<(), WriteError> {
	if now < 0 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let topic = topics(&tx, profile)?.into_iter().find(|t| t.id == id).ok_or(WriteError::Invalid)?;
	if revision != topic.revision {
		return Err(WriteError::Conflict);
	}
	match action {
		"ack" => {
			tx.execute(
				"UPDATE reading_topics SET baseline=coalesce((SELECT max(id) FROM reading_topic_matches WHERE topic=?1),0),revision=revision+1 WHERE id=?1",
				[id],
			)?;
		}
		"snooze" | "resume" => {
			let until = if action == "resume" { 0 } else { now.checked_add(86400).ok_or(WriteError::Invalid)? };
			tx.execute("UPDATE reading_topics SET until_at=?2,revision=revision+1 WHERE id=?1", params![id, until])?;
		}
		"remove" => {
			tx.execute("DELETE FROM reading_topics WHERE id=?1", [id])?;
		}
		_ => return Err(WriteError::Invalid),
	}
	tx.commit()?;
	Ok(())
}
pub fn schedule(db: &mut Connection, profile: i64, feed: &str, minutes: i64, interval: i64, now: i64) -> Result<(), WriteError> {
	if feed.is_empty() || feed.len() > 80 || !matches!(minutes, 5 | 10 | 20) || !matches!(interval, 21600 | 86400) || now < 0 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	if tx.query_row("SELECT count(*) FROM reading_schedules WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 10 {
		return Err(WriteError::Full);
	}
	tx.execute(
		"INSERT INTO reading_schedules(profile_id,feed,minutes,interval_seconds,next_at) VALUES(?1,?2,?3,?4,?5)",
		params![profile, feed, minutes, interval, now],
	)?;
	tx.commit()?;
	Ok(())
}
fn schedules(db: &Connection, profile: i64) -> Result<Vec<Schedule>, WriteError> {
	let mut stmt = db.prepare("SELECT id,feed,minutes,interval_seconds,next_at,error,revision FROM reading_schedules WHERE profile_id=?1 ORDER BY id")?;
	let rows = stmt.query_map([profile], |r| {
		Ok(Schedule {
			id: r.get(0)?,
			feed: r.get(1)?,
			minutes: r.get(2)?,
			interval: r.get(3)?,
			next: r.get(4)?,
			error: r.get(5)?,
			revision: r.get(6)?,
		})
	})?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
type ClaimedSchedule = (i64, i64, String, i64, Preferences);

fn claim_schedule(db: &mut Connection, now: i64) -> Result<Option<ClaimedSchedule>, WriteError> {
	if now < 0 {
		return Err(WriteError::Invalid);
	}
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let row=tx.query_row("SELECT s.id,s.profile_id,s.feed,s.minutes,s.interval_seconds,p.preferences_json FROM reading_schedules s JOIN profiles p ON p.id=s.profile_id LEFT JOIN users u ON u.id=p.user_id WHERE s.next_at<=?1 AND u.disabled_at IS NULL ORDER BY s.next_at,s.id LIMIT 1",[now],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?))).optional()?;
	let Some((id, profile, feed, minutes, interval, prefs)) = row else { return Ok(None) };
	let next = now.checked_add(interval).ok_or(WriteError::Invalid)?;
	tx.execute("UPDATE reading_schedules SET next_at=?2 WHERE id=?1", params![id, next])?;
	tx.commit()?;
	let prefs = serde_json::from_str(&prefs).map_err(|_| WriteError::Invalid)?;
	Ok(Some((id, profile, feed, minutes, prefs)))
}
pub async fn worker() {
	if account::mode() == account::ProfileMode::Browser {
		return;
	}
	loop {
		tokio::time::sleep(std::time::Duration::from_secs(60)).await;
		let candidate = account::open_database()
			.map_err(WriteError::Database)
			.and_then(|mut db| claim_schedule(&mut db, account::now()));
		if let Ok(Some((id, profile, feed, minutes, prefs))) = candidate {
			let encoded = {
				let mut serializer = url::form_urlencoded::Serializer::new(String::new());
				serializer
					.append_pair("action", "build")
					.append_pair("feed", &feed)
					.append_pair("minutes", &minutes.to_string());
				serializer.finish()
			};
			let mut req = Request::builder().method("POST").uri("/reading/editions").body(Body::from(encoded)).unwrap();
			req.extensions_mut().insert(account::AuthContext {
				profile_id: profile,
				user_id: None,
				username: String::new(),
				display_name: String::new(),
				is_admin: false,
				session_hash: None,
				preferences: prefs,
			});
			let result = crate::editions::mutate(req).await;
			let success = result.is_ok_and(|r| r.status() == StatusCode::SEE_OTHER);
			if let Ok(db) = account::open_database() {
				let _ = db.execute(
					"UPDATE reading_schedules SET error=?2 WHERE id=?1",
					params![
						id,
						if success {
							""
						} else {
							"Edition was not created. Check feed membership, saved capacity, or recent manual requests."
						}
					],
				);
			}
		}
	}
}
#[derive(Template)]
#[template(path = "agenda.html")]
struct Page {
	prefs: Preferences,
	url: String,
	topics: Vec<Topic>,
	matches: Vec<Match>,
	schedules: Vec<Schedule>,
	now: i64,
}
pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to manage watches."));
	};
	let db = account::open_database()?;
	let topics = topics(&db, profile).map_err(|e| format!("{e:?}"))?;
	let mut stmt=db.prepare("SELECT m.topic,m.title,m.community,m.post FROM reading_topic_matches m JOIN reading_topics t ON t.id=m.topic WHERE t.profile_id=?1 AND m.id>t.baseline ORDER BY m.id DESC LIMIT 200").map_err(|e|e.to_string())?;
	let matches = stmt
		.query_map([profile], |r| {
			Ok(Match {
				topic: r.get(0)?,
				title: r.get(1)?,
				community: r.get(2)?,
				post: r.get(3)?,
			})
		})
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let schedules = schedules(&db, profile).map_err(|e| format!("{e:?}"))?;
	Ok(template(&Page {
		prefs: Preferences::new(&req),
		url: req.uri().to_string(),
		topics,
		matches,
		schedules,
		now: account::now(),
	}))
}
pub fn stop_schedule(db: &Connection, profile: i64, id: i64, revision: i64) -> Result<(), WriteError> {
	let changed = db.execute(
		"DELETE FROM reading_schedules WHERE profile_id=?1 AND id=?2 AND revision=?3",
		params![profile, id, revision],
	)?;
	if changed == 1 {
		Ok(())
	} else {
		Err(WriteError::Conflict)
	}
}
pub async fn mutate(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(reply(StatusCode::UNAUTHORIZED, "Sign in to manage watches."));
	};
	let prefs = Preferences::new(&req);
	let bytes = crate::utils::read_body_limited(req.body_mut(), 4096, "Watch form too large.").await?;
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&bytes).into_owned().collect();
	let v = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
	let mut db = account::open_database()?;
	let result = match v("action") {
		"topic" | "schedule" => {
			if !prefs.feed_groups().iter().any(|f| f.slug == v("feed")) {
				Err(WriteError::Invalid)
			} else if v("action") == "topic" {
				add_topic(&mut db, profile, v("feed"), v("phrase"))
			} else {
				schedule(
					&mut db,
					profile,
					v("feed"),
					v("minutes").parse().unwrap_or(0),
					v("interval").parse().unwrap_or(0),
					account::now(),
				)
			}
		}
		"stop-schedule" => stop_schedule(&db, profile, v("id").parse().unwrap_or(0), v("revision").parse().unwrap_or(-1)),
		action => topic_command(&mut db, profile, v("id").parse().unwrap_or(0), v("revision").parse().unwrap_or(-1), action, account::now()),
	};
	if result.is_err() {
		return Ok(reply(
			StatusCode::CONFLICT,
			"The watch changed, already exists, or has invalid settings. Reload to review it.",
		));
	}
	Ok(
		Response::builder()
			.status(StatusCode::SEE_OTHER)
			.header("location", "/reading/agenda")
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
		account::initialize_schema(&db).unwrap();
		let prefs = serde_json::to_string(&Preferences::default()).unwrap();
		for id in [1, 2] {
			db.execute(
				"INSERT INTO profiles(id,label,preferences_json,created_at,updated_at) VALUES(?1,'Synthetic',?2,0,0)",
				params![id, prefs],
			)
			.unwrap();
		}
		db
	}
	fn item() -> crate::editions::Item {
		crate::editions::Item {
			id: "post1".into(),
			title: "Rust release".into(),
			community: "rust".into(),
			author: "reader".into(),
			excerpt: String::new(),
			key: String::new(),
			created: 1000,
		}
	}
	#[test]
	fn stop_schedule_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			schedule(&mut db, 1, "rust", 5, 86400, 1000).unwrap();
			let id = schedules(&db, 1).unwrap()[0].id;
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
					stop_schedule(&db, 1, id, 1).unwrap();
				}
				10 => {
					initialize(&db).unwrap();
				}
				11 => {
					claim_schedule(&mut db, 1000).unwrap();
				}
				_ => {}
			}
			assert_eq!(stop_schedule(&db, profile, target, rev).is_ok(), matches!(case, 0 | 10 | 11), "case {case}");
		}
	}
	#[test]
	fn topic_creation_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut profile = 1;
			let mut feed = "rust".to_string();
			let mut phrase = "release".to_string();
			match case {
				0 => {}
				1 => profile = 99,
				2 => feed.clear(),
				3 => feed = "x".repeat(81),
				4 => phrase.clear(),
				5 => phrase = " ".into(),
				6 => phrase = "x".repeat(161),
				7 => {
					add_topic(&mut db, 1, &feed, &phrase).unwrap();
				}
				8 => {
					initialize(&db).unwrap();
				}
				9 => phrase = "日本語".into(),
				10 => {
					for i in 0..50 {
						add_topic(&mut db, 1, "rust", &format!("old{i}")).unwrap();
					}
				}
				11 => phrase = " RELEASE ".into(),
				_ => {}
			}
			assert_eq!(add_topic(&mut db, profile, &feed, &phrase).is_ok(), matches!(case, 0 | 7 | 8 | 9 | 11), "case {case}");
		}
	}
	#[test]
	fn topic_observation_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			add_topic(&mut db, 1, "rust", "release").unwrap();
			let topic = topics(&db, 1).unwrap()[0].clone();
			let mut profile = 1;
			let mut feed = "rust";
			let mut items = vec![item()];
			let mut now = 1000;
			match case {
				0 => items.clear(),
				1 => profile = 2,
				2 => feed = "other",
				3 => items[0].title = "Different".into(),
				4 => items[0].title = "RELEASE".into(),
				5 => {
					observe(&db, 1, "rust", &items, 1000).unwrap();
				}
				6 => items = vec![item(); 26],
				7 => now = -1,
				8 => {
					topic_command(&mut db, 1, topic.id, 1, "snooze", 1000).unwrap();
				}
				9 => {
					topic_command(&mut db, 1, topic.id, 1, "snooze", 1000).unwrap();
					now = 87400;
				}
				10 => items[0].id = "../bad".into(),
				11 => {
					for i in 0..510 {
						items[0].id = format!("post{i}");
						observe(&db, 1, "rust", &items, 1000).unwrap();
					}
				}
				_ => {}
			}
			let result = observe(&db, profile, feed, &items, now);
			assert_eq!(result.is_ok(), !matches!(case, 6 | 7), "case {case}");
			let count = db.query_row("SELECT count(*) FROM reading_topic_matches", [], |r| r.get::<_, i64>(0)).unwrap();
			assert_eq!(
				count,
				match case {
					4 | 5 | 9 => 1,
					11 => 500,
					_ => 0,
				},
				"case {case}"
			);
		}
	}
	fn command_edges(action: &str) {
		for case in 0..12 {
			let mut db = db();
			add_topic(&mut db, 1, "rust", "release").unwrap();
			let id = topics(&db, 1).unwrap()[0].id;
			let mut profile = 1;
			let mut target = id;
			let mut rev = 1;
			let mut now = 1000;
			match case {
				0 => {}
				1 => profile = 2,
				2 => target = 999,
				3 => rev = 0,
				4 => rev = -1,
				5 => now = -1,
				6 => {
					topic_command(&mut db, 1, id, 1, "ack", 1000).unwrap();
				}
				7 => {
					initialize(&db).unwrap();
				}
				8 => {
					observe(&db, 1, "rust", &[item()], 1000).unwrap();
				}
				9 => now = i64::MAX,
				10 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				11 => now = 0,
				_ => {}
			}
			let ok = matches!(case, 0 | 7 | 8 | 11) || (case == 9 && action != "snooze");
			assert_eq!(topic_command(&mut db, profile, target, rev, action, now).is_ok(), ok, "{action} case {case}");
		}
	}
	#[test]
	fn topic_ack_twelve_edges() {
		command_edges("ack")
	}
	#[test]
	fn topic_snooze_twelve_edges() {
		command_edges("snooze")
	}
	#[test]
	fn topic_resume_twelve_edges() {
		command_edges("resume")
	}
	#[test]
	fn topic_remove_twelve_edges() {
		command_edges("remove")
	}
	#[test]
	fn scheduling_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			let mut profile = 1;
			let mut feed = "rust".to_string();
			let mut minutes = 5;
			let mut interval = 86400;
			let mut now = 1000;
			match case {
				0 => {}
				1 => profile = 99,
				2 => feed.clear(),
				3 => feed = "x".repeat(81),
				4 => minutes = 0,
				5 => minutes = 20,
				6 => interval = 0,
				7 => interval = 21600,
				8 => now = -1,
				9 => {
					schedule(&mut db, 1, "rust", 5, 86400, 1000).unwrap();
				}
				10 => {
					for i in 0..10 {
						schedule(&mut db, 1, &format!("feed{i}"), 5, 86400, 1000).unwrap();
					}
				}
				11 => {
					initialize(&db).unwrap();
				}
				_ => {}
			}
			assert_eq!(
				schedule(&mut db, profile, &feed, minutes, interval, now).is_ok(),
				matches!(case, 0 | 5 | 7 | 11),
				"case {case}"
			);
		}
	}
	#[test]
	fn scheduler_claim_twelve_edges() {
		for case in 0..12 {
			let mut db = db();
			schedule(&mut db, 1, "rust", 5, 86400, 1000).unwrap();
			let mut now = 1000;
			match case {
				0 => {}
				1 => now = 999,
				2 => now = -1,
				3 => {
					claim_schedule(&mut db, 1000).unwrap();
				}
				4 => {
					claim_schedule(&mut db, 1000).unwrap();
					now = 87400;
				}
				5 => {
					db.execute("DELETE FROM profiles WHERE id=1", []).unwrap();
				}
				6 => {
					db.execute("DELETE FROM reading_schedules", []).unwrap();
				}
				7 => {
					initialize(&db).unwrap();
				}
				8 => {
					db.execute("UPDATE profiles SET preferences_json='invalid' WHERE id=1", []).unwrap();
				}
				9 => {
					schedule(&mut db, 2, "other", 10, 21600, 1000).unwrap();
				}
				10 => now = i64::MAX,
				11 => now = 1001,
				_ => {}
			}
			let result = claim_schedule(&mut db, now);
			assert_eq!(result.is_ok(), !matches!(case, 2 | 8 | 10), "case {case}");
			if let Ok(result) = result {
				assert_eq!(result.is_some(), matches!(case, 0 | 4 | 7 | 9 | 11), "case {case}");
			}
		}
	}
}
