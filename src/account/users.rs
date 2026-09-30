//! Administrator account management.

use super::*;

pub async fn create_user_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(admin) = context(&request).cloned().filter(|context| context.is_admin) else {
		return Ok(plain_response(StatusCode::FORBIDDEN, "Administrator access is required."));
	};
	let form = read_form(request).await?;
	let username = match validate_username(form_value(&form, "username")) {
		Ok(value) => value,
		Err(_) => return Ok(see_other("/settings?account=username-invalid")),
	};
	let display_name = match validate_display_name(form_value(&form, "display_name"), &username) {
		Ok(value) => value,
		Err(_) => return Ok(see_other("/settings?account=display-name-invalid")),
	};
	if let Err(error) = validate_password(form_value(&form, "password"), form_value(&form, "password_confirm")) {
		return Ok(see_other(&format!("/settings?account={}", status_code(&error))));
	}
	let encoded = password_hash(form_value(&form, "password"))?;
	let preferences = if form.contains_key("clone_profile") { admin.preferences } else { default_preferences() };
	let serialized = serialize_preferences(&preferences)?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin account creation: {error}"))?;
	let timestamp = now();
	let inserted = transaction.execute(
		"INSERT INTO users (username, display_name, password_hash, is_admin, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
		params![username, display_name, encoded, i64::from(form.contains_key("is_admin")), timestamp],
	);
	if inserted.is_err() {
		return Ok(see_other("/settings?account=username-exists"));
	}
	let user_id = transaction.last_insert_rowid();
	transaction
		.execute(
			"INSERT INTO profiles (user_id, label, preferences_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
			params![user_id, display_name, serialized, timestamp],
		)
		.map_err(|error| format!("Unable to create the account profile: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish account creation: {error}"))?;
	Ok(see_other("/settings?account=user-created"))
}

pub async fn toggle_user_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(admin) = context(&request).cloned().filter(|context| context.is_admin) else {
		return Ok(plain_response(StatusCode::FORBIDDEN, "Administrator access is required."));
	};
	let target_id = request.param("id").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default();
	if admin.user_id == Some(target_id) {
		return Ok(see_other("/settings?account=self-disable"));
	}
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin the account status change: {error}"))?;
	let target: Option<(bool, bool)> = transaction
		.query_row("SELECT is_admin != 0, disabled_at IS NOT NULL FROM users WHERE id = ?1", params![target_id], |row| {
			Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0))
		})
		.optional()
		.map_err(|error| format!("Unable to inspect the target account: {error}"))?;
	let Some((target_is_admin, target_disabled)) = target else {
		return Ok(see_other("/settings?account=user-missing"));
	};
	if target_is_admin && !target_disabled {
		let enabled_admins: i64 = transaction
			.query_row("SELECT COUNT(*) FROM users WHERE is_admin != 0 AND disabled_at IS NULL", [], |row| row.get(0))
			.map_err(|error| format!("Unable to count Vale administrators: {error}"))?;
		if enabled_admins <= 1 {
			return Ok(see_other("/settings?account=last-admin"));
		}
	}
	if target_disabled {
		transaction
			.execute("UPDATE users SET disabled_at = NULL, updated_at = ?1 WHERE id = ?2", params![now(), target_id])
			.map_err(|error| format!("Unable to enable the Vale account: {error}"))?;
		transaction.commit().map_err(|error| format!("Unable to finish the account status change: {error}"))?;
		Ok(see_other("/settings?account=user-enabled"))
	} else {
		transaction
			.execute("UPDATE users SET disabled_at = ?1, updated_at = ?1 WHERE id = ?2", params![now(), target_id])
			.map_err(|error| format!("Unable to disable the Vale account: {error}"))?;
		transaction
			.execute("DELETE FROM sessions WHERE user_id = ?1", params![target_id])
			.map_err(|error| format!("Unable to revoke the disabled account sessions: {error}"))?;
		transaction.commit().map_err(|error| format!("Unable to finish the account status change: {error}"))?;
		Ok(see_other("/settings?account=user-disabled"))
	}
}

pub async fn reset_user_password_post(request: Request<Body>) -> Result<Response<Body>, String> {
	let Some(admin) = context(&request).cloned().filter(|context| context.is_admin) else {
		return Ok(plain_response(StatusCode::FORBIDDEN, "Administrator access is required."));
	};
	let target_id = request.param("id").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default();
	if admin.user_id == Some(target_id) {
		return Ok(see_other("/settings?account=use-own-password-form"));
	}
	let form = read_form(request).await?;
	if let Err(error) = validate_password(form_value(&form, "password"), form_value(&form, "password_confirm")) {
		return Ok(see_other(&format!("/settings?account={}", status_code(&error))));
	}
	let encoded = password_hash(form_value(&form, "password"))?;
	let mut connection = open_database()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.map_err(|error| format!("Unable to begin the password reset: {error}"))?;
	let changed = transaction
		.execute("UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3", params![encoded, now(), target_id])
		.map_err(|error| format!("Unable to reset the Vale password: {error}"))?;
	if changed == 0 {
		return Ok(see_other("/settings?account=user-missing"));
	}
	transaction
		.execute("DELETE FROM sessions WHERE user_id = ?1", params![target_id])
		.map_err(|error| format!("Unable to revoke the reset account sessions: {error}"))?;
	transaction.commit().map_err(|error| format!("Unable to finish the password reset: {error}"))?;
	Ok(see_other("/settings?account=password-reset"))
}

pub fn current_account(request: &Request<Body>) -> Option<AccountView> {
	context(request).and_then(|context| {
		context.user_id.map(|_| AccountView {
			username: context.username.clone(),
			display_name: context.display_name.clone(),
			is_admin: context.is_admin,
		})
	})
}

pub fn accounts(request: &Request<Body>) -> Result<Vec<AccountSummary>, String> {
	let Some(context) = context(request).filter(|context| context.is_admin) else {
		return Ok(Vec::new());
	};
	let connection = open_database()?;
	let mut statement = connection
		.prepare("SELECT id, username, display_name, is_admin, disabled_at IS NOT NULL FROM users ORDER BY username COLLATE NOCASE")
		.map_err(|error| format!("Unable to prepare the account list: {error}"))?;
	let rows = statement
		.query_map([], |row| {
			let id = row.get::<_, i64>(0)?;
			Ok(AccountSummary {
				id,
				username: row.get(1)?,
				display_name: row.get(2)?,
				is_admin: row.get::<_, i64>(3)? != 0,
				is_disabled: row.get::<_, i64>(4)? != 0,
				is_current: context.user_id == Some(id),
			})
		})
		.map_err(|error| format!("Unable to list Vale accounts: {error}"))?;
	rows
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| format!("Unable to read the Vale account list: {error}"))
}
