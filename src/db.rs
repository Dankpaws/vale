//! Profile database connections and blocking-work helpers.
//!
//! Every dynamic request consults the profile database, so connections are
//! kept in a small idle pool instead of being reopened and reconfigured for
//! each request. Handlers still run SQLite and password hashing synchronously;
//! `blocking` tells a multi-threaded Tokio runtime to move other tasks off the
//! current worker while that work runs.

use rusqlite::Connection;
use std::{
	fs,
	ops::{Deref, DerefMut},
	path::{Path, PathBuf},
	sync::{LazyLock, Mutex},
	time::Duration,
};
use tokio::runtime::{Handle, RuntimeFlavor};

const MAX_IDLE_CONNECTIONS: usize = 8;

struct IdleConnection {
	path: PathBuf,
	identity: Option<FileIdentity>,
	connection: Connection,
}

static IDLE: LazyLock<Mutex<Vec<IdleConnection>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Identifies the database file a connection was opened against so a pooled
/// connection is never reused after the file was replaced (for example a
/// restore from backup while Vale is running) or removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
	device: u64,
	inode: u64,
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<FileIdentity> {
	use std::os::unix::fs::MetadataExt;
	let metadata = fs::metadata(path).ok()?;
	Some(FileIdentity {
		device: metadata.dev(),
		inode: metadata.ino(),
	})
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<FileIdentity> {
	// Without inode numbers, only reuse a connection while the file exists.
	fs::metadata(path).ok().map(|_| FileIdentity { device: 0, inode: 0 })
}

/// A profile database connection that returns to the idle pool when dropped.
pub struct PooledConnection {
	path: PathBuf,
	identity: Option<FileIdentity>,
	connection: Option<Connection>,
}

impl Deref for PooledConnection {
	type Target = Connection;

	fn deref(&self) -> &Connection {
		self.connection.as_ref().expect("pooled connection is present until drop")
	}
}

impl DerefMut for PooledConnection {
	fn deref_mut(&mut self) -> &mut Connection {
		self.connection.as_mut().expect("pooled connection is present until drop")
	}
}

impl Drop for PooledConnection {
	fn drop(&mut self) {
		let Some(connection) = self.connection.take() else {
			return;
		};
		// A connection left inside a transaction must not be handed to another
		// request; dropping it lets SQLite roll the transaction back.
		if !connection.is_autocommit() || self.identity.is_none() {
			return;
		}
		let mut idle = IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		if idle.len() < MAX_IDLE_CONNECTIONS {
			idle.push(IdleConnection {
				path: std::mem::take(&mut self.path),
				identity: self.identity,
				connection,
			});
		}
	}
}

/// Opens and configures a new connection without using the pool.
pub(crate) fn open_at(path: &Path) -> Result<Connection, String> {
	if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
		fs::create_dir_all(parent).map_err(|error| format!("Unable to create the Vale profile directory: {error}"))?;
	}
	let connection = Connection::open(path).map_err(|error| format!("Unable to open the Vale profile database: {error}"))?;
	connection
		.busy_timeout(Duration::from_secs(5))
		.map_err(|error| format!("Unable to configure the Vale profile database: {error}"))?;
	connection
		.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
		.map_err(|error| format!("Unable to configure the Vale profile database: {error}"))?;
	Ok(connection)
}

/// Returns an idle connection for `path`, or opens a new one.
pub(crate) fn pooled_at(path: &Path) -> Result<PooledConnection, String> {
	let identity = file_identity(path);
	let reused = {
		let mut idle = IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		// Drop connections whose file was replaced or removed.
		idle.retain(|entry| entry.path != path || (identity.is_some() && entry.identity == identity));
		idle.iter().position(|entry| entry.path == path).map(|index| idle.swap_remove(index))
	};
	if let Some(entry) = reused {
		return Ok(PooledConnection {
			path: entry.path,
			identity: entry.identity,
			connection: Some(entry.connection),
		});
	}
	let connection = open_at(path)?;
	Ok(PooledConnection {
		path: path.to_path_buf(),
		// Opening can create the file, so read its identity afterwards.
		identity: file_identity(path),
		connection: Some(connection),
	})
}

/// Runs synchronous work (SQLite, password hashing) from async code without
/// starving other tasks scheduled on the same Tokio worker.
///
/// On a multi-threaded runtime the worker hands its queued tasks to other
/// threads for the duration of `work`. Elsewhere (current-thread test
/// runtimes, or outside Tokio) the work simply runs inline.
pub fn blocking<T>(work: impl FnOnce() -> T) -> T {
	match Handle::try_current() {
		Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(work),
		_ => work(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temporary_path(name: &str) -> PathBuf {
		std::env::temp_dir().join(format!("vale-db-{name}-{}.sqlite3", uuid::Uuid::new_v4()))
	}

	fn connection_marker(connection: &Connection) -> i64 {
		connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap()
	}

	#[test]
	fn idle_connections_are_reused_for_the_same_file() {
		let path = temporary_path("reuse");
		{
			let connection = pooled_at(&path).unwrap();
			// A per-connection setting proves the same handle comes back.
			connection.execute_batch("PRAGMA cache_size = -4321;").unwrap();
		}
		let connection = pooled_at(&path).unwrap();
		let cache_size: i64 = connection.query_row("PRAGMA cache_size", [], |row| row.get(0)).unwrap();
		assert_eq!(cache_size, -4321);
		drop(connection);
		let _ = fs::remove_file(&path);
	}

	#[test]
	fn connections_inside_a_transaction_are_not_returned() {
		let path = temporary_path("transaction");
		{
			let mut connection = pooled_at(&path).unwrap();
			connection.execute_batch("PRAGMA cache_size = -1234;").unwrap();
			let transaction = connection.transaction().unwrap();
			std::mem::forget(transaction);
		}
		let connection = pooled_at(&path).unwrap();
		let cache_size: i64 = connection.query_row("PRAGMA cache_size", [], |row| row.get(0)).unwrap();
		assert_ne!(cache_size, -1234);
		drop(connection);
		let _ = fs::remove_file(&path);
	}

	#[test]
	fn replaced_files_are_not_served_by_stale_connections() {
		let path = temporary_path("replaced");
		{
			let connection = pooled_at(&path).unwrap();
			connection.execute_batch("PRAGMA user_version = 7;").unwrap();
		}
		fs::remove_file(&path).unwrap();
		let replacement = open_at(&path).unwrap();
		replacement.execute_batch("PRAGMA user_version = 9;").unwrap();
		drop(replacement);

		let connection = pooled_at(&path).unwrap();
		assert_eq!(connection_marker(&connection), 9);
		drop(connection);
		let _ = fs::remove_file(&path);
	}

	#[test]
	fn blocking_runs_inline_without_a_runtime() {
		assert_eq!(blocking(|| 41 + 1), 42);
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn blocking_runs_on_a_multi_threaded_runtime() {
		assert_eq!(blocking(|| 6 * 7), 42);
	}

	#[tokio::test]
	async fn blocking_runs_on_a_current_thread_runtime() {
		assert_eq!(blocking(|| 40 + 2), 42);
	}
}
