use crate::client::json;
use crate::listing::{self, FragmentMode, ListingPolicy, ListingStatus, PostRenderKind};
use crate::server::RequestExt;
use crate::utils::{error, filter_posts, format_url, get_filters, listing_query, nsfw_landing, param, template, Post, Preferences, User};
use crate::{config, utils};
use askama::Template;
use chrono::DateTime;
use htmlescape::decode_html;
use hyper::{Body, Request, Response};
use time::{macros::format_description, OffsetDateTime};

// STRUCTS
#[derive(Template)]
#[template(path = "user.html")]
struct UserTemplate {
	user: User,
	posts: Vec<Post>,
	sort: (String, String),
	ends: (String, String),
	/// "overview", "comments", or "submitted"
	listing: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	/// Whether the user themself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	listing_status: String,
	visible_count: usize,
}

// FUNCTIONS
pub async fn profile(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());
	let fragment_mode = if listing == "submitted" {
		match listing::fragment_mode(&req) {
			Ok(mode) => mode,
			Err(response) => return Ok(response),
		}
	} else {
		if let Err(response) = listing::reject_fragment_request(&req) {
			return Ok(response);
		}
		FragmentMode::Document
	};

	// Build the Reddit JSON API path
	let path = format!(
		"/user/{}/{listing}.json?{}&raw_json=1",
		req.param("name").unwrap_or_else(|| "reddit".to_string()),
		listing_query(req.uri().query().unwrap_or_default()),
	);
	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26");

	// Retrieve other variables from Redlib request
	let sort = param(&path, "sort").unwrap_or_default();
	let username = req.param("name").unwrap_or_default();

	// Retrieve info from user about page.
	let user = user(&username).await.unwrap_or_default();

	let req_url = req.uri().to_string();
	// Return landing page if this post if this Reddit deems this user NSFW,
	// but we have also disabled the display of NSFW content or if the instance
	// is SFW-only.
	if user.nsfw && utils::should_be_nsfw_gated(&req, &req_url) {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				Preferences::new(&req),
				url,
				PostRenderKind::Direct,
				String::new(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		let response = nsfw_landing(req, req_url).await.unwrap_or_default();
		return Ok(if listing == "submitted" { listing::document_response(response) } else { response });
	}

	let filters = get_filters(&req);
	if filters.contains(&["u_", &username].concat()) {
		if fragment_mode == FragmentMode::Posts {
			return listing::render_posts_fragment(
				Vec::new(),
				Preferences::new(&req),
				url,
				PostRenderKind::Direct,
				String::new(),
				String::new(),
				String::new(),
				ListingStatus::End,
			);
		}
		let response = template(&UserTemplate {
			user,
			posts: Vec::new(),
			sort: (sort, param(&path, "t").unwrap_or_default()),
			ends: (param(&path, "after").unwrap_or_default(), String::new()),
			listing: listing.clone(),
			prefs: Preferences::new(&req),
			url,
			redirect_url,
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			listing_status: ListingStatus::End.as_str().to_string(),
			visible_count: 0,
		});
		Ok(if listing == "submitted" { listing::document_response(response) } else { response })
	} else if listing == "submitted" {
		let policy = match ListingPolicy::for_request(&req, None, false, false) {
			Ok(policy) => policy,
			Err(_) => return listing::policy_unavailable_response(req, fragment_mode).await,
		};
		match listing::accumulate(&path, false, policy).await {
			Ok(result) => {
				let previous_url = result.previous_url(&url);
				let next_url = result.next_url(&url);
				let all_posts_filtered = result.all_posts_filtered();
				let all_posts_hidden_nsfw = result.all_posts_hidden_nsfw();
				let no_posts = result.no_posts();
				let status = result.status;
				let visible_count = result.posts.len();
				if fragment_mode == FragmentMode::Posts {
					return listing::render_posts_fragment(
						result.posts,
						Preferences::new(&req),
						url,
						PostRenderKind::Direct,
						String::new(),
						previous_url,
						next_url,
						status,
					);
				}
				Ok(listing::document_response(template(&UserTemplate {
					user,
					posts: result.posts,
					sort: (sort, param(&path, "t").unwrap_or_default()),
					ends: (result.previous_cursor, result.next_cursor),
					listing,
					prefs: Preferences::new(&req),
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
					listing_status: status.as_str().to_string(),
					visible_count,
				})))
			}
			Err(msg) => {
				if fragment_mode == FragmentMode::Posts {
					Ok(listing::fragment_unavailable_response())
				} else {
					Ok(listing::document_response(error(req, &msg).await?))
				}
			}
		}
	} else {
		// Request user posts/comments from Reddit
		match Post::fetch(&path, false).await {
			Ok((mut posts, cursors)) => {
				let (_, all_posts_filtered) = filter_posts(&mut posts, &filters);
				crate::account::filter_hidden_posts(&req, &mut posts)?;
				let no_posts = posts.is_empty();
				let visible_count = posts.len();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && Preferences::new(&req).show_nsfw != "on");
				Ok(template(&UserTemplate {
					user,
					posts,
					sort: (sort, param(&path, "t").unwrap_or_default()),
					ends: (cursors.before, cursors.after),
					listing,
					prefs: Preferences::new(&req),
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
					listing_status: ListingStatus::End.as_str().to_string(),
					visible_count,
				}))
			}
			// If there is an error show error page
			Err(msg) => error(req, &msg).await,
		}
	}
}

// USER
async fn user(name: &str) -> Result<User, String> {
	// Build the Reddit JSON API path
	let path: String = format!("/user/{name}/about.json?raw_json=1");

	// Send a request to the url
	json(path, false).await.map(|res| {
		// Grab creation date as unix timestamp
		let created_unix = res["data"]["created"].as_f64().unwrap_or(0.0).round() as i64;
		let created = OffsetDateTime::from_unix_timestamp(created_unix).unwrap_or(OffsetDateTime::UNIX_EPOCH);

		// Closure used to parse JSON from Reddit APIs
		let about = |item| res["data"]["subreddit"][item].as_str().unwrap_or_default().to_string();

		// Parse the JSON output into a User struct
		User {
			name: res["data"]["name"].as_str().unwrap_or(name).to_owned(),
			title: about("title"),
			icon: format_url(&about("icon_img")),
			karma: res["data"]["total_karma"].as_i64().unwrap_or(0),
			created: created.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default(),
			banner: about("banner_img"),
			description: about("public_description"),
			nsfw: res["data"]["subreddit"]["over_18"].as_bool().unwrap_or_default(),
		}
	})
}

pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if config::get_setting("REDLIB_ENABLE_RSS").is_none() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}
	use crate::utils::rewrite_urls;
	use hyper::header::CONTENT_TYPE;
	use rss::{ChannelBuilder, Item};

	// Get user
	let user_str = req.param("name").unwrap_or_default();

	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());

	// Get path
	let path = format!("/user/{user_str}/{listing}.json?{}&raw_json=1", req.uri().query().unwrap_or_default(),);

	// Get user
	let user_obj = user(&user_str).await.unwrap_or_default();

	// Get posts
	let (posts, _) = Post::fetch(&path, false).await?;

	// Build the RSS feed
	let channel = ChannelBuilder::default()
		.title(user_str)
		.description(user_obj.description)
		.items(
			posts
				.into_iter()
				.map(|post| Item {
					title: Some(post.title.to_string()),
					link: Some(format_url(&utils::get_post_url(&post))),
					author: Some(post.author.name),
					pub_date: Some(DateTime::from_timestamp(post.created_ts as i64, 0).unwrap_or_default().to_rfc2822()),
					content: Some(rewrite_urls(&decode_html(&post.body).unwrap_or_else(|_| post.body.clone()))),
					..Default::default()
				})
				.collect::<Vec<_>>(),
		)
		.build();

	// Serialize the feed to RSS
	let body = channel.to_string().into_bytes();

	// Create the HTTP response
	let mut res = Response::new(Body::from(body));
	res.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/rss+xml"));

	Ok(res)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_fetching_user() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let user = user("spez").await;
		assert!(user.is_ok());
		assert!(user.unwrap().karma > 100);
	}
}
