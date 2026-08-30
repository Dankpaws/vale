use crate::config::{self, get_setting};
use crate::{client::json, server::RequestExt};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cookie::Cookie;
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use hyper::{body::HttpBody, Body, Request, Response};
use log::error;
use regex::Regex;
use revision::{revisioned, DeserializeRevisioned, SerializeRevisioned};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{Read, Write};
use std::str::FromStr;
use std::string::ToString;
use std::sync::LazyLock;
use time::{macros::format_description, Duration, OffsetDateTime};
use url::Url;

/// Write a message to stderr on debug mode. This function is a no-op on
/// release code.
#[macro_export]
macro_rules! dbg_msg {
	($x:expr) => {
		#[cfg(debug_assertions)]
		eprintln!("{}:{}: {}", file!(), line!(), $x.to_string())
	};

	($($x:expr),+) => {
		#[cfg(debug_assertions)]
		dbg_msg!(format!($($x),+))
	};
}

/// Identifies whether or not the page is a subreddit, a user page, or a post.
/// This is used by the NSFW landing template to determine the mesage to convey
/// to the user.
#[derive(PartialEq, Eq)]
pub enum ResourceType {
	Subreddit,
	User,
	Post,
}

/// Post flair with content, background color and foreground color
#[derive(Serialize)]
pub struct Flair {
	pub flair_parts: Vec<FlairPart>,
	pub text: String,
	pub background_color: String,
	pub foreground_color: String,
}

/// Part of flair, either emoji or text
#[derive(Clone, Serialize)]
pub struct FlairPart {
	pub flair_part_type: String,
	pub value: String,
}

impl FlairPart {
	pub fn parse(flair_type: &str, rich_flair: Option<&Vec<Value>>, text_flair: Option<&str>) -> Vec<Self> {
		// Parse type of flair
		match flair_type {
			// If flair contains emojis and text
			"richtext" => match rich_flair {
				Some(rich) => rich
					.iter()
					// For each part of the flair, extract text and emojis
					.map(|part| {
						let value = |name: &str| part[name].as_str().unwrap_or_default();
						Self {
							flair_part_type: value("e").to_string(),
							value: match value("e") {
								"text" => value("t").to_string(),
								"emoji" => format_url(value("u")),
								_ => String::new(),
							},
						}
					})
					.collect::<Vec<Self>>(),
				None => Vec::new(),
			},
			// If flair contains only text
			"text" => match text_flair {
				Some(text) => vec![Self {
					flair_part_type: "text".to_string(),
					value: text.to_string(),
				}],
				None => Vec::new(),
			},
			_ => Vec::new(),
		}
	}
}

#[derive(Serialize)]
pub struct Author {
	pub name: String,
	pub flair: Flair,
	pub distinguished: String,
}

#[derive(Serialize)]
pub struct Poll {
	pub poll_options: Vec<PollOption>,
	pub voting_end_timestamp: (String, String),
	pub total_vote_count: u64,
}

impl Poll {
	pub fn parse(poll_data: &Value) -> Option<Self> {
		poll_data.as_object()?;

		let total_vote_count = poll_data["total_vote_count"].as_u64()?;
		// voting_end_timestamp is in the format of milliseconds
		let voting_end_timestamp = time(poll_data["voting_end_timestamp"].as_f64()? / 1000.0);
		let poll_options = PollOption::parse(&poll_data["options"])?;

		Some(Self {
			poll_options,
			voting_end_timestamp,
			total_vote_count,
		})
	}

	pub fn most_votes(&self) -> u64 {
		self.poll_options.iter().filter_map(|o| o.vote_count).max().unwrap_or(0)
	}
}

#[derive(Serialize)]
pub struct PollOption {
	pub id: u64,
	pub text: String,
	pub vote_count: Option<u64>,
}

impl PollOption {
	pub fn parse(options: &Value) -> Option<Vec<Self>> {
		Some(
			options
				.as_array()?
				.iter()
				.filter_map(|option| {
					// For each poll option

					// we can't just use as_u64() because "id": String("...") and serde would parse it as None
					let id = option["id"].as_str()?.parse::<u64>().ok()?;
					let text = option["text"].as_str()?.to_owned();
					let vote_count = option["vote_count"].as_u64();

					// Construct PollOption items
					Some(Self { id, text, vote_count })
				})
				.collect::<Vec<Self>>(),
		)
	}
}

/// Post flags with NSFW and stickied
#[derive(Serialize)]
pub struct Flags {
	pub spoiler: bool,
	pub nsfw: bool,
	pub stickied: bool,
}

#[derive(Debug, Serialize)]
pub struct Media {
	pub url: String,
	pub alt_url: String,
	pub display_url: String,
	pub srcset: String,
	pub width: i64,
	pub height: i64,
	pub poster: String,
	pub download_name: String,
	pub download_url: String,
}

impl Media {
	pub async fn parse(data: &Value) -> (String, Self, Vec<GalleryMedia>) {
		let mut gallery = Vec::new();
		let permalink_base = url_path_basename(data["permalink"].as_str().unwrap_or_default());

		// Define the various known places that Reddit might put video URLs.
		let data_preview = &data["preview"]["reddit_video_preview"];
		let secure_media = &data["secure_media"]["reddit_video"];
		let crosspost_parent_media = &data["crosspost_parent_list"][0]["secure_media"]["reddit_video"];

		// If post is a video, return the video
		let (post_type, url_val, alt_url_val) = if data_preview["fallback_url"].is_string() {
			(
				if data_preview["is_gif"].as_bool().unwrap_or(false) { "gif" } else { "video" },
				&data_preview["fallback_url"],
				Some(&data_preview["hls_url"]),
			)
		} else if secure_media["fallback_url"].is_string() {
			(
				if secure_media["is_gif"].as_bool().unwrap_or(false) { "gif" } else { "video" },
				&secure_media["fallback_url"],
				Some(&secure_media["hls_url"]),
			)
		} else if crosspost_parent_media["fallback_url"].is_string() {
			(
				if crosspost_parent_media["is_gif"].as_bool().unwrap_or(false) { "gif" } else { "video" },
				&crosspost_parent_media["fallback_url"],
				Some(&crosspost_parent_media["hls_url"]),
			)
		} else if data["post_hint"].as_str().unwrap_or("") == "image" {
			// Handle images, whether GIFs or pics
			let preview = &data["preview"]["images"][0];
			let mp4 = &preview["variants"]["mp4"];

			if mp4.is_object() {
				// Return the mp4 if the media is a gif
				("gif", &mp4["source"]["url"], None)
			} else {
				// Return the picture if the media is an image
				if data["domain"] == "i.redd.it" {
					("image", &data["url"], None)
				} else {
					("image", &preview["source"]["url"], None)
				}
			}
		} else if data["is_self"].as_bool().unwrap_or_default() {
			// If type is self, return permalink
			("self", &data["permalink"], None)
		} else if data["is_gallery"].as_bool().unwrap_or_default() {
			// If this post contains a gallery of images
			gallery = GalleryMedia::parse(&data["gallery_data"]["items"], &data["media_metadata"], &permalink_base);

			("gallery", &data["url"], None)
		} else if data["crosspost_parent_list"][0]["is_gallery"].as_bool().unwrap_or_default() {
			// If this post contains a gallery of images
			gallery = GalleryMedia::parse(
				&data["crosspost_parent_list"][0]["gallery_data"]["items"],
				&data["crosspost_parent_list"][0]["media_metadata"],
				&permalink_base,
			);

			("gallery", &data["url"], None)
		} else if data["is_reddit_media_domain"].as_bool().unwrap_or_default() && data["domain"] == "i.redd.it" {
			// If this post contains a reddit media (image) URL.
			("image", &data["url"], None)
		} else {
			// If type can't be determined, return url
			("link", &data["url"], None)
		};

		let preview = &data["preview"]["images"][0];
		let source = &preview["source"];

		let alt_url = alt_url_val.map_or(String::new(), |val| format_url(val.as_str().unwrap_or_default()));
		let url = format_url(url_val.as_str().unwrap_or_default());
		let poster = format_url(source["url"].as_str().unwrap_or_default());
		let display_url = if post_type == "image" && !poster.is_empty() { poster.clone() } else { url.clone() };
		let srcset = if post_type == "image" {
			responsive_image_srcset(&preview["resolutions"], source)
		} else {
			String::new()
		};

		let download_name = if post_type == "image" || post_type == "gif" || post_type == "video" {
			let media_url_base = url_path_basename(url_val.as_str().unwrap_or_default());

			format!("vale_{permalink_base}_{media_url_base}")
		} else if post_type == "gallery" {
			format!("vale_{permalink_base}_gallery.zip")
		} else {
			String::new()
		};
		let download_url = media_download_url(&url, &download_name);

		(
			post_type.to_string(),
			Self {
				url,
				alt_url,
				display_url,
				srcset,
				// Note: in the data["is_reddit_media_domain"] path above
				// width and height will be 0.
				width: source["width"].as_i64().unwrap_or_default(),
				height: source["height"].as_i64().unwrap_or_default(),
				poster,
				download_name,
				download_url,
			},
			gallery,
		)
	}
}

#[derive(Serialize)]
pub struct GalleryMedia {
	pub url: String,
	pub srcset: String,
	pub original_url: String,
	pub download_url: String,
	pub download_name: String,
	pub width: i64,
	pub height: i64,
	pub caption: String,
	pub outbound_url: String,
}

impl GalleryMedia {
	fn parse(items: &Value, metadata: &Value, permalink_base: &str) -> Vec<Self> {
		items
			.as_array()
			.unwrap_or(&Vec::new())
			.iter()
			.enumerate()
			.map(|(index, item)| {
				// For each image in gallery
				let media_id = item["media_id"].as_str().unwrap_or_default();
				let image = &metadata[media_id]["s"];
				let image_type = &metadata[media_id]["m"];

				let url = if image_type == "image/gif" {
					image["gif"].as_str().unwrap_or_default()
				} else {
					image["u"].as_str().unwrap_or_default()
				};

				let url = format_url(url);
				let original_url = original_gallery_url(&url);
				let original_basename = url_path_basename(&original_url);
				let download_name = format!("vale_{permalink_base}_{:02}_{original_basename}", index + 1);

				// Construct gallery items
				Self {
					srcset: responsive_image_srcset(&metadata[media_id]["p"], image),
					download_url: media_download_url(&original_url, &download_name),
					download_name,
					original_url,
					url,
					width: image["x"].as_i64().unwrap_or_default(),
					height: image["y"].as_i64().unwrap_or_default(),
					caption: item["caption"].as_str().unwrap_or_default().to_string(),
					outbound_url: item["outbound_url"].as_str().unwrap_or_default().to_string(),
				}
			})
			.collect::<Vec<Self>>()
	}
}

/// A second Reddit submission that shares the representative post's exact
/// content identity.
#[derive(Debug, Serialize)]
pub struct GroupedPost {
	pub id: String,
	pub title: String,
	pub community: String,
	pub permalink: String,
	pub score: String,
	pub comments: String,
	pub created: String,
	/// Internal stable-New ordering metadata. These are deliberately omitted
	/// from serialized/template output; they only preserve representative and
	/// grouped-row election semantics while the bounded accumulator is running.
	#[serde(skip)]
	pub(crate) created_ts: u64,
	#[serde(skip)]
	pub(crate) stickied: bool,
}

/// Post containing content, metadata and media
#[derive(Serialize)]
pub struct Post {
	/// Reddit's raw listing fullname (for example, `t3_abc123`). This is kept
	/// separate from the display ID because cursor-safe replenishment must use
	/// the exact upstream boundary rather than reconstructing one.
	pub fullname: String,
	pub id: String,
	pub title: String,
	pub community: String,
	pub body: String,
	pub author: Author,
	pub permalink: String,
	pub link_title: String,
	pub poll: Option<Poll>,
	pub score: (String, String),
	pub upvote_ratio: i64,
	pub post_type: String,
	pub flair: Flair,
	pub flags: Flags,
	pub thumbnail: Media,
	pub media: Media,
	pub domain: String,
	pub rel_time: String,
	pub created: String,
	pub created_ts: u64,
	pub num_duplicates: u64,
	pub comments: (String, String),
	pub gallery: Vec<GalleryMedia>,
	pub awards: Awards,
	pub nsfw: bool,
	pub out_url: Option<String>,
	pub ws_url: String,
	pub content_key: String,
	pub grouped_posts: Vec<GroupedPost>,
	pub combined_url: String,
	/// Number of grouped discussions included by the bounded combined-comments
	/// route. Exact-content membership itself is intentionally not truncated.
	pub group_comment_count: usize,
	/// Exact-content discussions represented in the card but omitted from the
	/// combined-comments request's twelve-discussion safety bound.
	pub group_comment_overflow: usize,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ListingCursors {
	pub before: String,
	pub after: String,
}

/// One raw Reddit listing page, including the stable identity material needed
/// to detect upstream overlap and non-progress without trusting cursors alone.
pub struct ListingPage {
	pub posts: Vec<Post>,
	pub cursors: ListingCursors,
	pub fingerprint: String,
	pub raw_fullnames: Vec<String>,
}

impl Post {
	/// Fetch one bounded page of posts and return Reddit's explicit page cursors.
	pub async fn fetch(path: &str, quarantine: bool) -> Result<(Vec<Self>, ListingCursors), String> {
		let page = Self::fetch_page(path, quarantine).await?;
		Ok((page.posts, page.cursors))
	}

	/// Fetch one bounded page while retaining raw fullnames and a deterministic
	/// content fingerprint for the bounded listing accumulator.
	pub async fn fetch_page(path: &str, quarantine: bool) -> Result<ListingPage, String> {
		// Send a request to the url
		let res = match json(path.to_string(), quarantine).await {
			// If success, receive JSON in response
			Ok(response) => response,
			// If the Reddit API returns an error, exit this function
			Err(msg) => return Err(msg),
		};

		// Fetch the list of posts from the JSON response
		let Some(post_list) = res["data"]["children"].as_array() else {
			return Err("No posts found".to_string());
		};
		let upstream_record_count = post_list.len();
		let post_list = &post_list[..post_list.len().min(LISTING_PAGE_SIZE)];
		let first_fullname = post_list.first().map(|post| val(post, "name")).unwrap_or_default();
		let raw_fullnames = post_list.iter().map(|post| val(post, "name")).collect::<Vec<_>>();
		let reddit_after = res["data"]["after"].as_str().unwrap_or_default();
		let bounded_after = bounded_listing_after(upstream_record_count, &raw_fullnames, reddit_after);
		let fingerprint = {
			let encoded = serde_json::to_vec(post_list).unwrap_or_default();
			Sha256::digest(encoded).iter().map(|byte| format!("{byte:02x}")).collect::<String>()
		};

		let mut posts: Vec<Self> = Vec::new();

		// For each post from posts list
		for post in post_list {
			let data = &post["data"];

			let (rel_time, created) = time(data["created_utc"].as_f64().unwrap_or_default());
			let created_ts = data["created_utc"].as_f64().unwrap_or_default().round() as u64;
			let score = data["score"].as_i64().unwrap_or_default();
			let ratio: f64 = data["upvote_ratio"].as_f64().unwrap_or(1.0) * 100.0;
			let title = val(post, "title");

			// Determine the type of media along with the media URL
			let (post_type, media, gallery) = Media::parse(data).await;
			let content_key = post_content_key(data, &val(post, "id"), &post_type);
			let awards = Awards::parse(&data["all_awardings"]);

			// selftext_html is set for text posts when browsing.
			let mut body = rewrite_urls(&val(post, "selftext_html"));
			if body.is_empty() {
				body = rewrite_urls(&val(post, "body_html"));
			}

			posts.push(Self {
				fullname: val(post, "name"),
				id: val(post, "id"),
				title,
				community: val(post, "subreddit"),
				body,
				author: Author {
					name: val(post, "author"),
					flair: Flair {
						flair_parts: FlairPart::parse(
							data["author_flair_type"].as_str().unwrap_or_default(),
							data["author_flair_richtext"].as_array(),
							data["author_flair_text"].as_str(),
						),
						text: val(post, "link_flair_text"),
						background_color: val(post, "author_flair_background_color"),
						foreground_color: val(post, "author_flair_text_color"),
					},
					distinguished: val(post, "distinguished"),
				},
				score: if data["hide_score"].as_bool().unwrap_or_default() {
					("\u{2022}".to_string(), "Hidden".to_string())
				} else {
					format_num(score)
				},
				upvote_ratio: ratio as i64,
				post_type,
				thumbnail: Media {
					url: format_url(val(post, "thumbnail").as_str()),
					alt_url: String::new(),
					display_url: String::new(),
					srcset: String::new(),
					width: data["thumbnail_width"].as_i64().unwrap_or_default(),
					height: data["thumbnail_height"].as_i64().unwrap_or_default(),
					poster: String::new(),
					download_name: String::new(),
					download_url: String::new(),
				},
				media,
				domain: val(post, "domain"),
				flair: Flair {
					flair_parts: FlairPart::parse(
						data["link_flair_type"].as_str().unwrap_or_default(),
						data["link_flair_richtext"].as_array(),
						data["link_flair_text"].as_str(),
					),
					text: val(post, "link_flair_text"),
					background_color: val(post, "link_flair_background_color"),
					foreground_color: if val(post, "link_flair_text_color") == "dark" {
						"black".to_string()
					} else {
						"white".to_string()
					},
				},
				flags: Flags {
					spoiler: data["spoiler"].as_bool().unwrap_or_default(),
					nsfw: data["over_18"].as_bool().unwrap_or_default(),
					stickied: data["stickied"].as_bool().unwrap_or_default() || data["pinned"].as_bool().unwrap_or_default(),
				},
				permalink: val(post, "permalink"),
				link_title: val(post, "link_title"),
				poll: Poll::parse(&data["poll_data"]),
				rel_time,
				created,
				created_ts,
				num_duplicates: post["data"]["num_duplicates"].as_u64().unwrap_or(0),
				comments: format_num(data["num_comments"].as_i64().unwrap_or_default()),
				gallery,
				awards,
				nsfw: post["data"]["over_18"].as_bool().unwrap_or_default(),
				ws_url: val(post, "websocket_url"),
				out_url: post["data"]["url_overridden_by_dest"].as_str().map(|a| a.to_string()),
				content_key,
				grouped_posts: Vec::new(),
				combined_url: String::new(),
				group_comment_count: 0,
				group_comment_overflow: 0,
			});
		}
		Ok(ListingPage {
			posts,
			cursors: ListingCursors {
				before: listing_before_cursor(path, res["data"]["before"].as_str().unwrap_or_default(), &first_fullname),
				after: bounded_after,
			},
			fingerprint,
			raw_fullnames,
		})
	}
}

/// Reddit normally honors the requested 25-record limit. If it returns an
/// oversized page, its own `after` cursor points beyond records Vale did not
/// accept. Continue from the last accepted raw fullname instead, even when
/// Reddit omitted `after`; otherwise fail closed rather than declaring End and
/// silently skipping the unconsumed tail. An empty result for an oversized
/// page makes the accumulator's no-new-fullname guard mark the snapshot Retry.
fn bounded_listing_after(upstream_record_count: usize, accepted_fullnames: &[String], reddit_after: &str) -> String {
	if upstream_record_count <= accepted_fullnames.len() {
		return reddit_after.to_string();
	}
	accepted_fullnames.iter().rev().find(|fullname| !fullname.is_empty()).cloned().unwrap_or_default()
}

fn listing_before_cursor(request_path: &str, reddit_before: &str, first_fullname: &str) -> String {
	if !reddit_before.is_empty() {
		return reddit_before.to_string();
	}
	let is_cursor_page = ["after", "before"].into_iter().any(|name| param(request_path, name).is_some_and(|value| !value.is_empty()));
	if is_cursor_page {
		first_fullname.to_string()
	} else {
		String::new()
	}
}

fn canonical_content_url(value: &str) -> Option<String> {
	let mut parsed = Url::parse(value).ok()?;
	if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
		return None;
	}
	parsed.set_fragment(None);
	let query = parsed
		.query_pairs()
		.filter(|(key, _)| {
			let key = key.to_ascii_lowercase();
			!key.starts_with("utm_") && !matches!(key.as_str(), "fbclid" | "gclid" | "dclid" | "mc_cid" | "mc_eid")
		})
		.map(|(key, value)| (key.into_owned(), value.into_owned()))
		.collect::<Vec<_>>();
	parsed.set_query(None);
	if !query.is_empty() {
		let encoded = query
			.iter()
			.fold(url::form_urlencoded::Serializer::new(String::new()), |mut serializer, (key, value)| {
				serializer.append_pair(key, value);
				serializer
			})
			.finish();
		parsed.set_query(Some(&encoded));
	}
	Some(parsed.to_string())
}

fn post_content_key(data: &Value, post_id: &str, post_type: &str) -> String {
	let crosspost_parent = data["crosspost_parent"].as_str().unwrap_or_default().trim_start_matches("t3_");
	if post_type == "self" {
		return format!("reddit:{}", if crosspost_parent.is_empty() { post_id } else { crosspost_parent });
	}
	let destination = data["url_overridden_by_dest"].as_str().or_else(|| data["url"].as_str()).unwrap_or_default();
	canonical_content_url(destination).map(|url| format!("url:{url}")).unwrap_or_else(|| {
		if crosspost_parent.is_empty() {
			String::new()
		} else {
			format!("reddit:{crosspost_parent}")
		}
	})
}

pub const COMBINED_COMMENT_POST_LIMIT: usize = 12;

/// Refresh bounded combined-comment metadata after exact-content membership
/// changes. All members stay on the listing card; only the retrieval URL is
/// capped, so the interface can disclose any overflow honestly.
pub fn refresh_group_metadata(representative: &mut Post) {
	let total = representative.grouped_posts.len().saturating_add(1);
	representative.group_comment_count = total.min(COMBINED_COMMENT_POST_LIMIT);
	representative.group_comment_overflow = total.saturating_sub(representative.group_comment_count);
	let ids = std::iter::once(representative.id.as_str())
		.chain(
			representative
				.grouped_posts
				.iter()
				.take(representative.group_comment_count.saturating_sub(1))
				.map(|post| post.id.as_str()),
		)
		.collect::<Vec<_>>()
		.join(",");
	representative.combined_url = if total > 1 { format!("/combined?posts={ids}") } else { String::new() };
}

pub fn grouped_post(post: Post) -> GroupedPost {
	GroupedPost {
		id: post.id,
		title: post.title,
		community: post.community,
		permalink: post.permalink,
		score: post.score.0,
		comments: post.comments.0,
		created: post.created,
		created_ts: post.created_ts,
		stickied: post.flags.stickied,
	}
}

pub fn group_feed_posts(posts: &mut Vec<Post>, show_nsfw: bool) {
	let mut grouped = Vec::with_capacity(posts.len());
	let mut by_identity: HashMap<String, usize> = HashMap::new();
	for post in posts.drain(..) {
		if post.content_key.is_empty() || (post.flags.nsfw && !show_nsfw) {
			grouped.push(post);
			continue;
		}
		if let Some(index) = by_identity.get(&post.content_key).copied() {
			let representative: &mut Post = &mut grouped[index];
			representative.grouped_posts.push(grouped_post(post));
			refresh_group_metadata(representative);
		} else {
			by_identity.insert(post.content_key.clone(), grouped.len());
			grouped.push(post);
		}
	}
	*posts = grouped;
}

pub const LISTING_PAGE_SIZE: usize = 25;

fn caller_listing_state_parameter(name: &str) -> bool {
	matches!(
		name,
		"fragment" | "seen" | "seen_ids" | "group" | "group_state" | "target" | "target_count" | "profile" | "profile_state"
	) || name.starts_with("vale_")
}

/// Remove client-only and caller-controlled page-size parameters before
/// forwarding a listing query to Reddit, then enforce Vale's bounded page.
pub fn listing_query(query: &str) -> String {
	let mut serializer = url::form_urlencoded::parse(query.as_bytes())
		.filter(|(key, _)| key != "limit" && !caller_listing_state_parameter(key))
		.fold(url::form_urlencoded::Serializer::new(String::new()), |mut serializer, (key, value)| {
			serializer.append_pair(&key, &value);
			serializer
		});
	serializer.append_pair("limit", &LISTING_PAGE_SIZE.to_string());
	serializer.finish()
}

/// Build a finite cursor-page URL while retaining route/search semantics and
/// replacing any prior cursor or retired infinite-listing parameters.
pub fn listing_cursor_url(current: &str, cursor_name: &str, cursor: &str) -> String {
	if cursor.is_empty() || !matches!(cursor_name, "after" | "before") {
		return String::new();
	}
	let Ok(parsed) = Url::parse(&format!("https://vale.invalid{current}")) else {
		return String::new();
	};
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	for (key, value) in parsed.query_pairs() {
		if !matches!(key.as_ref(), "after" | "before" | "limit") && !caller_listing_state_parameter(&key) {
			serializer.append_pair(&key, &value);
		}
	}
	serializer.append_pair(cursor_name, cursor);
	let query = serializer.finish();
	if query.is_empty() {
		parsed.path().to_string()
	} else {
		format!("{}?{query}", parsed.path())
	}
}

pub const FEED_SORTS: [&str; 5] = ["hot", "new", "top", "rising", "controversial"];

pub fn canonical_feed_sort(value: &str) -> Option<&'static str> {
	match value {
		"best" | "hot" => Some("hot"),
		"new" => Some("new"),
		"top" => Some("top"),
		"rising" => Some("rising"),
		"controversial" => Some("controversial"),
		_ => None,
	}
}

pub fn preferred_feed_sort(value: &str) -> &'static str {
	canonical_feed_sort(value).unwrap_or("hot")
}

pub fn canonical_feed_path(feed_slug: &str, sort: &str) -> String {
	format!("/f/{feed_slug}/{}", preferred_feed_sort(sort))
}

pub fn canonical_feed_url(feed_slug: &str, sort: &str, query: Option<&str>) -> String {
	let path = canonical_feed_path(feed_slug, sort);
	match query.filter(|query| !query.is_empty()) {
		Some(query) => format!("{path}?{query}"),
		None => path,
	}
}

/// Whether a request URL names one canonical, sorted feed home. Keeping this
/// check here prevents the global brand from claiming the current page on
/// community, post, search, or compatibility routes.
pub fn is_canonical_feed_home(url: &str) -> bool {
	let path = url.split_once('?').map_or(url, |(path, _)| path);
	let mut segments = path.trim_start_matches('/').split('/');
	matches!(
		(segments.next(), segments.next(), segments.next(), segments.next()),
		(Some("f"), Some(feed), Some(sort), None)
			if !feed.is_empty() && FEED_SORTS.contains(&sort)
	)
}

#[derive(Template)]
#[template(path = "comment.html")]
/// Comment with content, post, score and data/time that it was posted
pub struct Comment {
	pub id: String,
	pub kind: String,
	pub parent_id: String,
	pub parent_kind: String,
	pub post_link: String,
	pub post_author: String,
	pub body: String,
	pub author: Author,
	pub score: (String, String),
	pub rel_time: String,
	pub created: String,
	pub edited: (String, String),
	pub replies: Vec<Comment>,
	pub highlighted: bool,
	pub awards: Awards,
	pub collapsed: bool,
	pub is_filtered: bool,
	pub is_keyword_filtered: bool,
	pub more_count: i64,
	pub prefs: Preferences,
	pub node_id: String,
	pub parent_node_id: String,
	pub ancestor_path: String,
	pub ancestor_path_complete: bool,
	pub depth: usize,
	pub preorder: usize,
	pub filter_state: String,
	pub continuation_state: String,
	pub continuation_children: String,
	pub thread_root_id: String,
	pub parent_author: String,
	pub parent_available: bool,
	pub wide_indent: usize,
	pub narrow_indent: usize,
	pub reply_region_id: String,
	pub projected_reply_count: usize,
	pub projected_replies_complete: bool,
	pub is_group_root: bool,
	pub search_match: bool,
	pub search_context: bool,
}

#[derive(Default, Clone, Serialize)]
pub struct Award {
	pub name: String,
	pub icon_url: String,
	pub description: String,
	pub count: i64,
}

impl std::fmt::Display for Award {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} {} {}", self.name, self.icon_url, self.description)
	}
}

#[derive(Default, Serialize)]
pub struct Awards(pub Vec<Award>);

impl std::ops::Deref for Awards {
	type Target = Vec<Award>;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl std::fmt::Display for Awards {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.iter().try_fold((), |_, award| writeln!(f, "{award}"))
	}
}

impl Awards {
	/// Convert Reddit awards JSON to Awards struct
	pub fn parse(items: &Value) -> Self {
		let parsed = items.as_array().unwrap_or(&Vec::new()).iter().fold(Vec::new(), |mut awards, item| {
			let name = item["name"].as_str().unwrap_or_default().to_string();
			let icon_url = format_url(item["resized_icons"][0]["url"].as_str().unwrap_or_default());
			let description = item["description"].as_str().unwrap_or_default().to_string();
			let count: i64 = i64::from_str(&item["count"].to_string()).unwrap_or(1);

			awards.push(Award {
				name,
				icon_url,
				description,
				count,
			});

			awards
		});

		Self(parsed)
	}
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
	pub msg: String,
	pub prefs: Preferences,
	pub url: String,
}

#[derive(Template)]
#[template(path = "info.html")]
pub struct InfoTemplate {
	pub msg: String,
	pub prefs: Preferences,
	pub url: String,
}

/// Template for NSFW landing page. The landing page is displayed when a page's
/// content is wholly NSFW, but a user has not enabled the option to view NSFW
/// posts.
#[derive(Template)]
#[template(path = "nsfwlanding.html")]
pub struct NSFWLandingTemplate {
	/// Identifier for the resource. This is either a subreddit name or a
	/// username. (In the case of the latter, set is_user to true.)
	pub res: String,

	/// Identifies whether or not the resource is a subreddit, a user page,
	/// or a post.
	pub res_type: ResourceType,

	/// User preferences.
	pub prefs: Preferences,

	/// Request URL.
	pub url: String,
}

#[derive(Default)]
/// User struct containing metadata about user
pub struct User {
	pub name: String,
	pub title: String,
	pub icon: String,
	pub karma: i64,
	pub created: String,
	pub banner: String,
	pub description: String,
	pub nsfw: bool,
}

#[derive(Default)]
/// Subreddit struct containing metadata about community
pub struct Subreddit {
	pub name: String,
	pub title: String,
	pub description: String,
	pub info: String,
	pub icon: String,
	pub members: (String, String),
	pub active: (String, String),
	pub wiki: bool,
	pub nsfw: bool,
}

/// Parser for query params, used in sorting (eg. /r/rust/?sort=hot)
#[derive(serde::Deserialize)]
pub struct Params {
	pub t: Option<String>,
	pub q: Option<String>,
	pub sort: Option<String>,
	pub after: Option<String>,
	pub before: Option<String>,
}

#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FeedGroup {
	pub name: String,
	pub slug: String,
	pub communities: Vec<String>,
}

/// Map every inherited Redlib theme value onto Vale's three-state theme
/// contract. Unknown legacy values are dark-family values by default; only
/// the known light palettes migrate to Light.
pub fn canonical_theme(value: &str) -> String {
	let value = value.trim();
	if value.eq_ignore_ascii_case("system") || value.is_empty() {
		"system".to_string()
	} else if ["light", "libredditlight", "gruvboxlight"].iter().any(|light| value.eq_ignore_ascii_case(light)) {
		"light".to_string()
	} else {
		"dark".to_string()
	}
}

#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[revisioned(revision = 6)]
pub struct Preferences {
	#[revision(start = 1)]
	#[serde(skip_serializing, skip_deserializing)]
	pub available_themes: Vec<String>,
	#[revision(start = 1)]
	pub theme: String,
	#[revision(start = 1)]
	pub front_page: String,
	#[revision(start = 1)]
	pub layout: String,
	#[revision(start = 1)]
	pub wide: String,
	#[revision(start = 1)]
	pub blur_spoiler: String,
	#[revision(start = 1)]
	pub show_nsfw: String,
	#[revision(start = 1)]
	pub blur_nsfw: String,
	#[revision(start = 1)]
	pub hide_hls_notification: String,
	#[revision(start = 1)]
	pub video_quality: String,
	#[revision(start = 1)]
	pub hide_sidebar_and_summary: String,
	#[revision(start = 1)]
	pub use_hls: String,
	#[revision(start = 1)]
	pub autoplay_videos: String,
	#[revision(start = 1)]
	pub fixed_navbar: String,
	#[revision(start = 1)]
	pub disable_visit_reddit_confirmation: String,
	#[revision(start = 1)]
	pub comment_sort: String,
	#[revision(start = 1)]
	pub post_sort: String,
	#[revision(start = 1)]
	#[serde(serialize_with = "serialize_vec_with_plus", deserialize_with = "deserialize_vec_with_plus")]
	pub subscriptions: Vec<String>,
	#[revision(start = 1)]
	#[serde(serialize_with = "serialize_vec_with_plus", deserialize_with = "deserialize_vec_with_plus")]
	pub filters: Vec<String>,
	#[revision(start = 1)]
	pub hide_awards: String,
	#[revision(start = 1)]
	pub hide_score: String,
	#[revision(start = 1)]
	pub remove_default_feeds: String,
	#[revision(start = 2, default_fn = "default_collapse_child_comments")]
	pub collapse_child_comments: String,
	#[revision(start = 3, default_fn = "default_comment_filter_keywords")]
	pub comment_filter_keywords: String,
	#[revision(start = 3, default_fn = "default_feed_groups")]
	pub feed_groups: String,
	#[revision(start = 3, default_fn = "default_active_feed")]
	pub active_feed: String,
	#[revision(start = 4, default_fn = "default_keyboard_navigation")]
	#[serde(default)]
	pub keyboard_navigation: String,
	#[revision(start = 4, default_fn = "default_key_next_post")]
	#[serde(default)]
	pub key_next_post: String,
	#[revision(start = 4, default_fn = "default_key_previous_post")]
	#[serde(default)]
	pub key_previous_post: String,
	#[revision(start = 4, default_fn = "default_key_open_post")]
	#[serde(default)]
	pub key_open_post: String,
	#[revision(start = 4, default_fn = "default_key_toggle_preview")]
	#[serde(default)]
	pub key_toggle_preview: String,
	#[revision(start = 4, default_fn = "default_key_hide_post")]
	#[serde(default)]
	pub key_hide_post: String,
	#[revision(start = 4, default_fn = "default_hide_post_behavior")]
	#[serde(default)]
	pub hide_post_behavior: String,
	#[revision(start = 6, default_fn = "default_archive_budget_mib")]
	#[serde(default)]
	pub archive_budget_mib: u64,
}

#[derive(Deserialize)]
struct LegacyPreferencesV2 {
	#[serde(skip_deserializing)]
	available_themes: Vec<String>,
	theme: String,
	front_page: String,
	layout: String,
	wide: String,
	blur_spoiler: String,
	show_nsfw: String,
	blur_nsfw: String,
	hide_hls_notification: String,
	video_quality: String,
	hide_sidebar_and_summary: String,
	use_hls: String,
	autoplay_videos: String,
	fixed_navbar: String,
	disable_visit_reddit_confirmation: String,
	comment_sort: String,
	post_sort: String,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	subscriptions: Vec<String>,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	filters: Vec<String>,
	hide_awards: String,
	hide_score: String,
	remove_default_feeds: String,
	collapse_child_comments: String,
}

#[derive(Deserialize)]
struct LegacyPreferencesV1 {
	#[serde(skip_deserializing)]
	available_themes: Vec<String>,
	theme: String,
	front_page: String,
	layout: String,
	wide: String,
	blur_spoiler: String,
	show_nsfw: String,
	blur_nsfw: String,
	hide_hls_notification: String,
	video_quality: String,
	hide_sidebar_and_summary: String,
	use_hls: String,
	autoplay_videos: String,
	fixed_navbar: String,
	disable_visit_reddit_confirmation: String,
	comment_sort: String,
	post_sort: String,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	subscriptions: Vec<String>,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	filters: Vec<String>,
	hide_awards: String,
	hide_score: String,
	remove_default_feeds: String,
}

impl From<LegacyPreferencesV2> for Preferences {
	fn from(value: LegacyPreferencesV2) -> Self {
		Self {
			available_themes: value.available_themes,
			theme: value.theme,
			front_page: value.front_page,
			layout: value.layout,
			wide: value.wide,
			blur_spoiler: value.blur_spoiler,
			show_nsfw: value.show_nsfw,
			blur_nsfw: value.blur_nsfw,
			hide_hls_notification: value.hide_hls_notification,
			video_quality: value.video_quality,
			hide_sidebar_and_summary: value.hide_sidebar_and_summary,
			use_hls: value.use_hls,
			autoplay_videos: value.autoplay_videos,
			fixed_navbar: value.fixed_navbar,
			disable_visit_reddit_confirmation: value.disable_visit_reddit_confirmation,
			comment_sort: value.comment_sort,
			post_sort: value.post_sort,
			subscriptions: value.subscriptions,
			filters: value.filters,
			hide_awards: value.hide_awards,
			hide_score: value.hide_score,
			remove_default_feeds: value.remove_default_feeds,
			collapse_child_comments: value.collapse_child_comments,
			comment_filter_keywords: String::new(),
			feed_groups: String::new(),
			active_feed: String::new(),
			keyboard_navigation: "on".to_string(),
			key_next_post: "j".to_string(),
			key_previous_post: "k".to_string(),
			key_open_post: "Enter".to_string(),
			key_toggle_preview: "e".to_string(),
			key_hide_post: "h".to_string(),
			hide_post_behavior: "instant".to_string(),
			archive_budget_mib: 0,
		}
	}
}

impl From<LegacyPreferencesV1> for Preferences {
	fn from(value: LegacyPreferencesV1) -> Self {
		LegacyPreferencesV2 {
			available_themes: value.available_themes,
			theme: value.theme,
			front_page: value.front_page,
			layout: value.layout,
			wide: value.wide,
			blur_spoiler: value.blur_spoiler,
			show_nsfw: value.show_nsfw,
			blur_nsfw: value.blur_nsfw,
			hide_hls_notification: value.hide_hls_notification,
			video_quality: value.video_quality,
			hide_sidebar_and_summary: value.hide_sidebar_and_summary,
			use_hls: value.use_hls,
			autoplay_videos: value.autoplay_videos,
			fixed_navbar: value.fixed_navbar,
			disable_visit_reddit_confirmation: value.disable_visit_reddit_confirmation,
			comment_sort: value.comment_sort,
			post_sort: value.post_sort,
			subscriptions: value.subscriptions,
			filters: value.filters,
			hide_awards: value.hide_awards,
			hide_score: value.hide_score,
			remove_default_feeds: value.remove_default_feeds,
			collapse_child_comments: "off".to_string(),
		}
		.into()
	}
}

fn serialize_vec_with_plus<S>(vec: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_str(&vec.join("+"))
}

fn deserialize_vec_with_plus<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
	D: Deserializer<'de>,
{
	let string = String::deserialize(deserializer)?;
	if string.is_empty() {
		return Ok(Vec::new());
	}
	Ok(string.split('+').map(|s| s.to_string()).collect())
}

impl Preferences {
	fn default_collapse_child_comments(_revision: u16) -> Result<String, revision::Error> {
		Ok("off".to_owned())
	}

	fn default_comment_filter_keywords(_revision: u16) -> Result<String, revision::Error> {
		Ok(String::new())
	}

	fn default_feed_groups(_revision: u16) -> Result<String, revision::Error> {
		Ok(String::new())
	}

	fn default_active_feed(_revision: u16) -> Result<String, revision::Error> {
		Ok(String::new())
	}

	fn default_keyboard_navigation(_revision: u16) -> Result<String, revision::Error> {
		Ok("on".to_owned())
	}

	fn default_key_next_post(_revision: u16) -> Result<String, revision::Error> {
		Ok("j".to_owned())
	}

	fn default_key_previous_post(_revision: u16) -> Result<String, revision::Error> {
		Ok("k".to_owned())
	}

	fn default_key_open_post(_revision: u16) -> Result<String, revision::Error> {
		Ok("Enter".to_owned())
	}

	fn default_key_toggle_preview(_revision: u16) -> Result<String, revision::Error> {
		Ok("e".to_owned())
	}

	fn default_key_hide_post(_revision: u16) -> Result<String, revision::Error> {
		Ok("h".to_owned())
	}

	fn default_hide_post_behavior(_revision: u16) -> Result<String, revision::Error> {
		Ok("instant".to_owned())
	}

	fn default_archive_budget_mib(_revision: u16) -> Result<u64, revision::Error> {
		Ok(0)
	}

	/// Fill preferences added by Vale when loading older JSON profiles or
	/// browser cookies that predate those settings.
	pub fn apply_reader_defaults(&mut self) {
		self.theme = canonical_theme(&self.theme);
		// Vale has one intentional responsive interface. These legacy fields
		// remain serialized only so pre-Vale cookie and export formats continue
		// to decode without changing the rendered product.
		self.front_page = "default".to_string();
		self.layout = "compact".to_string();
		self.wide = "on".to_string();
		self.fixed_navbar = "on".to_string();
		self.remove_default_feeds = "on".to_string();
		self.hide_sidebar_and_summary = "off".to_string();
		if self.keyboard_navigation.is_empty() {
			self.keyboard_navigation = "on".to_string();
		}
		if self.key_next_post.is_empty() {
			self.key_next_post = "j".to_string();
		}
		if self.key_previous_post.is_empty() {
			self.key_previous_post = "k".to_string();
		}
		if self.key_open_post.is_empty() {
			self.key_open_post = "Enter".to_string();
		}
		if self.key_toggle_preview.is_empty() {
			self.key_toggle_preview = "e".to_string();
		}
		if self.key_hide_post.is_empty() {
			self.key_hide_post = "h".to_string();
		}
		// Keep the serialized field so older LUR backups remain decodable, but
		// Vale now has one releaseable Hide contract: instant plus stacked Undo.
		self.hide_post_behavior = "instant".to_string();
	}

	fn available_themes() -> Vec<String> {
		vec!["system".to_string(), "light".to_string(), "dark".to_string()]
	}

	/// Build browser-local preferences from cookies and instance defaults.
	pub fn from_browser(req: &Request<Body>) -> Self {
		let mut preferences = Self {
			available_themes: Self::available_themes(),
			theme: req.cookie("theme").map_or_else(|| "system".to_string(), |cookie| canonical_theme(cookie.value())),
			front_page: setting(req, "front_page"),
			layout: setting(req, "layout"),
			wide: setting(req, "wide"),
			blur_spoiler: setting(req, "blur_spoiler"),
			show_nsfw: setting(req, "show_nsfw"),
			hide_sidebar_and_summary: setting(req, "hide_sidebar_and_summary"),
			blur_nsfw: setting(req, "blur_nsfw"),
			use_hls: setting(req, "use_hls"),
			hide_hls_notification: setting(req, "hide_hls_notification"),
			video_quality: setting(req, "video_quality"),
			autoplay_videos: setting(req, "autoplay_videos"),
			fixed_navbar: setting_or_default(req, "fixed_navbar", "on".to_string()),
			disable_visit_reddit_confirmation: setting(req, "disable_visit_reddit_confirmation"),
			comment_sort: setting(req, "comment_sort"),
			post_sort: setting(req, "post_sort"),
			subscriptions: setting(req, "subscriptions").split('+').map(String::from).filter(|s| !s.is_empty()).collect(),
			filters: setting(req, "filters").split('+').map(String::from).filter(|s| !s.is_empty()).collect(),
			hide_awards: setting(req, "hide_awards"),
			hide_score: setting(req, "hide_score"),
			remove_default_feeds: setting(req, "remove_default_feeds"),
			collapse_child_comments: setting(req, "collapse_child_comments"),
			comment_filter_keywords: decode_cookie_text(&setting(req, "comment_filter_keywords")),
			feed_groups: decode_cookie_text(&setting(req, "feed_groups")),
			active_feed: setting(req, "active_feed"),
			keyboard_navigation: setting_or_default(req, "keyboard_navigation", "on".to_string()),
			key_next_post: setting_or_default(req, "key_next_post", "j".to_string()),
			key_previous_post: setting_or_default(req, "key_previous_post", "k".to_string()),
			key_open_post: setting_or_default(req, "key_open_post", "Enter".to_string()),
			key_toggle_preview: setting_or_default(req, "key_toggle_preview", "e".to_string()),
			key_hide_post: setting_or_default(req, "key_hide_post", "h".to_string()),
			hide_post_behavior: setting_or_default(req, "hide_post_behavior", "instant".to_string()),
			archive_budget_mib: 0,
		};
		preferences.apply_reader_defaults();
		preferences
	}

	/// Resolve preferences from the authenticated/shared server profile when
	/// present, otherwise retain Redlib's browser-cookie behavior.
	pub fn new(req: &Request<Body>) -> Self {
		let Some(mut preferences) = crate::account::stored_preferences(req) else {
			return Self::from_browser(req);
		};
		preferences.apply_reader_defaults();
		preferences.available_themes = Self::available_themes();
		let device_feed = setting(req, "active_feed");
		let groups = preferences.feed_groups();
		preferences.active_feed = if groups.iter().any(|group| group.slug == device_feed) {
			device_feed
		} else {
			groups.first().map(|group| group.slug.clone()).unwrap_or_default()
		};
		preferences
	}

	pub fn comment_keywords(&self) -> Vec<String> {
		parse_comment_keywords(&self.comment_filter_keywords)
	}

	pub fn feed_groups(&self) -> Vec<FeedGroup> {
		parse_feed_groups(&self.feed_groups)
	}

	pub fn active_feed_group(&self) -> Option<FeedGroup> {
		let groups = self.feed_groups();
		groups.iter().find(|group| group.slug == self.active_feed).cloned().or_else(|| groups.first().cloned())
	}

	pub fn to_urlencoded(&self) -> Result<String, String> {
		serde_urlencoded::to_string(self).map_err(|e| e.to_string())
	}

	pub fn to_bincode(&self) -> Result<Vec<u8>, String> {
		self.validate_archive_budget()?;
		let mut bytes = b"VAL1".to_vec();
		self.serialize_revisioned(&mut bytes).map_err(|error| error.to_string())?;
		Ok(bytes)
	}

	pub fn from_bincode(bytes: &[u8]) -> Result<Self, String> {
		if let Some(bytes) = bytes
			.strip_prefix(b"VAL1")
			.or_else(|| bytes.strip_prefix(b"LUR5"))
			.or_else(|| bytes.strip_prefix(b"LUR4"))
			.or_else(|| bytes.strip_prefix(b"LUR3"))
		{
			let mut reader = bytes;
			let mut preferences = Self::deserialize_revisioned(&mut reader).map_err(|error| error.to_string())?;
			preferences.validate_archive_budget()?;
			preferences.apply_reader_defaults();
			return Ok(preferences);
		}

		let mut preferences = bincode::deserialize::<Self>(bytes)
			.or_else(|_| bincode::deserialize::<LegacyPreferencesV2>(bytes).map(Into::into))
			.or_else(|_| bincode::deserialize::<LegacyPreferencesV1>(bytes).map(Into::into))
			.map_err(|error| error.to_string())?;
		preferences.validate_archive_budget()?;
		preferences.apply_reader_defaults();
		Ok(preferences)
	}

	pub fn validate_archive_budget(&self) -> Result<(), String> {
		if self.archive_budget_mib == 0 || (self.archive_budget_mib >= 256 && self.archive_budget_mib.is_multiple_of(256)) {
			Ok(())
		} else {
			Err("Archive budgets must use the instance maximum or a whole 256 MiB step.".to_string())
		}
	}
	pub fn to_compressed_bincode(&self) -> Result<Vec<u8>, String> {
		deflate_compress(self.to_bincode()?)
	}
	pub fn to_bincode_str(&self) -> Result<String, String> {
		Ok(base2048::encode(&self.to_compressed_bincode()?))
	}
}

pub fn encode_cookie_text(value: &str) -> String {
	format!("b64.{}", URL_SAFE_NO_PAD.encode(value.as_bytes()))
}

pub fn decode_cookie_text(value: &str) -> String {
	value
		.strip_prefix("b64.")
		.and_then(|encoded| URL_SAFE_NO_PAD.decode(encoded).ok())
		.and_then(|bytes| String::from_utf8(bytes).ok())
		.unwrap_or_else(|| value.to_string())
}

pub fn parse_comment_keywords(value: &str) -> Vec<String> {
	let mut seen = HashSet::new();
	value
		.split([',', '\n', '\r'])
		.map(str::trim)
		.filter(|keyword| !keyword.is_empty())
		.filter_map(|keyword| {
			let mut end = keyword.len().min(60);
			while !keyword.is_char_boundary(end) {
				end -= 1;
			}
			let keyword = keyword[..end].to_string();
			let normalized = keyword.to_lowercase();
			seen.insert(normalized).then_some(keyword)
		})
		.take(30)
		.collect()
}

pub fn canonical_comment_keywords(value: &str) -> String {
	parse_comment_keywords(value).join("\n")
}

pub fn comment_matches_keywords(body: &str, keywords: &[String]) -> bool {
	let body = body.to_lowercase();
	keywords.iter().any(|keyword| body.contains(&keyword.to_lowercase()))
}

pub fn normalize_community_name(value: &str) -> Option<String> {
	let mut name = value.trim().trim_matches('/');
	if name.get(..2).is_some_and(|prefix| prefix.eq_ignore_ascii_case("r/")) {
		name = &name[2..];
	}
	if name.is_empty() || name.len() > 50 || !name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_') {
		return None;
	}
	Some(name.to_string())
}

pub fn normalize_feed_name(value: &str) -> Option<String> {
	let name = value.split_whitespace().collect::<Vec<_>>().join(" ");
	if name.is_empty() || name.len() > 40 || name.chars().any(char::is_control) {
		None
	} else {
		Some(name)
	}
}

pub fn feed_slug(value: &str) -> String {
	let mut slug = String::new();
	let mut separator = false;
	for character in value.chars().flat_map(char::to_lowercase) {
		if character.is_ascii_alphanumeric() {
			if separator && !slug.is_empty() {
				slug.push('-');
			}
			slug.push(character);
			separator = false;
		} else {
			separator = true;
		}
		if slug.len() >= 32 {
			break;
		}
	}
	let slug = slug.trim_matches('-').to_string();
	if slug.is_empty() {
		"feed".to_string()
	} else {
		slug
	}
}

pub fn sanitize_feed_groups(groups: &[FeedGroup]) -> Vec<FeedGroup> {
	let mut clean = Vec::new();
	let mut used_slugs = HashSet::new();
	let mut used_communities = HashSet::new();

	for group in groups.iter().take(8) {
		let Some(name) = normalize_feed_name(&group.name) else {
			continue;
		};
		let base_slug = feed_slug(&name);
		let mut slug = base_slug.clone();
		let mut suffix = 2;
		while used_slugs.contains(&slug) {
			slug = format!("{}-{suffix}", base_slug.chars().take(28).collect::<String>());
			suffix += 1;
		}
		used_slugs.insert(slug.clone());

		let remaining = 32usize.saturating_sub(used_communities.len());
		let communities = group
			.communities
			.iter()
			.filter_map(|community| normalize_community_name(community))
			.filter(|community| used_communities.insert(community.to_lowercase()))
			.take(remaining)
			.collect();

		clean.push(FeedGroup { name, slug, communities });
	}

	clean
}

pub fn parse_feed_groups(value: &str) -> Vec<FeedGroup> {
	serde_json::from_str::<Vec<FeedGroup>>(value).map_or_else(|_| Vec::new(), |groups| sanitize_feed_groups(&groups))
}

pub fn serialize_feed_groups(groups: &[FeedGroup]) -> String {
	serde_json::to_string(&sanitize_feed_groups(groups)).unwrap_or_else(|_| "[]".to_string())
}

pub fn deflate_compress(i: Vec<u8>) -> Result<Vec<u8>, String> {
	let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
	e.write_all(&i).map_err(|e| e.to_string())?;
	e.finish().map_err(|e| e.to_string())
}

/// Maximum size of the uncompressed preference export accepted from a
/// browser. The export fields are all bounded, so a larger value indicates a
/// malformed or deliberately expanded payload rather than a useful backup.
pub const MAX_DECOMPRESSED_PREFERENCES_BYTES: usize = 1024 * 1024;

/// Preference cookies follow the same transport setting as native account
/// sessions. Plain-HTTP loopback installs must opt out explicitly.
pub fn cookie_is_secure() -> bool {
	get_setting("VALE_COOKIE_SECURE").is_none_or(|value| value != "off")
}

pub fn deflate_decompress(i: Vec<u8>) -> Result<Vec<u8>, String> {
	let decoder = DeflateDecoder::new(&i[..]);
	let mut out = Vec::with_capacity(i.len().min(MAX_DECOMPRESSED_PREFERENCES_BYTES));
	decoder
		.take((MAX_DECOMPRESSED_PREFERENCES_BYTES as u64).saturating_add(1))
		.read_to_end(&mut out)
		.map_err(|e| format!("Failed to read from deflate decoder: {e}"))?;
	if out.len() > MAX_DECOMPRESSED_PREFERENCES_BYTES {
		return Err(format!("Decompressed preference data exceeds the {} byte limit", MAX_DECOMPRESSED_PREFERENCES_BYTES));
	}
	Ok(out)
}

/// Read an HTTP body without buffering more than `maximum` bytes. The upper
/// size hint is checked before reading, and streamed chunks are checked again
/// so chunked requests cannot bypass the limit.
pub async fn read_body_limited(body: &mut Body, maximum: usize, too_large_message: &str) -> Result<Vec<u8>, String> {
	if body.size_hint().upper().is_some_and(|size| size > maximum as u64) {
		return Err(too_large_message.to_string());
	}

	let mut output = Vec::new();
	while let Some(chunk) = body.data().await {
		let chunk = chunk.map_err(|error| error.to_string())?;
		if chunk.len() > maximum.saturating_sub(output.len()) {
			return Err(too_large_message.to_string());
		}
		output.extend_from_slice(&chunk);
	}
	Ok(output)
}

/// Gets the active profile's filters for the given request.
pub fn get_filters(req: &Request<Body>) -> HashSet<String> {
	Preferences::new(req).filters.into_iter().collect::<HashSet<String>>()
}

/// Filters a `Vec<Post>` by the given `HashSet` of filters (each filter being
/// a subreddit name or a user name). If a `Post`'s subreddit or author is
/// found in the filters, it is removed.
///
/// The first value of the return tuple is the number of posts filtered. The
/// second return value is `true` if all posts were filtered.
pub fn filter_posts(posts: &mut Vec<Post>, filters: &HashSet<String>) -> (u64, bool) {
	// This is the length of the Vec<Post> prior to applying the filter.
	let lb: u64 = posts.len().try_into().unwrap_or(0);

	if posts.is_empty() {
		(0, false)
	} else {
		posts.retain(|p| !(filters.contains(&p.community) || filters.contains(&["u_", &p.author.name].concat())));

		// Get the length of the Vec<Post> after applying the filter.
		// If lb > la, then at least one post was removed.
		let la: u64 = posts.len().try_into().unwrap_or(0);

		(lb - la, posts.is_empty())
	}
}

/// Creates a [`Post`] from a provided JSON.
pub async fn parse_post(post: &Value) -> Post {
	// Grab UTC time as unix timestamp
	let (rel_time, created) = time(post["data"]["created_utc"].as_f64().unwrap_or_default());
	// Parse post score and upvote ratio
	let score = post["data"]["score"].as_i64().unwrap_or_default();
	let ratio: f64 = post["data"]["upvote_ratio"].as_f64().unwrap_or(1.0) * 100.0;

	// Determine the type of media along with the media URL
	let (post_type, media, gallery) = Media::parse(&post["data"]).await;
	let content_key = post_content_key(&post["data"], &val(post, "id"), &post_type);

	let created_ts = post["data"]["created_utc"].as_f64().unwrap_or_default().round() as u64;

	let awards: Awards = Awards::parse(&post["data"]["all_awardings"]);

	let permalink = val(post, "permalink");

	let poll = Poll::parse(&post["data"]["poll_data"]);

	let body = if val(post, "removed_by_category") == "moderator" {
		format!(
			"<div class=\"md\"><p>[removed] — <a href=\"https://{}{permalink}\">view removed post</a></p></div>",
			get_setting("REDLIB_PUSHSHIFT_FRONTEND").unwrap_or_else(|| String::from(crate::config::DEFAULT_PUSHSHIFT_FRONTEND)),
		)
	} else {
		let selftext = val(post, "selftext");
		if selftext.contains("```") {
			let mut html_output = String::new();
			let parser = pulldown_cmark::Parser::new(&selftext);
			pulldown_cmark::html::push_html(&mut html_output, parser);
			rewrite_urls(&html_output)
		} else {
			rewrite_urls(&val(post, "selftext_html"))
		}
	};

	// Build a post using data parsed from Reddit post API
	Post {
		fullname: val(post, "name"),
		id: val(post, "id"),
		title: val(post, "title"),
		community: val(post, "subreddit"),
		body,
		author: Author {
			name: val(post, "author"),
			flair: Flair {
				flair_parts: FlairPart::parse(
					post["data"]["author_flair_type"].as_str().unwrap_or_default(),
					post["data"]["author_flair_richtext"].as_array(),
					post["data"]["author_flair_text"].as_str(),
				),
				text: val(post, "link_flair_text"),
				background_color: val(post, "author_flair_background_color"),
				foreground_color: val(post, "author_flair_text_color"),
			},
			distinguished: val(post, "distinguished"),
		},
		permalink,
		link_title: val(post, "link_title"),
		poll,
		score: format_num(score),
		upvote_ratio: ratio as i64,
		post_type,
		media,
		thumbnail: Media {
			url: format_url(val(post, "thumbnail").as_str()),
			alt_url: String::new(),
			display_url: String::new(),
			srcset: String::new(),
			width: post["data"]["thumbnail_width"].as_i64().unwrap_or_default(),
			height: post["data"]["thumbnail_height"].as_i64().unwrap_or_default(),
			poster: String::new(),
			download_name: String::new(),
			download_url: String::new(),
		},
		flair: Flair {
			flair_parts: FlairPart::parse(
				post["data"]["link_flair_type"].as_str().unwrap_or_default(),
				post["data"]["link_flair_richtext"].as_array(),
				post["data"]["link_flair_text"].as_str(),
			),
			text: val(post, "link_flair_text"),
			background_color: val(post, "link_flair_background_color"),
			foreground_color: if val(post, "link_flair_text_color") == "dark" {
				"black".to_string()
			} else {
				"white".to_string()
			},
		},
		flags: Flags {
			spoiler: post["data"]["spoiler"].as_bool().unwrap_or_default(),
			nsfw: post["data"]["over_18"].as_bool().unwrap_or_default(),
			stickied: post["data"]["stickied"].as_bool().unwrap_or_default() || post["data"]["pinned"].as_bool().unwrap_or(false),
		},
		domain: val(post, "domain"),
		rel_time,
		created,
		created_ts,
		num_duplicates: post["data"]["num_duplicates"].as_u64().unwrap_or(0),
		comments: format_num(post["data"]["num_comments"].as_i64().unwrap_or_default()),
		gallery,
		awards,
		nsfw: post["data"]["over_18"].as_bool().unwrap_or_default(),
		ws_url: val(post, "websocket_url"),
		out_url: post["data"]["url_overridden_by_dest"].as_str().map(|a| a.to_string()),
		content_key,
		grouped_posts: Vec::new(),
		combined_url: String::new(),
		group_comment_count: 0,
		group_comment_overflow: 0,
	}
}

//
// FORMATTING
//

/// Grab a query parameter from a url
pub fn param(path: &str, value: &str) -> Option<String> {
	Some(
		Url::parse(format!("https://libredd.it/{path}").as_str())
			.ok()?
			.query_pairs()
			.into_owned()
			.collect::<HashMap<_, _>>()
			.get(value)?
			.clone(),
	)
}

/// Retrieve the value of a setting by name
pub fn setting(req: &Request<Body>, name: &str) -> String {
	// Parse a cookie value from request

	// If this was called with "subscriptions" and the "subscriptions" cookie has a value
	if name == "subscriptions" && req.cookie("subscriptions").is_some() {
		// Create subscriptions string
		let mut subscriptions = String::new();

		// Default subscriptions cookie
		if req.cookie("subscriptions").is_some() {
			subscriptions.push_str(req.cookie("subscriptions").unwrap().value());
		}

		// Start with first numbered subscription cookie
		let mut subscriptions_number = 1;

		// While whatever subscriptionsNUMBER cookie we're looking at has a value
		while req.cookie(&format!("subscriptions{subscriptions_number}")).is_some() {
			// Push whatever subscriptionsNUMBER cookie we're looking at into the subscriptions string
			subscriptions.push_str(req.cookie(&format!("subscriptions{subscriptions_number}")).unwrap().value());

			// Increment subscription cookie number
			subscriptions_number += 1;
		}

		// Return the subscriptions cookies as one large string
		subscriptions
	}
	// If this was called with "filters" and the "filters" cookie has a value
	else if name == "filters" && req.cookie("filters").is_some() {
		// Create filters string
		let mut filters = String::new();

		// Default filters cookie
		if req.cookie("filters").is_some() {
			filters.push_str(req.cookie("filters").unwrap().value());
		}

		// Start with first numbered filters cookie
		let mut filters_number = 1;

		// While whatever filtersNUMBER cookie we're looking at has a value
		while req.cookie(&format!("filters{filters_number}")).is_some() {
			// Push whatever filtersNUMBER cookie we're looking at into the filters string
			filters.push_str(req.cookie(&format!("filters{filters_number}")).unwrap().value());

			// Increment filters cookie number
			filters_number += 1;
		}

		// Return the filters cookies as one large string
		filters
	}
	// The above two still come to this if there was no existing value
	else {
		req
			.cookie(name)
			.unwrap_or_else(|| {
				// If there is no cookie for this setting, try receiving a default from the config
				if let Some(default) = get_setting(&format!("REDLIB_DEFAULT_{}", name.to_uppercase())) {
					Cookie::new(name, default)
				} else {
					Cookie::from(name)
				}
			})
			.value()
			.to_string()
	}
}

/// Retrieve the value of a setting by name or the default value
pub fn setting_or_default(req: &Request<Body>, name: &str, default: String) -> String {
	let value = setting(req, name);
	if value.is_empty() {
		default
	} else {
		value
	}
}

/// Detect and redirect in the event of a random subreddit
pub async fn catch_random(sub: &str, additional: &str) -> Result<Response<Body>, String> {
	if sub == "random" || sub == "randnsfw" {
		Ok(redirect(&format!(
			"/r/{}{additional}",
			json(format!("/r/{sub}/about.json?raw_json=1"), false).await?["data"]["display_name"]
				.as_str()
				.unwrap_or_default()
		)))
	} else {
		Err("No redirect needed".to_string())
	}
}

static REGEX_URL_WWW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://www\.reddit\.com/(.*)").unwrap());
static REGEX_URL_OLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://old\.reddit\.com/(.*)").unwrap());
static REGEX_URL_NP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://np\.reddit\.com/(.*)").unwrap());
static REGEX_URL_PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://reddit\.com/(.*)").unwrap());
static REGEX_URL_VIDEOS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://v\.redd\.it/(.*)/DASH_([0-9]{2,4}(\.mp4|$|\?source=fallback))").unwrap());
static REGEX_URL_VIDEOS_HLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://v\.redd\.it/(.+)/(HLSPlaylist\.m3u8.*)$").unwrap());
static REGEX_URL_IMAGES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://i\.redd\.it/(.*)").unwrap());
static REGEX_URL_THUMBS_A: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://a\.thumbs\.redditmedia\.com/(.*)").unwrap());
static REGEX_URL_THUMBS_B: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://b\.thumbs\.redditmedia\.com/(.*)").unwrap());
static REGEX_URL_EMOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://emoji\.redditmedia\.com/(.*)/(.*)").unwrap());
static REGEX_URL_PREVIEW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://preview\.redd\.it/(.*)").unwrap());
static REGEX_URL_EXTERNAL_PREVIEW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://external\-preview\.redd\.it/(.*)").unwrap());
static REGEX_URL_STYLES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://styles\.redditmedia\.com/(.*)").unwrap());
static REGEX_URL_STATIC_MEDIA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://www\.redditstatic\.com/(.*)").unwrap());

/// Direct urls to proxy if proxy is enabled
pub fn format_url(url: &str) -> String {
	if url.is_empty() || url == "self" || url == "default" || url == "nsfw" || url == "spoiler" {
		String::new()
	} else {
		Url::parse(url).map_or(url.to_string(), |parsed| {
			let domain = parsed.domain().unwrap_or_default();
			let generic_vreddit = || {
				let path = parsed.path().trim_start_matches('/');
				let valid_path = !path.is_empty()
					&& !path.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..")
					&& path
						.split('/')
						.next()
						.is_some_and(|id| id.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')));
				if !valid_path {
					return String::new();
				}
				match parsed.query() {
					Some(query) => format!("/hls/{path}?{query}"),
					None => format!("/hls/{path}"),
				}
			};

			let capture = |regex: &Regex, format: &str, segments: i16| {
				regex.captures(url).map_or(String::new(), |caps| match segments {
					1 => [format, &caps[1]].join(""),
					2 => [format, &caps[1], "/", &caps[2]].join(""),
					_ => String::new(),
				})
			};

			match domain {
				"www.reddit.com" => capture(&REGEX_URL_WWW, "/", 1),
				"old.reddit.com" => capture(&REGEX_URL_OLD, "/", 1),
				"np.reddit.com" => capture(&REGEX_URL_NP, "/", 1),
				"reddit.com" => capture(&REGEX_URL_PLAIN, "/", 1),
				"v.redd.it" => {
					let mp4 = capture(&REGEX_URL_VIDEOS, "/vid/", 2);
					if !mp4.is_empty() {
						mp4
					} else {
						let hls = capture(&REGEX_URL_VIDEOS_HLS, "/hls/", 2);
						if hls.is_empty() {
							generic_vreddit()
						} else {
							hls
						}
					}
				}
				"i.redd.it" => capture(&REGEX_URL_IMAGES, "/img/", 1),
				"a.thumbs.redditmedia.com" => capture(&REGEX_URL_THUMBS_A, "/thumb/a/", 1),
				"b.thumbs.redditmedia.com" => capture(&REGEX_URL_THUMBS_B, "/thumb/b/", 1),
				"emoji.redditmedia.com" => capture(&REGEX_URL_EMOJI, "/emoji/", 2),
				"preview.redd.it" => capture(&REGEX_URL_PREVIEW, "/preview/pre/", 1),
				"external-preview.redd.it" => capture(&REGEX_URL_EXTERNAL_PREVIEW, "/preview/external-pre/", 1),
				"styles.redditmedia.com" => capture(&REGEX_URL_STYLES, "/style/", 1),
				"www.redditstatic.com" => capture(&REGEX_URL_STATIC_MEDIA, "/static/", 1),
				_ => url.to_string(),
			}
		})
	}
}

static REGEX_BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^- (.*)$").unwrap());
static REGEX_BULLET_CONSECUTIVE_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</ul>\n<ul>").unwrap());

pub fn render_bullet_lists(input_text: &str) -> String {
	// ref: https://stackoverflow.com/a/4902622
	// First enclose each bullet with <ul> <li> tags
	let text1 = REGEX_BULLET.replace_all(input_text, "<ul><li>$1</li></ul>").to_string();
	// Then remove any consecutive </ul> <ul> tags
	REGEX_BULLET_CONSECUTIVE_LINES.replace_all(&text1, "").to_string()
}

// These are links we want to replace in-body
static REDDIT_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="(https|http|)://(www\.|old\.|np\.|amp\.|new\.|)(reddit\.com|redd\.it)/"#).unwrap());
static REDDIT_PREVIEW_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://(external-preview|preview|i)\.redd\.it(.*)").unwrap());
static REDDIT_EMOJI_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://(www|).redditstatic\.com/(.*)").unwrap());
static REDLIB_PREVIEW_LINK_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"/(img|preview/)(pre|external-pre)?/(.*?)>"#).unwrap());
static REDLIB_PREVIEW_TEXT_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r">(.*?)</a>").unwrap());

/// Rewrite Reddit links to local Vale routes in body text.
pub fn rewrite_urls(input_text: &str) -> String {
	let mut text1 =
		// Rewrite Reddit links to local Vale routes.
		REDDIT_REGEX.replace_all(input_text, r#"href="/"#).to_string();

	loop {
		if REDDIT_EMOJI_REGEX.find(&text1).is_none() {
			break;
		} else {
			text1 = REDDIT_EMOJI_REGEX
				.replace_all(&text1, format_url(REDDIT_EMOJI_REGEX.find(&text1).map(|x| x.as_str()).unwrap_or_default()))
				.to_string()
		}
	}

	// Remove (html-encoded) "\" from URLs.
	text1 = text1.replace("%5C", "").replace("\\_", "_");

	// Rewrite external media previews to Vale's same-origin proxy.
	loop {
		if REDDIT_PREVIEW_REGEX.find(&text1).is_none() {
			return text1;
		} else {
			let formatted_url = format_url(REDDIT_PREVIEW_REGEX.find(&text1).map(|x| x.as_str()).unwrap_or_default());

			let image_url = REDLIB_PREVIEW_LINK_REGEX.find(&formatted_url).map_or("", |m| m.as_str());
			let mut image_caption = REDLIB_PREVIEW_TEXT_REGEX.find(&formatted_url).map_or("", |m| m.as_str());

			/* As long as image_caption isn't empty remove first and last four characters of image_text to leave us with just the text in the caption without any HTML.
			This makes it possible to enclose it in a <figcaption> later on without having stray HTML breaking it */
			if !image_caption.is_empty() {
				image_caption = &image_caption[1..image_caption.len() - 4];
			}

			// image_url contains > at the end of it, and right above this we remove image_text's front >, leaving us with just a single > between them
			let image_to_replace = format!("<p><a href=\"{image_url}{image_caption}</a></p>");

			/* We don't want to show a caption that's just the image's link, so we check if we find a Reddit preview link within the image's caption.
			If we don't find one we must have actual text, so we include a <figcaption> block that contains it.
			Otherwise we don't include the <figcaption> block as we don't need it. */
			let _image_replacement = if REDDIT_PREVIEW_REGEX.find(image_caption).is_none() {
				// Without this " would show as \" instead. "\&quot;" is how the quotes are formatted within image_text beforehand
				format!(
					"<figure><a href=\"{image_url}<img loading=\"lazy\" src=\"{image_url}</a><figcaption>{}</figcaption></figure>",
					image_caption.replace("\\&quot;", "\"")
				)
			} else {
				format!("<figure><a href=\"{image_url}<img loading=\"lazy\" src=\"{image_url}</a></figure>")
			};

			/* In order to know if we're dealing with a normal or external preview we need to take a look at the first capture group of REDDIT_PREVIEW_REGEX
			if it's preview we're dealing with something that needs /preview/pre, external-preview is /preview/external-pre, and i is /img */
			let reddit_preview_regex_capture = REDDIT_PREVIEW_REGEX.captures(&text1).unwrap().get(1).map_or("", |m| m.as_str());

			let _preview_type = match reddit_preview_regex_capture {
				"preview" => "/preview/pre",
				"external-preview" => "/preview/external-pre",
				_ => "/img",
			};

			text1 = REDDIT_PREVIEW_REGEX
				.replace(&text1, format!("{_preview_type}$2"))
				.replace(&image_to_replace, &_image_replacement)
		}
	}
}

const REDDIT_EMOTE_ASSET_PREFIX: &str = "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/";

pub fn rewrite_emotes(media_metadata: &Value, comment: String) -> String {
	let mut comment = comment;
	if let Some(entries) = media_metadata.as_object() {
		for metadata in entries.values() {
			let Some(id) = metadata["id"].as_str().filter(|id| id.starts_with("emote|")) else {
				continue;
			};
			let Some(id_number) = id.rsplit('|').next().filter(|value| !value.is_empty()) else {
				continue;
			};
			let Some(link) = metadata["s"]["u"].as_str() else {
				continue;
			};
			let Some(asset_path) = link.strip_prefix(REDDIT_EMOTE_ASSET_PREFIX) else {
				continue;
			};
			let path = asset_path.split(['?', '#']).next().unwrap_or_default();
			let mut segments = path.split('/');
			let safe_segment = |value: &str| {
				!value.is_empty()
					&& !matches!(value, "." | "..")
					&& value
						.chars()
						.all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '~' | '%'))
			};
			let valid_path = matches!(
				(segments.next(), segments.next(), segments.next()),
				(Some(community), Some(filename), None)
					if safe_segment(community) && safe_segment(filename)
			);
			if !valid_path {
				continue;
			}
			let proxied_link = format!("/emote/{path}");
			let size = metadata["s"]["y"].as_u64().unwrap_or(20).clamp(1, 512);
			let image = format!(
				"<img loading=\"lazy\" src=\"{}\" width=\"{size}\" height=\"{size}\" style=\"vertical-align:text-bottom\">",
				proxied_link
			);
			comment = comment.replace(&format!(":{id_number}:"), &image);
		}
	}

	// render bullet (unordered) lists
	comment = render_bullet_lists(&comment);

	// Call rewrite_urls() to transform any other Reddit links
	rewrite_urls(&comment)
}

/// Format vote count to a string that will be displayed.
/// Append `m` and `k` for millions and thousands respectively, and
/// round to the nearest tenth.
pub fn format_num(num: i64) -> (String, String) {
	let truncated = if num >= 1_000_000 || num <= -1_000_000 {
		format!("{:.1}m", num as f64 / 1_000_000.0)
	} else if num >= 1000 || num <= -1000 {
		format!("{:.1}k", num as f64 / 1_000.0)
	} else {
		num.to_string()
	};

	(truncated, num.to_string())
}

/// Parse a relative and absolute time from a UNIX timestamp
pub fn time(created: f64) -> (String, String) {
	let time = OffsetDateTime::from_unix_timestamp(created.round() as i64).unwrap_or(OffsetDateTime::UNIX_EPOCH);
	let now = OffsetDateTime::now_utc();
	let min = time.min(now);
	let max = time.max(now);
	let time_delta = max - min;

	// If the time difference is more than a month, show full date
	let mut rel_time = if time_delta > Duration::days(30) {
		time.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default()
	// Otherwise, show relative date/time
	} else if time_delta.whole_days() > 0 {
		format!("{}d", time_delta.whole_days())
	} else if time_delta.whole_hours() > 0 {
		format!("{}h", time_delta.whole_hours())
	} else {
		format!("{}m", time_delta.whole_minutes())
	};

	if time_delta <= Duration::days(30) {
		if now < time {
			rel_time += " left";
		} else {
			rel_time += " ago";
		}
	}

	(
		rel_time,
		time
			.format(format_description!("[month repr:short] [day] [year], [hour]:[minute]:[second] UTC"))
			.unwrap_or_default(),
	)
}

/// val() function used to parse JSON from Reddit APIs
pub fn val(j: &Value, k: &str) -> String {
	j["data"][k].as_str().unwrap_or_default().to_string()
}

//
// NETWORKING
//

pub fn template(t: &impl Template) -> Response<Body> {
	Response::builder()
		.status(200)
		.header("content-type", "text/html")
		.body(t.render().unwrap_or_default().into())
		.unwrap_or_default()
}

pub fn redirect(path: &str) -> Response<Body> {
	Response::builder()
		.status(302)
		.header("content-type", "text/plain; charset=utf-8")
		.header("Location", path)
		.body("Redirecting…".into())
		.unwrap_or_default()
}

pub fn see_other(path: &str) -> Response<Body> {
	Response::builder()
		.status(303)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "no-store")
		.header("Location", path)
		.body("Redirecting…".into())
		.unwrap_or_default()
}

/// Accept only an origin-relative redirect target. Backslashes are rejected
/// because browsers may normalize them into a network-path (`//host`) URL.
pub fn safe_local_redirect(value: &str, fallback: &str, maximum: usize) -> String {
	if value.starts_with('/') && !value.starts_with("//") && value.len() <= maximum && !value.contains('\\') && !value.chars().any(char::is_control) {
		value.to_string()
	} else {
		fallback.to_string()
	}
}

/// Renders a generic error landing page.
pub async fn error(req: Request<Body>, msg: &str) -> Result<Response<Body>, String> {
	error!("Vale could not render a requested page");
	let url = req.uri().to_string();
	let body = ErrorTemplate {
		msg: msg.to_string(),
		prefs: Preferences::new(&req),
		url,
	}
	.render()
	.unwrap_or_default();

	Ok(Response::builder().status(404).header("content-type", "text/html").body(body.into()).unwrap_or_default())
}

/// Renders a generic info landing page.
pub async fn info(req: Request<Body>, msg: &str) -> Result<Response<Body>, String> {
	let url = req.uri().to_string();
	let body = InfoTemplate {
		msg: msg.to_string(),
		prefs: Preferences::new(&req),
		url,
	}
	.render()
	.unwrap_or_default();

	Ok(Response::builder().status(200).header("content-type", "text/html").body(body.into()).unwrap_or_default())
}

/// Returns true if the config/env variable `REDLIB_SFW_ONLY` carries the
/// value `on`.
///
/// If this variable is set as such, the instance will operate in SFW-only
/// mode; all NSFW content will be filtered. Attempts to access NSFW
/// subreddits or posts or userpages for users Reddit has deemed NSFW will
/// be denied.
pub fn sfw_only() -> bool {
	match get_setting("REDLIB_SFW_ONLY") {
		Some(val) => val == "on",
		None => false,
	}
}

/// Returns true if the config/env variable REDLIB_ENABLE_RSS is set to "on".
/// If this variable is set as such, the instance will enable RSS feeds.
/// Otherwise, the instance will not provide RSS feeds.
pub fn enable_rss() -> bool {
	match get_setting("REDLIB_ENABLE_RSS") {
		Some(val) => val == "on",
		None => false,
	}
}

/// Returns true if the config/env variable `REDLIB_ROBOTS_DISABLE_INDEXING` carries the
/// value `on`.
///
/// If this variable is set as such, the instance will block all robots in robots.txt and
/// insert the noindex, nofollow meta tag on every page.
pub fn disable_indexing() -> bool {
	match get_setting("REDLIB_ROBOTS_DISABLE_INDEXING") {
		Some(val) => val == "on",
		None => false,
	}
}

/// Determines if a request should redirect to a NSFW landing gate.
pub fn should_be_nsfw_gated(req: &Request<Body>, _req_url: &str) -> bool {
	(Preferences::new(req).show_nsfw != "on") || sfw_only()
}

/// Renders the landing page for NSFW content when the user has not enabled
/// "show NSFW posts" in settings.
pub async fn nsfw_landing(req: Request<Body>, req_url: String) -> Result<Response<Body>, String> {
	let res_type: ResourceType;

	// Determine from the request URL if the resource is a subreddit, a user
	// page, or a post.
	let resource: String = if !req.param("name").unwrap_or_default().is_empty() {
		res_type = ResourceType::User;
		req.param("name").unwrap_or_default()
	} else if !req.param("id").unwrap_or_default().is_empty() {
		res_type = ResourceType::Post;
		req.param("id").unwrap_or_default()
	} else {
		res_type = ResourceType::Subreddit;
		req.param("sub").unwrap_or_default()
	};

	let body = NSFWLandingTemplate {
		res: resource,
		res_type,
		prefs: Preferences::new(&req),
		url: req_url,
	}
	.render()
	.unwrap_or_default();

	Ok(Response::builder().status(403).header("content-type", "text/html").body(body.into()).unwrap_or_default())
}

/// Returns the last (non-empty) segment of a path string
pub fn url_path_basename(path: &str) -> String {
	let url_result = Url::parse(format!("https://libredd.it/{path}").as_str());

	match url_result {
		Ok(mut url) => {
			url.path_segments_mut().unwrap().pop_if_empty();

			url.path_segments().unwrap().next_back().unwrap().to_string()
		}
		Err(_) => path.to_string(),
	}
}

fn responsive_image_srcset(resolutions: &Value, source: &Value) -> String {
	let mut candidates = resolutions.as_array().cloned().unwrap_or_default();
	candidates.push(source.clone());

	let mut sources = candidates
		.iter()
		.filter_map(|candidate| {
			let width = candidate["width"].as_i64().or_else(|| candidate["x"].as_i64()).unwrap_or_default();
			let source_url = candidate["url"].as_str().or_else(|| candidate["u"].as_str()).unwrap_or_default();
			let url = format_url(source_url);
			(width >= 216 && !url.is_empty() && url.starts_with('/')).then_some((width, url))
		})
		.collect::<Vec<_>>();
	sources.sort_by_key(|(width, _)| *width);
	sources.dedup_by(|left, right| left.0 == right.0);
	sources.into_iter().map(|(width, url)| format!("{url} {width}w")).collect::<Vec<_>>().join(", ")
}

fn original_gallery_url(url: &str) -> String {
	if let Some(preview) = url.strip_prefix("/preview/pre/") {
		format!("/img/{}", preview.split('?').next().unwrap_or_default())
	} else {
		url.split_once('?').map_or_else(|| url.to_string(), |(path, _)| path.to_string())
	}
}

pub fn media_download_url(url: &str, filename: &str) -> String {
	let is_downloadable_media = ["/img/", "/preview/", "/vid/", "/hls/"].iter().any(|prefix| url.starts_with(prefix));
	if !is_downloadable_media || filename.is_empty() {
		return String::new();
	}

	let Ok(mut parsed) = Url::parse(&format!("https://vale.invalid{url}")) else {
		return String::new();
	};
	parsed.query_pairs_mut().append_pair("download", filename);
	match parsed.query() {
		Some(query) => format!("{}?{query}", parsed.path()),
		None => parsed.path().to_string(),
	}
}

pub fn safe_download_filename(filename: &str, fallback: &str) -> String {
	let mut safe = filename
		.chars()
		.map(|character| {
			if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
				character
			} else {
				'_'
			}
		})
		.collect::<String>();
	safe = safe.trim_matches(|character| character == '.' || character == '_').chars().take(160).collect();
	if safe.is_empty() {
		fallback.to_string()
	} else {
		safe
	}
}

/// Returns the absolute URL of a post, as needed by RSS feeds
pub fn get_post_url(post: &Post) -> String {
	match post.post_type.as_str() {
		"image" | "gallery" | "gif" | "video" => return to_absolute_url(&post.permalink),
		_ => {}
	}

	if let Some(out_url) = &post.out_url {
		return if out_url.starts_with("/r/") { to_absolute_url(out_url) } else { out_url.clone() };
	}

	to_absolute_url(&post.permalink)
}

/// Returns an absolute URL given a relative URL, as needed by RSS feeds
pub fn to_absolute_url(relative_path: &str) -> String {
	format!("{}{}", config::get_setting("REDLIB_FULL_URL").unwrap_or_default(), relative_path)
}

#[cfg(test)]
mod tests {
	use super::{
		bounded_listing_after, canonical_comment_keywords, canonical_content_url, canonical_feed_path, canonical_feed_sort, canonical_theme, comment_matches_keywords,
		decode_cookie_text, deflate_compress, deflate_decompress, encode_cookie_text, feed_slug, format_num, format_url, group_feed_posts, is_canonical_feed_home,
		listing_before_cursor, listing_cursor_url, listing_query, media_download_url, original_gallery_url, parse_comment_keywords, parse_post, read_body_limited,
		render_bullet_lists, responsive_image_srcset, rewrite_emotes, rewrite_urls, safe_local_redirect, sanitize_feed_groups, serialize_feed_groups, url_path_basename,
		FeedGroup, Post, Preferences, COMBINED_COMMENT_POST_LIMIT, MAX_DECOMPRESSED_PREFERENCES_BYTES,
	};
	use hyper::Body;
	use revision::SerializeRevisioned;
	use serde_json::json;

	#[test]
	fn legacy_theme_catalog_maps_to_three_intentional_themes() {
		for theme in ["", "system", "SYSTEM"] {
			assert_eq!(canonical_theme(theme), "system", "{theme}");
		}
		for theme in ["light", "libredditLight", "gruvboxlight"] {
			assert_eq!(canonical_theme(theme), "light", "{theme}");
		}
		for theme in [
			"dark",
			"black",
			"dracula",
			"nord",
			"laserwave",
			"violet",
			"gold",
			"rosebox",
			"gruvboxdark",
			"tokyoNight",
			"icebergDark",
			"doomone",
			"libredditBlack",
			"libredditDark",
		] {
			assert_eq!(canonical_theme(theme), "dark", "{theme}");
		}
	}

	#[test]
	fn listing_cursor_replacement_keeps_semantics_and_drops_internal_state() {
		assert_eq!(
			listing_cursor_url("/search?q=rust&sort=new&before=old&fragment=posts&limit=100", "after", "t3_next"),
			"/search?q=rust&sort=new&after=t3_next"
		);
		assert_eq!(listing_cursor_url("/r/rust?after=old", "before", "t3_previous"), "/r/rust?before=t3_previous");
		assert_eq!(listing_cursor_url("/r/rust?after=old", "after", ""), "");
	}

	#[test]
	fn listing_queries_are_bounded_to_one_server_page() {
		assert_eq!(listing_query("sort=new&limit=100&fragment=posts"), "sort=new&limit=25");
		assert_eq!(listing_query(""), "limit=25");
	}

	#[test]
	fn cursor_pages_derive_a_previous_cursor_when_reddit_omits_one() {
		assert_eq!(listing_before_cursor("/search.json?q=rust", "", "t3_first"), "");
		assert_eq!(listing_before_cursor("/search.json?q=rust&after=t3_prior", "", "t3_first"), "t3_first");
		assert_eq!(listing_before_cursor("/search.json?q=rust&after=t3_prior", "t3_reddit", "t3_first"), "t3_reddit");
	}

	#[test]
	fn oversized_listing_pages_continue_from_the_last_accepted_fullname() {
		let mut fullnames = (0..25).map(|index| format!("t3_{index:02}")).collect::<Vec<_>>();
		assert_eq!(bounded_listing_after(26, &fullnames, ""), "t3_24");
		assert_eq!(bounded_listing_after(26, &fullnames, "t3_unaccepted_tail"), "t3_24");

		fullnames[24].clear();
		assert_eq!(bounded_listing_after(26, &fullnames, ""), "t3_23");
		assert_eq!(bounded_listing_after(26, &vec![String::new(); 25], ""), "");

		assert_eq!(bounded_listing_after(25, &fullnames, "t3_reddit"), "t3_reddit");
	}

	#[test]
	fn named_feed_paths_have_one_sort_vocabulary() {
		assert_eq!(canonical_feed_sort("best"), Some("hot"));
		assert_eq!(canonical_feed_sort("random"), None);
		assert_eq!(canonical_feed_path("ai-homelab", "new"), "/f/ai-homelab/new");
		assert!(is_canonical_feed_home("/f/ai-homelab/hot"));
		assert!(is_canonical_feed_home("/f/ai-homelab/top?t=week"));
		assert!(!is_canonical_feed_home("/"));
		assert!(!is_canonical_feed_home("/f/ai-homelab"));
		assert!(!is_canonical_feed_home("/r/rust/hot"));
		assert!(!is_canonical_feed_home("/f/ai-homelab/best"));
	}

	#[test]
	fn content_identity_removes_only_known_tracking_parameters() {
		assert_eq!(
			canonical_content_url("https://example.com/story?utm_source=reddit&id=42&fbclid=noise#comments").as_deref(),
			Some("https://example.com/story?id=42")
		);
		assert_ne!(
			canonical_content_url("https://example.com/story?id=42"),
			canonical_content_url("https://example.com/story?id=43")
		);
		assert_ne!(
			canonical_content_url("https://example.com/story?id=42"),
			canonical_content_url("https://other.example/story?id=42")
		);
		assert_ne!(
			canonical_content_url("https://example.com/story?a=1&b=2"),
			canonical_content_url("https://example.com/story?b=2&a=1")
		);
	}

	#[tokio::test]
	async fn feed_grouping_uses_exact_identity_and_preserves_rank_order() {
		let thing = |id: &str, community: &str, url: &str| {
			json!({
				"kind": "t3",
				"data": {
					"name": format!("t3_{id}"),
					"id": id,
					"title": format!("Post {id}"),
					"subreddit": community,
					"author": "reader",
					"permalink": format!("/r/{community}/comments/{id}/post/"),
					"created_utc": 1_700_000_000.0,
					"score": 100,
					"upvote_ratio": 0.95,
					"num_comments": 20,
					"url": url,
					"url_overridden_by_dest": url,
					"domain": "example.com"
				}
			})
		};
		let mut posts = vec![
			parse_post(&thing("one", "alpha", "https://example.com/story?id=42&utm_source=reddit")).await,
			parse_post(&thing("two", "beta", "https://example.com/story?fbclid=noise&id=42")).await,
			parse_post(&thing("three", "gamma", "https://example.com/story?id=43")).await,
		];
		group_feed_posts(&mut posts, true);
		assert_eq!(posts.len(), 2);
		assert_eq!(posts[0].id, "one");
		assert_eq!(posts[0].grouped_posts.len(), 1);
		assert_eq!(posts[0].grouped_posts[0].id, "two");
		assert_eq!(posts[0].combined_url, "/combined?posts=one,two");
		assert_eq!(posts[1].id, "three");
	}

	#[tokio::test]
	async fn exact_groups_keep_all_members_but_bound_combined_comment_ids() {
		let mut posts = Vec::new();
		for index in 0..15 {
			let id = format!("post{index:02}");
			posts.push(
				parse_post(&json!({
					"kind": "t3",
					"data": {
						"name": format!("t3_{id}"),
						"id": id,
						"title": format!("Post {index}"),
						"subreddit": format!("community{index}"),
						"author": "reader",
						"permalink": format!("/comments/post{index:02}/post/"),
						"created_utc": 1_700_000_000.0,
						"score": 1,
						"num_comments": 1,
						"url": "https://example.com/exact",
						"url_overridden_by_dest": "https://example.com/exact",
						"domain": "example.com"
					}
				}))
				.await,
			);
		}
		group_feed_posts(&mut posts, true);
		assert_eq!(posts.len(), 1);
		assert_eq!(posts[0].grouped_posts.len(), 14);
		assert_eq!(posts[0].group_comment_count, COMBINED_COMMENT_POST_LIMIT);
		assert_eq!(posts[0].group_comment_overflow, 3);
		assert_eq!(posts[0].combined_url.trim_start_matches("/combined?posts=").split(',').count(), COMBINED_COMMENT_POST_LIMIT);
	}

	#[test]
	fn format_num_works() {
		assert_eq!(format_num(567), ("567".to_string(), "567".to_string()));
		assert_eq!(format_num(1234), ("1.2k".to_string(), "1234".to_string()));
		assert_eq!(format_num(1999), ("2.0k".to_string(), "1999".to_string()));
		assert_eq!(format_num(1001), ("1.0k".to_string(), "1001".to_string()));
		assert_eq!(format_num(1_999_999), ("2.0m".to_string(), "1999999".to_string()));
	}

	#[test]
	fn rewrite_urls_removes_backslashes_and_rewrites_url() {
		assert_eq!(
			rewrite_urls(
				"<a href=\"https://new.reddit.com/r/linux%5C_gaming/comments/x/just%5C_a%5C_test%5C/\">https://new.reddit.com/r/linux\\_gaming/comments/x/just\\_a\\_test/</a>"
			),
			"<a href=\"/r/linux_gaming/comments/x/just_a_test/\">https://new.reddit.com/r/linux_gaming/comments/x/just_a_test/</a>"
		);
		assert_eq!(
			rewrite_urls(
				"e.g. &lt;a href=\"https://www.reddit.com/r/linux%5C_gaming/comments/ql9j15/anyone%5C_else%5C_confused%5C_with%5C_linus%5C_linux%5C_issues/\"&gt;https://www.reddit.com/r/linux\\_gaming/comments/ql9j15/anyone\\_else\\_confused\\_with\\_linus\\_linux\\_issues/&lt;/a&gt;"
			),
			"e.g. &lt;a href=\"/r/linux_gaming/comments/ql9j15/anyone_else_confused_with_linus_linux_issues/\"&gt;https://www.reddit.com/r/linux_gaming/comments/ql9j15/anyone_else_confused_with_linus_linux_issues/&lt;/a&gt;"
		);
	}

	#[test]
	fn rewrite_urls_keeps_intentional_backslashes() {
		assert_eq!(
			rewrite_urls("printf \"\\npolkit.addRule(function(action, subject)"),
			"printf \"\\npolkit.addRule(function(action, subject)"
		);
	}

	#[test]
	fn test_format_url() {
		assert_eq!(format_url("https://a.thumbs.redditmedia.com/XYZ.jpg"), "/thumb/a/XYZ.jpg");
		assert_eq!(format_url("https://emoji.redditmedia.com/a/b"), "/emoji/a/b");

		assert_eq!(
			format_url("https://external-preview.redd.it/foo.jpg?auto=webp&s=bar"),
			"/preview/external-pre/foo.jpg?auto=webp&s=bar"
		);

		assert_eq!(format_url("https://i.redd.it/foobar.jpg"), "/img/foobar.jpg");
		assert_eq!(
			format_url("https://preview.redd.it/qwerty.jpg?auto=webp&s=asdf"),
			"/preview/pre/qwerty.jpg?auto=webp&s=asdf"
		);
		assert_eq!(format_url("https://v.redd.it/foo/DASH_360.mp4?source=fallback"), "/vid/foo/360.mp4");
		assert_eq!(
			format_url("https://v.redd.it/foo/HLSPlaylist.m3u8?a=bar&v=1&f=sd"),
			"/hls/foo/HLSPlaylist.m3u8?a=bar&v=1&f=sd"
		);
		assert_eq!(
			format_url("https://v.redd.it/abc123/DASH_720_not_audio.mp4?source=fallback&x=1"),
			"/hls/abc123/DASH_720_not_audio.mp4?source=fallback&x=1"
		);
		assert_eq!(format_url("https://www.redditstatic.com/gold/awards/icon/icon.png"), "/static/gold/awards/icon/icon.png");
		assert_eq!(
			format_url("https://www.redditstatic.com/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif"),
			"/static/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif"
		);

		assert_eq!(format_url(""), "");
		assert_eq!(format_url("self"), "");
		assert_eq!(format_url("default"), "");
		assert_eq!(format_url("nsfw"), "");
		assert_eq!(format_url("spoiler"), "");
	}

	#[test]
	fn responsive_media_prefers_proxied_candidates_and_keeps_original_downloads() {
		let preview = serde_json::json!({
			"resolutions": [
				{"url": "https://preview.redd.it/example.jpg?width=108&amp;auto=webp", "width": 108},
				{"url": "https://preview.redd.it/example.jpg?width=320&amp;auto=webp", "width": 320},
				{"url": "https://preview.redd.it/example.jpg?width=640&amp;auto=webp", "width": 640}
			],
			"source": {"url": "https://preview.redd.it/example.jpg?auto=webp", "width": 1600}
		});
		let srcset = responsive_image_srcset(&preview["resolutions"], &preview["source"]);
		assert!(!srcset.contains("108w"));
		assert!(srcset.contains("/preview/pre/example.jpg?width=320&amp;auto=webp 320w"));
		assert!(srcset.contains("/preview/pre/example.jpg?auto=webp 1600w"));

		let original = original_gallery_url("/preview/pre/example.jpg?width=1080&format=pjpg&auto=webp");
		assert_eq!(original, "/img/example.jpg");
		assert_eq!(
			media_download_url(&original, "vale_post_01_example.jpg"),
			"/img/example.jpg?download=vale_post_01_example.jpg"
		);
		assert!(media_download_url("https://example.com/image.jpg", "image.jpg").is_empty());
	}
	#[test]
	fn serialize_prefs() {
		let prefs = Preferences {
			available_themes: vec![],
			theme: "laserwave".to_owned(),
			front_page: "default".to_owned(),
			layout: "compact".to_owned(),
			wide: "on".to_owned(),
			blur_spoiler: "on".to_owned(),
			show_nsfw: "off".to_owned(),
			blur_nsfw: "on".to_owned(),
			hide_hls_notification: "off".to_owned(),
			video_quality: "best".to_owned(),
			hide_sidebar_and_summary: "off".to_owned(),
			use_hls: "on".to_owned(),
			autoplay_videos: "on".to_owned(),
			fixed_navbar: "on".to_owned(),
			disable_visit_reddit_confirmation: "on".to_owned(),
			comment_sort: "confidence".to_owned(),
			post_sort: "top".to_owned(),
			subscriptions: vec!["memes".to_owned(), "mildlyinteresting".to_owned()],
			filters: vec![],
			hide_awards: "off".to_owned(),
			hide_score: "off".to_owned(),
			remove_default_feeds: "off".to_owned(),
			collapse_child_comments: "on".to_owned(),
			comment_filter_keywords: String::new(),
			feed_groups: String::new(),
			active_feed: String::new(),
			keyboard_navigation: "on".to_owned(),
			key_next_post: "j".to_owned(),
			key_previous_post: "k".to_owned(),
			key_open_post: "Enter".to_owned(),
			key_toggle_preview: "e".to_owned(),
			key_hide_post: "h".to_owned(),
			hide_post_behavior: "delay".to_owned(),
			archive_budget_mib: 0,
		};
		let urlencoded = serde_urlencoded::to_string(prefs).expect("Failed to serialize Prefs");

		assert_eq!(urlencoded, "theme=laserwave&front_page=default&layout=compact&wide=on&blur_spoiler=on&show_nsfw=off&blur_nsfw=on&hide_hls_notification=off&video_quality=best&hide_sidebar_and_summary=off&use_hls=on&autoplay_videos=on&fixed_navbar=on&disable_visit_reddit_confirmation=on&comment_sort=confidence&post_sort=top&subscriptions=memes%2Bmildlyinteresting&filters=&hide_awards=off&hide_score=off&remove_default_feeds=off&collapse_child_comments=on&comment_filter_keywords=&feed_groups=&active_feed=&keyboard_navigation=on&key_next_post=j&key_previous_post=k&key_open_post=Enter&key_toggle_preview=e&key_hide_post=h&hide_post_behavior=delay&archive_budget_mib=0");
	}

	#[test]
	fn comment_keyword_filters_are_canonical_and_case_insensitive() {
		assert_eq!(canonical_comment_keywords(" Spoiler,topic\nSPOILER\r\n\n"), "Spoiler\ntopic");
		assert!(comment_matches_keywords("A quiet TOPIC in the middle", &["topic".to_string()]));
		assert!(!comment_matches_keywords("A quiet comment", &["topic".to_string()]));
	}

	#[test]
	fn cookie_text_round_trips_reserved_characters() {
		let value = "spoiler phrase\nAI & homelab + anime";
		assert_eq!(decode_cookie_text(&encode_cookie_text(value)), value);
	}

	#[test]
	fn local_redirects_reject_network_paths_and_backslash_normalization() {
		assert_eq!(safe_local_redirect("/feeds?sort=new", "/", 512), "/feeds?sort=new");
		assert_eq!(safe_local_redirect("//example.com", "/", 512), "/");
		assert_eq!(safe_local_redirect("/\\example.com", "/", 512), "/");
		assert_eq!(safe_local_redirect("https://example.com", "/", 512), "/");
	}

	#[test]
	fn custom_reader_preferences_stay_within_cookie_limits() {
		let keyword_input = (0..40).map(|index| format!("{index:02}{}", "keyword".repeat(12))).collect::<Vec<_>>().join("\n");
		let keywords = parse_comment_keywords(&keyword_input);
		let encoded_keywords = encode_cookie_text(&keywords.join("\n"));
		assert_eq!(keywords.len(), 30);
		assert!(keywords.iter().all(|keyword| keyword.len() <= 60));
		assert!(encoded_keywords.len() < 3_800);

		let unicode_keywords = (0..40).map(|index| format!("{index:02}{}", "🧠".repeat(30))).collect::<Vec<_>>().join("\n");
		let unicode_keywords = parse_comment_keywords(&unicode_keywords);
		assert_eq!(unicode_keywords.len(), 30);
		assert!(unicode_keywords.iter().all(|keyword| keyword.len() <= 60));
		assert!(encode_cookie_text(&unicode_keywords.join("\n")).len() < 3_800);

		let groups = (0..10)
			.map(|group| FeedGroup {
				name: format!("Feed {group} {}", "n".repeat(32)),
				slug: String::new(),
				communities: (0..10).map(|community| format!("g{group:02}_c{community:02}_{}", "x".repeat(42))).collect(),
			})
			.collect::<Vec<_>>();
		let groups = sanitize_feed_groups(&groups);
		let encoded_groups = encode_cookie_text(&serialize_feed_groups(&groups));
		assert_eq!(groups.len(), 8);
		assert_eq!(groups.iter().map(|group| group.communities.len()).sum::<usize>(), 32);
		assert!(encoded_groups.len() < 3_800);
	}

	#[test]
	fn feed_groups_are_named_and_do_not_overlap() {
		let groups = sanitize_feed_groups(&[
			FeedGroup {
				name: "AI & homelab".to_string(),
				slug: String::new(),
				communities: vec!["LocalLLaMA".to_string(), "homelab".to_string()],
			},
			FeedGroup {
				name: "Anime news".to_string(),
				slug: String::new(),
				communities: vec!["localllama".to_string(), "anime".to_string()],
			},
		]);
		assert_eq!(feed_slug("AI & homelab"), "ai-homelab");
		assert_eq!(groups[0].communities, ["LocalLLaMA", "homelab"]);
		assert_eq!(groups[1].communities, ["anime"]);
		assert_eq!(serialize_feed_groups(&groups), serde_json::to_string(&groups).unwrap());
	}

	#[test]
	fn test_rewriting_emoji() {
		let input = r#"<div class="md"><p>How can you have such hard feelings towards a license? <img src="https://www.redditstatic.com/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif" width="20" height="20" style="vertical-align:middle"> Let people use what license they want, and BSD is one of the least restrictive ones AFAIK.</p>"#;
		let output = r#"<div class="md"><p>How can you have such hard feelings towards a license? <img src="/static/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif" width="20" height="20" style="vertical-align:middle"> Let people use what license they want, and BSD is one of the least restrictive ones AFAIK.</p>"#;
		assert_eq!(rewrite_urls(input), output);
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_fetching_subreddit_quarantined() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let subreddit = Post::fetch("/r/drugs", true).await;
		assert!(subreddit.is_ok());
		assert!(!subreddit.unwrap().0.is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_fetching_nsfw_subreddit() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		// Gonwild is a place for closed, Euclidean Geometric shapes to exchange their nth terms for karma; showing off their edges in a comfortable environment without pressure.
		// Find a good sub that is tagged NSFW but that actually isn't in case my future employers are watching (they probably are)
		// switched from randnsfw as it is no longer functional.
		let subreddit = Post::fetch("/r/gonwild", false).await;
		assert!(subreddit.is_ok());
		assert!(!subreddit.unwrap().0.is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit access"]
	async fn test_fetching_ws() {
		crate::oauth::ensure_live_oauth().await.unwrap();
		let subreddit = Post::fetch("/r/popular", false).await;
		assert!(subreddit.is_ok());
		for post in subreddit.unwrap().0 {
			assert!(post.ws_url.starts_with("wss://k8s-lb.wss.redditmedia.com/link/"));
		}
	}

	#[test]
	fn test_rewriting_image_links() {
		let input =
			r#"<p><a href="https://preview.redd.it/6awags382xo31.png?width=2560&amp;format=png&amp;auto=webp&amp;s=9c563aed4f07a91bdd249b5a3cea43a79710dcfc">caption 1</a></p>"#;
		let output = r#"<figure><a href="/preview/pre/6awags382xo31.png?width=2560&amp;format=png&amp;auto=webp&amp;s=9c563aed4f07a91bdd249b5a3cea43a79710dcfc"><img loading="lazy" src="/preview/pre/6awags382xo31.png?width=2560&amp;format=png&amp;auto=webp&amp;s=9c563aed4f07a91bdd249b5a3cea43a79710dcfc"></a><figcaption>caption 1</figcaption></figure>"#;
		assert_eq!(rewrite_urls(input), output);
	}

	#[test]
	fn test_url_path_basename() {
		// without trailing slash
		assert_eq!(url_path_basename("/first/last"), "last");
		// with trailing slash
		assert_eq!(url_path_basename("/first/last/"), "last");
		// with query parameters
		assert_eq!(url_path_basename("/first/last/?some=query"), "last");
		// file path
		assert_eq!(url_path_basename("/cdn/image.jpg"), "image.jpg");
		// when a full url is passed instead of just a path
		assert_eq!(url_path_basename("https://doma.in/first/last"), "last");
		// empty path
		assert_eq!(url_path_basename("/"), "");
	}

	#[test]
	fn test_rewriting_emotes() {
		let json_input = serde_json::from_str(r#"{"emote|t5_31hpy|2028":{"e":"Image","id":"emote|t5_31hpy|2028","m":"image/png","s":{"u":"https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/t5_31hpy/PW6WsOaLcd.png","x":60,"y":60},"status":"valid","t":"sticker"}}"#).expect("Valid JSON");
		let comment_input = r#"<div class="comment_body "><div class="md"><p>:2028:</p></div></div>"#;
		let output = r#"<div class="comment_body "><div class="md"><p><img loading="lazy" src="/emote/t5_31hpy/PW6WsOaLcd.png" width="60" height="60" style="vertical-align:text-bottom"></p></div></div>"#;
		assert_eq!(rewrite_emotes(&json_input, comment_input.to_string()), output);
	}

	#[test]
	fn malformed_emote_metadata_is_ignored_without_panicking() {
		let metadata = serde_json::json!({
			"missing-size": {
				"id": "emote|community|42",
				"s": {"u": "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/community/emote.png"}
			},
			"wrong-host": {
				"id": "emote|community|99",
				"s": {"u": "https://example.invalid/emote.png", "y": 60}
			},
			"missing-id": {"s": {"u": "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/community/other.png"}}
		});
		let rendered = rewrite_emotes(&metadata, ":42: :99:".to_string());
		assert!(rendered.contains("src=\"/emote/community/emote.png\" width=\"20\" height=\"20\""));
		assert!(rendered.contains(":99:"));
	}

	#[test]
	fn test_rewriting_bullet_list() {
		let input = r#"<div class="md"><p>Hi, I&#39;ve bought this very same monitor and found no calibration whatsoever. I have an ICC profile that has been set up since I&#39;ve installed its driver from the LG website and it works ok. I also used <a href="http://www.lagom.nl/lcd-test/">http://www.lagom.nl/lcd-test/</a> to calibrate it. After some good tinkering I&#39;ve found the following settings + the color profile from the driver gets me past all the tests perfectly:
- Brightness 50 (still have to settle on this one, it&#39;s personal preference, it controls the backlight, not the colors)
- Contrast 70 (which for me was the default one)
- Picture mode Custom
- Super resolution + Off (it looks horrible anyway)
- Sharpness 50 (default one I think)
- Black level High (low messes up gray colors)
- DFC Off
- Response Time Middle (personal preference, <a href="https://www.blurbusters.com/">https://www.blurbusters.com/</a> show horrible overdrive with it on high)
- Freesync doesn&#39;t matter
- Black stabilizer 50
- Gamma setting on 0
- Color Temp Medium
How`s your monitor by the way? Any IPS bleed whatsoever? I either got lucky or the panel is pretty good, 0 bleed for me, just the usual IPS glow. How about the pixels? I see the pixels even at one meter away, especially on Microsoft Edge&#39;s icon for example, the blue background is just blocky, don&#39;t know why.</p>
</div>"#;
		let output = r#"<div class="md"><p>Hi, I&#39;ve bought this very same monitor and found no calibration whatsoever. I have an ICC profile that has been set up since I&#39;ve installed its driver from the LG website and it works ok. I also used <a href="http://www.lagom.nl/lcd-test/">http://www.lagom.nl/lcd-test/</a> to calibrate it. After some good tinkering I&#39;ve found the following settings + the color profile from the driver gets me past all the tests perfectly:
<ul><li>Brightness 50 (still have to settle on this one, it&#39;s personal preference, it controls the backlight, not the colors)</li><li>Contrast 70 (which for me was the default one)</li><li>Picture mode Custom</li><li>Super resolution + Off (it looks horrible anyway)</li><li>Sharpness 50 (default one I think)</li><li>Black level High (low messes up gray colors)</li><li>DFC Off</li><li>Response Time Middle (personal preference, <a href="https://www.blurbusters.com/">https://www.blurbusters.com/</a> show horrible overdrive with it on high)</li><li>Freesync doesn&#39;t matter</li><li>Black stabilizer 50</li><li>Gamma setting on 0</li><li>Color Temp Medium</li></ul>
How`s your monitor by the way? Any IPS bleed whatsoever? I either got lucky or the panel is pretty good, 0 bleed for me, just the usual IPS glow. How about the pixels? I see the pixels even at one meter away, especially on Microsoft Edge&#39;s icon for example, the blue background is just blocky, don&#39;t know why.</p>
</div>"#;

		assert_eq!(render_bullet_lists(input), output);
	}

	#[test]
	fn test_default_prefs_serialization_loop_json() {
		let prefs = Preferences::default();
		let serialized = serde_json::to_string(&prefs).unwrap();
		let deserialized: Preferences = serde_json::from_str(&serialized).unwrap();
		assert_eq!(prefs, deserialized);
	}

	#[test]
	fn test_default_prefs_serialization_loop_bincode() {
		let prefs = Preferences::default();
		test_round_trip(&prefs, false);
		test_round_trip(&prefs, true);
	}

	#[test]
	fn decompression_rejects_oversized_preference_exports() {
		let compressed = deflate_compress(vec![b'x'; MAX_DECOMPRESSED_PREFERENCES_BYTES + 1]).unwrap();
		let error = deflate_decompress(compressed).unwrap_err();
		assert!(error.contains("exceeds"));
	}

	#[tokio::test]
	async fn body_reader_rejects_oversized_payloads_before_buffering() {
		let mut body = Body::from(vec![b'x'; 9]);
		let error = read_body_limited(&mut body, 8, "body too large").await.unwrap_err();
		assert_eq!(error, "body too large");
	}

	static KNOWN_GOOD_CONFIGS: &[&str] = &[
		"ఴӅβØØҞÉဏႢձĬ༧ȒʯऌԔӵ୮༏",
		"ਧՊΥÀÃǎƱГ۸ඣമĖฤ႙ʟาúໜϾௐɥঀĜໃહཞઠѫҲɂఙ࿔ǲઉƲӟӻĻฅΜδ໖ԜǗဖငƦơ৶Ą௩ԹʛใЛʃශаΏ",
		"ਧԩΥÀÃÎŠ౭൩ඔႠϼҭöҪƸռઇԾॐნɔາǒՍҰच௨ಖມŃЉŐདƦ๙ϩএఠȝഽйʮჯඒϰळՋ௮ສ৵ऎΦѧਹಧଟƙŃ३î༦ŌပղयƟแҜ།",
	];

	#[test]
	fn test_known_good_configs_deserialization() {
		for config in KNOWN_GOOD_CONFIGS {
			let bytes = base2048::decode(config).unwrap();
			let decompressed = deflate_decompress(bytes).unwrap();
			assert!(Preferences::from_bincode(&decompressed).is_ok());
		}
	}

	#[test]
	fn test_known_good_configs_full_round_trip() {
		for config in KNOWN_GOOD_CONFIGS {
			let bytes = base2048::decode(config).unwrap();
			let decompressed = deflate_decompress(bytes).unwrap();
			let prefs = Preferences::from_bincode(&decompressed).unwrap();
			assert!(matches!(prefs.theme.as_str(), "system" | "light" | "dark"));
			assert_eq!(prefs.front_page, "default");
			assert_eq!(prefs.layout, "compact");
			assert_eq!(prefs.wide, "on");
			assert_eq!(prefs.fixed_navbar, "on");
			assert_eq!(prefs.remove_default_feeds, "on");
			assert_eq!(prefs.hide_sidebar_and_summary, "off");
			test_round_trip(&prefs, false);
			test_round_trip(&prefs, true);
		}
	}

	#[test]
	fn revisioned_preferences_export_round_trip() {
		let mut prefs = Preferences {
			theme: "dark".to_string(),
			front_page: "default".to_string(),
			layout: "compact".to_string(),
			wide: "on".to_string(),
			fixed_navbar: "on".to_string(),
			remove_default_feeds: "on".to_string(),
			hide_sidebar_and_summary: "off".to_string(),
			hide_post_behavior: "instant".to_string(),
			comment_filter_keywords: "spoiler".to_string(),
			feed_groups: r#"[{"name":"Anime","slug":"anime","communities":["anime"]}]"#.to_string(),
			active_feed: "anime".to_string(),
			..Preferences::default()
		};
		prefs.apply_reader_defaults();
		let bytes = prefs.to_bincode().unwrap();
		assert!(bytes.starts_with(b"VAL1"));
		assert_eq!(Preferences::from_bincode(&bytes).unwrap(), prefs);
	}

	#[test]
	fn invalid_imported_archive_budget_is_rejected() {
		let mut preferences = Preferences {
			archive_budget_mib: 300,
			..Preferences::default()
		};
		preferences.apply_reader_defaults();
		assert!(preferences.to_bincode().is_err());
		let mut bytes = b"VAL1".to_vec();
		preferences.serialize_revisioned(&mut bytes).unwrap();
		assert!(Preferences::from_bincode(&bytes).is_err());
	}

	#[test]
	fn supported_export_prefixes_apply_revision_six_canonicalization() {
		let legacy = Preferences {
			theme: "laserwave".to_string(),
			front_page: "popular".to_string(),
			layout: "card".to_string(),
			wide: "off".to_string(),
			fixed_navbar: "off".to_string(),
			remove_default_feeds: "off".to_string(),
			hide_sidebar_and_summary: "on".to_string(),
			hide_post_behavior: "delay".to_string(),
			..Preferences::default()
		};
		let current = legacy.to_bincode().unwrap();
		for prefix in [b"VAL1", b"LUR5", b"LUR4", b"LUR3"] {
			let mut encoded = current.clone();
			encoded[..4].copy_from_slice(prefix);
			let migrated = Preferences::from_bincode(&encoded).unwrap();
			assert_eq!(migrated.theme, "dark");
			assert_eq!(migrated.front_page, "default");
			assert_eq!(migrated.layout, "compact");
			assert_eq!(migrated.wide, "on");
			assert_eq!(migrated.fixed_navbar, "on");
			assert_eq!(migrated.remove_default_feeds, "on");
			assert_eq!(migrated.hide_sidebar_and_summary, "off");
			assert_eq!(migrated.hide_post_behavior, "instant");
			assert_eq!(migrated.archive_budget_mib, 0);
		}
	}

	fn test_round_trip(input: &Preferences, compression: bool) {
		let serialized = bincode::serialize(input).unwrap();
		let compressed = if compression { deflate_compress(serialized).unwrap() } else { serialized };
		let decompressed = if compression { deflate_decompress(compressed).unwrap() } else { compressed };
		let deserialized: Preferences = bincode::deserialize(&decompressed).unwrap();
		assert_eq!(*input, deserialized);
	}
}
