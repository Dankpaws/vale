//! Explicit offline packs and idempotent replay of revisioned user intents.
use crate::{account, reading::WriteError};
use hyper::{Body, Request, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
	db.execute_batch("CREATE TABLE IF NOT EXISTS offline_receipts(profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,operation TEXT NOT NULL,digest TEXT NOT NULL,PRIMARY KEY(profile_id,operation));")
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
	pub token: String,
	pub kind: String,
	pub id: i64,
	pub revision: i64,
	#[serde(default)]
	pub note: String,
	#[serde(default)]
	pub collection: String,
	#[serde(default)]
	pub position: i64,
}
pub fn replay(db: &mut Connection, profile: i64, op: &Operation) -> Result<(), WriteError> {
	if uuid::Uuid::parse_str(&op.token).is_err()
		|| op.note.chars().count() > 8192
		|| op.collection.chars().count() > 80
		|| op.collection.contains(['\n', '\r', '\0'])
		|| op.revision < 1
	{
		return Err(WriteError::Invalid);
	}
	let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(op).map_err(|_| WriteError::Invalid)?));
	let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let previous: Option<String> = tx
		.query_row(
			"SELECT digest FROM offline_receipts WHERE profile_id=?1 AND operation=?2",
			params![profile, op.token],
			|r| r.get(0),
		)
		.optional()?;
	if let Some(previous) = previous {
		return if previous == digest { Ok(()) } else { Err(WriteError::Conflict) };
	}
	if tx.query_row("SELECT count(*) FROM offline_receipts WHERE profile_id=?1", [profile], |r| r.get::<_, i64>(0))? >= 10000 {
		return Err(WriteError::Full);
	}
	match op.kind.as_str() {
		"note" => {
			let item = crate::library::get(&tx, profile, op.id)?.ok_or(WriteError::Invalid)?;
			if item.revision != op.revision {
				return Err(WriteError::Conflict);
			}
			tx.execute(
				"UPDATE reading_library SET note=?3,collection=?4,revision=revision+1 WHERE profile_id=?1 AND id=?2",
				params![profile, op.id, op.note, op.collection.trim()],
			)?;
		}
		"place" | "complete" => {
			let e = crate::editions::get(&tx, profile, op.id)?.ok_or(WriteError::Invalid)?;
			if e.revision != op.revision {
				return Err(WriteError::Conflict);
			}
			if op.position < 0 || op.position as usize > e.items.len() {
				return Err(WriteError::Invalid);
			}
			tx.execute(
				"UPDATE reading_editions SET position=?3,complete=?4,revision=revision+1 WHERE profile_id=?1 AND id=?2",
				params![profile, op.id, op.position, op.kind == "complete"],
			)?;
		}
		_ => return Err(WriteError::Invalid),
	}
	tx.execute(
		"INSERT INTO offline_receipts(profile_id,operation,digest) VALUES(?1,?2,?3)",
		params![profile, op.token, digest],
	)?;
	tx.commit()?;
	Ok(())
}
pub async fn catalog(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(json(StatusCode::UNAUTHORIZED, serde_json::json!({"error":"Sign in first"})));
	};
	let db = account::open_database()?;
	let mut stmt = db
		.prepare("SELECT id,name,created FROM reading_editions WHERE profile_id=?1 ORDER BY id DESC LIMIT 100")
		.map_err(|e| e.to_string())?;
	let editions = stmt
		.query_map([profile], |r| {
			Ok(serde_json::json!({"id":r.get::<_,i64>(0)?,"name":r.get::<_,String>(1)?,"created":r.get::<_,i64>(2)?}))
		})
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let mut stmt = db
		.prepare("SELECT id,title,collection FROM reading_library WHERE profile_id=?1 ORDER BY id DESC LIMIT 2000")
		.map_err(|e| e.to_string())?;
	let items = stmt
		.query_map([profile], |r| {
			Ok(serde_json::json!({"id":r.get::<_,i64>(0)?,"title":r.get::<_,String>(1)?,"collection":r.get::<_,String>(2)?}))
		})
		.map_err(|e| e.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| e.to_string())?;
	let archives = crate::archive::recent_for_profile(&req, 100)?
		.into_iter()
		.filter(|a| a.is_viewable())
		.map(|a| serde_json::json!({"id":a.id,"title":a.title}))
		.collect::<Vec<_>>();
	Ok(json(
		StatusCode::OK,
		serde_json::json!({"owner":profile.to_string(),"editions":editions,"items":items,"archives":archives}),
	))
}
pub async fn data(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(json(StatusCode::UNAUTHORIZED, serde_json::json!({"error":"Sign in first"})));
	};
	let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
	let db = account::open_database()?;
	let ids = form.get("items").map(String::as_str).unwrap_or("").split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>();
	if ids.len() > 100 {
		return Ok(json(StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"error":"Choose at most 100 saved items"})));
	}
	let mut items = Vec::new();
	for id in ids {
		let Some(item) = crate::library::get(&db, profile, id.parse().unwrap_or(0)).map_err(|e| format!("{e:?}"))? else {
			return Ok(json(StatusCode::NOT_FOUND, serde_json::json!({"error":"Saved item unavailable"})));
		};
		items.push(item);
	}
	let edition = if let Some(id) = form.get("edition").filter(|s| !s.is_empty()) {
		let Some(e) = crate::editions::get(&db, profile, id.parse().unwrap_or(0)).map_err(|e| format!("{e:?}"))? else {
			return Ok(json(StatusCode::NOT_FOUND, serde_json::json!({"error":"Edition unavailable"})));
		};
		Some(serde_json::json!({"id":e.id,"name":e.name,"items":e.items,"position":e.position,"complete":e.complete,"revision":e.revision,"coverage":e.coverage}))
	} else {
		None
	};
	let value = serde_json::json!({"format":"vale-offline-v1","owner":profile.to_string(),"created":account::now(),"edition":edition,"items":items,"archives":[],"queue":[]});
	if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() > 8 * 1024 * 1024 {
		return Ok(json(
			StatusCode::UNPROCESSABLE_ENTITY,
			serde_json::json!({"error":"Select fewer items; text packs are limited to 8 MiB"}),
		));
	}
	Ok(json(StatusCode::OK, value))
}
pub async fn sync(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile) = account::context(&req).map(|c| c.profile_id) else {
		return Ok(json(StatusCode::UNAUTHORIZED, serde_json::json!({"error":"Sign in first"})));
	};
	#[derive(Deserialize)]
	struct Batch {
		owner: String,
		operations: Vec<Operation>,
	}
	let bytes = crate::utils::read_body_limited(req.body_mut(), 8 * 1024 * 1024, "Offline queue too large.").await?;
	let batch: Batch = match serde_json::from_slice(&bytes) {
		Ok(b) => b,
		Err(_) => return Ok(json(StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"error":"Invalid queue"}))),
	};
	if batch.owner != profile.to_string() {
		return Ok(json(StatusCode::CONFLICT, serde_json::json!({"error":"Sign in to the profile that created this pack"})));
	}
	if batch.operations.len() > 100 {
		return Ok(json(StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"error":"Queue limit exceeded"})));
	}
	let mut db = account::open_database()?;
	let mut accepted = Vec::new();
	let mut conflicts = Vec::new();
	let mut blocked = std::collections::HashSet::new();
	for op in batch.operations {
		let target = (if op.kind == "note" { "note" } else { "edition" }, op.id);
		if blocked.contains(&target) {
			conflicts.push(op.token);
			continue;
		}
		match replay(&mut db, profile, &op) {
			Ok(()) => accepted.push(op.token),
			Err(_) => {
				blocked.insert(target);
				conflicts.push(op.token)
			}
		}
	}
	Ok(json(StatusCode::OK, serde_json::json!({"accepted":accepted,"conflicts":conflicts})))
}
fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("content-type", "application/json")
		.header("cache-control", "private, no-store")
		.body(Body::from(value.to_string()))
		.unwrap()
}

#[cfg(test)]
mod tests {
	use super::*;
	fn db() -> (Connection, i64) {
		let mut db = Connection::open_in_memory().unwrap();
		account::initialize_schema(&db).unwrap();
		let prefs = serde_json::to_string(&crate::utils::Preferences::default()).unwrap();
		for id in [1, 2] {
			db.execute(
				"INSERT INTO profiles(id,label,preferences_json,created_at,updated_at) VALUES(?1,'Synthetic',?2,0,0)",
				params![id, prefs],
			)
			.unwrap();
		}
		let item = crate::library::Saved {
			id: 0,
			post: "post1".into(),
			comment: "reply1".into(),
			title: "Title".into(),
			community: "rust".into(),
			author: "reader".into(),
			body: "Evidence".into(),
			context: String::new(),
			captured: 1000,
			note: String::new(),
			collection: String::new(),
			revision: 0,
		};
		let id = crate::library::save(&mut db, 1, &item).unwrap();
		(db, id)
	}
	fn edges(kind: &str) {
		for case in 0..12 {
			let (mut db, note_id) = db();
			let id = if kind == "note" {
				note_id
			} else {
				crate::editions::store(&mut db, 1, "rust", "Rust", 5, &[], "", 1000).unwrap()
			};
			let mut op = Operation {
				token: uuid::Uuid::new_v4().to_string(),
				kind: kind.into(),
				id,
				revision: 1,
				note: "Offline note".into(),
				collection: "Craft".into(),
				position: 0,
			};
			let mut profile = 1;
			match case {
				0 => {}
				1 => op.token = "bad".into(),
				2 => profile = 2,
				3 => op.id = 999,
				4 => op.revision = 0,
				5 => op.revision = 9,
				6 => op.note = "x".repeat(32769),
				7 => op.collection = "x".repeat(81),
				8 => op.collection = "a\nb".into(),
				9 => {
					replay(&mut db, 1, &op).unwrap();
				}
				10 => {
					replay(&mut db, 1, &op).unwrap();
					op.note = "Changed retry".into();
				}
				11 => initialize(&db).unwrap(),
				_ => {}
			}
			let result = replay(&mut db, profile, &op);
			assert_eq!(result.is_ok(), matches!(case, 0 | 9 | 11), "{kind} case {case}");
			if result.is_ok() {
				let revision = if kind == "note" {
					crate::library::get(&db, 1, id).unwrap().unwrap().revision
				} else {
					crate::editions::get(&db, 1, id).unwrap().unwrap().revision
				};
				assert_eq!(revision, 2, "{kind} case {case}");
			}
		}
	}
	#[test]
	fn note_replay_twelve_edges() {
		edges("note")
	}
	#[test]
	fn checkpoint_replay_twelve_edges() {
		edges("place")
	}
	#[test]
	fn completion_replay_twelve_edges() {
		edges("complete")
	}
}
