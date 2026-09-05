# Reading-flow visual fixtures

Export synthetic feed and discussion data through the production Askama templates:

```sh
VALE_READING_FIXTURE_DIR=/tmp/vale-reading-fixtures cargo test --locked
python3 tests/reading/serve.py /tmp/vale-reading-fixtures --port 3102
# Optional second server:
python3 tests/reading/serve.py /tmp/vale-reading-fixtures --theme light --port 3103
```

Open http://127.0.0.1:3102/ in a browser. Test Preview text, the local title link,
Read comments, reply disclosure, and browser Back. At 320, 390, 768, 1120, 1280,
1440, and 1920 CSS pixels, inspect overflow, full titles, action targets, preview
prose, deep replies, and the sticky toolbar. Use keyboard Enter as well as clicks.

The server is loopback-only, uses actual source assets, and changes asset query
keys when CSS or JavaScript changes. It disables service-worker registration and
serves no account, save, hide, feed-management, or comment-continuation APIs.
Every feed route returns the same fixture and every discussion route returns the
same discussion. Use the compiled application for real routes and mutations.
These fixtures contain no account data and are compiled only under cfg(test).

Secondary-page fixtures are available under `/review/<name>.html`: author,
duplicates, wiki, saved, saved-detail, history, login, setup, error, info, gate,
and wall. Combined discussion is exported in the light theme as combined.html.
These render production templates with synthetic data. Check disclosures by
keyboard, mobile sorting visibility, prose, and both themes; use the compiled
application for Feeds, Settings, real search, and real community routes.

## Optional copied-state migration rehearsal

Use a consistent standalone SQLite backup, never a live database file. The test
copies that backup into a temporary file and initializes the copy twice, checking
integrity and foreign keys. The supplied backup is not modified.

```sh
VALE_MIGRATION_FIXTURE=/path/to/profile-backup.sqlite3 \
  cargo test --locked copied_state_schema_rehearsal -- --ignored
```
