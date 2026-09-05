import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = (await readFile(new URL("../../static/service-worker.js", import.meta.url), "utf8")).replaceAll("__VALE_VERSION__", "test");

const runFetch = async (cachedResponse, path = "/style.css?v=test-vale-v78", options = {}) => {
	let fetchHandler;
	let fetches = 0;
	let puts = 0;
 let matches=[];
	const networkResponse = { ok: true, clone: () => networkResponse, source: "network" };
	const cache = {
		match: async request => {matches.push(typeof request === "string" ? request : request.url);return cachedResponse;},
		put: async () => {
			puts += 1;
		},
	};
	const self = {
		location: { origin: "https://vale.example" },
		addEventListener(type, handler) {
			if (type === "fetch") fetchHandler = handler;
		},
	};
	const sandbox = {
		Promise,
 Response,
		URL,
		caches: { keys: async () => [], open: async () => cache },
		fetch: async () => {
			fetches += 1;
 if(options.offline)throw Error("Offline");
			return networkResponse;
		},
		self,
	};
	vm.createContext(sandbox);
	vm.runInContext(source, sandbox, { filename: "service-worker.js" });

	let response;
	fetchHandler({
		request: {
			headers: { get: () => options.fragment || null },
			method: options.method || "GET",
			mode: options.mode || "same-origin",
			url: options.absolute || `https://vale.example${path}`,
		},
		respondWith(value) {
			response = value;
		},
	});
	response = await response;
	return { fetches, puts, response, matches };
};

test("static cache hits do not trigger a redundant network request", async () => {
	const cachedResponse = { source: "cache" };
	const result = await runFetch(cachedResponse);
	assert.equal(result.response, cachedResponse);
	assert.equal(result.fetches, 0);
	assert.equal(result.puts, 0);
});

test("static cache misses fetch and populate the cache", async () => {
	const result = await runFetch(undefined);
	assert.equal(result.response.source, "network");
	assert.equal(result.fetches, 1);
	assert.equal(result.puts, 1);
});

test("installation precaches fonts and icons without downloading unversioned script/style copies", async () => {
 let install;
 let paths;
 const self = { addEventListener(type, handler) { if (type === "install") install = handler; }, skipWaiting() {} };
 const cache = { async addAll(assets) { paths = [...assets]; } };
 vm.runInNewContext(source, { self, caches: { open: async () => cache } });
 let pending;
 install({ waitUntil(value) { pending = value; } });
 await pending;
 assert.ok(paths.includes("/fonts/source-sans-3.woff2"));
 assert.ok(paths.includes("/vale-mark.svg"));
 assert.ok(paths.includes("/style.css?v=test-vale-v78"));
 assert.ok(paths.includes("/vale-interactions.js?v=test-v55"));
 assert.ok(paths.includes("/playHLSVideo.js?v=test-v8"));
 assert.ok(!paths.includes("/style.css"));
 const base = await readFile(new URL("../../templates/base.html", import.meta.url), "utf8");
 for (const path of paths.filter(path => path.includes("?v="))) {
  assert.ok(base.includes(path.replace("test", '{{ env!("CARGO_PKG_VERSION") }}')), `page and precache must agree: ${path}`);
 }
});

test("an old asset revision cannot receive the current revision from CacheStorage", async () => {
 const result = await runFetch({ source: "cache" }, "/style.css?v=old");
 assert.equal(result.response, undefined, "unmatched revisions use the browser network path");
 assert.equal(result.fetches, 0);
});


test("offline shell caching preserves twelve privacy boundaries",async()=>{
 const cases=[
 ["/offline.html",{mode:"navigate"},true],
 ["/offline.js",{},true],
 ["/offline.html?owner=1",{mode:"navigate"},false],
 ["/reading/offline/data",{},false],
 ["/reading/offline/catalog",{},false],
 ["/saved/id/manifest.json",{},false],
 ["/comments/post1",{mode:"navigate"},false],
 ["/reading/library?export=json",{},false],
 ["/offline.html",{method:"POST"},false],
 ["/offline.html",{absolute:"https://external.example/offline.html"},false],
 ["/offline.html",{fragment:"posts-v1"},false],
 ["/img/private.png",{},false],
 ];for(const[path,options,cacheable]of cases){const result=await runFetch({source:"cache"},path,options);assert.equal(result.response?.source === "cache",cacheable,`${path} ${JSON.stringify(options)}`)}
});


test("disconnected private navigation opens only the locked shell",async()=>{for(const path of ["/","/reading","/comments/post1","/f/science/new","/reading/library?id=1","/saved/a/view.html","/reading/stories","/settings","/reading/agenda","/reading/editions","/reading/sources","/reading/watch?post=post1"]){const result=await runFetch({source:"offline-shell"},path,{mode:"navigate",offline:true});assert.equal(result.response.source,"offline-shell");assert.deepEqual(result.matches,["/offline.html"]);assert.equal(result.puts,0);}});
