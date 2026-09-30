//! Sessions, passwords, and the sign-in, setup, sign-out, and password-change handlers.

use super::*;

pub fn active_feed_cookie(value: String) -> Cookie<'static> {
	Cookie::build(("active_feed", value))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
		.into()
}

pub(super) fn session_cookie(token: String) -> Cookie<'static> {
	let days = session_days();
	Cookie::build((session_cookie_name(), token))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::days(days))
		.max_age(Duration::days(days))
		.into()
}

pub(super) fn remove_session_cookie(response: &mut Response<Body>) {
	let cookie = Cookie::build((session_cookie_name(), ""))
		.path("/")
		.http_only(true)
		.secure(cookie_secure())
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::UNIX_EPOCH)
		.max_age(Duration::ZERO)
		.into();
	response.insert_cookie(cookie);
}

pub(super) fn password_hash(password: &str) -> Result<String, String> {
	crate::db::blocking(|| password_hash_blocking(password))
}

pub(super) fn password_hash_blocking(password: &str) -> Result<String, String> {
	let salt = SaltString::generate(&mut OsRng);
	Argon2::default()
		.hash_password(password.as_bytes(), &salt)
		.map(|hash| hash.to_string())
		.map_err(|error| format!("Unable to secure the password: {error}"))
}

pub(super) fn password_matches(password: &str, encoded: &str) -> bool {
	crate::db::blocking(|| password_matches_blocking(password, encoded))
}

pub(super) fn password_matches_blocking(password: &str, encoded: &str) -> bool {
	PasswordHash::new(encoded)
		.ok()
		.is_some_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
}

pub(super) fn validate_username(value: &str) -> Result<String, String> {
	let username = value.trim();
	if !(3..=32).contains(&username.len()) || !username.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')) {
		return Err("Use 3–32 letters, numbers, periods, underscores, or hyphens for the username.".to_string());
	}
	Ok(username.to_ascii_lowercase())
}

pub(super) fn validate_display_name(value: &str, username: &str) -> Result<String, String> {
	let display_name = if value.trim().is_empty() { username } else { value.trim() };
	if display_name.chars().count() > 64 || display_name.chars().any(char::is_control) {
		return Err("Display names must be 64 characters or fewer.".to_string());
	}
	Ok(display_name.to_string())
}

pub(super) fn validate_password(password: &str, confirmation: &str) -> Result<(), String> {
	if password != confirmation {
		return Err("The passwords do not match.".to_string());
	}
	if !(12..=128).contains(&password.chars().count()) {
		return Err("Use a password between 12 and 128 characters.".to_string());
	}
	Ok(())
}

pub(super) async fn read_form(request: Request<Body>) -> Result<HashMap<String, String>, String> {
	let mut body = request.into_body();
	let body = read_body_limited(&mut body, MAX_FORM_BYTES, "The submitted form is too large.")
		.await
		.map_err(|error| format!("Unable to read the form: {error}"))?;
	Ok(url::form_urlencoded::parse(&body).map(|(key, value)| (key.into_owned(), value.into_owned())).collect())
}

pub(super) fn form_value<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
	form.get(key).map(String::as_str).unwrap_or_default()
}

pub(super) fn find_user(connection: &Connection, username: &str) -> Result<Option<UserRecord>, String> {
	connection
		.query_row(
			"SELECT id, password_hash, disabled_at IS NOT NULL FROM users WHERE username = ?1 COLLATE NOCASE",
			params![username],
			|row| {
				Ok(UserRecord {
					id: row.get(0)?,
					password_hash: row.get(1)?,
					is_disabled: row.get::<_, i64>(2)? != 0,
				})
			},
		)
		.optional()
		.map_err(|error| format!("Unable to read the Vale account: {error}"))
}

pub(super) fn insert_session(connection: &Connection, user_id: i64) -> Result<String, String> {
	let token = random_token();
	let timestamp = now();
	connection
		.execute(
			"INSERT INTO sessions (token_hash, user_id, created_at, expires_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?3)",
			params![token_hash(&token), user_id, timestamp, timestamp + session_days() * 86_400],
		)
		.map_err(|error| format!("Unable to create the Vale session: {error}"))?;
	Ok(token)
}

/// Reset one account's password for a local operator tool.
///
/// The caller is responsible for obtaining the replacement password through a
/// protected prompt. This function does not log, include, or return the
/// password, and keeps a disabled account disabled while revoking its sessions.
pub fn reset_password_offline(username: &str, new_password: &str, confirmation: &str) -> Result<(), String> {
	if mode() != ProfileMode::Accounts {
		return Err("Offline password reset requires Vale account mode.".to_string());
	}
	let username = validate_username(username).map_err(|_| "The account username is invalid; use 3–32 letters, numbers, periods, underscores, or hyphens.".to_string())?;
	validate_password(new_password, confirmation).map_err(|error| format!("Replacement password rejected: {error}"))?;
	let encoded_password = password_hash(new_password).map_err(|_| "Unable to secure the replacement password.".to_string())?;
	let mut connection = open_database().map_err(|_| "Unable to open the configured Vale account database.".to_string())?;
	reset_password_offline_in_connection(&mut connection, &username, &encoded_password)
}

pub(super) fn reset_password_offline_in_connection(connection: &mut Connection, username: &str, encoded_password: &str) -> Result<(), String> {
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|_| "Unable to lock the account database for offline password reset.".to_string())?;
	let user_id: Option<i64> = transaction
		.query_row("SELECT id FROM users WHERE username = ?1 COLLATE NOCASE", params![username], |row| row.get(0))
		.optional()
		.map_err(|_| "Unable to inspect the Vale account database.".to_string())?;
	let Some(user_id) = user_id else {
		return Err("No Vale account matched that username.".to_string());
	};
	let updated = transaction
		.execute(
			"UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3",
			params![encoded_password, now(), user_id],
		)
		.map_err(|_| "Unable to update the Vale account password.".to_string())?;
	if updated != 1 {
		return Err("Unable to update exactly one Vale account.".to_string());
	}
	transaction
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
		.map_err(|_| "Unable to revoke the Vale account sessions.".to_string())?;
	transaction.commit().map_err(|_| "Unable to commit the offline password reset.".to_string())
}

pub(super) fn render_login(preferences: Preferences, next: String, error: String, status: StatusCode) -> Response<Body> {
	let body = LoginTemplate {
		prefs: preferences,
		url: "/login".to_string(),
		next,
		error,
	}
	.render()
	.unwrap_or_default();
	Response::builder()
		.status(status)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.unwrap_or_default()
}

pub async fn login_get(request: Request<Body>) -> Result<Response<Body>, String> {
	if mode() != ProfileMode::Accounts {
		return Ok(redirect("/"));
	}
	if !has_accounts()? {
		return Ok(see_other("/setup"));
	}
	let next = request
		.uri()
		.query()
		.and_then(|query| {
			url::form_urlencoded::parse(query.as_bytes())
				.find(|(key, _)| key == "next")
				.map(|(_, value)| value.into_owned())
		})
		.map_or_else(|| "/".to_string(), |value| safe_next(&value));
	Ok(render_login(Preferences::from_browser(&request), next, String::new(), StatusCode::OK))
}

pub async fn login_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let preferences = Preferences::from_browser(&request);
	let client = login_throttle::client_key(crate::net::client_ip(&request));
	let form = read_form(request).await?;
	let username = form_value(&form, "username").trim().to_ascii_lowercase();
	let password = form_value(&form, "password");
	let next = safe_next(form_value(&form, "next"));
	if !login_throttle::allowed(&client, &username) {
		return Ok(render_login(
			preferences,
			next,
			"Too many sign-in attempts. Wait fifteen minutes and try again.".to_string(),
			StatusCode::TOO_MANY_REQUESTS,
		));
	}
	let connection = open_database()?;
	let user = find_user(&connection, &username)?;
	let encoded = user.as_ref().map(|user| user.password_hash.as_str()).unwrap_or(DUMMY_PASSWORD_HASH.as_str());
	let valid = password_matches(password, encoded) && user.as_ref().is_some_and(|user| !user.is_disabled);
	if !valid {
		login_throttle::record_failure(&client, &username);
		return Ok(render_login(
			preferences,
			next,
			"The username or password was not accepted.".to_string(),
			StatusCode::UNAUTHORIZED,
		));
	}
	let user = user.expect("valid login has a user");
	login_throttle::record_success(&client, &username);
	let token = insert_session(&connection, user.id)?;
	let mut response = see_other(&next);
	response.insert_cookie(session_cookie(token));
	Ok(response)
}

pub(super) fn render_setup(preferences: Preferences, error: String, status: StatusCode) -> Response<Body> {
	let feed_groups = preferences.feed_groups();
	let subscription_count = preferences.subscriptions.len();
	let body = SetupTemplate {
		prefs: preferences,
		url: "/setup".to_string(),
		feed_groups,
		subscription_count,
		error,
	}
	.render()
	.unwrap_or_default();
	Response::builder()
		.status(status)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.unwrap_or_default()
}

pub async fn setup_get(request: Request<Body>) -> Result<Response<Body>, String> {
	if mode() != ProfileMode::Accounts {
		return Ok(redirect("/"));
	}
	if has_accounts()? {
		return Ok(see_other("/login"));
	}
	Ok(render_setup(Preferences::from_browser(&request), String::new(), StatusCode::OK))
}

pub async fn setup_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let browser_preferences = Preferences::from_browser(&request);
	let form = read_form(request).await?;
	let username = match validate_username(form_value(&form, "username")) {
		Ok(value) => value,
		Err(error) => return Ok(render_setup(browser_preferences, error, StatusCode::UNPROCESSABLE_ENTITY)),
	};
	let display_name = match validate_display_name(form_value(&form, "display_name"), &username) {
		Ok(value) => value,
		Err(error) => return Ok(render_setup(browser_preferences, error, StatusCode::UNPROCESSABLE_ENTITY)),
	};
	if let Err(error) = validate_password(form_value(&form, "password"), form_value(&form, "password_confirm")) {
		return Ok(render_setup(browser_preferences, error, StatusCode::UNPROCESSABLE_ENTITY));
	}
	let preferences = if form.contains_key("import_current") {
		browser_preferences.clone()
	} else {
		default_preferences()
	};
	let encoded_password = password_hash(form_value(&form, "password"))?;
	let serialized = serialize_preferences(&preferences)?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin owner setup: {error}"))?;
	if user_count(&transaction)? != 0 {
		return Ok(see_other("/login"));
	}
	let timestamp = now();
	transaction
		.execute(
			"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?4)",
			params![username, display_name, encoded_password, timestamp],
		)
		.map_err(|error| format!("Unable to create the owner account: {error}"))?;
	let user_id = transaction.last_insert_rowid();
	let shared_profile_id: Option<i64> = transaction
		.query_row("SELECT id FROM profiles WHERE user_id IS NULL ORDER BY id LIMIT 1", [], |row| row.get(0))
		.optional()
		.map_err(|error| format!("Unable to inspect the shared profile during setup: {error}"))?;
	if let Some(profile_id) = shared_profile_id {
		transaction
			.execute(
				"UPDATE profiles SET user_id = ?1, label = ?2, preferences_json = ?3, updated_at = ?4 WHERE id = ?5",
				params![user_id, display_name, serialized, timestamp, profile_id],
			)
			.map_err(|error| format!("Unable to promote the shared profile: {error}"))?;
	} else {
		transaction
			.execute(
				"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
				params![user_id, display_name, serialized, timestamp],
			)
			.map_err(|error| format!("Unable to create the owner profile: {error}"))?;
	}
	let token = random_token();
	transaction
		.execute(
			"INSERT INTO sessions (token_hash, user_id, created_at, expires_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?3)",
			params![token_hash(&token), user_id, timestamp, timestamp + session_days() * 86_400],
		)
		.map_err(|error| format!("Unable to create the owner session: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish owner setup: {error}"))?;
	let mut response = see_other("/");
	response.insert_cookie(session_cookie(token));
	if !preferences.active_feed.is_empty() {
		response.insert_cookie(active_feed_cookie(preferences.active_feed));
	}
	Ok(response)
}

pub async fn logout_post(request: Request<Body>) -> Result<Response<Body>, String> {
	if let Some(context) = context(&request) {
		if let Some(hash) = &context.session_hash {
			open_database()?
				.execute("DELETE FROM sessions WHERE token_hash = ?1", params![hash])
				.map_err(|error| format!("Unable to end the Vale session: {error}"))?;
		}
	}
	let mut response = see_other("/login");
	remove_session_cookie(&mut response);
	Ok(response)
}

pub async fn logout_all_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(user_id) = context(&request).and_then(|context| context.user_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	open_database()?
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
		.map_err(|error| format!("Unable to end the Vale sessions: {error}"))?;
	let mut response = see_other("/login");
	remove_session_cookie(&mut response);
	Ok(response)
}

pub async fn change_password_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(user_id) = context(&request).and_then(|context| context.user_id) else {
		return Ok(plain_response(StatusCode::UNAUTHORIZED, "Sign in is required."));
	};
	let form = read_form(request).await?;
	if let Err(error) = validate_password(form_value(&form, "new_password"), form_value(&form, "new_password_confirm")) {
		return Ok(see_other(&format!("/settings?account={}", status_code(&error))));
	}
	let mut connection = open_database()?;
	let current_hash: String = connection
		.query_row("SELECT password_hash FROM users WHERE id = ?1", params![user_id], |row| row.get(0))
		.map_err(|error| format!("Unable to read the Vale account: {error}"))?;
	if !password_matches(form_value(&form, "current_password"), &current_hash) {
		return Ok(see_other("/settings?account=current-password"));
	}
	let encoded = password_hash(form_value(&form, "new_password"))?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin the password change: {error}"))?;
	transaction
		.execute("UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3", params![encoded, now(), user_id])
		.map_err(|error| format!("Unable to change the Vale password: {error}"))?;
	transaction
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
		.map_err(|error| format!("Unable to rotate the Vale sessions: {error}"))?;
	let token = insert_session(&transaction, user_id)?;
	transaction.commit().map_err(|error| format!("Unable to finish the password change: {error}"))?;
	let mut response = see_other("/settings?account=password-changed");
	response.insert_cookie(session_cookie(token));
	Ok(response)
}

pub(super) fn status_code(error: &str) -> &'static str {
	if error.contains("match") {
		"password-mismatch"
	} else {
		"password-length"
	}
}
