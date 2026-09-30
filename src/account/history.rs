//! Per-profile reading history.

use super::*;

pub(super) fn truncate(value: &str, max_chars: usize) -> String {
	value.chars().take(max_chars).collect()
}

pub fn record_post_view(request: &Request<Body>, post: &Post) -> Result<(), String> {
	crate::db::blocking(|| record_post_view_blocking(request, post))
}

pub(super) fn record_post_view_blocking(request: &Request<Body>, post: &Post) -> Result<(), String> {
	let Some(context) = context(request) else {
		return Ok(());
	};
	let connection = open_database()?;
	let timestamp = now();
	connection
		.execute(
			"INSERT INTO post_history (profile_id, post_id, title, community, permalink, first_viewed_at, last_viewed_at, view_count)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
			 ON CONFLICT(profile_id, post_id) DO UPDATE SET
				title = excluded.title,
				community = excluded.community,
				permalink = excluded.permalink,
				last_viewed_at = excluded.last_viewed_at,
				view_count = post_history.view_count + 1",
			params![
				context.profile_id,
				truncate(&post.id, 80),
				truncate(&post.title, 500),
				truncate(&post.community, 120),
				truncate(&post.permalink, 1_000),
				timestamp
			],
		)
		.map_err(|error| format!("Unable to record Vale reading history: {error}"))?;
	connection
		.execute(
			"DELETE FROM post_history WHERE profile_id = ?1 AND last_viewed_at < ?2",
			params![context.profile_id, timestamp - HISTORY_RETENTION_SECONDS],
		)
		.map_err(|error| format!("Unable to expire Vale reading history: {error}"))?;
	connection
		.execute(
			"DELETE FROM post_history WHERE profile_id = ?1 AND post_id IN (
				SELECT post_id FROM post_history WHERE profile_id = ?1 ORDER BY last_viewed_at DESC LIMIT -1 OFFSET ?2
			)",
			params![context.profile_id, HISTORY_LIMIT],
		)
		.map_err(|error| format!("Unable to bound Vale reading history: {error}"))?;
	Ok(())
}

pub async fn history_get(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(context) = context(&request).cloned() else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	let connection = open_database()?;
	let mut statement = connection
		.prepare(
			"SELECT title, community, permalink, last_viewed_at, view_count
			 FROM post_history WHERE profile_id = ?1 ORDER BY last_viewed_at DESC LIMIT 250",
		)
		.map_err(|error| format!("Unable to prepare Vale history: {error}"))?;
	let entries = statement
		.query_map(params![context.profile_id], |row| {
			let timestamp = row.get::<_, i64>(3)?;
			Ok(HistoryEntry {
				title: row.get(0)?,
				community: row.get(1)?,
				permalink: row.get(2)?,
				viewed: crate::utils::time(timestamp as f64).0,
				view_count: row.get(4)?,
			})
		})
		.map_err(|error| format!("Unable to query Vale history: {error}"))?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| format!("Unable to read Vale history: {error}"))?;
	Ok(template(&HistoryTemplate {
		prefs: Preferences::new(&request),
		url: "/history".to_string(),
		entries,
	}))
}

pub async fn history_clear_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(profile_id) = context(&request).map(|context| context.profile_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	let mut connection = open_database()?;
	let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|error| error.to_string())?;
	transaction
		.execute("DELETE FROM post_history WHERE profile_id = ?1", params![profile_id])
		.map_err(|error| format!("Unable to clear Vale history: {error}"))?;
	transaction
		.execute("DELETE FROM post_activity WHERE profile_id = ?1", params![profile_id])
		.map_err(|error| error.to_string())?;
	transaction.commit().map_err(|error| error.to_string())?;
	Ok(see_other("/history"))
}
