# Reddit access fallbacks

Vale reads Reddit by presenting itself as Reddit's official Android app
(`src/oauth.rs`, `src/oauth_resources.rs`) and using the anonymous OAuth token
Reddit issues to installed clients. That is the largest long-term risk to
Vale. Reddit can revoke that client ID, fingerprint the traffic, or retire the
flow without notice, and Vale has no second path today.

This page lists the realistic alternatives, what each costs, and how far each
would carry Vale. It is a planning document; none of it is implemented yet.

> Research date: 2026-09-30. Reddit's policies changed repeatedly in 2025–2026.
> Most details below come from developer reports and press coverage rather
> than Reddit's own pages, which could not be fetched while writing this.
> Confirm against [Reddit's Data API terms](https://support.reddithelp.com/hc/en-us/articles/42728983564564-Responsible-Builder-Policy)
> before acting on them.

## The landscape as of September 2026

- **Self-service API keys are gone.** Under the Responsible Builder Policy
  (updated November 2025), new OAuth apps need a manual approval that reportedly
  takes two to four weeks. The free tier remains about 100 queries per minute
  per OAuth client.
- **Registration deadline.** Reddit told developers on 2026-08-05 that the Data
  API will be gradually restricted. Apps must be registered by
  **2026-09-30** to stay in good standing, and selected apps are being moved to
  the Developer Platform (Devvit) by 2026-12-31. Existing approved credentials
  reportedly keep working for now.
- **Unauthenticated `.json` is dead.** Since late May 2026, appending `.json`
  to a Reddit URL without a token returns 403. Many scrapers broke then. Vale
  was unaffected because it always sends an OAuth token.
- **RSS still answers, heavily throttled.** `.rss` URLs still return 200, but
  anonymous access is limited to roughly one request per minute. Private feed
  URLs that carry `user=` and `feed=` parameters (from a logged-in account's
  RSS preferences) reportedly get more. Reddit has hinted RSS may be next.
- **Upstream is quiet.** Redlib `main` has not changed since 2026-04-24 (see
  [Tracking upstream](UPSTREAM.md)). If the Android-app flow breaks, Vale
  cannot expect an upstream fix to arrive first.

## Early warning

Watch for these before users notice:

- Log lines `OAuth mobile compatibility attempt failed` or `timed out` that
  keep repeating: the token flow itself is being refused.
- Pages rendering Reddit, OAuth, or rate-limit errors while `/healthz` is
  green (see *Troubleshooting* in [Operations](OPERATIONS.md)).
- A rising 403/429 rate from `oauth.reddit.com` for a steady request volume:
  the fingerprint or the client ID is being penalized.
- Redlib issue tracker and forks reporting the same symptoms.

## Options

Ordered from least to most disruptive.

### 1. Keep the Android-app method working (status quo)

- **What:** refresh `ANDROID_APP_VERSION_LIST` and the client fingerprint when
  Reddit ships new app versions, pace outbound requests, and cache listings.
- **Cost:** small, recurring maintenance.
- **Coverage:** everything Vale does today.
- **Risk:** it depends on Reddit tolerating an impersonated first-party
  client, which Reddit's terms do not permit. One server-side change can end
  it, and there is no appeal.

### 2. A registered OAuth app of Vale's own

- **What:** apply for Data API access under the Responsible Builder Policy and
  add an OAuth backend that uses Vale's own client ID. Each operator registers
  their own app and sets its ID and secret in the Vale environment. The
  app-only grant needs no Reddit username or password, so it keeps Vale's
  "not an account client" promise.
- **Cost:** low in code. `oauth.rs` already separates token acquisition from
  use, so this is a second backend plus two settings. The larger costs are
  the approval process and an honest description of the use (a private,
  non-commercial reader for a handful of people).
- **Coverage:** the same JSON endpoints Vale already parses, within about
  100 QPM per client. That is enough for a private instance that caches.
- **Risk:** Reddit says it is winding down the public Data API for third-party
  apps. Approval is not guaranteed and can be revoked. Registration after the
  2026-09-30 deadline may be refused. This is the best bridge, not a permanent
  answer.
- **Recommended now:** it is the only compliant path that keeps full
  functionality, and it depends on timing Vale doesn't control.

### 3. Degraded "headlines" mode over RSS

- **What:** when the JSON API is unavailable, build community listings from
  `/r/<name>/.rss` (and a post's comments from `/comments/<id>/.rss`), fetched
  on a slow schedule and cached.
- **Cost:** moderate. Vale already fetches and parses RSS/Atom for
  **Sources** (`src/sources.rs`), so fetching, parsing, storage, and
  unread tracking exist. The work is mapping RSS entries onto Vale's post
  model and marking pages as degraded.
- **Coverage:** titles, authors, links, timestamps, and HTML bodies. Scores,
  flair, galleries, video metadata, and comment trees are missing (comments
  arrive as a flat list). With a limit near one request per minute, a feed of
  twenty communities refreshes roughly every twenty minutes.
- **Risk:** reportedly the next surface Reddit may close. The private `feed=`
  token variant ties requests to a Reddit account, which Vale avoids today.

### 4. Archive services for history, not live reading

- **What:** use a third-party Reddit archive such as
  [Arctic Shift](https://github.com/ArthurHeitmann/arctic_shift) to backfill
  Saved items and permanent copies, recover deleted posts, or show old
  threads when Reddit refuses.
- **Cost:** low to moderate for a read-only lookup behind the existing
  "removed content" link (`REDLIB_PUSHSHIFT_FRONTEND` already points at
  PullPush).
- **Coverage:** historical posts and comments with some lag and incomplete
  coverage. Not a replacement for live feeds.
- **Risk:** volunteer-run services whose own Reddit access is subject to the
  same policies.

### 5. Reddit's Developer Platform (Devvit): not a fit

Devvit apps run as Reddit-hosted code inside Reddit's own interface. A
self-hosted reader that keeps reading, storage, and media on its own origin
is exactly what Devvit does not allow. Reconsider only if Reddit publishes an
external read API on top of it.

### 6. Scraping HTML: not recommended

Parsing `old.reddit.com` or `www.reddit.com` HTML avoids the API but is
explicitly prohibited, breaks often, and is the traffic Reddit is working
hardest to block. It would also make Vale's privacy promise depend on
evading detection.

### 7. Beyond Reddit: federated communities

- **What:** add a second retrieval backend for Lemmy, PieFed, or Mbin
  communities (ActivityPub, with open, documented HTTP APIs), alongside
  Reddit.
- **Cost:** large. It needs a new retrieval backend, community naming (for
  example `!name@instance`), and media handling. Vale's model of named feeds,
  Reading, Saved, and archives maps well onto it, and that model is most of
  Vale's value.
- **Coverage:** whatever communities exist there, which is different content
  from Reddit.
- **Risk:** low platform risk. Each instance sets its own rules, but no single
  company can switch the protocol off.

## Suggested plan

1. **Now:** decide whether to apply for Data API access (option 2) before the
   registration window closes, and start watching for the early-warning signs
   above.
2. **Next:** design option 2 as a drop-in OAuth backend chosen by
   configuration, keeping the Android-app flow as the default while it works.
3. **If Reddit refuses or revokes API access:** build the RSS headlines mode
   (option 3) so Vale degrades instead of going blank. Local Saved items,
   notes, and archives keep working regardless.
4. **As a hedge:** prototype a federated backend (option 7) behind the same
   feed abstraction, so Vale is not tied to one company's API policy.

## Sources

- [Responsible Builder Policy](https://support.reddithelp.com/hc/en-us/articles/42728983564564-Responsible-Builder-Policy) (Reddit Help)
- [Reddit Developer Platform App Migration Program 2026 Terms](https://support.reddithelp.com/hc/en-us/articles/47822311698452-Reddit-Developer-Platform-App-Migration-Program-2026-Terms) (Reddit Help)
- [Reddit Data API: register your app by 30 September](https://creuto.com/reddit-data-api-registration-developer-platform-2026)
- [Reddit's Data API Migration to the Developer Platform (2026)](https://www.redditapis.com/blogs/reddit-developer-platform-migration-2026)
- [Why Reddit Blocked Unauthenticated JSON in 2026](https://dev.to/tonywangca/why-reddit-blocked-unauthenticated-json-in-2026-and-how-to-still-get-reddit-data-58b9)
- [Reddit's .json returns 403 in 2026. The RSS feeds still answer.](https://dev.to/listwright/reddits-json-returns-403-in-2026-the-rss-feeds-still-answer-1gg5)
- [Reddit RSS feeds recent rate limiting and solution](https://lapcatsoftware.com/articles/2026/6/3.html)
- [Arctic Shift](https://github.com/ArthurHeitmann/arctic_shift)
