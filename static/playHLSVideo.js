// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(() => {
	"use strict";

	const HLS_LIBRARY_TIMEOUT_MS = 8_000;
	const HLS_ATTACH_TIMEOUT_MS = 4_000;
	const REQUEST_TIMEOUT_MS = 8_000;
	const PREPARATION_DEADLINE_MS = 30_000;
	const NETWORK_RETRY_DELAYS_MS = [500, 1_500];
	const AUDIO_CODEC = /(?:mp4a|aac|ac-3|ec-3|opus|vorbis)/i;
	const qualitySetting = document.querySelector('meta[name="vale-video-quality"]')?.content || "best";
	const players = new WeakMap();
	let hlsLoader = null;

	const frameElement = (video) => video.closest("[data-media-frame]") || video.parentElement;
	const statusElement = (video) => frameElement(video)?.querySelector("[data-media-status]");
	const failureElement = (video) => frameElement(video)?.querySelector("[data-media-failure]");
	const supportsNativeHls = (video) => Boolean(
		video.canPlayType("application/vnd.apple.mpegurl")
		|| video.canPlayType("application/x-mpegURL"),
	);
	const prefersNativeHls = (nativeSupported) => nativeSupported && (
		/Apple/i.test(window.navigator?.vendor || "")
		|| /\b(?:iPhone|iPad|iPod)\b/i.test(window.navigator?.userAgent || "")
	);

	const setStatus = (video, message = "") => {
		const status = statusElement(video);
		if (!status) return;
		status.textContent = message;
		status.hidden = !message;
	};

	const hideFailure = (video) => {
		const failure = failureElement(video);
		if (!failure) return;
		failure.hidden = true;
		const message = failure.querySelector("[data-media-failure-message]");
		if (message) message.textContent = "";
		failure.querySelector("[data-media-settings]")?.setAttribute("hidden", "");
	};

	const showFailure = (video, message, showSettings = false) => {
		const failure = failureElement(video);
		if (!failure) return;
		const copy = failure.querySelector("[data-media-failure-message]");
		if (copy) copy.textContent = message;
		const settings = failure.querySelector("[data-media-settings]");
		if (settings) settings.hidden = !showSettings;
		failure.hidden = false;
	};

	const safePlay = (video) => {
		if (video.dataset.mediaAutoplay !== "true") return;
		const attempt = video.play();
		if (attempt?.catch) attempt.catch(() => {});
	};

	const defaultLevel = (length) => {
		if (length <= 1 || qualitySetting === "worst") return 0;
		if (qualitySetting === "medium") return Math.floor((length - 1) / 2);
		return length - 1;
	};

	const removeQualitySelector = (video) => frameElement(video)?.querySelector(".quality-selector")?.remove();

	const addQualitySelector = (video, hls) => {
		removeQualitySelector(video);
		if (!hls.levels || hls.levels.length < 2) return;
		const selector = document.createElement("select");
		selector.className = "quality-selector";
		selector.setAttribute("aria-label", "Video quality");
		hls.levels.forEach((level, index) => {
			const option = document.createElement("option");
			option.value = String(index);
			const dimensions = level.height ? `${level.height}p` : `Quality ${index + 1}`;
			const bitrate = level.bitrate ? ` · ${Math.round(level.bitrate / 1_000)} kbps` : "";
			option.textContent = `${dimensions}${bitrate}`;
			selector.appendChild(option);
		});
		const selected = defaultLevel(hls.levels.length);
		selector.selectedIndex = selected;
		hls.currentLevel = selected;
		selector.addEventListener("change", () => {
			hls.currentLevel = Number(selector.value);
		});
		frameElement(video)?.appendChild(selector);
	};

	const loadHls = () => {
		if (window.Hls) return Promise.resolve(window.Hls);
		if (hlsLoader) return hlsLoader;
		const attempt = new Promise((resolve, reject) => {
			const script = document.createElement("script");
			let settled = false;
			const finish = (error) => {
				if (settled) return;
				settled = true;
				clearTimeout(timer);
				script.onload = null;
				script.onerror = null;
				if (error) {
					script.remove();
					reject(error);
				} else {
					resolve(window.Hls);
				}
			};
			const timer = window.setTimeout(() => finish(new Error("HLS player load timed out")), HLS_LIBRARY_TIMEOUT_MS);
			script.src = "/hls.min.js";
			script.async = true;
			script.onload = () => finish(window.Hls ? null : new Error("HLS player API is unavailable"));
			script.onerror = () => finish(new Error("HLS player could not be loaded"));
			document.head.appendChild(script);
		});
		hlsLoader = attempt.catch((error) => {
			hlsLoader = null;
			throw error;
		});
		return hlsLoader;
	};

	const listen = (state, target, type, handler, options) => {
		target.addEventListener(type, handler, options);
		state.listeners.push(() => target.removeEventListener(type, handler, options));
	};

	const later = (state, delay) => new Promise((resolve) => {
		const timer = window.setTimeout(() => {
			state.timers.delete(timer);
			resolve(true);
		}, delay);
		state.timers.set(timer, () => resolve(false));
	});

	const clearTimers = (state) => {
		for (const [timer, cancel] of state.timers) {
			clearTimeout(timer);
			cancel();
		}
		state.timers.clear();
	};

	const teardown = (video, state, clearSource = true) => {
		clearTimers(state);
		for (const controller of state.controllers) controller.abort();
		state.controllers.clear();
		for (const remove of state.listeners.splice(0)) remove();
		state.hls?.destroy();
		state.hls = null;
		state.networkRecovering = false;
		removeQualitySelector(video);
		video.pause();
		if (clearSource) {
			video.removeAttribute("src");
			video.load();
		}
	};

	const failCycle = (video, state, cycle, message, showSettings = false) => {
		if (state.cycle !== cycle) return;
		state.cycle += 1;
		teardown(video, state);
		state.phase = "failed";
		setStatus(video);
		showFailure(video, message, showSettings);
	};

	const markReady = (video, state, cycle) => {
		if (state.cycle !== cycle) return;
		clearTimers(state);
		state.phase = "ready";
		setStatus(video);
		hideFailure(video);
		safePlay(video);
	};

	const nextNetworkRetry = async (video, state, cycle) => {
		if (state.networkRetries >= NETWORK_RETRY_DELAYS_MS.length) return false;
		const delay = NETWORK_RETRY_DELAYS_MS[state.networkRetries];
		state.networkRetries += 1;
		setStatus(video, `Retrying audio-aware video (${state.networkRetries}/${NETWORK_RETRY_DELAYS_MS.length})…`);
		return await later(state, delay) && state.cycle === cycle;
	};

	const manifestAdvertisesAudio = (manifest) => manifest
		.split(/\r?\n/)
		.some((line) =>
			(/^#EXT-X-MEDIA:/i.test(line) && /(?:^|,)TYPE=AUDIO(?:,|$)/i.test(line))
			|| (/^#EXT-X-STREAM-INF:/i.test(line) && (/(?:^|,)AUDIO="[^"]+"/i.test(line) || AUDIO_CODEC.test(line))),
		);

	const fetchManifest = async (video, state, cycle, source) => {
		while (state.cycle === cycle) {
			const controller = new AbortController();
			state.controllers.add(controller);
			const timer = window.setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
			try {
				const response = await fetch(source, {
					credentials: "same-origin",
					headers: { Accept: "application/vnd.apple.mpegurl, application/x-mpegURL, text/plain" },
					signal: controller.signal,
				});
				if (!response.ok) throw new Error("Manifest request failed");
				return await response.text();
			} catch (_) {
				if (state.cycle !== cycle || !await nextNetworkRetry(video, state, cycle)) throw new Error("Manifest request failed");
			} finally {
				clearTimeout(timer);
				state.controllers.delete(controller);
			}
		}
		throw new Error("Playback cycle ended");
	};

	const prepareGif = (video, state, cycle) => {
		const source = video.dataset.mp4Src || "";
		if (!source) {
			failCycle(video, state, cycle, "This GIF is temporarily unavailable.");
			return;
		}
		listen(state, video, "loadedmetadata", () => markReady(video, state, cycle), { once: true });
		listen(state, video, "error", () => failCycle(video, state, cycle, "This GIF could not be loaded."), { once: true });
		video.src = source;
		video.load();
	};

	const prepareNativeHls = async (video, state, cycle, source) => {
		let manifest;
		try {
			manifest = await fetchManifest(video, state, cycle, source);
		} catch (_) {
			failCycle(video, state, cycle, "This video could not be prepared with audio.");
			return;
		}
		if (state.cycle !== cycle) return;
		if (!manifestAdvertisesAudio(manifest)) {
			failCycle(video, state, cycle, "This video stream does not advertise audio, so Vale will not play a silent fallback.");
			return;
		}

		const recover = async () => {
			if (state.networkRecovering || state.cycle !== cycle) return;
			state.networkRecovering = true;
			const retry = await nextNetworkRetry(video, state, cycle);
			if (state.cycle !== cycle || state.phase === "ready") return;
			if (!retry) {
				failCycle(video, state, cycle, "This video could not be prepared with audio.");
				return;
			}
			state.networkRecovering = false;
			video.removeAttribute("src");
			video.load();
			video.src = source;
			video.load();
		};
		listen(state, video, "loadedmetadata", () => markReady(video, state, cycle));
		listen(state, video, "error", recover);
		video.src = source;
		video.load();
	};

	const hlsAdvertisesAudio = (hls, data) => {
		const tracks = data?.audioTracks || hls.audioTracks || [];
		const levels = data?.levels || hls.levels || [];
		return data?.audio === true || tracks.length > 0 || levels.some((level) =>
			Boolean(level.audioCodec)
			|| AUDIO_CODEC.test(level.attrs?.CODECS || "")
			|| Boolean(level.attrs?.AUDIO),
		);
	};

	const prepareHlsJs = (video, state, cycle, source, Hls, nativeSupported) => {
		const hls = new Hls({
			// Vale's effective production CSP does not permit blob workers. Running
			// HLS.js inline avoids a browser-dependent worker fallback path.
			enableWorker: false,
			manifestLoadingMaxRetry: 0,
			levelLoadingMaxRetry: 0,
			fragLoadingMaxRetry: 0,
			manifestLoadingTimeOut: REQUEST_TIMEOUT_MS,
			levelLoadingTimeOut: REQUEST_TIMEOUT_MS,
			fragLoadingTimeOut: REQUEST_TIMEOUT_MS,
		});
		state.hls = hls;
		const attachDeadline = window.setTimeout(() => {
			state.timers.delete(attachDeadline);
			if (state.cycle !== cycle || state.hls !== hls || state.phase !== "preparing") return;
			state.hls = null;
			hls.destroy();
			if (nativeSupported) {
				prepareNativeHls(video, state, cycle, source);
				return;
			}
			failCycle(video, state, cycle, "This browser could not attach Vale’s audio-aware video player.");
		}, HLS_ATTACH_TIMEOUT_MS);
		state.timers.set(attachDeadline, () => {});
		hls.on(Hls.Events.MEDIA_ATTACHED, () => {
			if (state.cycle !== cycle || state.hls !== hls) return;
			clearTimeout(attachDeadline);
			state.timers.delete(attachDeadline);
			hls.loadSource(source);
		});
		hls.on(Hls.Events.MANIFEST_PARSED, (_event, data) => {
			if (state.cycle !== cycle || state.hls !== hls) return;
			if (!hlsAdvertisesAudio(hls, data)) {
				failCycle(video, state, cycle, "This video stream does not advertise audio, so Vale will not play a silent fallback.");
				return;
			}
			addQualitySelector(video, hls);
			markReady(video, state, cycle);
		});
		hls.on(Hls.Events.ERROR, async (_event, data) => {
			if (!data.fatal || state.cycle !== cycle || state.hls !== hls) return;
			if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
				if (state.networkRecovering) return;
				state.networkRecovering = true;
				hls.stopLoad();
				const retry = await nextNetworkRetry(video, state, cycle);
				if (state.cycle !== cycle || state.phase === "ready") return;
				if (!retry) {
					failCycle(video, state, cycle, "This video could not be prepared with audio.");
					return;
				}
				state.networkRecovering = false;
				if (/manifest|level/i.test(data.details || "")) hls.loadSource(source);
				else hls.startLoad();
				return;
			}
			if (data.type === Hls.ErrorTypes.MEDIA_ERROR && state.mediaRetries < 1) {
				state.mediaRetries += 1;
				hls.recoverMediaError();
				return;
			}
			failCycle(video, state, cycle, "This video could not be prepared with audio.");
		});
		hls.attachMedia(video);
	};

	const prepareAudioHls = async (video, state, cycle, source) => {
		const nativeSupported = supportsNativeHls(video);
		// Safari and every browser on iOS already have a first-class HLS engine.
		// Prefer it instead of routing those browsers through HLS.js/MSE, whose
		// media attachment can stall without an error until the outer deadline.
		if (prefersNativeHls(nativeSupported)) {
			prepareNativeHls(video, state, cycle, source);
			return;
		}
		let Hls;
		try {
			Hls = await loadHls();
		} catch (_) {
			if (state.cycle !== cycle) return;
			if (nativeSupported) {
				prepareNativeHls(video, state, cycle, source);
				return;
			}
			failCycle(video, state, cycle, "This browser could not load Vale’s audio-aware video player.");
			return;
		}
		if (state.cycle !== cycle) return;
		if (Hls.isSupported()) {
			prepareHlsJs(video, state, cycle, source, Hls, nativeSupported);
			return;
		}
		if (nativeSupported) {
			prepareNativeHls(video, state, cycle, source);
			return;
		}
		failCycle(video, state, cycle, "This browser cannot play the available audio-aware video stream.");
	};

	const startCycle = (video, state) => {
		teardown(video, state);
		state.cycle += 1;
		const cycle = state.cycle;
		state.phase = "preparing";
		state.networkRetries = 0;
		state.mediaRetries = 0;
		state.networkRecovering = false;
		hideFailure(video);
		setStatus(video, video.dataset.mediaKind === "gif" ? "Preparing GIF…" : "Preparing audio-aware video…");
		const deadline = window.setTimeout(() => {
			failCycle(video, state, cycle, "Video playback could not be prepared within 30 seconds.");
		}, PREPARATION_DEADLINE_MS);
		state.timers.set(deadline, () => {});

		if (video.dataset.mediaKind === "gif") {
			prepareGif(video, state, cycle);
			return;
		}
		if (video.dataset.hlsEnabled !== "true") {
			failCycle(video, state, cycle, "Audio-aware video playback is disabled. Enable HLS in Settings to play this video with sound.", true);
			return;
		}
		const source = video.dataset.hlsSrc || "";
		if (!source) {
			failCycle(video, state, cycle, "This post has no audio-aware video stream, so Vale will not play a silent fallback.");
			return;
		}
		prepareAudioHls(video, state, cycle, source);
	};

	const initialize = (video) => {
		if (!(video instanceof HTMLVideoElement)) return;
		if (video.dataset.mediaDeferred === "true" && video.closest("[hidden]")) return;
		const existing = players.get(video);
		if (existing) {
			if (existing.phase === "ready") safePlay(video);
			return;
		}
		const state = {
			controllers: new Set(),
			cycle: 0,
			hls: null,
			listeners: [],
			mediaRetries: 0,
			networkRecovering: false,
			networkRetries: 0,
			phase: "new",
			timers: new Map(),
		};
		players.set(video, state);
		video.dataset.mediaInitialized = "true";
		const retry = frameElement(video)?.querySelector("[data-media-retry]");
		if (retry) listen(state, retry, "click", () => startCycle(video, state));
		startCycle(video, state);
	};

	const pause = (video) => {
		if (video instanceof HTMLVideoElement) video.pause();
	};

	const destroy = (video) => {
		if (!(video instanceof HTMLVideoElement)) return;
		const state = players.get(video);
		if (state) {
			state.cycle += 1;
			teardown(video, state);
			players.delete(video);
		} else {
			video.pause();
			video.removeAttribute("src");
			video.load();
		}
		delete video.dataset.mediaInitialized;
	};

	window.ValeMedia = { destroy, initialize, pause };
	document.querySelectorAll('video[data-vale-media][data-media-deferred="false"]').forEach(initialize);
})();
// @license-end
