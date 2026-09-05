use crate::listing::{self, FragmentMode, ListingPolicy, ListingStatus, PostRenderKind};
use crate::utils::{
	canonical_feed_sort, canonical_feed_url, catch_random, cookie_is_secure, encode_cookie_text, error, format_num, format_url, get_filters, info, listing_query, nsfw_landing,
	param, preferred_feed_sort, redirect, rewrite_urls, safe_local_redirect, see_other, serialize_feed_groups, setting, template, to_absolute_url, val, FeedGroup, Post,
	Preferences, Subreddit,
};
use crate::{account, client::json, server::RequestExt, server::ResponseExt};
use crate::{config, utils};
use askama::Template;
use cookie::{Cookie, SameSite};
use htmlescape::decode_html;
use hyper::{Body, Request, Response};

use chrono::DateTime;
use regex::Regex;
use rss::{ChannelBuilder, Enclosure, Item};
use std::sync::LazyLock;
use time::{Duration, OffsetDateTime};

// STRUCTS
#[derive(Template)]
#[template(path = "subreddit.html")]
struct SubredditTemplate {
	sub: Subreddit,
	posts: Vec<Post>,
	sort: (String, String),
	previous_url: String,
	next_url: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	/// Whether the subreddit itself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	listing_status: String,
	visible_count: usize,
	feed_groups: Vec<FeedGroup>,
	active_feed: String,
	active_feed_name: String,
	active_communities: Vec<String>,
}

#[derive(Template)]
#[template(path = "wiki.html")]
struct WikiTemplate {
	sub: String,
	wiki: String,
	page: String,
	prefs: Preferences,
	url: String,
}

#[derive(Template)]
#[template(path = "wall.html")]
struct WallTemplate {
	title: String,
	sub: String,
	msg: String,
	prefs: Preferences,
	url: String,
}

static GEO_FILTER_MATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"geo_filter=(?<region>\w+)").unwrap());

// SERVICES
pub async fn front_page(req: Request<Body>) -> Result<Response<Body>, String> {
	let fragment_mode = match listing::fragment_mode(&req) {
		Ok(mode) => mode,
		Err(response) => return Ok(response),
	};
	if fragment_mode == FragmentMode::Posts {
		return Ok(listing::fragment_route_rejection("Use a canonical named-feed URL before requesting a fragment."));
	}
	community(req).await
}

pub async fn feed_without_sort(req: Request<Body>) -> Result<Response<Body>, String> {
	let fragment_mode = match listing::fragment_mode(&req) {
		Ok(mode) => mode,
		Err(response) => return Ok(response),
	};
	if fragment_mode == FragmentMode::Posts {
		return Ok(listing::fragment_route_rejection("Use a canonical named-feed sort URL before requesting a fragment."));
	}
	let prefs = Preferences::new(&req);
	let feed_slug = req.param("feed").unwrap_or_default();
	if !prefs.feed_groups().iter().any(|group| group.slug == feed_slug) {
		return Ok(listing::document_response(error(req, "That named feed no longer exists.").await?));
	}
	let target = canonical_feed_url(&feed_slug, preferred_feed_sort(&prefs.post_sort), req.uri().query());
	Ok(listing::document_response(see_other(&target)))
}

pub async fn legacy_front_page(req: Request<Body>) -> Result<Response<Body>, String> {
	let fragment_mode = match listing::fragment_mode(&req) {
		Ok(mode) => mode,
		Err(response) => return Ok(response),
	};
	if fragment_mode == FragmentMode::Posts {
		return Ok(listing::fragment_route_rejection("Use a canonical named-feed URL before requesting a fragment."));
	}
	let prefs = Preferences::new(&req);
	let Some(group) = prefs.active_feed_group() else {
		return Ok(listing::document_response(see_other("/feeds")));
	};
	let requested_sort = req.param("id").unwrap_or_else(|| prefs.post_sort.clone());
	let Some(sort) = canonical_feed_sort(&requested_sort) else {
		return Ok(listing::document_response(error(req, "That feed sort does not exist.").await?));
	};
	let target = canonical_feed_url(&group.slug, sort, req.uri().query());
	Ok(listing::document_response(see_other(&target)))
}

fn with_active_feed(mut response: Response<Body>, feed_slug: &str) -> Response<Body> {
	if !feed_slug.is_empty() {
		response.insert_cookie(account::active_feed_cookie(feed_slug.to_string()));
	}
	response
}

fn canonical_subreddit_sort_url(subreddit: &str, sort: &str, query: Option<&str>) -> String {
	let path = format!("/r/{subreddit}/{sort}");
	match query.filter(|query| !query.is_empty()) {
		Some(query) => format!("{path}?{query}"),
		None => path,
	}
}

pub async fn community(req: Request<Body>) -> Result<Response<Body>, String> {
	let fragment_mode = match listing::fragment_mode(&req) {
		Ok(mode) => mode,
		Err(response) => return Ok(response),
	};
	let requested_feed = req.param("feed");
	let root_request = req.uri().path() == "/";
	let front_page_request = root_request || requested_feed.is_some();
	let query = listing_query(req.uri().query().unwrap_or_default());
	let mut prefs = Preferences::new(&req);
	let front_page = prefs.front_page.clone();
	let feed_groups = prefs.feed_groups();
	let active_group = if root_request {
		feed_groups
			.iter()
			.find(|group| group.slug == prefs.active_feed)
			.cloned()
			.or_else(|| feed_groups.first().cloned())
	} else if let Some(feed_slug) = requested_feed.as_deref() {
		let Some(group) = feed_groups.iter().find(|group| group.slug == feed_slug).cloned() else {
			if fragment_mode == FragmentMode::Posts {
				return Ok(listing::fragment_route_rejection("That named feed no longer exists."));
			}
			return Ok(listing::document_response(error(req, "That named feed no longer exists.").await?));
		};
		prefs.active_feed = group.slug.clone();
		Some(group)
	} else {
		None
	};
	if root_request && active_group.is_none() {
		return Ok(listing::document_response(see_other("/feeds")));
	}
	let active_feed = active_group.as_ref().map(|group| group.slug.clone()).unwrap_or_default();
	let active_feed_name = active_group.as_ref().map(|group| group.name.clone()).unwrap_or_else(|| "My feed".to_string());
	let active_communities = active_group.as_ref().map(|group| group.communities.clone()).unwrap_or_else(|| prefs.subscriptions.clone());
	let subscribed = active_communities.join("+");
	let remove_default_feeds = prefs.remove_default_feeds == "on";
	let post_sort = preferred_feed_sort(&prefs.post_sort).to_string();
	let requested_sort = req.param("sort").unwrap_or(post_sort);
	let Some(canonical_sort) = canonical_feed_sort(&requested_sort) else {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("That post listing sort is not fragment-eligible."));
		}
		return Ok(listing::document_response(error(req, "That post listing sort does not exist.").await?));
	};
	if canonical_sort != requested_sort {
		let target = if front_page_request {
			canonical_feed_url(&active_feed, canonical_sort, req.uri().query())
		} else {
			canonical_subreddit_sort_url(req.param("sub").as_deref().unwrap_or_default(), canonical_sort, req.uri().query())
		};
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("Use the canonical post listing URL before requesting a fragment."));
		}
		return Ok(listing::document_response(see_other(&target)));
	}
	let sort = canonical_sort.to_string();

	if active_group.is_some() && active_communities.is_empty() {
		let url = String::from(req.uri().path_and_query().map_or("", |value| value.as_str()));
		if fragment_mode == FragmentMode::Posts {
			let response = listing::render_posts_fragment(
				Vec::new(),
				prefs,
				url,
				PostRenderKind::Direct,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			)?;
			return Ok(with_active_feed(response, &active_feed));
		}
		let response = template(&SubredditTemplate {
			sub: Subreddit::default(),
			posts: Vec::new(),
			sort: (sort, String::new()),
			previous_url: String::new(),
			next_url: String::new(),
			prefs,
			url,
			redirect_url: String::new(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: true,
			listing_status: ListingStatus::End.as_str().to_string(),
			visible_count: 0,
			feed_groups,
			active_feed: active_feed.clone(),
			active_feed_name,
			active_communities,
		});
		return Ok(with_active_feed(listing::document_response(response), &active_feed));
	}

	let sub_name = req.param("sub").unwrap_or(if front_page == "default" || front_page.is_empty() {
		if subscribed.is_empty() {
			"popular".to_string()
		} else {
			subscribed.clone()
		}
	} else {
		front_page.clone()
	});

	if (sub_name == "popular" || sub_name == "all") && remove_default_feeds {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("This profile does not expose that post listing."));
		}
		if subscribed.is_empty() {
			return Ok(listing::document_response(
				info(req, "Subscribe to some subreddits! (Default feeds disabled in settings)").await?,
			));
		} else {
			// If there are subscribed subs, but we get here, then the problem is that front_page pref is set to something besides default.
			// Tell user to go to settings and change front page to default.
			return Ok(listing::document_response(
				info(
					req,
					"You have subscribed to some subreddits, but your front page is not set to default. Visit settings and change front page to default.",
				)
				.await?,
			));
		}
	}

	let quarantined = can_access_quarantine(&req, &sub_name) || front_page_request;

	// Handle random subreddits
	if fragment_mode == FragmentMode::Posts && (sub_name.eq_ignore_ascii_case("random") || sub_name.eq_ignore_ascii_case("randnsfw")) {
		return Ok(listing::fragment_route_rejection("Random destinations do not provide authoritative post fragments."));
	}
	if let Ok(random) = catch_random(&sub_name, "").await {
		return Ok(listing::document_response(random));
	}

	if req.param("sub").is_some() && sub_name.starts_with("u_") {
		if fragment_mode == FragmentMode::Posts {
			return Ok(listing::fragment_route_rejection("User redirects do not provide post fragments."));
		}
		return Ok(listing::document_response(redirect(&["/user/", &sub_name[2..]].concat())));
	}

	// Request subreddit metadata
	let sub = if !sub_name.contains('+') && sub_name != subscribed && sub_name != "popular" && sub_name != "all" {
		// Regular subreddit
		subreddit(&sub_name, quarantined).await.unwrap_or_default()
	} else if sub_name == subscribed {
		// Subscription feed
		if req.uri().path().starts_with("/r/") {
			subreddit(&sub_name, quarantined).await.unwrap_or_default()
		} else {
			Subreddit::default()
		}
	} else {
		// Multireddit, all, popular
		Subreddit {
			name: sub_name.clone(),
			..Subreddit::default()
		}
	};

	let req_url = req.uri().to_string();
	// Return landing page if this post if this is NSFW community but the user
	// has disabled the display of NSFW content or if the instance is SFW-only.
	if sub.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
		if fragment_mode == FragmentMode::Posts {
			let response = listing::render_posts_fragment(
				Vec::new(),
				prefs,
				req_url,
				PostRenderKind::Direct,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			)?;
			return Ok(with_active_feed(response, &active_feed));
		}
		return Ok(listing::document_response(nsfw_landing(req, req_url).await.unwrap_or_default()));
	}

	let mut params = String::from("&raw_json=1");
	if sub_name == "popular" {
		let geo_filter = match GEO_FILTER_MATCH.captures(&query) {
			Some(geo_filter) => geo_filter["region"].to_string(),
			None => "GLOBAL".to_owned(),
		};
		params.push_str(&format!("&geo_filter={geo_filter}"));
	}

	let path = format!("/r/{}/{sort}.json?{query}{params}", sub_name.replace('+', "%2B"));
	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let listing_url = if root_request {
		canonical_feed_url(&active_feed, &sort, req.uri().query())
	} else {
		url.clone()
	};
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");
	let filters = get_filters(&req);

	// If all requested subs are filtered, we don't need to fetch posts.
	if sub_name.split('+').all(|s| filters.contains(s)) {
		if fragment_mode == FragmentMode::Posts {
			let response = listing::render_posts_fragment(
				Vec::new(),
				prefs,
				url,
				PostRenderKind::Direct,
				active_feed.clone(),
				String::new(),
				String::new(),
				ListingStatus::End,
			)?;
			return Ok(with_active_feed(response, &active_feed));
		}
		let response = template(&SubredditTemplate {
			sub,
			posts: Vec::new(),
			sort: (sort, param(&path, "t").unwrap_or_default()),
			previous_url: String::new(),
			next_url: String::new(),
			prefs,
			url,
			redirect_url,
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			listing_status: ListingStatus::End.as_str().to_string(),
			visible_count: 0,
			feed_groups,
			active_feed: active_feed.clone(),
			active_feed_name,
			active_communities,
		});
		Ok(with_active_feed(listing::document_response(response), &active_feed))
	} else {
		let policy = match ListingPolicy::for_request(
			&req,
			front_page_request.then(|| active_communities.clone()),
			front_page_request && active_group.is_some() && active_communities.len() > 1,
			sort == "new",
		) {
			Ok(policy) => policy,
			Err(_) => {
				let response = listing::policy_unavailable_response(req, fragment_mode).await?;
				return Ok(with_active_feed(response, &active_feed));
			}
		};
		match listing::accumulate(&path, quarantined, policy).await {
			Ok(mut result) => {
				crate::activity::annotate(&req, &mut result.posts)?;
				if front_page_request {
					if let Some(context) = account::context(&req) {
						if active_group.is_some() {
							crate::sources::observe(&account::open_database()?, context.profile_id, &active_feed, &result.posts, account::now()).map_err(|e| format!("{e:?}"))?;
						}
						let items = result.posts.iter().map(crate::editions::Item::from_post).collect::<Vec<_>>();
						let _ = crate::agenda::observe(&account::open_database()?, context.profile_id, &active_feed, &items, account::now());
					}
				}
				let previous_url = result.previous_url(&listing_url);
				let next_url = result.next_url(&listing_url);
				let all_posts_filtered = result.all_posts_filtered();
				let all_posts_hidden_nsfw = result.all_posts_hidden_nsfw();
				let no_posts = result.no_posts();
				let listing_status = result.status;
				let visible_count = result.posts.len();
				if fragment_mode == FragmentMode::Posts {
					let response = listing::render_posts_fragment(
						result.posts,
						prefs,
						url,
						PostRenderKind::Direct,
						active_feed.clone(),
						previous_url,
						next_url,
						listing_status,
					)?;
					return Ok(with_active_feed(response, &active_feed));
				}
				let response = template(&SubredditTemplate {
					sub,
					posts: result.posts,
					sort: (sort, param(&path, "t").unwrap_or_default()),
					previous_url,
					next_url,
					prefs,
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
					listing_status: listing_status.as_str().to_string(),
					visible_count,
					feed_groups,
					active_feed: active_feed.clone(),
					active_feed_name,
					active_communities,
				});
				Ok(with_active_feed(listing::document_response(response), &active_feed))
			}
			Err(msg) => {
				if fragment_mode == FragmentMode::Posts {
					return Ok(with_active_feed(listing::fragment_unavailable_response(), &active_feed));
				}
				let response = match msg.as_str() {
					"quarantined" | "gated" => quarantine(&req, sub_name, &msg),
					"private" => error(req, &format!("r/{sub_name} is a private community")).await?,
					"banned" => error(req, &format!("r/{sub_name} has been banned from Reddit")).await?,
					_ => error(req, &msg).await?,
				};
				Ok(with_active_feed(listing::document_response(response), &active_feed))
			}
		}
	}
}

pub fn quarantine(req: &Request<Body>, sub: String, restriction: &str) -> Response<Body> {
	let wall = WallTemplate {
		title: format!("r/{sub} is {restriction}"),
		msg: "Please click the button below to continue to this subreddit.".to_string(),
		url: req.uri().to_string(),
		sub,
		prefs: Preferences::new(req),
	};

	Response::builder()
		.status(403)
		.header("content-type", "text/html")
		.body(wall.render().unwrap_or_default().into())
		.unwrap_or_default()
}

pub async fn add_quarantine_exception(req: Request<Body>) -> Result<Response<Body>, String> {
	let subreddit = req.param("sub").ok_or("Invalid URL")?;
	let redir = param(&format!("?{}", req.uri().query().unwrap_or_default()), "redir").ok_or("Invalid URL")?;
	let mut response = redirect(&safe_local_redirect(&redir, "/", 2_048));
	response.insert_cookie(
		Cookie::build((&format!("allow_quaran_{}", subreddit.to_lowercase()), "true"))
			.path("/")
			.http_only(true)
			.secure(cookie_is_secure())
			.same_site(SameSite::Lax)
			.expires(cookie::Expiration::Session)
			.into(),
	);
	Ok(response)
}

pub fn can_access_quarantine(req: &Request<Body>, sub: &str) -> bool {
	// Determine if the subreddit can be accessed
	setting(req, &format!("allow_quaran_{}", sub.to_lowercase())).parse().unwrap_or_default()
}

// Join items and split the resulting cookie value into chunks of at most 4000
// bytes. Request parsing concatenates the numbered cookies before splitting on
// `+`, so chunks may safely end in the middle of an item.
pub fn join_until_size_limit<T: std::fmt::Display>(vec: &[T]) -> Vec<std::string::String> {
	const COOKIE_VALUE_BYTES: usize = 4000;
	let mut joined = String::new();
	for (index, item) in vec.iter().enumerate() {
		if index > 0 {
			joined.push('+');
		}
		joined.push_str(&item.to_string());
	}
	if joined.is_empty() {
		return vec![joined];
	}

	let mut chunks = Vec::new();
	let mut start = 0;
	while start < joined.len() {
		let mut end = (start + COOKIE_VALUE_BYTES).min(joined.len());
		while end > start && !joined.is_char_boundary(end) {
			end -= 1;
		}
		chunks.push(joined[start..end].to_string());
		start = end;
	}
	chunks
}

// Sub, filter, unfilter, or unsub by setting subscription cookie using response "Set-Cookie" header
pub async fn subscriptions_filters(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_default();
	let action: Vec<String> = req.uri().path().split('/').map(String::from).collect();

	// Handle random subreddits
	if sub == "random" || sub == "randnsfw" {
		if action.contains(&"filter".to_string()) || action.contains(&"unfilter".to_string()) {
			return Err("Can't filter random subreddit!".to_string());
		}
		return Err("Can't subscribe to random subreddit!".to_string());
	}

	let query = req.uri().query().unwrap_or_default().to_string();

	let mut preferences = Preferences::new(&req);
	let server_backed = account::server_backed(&req);
	let mut feed_groups = preferences.feed_groups();
	let mut active_feed = preferences.active_feed_group().map(|group| group.slug).unwrap_or_default();
	let mut sub_list = preferences.subscriptions.clone();
	let mut filters = preferences.filters.clone();

	// Retrieve list of posts for these subreddits to extract display names

	let posts = json(format!("/r/{sub}/hot.json?raw_json=1"), true).await;
	let display_lookup: Vec<(String, &str)> = match &posts {
		Ok(posts) => posts["data"]["children"]
			.as_array()
			.map(|list| {
				list
					.iter()
					.map(|post| {
						let display_name = post["data"]["subreddit"].as_str().unwrap_or_default();
						(display_name.to_lowercase(), display_name)
					})
					.collect::<Vec<_>>()
			})
			.unwrap_or_default(),
		Err(_) => vec![],
	};

	// Find each subreddit name (separated by '+') in sub parameter
	for part in sub.split('+').filter(|x| x != &"") {
		// Retrieve display name for the subreddit
		let display;
		let part = if part.starts_with("u_") {
			part
		} else if let Some(&(_, display)) = display_lookup.iter().find(|x| x.0 == part.to_lowercase()) {
			// This is already known, doesn't require separate request
			display
		} else {
			// This subreddit display name isn't known, retrieve it
			let path: String = format!("/r/{part}/about.json?raw_json=1");
			display = json(path, true).await;
			match &display {
				Ok(display) => display["data"]["display_name"].as_str(),
				Err(_) => None,
			}
			.unwrap_or(part)
		};

		// Modify sub list based on action
		if action.contains(&"subscribe".to_string()) && !sub_list.contains(&part.to_owned()) {
			// Add each sub name to the subscribed list
			sub_list.push(part.to_owned());
			filters.retain(|s| s.to_lowercase() != part.to_lowercase());
			if !feed_groups.is_empty() {
				for group in &mut feed_groups {
					group.communities.retain(|community| !community.eq_ignore_ascii_case(part));
				}
				let active_index = feed_groups.iter().position(|group| group.slug == active_feed).unwrap_or(0);
				feed_groups[active_index].communities.push(part.to_owned());
			}
			// Reorder sub names alphabetically
			sub_list.sort_by_key(|a| a.to_lowercase());
			filters.sort_by_key(|a| a.to_lowercase());
		} else if action.contains(&"unsubscribe".to_string()) {
			// Remove sub name from subscribed list
			sub_list.retain(|s| s.to_lowercase() != part.to_lowercase());
			for group in &mut feed_groups {
				group.communities.retain(|community| !community.eq_ignore_ascii_case(part));
			}
		} else if action.contains(&"filter".to_string()) && !filters.contains(&part.to_owned()) {
			// Add each sub name to the filtered list
			filters.push(part.to_owned());
			sub_list.retain(|s| s.to_lowercase() != part.to_lowercase());
			for group in &mut feed_groups {
				group.communities.retain(|community| !community.eq_ignore_ascii_case(part));
			}
			// Reorder sub names alphabetically
			filters.sort_by_key(|a| a.to_lowercase());
			sub_list.sort_by_key(|a| a.to_lowercase());
		} else if action.contains(&"unfilter".to_string()) {
			// Remove sub name from filtered list
			filters.retain(|s| s.to_lowercase() != part.to_lowercase());
		}
	}

	// Redirect back to subreddit
	// check for redirect parameter if unsubscribing/unfiltering from outside sidebar
	let fallback = format!("/r/{sub}");
	let path = param(&format!("?{query}"), "redirect")
		.map(|redirect_path| safe_local_redirect(&format!("/{redirect_path}"), &fallback, 2_048))
		.unwrap_or(fallback);

	let mut response = redirect(&path);
	if server_backed {
		if !feed_groups.iter().any(|group| group.slug == active_feed) {
			active_feed = feed_groups.first().map(|group| group.slug.clone()).unwrap_or_default();
		}
		preferences.subscriptions = sub_list;
		preferences.filters = filters;
		preferences.feed_groups = serialize_feed_groups(&feed_groups);
		account::save_preferences(&req, &preferences)?;
		if feed_groups.is_empty() {
			response.remove_cookie("active_feed".to_string());
		} else {
			response.insert_cookie(account::active_feed_cookie(active_feed));
		}
		return Ok(response);
	}

	// If sub_list is empty remove all subscriptions cookies, otherwise update them and remove old ones
	if sub_list.is_empty() {
		// Remove subscriptions cookie
		response.remove_cookie("subscriptions".to_string());

		// Start with first numbered subscriptions cookie
		let mut subscriptions_number = 1;

		// While whatever subscriptionsNUMBER cookie we're looking at has a value
		while req.cookie(&format!("subscriptions{subscriptions_number}")).is_some() {
			// Remove that subscriptions cookie
			response.remove_cookie(format!("subscriptions{subscriptions_number}"));

			// Increment subscriptions cookie number
			subscriptions_number += 1;
		}
	} else {
		// Start at 0 to keep track of what number we need to start deleting old subscription cookies from
		let mut subscriptions_number_to_delete_from = 0;

		// Starting at 0 so we handle the subscription cookie without a number first
		for (subscriptions_number, list) in join_until_size_limit(&sub_list).into_iter().enumerate() {
			let subscriptions_cookie = if subscriptions_number == 0 {
				"subscriptions".to_string()
			} else {
				format!("subscriptions{subscriptions_number}")
			};

			response.insert_cookie(
				Cookie::build((subscriptions_cookie, list))
					.path("/")
					.http_only(true)
					.secure(cookie_is_secure())
					.same_site(SameSite::Lax)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			);

			subscriptions_number_to_delete_from += 1;
		}

		// While whatever subscriptionsNUMBER cookie we're looking at has a value
		while req.cookie(&format!("subscriptions{subscriptions_number_to_delete_from}")).is_some() {
			// Remove that subscriptions cookie
			response.remove_cookie(format!("subscriptions{subscriptions_number_to_delete_from}"));

			// Increment subscriptions cookie number
			subscriptions_number_to_delete_from += 1;
		}
	}

	// If filters is empty remove all filters cookies, otherwise update them and remove old ones
	if filters.is_empty() {
		// Remove filters cookie
		response.remove_cookie("filters".to_string());

		// Start with first numbered filters cookie
		let mut filters_number = 1;

		// While whatever filtersNUMBER cookie we're looking at has a value
		while req.cookie(&format!("filters{filters_number}")).is_some() {
			// Remove that filters cookie
			response.remove_cookie(format!("filters{filters_number}"));

			// Increment filters cookie number
			filters_number += 1;
		}
	} else {
		// Start at 0 to keep track of what number we need to start deleting old filters cookies from
		let mut filters_number_to_delete_from = 0;

		for (filters_number, list) in join_until_size_limit(&filters).into_iter().enumerate() {
			let filters_cookie = if filters_number == 0 {
				"filters".to_string()
			} else {
				format!("filters{filters_number}")
			};

			response.insert_cookie(
				Cookie::build((filters_cookie, list))
					.path("/")
					.http_only(true)
					.secure(cookie_is_secure())
					.same_site(SameSite::Lax)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			);

			filters_number_to_delete_from += 1;
		}

		// While whatever filtersNUMBER cookie we're looking at has a value
		while req.cookie(&format!("filters{filters_number_to_delete_from}")).is_some() {
			// Remove that filters cookie
			response.remove_cookie(format!("filters{filters_number_to_delete_from}"));

			// Increment filters cookie number
			filters_number_to_delete_from += 1;
		}
	}

	if feed_groups.is_empty() {
		response.remove_cookie("feed_groups".to_string());
		response.remove_cookie("active_feed".to_string());
	} else {
		if !feed_groups.iter().any(|group| group.slug == active_feed) {
			active_feed = feed_groups.first().map(|group| group.slug.clone()).unwrap_or_default();
		}
		response.insert_cookie(
			Cookie::build(("feed_groups", encode_cookie_text(&serialize_feed_groups(&feed_groups))))
				.path("/")
				.http_only(true)
				.secure(cookie_is_secure())
				.same_site(SameSite::Lax)
				.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
				.into(),
		);
		response.insert_cookie(
			Cookie::build(("active_feed", active_feed))
				.path("/")
				.http_only(true)
				.secure(cookie_is_secure())
				.same_site(SameSite::Lax)
				.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
				.into(),
		);
	}

	Ok(response)
}

pub async fn wiki(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let quarantined = can_access_quarantine(&req, &sub);
	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/wiki").await {
		return Ok(random);
	}

	let page = req.param("page").unwrap_or_else(|| "index".to_string());
	let path: String = format!("/r/{sub}/wiki/{page}.json?raw_json=1");
	let url = req.uri().to_string();

	match json(path, quarantined).await {
		Ok(response) => Ok(template(&WikiTemplate {
			sub,
			wiki: rewrite_urls(response["data"]["content_html"].as_str().unwrap_or("<h3>Wiki not found</h3>")),
			page,
			prefs: Preferences::new(&req),
			url,
		})),
		Err(msg) => {
			if msg == "quarantined" || msg == "gated" {
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

pub async fn sidebar(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let quarantined = can_access_quarantine(&req, &sub);

	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/about/sidebar").await {
		return Ok(random);
	}

	// Build the Reddit JSON API url
	let path: String = format!("/r/{sub}/about.json?raw_json=1");
	let url = req.uri().to_string();

	// Send a request to the url
	match json(path, quarantined).await {
		// If success, receive JSON in response
		Ok(response) => Ok(template(&WikiTemplate {
			wiki: rewrite_urls(&val(&response, "description_html")),
			sub,
			page: "Sidebar".to_string(),
			prefs: Preferences::new(&req),
			url,
		})),
		Err(msg) => {
			if msg == "quarantined" || msg == "gated" {
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

// SUBREDDIT
async fn subreddit(sub: &str, quarantined: bool) -> Result<Subreddit, String> {
	// Build the Reddit JSON API url
	let path: String = format!("/r/{sub}/about.json?raw_json=1");

	// Send a request to the url
	let res = json(path, quarantined).await?;

	// Metadata regarding the subreddit
	let members: i64 = res["data"]["subscribers"].as_u64().unwrap_or_default() as i64;
	let active: i64 = res["data"]["accounts_active"].as_u64().unwrap_or_default() as i64;

	// Fetch subreddit icon either from the community_icon or icon_img value
	let community_icon: &str = res["data"]["community_icon"].as_str().unwrap_or_default();
	let icon = if community_icon.is_empty() { val(&res, "icon_img") } else { community_icon.to_string() };

	Ok(Subreddit {
		name: val(&res, "display_name"),
		title: val(&res, "title"),
		description: val(&res, "public_description"),
		info: crate::html::sanitize_subreddit_info(&val(&res, "description_html")),
		// moderators: moderators_list(sub, quarantined).await.unwrap_or_default(),
		icon: crate::html::sanitize_subreddit_image_source(&icon),
		members: format_num(members),
		active: format_num(active),
		wiki: res["data"]["wiki_enabled"].as_bool().unwrap_or_default(),
		nsfw: res["data"]["over18"].as_bool().unwrap_or_default(),
	})
}

pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if config::get_setting("REDLIB_ENABLE_RSS").is_none() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}

	use hyper::header::CONTENT_TYPE;

	// Get subreddit
	let sub = req.param("sub").unwrap_or_default();
	let post_sort = req.cookie("post_sort").map_or_else(|| "hot".to_string(), |c| c.value().to_string());
	let sort = req.param("sort").unwrap_or_else(|| req.param("id").unwrap_or(post_sort));

	// Get path
	let path = format!("/r/{sub}/{sort}.json?{}", req.uri().query().unwrap_or_default());

	// Get subreddit link
	let subreddit_link: String = format!("{}/r/{sub}", config::get_setting("REDLIB_FULL_URL").unwrap_or_default());

	// Get subreddit data
	let subreddit = subreddit(&sub, false).await?;

	// Get posts
	let (posts, _) = Post::fetch(&path, false).await?;

	// Build the RSS feed
	let channel = ChannelBuilder::default()
		.title(&subreddit.title)
		.description(&subreddit.description)
		.link(&subreddit_link)
		.items(
			posts
				.into_iter()
				.map(|post| {
					let decoded_body = decode_html(&post.body).unwrap_or_else(|_| post.body.clone());
					let mut item = Item {
						title: Some(post.title.to_string()),
						link: Some(format_url(&utils::get_post_url(&post))),
						author: Some(post.author.name.to_string()),
						content: Some(rewrite_urls(&decoded_body)),
						pub_date: Some(DateTime::from_timestamp(post.created_ts as i64, 0).unwrap_or_default().to_rfc2822()),
						description: Some(format!("<a href='{}'>Comments</a>", to_absolute_url(&post.permalink))),
						..Default::default()
					};

					apply_enclosure(&mut item, &post);
					item
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

// Set enclosure image for RSS feed item
fn apply_enclosure(item: &mut Item, post: &Post) {
	item.set_enclosure(get_rss_image(post));

	// Embed the number of gallery images in description and content since
	// only the first image in the gallery is used for the enclosure
	if post.post_type == "gallery" && post.gallery.len() > 1 {
		item.set_description(format!("<a href='{}'>Gallery with {} images</a>", to_absolute_url(&post.permalink), post.gallery.len()));

		if let Some(content) = item.content() {
			let new_content = format!("{}<br/>{}", item.description().unwrap_or(""), content,);
			item.set_content(new_content);
		}
	}
}

fn get_rss_image(post: &Post) -> Option<Enclosure> {
	let image_url = match post.post_type.as_str() {
		"image" => Some(post.media.url.clone()),
		"gallery" => post.gallery.first().and_then(|media| decode_html(&media.url).ok()),
		"gif" | "video" => decode_html(&post.media.poster).ok(),
		_ => None,
	};

	image_url.map(|url| {
		let mut enclosure = Enclosure::default();
		enclosure.set_mime_type(get_mime_type(&url));
		enclosure.set_url(to_absolute_url(&url));
		enclosure.set_length("0");
		enclosure
	})
}

/// Determines the MIME type based on file extension in a URL.
/// Handles both absolute and relative URLs with query parameters.
fn get_mime_type(url: &str) -> &'static str {
	// Extract the path component, removing query parameters
	let path = url.split('?').next().unwrap_or(url);

	// Get the file extension (everything after the last dot)
	let extension = path.rsplit('.').next().unwrap_or("").to_lowercase();

	// Match common image extensions
	match extension.as_str() {
		"jpg" | "jpeg" => "image/jpeg",
		"png" => "image/png",
		"gif" => "image/gif",
		"webp" => "image/webp",
		"svg" => "image/svg+xml",
		_ => "application/octet-stream",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use hyper::{body::to_bytes, header, StatusCode};
	use route_recognizer::Params;

	fn route_request(path: &str, params: &[(&str, &str)]) -> Request<Body> {
		let groups = vec![
			FeedGroup {
				name: "AI & homelab".to_string(),
				slug: "ai-homelab".to_string(),
				communities: vec!["homelab".to_string()],
			},
			FeedGroup {
				name: "Anime news".to_string(),
				slug: "anime-news".to_string(),
				communities: vec!["anime".to_string()],
			},
		];
		let cookies = format!("feed_groups={}; active_feed=ai-homelab; post_sort=new", encode_cookie_text(&serialize_feed_groups(&groups)));
		let mut request = Request::builder().uri(path).header(header::COOKIE, cookies).body(Body::empty()).unwrap();
		let mut route_params = Params::new();
		for (name, value) in params {
			route_params.insert((*name).to_string(), (*value).to_string());
		}
		request.set_params(route_params);
		request
	}

	fn fragment_route_request(path: &str, params: &[(&str, &str)]) -> Request<Body> {
		let mut request = route_request(path, params);
		request.headers_mut().insert("X-Vale-Fragment", "posts-v1".parse().unwrap());
		request
	}

	fn assert_private_variant(response: &Response<Body>) {
		assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store");
		assert_eq!(response.headers()[header::VARY], "X-Vale-Fragment");
	}

	fn render_community_fixture(icon: &str, wiki: bool) -> String {
		SubredditTemplate {
			sub: Subreddit {
				name: "long-community-name-that-wraps-cleanly".to_string(),
				title: "A deliberately long community title that remains readable without truncation".to_string(),
				description: "A public description owned by the hero.".to_string(),
				info: crate::html::sanitize_subreddit_info("<h1>Rules</h1><p>Safe details.</p><script>bad()</script>"),
				icon: crate::html::sanitize_subreddit_image_source(icon),
				members: ("12K".to_string(), "12000".to_string()),
				active: ("42".to_string(), "42".to_string()),
				wiki,
				nsfw: false,
			},
			posts: Vec::new(),
			sort: ("hot".to_string(), "day".to_string()),
			previous_url: String::new(),
			next_url: String::new(),
			prefs: Preferences::default(),
			url: "/r/long-community-name-that-wraps-cleanly/hot".to_string(),
			redirect_url: "/r/long-community-name-that-wraps-cleanly/hot".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: true,
			listing_status: "end".to_string(),
			visible_count: 0,
			feed_groups: Vec::new(),
			active_feed: String::new(),
			active_feed_name: String::new(),
			active_communities: Vec::new(),
		}
		.render()
		.unwrap()
	}

	#[test]
	fn community_template_uses_owned_icon_fallback_and_omits_missing_wiki() {
		let without_icon = render_community_fixture("https://tracker.example/icon.png", false);
		assert!(!without_icon.contains("tracker.example"));
		assert!(!without_icon.contains("data-community-icon>"));
		assert!(without_icon.contains("class=\"community-icon-fallback\" data-community-icon-fallback"));
		assert!(!without_icon.contains(">Wiki</a>"));
		assert!(without_icon.contains("<h2>Rules</h2><p>Safe details.</p>"));
		assert!(!without_icon.contains("bad()"));

		let with_icon = render_community_fixture("https://i.redd.it/icon.png", true);
		assert!(with_icon.contains("src=\"/img/icon.png\""));
		assert!(with_icon.contains("data-community-icon"));
		assert!(with_icon.contains("community-icon-fallback\" hidden data-community-icon-fallback"));
		assert!(with_icon.contains(">Wiki</a>"));
	}

	#[tokio::test]
	async fn root_renders_the_active_feed_and_legacy_entries_still_redirect() {
		let mut request = route_request("/", &[]);
		let cookies = request.headers()[header::COOKIE].to_str().unwrap().to_string();
		request.headers_mut().insert(header::COOKIE, format!("{cookies}; filters=homelab").parse().unwrap());
		let root = front_page(request).await.unwrap();
		assert_eq!(root.status(), StatusCode::OK);
		assert!(root.headers().get(header::LOCATION).is_none());
		assert_private_variant(&root);
		let body = String::from_utf8(to_bytes(root.into_body()).await.unwrap().to_vec()).unwrap();
		assert!(body.contains("/f/ai-homelab/new"));
		assert!(body.contains("aria-label=\"Vale home\" aria-current=\"page\""));

		let without_feeds = front_page(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(without_feeds.status(), StatusCode::SEE_OTHER);
		assert_eq!(without_feeds.headers()[header::LOCATION], "/feeds");
		assert_private_variant(&without_feeds);

		let without_sort = feed_without_sort(route_request("/f/anime-news", &[("feed", "anime-news")])).await.unwrap();
		assert_eq!(without_sort.status(), StatusCode::SEE_OTHER);
		assert_eq!(without_sort.headers()[header::LOCATION], "/f/anime-news/new");
		assert_private_variant(&without_sort);

		let legacy = legacy_front_page(route_request("/top", &[("id", "top")])).await.unwrap();
		assert_eq!(legacy.status(), StatusCode::SEE_OTHER);
		assert_eq!(legacy.headers()[header::LOCATION], "/f/ai-homelab/top");
		assert_private_variant(&legacy);
	}

	#[tokio::test]
	async fn missing_named_feed_is_an_explicit_not_found() {
		let response = feed_without_sort(route_request("/f/missing", &[("feed", "missing")])).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
		assert_private_variant(&response);

		let response = community(route_request("/f/ai-homelab/random", &[("feed", "ai-homelab"), ("sort", "random")]))
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
		assert_private_variant(&response);
	}

	#[tokio::test]
	async fn only_canonical_homogeneous_post_sorts_accept_fragment_mode() {
		let document = community(route_request("/r/rust/comments", &[("sub", "rust"), ("sort", "comments")])).await.unwrap();
		assert_eq!(document.status(), StatusCode::NOT_FOUND);
		assert_private_variant(&document);
		let body = String::from_utf8(to_bytes(document.into_body()).await.unwrap().to_vec()).unwrap();
		assert!(!body.contains("data-vale-listing=\"posts-v1\""));

		let fragment = community(fragment_route_request("/r/rust/comments", &[("sub", "rust"), ("sort", "comments")]))
			.await
			.unwrap();
		assert_eq!(fragment.status(), StatusCode::BAD_REQUEST);
		assert_private_variant(&fragment);
		assert!(fragment.headers().get("X-Vale-Fragment").is_none());
		let body = String::from_utf8(to_bytes(fragment.into_body()).await.unwrap().to_vec()).unwrap();
		assert!(!body.contains("<html"));
		assert!(!body.contains("data-vale-posts-fragment"));

		let alias = community(route_request("/r/rust/best", &[("sub", "rust"), ("sort", "best")])).await.unwrap();
		assert_eq!(alias.status(), StatusCode::SEE_OTHER);
		assert_eq!(alias.headers()[header::LOCATION], "/r/rust/hot");
		assert_private_variant(&alias);

		let fragment_alias = community(fragment_route_request("/r/rust/best", &[("sub", "rust"), ("sort", "best")])).await.unwrap();
		assert_eq!(fragment_alias.status(), StatusCode::BAD_REQUEST);
		assert_private_variant(&fragment_alias);
		assert!(fragment_alias.headers().get("X-Vale-Fragment").is_none());
	}

	#[tokio::test]
	async fn redirect_only_and_missing_feed_routes_reject_fragment_mode() {
		for request in [
			fragment_route_request("/", &[]),
			fragment_route_request("/f/anime-news", &[("feed", "anime-news")]),
			fragment_route_request("/top", &[("id", "top")]),
		] {
			let response = if request.uri().path() == "/" {
				front_page(request).await.unwrap()
			} else if request.uri().path().starts_with("/f/") {
				feed_without_sort(request).await.unwrap()
			} else {
				legacy_front_page(request).await.unwrap()
			};
			assert_eq!(response.status(), StatusCode::BAD_REQUEST);
			assert_private_variant(&response);
			assert!(response.headers().get("X-Vale-Fragment").is_none());
		}

		let missing = community(fragment_route_request("/f/missing/hot", &[("feed", "missing"), ("sort", "hot")])).await.unwrap();
		assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
		assert_private_variant(&missing);
		assert!(missing.headers().get("X-Vale-Fragment").is_none());

		for alias in ["random", "randnsfw", "RaNdOm"] {
			let path = format!("/r/{alias}/hot");
			let response = community(fragment_route_request(&path, &[("sub", alias), ("sort", "hot")])).await.unwrap();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{alias}");
			assert_private_variant(&response);
			assert!(response.headers().get("X-Vale-Fragment").is_none(), "{alias}");
		}
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_fetching_subreddit() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let subreddit = subreddit("rust", false).await;
		assert!(subreddit.is_ok());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_gated_and_quarantined() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let quarantined = subreddit("edgy", true).await;
		assert!(quarantined.is_ok());
		let gated = subreddit("drugs", true).await;
		assert!(gated.is_ok());
	}

	#[test]
	fn cookie_chunks_preserve_exact_joined_value_and_size_bound() {
		let values = vec!["a".repeat(3_500), "β".repeat(400), "z".repeat(1_000)];
		let chunks = join_until_size_limit(&values);
		assert!(chunks.iter().all(|chunk| chunk.len() <= 4_000));
		assert_eq!(chunks.concat(), values.join("+"));
		assert_eq!(join_until_size_limit::<String>(&[]), vec![String::new()]);
	}
}

#[cfg(test)]
mod reading_fixture_tests {
	use super::*;
	#[tokio::test]
	async fn reading_feed_fixture() {
		for theme in ["dark", "light"] {
			let groups = crate::reading_fixtures::feeds();
			let html = SubredditTemplate {
				sub: Subreddit::default(),
				posts: crate::reading_fixtures::posts().await,
				sort: ("hot".into(), "day".into()),
				previous_url: String::new(),
				next_url: String::new(),
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/f/field-notes/hot".into(),
				redirect_url: String::new(),
				is_filtered: false,
				all_posts_filtered: false,
				all_posts_hidden_nsfw: false,
				no_posts: false,
				listing_status: "complete".into(),
				visible_count: 8,
				active_feed: "field-notes".into(),
				active_feed_name: "Field notes".into(),
				active_communities: groups[0].communities.clone(),
				feed_groups: groups,
			}
			.render()
			.unwrap();
			assert!(html.contains("href=\"/r/woodworking/comments/post0/discussion/\""));
			assert!(html.contains("href=\"/r/woodworking/comments/post0/discussion/#comments\""));
			assert!(html.contains("class=\"preview-footer\""));
			assert!(html.contains("<button type=\"button\" class=\"post_thumbnail\" data-inline-toggle=\"inline-post-post2\""));
			assert!(html.contains("data-inline-toggle=\"inline-post-post0\""));
			assert!(html.contains("data-src=\"/scenes/vale-light.webp\""));
			crate::reading_fixtures::export(theme, "feed.html", &html);
		}
	}
}

#[cfg(test)]
mod wiki_surface_fixture_tests {
	use super::*;
	#[test]
	fn wiki_surface_fixture() {
		for theme in ["dark", "light"] {
			let html = WikiTemplate { sub: "woodworking".into(), page: "Getting started".into(), wiki: "<h2>Welcome to the workshop</h2><p>Start with a clear surface, good light, and enough room to work. This guide collects the community’s most useful advice.</p><h2>Choosing tools</h2><p>Choose tools for the work you actually do. Learn their limits and take care of them.</p><pre>A deliberately long line should scroll inside its own code block rather than widening the whole page.</pre>".into(),
                prefs: crate::reading_fixtures::preferences(theme), url: "/r/woodworking/wiki/index".into() }.render().unwrap();
			assert!(html.contains("Community pages"));
			assert!(html.contains("Welcome to the workshop"));
			crate::reading_fixtures::export(theme, "wiki.html", &html);
			let wall = WallTemplate {
				title: "Community notice".into(),
				sub: "review-community".into(),
				msg: "This community requires an explicit choice before continuing.".into(),
				prefs: crate::reading_fixtures::preferences(theme),
				url: "/r/review-community".into(),
			}
			.render()
			.unwrap();
			crate::reading_fixtures::export(theme, "wall.html", &wall);
		}
	}
}
