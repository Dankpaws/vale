// Global specifiers
#![forbid(unsafe_code)]
use clap::{Arg, ArgAction, ArgMatches, Command};
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;

use futures_lite::FutureExt;
use hyper::{header::HeaderValue, Body, Request, Response};
use log::{info, warn};
use redlib::client::{canonical_path, proxy, rate_limit_check};
use redlib::server::{self, RequestExt};
use redlib::utils::{error, redirect};
use redlib::{account, archive, combined, config, duplicates, headers, media, post, search, settings, subreddit, user};

use redlib::client::OAUTH_CLIENT;
use redlib::oauth::{force_refresh_token, token_daemon};
use zeroize::Zeroize;

const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");

// Create Services

async fn legacy_subreddit_wiki_redirect(req: Request<Body>) -> Result<Response<Body>, String> {
	Ok(redirect(&format!(
		"/r/{}/wiki/{}",
		req.param("sub").unwrap_or_default(),
		req.param("page").unwrap_or_default()
	)))
}

// Required for the manifest to be valid
async fn pwa_logo() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/png")
			.body(include_bytes!("../static/logo.png").as_ref().into())
			.unwrap_or_default(),
	)
}

// Required for iOS App Icons
async fn iphone_logo() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/png")
			.body(include_bytes!("../static/apple-touch-icon.png").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn favicon() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/vnd.microsoft.icon")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/favicon.ico").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn binary_resource(body: &'static [u8], content_type: &'static str) -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", content_type)
			.header("Cache-Control", "public, max-age=31536000, immutable")
			.body(body.into())
			.unwrap_or_default(),
	)
}

async fn opensearch() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/opensearchdescription+xml")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/opensearch.xml").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn resource(body: &str, content_type: &str, cache: bool) -> Result<Response<Body>, String> {
	let mut res = Response::builder()
		.status(200)
		.header("content-type", content_type)
		.body(body.to_string().into())
		.unwrap_or_default();

	if cache {
		if let Ok(val) = HeaderValue::from_str("public, max-age=1209600, s-maxage=86400") {
			res.headers_mut().insert("Cache-Control", val);
		}
	}

	Ok(res)
}

fn run_admin_command(matches: &ArgMatches) -> Result<(), String> {
	match matches.subcommand() {
		Some(("reset-password", reset)) => {
			let username = reset.get_one::<String>("username").expect("clap requires an account username");
			let mut password = rpassword::prompt_password("New password: ").map_err(|error| format!("Unable to read the new password from the terminal: {error}"))?;
			let mut confirmation = match rpassword::prompt_password("Confirm new password: ") {
				Ok(confirmation) => confirmation,
				Err(error) => {
					password.zeroize();
					return Err(format!("Unable to read the password confirmation from the terminal: {error}"));
				}
			};
			let reset = account::reset_password_offline(username, &password, &confirmation);
			password.zeroize();
			confirmation.zeroize();
			reset?;
			println!("Password reset. All existing sessions for that account were revoked.");
			Ok(())
		}
		_ => Err("Choose a supported Vale administration command.".to_string()),
	}
}

async fn style() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "text/css")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_str!("../static/style.css").to_string().into())
			.unwrap_or_default(),
	)
}

#[tokio::main]
async fn main() {
	// Load environment variables
	_ = dotenvy::dotenv();

	// Initialize logger
	pretty_env_logger::init();

	let matches = Command::new("Vale")
		.bin_name("vale")
		.version(BUILD_VERSION)
		.about("A quiet, subscription-first reader for Reddit communities")
		.arg(
			Arg::new("ipv4-only")
				.short('4')
				.long("ipv4-only")
				.help("Require the configured listener address to be IPv4")
				.conflicts_with("ipv6-only")
				.num_args(0),
		)
		.arg(
			Arg::new("ipv6-only")
				.short('6')
				.long("ipv6-only")
				.help("Require the configured listener address to be IPv6")
				.conflicts_with("ipv4-only")
				.num_args(0),
		)
		.arg(
			Arg::new("address")
				.short('a')
				.long("address")
				.value_name("ADDRESS")
				.env("REDLIB_ADDRESS")
				.help("Numeric IP address to listen on")
				.default_value("127.0.0.1")
				.value_parser(clap::value_parser!(IpAddr))
				.num_args(1),
		)
		.arg(
			Arg::new("port")
				.short('p')
				.long("port")
				.value_name("PORT")
				.env("PORT")
				.help("Port to listen on")
				.default_value("8080")
				.value_parser(clap::value_parser!(u16).range(1..))
				.action(ArgAction::Set)
				.num_args(1),
		)
		.arg(
			Arg::new("hsts")
				.short('H')
				.long("hsts")
				.value_name("EXPIRE_TIME")
				.help("HSTS header to tell browsers that this site should only be accessed over HTTPS")
				.default_value("604800")
				.value_parser(clap::value_parser!(u64))
				.num_args(1),
		)
		.subcommand(
			Command::new("admin")
				.about("Local recovery tools for the Vale administrator")
				.subcommand_required(true)
				.subcommand(
					Command::new("reset-password").about("Reset one local account password and revoke its sessions").arg(
						Arg::new("username")
							.short('u')
							.long("username")
							.value_name("ACCOUNT")
							.required(true)
							.help("Local Vale account to recover"),
					),
				),
		)
		.get_matches();

	if let Some(("admin", admin_matches)) = matches.subcommand() {
		if let Err(error) = run_admin_command(admin_matches) {
			eprintln!("Unable to reset the Vale password: {error}");
			std::process::exit(1);
		}
		return;
	}

	let address = *matches.get_one::<IpAddr>("address").expect("clap validates the listener address");
	let port = *matches.get_one::<u16>("port").expect("clap validates the listener port");
	let hsts = matches.get_one::<u64>("hsts").copied();

	let enabled_env_flag = |name: &str| {
		std::env::var(name)
			.ok()
			.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
	};
	let ipv4_only = enabled_env_flag("IPV4_ONLY") || matches.get_flag("ipv4-only");
	let ipv6_only = enabled_env_flag("IPV6_ONLY") || matches.get_flag("ipv6-only");

	if ipv4_only && ipv6_only {
		eprintln!("IPV4_ONLY and IPV6_ONLY cannot both be enabled.");
		std::process::exit(2);
	}
	if ipv4_only && !address.is_ipv4() {
		eprintln!("The configured listener address is not IPv4.");
		std::process::exit(2);
	}
	if ipv6_only && !address.is_ipv6() {
		eprintln!("The configured listener address is not IPv6.");
		std::process::exit(2);
	}
	let listener = SocketAddr::new(address, port).to_string();

	println!("Starting Vale...");

	// Begin constructing a server
	let mut app = server::Server::new();

	// Evaluate configuration before accepting requests while Reddit
	// compatibility is recovered independently in the background.

	info!("Evaluating config.");
	LazyLock::force(&config::CONFIG);
	if let Err(error) = account::initialize() {
		eprintln!("Unable to initialize Vale profiles: {error}");
		std::process::exit(1);
	}
	if let Err(error) = archive::resume_pending() {
		eprintln!("Unable to resume Vale saved-post captures: {error}");
		std::process::exit(1);
	}

	// Define default headers (added to all responses)
	app.default_headers = headers! {
		"Referrer-Policy" => "no-referrer",
		"X-Content-Type-Options" => "nosniff",
		"X-Frame-Options" => "DENY",
		"Permissions-Policy" => "camera=(), microphone=(), geolocation=(), payment=()",
		"Cross-Origin-Opener-Policy" => "same-origin",
		"Content-Security-Policy" => "default-src 'none'; font-src 'self'; script-src 'self' blob:; manifest-src 'self'; media-src 'self' data: blob: about:; style-src 'self' 'unsafe-inline'; base-uri 'none'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; connect-src 'self'; worker-src 'self' blob:;"
	};

	if let Some(expire_time) = hsts {
		if let Ok(val) = HeaderValue::from_str(&format!("max-age={expire_time}")) {
			app.default_headers.insert("Strict-Transport-Security", val);
		}
	}

	// Read static files
	app.at("/style.css").get(|_| style().boxed());
	app
		.at("/manifest.json")
		.get(|_| resource(include_str!("../static/manifest.json"), "application/json", false).boxed());
	app.at("/robots.txt").get(|_| {
		resource(
			if match config::get_setting("REDLIB_ROBOTS_DISABLE_INDEXING") {
				Some(val) => val == "on",
				None => false,
			} {
				"User-agent: *\nDisallow: /"
			} else {
				"User-agent: *\nDisallow: /u/\nDisallow: /user/"
			},
			"text/plain",
			true,
		)
		.boxed()
	});
	app.at("/favicon.ico").get(|_| favicon().boxed());
	app
		.at("/vale-mark.svg")
		.get(|_| resource(include_str!("../static/vale-mark.svg"), "image/svg+xml", true).boxed());
	app.at("/logo.png").get(|_| pwa_logo().boxed());
	app
		.at("/fonts/source-sans-3.woff2")
		.get(|_| binary_resource(include_bytes!("../static/fonts/SourceSans3VF-Upright.ttf.woff2"), "font/woff2").boxed());
	app
		.at("/fonts/source-serif-4.woff2")
		.get(|_| binary_resource(include_bytes!("../static/fonts/SourceSerif4-Regular.ttf.woff2"), "font/woff2").boxed());
	app
		.at("/scenes/vale-dark.avif")
		.get(|_| binary_resource(include_bytes!("../static/scenes/vale-dark.avif"), "image/avif").boxed());
	app
		.at("/scenes/vale-dark.webp")
		.get(|_| binary_resource(include_bytes!("../static/scenes/vale-dark.webp"), "image/webp").boxed());
	app
		.at("/scenes/vale-light.avif")
		.get(|_| binary_resource(include_bytes!("../static/scenes/vale-light.avif"), "image/avif").boxed());
	app
		.at("/scenes/vale-light.webp")
		.get(|_| binary_resource(include_bytes!("../static/scenes/vale-light.webp"), "image/webp").boxed());
	app.at("/touch-icon-iphone.png").get(|_| iphone_logo().boxed());
	app.at("/apple-touch-icon.png").get(|_| iphone_logo().boxed());
	app.at("/opensearch.xml").get(|_| opensearch().boxed());
	app
		.at("/playHLSVideo.js")
		.get(|_| resource(include_str!("../static/playHLSVideo.js"), "text/javascript", false).boxed());
	app
		.at("/hls.min.js")
		.get(|_| resource(include_str!("../static/hls.min.js"), "text/javascript", false).boxed());
	app
		.at("/highlighted.js")
		.get(|_| resource(include_str!("../static/highlighted.js"), "text/javascript", false).boxed());
	app.at("/copy.js").get(|_| resource(include_str!("../static/copy.js"), "text/javascript", false).boxed());
	app
		.at("/register-sw.js")
		.get(|_| resource(include_str!("../static/register-sw.js"), "text/javascript", false).boxed());
	app
		.at("/vale-interactions.js")
		.get(|_| resource(include_str!("../static/vale-interactions.js"), "text/javascript", false).boxed());
	app
		.at("/service-worker.js")
		.get(|_| resource(include_str!("../static/service-worker.js"), "text/javascript", false).boxed());
	app.at("/healthz").get(|_| account::health().boxed());

	// Native Vale profiles and authentication.
	app.at("/login").get(|r| account::login_get(r).boxed()).post(|r| account::login_post(r).boxed());
	app.at("/setup").get(|r| account::setup_get(r).boxed()).post(|r| account::setup_post(r).boxed());
	app.at("/logout").post(|r| account::logout_post(r).boxed());
	app.at("/account/logout-all").post(|r| account::logout_all_post(r).boxed());
	app.at("/account/password").post(|r| account::change_password_post(r).boxed());
	app.at("/account/users").post(|r| account::create_user_post(r).boxed());
	app.at("/account/users/:id/toggle").post(|r| account::toggle_user_post(r).boxed());
	app.at("/account/users/:id/password").post(|r| account::reset_user_password_post(r).boxed());
	app.at("/history").get(|r| account::history_get(r).boxed());
	app.at("/history/clear").post(|r| account::history_clear_post(r).boxed());
	app.at("/saved").get(|r| archive::list_get(r).boxed());
	app.at("/saved/:archive_id").get(|r| archive::detail_get(r).boxed());
	app.at("/saved/:archive_id/view.html").get(|r| archive::view_get(r).boxed());
	app.at("/saved/:archive_id/manifest.json").get(|r| archive::manifest_get(r).boxed());
	app.at("/saved/:archive_id/files/*path").get(|r| archive::file_get(r).boxed());
	app.at("/saved/:archive_id/delete").post(|r| archive::delete_post(r).boxed());
	app.at("/posts/:id/archive").post(|r| archive::save_post(r).boxed());
	app.at("/combined").get(|r| combined::item(r).boxed());
	app.at("/posts/:id/hide").post(|r| account::hide_post_post(r).boxed());
	app.at("/posts/:id/unhide").post(|r| account::unhide_post_post(r).boxed());
	app.at("/hidden/state").get(|r| account::hidden_state_get(r).boxed());
	app.at("/hidden/clear").post(|r| account::hidden_clear_post(r).boxed());
	app.at("/download/gallery").post(|r| media::gallery_download(r).boxed());
	app.at("/download/video").post(|r| media::video_download(r).boxed());

	// Proxy media through Vale's same-origin reader.
	app.at("/vid/:id/:size").get(|r| proxy(r, "https://v.redd.it/{id}/DASH_{size}").boxed());
	app.at("/hls/:id/*path").get(|r| proxy(r, "https://v.redd.it/{id}/{path}").boxed());
	app.at("/img/*path").get(|r| proxy(r, "https://i.redd.it/{path}").boxed());
	app.at("/thumb/:point/:id").get(|r| proxy(r, "https://{point}.thumbs.redditmedia.com/{id}").boxed());
	app.at("/emoji/:id/:name").get(|r| proxy(r, "https://emoji.redditmedia.com/{id}/{name}").boxed());
	app
		.at("/emote/:subreddit_id/:filename")
		.get(|r| proxy(r, "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/{subreddit_id}/{filename}").boxed());
	app
		.at("/preview/:loc/award_images/:fullname/:id")
		.get(|r| proxy(r, "https://{loc}view.redd.it/award_images/{fullname}/{id}").boxed());
	app.at("/preview/:loc/:id").get(|r| proxy(r, "https://{loc}view.redd.it/{id}").boxed());
	app.at("/style/*path").get(|r| proxy(r, "https://styles.redditmedia.com/{path}").boxed());
	app.at("/static/*path").get(|r| proxy(r, "https://www.redditstatic.com/{path}").boxed());

	// Browse user profile
	app
		.at("/u/:name")
		.get(|r| async move { Ok(redirect(&format!("/user/{}", r.param("name").unwrap_or_default()))) }.boxed());
	app.at("/u/:name/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/u/:name/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());

	app.at("/user/[deleted]").get(|req| error(req, "User has deleted their account").boxed());
	app.at("/user/:name.rss").get(|r| user::rss(r).boxed());
	app.at("/user/:name").get(|r| user::profile(r).boxed());
	app.at("/user/:name/:listing").get(|r| user::profile(r).boxed());
	app.at("/user/:name/comments/:id").get(|r| post::item(r).boxed());
	app.at("/user/:name/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/user/:name/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());

	// Configure settings
	app.at("/settings").get(|r| settings::get(r).boxed()).post(|r| settings::set(r).boxed());
	app.at("/settings/archive-storage").post(|r| settings::archive_storage(r).boxed());
	app.at("/subscriptions").get(|r| settings::subscriptions(r).boxed());
	app.at("/feeds").get(|r| settings::subscriptions(r).boxed()).post(|r| settings::manage_feeds(r).boxed());
	app.at("/discover").get(|r| settings::legacy_discover(r).boxed());
	app.at("/settings/restore").get(|r| settings::restore(r).boxed());
	app.at("/settings/encoded-restore").post(|r| settings::encoded_restore(r).boxed());
	app.at("/settings/update").post(|r| settings::update(r).boxed());

	// RSS Subscriptions
	app.at("/r/:sub.rss").get(|r| subreddit::rss(r).boxed());

	// Subreddit services
	app
		.at("/r/:sub")
		.get(|r| subreddit::community(r).boxed())
		.post(|r| subreddit::add_quarantine_exception(r).boxed());

	app
		.at("/r/u_:name")
		.get(|r| async move { Ok(redirect(&format!("/user/{}", r.param("name").unwrap_or_default()))) }.boxed());

	app.at("/r/:sub/subscribe").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/unsubscribe").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/filter").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/unfilter").post(|r| subreddit::subscriptions_filters(r).boxed());

	app.at("/r/:sub/comments/:id").get(|r| post::item(r).boxed());
	app.at("/r/:sub/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/r/:sub/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());
	app.at("/comments/:id").get(|r| post::item(r).boxed());
	app.at("/comments/:id/comments").get(|r| post::item(r).boxed());
	app.at("/comments/:id/comments/:comment_id").get(|r| post::item(r).boxed());
	app.at("/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());

	app.at("/r/:sub/duplicates/:id").get(|r| duplicates::item(r).boxed());
	app.at("/r/:sub/duplicates/:id/:title").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates/:id").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates/:id/:title").get(|r| duplicates::item(r).boxed());

	app.at("/r/:sub/search").get(|r| search::find(r).boxed());

	app
		.at("/r/:sub/w")
		.get(|r| async move { Ok(redirect(&format!("/r/{}/wiki", r.param("sub").unwrap_or_default()))) }.boxed());
	app.at("/r/:sub/w/*page").get(|r| legacy_subreddit_wiki_redirect(r).boxed());
	app.at("/r/:sub/wiki").get(|r| subreddit::wiki(r).boxed());
	app.at("/r/:sub/wiki/*page").get(|r| subreddit::wiki(r).boxed());

	app.at("/r/:sub/about/sidebar").get(|r| subreddit::sidebar(r).boxed());

	app.at("/r/:sub/:sort").get(|r| subreddit::community(r).boxed());
	app.at("/f/:feed").get(|r| subreddit::feed_without_sort(r).boxed());
	app.at("/f/:feed/:sort").get(|r| subreddit::community(r).boxed());

	// Front page
	app.at("/").get(|r| subreddit::front_page(r).boxed());

	// View Reddit wiki
	app.at("/w").get(|_| async { Ok(redirect("/wiki")) }.boxed());
	app
		.at("/w/*page")
		.get(|r| async move { Ok(redirect(&format!("/wiki/{}", r.param("page").unwrap_or_default()))) }.boxed());
	app.at("/wiki").get(|r| subreddit::wiki(r).boxed());
	app.at("/wiki/*page").get(|r| subreddit::wiki(r).boxed());

	// Search all of Reddit
	app.at("/search").get(|r| search::find(r).boxed());

	// Handle obfuscated share links.
	// Note that this still forces the server to follow the share link to get to the post, so maybe this wants to be updated with a warning before it follow it
	app.at("/r/:sub/s/:id").get(|req: Request<Body>| {
		Box::pin(async move {
			let sub = req.param("sub").unwrap_or_default();
			match req.param("id").as_deref() {
				// Share link
				Some(id) if (8..12).contains(&id.len()) => match canonical_path(format!("/r/{sub}/s/{id}"), 3).await {
					Ok(Some(path)) => Ok(redirect(&path)),
					Ok(None) => error(req, "Post ID is invalid. It may point to a post on a community that has been banned.").await,
					Err(e) => error(req, &e).await,
				},

				// Error message for unknown pages
				_ => error(req, "Nothing here").await,
			}
		})
	});

	app.at("/:id").get(|req: Request<Body>| {
		Box::pin(async move {
			match req.param("id").as_deref() {
				// Sort front page
				Some("best" | "hot" | "new" | "top" | "rising" | "controversial") => subreddit::legacy_front_page(req).await,

				// Short link for post
				Some(id) if (5..8).contains(&id.len()) => match canonical_path(format!("/comments/{id}"), 3).await {
					Ok(path_opt) => match path_opt {
						Some(path) => Ok(redirect(&path)),
						None => error(req, "Post ID is invalid. It may point to a post on a community that has been banned.").await,
					},
					Err(e) => error(req, &e).await,
				},

				// Error message for unknown pages
				_ => error(req, "Nothing here").await,
			}
		})
	});

	// Default service in case no routes match
	app.at("/*").get(|req| error(req, "Nothing here").boxed());

	// Reddit compatibility is deliberately independent from local startup.
	// Health, setup, login, profiles, and saved local data remain available
	// while acquisition retries in the background.
	LazyLock::force(&OAUTH_CLIENT);
	tokio::spawn(async {
		if force_refresh_token().await {
			match rate_limit_check().await {
				Ok(()) => info!("[✅] Rate limit check passed"),
				Err(error) => warn!("Rate limit check failed: {error}. Reddit retrieval may be limited."),
			}
		} else {
			warn!("Vale is running without Reddit access; OAuth acquisition will continue in the background.");
			tokio::time::sleep(std::time::Duration::from_secs(15)).await;
		}
		token_daemon().await;
	});

	println!("Running Vale v{} ({}) on {listener}!", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"));

	let server = app.listen(&listener);

	// Run this server for... forever!
	if let Err(e) = server.await {
		eprintln!("Server error: {e}");
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use route_recognizer::Params;

	#[test]
	fn legacy_subreddit_wiki_redirect_preserves_the_wildcard_page() {
		let mut request = Request::builder().uri("/r/rust/w/getting-started/install").body(Body::empty()).unwrap();
		let mut params = Params::new();
		params.insert("sub".to_string(), "rust".to_string());
		params.insert("page".to_string(), "getting-started/install".to_string());
		request.set_params(params);

		let response = futures_lite::future::block_on(legacy_subreddit_wiki_redirect(request)).unwrap();
		assert_eq!(
			response.headers().get(hyper::header::LOCATION).and_then(|value| value.to_str().ok()),
			Some("/r/rust/wiki/getting-started/install")
		);
	}
}
