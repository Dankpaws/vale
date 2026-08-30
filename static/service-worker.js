const CACHE = "vale-v76-static";
const STATIC_ASSETS = [
	"/style.css",
	"/vale-interactions.js",
	"/playHLSVideo.js",
	"/manifest.json",
	"/vale-mark.svg",
	"/logo.png",
	"/touch-icon-iphone.png",
	"/fonts/source-sans-3.woff2",
	"/fonts/source-serif-4.woff2",
];

self.addEventListener("install", (event) => {
	event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(STATIC_ASSETS)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
	event.waitUntil(
		caches.keys()
			.then((names) => Promise.all(names.filter((name) => name.startsWith("vale-") && name !== CACHE).map((name) => caches.delete(name))))
			.then(() => self.clients.claim()),
	);
});

self.addEventListener("fetch", (event) => {
	const request = event.request;
	const url = new URL(request.url);
	if (request.method !== "GET" || url.origin !== self.location.origin) return;
	if (request.headers.get("X-Vale-Fragment") === "posts-v1") return;

	// Never persist feeds, comments, searches, proxied media, or browser preferences.
	if (request.mode === "navigate" || url.pathname.startsWith("/img/") || url.pathname.startsWith("/vid/") || url.pathname.startsWith("/hls/") || url.pathname.startsWith("/preview/")) return;

	if (STATIC_ASSETS.includes(url.pathname) || url.pathname === "/favicon.ico") {
		event.respondWith(
			caches.open(CACHE).then(async (cache) => {
				const cached = await cache.match(request);
				const fresh = fetch(request).then((response) => {
					if (response.ok) cache.put(request, response.clone());
					return response;
				});
				return cached || fresh;
			}),
		);
	}
});
