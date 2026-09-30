//! Profile database schema creation and migration.

use super::*;

pub(super) fn enable_wal(connection: &Connection) -> Result<(), String> {
	let mode = connection
		.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0))
		.map_err(|error| format!("Unable to enable the Vale profile write-ahead log: {error}"))?;
	if !mode.eq_ignore_ascii_case("wal") {
		return Err(format!("Vale requested WAL mode but SQLite selected {mode}."));
	}
	Ok(())
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), String> {
	connection
		.execute_batch(
			"CREATE TABLE IF NOT EXISTS users (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				username TEXT NOT NULL COLLATE NOCASE UNIQUE,
				display_name TEXT NOT NULL,
				password_hash TEXT NOT NULL,
				is_admin INTEGER NOT NULL DEFAULT 0,
				disabled_at INTEGER,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS profiles (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				user_id INTEGER UNIQUE REFERENCES users(id) ON DELETE CASCADE,
				label TEXT NOT NULL,
				preferences_json TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS sessions (
				token_hash BLOB PRIMARY KEY,
				user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
				created_at INTEGER NOT NULL,
				expires_at INTEGER NOT NULL,
				last_seen_at INTEGER NOT NULL,
				user_agent TEXT NOT NULL DEFAULT ''
			);
			CREATE INDEX IF NOT EXISTS sessions_user_id ON sessions(user_id);
			CREATE INDEX IF NOT EXISTS sessions_expires_at ON sessions(expires_at);
			CREATE TABLE IF NOT EXISTS post_history (
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				post_id TEXT NOT NULL,
				title TEXT NOT NULL,
				community TEXT NOT NULL,
				permalink TEXT NOT NULL,
				first_viewed_at INTEGER NOT NULL,
				last_viewed_at INTEGER NOT NULL,
				view_count INTEGER NOT NULL DEFAULT 1,
				PRIMARY KEY (profile_id, post_id)
			);
			CREATE INDEX IF NOT EXISTS post_history_recent ON post_history(profile_id, last_viewed_at DESC);
			CREATE TABLE IF NOT EXISTS hidden_posts (
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				post_id TEXT NOT NULL,
				hidden_at INTEGER NOT NULL,
				PRIMARY KEY (profile_id, post_id)
			);
			CREATE INDEX IF NOT EXISTS hidden_posts_recent ON hidden_posts(profile_id, hidden_at DESC);
			CREATE TABLE IF NOT EXISTS post_archives (
				id TEXT PRIMARY KEY,
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				post_id TEXT NOT NULL,
				permalink TEXT NOT NULL DEFAULT '',
				title TEXT NOT NULL DEFAULT '',
				community TEXT NOT NULL DEFAULT '',
				source_url TEXT NOT NULL DEFAULT '',
				status TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL,
				captured_at INTEGER,
				comment_count INTEGER NOT NULL DEFAULT 0,
				asset_count INTEGER NOT NULL DEFAULT 0,
				generated_asset_count INTEGER NOT NULL DEFAULT 0,
				total_bytes INTEGER NOT NULL DEFAULT 0,
				issues_json TEXT NOT NULL DEFAULT '[]',
				error TEXT NOT NULL DEFAULT '',
				UNIQUE (profile_id, post_id)
			);
			CREATE INDEX IF NOT EXISTS post_archives_profile_recent ON post_archives(profile_id, created_at DESC);
			CREATE INDEX IF NOT EXISTS post_archives_status ON post_archives(status, updated_at);
			CREATE TABLE IF NOT EXISTS profile_archive_settings (
				profile_id INTEGER PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
				archive_budget_mib INTEGER NOT NULL DEFAULT 0,
				revision INTEGER NOT NULL DEFAULT 0,
				updated_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS archive_reservations (
				archive_id TEXT PRIMARY KEY REFERENCES post_archives(id) ON DELETE CASCADE,
				profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
				state TEXT NOT NULL,
				reserved_bytes INTEGER NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			);
			CREATE INDEX IF NOT EXISTS archive_reservations_profile ON archive_reservations(profile_id, state);",
		)
		.map_err(|error| format!("Unable to migrate the Vale profile database: {error}"))?;
	ensure_column(connection, "post_archives", "generated_asset_count", "INTEGER NOT NULL DEFAULT 0")?;
	crate::reading_filters::initialize(connection).map_err(|e| e.to_string())?;
	crate::reading::initialize(connection).map_err(|error| format!("Unable to initialize reading state: {error}"))?;
	crate::watch::initialize(connection).map_err(|e| e.to_string())?;
	crate::editions::initialize(connection).map_err(|e| e.to_string())?;
	crate::library::initialize(connection).map_err(|e| e.to_string())?;
	crate::offline::initialize(connection).map_err(|e| e.to_string())?;
	crate::sources::initialize(connection).map_err(|e| e.to_string())?;
	crate::stories::initialize(connection).map_err(|e| e.to_string())?;
	crate::agenda::initialize(connection).map_err(|e| e.to_string())?;
	crate::activity::initialize(connection).map_err(|error| format!("Unable to initialize Vale comment activity: {error}"))?;
	Ok(())
}

pub(super) fn ensure_column(connection: &Connection, table: &str, column: &str, definition: &str) -> Result<(), String> {
	let mut statement = connection
		.prepare(&format!("PRAGMA table_info({table})"))
		.map_err(|error| format!("Unable to inspect the Vale database schema: {error}"))?;
	let columns = statement
		.query_map([], |row| row.get::<_, String>(1))
		.map_err(|error| format!("Unable to query the Vale database schema: {error}"))?
		.collect::<Result<HashSet<_>, _>>()
		.map_err(|error| format!("Unable to read the Vale database schema: {error}"))?;
	drop(statement);
	if !columns.contains(column) {
		connection
			.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
			.map_err(|error| format!("Unable to extend the Vale database schema: {error}"))?;
	}
	Ok(())
}

pub fn initialize() -> Result<(), String> {
	if mode() == ProfileMode::Browser {
		return Ok(());
	}
	let connection = open_database()?;
	enable_wal(&connection)?;
	initialize_schema(&connection)?;
	connection
		.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now()])
		.map_err(|error| format!("Unable to expire old Vale sessions: {error}"))?;
	if mode() == ProfileMode::Shared {
		ensure_shared_profile(&connection)?;
	}
	Ok(())
}
