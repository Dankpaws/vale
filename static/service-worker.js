const CACHE = "vale-v97-static";
const STATIC_ASSETS = [
	"/style.css?v=__VALE_VERSION__-vale-v78",
	"/vale-interactions.js?v=__VALE_VERSION__-v55",
	"/playHLSVideo.js?v=__VALE_VERSION__-v8",
	"/manifest.json",
	"/offline.html",
	"/offline.js",
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

    // Never cache authenticated navigation responses. An unavailable connection
    // opens only the empty offline shell, which still requires a pack passphrase.
    if (request.mode === "navigate" && url.pathname !== "/offline.html") {
        event.respondWith(fetch(request).catch(async () => (await caches.open(CACHE)).match("/offline.html").then(shell => shell || Response.error())));
        return;
    }
	// Never persist feeds, comments, searches, proxied media, or browser preferences.
	if ((request.mode === "navigate" && url.pathname !== "/offline.html") || url.pathname.startsWith("/img/") || url.pathname.startsWith("/vid/") || url.pathname.startsWith("/hls/") || url.pathname.startsWith("/preview/")) return;

	if (STATIC_ASSETS.includes(`${url.pathname}${url.search}`) || url.pathname === "/favicon.ico") {
		event.respondWith(
			caches.open(CACHE).then(async (cache) => {
				const cached = await cache.match(request);
				if (cached) return cached;
				const response = await fetch(request);
				if (response.ok) await cache.put(request, response.clone());
				return response;
			}),
		);
	}
});
