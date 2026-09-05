import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const playerSource = await readFile(new URL("../../static/playHLSVideo.js", import.meta.url), "utf8");
const audioManifest = await readFile(new URL("../fixtures/hls/audio-master.m3u8", import.meta.url), "utf8");

class FakeElement {
	constructor() {
		this.attributes = new Map();
		this.dataset = {};
		this.hidden = true;
		this.listeners = new Map();
		this.textContent = "";
	}

	addEventListener(type, handler, options = {}) {
		const handlers = this.listeners.get(type) || [];
		handlers.push({ handler, once: Boolean(options?.once) });
		this.listeners.set(type, handlers);
	}

	removeEventListener(type, handler) {
		this.listeners.set(type, (this.listeners.get(type) || []).filter((entry) => entry.handler !== handler));
	}

	dispatch(type) {
		for (const entry of [...(this.listeners.get(type) || [])]) {
			entry.handler({ type, target: this });
			if (entry.once) this.removeEventListener(type, entry.handler);
		}
	}

	remove() {
		this.removed = true;
	}

	removeAttribute(name) {
		this.attributes.delete(name);
		if (name === "src") this.src = "";
	}

	setAttribute(name, value) {
		this.attributes.set(name, value);
		if (name === "hidden") this.hidden = true;
	}
}

const createClock = () => {
	let nextId = 1;
	const timers = new Map();
	return {
		clearTimeout(id) {
			timers.delete(id);
		},
		fireDelay(delay) {
			for (const [id, timer] of [...timers]) {
				if (timer.delay !== delay) continue;
				timers.delete(id);
				timer.callback();
			}
		},
		pendingDelays() {
			return [...timers.values()].map(({ delay }) => delay).sort((left, right) => left - right);
		},
		setTimeout(callback, delay) {
			const id = nextId++;
			timers.set(id, { callback, delay });
			return id;
		},
	};
};

const settle = async () => {
	for (let index = 0; index < 8; index += 1) await Promise.resolve();
};

const createHarness = ({ attach = "immediate", native = false, userAgent = "", vendor = "" } = {}) => {
	const clock = createClock();
	const status = new FakeElement();
	const failure = new FakeElement();
	const failureMessage = new FakeElement();
	const settings = new FakeElement();
	const retry = new FakeElement();
	const frame = new FakeElement();
	const fetches = [];
	const hlsInstances = [];

	failure.querySelector = (selector) => ({
		"[data-media-failure-message]": failureMessage,
		"[data-media-settings]": settings,
	})[selector] || null;
	frame.querySelector = (selector) => ({
		".quality-selector": null,
		"[data-media-failure]": failure,
		"[data-media-retry]": retry,
		"[data-media-status]": status,
	})[selector] || null;
	frame.appendChild = () => {};

	class FakeVideo extends FakeElement {
		constructor() {
			super();
			this.dataset = {
				hlsEnabled: "true",
				hlsSrc: "/hls/audio-master.m3u8",
				mediaAutoplay: "false",
				mediaDeferred: "false",
				mediaKind: "video",
			};
			this.parentElement = frame;
			this.src = "";
		}

		canPlayType(type) {
			return native && /mpegurl/i.test(type) ? "probably" : "";
		}

		closest() {
			return frame;
		}

		load() {
			if (native && this.src) this.dispatch("loadedmetadata");
		}

		pause() {}

		play() {
			return Promise.resolve();
		}
	}

	class HlsMock {
		static ErrorTypes = { MEDIA_ERROR: "mediaError", NETWORK_ERROR: "networkError" };
		static Events = { ERROR: "error", MANIFEST_PARSED: "manifestParsed", MEDIA_ATTACHED: "mediaAttached" };

		static isSupported() {
			return true;
		}

		constructor(config) {
			this.audioTracks = [];
			this.config = config;
			this.handlers = new Map();
			this.levels = [];
			hlsInstances.push(this);
		}

		attachMedia() {
			if (attach === "immediate") this.emit(HlsMock.Events.MEDIA_ATTACHED);
		}

		destroy() {
			this.destroyed = true;
		}

		emit(event, data) {
			this.handlers.get(event)?.(event, data);
		}

		loadSource(source) {
			this.source = source;
			this.emit(HlsMock.Events.MANIFEST_PARSED, { audio: true, audioTracks: [], levels: [{}] });
		}

		on(event, handler) {
			this.handlers.set(event, handler);
		}

		recoverMediaError() {}
		startLoad() {}
		stopLoad() {}
	}

	const video = new FakeVideo();
	const document = {
		createElement: () => new FakeElement(),
		head: { appendChild: () => {} },
		querySelector: () => ({ content: "best" }),
		querySelectorAll: () => [video],
	};
	const sandbox = {
		AbortController,
		HTMLVideoElement: FakeVideo,
		Hls: HlsMock,
		clearTimeout: clock.clearTimeout,
		console,
		document,
		fetch: async (source) => {
			fetches.push(source);
			return { ok: true, text: async () => audioManifest };
		},
		navigator: { userAgent, vendor },
		setTimeout: clock.setTimeout,
	};
	sandbox.window = sandbox;
	sandbox.globalThis = sandbox;
	vm.createContext(sandbox);
	vm.runInContext(playerSource, sandbox, { filename: "playHLSVideo.js" });

	return { clock, failure, fetches, hlsInstances, status, video, media: sandbox.ValeMedia };
};

test("Apple browsers prepare an audio-bearing stream with native HLS", async () => {
	const harness = createHarness({
		native: true,
		userAgent: "Mozilla/5.0 (Macintosh) AppleWebKit/617.1 Safari/617.1",
		vendor: "Apple Computer, Inc.",
	});
	await settle();

	assert.equal(harness.hlsInstances.length, 0, "HLS.js must not be constructed on Apple media runtimes");
	assert.deepEqual(harness.fetches, ["/hls/audio-master.m3u8"]);
	assert.equal(harness.video.src, "/hls/audio-master.m3u8");
	assert.equal(harness.status.hidden, true);
	assert.equal(harness.failure.hidden, true);
	assert.deepEqual(harness.clock.pendingDelays(), []);
});

test("HLS.js accepts its manifest audio flag and runs without a blob worker", async () => {
	const harness = createHarness({ native: false, vendor: "Google Inc." });
	await settle();

	assert.equal(harness.hlsInstances.length, 1);
	assert.equal(harness.hlsInstances[0].config.enableWorker, false);
	assert.equal(harness.hlsInstances[0].source, "/hls/audio-master.m3u8");
	assert.equal(harness.status.hidden, true);
	assert.equal(harness.failure.hidden, true);
	assert.deepEqual(harness.clock.pendingDelays(), []);
});

test("a stalled HLS.js attachment falls back to native HLS after four seconds", async () => {
	const harness = createHarness({ attach: "stall", native: true, vendor: "Google Inc." });
	await settle();
	assert.deepEqual(harness.clock.pendingDelays(), [4_000, 30_000]);

	harness.clock.fireDelay(4_000);
	await settle();

	assert.equal(harness.hlsInstances[0].destroyed, true);
	assert.deepEqual(harness.fetches, ["/hls/audio-master.m3u8"]);
	assert.equal(harness.video.src, "/hls/audio-master.m3u8");
	assert.equal(harness.status.hidden, true);
	assert.equal(harness.failure.hidden, true);
	assert.deepEqual(harness.clock.pendingDelays(), []);
});

for (const native of [false, true]) {
 test(`closing ${native ? "native" : "HLS.js"} video releases resources and reopening restores position`, async () => {
  const h = createHarness({ native, vendor: native ? "Apple Computer, Inc." : "" });
  await settle();
  h.video.currentTime = 12;
  h.video.readyState = 1;
  h.media.pause(h.video);
  assert.equal(h.video.src, "");
  if (!native) assert.equal(h.hlsInstances[0].destroyed, true);
  assert.deepEqual(h.clock.pendingDelays(), []);
  h.video.currentTime = 0;
  h.media.pause(h.video); // Repeated close must not erase the saved position.
  h.media.initialize(h.video);
  await settle();
  assert.equal(h.video.currentTime, 12);
  if (!native) assert.equal(h.hlsInstances.length, 2);
  assert.equal(h.failure.hidden, true);
 });
}

test("closing during HLS attachment cancels preparation without a late failure", async () => {
 const h = createHarness({ attach: "stalled" });
 await settle();
 h.media.pause(h.video);
 assert.equal(h.hlsInstances[0].destroyed, true);
 assert.deepEqual(h.clock.pendingDelays(), []);
 h.clock.fireDelay(4000);
 h.clock.fireDelay(30000);
 assert.equal(h.failure.hidden, true);
});

test("HLS resume waits for metadata before seeking", async () => {
 const h = createHarness();
 await settle();
 h.video.currentTime = 12;
 h.media.pause(h.video);
 h.video.currentTime = 0;
 h.video.readyState = 0;
 h.media.initialize(h.video);
 await settle();
 assert.equal(h.video.currentTime, 0);
 h.video.readyState = 1;
 h.video.dispatch("loadedmetadata");
 assert.equal(h.video.currentTime, 12);
});
