(() => {
	"use strict";

	class OrderedKeyQueue {
		constructor() {
			this.items = [];
			this.keys = new Set();
		}

		enqueue(key) {
			if (this.keys.has(key)) return false;
			this.keys.add(key);
			this.items.push(key);
			return true;
		}

		shift() {
			const key = this.items.shift();
			if (key !== undefined) this.keys.delete(key);
			return key;
		}

		get length() {
			return this.items.length;
		}

		has(key) {
			return this.keys.has(key);
		}
	}

	const keyedReconciliationPlan = (currentIds, incomingIds, limit = 25) => {
		if (incomingIds.length > limit || new Set(incomingIds).size !== incomingIds.length || new Set(currentIds).size !== currentIds.length) {
			throw new Error("A keyed listing plan must be unique and bounded.");
		}
		const current = new Set(currentIds);
		const incoming = new Set(incomingIds);
		return {
			ordered: incomingIds.map((id) => ({ id, action: current.has(id) ? "reuse" : "insert" })),
			removed: currentIds.filter((id) => !incoming.has(id)),
		};
	};

	const mutationWaiterOutcome = (waiter, state, result) => {
		const superseded = waiter.hidden !== state.confirmed && waiter.epoch < state.epoch;
		const desiredStateWasVerified = Boolean(result.verified || result.collapsed) && waiter.hidden === state.confirmed;
		return {
			...result,
			ok: Boolean(result.ok || desiredStateWasVerified),
			requestedHidden: waiter.hidden,
			superseded,
		};
	};

	const hiddenIntentNeedsWrite = (confirmed, desired, uncertain = false) => uncertain || confirmed !== desired;
	const hiddenShellEvictionPlan = (hidden, pending) => ({ remove: hidden, trackPending: hidden && pending });
	const hiddenVerificationCanApply = (capturedEpoch, currentEpoch) => capturedEpoch === currentEpoch;
	const invalidateBufferedForeignState = (pendingStates, postId) => pendingStates.delete(postId);
	const mutationNeedsListingRecovery = ({ changed, wasUncertain, actualHidden, removedPendingShell, hasListing }) => (
		changed || removedPendingShell || (wasUncertain && !actualHidden && hasListing)
	);
	const mobileFeedContextShouldPin = (heroBottom, headerBottom) => heroBottom <= headerBottom;
	const mobileHomeTopInset = (headerBottom, pinned, contextHeight) => Math.max(0, headerBottom + (pinned ? Math.max(0, contextHeight) : 0) + 12);
	const serializedFormIsDirty = (baseline, current) => current !== baseline;
	const settingsSaveBarShouldActivate = ({ mobile, dirty, formTop, formBottom, viewportTop, viewportBottom, saveTop, barHeight }) => (
		mobile
		&& dirty
		&& formBottom > viewportTop
		&& formTop < viewportBottom
		&& saveTop > viewportBottom - barHeight - 12
	);
	const settingsSavedCleanTarget = (hasSavedStatus, pathname, search, hash) => (
		hasSavedStatus && pathname === "/settings" && search === "?saved=1" && hash === "#preferences"
			? "/settings#preferences"
			: ""
	);
	const quiescentUncertainStateCanRecover = ({
		snapshotUncertain,
		snapshotQueued,
		snapshotInFlight,
		snapshotEpoch,
		currentUncertain,
		currentQueued,
		currentInFlight,
		currentEpoch,
	}) => (
		snapshotUncertain
		&& !snapshotQueued
		&& !snapshotInFlight
		&& currentUncertain
		&& !currentQueued
		&& !currentInFlight
		&& currentEpoch === snapshotEpoch
	);
	const queuedListingRefreshCanStart = ({ queued, pendingMutations, workerActive, mutationQueueLength, fragmentActive, bfcacheActive }) => (
		queued
		&& pendingMutations === 0
		&& !workerActive
		&& mutationQueueLength === 0
		&& !fragmentActive
		&& !bfcacheActive
	);

	const acceptBroadcastSequence = (sequences, source, sequence) => {
		const previous = sequences.get(source) || 0;
		if (!Number.isSafeInteger(sequence) || sequence <= previous) return false;
		sequences.set(source, sequence);
		return true;
	};
	const boundedHiddenVerificationIds = (groups, limit = 250) => [...new Set(groups.flat())]
		.filter((value) => typeof value === "string" && value.length > 0 && value.length <= 80 && /^[A-Za-z0-9_-]+$/.test(value))
		.slice(0, limit);

	const appendBoundedUndo = (entries, entry, limit) => {
		const next = entries.filter((candidate) => candidate.postId !== entry.postId);
		next.push(entry);
		const evicted = next.length > limit ? next.splice(0, next.length - limit) : [];
		return { entries: next, evicted };
	};

	if (typeof window === "undefined" || typeof document === "undefined") {
		if (typeof module !== "undefined") {
			module.exports = {
				OrderedKeyQueue,
				acceptBroadcastSequence,
				appendBoundedUndo,
				boundedHiddenVerificationIds,
				hiddenIntentNeedsWrite,
				hiddenShellEvictionPlan,
				hiddenVerificationCanApply,
				invalidateBufferedForeignState,
				keyedReconciliationPlan,
				mobileFeedContextShouldPin,
				mobileHomeTopInset,
				mutationNeedsListingRecovery,
				mutationWaiterOutcome,
				quiescentUncertainStateCanRecover,
				queuedListingRefreshCanStart,
				serializedFormIsDirty,
				settingsSaveBarShouldActivate,
				settingsSavedCleanTarget,
			};
		}
		return;
	}

	const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
	const postMutationIds = new Set();
	const hiddenMutationStates = new Map();
	const hiddenMutationQueue = new OrderedKeyQueue();
	const hiddenMutationDrainWaiters = [];
	const undoStack = [];
	const UNDO_LIMIT = 12;
	const UNDO_LIFETIME = 120_000;
	const POSTS_FRAGMENT_VERSION = "posts-v1";
	const POSTS_FRAGMENT_LIMIT = 4 * 1024 * 1024;
	const POSTS_FRAGMENT_TIMEOUT = 15_000;
	const HIDDEN_MUTATION_TIMEOUT = 10_000;
	const NAVIGATION_STATE_VERSION = 3;
	const THREAD_PATCH_VERSION = 1;
	const COMMENT_SEARCH_BRANCH_BATCH = 8;
	const profileSourceTab = crypto.randomUUID?.() || `${Date.now()}-${Math.random().toString(36).slice(2)}`;
	let activeCard = null;
	let bfcacheReconcileInProgress = false;
	let broadcastSequence = 0;
	let hiddenMutationWorker = null;
	let hiddenMutationEpoch = 0;
	let listingFragmentController = null;
	let listingFragmentPromise = null;
	let listingRefreshQueued = false;
	let listingRefreshReason = "refresh";
	let listingRefreshTimer = null;
	let toastTimer = null;
	let navigationRestoreInProgress = true;
	let navigationWriteTimer = null;
	let navigationLeaving = false;
	let lastReadingFocus = null;
	let mobileFeedContextHeight = 0;
	let mobileFeedContextPinned = false;
	let refreshMobileFeedContext = null;
	let refreshSettingsSaveBar = () => {};
	const appliedThreadPatches = new Map();
	const commentSearchState = { query: "", matches: [], currentId: "", loading: false };
	const foreignProfileSequences = new Map();
	const pendingForeignHiddenStates = new Map();
	const removedPendingCardIds = new Set();
	const mobileHomeMedia = window.matchMedia("(max-width: 760px)");

	const appHeaderBottom = () => Math.max(0, document.querySelector(".app-header")?.getBoundingClientRect().bottom || 0);
	const visualViewportTop = () => window.visualViewport?.offsetTop || 0;
	const visualViewportBottom = () => (window.visualViewport ? window.visualViewport.offsetTop + window.visualViewport.height : window.innerHeight);
	const mobileHomeEnhanced = () => mobileHomeMedia.matches && Boolean(document.querySelector(".is-home-feed > .feed-hero"));
	const effectiveTopInset = () => {
		const headerBottom = appHeaderBottom();
		return mobileHomeEnhanced() ? mobileHomeTopInset(headerBottom, mobileFeedContextPinned, mobileFeedContextHeight) : headerBottom;
	};

	const scrollElementWithInset = (element, { block = "nearest", behavior = reducedMotion.matches ? "auto" : "smooth" } = {}) => {
		if (!element) return;
		if (!mobileHomeEnhanced()) {
			element.scrollIntoView({ block, behavior });
			return;
		}
		refreshMobileFeedContext?.();
		const rect = element.getBoundingClientRect();
		const inset = effectiveTopInset();
		const viewportBottom = visualViewportBottom() - 12;
		let delta = 0;
		if (block === "start" || rect.height >= viewportBottom - inset) delta = rect.top - inset;
		else if (rect.top < inset) delta = rect.top - inset;
		else if (rect.bottom > viewportBottom) delta = rect.bottom - viewportBottom;
		if (Math.abs(delta) <= 0.5) return;
		window.scrollBy({ top: delta, left: 0, behavior: "auto" });
		refreshMobileFeedContext?.();
		const settledRect = element.getBoundingClientRect();
		const settledInset = effectiveTopInset();
		if (settledRect.top < settledInset) window.scrollBy(0, settledRect.top - settledInset);
	};

	const correctMobileHomeHashTarget = () => {
		if (!mobileHomeEnhanced() || !window.location.hash) return;
		let id = window.location.hash.slice(1);
		try {
			id = decodeURIComponent(id);
		} catch (_) {
			return;
		}
		const target = document.getElementById(id);
		if (target) scrollElementWithInset(target, { block: "start", behavior: "auto" });
	};

	const measureOffFlow = (element, measurementClass) => {
		const clone = element.cloneNode(true);
		clone.hidden = false;
		clone.removeAttribute("id");
		clone.querySelectorAll("[id]").forEach((child) => child.removeAttribute("id"));
		clone.querySelectorAll("[autofocus]").forEach((child) => child.removeAttribute("autofocus"));
		clone.dataset.valeMeasuring = "true";
		if (measurementClass) clone.classList.add(measurementClass);
		document.body.appendChild(clone);
		const height = clone.getBoundingClientRect().height;
		clone.remove();
		return Math.ceil(height);
	};

	const setupMobileFeedContext = () => {
		const page = document.querySelector(".is-home-feed");
		const hero = page?.querySelector(":scope > .feed-hero");
		const context = page?.querySelector(":scope > .mobile-feed-context");
		const header = document.querySelector(".app-header");
		if (!page || !hero || !context || !header) return;

		const root = document.documentElement;
		let observer = null;
		let observedHeaderBottom = -1;
		let frame = 0;

		const setRootInset = (headerBottom, pinned, contextHeight) => {
			root.style.setProperty("--vale-mobile-home-top-inset", `${mobileHomeTopInset(headerBottom, pinned, contextHeight)}px`);
		};

		const unpin = (headerBottom) => {
			context.hidden = true;
			context.classList.remove("is-pinned");
			context.style.removeProperty("--mobile-feed-context-top");
			mobileFeedContextPinned = false;
			mobileFeedContextHeight = 0;
			setRootInset(headerBottom, false, 0);
		};

		const rebuildObserver = (headerBottom) => {
			if (!mobileHomeMedia.matches || typeof IntersectionObserver === "undefined") {
				observer?.disconnect();
				observer = null;
				observedHeaderBottom = headerBottom;
				return;
			}
			if (observer && Math.abs(observedHeaderBottom - headerBottom) <= 0.5) return;
			observer?.disconnect();
			observedHeaderBottom = headerBottom;
			observer = new IntersectionObserver(() => schedule(), {
				root: null,
				rootMargin: `${-headerBottom}px 0px 0px 0px`,
				threshold: [0, 1],
			});
			observer.observe(hero);
		};

		const update = () => {
			frame = 0;
			const headerBottom = Math.max(0, header.getBoundingClientRect().bottom);
			rebuildObserver(headerBottom);
			if (!mobileHomeMedia.matches) {
				unpin(headerBottom);
				root.classList.remove("vale-mobile-home-enhanced");
				return;
			}

			root.classList.add("vale-mobile-home-enhanced");
			const shouldPin = mobileFeedContextShouldPin(hero.getBoundingClientRect().bottom, headerBottom);
			if (!shouldPin) {
				unpin(headerBottom);
				return;
			}

			context.style.setProperty("--mobile-feed-context-top", `${headerBottom}px`);
			context.classList.add("is-pinned");
			const measuredHeight = context.hidden ? measureOffFlow(context, "is-pinned") : Math.ceil(context.getBoundingClientRect().height);
			mobileFeedContextPinned = true;
			mobileFeedContextHeight = measuredHeight;
			setRootInset(headerBottom, true, measuredHeight);
			context.hidden = false;
		};

		function schedule() {
			if (!frame) frame = requestAnimationFrame(update);
		}

		refreshMobileFeedContext = update;
		window.addEventListener("scroll", schedule, { passive: true });
		window.addEventListener("resize", schedule);
		window.addEventListener("orientationchange", schedule);
		window.addEventListener("pageshow", schedule);
		window.visualViewport?.addEventListener("resize", schedule);
		mobileHomeMedia.addEventListener?.("change", schedule);
		if (typeof ResizeObserver !== "undefined") {
			const resizeObserver = new ResizeObserver(schedule);
			resizeObserver.observe(hero);
			resizeObserver.observe(header);
		}
		update();
		if (window.location.hash) requestAnimationFrame(correctMobileHomeHashTarget);
	};

	const setupSettingsSaveBar = () => {
		const form = document.getElementById("preferences-form");
		const preferences = document.getElementById("preferences");
		const nativeSave = document.getElementById("save");
		const bar = document.querySelector("[data-settings-save-bar]");
		if (!form || !preferences || !nativeSave || !bar) return;

		const root = document.documentElement;
		const media = window.matchMedia("(max-width: 760px)");
		const baseline = new URLSearchParams(new FormData(form)).toString();
		let barHeight = 0;
		let frame = 0;
		let needsMeasure = true;

		const serialized = () => new URLSearchParams(new FormData(form)).toString();
		const update = () => {
			frame = 0;
			const mobile = media.matches;
			if (!mobile) {
				bar.hidden = true;
				root.classList.remove("settings-save-bar-enhanced");
				root.style.removeProperty("--settings-save-bar-height");
				bar.style.removeProperty("--settings-save-bar-bottom-offset");
				return;
			}

			if (needsMeasure || barHeight <= 0) {
				barHeight = measureOffFlow(bar, "");
				needsMeasure = false;
			}
			root.style.setProperty("--settings-save-bar-height", `${barHeight}px`);
			root.classList.add("settings-save-bar-enhanced");
			const viewportTop = visualViewportTop();
			const viewportBottom = visualViewportBottom();
			bar.style.setProperty("--settings-save-bar-bottom-offset", `${Math.max(0, window.innerHeight - viewportBottom)}px`);
			const formRect = form.getBoundingClientRect();
			const saveTop = nativeSave.getBoundingClientRect().top;
			const dirty = serializedFormIsDirty(baseline, serialized());
			const active = settingsSaveBarShouldActivate({
				mobile,
				dirty,
				formTop: formRect.top,
				formBottom: formRect.bottom,
				viewportTop,
				viewportBottom,
				saveTop,
				barHeight,
			});
			bar.hidden = !active;

			if (!active) return;
			const focused = document.activeElement;
			if (!focused || !form.contains(focused) || bar.contains(focused) || !focused.matches("input, select, textarea, button")) return;
			const focusedRect = focused.getBoundingClientRect();
			const exclusionTop = bar.getBoundingClientRect().top - 12;
			if (focusedRect.bottom > exclusionTop && focusedRect.top < viewportBottom) {
				window.scrollBy(0, focusedRect.bottom - exclusionTop);
			}
		};

		const schedule = (remeasure = false) => {
			needsMeasure ||= remeasure;
			if (!frame) frame = requestAnimationFrame(update);
		};

		refreshSettingsSaveBar = () => schedule(false);
		form.addEventListener("input", () => schedule(false));
		form.addEventListener("change", () => schedule(false));
		window.addEventListener("scroll", () => schedule(false), { passive: true });
		window.addEventListener("resize", () => schedule(true));
		window.addEventListener("orientationchange", () => schedule(true));
		window.addEventListener("pageshow", () => schedule(true));
		window.visualViewport?.addEventListener("resize", () => schedule(true));
		media.addEventListener?.("change", () => schedule(true));

		const cleanSavedUrl = () => {
			const current = new URL(window.location.href);
			const target = settingsSavedCleanTarget(Boolean(document.querySelector(".settings-saved-status")), current.pathname, current.search, current.hash);
			if (target) history.replaceState(history.state, "", target);
		};
		if (document.readyState === "complete") cleanSavedUrl();
		else window.addEventListener("load", cleanSavedUrl, { once: true });
		update();
	};

	const focusSavedReturnTarget = () => {
		if (window.location.pathname !== "/saved" || !window.location.hash.startsWith("#saved-")) return;
		const target = document.getElementById(window.location.hash.slice(1));
		const focusTarget = target || document.querySelector(".saved-page h1");
		if (!focusTarget) return;
		if (!focusTarget.matches("a, button, input, select, textarea, [tabindex]")) focusTarget.tabIndex = -1;
		requestAnimationFrame(() => {
			focusTarget.scrollIntoView({ block: "center", behavior: reducedMotion.matches ? "auto" : "smooth" });
			focusTarget.focus({ preventScroll: true });
		});
	};

	const syncInlineToggle = (button, expanded) => {
		const visibleLabel = expanded ? button.dataset.collapseLabel || "Collapse" : button.dataset.expandLabel || "Expand";
		const accessibleName = expanded ? button.dataset.collapseName || visibleLabel : button.dataset.expandName || visibleLabel;
		button.setAttribute("aria-expanded", String(expanded));
		button.setAttribute("aria-label", accessibleName);
		button.title = accessibleName;
		const label = button.querySelector("[data-inline-label]");
		if (label) label.textContent = visibleLabel;
	};

	const setInlineState = (panel, expanded, sourceButton, persist = true) => {
		const card = panel.closest(".post");
		const beforeTop = card ? card.getBoundingClientRect().top : 0;
		panel.hidden = !expanded;
		panel.dataset.inlineExpanded = String(expanded);
		card?.classList.toggle("is-inline-expanded", expanded);

		if (expanded) {
			panel.querySelectorAll("img[data-src]").forEach((image) => {
				if (image.dataset.srcset) {
					image.srcset = image.dataset.srcset;
					image.removeAttribute("data-srcset");
				}
				image.src = image.dataset.src;
				image.removeAttribute("data-src");
			});
			panel.querySelectorAll("video[data-vale-media]").forEach((video) => window.ValeMedia?.initialize(video));
		} else {
			panel.querySelectorAll("video[data-vale-media]").forEach((video) => window.ValeMedia?.pause(video));
		}


		const toggle = card?.querySelector(`.post_inline_toggle[data-inline-toggle="${panel.id}"]`);
		if (toggle) syncInlineToggle(toggle, expanded);

		if (!expanded) {
			if (sourceButton && panel.contains(sourceButton)) toggle?.focus({ preventScroll: true });
			requestAnimationFrame(() => {
				if (card) window.scrollBy(0, card.getBoundingClientRect().top - beforeTop);
			});
		}
		if (persist) scheduleNavigationStateWrite();
	};

	const ancestorIds = (node) => (node?.dataset.threadAncestorPath || "").trim().split(/\s+/).filter(Boolean);
	const threadProjection = () => document.querySelector("[data-thread-projection]");
	const threadNodeElements = (root = document) => [...root.querySelectorAll(".thread-node[data-thread-node-id]")];

	const threadNodeRecord = (element) => ({
		id: element.dataset.threadNodeId || "",
		parentId: element.dataset.threadParentId || "",
		rootId: element.dataset.threadRootId || "",
		ancestors: ancestorIds(element),
		ancestorComplete: element.dataset.threadAncestorComplete === "true",
		depth: Number.parseInt(element.dataset.threadDepth || "0", 10),
		kind: element.classList.contains("comment") ? "t1" : "more",
		element,
	});

	const buildThreadModel = () => {
		const projection = threadProjection();
		const nodes = new Map();
		if (!projection) return { projection: null, nodes };
		for (const element of threadNodeElements(projection)) {
			const record = threadNodeRecord(element);
			if (!record.id || nodes.has(record.id)) throw new Error(`Duplicate or missing thread node identity: ${record.id || "unknown"}`);
			nodes.set(record.id, record);
		}
		for (const record of nodes.values()) {
			const parent = nodes.get(record.parentId);
			if (!parent) continue;
			if (record.rootId !== parent.rootId) throw new Error(`Thread node ${record.id} crosses projection groups`);
			if (record.ancestors.at(-1) !== parent.id) throw new Error(`Thread node ${record.id} has a detached ancestor path`);
			if (record.depth <= parent.depth) throw new Error(`Thread node ${record.id} has a non-descending depth`);
		}
		return { projection, nodes };
	};

	const syncThreadGroup = (group) => {
		const nodes = [...group.querySelectorAll(".thread-node[data-thread-node-id]")];
		const rootId = group.dataset.threadGroupId;
		const root = nodes.find((node) => node.dataset.threadNodeId === rootId);
		const descendants = group.querySelector(":scope > [data-thread-descendants]");
		if (!root || !descendants) return;
		const repliesToggle = root.querySelector("[data-replies-toggle]");
		if (repliesToggle) {
			const descendantNodes = [...descendants.querySelectorAll(":scope > .thread-node[data-thread-node-id]")];
			repliesToggle.dataset.repliesCount = String(descendantNodes.filter((node) => node.classList.contains("comment")).length);
			repliesToggle.dataset.repliesComplete = String(!descendantNodes.some((node) => node.classList.contains("deeper_replies")));
		}
		const repliesExpanded = !repliesToggle || repliesToggle.getAttribute("aria-expanded") === "true";
		if (repliesToggle) setRepliesState(repliesToggle, repliesExpanded, false, false);
		const rootCollapsed = root.classList.contains("is-comment-collapsed");
		descendants.hidden = !repliesExpanded || rootCollapsed;

		const collapsed = new Set(
			nodes
				.filter((node) => node.classList.contains("comment") && node.classList.contains("is-comment-collapsed"))
				.map((node) => node.dataset.threadNodeId),
		);
		nodes.forEach((node) => {
			if (node === root) return;
			node.hidden = ancestorIds(node).some((ancestor) => ancestor !== rootId && collapsed.has(ancestor));
		});
	};

	const syncThreadProjection = (projection = document) => {
		projection.querySelectorAll("[data-thread-group]").forEach(syncThreadGroup);
	};

	const setCommentState = (button, expanded, persist = true, sync = true) => {
		const comment = button.closest(".comment");
		const content = document.getElementById(button.getAttribute("aria-controls"));
		if (!comment || !content) return;
		content.hidden = !expanded;
		comment.classList.toggle("is-comment-collapsed", !expanded);
		button.setAttribute("aria-expanded", String(expanded));
		const author = button.dataset.commentAuthor || "this author";
		button.setAttribute("aria-label", `${expanded ? "Collapse" : "Expand"} comment by ${author}`);
		button.title = `${expanded ? "Collapse" : "Expand"} this comment`;
		if (sync) syncThreadProjection(comment.closest("[data-thread-projection]") || document);
		if (persist) scheduleNavigationStateWrite();
	};

	const setRepliesState = (button, expanded, persist = true, sync = true) => {
		const replies = document.getElementById(button.getAttribute("aria-controls"));
		if (!replies) return;
		button.setAttribute("aria-expanded", String(expanded));
		button.classList.toggle("is-open", expanded);
		const icon = button.querySelector(".comment_children_icon");
		const label = button.querySelector("[data-replies-label]");
		if (icon) icon.textContent = expanded ? "−" : "+";
		const action = expanded ? "Hide" : "Show";
		const count = Number.parseInt(button.dataset.repliesCount || "0", 10);
		const complete = button.dataset.repliesComplete === "true";
		const author = button.dataset.commentAuthor || "this author";
		const countCopy = complete ? ` ${count} ${count === 1 ? "reply" : "replies"}` : " replies";
		const accessibleName = `${action}${countCopy} to comment by ${author}`;
		button.setAttribute("aria-label", accessibleName);
		button.title = `${action} replies`;
		if (label) label.textContent = accessibleName;
		if (sync) syncThreadProjection(button.closest("[data-thread-projection]") || document);
		if (persist) scheduleNavigationStateWrite();
	};

	const keywordFilteredComments = () => [...document.querySelectorAll('.comment[data-keyword-filtered="true"]')];

	const syncKeywordFilter = (showAll = document.body.classList.contains("comments-show-filtered"), resetIndividual = false, persist = true) => {
		const comments = keywordFilteredComments();
		const bar = document.querySelector("[data-comment-filter-bar]");
		const toggle = document.querySelector("[data-comment-filter-toggle]");
		if (resetIndividual) comments.forEach((comment) => comment.classList.remove("is-keyword-revealed"));
		document.body.classList.toggle("comments-show-filtered", showAll);
		if (bar) bar.hidden = comments.length === 0;
		document.querySelectorAll("[data-comment-filter-count]").forEach((count) => {
			count.textContent = String(comments.length);
		});
		if (toggle) {
			toggle.setAttribute("aria-pressed", String(showAll));
			const label = toggle.querySelector("[data-comment-filter-label]");
			if (label) label.textContent = showAll ? "Hide filtered comments" : "Show filtered comments";
		}
		comments.forEach((comment) => {
			const revealed = showAll || comment.classList.contains("is-keyword-revealed");
			const button = comment.querySelector(":scope .comment_keyword_notice [data-comment-reveal]");
			const state = comment.querySelector(":scope .comment_keyword_notice [data-comment-filter-state]");
			if (button) {
				button.setAttribute("aria-expanded", String(revealed));
				button.textContent = revealed ? "Hide comment" : "Show comment";
			}
			if (state) state.textContent = revealed ? "Filtered comment shown" : "Comment hidden";
		});
		if (persist) scheduleNavigationStateWrite();
	};

	const setThreadStatus = (message) => {
		let status = document.querySelector("[data-thread-status]");
		if (!status) {
			status = document.createElement("p");
			status.className = "sr-only";
			status.dataset.threadStatus = "true";
			status.setAttribute("role", "status");
			status.setAttribute("aria-live", "polite");
			document.getElementById("comments")?.appendChild(status);
		}
		if (status) status.textContent = message;
	};

	const renumberThreadPreorder = () => {
		threadNodeElements(threadProjection() || document).forEach((node, index) => {
			node.dataset.threadPreorder = String(index);
		});
	};

	const updateThreadSummary = () => {
		const commentsRegion = document.getElementById("comments");
		const projection = threadProjection();
		if (!commentsRegion || !projection) return;
		const nodes = threadNodeElements(projection);
		const comments = nodes.filter((node) => node.classList.contains("comment"));
		const continuations = nodes.filter((node) => node.classList.contains("deeper_replies"));
		const pending = continuations.filter((node) => node.dataset.continuationState === "pending").length;
		const unavailable = continuations.filter((node) => node.dataset.continuationState === "unavailable").length;
		const explicitRemaining = continuations.reduce((total, node) => {
			const count = Number.parseInt(node.dataset.continuationCount || "0", 10);
			const children = (node.dataset.continuationChildren || "").split(",").filter(Boolean).length;
			return total + Math.max(count, children);
		}, 0);
		const reported = Number.parseInt(commentsRegion.dataset.threadReportedCommentCount || "0", 10);
		const reportedGap = Math.max(0, reported - comments.length);
		const unaccounted = Math.max(0, reportedGap - explicitRemaining);
		const incompleteAncestry = comments.filter((node) => node.dataset.threadAncestorComplete !== "true").length;
		commentsRegion.dataset.threadCommentCount = String(comments.length);
		commentsRegion.dataset.threadContinuationCount = String(continuations.length);
		commentsRegion.dataset.threadPendingContinuations = String(pending);
		commentsRegion.dataset.threadUnavailableContinuations = String(unavailable);
		commentsRegion.dataset.threadEstimatedRemaining = String(Math.max(explicitRemaining, reportedGap));
		commentsRegion.dataset.threadUnaccountedCommentCount = String(unaccounted);
		commentsRegion.dataset.threadIncompleteAncestryCount = String(incompleteAncestry);
		commentsRegion.dataset.threadCoverage = continuations.length === 0 && unaccounted === 0 && incompleteAncestry === 0 ? "complete" : "incomplete";
	};

	const commentSearchQuery = () => document.getElementById("comment-search-input")?.value.trim() || "";
	const countLabel = (count, singular, plural = `${singular}s`) => `${count} ${count === 1 ? singular : plural}`;

	const updateCommentSearchPanel = () => {
		const panel = document.querySelector("[data-comment-search-panel]");
		const commentsRegion = document.getElementById("comments");
		if (!panel || !commentsRegion) return;
		const query = commentSearchState.query;
		panel.hidden = !query;
		if (!query) return;

		const matchCount = commentSearchState.matches.length;
		const currentIndex = commentSearchState.matches.findIndex((comment) => comment.dataset.threadNodeId === commentSearchState.currentId);
		const searchedCount = Number.parseInt(commentsRegion.dataset.threadSearchSearchedCount || "0", 10);
		const filteredCount = Number.parseInt(commentsRegion.dataset.threadSearchFilteredCount || "0", 10);
		const pending = Number.parseInt(commentsRegion.dataset.threadPendingContinuations || "0", 10);
		const unavailable = Number.parseInt(commentsRegion.dataset.threadUnavailableContinuations || "0", 10);
		const unaccounted = Number.parseInt(commentsRegion.dataset.threadUnaccountedCommentCount || "0", 10);
		const incompleteAncestry = Number.parseInt(commentsRegion.dataset.threadIncompleteAncestryCount || "0", 10);
		const complete = commentsRegion.dataset.threadCoverage === "complete";
		const reasons = [];
		if (pending > 0) reasons.push(`${countLabel(pending, "loadable reply branch", "loadable reply branches")} not searched yet`);
		if (unavailable > 0) reasons.push(`${countLabel(unavailable, "reply branch", "reply branches")} without usable Reddit identifiers`);
		if (unaccounted > 0) reasons.push(`${countLabel(unaccounted, "reported comment")} not supplied by Reddit`);
		if (incompleteAncestry > 0) reasons.push(`${countLabel(incompleteAncestry, "partial ancestor path")}`);

		const heading = panel.querySelector("[data-comment-search-heading]");
		const coverage = panel.querySelector("[data-comment-search-coverage]");
		const detail = panel.querySelector("[data-comment-search-detail]");
		const position = panel.querySelector("[data-comment-search-position]");
		const previous = panel.querySelector("[data-comment-search-previous]");
		const next = panel.querySelector("[data-comment-search-next]");
		const load = panel.querySelector("[data-comment-search-load]");
		if (heading) heading.textContent = `${countLabel(matchCount, "matching comment")} for “${query}”`;
		if (coverage) coverage.textContent = complete ? "All available searchable comments searched." : `Results incomplete: ${reasons.join("; ") || "Reddit returned only part of this thread"}.`;
		if (detail) {
			detail.textContent = `${countLabel(searchedCount, "searchable comment")} checked.${filteredCount > 0 ? ` ${countLabel(filteredCount, "filtered comment")} excluded for privacy.` : ""}`;
		}
		if (position) position.textContent = matchCount > 0 ? `Match ${Math.max(0, currentIndex) + 1} of ${matchCount}` : "No loaded matches";
		if (previous) previous.disabled = matchCount < 2;
		if (next) next.disabled = matchCount < 2;
		if (load) {
			load.hidden = pending === 0;
			load.disabled = commentSearchState.loading;
			load.setAttribute("aria-busy", String(commentSearchState.loading));
			load.textContent = commentSearchState.loading
				? "Searching reply branches…"
				: pending > COMMENT_SEARCH_BRANCH_BATCH
					? `Search next ${COMMENT_SEARCH_BRANCH_BATCH} reply branches`
					: `Search ${countLabel(pending, "remaining reply branch", "remaining reply branches")}`;
		}
		panel.classList.toggle("is-incomplete", !complete);
		panel.setAttribute("aria-busy", String(commentSearchState.loading));
	};

	const expandCommentSearchPath = (comment) => {
		const projection = threadProjection();
		if (!projection || !comment) return;
		const group = comment.closest("[data-thread-group]");
		const repliesToggle = group?.querySelector("[data-replies-toggle]");
		if (repliesToggle) setRepliesState(repliesToggle, true, false, false);
		const pathIds = [...ancestorIds(comment), comment.dataset.threadNodeId];
		for (const id of pathIds) {
			const pathComment = threadNodeElements(projection).find((node) => node.classList.contains("comment") && node.dataset.threadNodeId === id);
			const collapse = pathComment?.querySelector("[data-comment-collapse]");
			if (collapse) setCommentState(collapse, true, false, false);
		}
		syncThreadProjection(projection);
	};

	const activateCommentSearchMatch = (index, { focus = true, scroll = true, announce = true } = {}) => {
		const matches = commentSearchState.matches;
		if (matches.length === 0) {
			commentSearchState.currentId = "";
			updateCommentSearchPanel();
			return null;
		}
		const normalizedIndex = ((index % matches.length) + matches.length) % matches.length;
		const comment = matches[normalizedIndex];
		commentSearchState.currentId = comment.dataset.threadNodeId;
		matches.forEach((match) => {
			const current = match === comment;
			match.classList.toggle("is-comment-search-current", current);
			if (current) match.setAttribute("aria-current", "true");
			else match.removeAttribute("aria-current");
		});
		expandCommentSearchPath(comment);
		comment.tabIndex = -1;
		if (scroll) comment.scrollIntoView({ behavior: reducedMotion.matches ? "auto" : "smooth", block: "start" });
		if (focus) comment.focus({ preventScroll: true });
		updateCommentSearchPanel();
		if (announce) setThreadStatus(`Match ${normalizedIndex + 1} of ${matches.length} for ${commentSearchState.query}.`);
		scheduleNavigationStateWrite();
		return comment;
	};

	const syncCommentSearch = ({ currentId = commentSearchState.currentId, revealCurrent = false, focus = false, scroll = false, announce = false } = {}) => {
		const commentsRegion = document.getElementById("comments");
		const projection = threadProjection();
		const query = commentSearchQuery();
		const normalizedQuery = query.toLocaleLowerCase();
		if (!commentsRegion || !projection) return commentSearchState;

		const comments = threadNodeElements(projection).filter((node) => node.classList.contains("comment"));
		const searchable = comments.filter((comment) => comment.dataset.threadFilterState === "visible");
		const matches = normalizedQuery
			? searchable.filter((comment) => (comment.querySelector(":scope > .comment_right > .comment_content > .comment_body")?.textContent || "").toLocaleLowerCase().includes(normalizedQuery))
			: [];
		const matchIds = new Set(matches.map((comment) => comment.dataset.threadNodeId));
		const contextIds = new Set(matches.flatMap(ancestorIds));
		comments.forEach((comment) => {
			const id = comment.dataset.threadNodeId;
			const isMatch = matchIds.has(id);
			const isContext = !isMatch && contextIds.has(id);
			comment.classList.toggle("is-comment-search-match", isMatch);
			comment.classList.toggle("is-comment-search-context", isContext);
			comment.dataset.commentSearchMatch = String(isMatch);
			comment.dataset.commentSearchContext = String(isContext);
			if (!isMatch) {
				comment.classList.remove("is-comment-search-current");
				comment.removeAttribute("aria-current");
			}
		});

		commentSearchState.query = query;
		commentSearchState.matches = matches;
		commentSearchState.currentId = matches.some((comment) => comment.dataset.threadNodeId === currentId) ? currentId : matches[0]?.dataset.threadNodeId || "";
		commentsRegion.dataset.threadSearchQuery = query;
		commentsRegion.dataset.threadSearchMatchCount = String(matches.length);
		commentsRegion.dataset.threadSearchSearchedCount = String(searchable.length);
		commentsRegion.dataset.threadSearchFilteredCount = String(comments.length - searchable.length);
		commentsRegion.dataset.threadSearchCoverage = commentsRegion.dataset.threadCoverage;
		updateCommentSearchPanel();
		if (revealCurrent && commentSearchState.currentId) {
			const currentIndex = matches.findIndex((comment) => comment.dataset.threadNodeId === commentSearchState.currentId);
			activateCommentSearchMatch(currentIndex, { focus, scroll, announce });
		}
		return commentSearchState;
	};

	const searchRemainingCommentBranches = async (button) => {
		if (commentSearchState.loading) return;
		commentSearchState.loading = true;
		updateCommentSearchPanel();
		let branches = 0;
		let addedComments = 0;
		let failed = false;
		while (branches < COMMENT_SEARCH_BRANCH_BATCH) {
			const continuation = threadNodeElements(threadProjection() || document).find(
				(node) => node.matches('button.deeper_replies[data-continuation-state="pending"][data-comments-url]') && node.dataset.loading !== "true",
			);
			if (!continuation) break;
			const outcome = await loadMoreReplies(continuation, { announce: false, focusLoaded: false, preservePosition: false });
			if (!outcome?.ok) {
				failed = true;
				break;
			}
			branches += 1;
			addedComments += outcome.addedComments || 0;
			syncCommentSearch({ currentId: commentSearchState.currentId });
		}
		commentSearchState.loading = false;
		const state = syncCommentSearch({ currentId: commentSearchState.currentId });
		const pending = Number.parseInt(document.getElementById("comments")?.dataset.threadPendingContinuations || "0", 10);
		const focusTarget = pending > 0 ? button : document.querySelector("[data-comment-search-clear]");
		focusTarget?.focus({ preventScroll: true });
		if (failed) {
			setThreadStatus(`Search paused after ${countLabel(branches, "reply branch", "reply branches")}. A visible branch could not load; retry it or continue searching.`);
		} else if (pending > 0) {
			setThreadStatus(`Searched ${countLabel(branches, "reply branch", "reply branches")} and loaded ${countLabel(addedComments, "comment")}. ${countLabel(pending, "reply branch", "reply branches")} still remain.`);
		} else {
			setThreadStatus(`Reply search finished with ${countLabel(state.matches.length, "matching comment")}. Coverage details are shown above the thread.`);
		}
		scheduleNavigationStateWrite();
	};

	const parsePatchElement = (html) => {
		const template = document.createElement("template");
		template.innerHTML = String(html || "").trim();
		if (template.content.children.length !== 1) throw new Error("A continuation node did not render as one element");
		const element = template.content.firstElementChild;
		if (!element?.matches(".thread-node[data-thread-node-id]")) throw new Error("A continuation node is missing normalized identity");
		return element;
	};

	const continuationButton = (continuationId) =>
		threadNodeElements(threadProjection() || document).find(
			(node) => node.classList.contains("deeper_replies") && node.dataset.threadNodeId === continuationId,
		) || null;

	const stageThreadPatch = (button, patch) => {
		if (!patch || patch.version !== THREAD_PATCH_VERSION) throw new Error("Vale received an unsupported continuation patch");
		const model = buildThreadModel();
		if (!model.projection) throw new Error("The current page has no normalized thread model");
		if (patch.continuationId !== button.dataset.threadNodeId) throw new Error("The continuation patch identity changed during loading");
		if (!Array.isArray(patch.nodes) || patch.nodes.length > 2000) throw new Error("The continuation patch is not bounded");

		const target = model.nodes.get(button.dataset.threadParentId);
		const continuation = model.nodes.get(button.dataset.threadNodeId);
		if (!target || target.kind !== "t1" || !continuation || continuation.kind !== "more") throw new Error("The continuation target is no longer available");
		if (patch.requestedParentId !== target.id || patch.sourceRoot?.nodeId !== target.id) throw new Error("The continuation parent does not match the requested branch");
		const group = button.closest("[data-thread-group]");
		const groupRoot = model.nodes.get(group?.dataset.threadGroupId || "");
		if (!group || !groupRoot || groupRoot.id !== target.rootId) throw new Error("The continuation target has a detached thread group");
		if (patch.postId !== group.dataset.threadPostId) throw new Error("The continuation patch belongs to another post");
		if (group.dataset.threadSort && patch.sort !== group.dataset.threadSort) throw new Error("The continuation patch uses another comment sort");

		const staged = new Map(model.nodes);
		const incoming = new Set();
		const imported = [];
		for (const patchNode of patch.nodes) {
			if (!patchNode?.nodeId || incoming.has(patchNode.nodeId)) throw new Error("The continuation patch repeats a node identity");
			if (patchNode.nodeId === patch.continuationId) throw new Error("Reddit returned the same unresolved continuation");
			incoming.add(patchNode.nodeId);
			const sourceAncestors = Array.isArray(patchNode.ancestorPath) ? patchNode.ancestorPath : [];
			const sourceRootIndex = sourceAncestors.indexOf(patch.sourceRoot.nodeId);
			if (sourceRootIndex < 0) throw new Error(`Continuation node ${patchNode.nodeId} is outside the returned parent`);
			const relativeAncestors = sourceAncestors.slice(sourceRootIndex + 1);
			const ancestors = [...target.ancestors, target.id, ...relativeAncestors];
			const expectedParent = ancestors.at(-1);
			if (patchNode.parentId !== expectedParent) throw new Error(`Continuation node ${patchNode.nodeId} has a detached parent`);
			const relativeDepth = Number(patchNode.depth) - Number(patch.sourceRoot.depth);
			if (!Number.isInteger(relativeDepth) || relativeDepth < 1) throw new Error(`Continuation node ${patchNode.nodeId} has an invalid depth`);
			const depth = target.depth + relativeDepth;
			const existing = staged.get(patchNode.nodeId);
			if (existing) {
				if (existing.parentId !== patchNode.parentId || existing.rootId !== target.rootId) {
					throw new Error(`Continuation node ${patchNode.nodeId} conflicts with the current thread`);
				}
				continue;
			}
			if (!staged.has(expectedParent)) throw new Error(`Continuation node ${patchNode.nodeId} arrived before its parent`);

			const element = parsePatchElement(patchNode.html);
			if (element.dataset.threadNodeId !== patchNode.nodeId) throw new Error("Rendered continuation identity does not match its metadata");
			element.dataset.threadParentId = patchNode.parentId;
			element.dataset.threadRootId = target.rootId;
			element.dataset.threadAncestorPath = ancestors.join(" ");
			element.dataset.threadAncestorComplete = String(target.ancestorComplete);
			element.dataset.threadDepth = String(depth);
			const relativeGroupDepth = Math.max(0, depth - groupRoot.depth);
			element.style.setProperty("--thread-wide-indent", `${Math.min(84, relativeGroupDepth * 14)}px`);
			element.style.setProperty("--thread-narrow-indent", `${Math.min(16, relativeGroupDepth * 8)}px`);
			const depthLabel = element.querySelector(".comment_parent_context > span:first-child");
			if (depthLabel) depthLabel.textContent = `Depth ${depth}`;
			const record = threadNodeRecord(element);
			staged.set(record.id, record);
			imported.push(element);
		}
		return { group, imported };
	};

	const recordThreadPatch = (requestUrl, patch) => {
		appliedThreadPatches.set(patch.continuationId, { continuationId: patch.continuationId, requestUrl, response: patch });
	};

	const mergeThreadPatch = async (requestUrl, patch, { restoring = false, announce = true, focusLoaded = true, preservePosition = true } = {}) => {
		let button = continuationButton(patch?.continuationId || "");
		if (!button) {
			if (patch?.continuationId) recordThreadPatch(requestUrl, patch);
			return { addedComments: 0, addedContinuations: 0, idempotent: true };
		}
		const beforeTop = button.getBoundingClientRect().top;
		const restoreFocus = !restoring && focusLoaded;
		const { group, imported } = stageThreadPatch(button, patch);
		const fragment = document.createDocumentFragment();
		imported.forEach((node) => fragment.appendChild(node));
		button.replaceWith(fragment);
		renumberThreadPreorder();
		updateThreadSummary();
		syncKeywordFilter(document.body.classList.contains("comments-show-filtered"), false, false);
		syncThreadProjection(group.closest("[data-thread-projection]") || document);
		buildThreadModel();
		recordThreadPatch(requestUrl, patch);
		const search = syncCommentSearch({ currentId: commentSearchState.currentId });

		const addedComments = imported.filter((node) => node.classList.contains("comment")).length;
		const addedContinuations = imported.filter((node) => node.classList.contains("deeper_replies")).length;
		const focusTarget = imported.find((node) => node.classList.contains("comment"))?.querySelector("[data-comment-collapse]") || imported[0] || group.querySelector("[data-comment-collapse]");
		requestAnimationFrame(() => {
			const first = imported[0];
			if (first && preservePosition) window.scrollBy(0, first.getBoundingClientRect().top - beforeTop);
			if (restoreFocus) focusTarget?.focus({ preventScroll: true });
		});
		if (announce) {
			const searchSuffix = search.query ? ` Search now has ${countLabel(search.matches.length, "matching comment")} among loaded replies.` : "";
			setThreadStatus(addedComments > 0 ? `Loaded ${addedComments} more ${addedComments === 1 ? "reply" : "replies"}.${searchSuffix}` : `This branch is up to date.${searchSuffix}`);
		}
		scheduleNavigationStateWrite();
		return { addedComments, addedContinuations, idempotent: imported.length === 0 };
	};

	const fetchThreadPatch = async (requestUrl) => {
		const url = new URL(requestUrl, window.location.href);
		if (url.origin !== window.location.origin) throw new Error("A continuation request cannot leave Vale");
		const response = await fetch(url, {
			credentials: "same-origin",
			headers: { Accept: "application/vnd.vale.thread-patch+json" },
		});
		const payload = await response.json().catch(() => null);
		if (!response.ok || response.redirected) throw new Error(payload?.error || `Comment request failed with ${response.status}`);
		return payload;
	};

	const loadMoreReplies = async (button, { announce = true, focusLoaded = true, preservePosition = true } = {}) => {
		if (button.dataset.loading === "true") return { ok: false, busy: true };
		const parentId = button.dataset.parentId;
		const requestPath = button.dataset.commentsUrl;
		const label = button.querySelector(".deeper_replies_label");
		const originalLabel = label?.textContent || "Load more replies here";
		if (!parentId || !requestPath) return { ok: false };

		const requestUrl = new URL(requestPath, window.location.href);
		requestUrl.searchParams.set("thread_patch", "1");
		requestUrl.searchParams.set("continuation", button.dataset.threadNodeId);
		const sort = button.closest("[data-thread-group]")?.dataset.threadSort || document.getElementById("commentSortSelect")?.value;
		if (sort) requestUrl.searchParams.set("sort", sort);
		const searchQuery = commentSearchQuery();
		if (searchQuery) {
			requestUrl.searchParams.set("q", searchQuery);
			requestUrl.searchParams.set("type", "comment");
		}
		button.dataset.loading = "true";
		button.disabled = true;
		button.classList.remove("has-error");
		button.setAttribute("aria-busy", "true");
		if (label) label.textContent = "Loading replies…";
		try {
			const patch = await fetchThreadPatch(requestUrl.href);
			const result = await mergeThreadPatch(requestUrl.href, patch, { announce, focusLoaded, preservePosition });
			return { ok: true, ...result };
		} catch (error) {
			console.warn("Vale could not merge the additional replies in place.", error);
			button.dataset.loading = "false";
			button.disabled = false;
			button.classList.add("has-error");
			button.removeAttribute("aria-busy");
			if (label) label.textContent = "Couldn’t load — try again";
			button.title = originalLabel;
			if (announce) setThreadStatus("Couldn’t load more replies. The branch is unchanged; try again.");
			return { ok: false, error };
		}
	};

	const navigationRouteKey = () => `${window.location.pathname}${window.location.search}`;

	const pageAnchorElement = (anchor) => {
		if (!anchor) return null;
		if (anchor.kind === "thread") return threadNodeElements(threadProjection() || document).find((node) => node.dataset.threadNodeId === anchor.id) || null;
		if (anchor.kind === "post") return [...document.querySelectorAll(".post[data-post-id]")].find((post) => post.dataset.postId === anchor.id) || null;
		return anchor.id ? document.getElementById(anchor.id) : null;
	};

	const capturePageAnchor = () => {
		refreshMobileFeedContext?.();
		const line = effectiveTopInset();
		const candidates = [...threadNodeElements(threadProjection() || document), ...document.querySelectorAll(".post[data-post-id]")].filter((element) => {
			const rect = element.getBoundingClientRect();
			return rect.width > 0 && rect.height > 0;
		});
		const element = candidates.find((candidate) => candidate.getBoundingClientRect().bottom > line) || candidates.at(-1);
		if (!element) return { kind: "scroll", id: "", offset: 0, scrollY: window.scrollY };
		return {
			kind: element.matches(".thread-node") ? "thread" : "post",
			id: element.dataset.threadNodeId || element.dataset.postId || element.id,
			offset: Math.round(element.getBoundingClientRect().top - line),
			scrollY: window.scrollY,
		};
	};

	const captureFocus = () => {
		const focused = document.activeElement;
		if (!focused || focused === document.body || focused === document.documentElement) return lastReadingFocus;
		const node = focused.closest?.(".thread-node[data-thread-node-id]");
		if (node) {
			let control = "node";
			if (focused.matches("[data-comment-collapse]")) control = "collapse";
			else if (focused.matches("[data-replies-toggle]")) control = "replies";
			else if (focused.matches("[data-comment-reveal]")) control = "reveal";
			else if (focused.matches(".deeper_replies")) control = "continuation";
			else if (focused.matches("[data-thread-parent-link]")) control = "parent";
			lastReadingFocus = { kind: "thread", id: node.dataset.threadNodeId, control };
			return lastReadingFocus;
		}
		const post = focused.closest?.(".post[data-post-id]");
		if (post) {
			lastReadingFocus = { kind: "post", id: post.dataset.postId, control: focused.matches("[data-inline-toggle]") ? "inline" : "card" };
			return lastReadingFocus;
		}
		if (focused.id) {
			lastReadingFocus = { kind: "element", id: focused.id, control: "" };
			return lastReadingFocus;
		}
		return lastReadingFocus;
	};

	const focusElement = (descriptor) => {
		if (!descriptor) return null;
		if (descriptor.kind === "thread") {
			const node = threadNodeElements(threadProjection() || document).find((element) => element.dataset.threadNodeId === descriptor.id);
			if (!node) return null;
			if (descriptor.control === "collapse") return node.querySelector("[data-comment-collapse]");
			if (descriptor.control === "replies") return node.querySelector("[data-replies-toggle]");
			if (descriptor.control === "reveal") return node.querySelector("[data-comment-reveal]");
			if (descriptor.control === "parent") return node.querySelector("[data-thread-parent-link]");
			return node;
		}
		if (descriptor.kind === "post") {
			const post = [...document.querySelectorAll(".post[data-post-id]")].find((element) => element.dataset.postId === descriptor.id);
			return descriptor.control === "inline" ? post?.querySelector("[data-inline-toggle]") : post;
		}
		return descriptor.id ? document.getElementById(descriptor.id) : null;
	};

	const captureThreadPresentation = () => {
		const projection = threadProjection();
		if (!projection) return null;
		return {
			patches: [...appliedThreadPatches.values()],
			groupStates: [...projection.querySelectorAll("[data-thread-group]")].map((group) => ({
				id: group.dataset.threadGroupId,
				expanded: group.querySelector("[data-replies-toggle]")?.getAttribute("aria-expanded") !== "false",
			})),
			commentStates: [...projection.querySelectorAll(".comment[data-thread-node-id]")].map((comment) => ({
				id: comment.dataset.threadNodeId,
				expanded: comment.querySelector("[data-comment-collapse]")?.getAttribute("aria-expanded") !== "false",
			})),
			revealedComments: [...projection.querySelectorAll(".comment.is-keyword-revealed[data-thread-node-id]")].map((comment) => comment.dataset.threadNodeId),
			showFiltered: document.body.classList.contains("comments-show-filtered"),
			search: {
				query: commentSearchState.query,
				currentId: commentSearchState.currentId,
			},
		};
	};

	const captureFeedPresentation = () => {
		const cards = [...document.querySelectorAll('.post:not(.highlighted)[data-post-id]')];
		if (cards.length === 0) return null;
		return {
			activePostId: activeCard?.dataset.postId || document.querySelector(".post.is-keyboard-active[data-post-id]")?.dataset.postId || "",
			expandedPanels: [...document.querySelectorAll(".post_inline_panel[id]:not([hidden])")].map((panel) => panel.id),
		};
	};

	const navigationPayload = (includePatchBodies = true) => {
		const thread = captureThreadPresentation();
		if (thread && !includePatchBodies) {
			thread.patches = thread.patches.map((patch) => ({ continuationId: patch.continuationId, requestUrl: patch.requestUrl, response: null }));
		}
		return {
			version: NAVIGATION_STATE_VERSION,
			routeKey: navigationRouteKey(),
			anchor: capturePageAnchor(),
			focus: captureFocus(),
			thread,
			feed: captureFeedPresentation(),
		};
	};

	const writeNavigationState = () => {
		if (navigationRestoreInProgress || navigationLeaving || (!threadProjection() && document.querySelectorAll('.post:not(.highlighted)[data-post-id]').length === 0)) return;
		const current = history.state && typeof history.state === "object" ? history.state : {};
		try {
			const payload = navigationPayload(true);
			history.replaceState({ ...current, valeNavigation: payload }, "");
		} catch (error) {
			try {
				history.replaceState({ ...current, valeNavigation: navigationPayload(false) }, "");
				setThreadStatus("This large thread will reload saved branches when you return with Back.");
			} catch (compactError) {
				console.warn("Vale could not preserve this page’s navigation state.", compactError || error);
			}
		}
	};

	function scheduleNavigationStateWrite() {
		if (navigationRestoreInProgress || navigationLeaving) return;
		window.clearTimeout(navigationWriteTimer);
		navigationWriteTimer = window.setTimeout(writeNavigationState, 90);
	}

	const captureBeforeNavigation = (event = null) => {
		window.clearTimeout(navigationWriteTimer);
		writeNavigationState();
		navigationLeaving = true;
		if (event) queueMicrotask(() => {
			if (event.defaultPrevented) navigationLeaving = false;
		});
		else window.setTimeout(() => {
			if (document.visibilityState === "visible") navigationLeaving = false;
		}, 15_000);
	};

	const applyThreadPresentation = (state) => {
		const projection = threadProjection();
		if (!projection || !state) return;
		const groupStates = new Map((state.groupStates || []).map((entry) => [entry.id, entry.expanded]));
		projection.querySelectorAll("[data-thread-group]").forEach((group) => {
			const button = group.querySelector("[data-replies-toggle]");
			if (button && groupStates.has(group.dataset.threadGroupId)) setRepliesState(button, groupStates.get(group.dataset.threadGroupId), false, false);
		});
		const commentStates = new Map((state.commentStates || []).map((entry) => [entry.id, entry.expanded]));
		projection.querySelectorAll(".comment[data-thread-node-id]").forEach((comment) => {
			const button = comment.querySelector("[data-comment-collapse]");
			if (button && commentStates.has(comment.dataset.threadNodeId)) setCommentState(button, commentStates.get(comment.dataset.threadNodeId), false, false);
			comment.classList.toggle("is-keyword-revealed", (state.revealedComments || []).includes(comment.dataset.threadNodeId));
		});
		syncKeywordFilter(Boolean(state.showFiltered), false, false);
		syncThreadProjection(projection);
		const savedSearch = state.search?.query === commentSearchQuery() ? state.search : null;
		syncCommentSearch({ currentId: savedSearch?.currentId || "", revealCurrent: Boolean(savedSearch?.currentId), focus: false, scroll: false, announce: false });
	};

	const applyFeedPresentation = (state) => {
		if (!state) return;
		const expanded = new Set(state.expandedPanels || []);
		document.querySelectorAll(".post_inline_panel[id]").forEach((panel) => setInlineState(panel, expanded.has(panel.id), null, false));
		const card = [...document.querySelectorAll('.post:not(.highlighted)[data-post-id]')].find((candidate) => candidate.dataset.postId === state.activePostId);
		if (card) setActiveCard(card, false, false, false);
	};

	const restorePageAnchor = async (anchor, focus) => {
		refreshMobileFeedContext?.();
		await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
		refreshMobileFeedContext?.();
		const element = pageAnchorElement(anchor);
		if (element) {
			const restoreElementOffset = () => {
				window.scrollBy(0, element.getBoundingClientRect().top - (effectiveTopInset() + Number(anchor.offset || 0)));
			};
			restoreElementOffset();
			await new Promise((resolve) => requestAnimationFrame(resolve));
			refreshMobileFeedContext?.();
			restoreElementOffset();
		} else if (anchor && Number.isFinite(anchor.scrollY)) {
			window.scrollTo(0, anchor.scrollY);
		}
		const restoredFocus = focusElement(focus);
		restoredFocus?.focus({ preventScroll: true });
		if (restoredFocus && mobileHomeEnhanced()) scrollElementWithInset(restoredFocus, { block: "nearest", behavior: "auto" });
	};

	const restoreNavigationState = async () => {
		const saved = history.state?.valeNavigation;
		if (!saved || saved.version !== NAVIGATION_STATE_VERSION || saved.routeKey !== navigationRouteKey()) {
			navigationRestoreInProgress = false;
			writeNavigationState();
			if (commentSearchState.query && commentSearchState.currentId) {
				window.setTimeout(() => {
					const index = commentSearchState.matches.findIndex((comment) => comment.dataset.threadNodeId === commentSearchState.currentId);
					activateCommentSearchMatch(index, { focus: false, scroll: true, announce: false });
				}, 240);
			}
			return;
		}
		navigationRestoreInProgress = true;
		try {
			for (const record of saved.thread?.patches || []) {
				try {
					const patch = record.response || (await fetchThreadPatch(record.requestUrl));
					await mergeThreadPatch(record.requestUrl, patch, { restoring: true, announce: false });
				} catch (error) {
					console.warn("Vale could not restore one loaded comment branch.", error);
					setThreadStatus("One previously loaded reply branch could not be restored. Its visible loader can retry it.");
				}
			}
			applyThreadPresentation(saved.thread);
			applyFeedPresentation(saved.feed);
			await restorePageAnchor(saved.anchor, saved.focus);
		} finally {
			navigationRestoreInProgress = false;
			writeNavigationState();
		}
	};

	const feedCards = () => [...document.querySelectorAll('.post:not(.highlighted)[data-post-permalink]')].filter((card) => !card.hidden);

	const setActiveCard = (card, focus = false, scroll = true, persist = true) => {
		activeCard?.classList.remove("is-keyboard-active");
		activeCard?.removeAttribute("aria-current");
		activeCard = card && !card.hidden ? card : null;
		if (!activeCard) return;
		activeCard.classList.add("is-keyboard-active");
		activeCard.setAttribute("aria-current", "true");
		if (scroll) scrollElementWithInset(activeCard);
		if (focus) activeCard.focus({ preventScroll: true });
		if (persist) scheduleNavigationStateWrite();
	};

	const setCardHidden = (card, hidden) => {
		card.hidden = hidden;
		card.toggleAttribute("aria-hidden", hidden);
		if (hidden) card.querySelectorAll("video[data-vale-media]").forEach((video) => window.ValeMedia?.pause(video));
	};

	const showToast = (message, actionLabel = "", action = null, duration = 7000) => {
		let toast = document.querySelector("[data-vale-toast]");
		if (!toast) {
			toast = document.createElement("div");
			toast.className = "vale-toast";
			toast.dataset.valeToast = "true";
			toast.setAttribute("role", "status");
			toast.setAttribute("aria-live", "polite");
			document.body.appendChild(toast);
		}
		clearTimeout(toastTimer);
		toast.replaceChildren();
		const copy = document.createElement("span");
		copy.textContent = message;
		toast.appendChild(copy);
		if (actionLabel && action) {
			const button = document.createElement("button");
			button.type = "button";
			button.textContent = actionLabel;
			button.addEventListener("click", action);
			toast.appendChild(button);
		}
		requestAnimationFrame(() => toast.classList.add("is-visible"));
		toastTimer = setTimeout(() => toast.classList.remove("is-visible"), duration);
		return toast.querySelector("button");
	};

	const submitOfflineSave = async (form) => {
		if (!form || form.dataset.offlineSavePending === "true") return;
		const button = form.querySelector("[data-offline-save-submit]");
		const status = form.querySelector("[data-offline-save-status]");
		const originalLabel = button?.textContent || "Save";
		form.dataset.offlineSavePending = "true";
		form.setAttribute("aria-busy", "true");
		if (button) {
			button.disabled = true;
			button.textContent = originalLabel === "Retry" ? "Retrying…" : "Saving…";
		}
		if (status) status.textContent = "Starting the offline save.";
		try {
			const response = await fetch(form.action, {
				method: "POST",
				credentials: "same-origin",
				headers: { Accept: "text/html" },
			});
			const destination = new URL(response.url, window.location.href);
			if (!response.ok || !response.redirected || destination.origin !== window.location.origin || !/^\/saved\/[^/]+$/.test(destination.pathname)) {
				throw new Error(`Offline save failed with ${response.status}`);
			}
			captureBeforeNavigation();
			window.location.assign(destination.href);
		} catch (error) {
			console.warn("Vale could not start the offline save.", error);
			navigationLeaving = false;
			delete form.dataset.offlineSavePending;
			form.removeAttribute("aria-busy");
			if (button) {
				button.disabled = false;
				button.textContent = originalLabel;
				button.focus({ preventScroll: true });
			}
			if (status) status.textContent = "Couldn’t start the offline save. Try again.";
			showToast("Couldn’t start the offline save. Try again.", "", null, 5500);
		}
	};

	const validPostId = (value) => typeof value === "string" && value.length > 0 && value.length <= 80 && /^[A-Za-z0-9_-]+$/.test(value);
	const listingCollection = () => document.querySelector('#posts[data-vale-listing="posts-v1"]');

	const listingEnvironment = () => {
		const collection = listingCollection();
		const region = collection?.closest("#column_one") || collection?.parentElement;
		const pagination = region?.querySelector("[data-listing-pagination]");
		const statusMessage = region?.querySelector("[data-listing-status-message]");
		const renderKind = collection?.dataset.valeRenderKind || "";
		if (!collection || !pagination || !statusMessage || !["direct", "search"].includes(renderKind)) return null;
		return { collection, pagination, statusMessage, renderKind };
	};

	const cardFromListingEntry = (entry, renderKind) => {
		if (renderKind === "direct") return entry.matches("article.post[data-post-id]") ? entry : null;
		if (!entry.matches('div.search-result-entry[data-vale-search-result="1"]')) return null;
		const directCards = [...entry.children].filter((child) => child.matches("article.post[data-post-id]"));
		return directCards.length === 1 ? directCards[0] : null;
	};

	const destroyCardMedia = (card) => {
		card?.querySelectorAll("video[data-vale-media]").forEach((video) => {
			try {
				window.ValeMedia?.destroy?.(video);
				if (!window.ValeMedia?.destroy) {
					video.pause();
					video.removeAttribute("src");
					video.load();
				}
			} catch (error) {
				console.warn("Vale could not fully tear down removed media.", error);
			}
		});
	};

	const readBoundedFragment = async (response, signal) => {
		const declared = Number.parseInt(response.headers.get("content-length") || "0", 10);
		if (Number.isFinite(declared) && declared > POSTS_FRAGMENT_LIMIT) throw new Error("The listing fragment is too large.");
		const decoder = new TextDecoder("utf-8", { fatal: true });
		if (!response.body) {
			const bytes = new Uint8Array(await response.arrayBuffer());
			if (bytes.byteLength > POSTS_FRAGMENT_LIMIT) throw new Error("The listing fragment is too large.");
			return decoder.decode(bytes);
		}

		const reader = response.body.getReader();
		let byteCount = 0;
		let decoded = "";
		try {
			while (true) {
				if (signal.aborted) throw signal.reason || new DOMException("Aborted", "AbortError");
				const { done, value } = await reader.read();
				if (done) break;
				byteCount += value.byteLength;
				if (byteCount > POSTS_FRAGMENT_LIMIT) {
					await reader.cancel();
					throw new Error("The listing fragment is too large.");
				}
				decoded += decoder.decode(value, { stream: true });
				if (decoded.length > POSTS_FRAGMENT_LIMIT) {
					await reader.cancel();
					throw new Error("The decoded listing fragment is too large.");
				}
			}
			decoded += decoder.decode();
			if (decoded.length > POSTS_FRAGMENT_LIMIT) throw new Error("The decoded listing fragment is too large.");
			return decoded;
		} finally {
			reader.releaseLock();
		}
	};

	const validateSameOriginLink = (link) => {
		const url = new URL(link.getAttribute("href") || "", window.location.origin);
		if (url.origin !== window.location.origin) throw new Error("A fragment pagination link left Vale.");
	};

	const parsePostsFragment = (html, expectedRenderKind) => {
		if (/^\s*(?:<!doctype|<html|<head|<body)\b/i.test(html)) throw new Error("A document shell is not a listing fragment.");
		const parsed = new DOMParser().parseFromString(html, "text/html");
		if (parsed.querySelector("parsererror")) throw new Error("The listing fragment is not valid HTML.");
		if (parsed.doctype || parsed.head.children.length || parsed.body.children.length !== 1) throw new Error("The listing fragment must have one inert root.");
		const root = parsed.body.firstElementChild;
		if (
			!root?.matches('[data-vale-posts-fragment="1"][data-vale-fragment-version="posts-v1"]')
			|| root.dataset.valeRenderKind !== expectedRenderKind
			|| root.querySelector("script, style, link, meta, base, iframe, object, embed, template")
			|| root.querySelector("[autofocus]")
		) {
			throw new Error("The listing fragment shell is invalid.");
		}
		for (const node of parsed.body.childNodes) {
			if (node !== root && (node.nodeType !== Node.TEXT_NODE || node.textContent.trim())) throw new Error("The listing fragment has content outside its root.");
		}

		const directChildren = [...root.children];
		const regions = ["[data-vale-posts-collection]", "[data-listing-pagination]", "[data-listing-status-message]"];
		const regionMatches = regions.map((selector) => directChildren.filter((child) => child.matches(selector)));
		if (
			directChildren.length !== 3
			|| regionMatches.some((matches) => matches.length !== 1)
			|| directChildren.some((child) => regions.filter((selector) => child.matches(selector)).length !== 1)
		) throw new Error("The listing fragment is missing a unique required region.");
		const [[collection], [pagination], [statusMessage]] = regionMatches;
		const paginationLinks = [...pagination.children];
		const paginationRelations = paginationLinks.map((link) => link.getAttribute("rel") || "");
		if (
			paginationLinks.length > 2
			|| paginationLinks.some((link) => !link.matches('a[href][rel="prev"], a[href][rel="next"]'))
			|| new Set(paginationRelations).size !== paginationRelations.length
		) {
			throw new Error("The listing fragment pagination is invalid.");
		}

		const status = root.dataset.listingStatus;
		if (!["complete", "end", "retry"].includes(status)) throw new Error("The listing fragment status is invalid.");
		const rawCount = root.dataset.visibleCount || "";
		if (!/^(?:0|[1-9][0-9]*)$/.test(rawCount)) throw new Error("The listing fragment count is invalid.");
		const visibleCount = Number(rawCount);
		if (visibleCount > 25 || statusMessage.hidden !== (status === "complete") || (status !== "complete" && !statusMessage.textContent.trim())) {
			throw new Error("The listing fragment status is inconsistent.");
		}

		const elementIds = new Set();
		for (const element of root.querySelectorAll("[id]")) {
			if (!element.id || elementIds.has(element.id)) throw new Error("The listing fragment contains duplicate element IDs.");
			elementIds.add(element.id);
		}
		const postIds = new Set();
		const groupedIds = new Set();
		const entries = [...collection.children].map((entry) => {
			const card = cardFromListingEntry(entry, expectedRenderKind);
			const postId = card?.dataset.postId || "";
			if (!card || !validPostId(postId) || card.id !== postId || postIds.has(postId) || card.hidden) {
				throw new Error("The listing fragment contains an invalid or duplicate post card.");
			}
			postIds.add(postId);
			if (!card.dataset.postPermalink || !card.hasAttribute("data-content-key") || !card.dataset.groupIds) throw new Error("A listing card is missing reconciliation identity.");
			const requiredHooks = [".post_subreddit", ".post_author", ".created", ".post_title_link", "[data-post-score-value]", ".post_comments"];
			if (requiredHooks.some((selector) => card.querySelectorAll(selector).length !== 1)) {
				throw new Error("A listing card is missing a unique metadata hook.");
			}
			if (
				expectedRenderKind === "search"
				&& (entry.children.length !== 2 || entry.querySelectorAll(":scope > .search-result-context").length !== 1)
			) {
				throw new Error("A search result is missing its unique context hook.");
			}

			const groupIds = card.dataset.groupIds.split(",");
			if (groupIds[0] !== postId || groupIds.some((id) => !validPostId(id)) || new Set(groupIds).size !== groupIds.length) {
				throw new Error("A listing card has an invalid content group.");
			}
			for (const groupId of groupIds) {
				if (groupedIds.has(groupId)) throw new Error("The listing fragment repeats a grouped post.");
				groupedIds.add(groupId);
			}

			const groups = [...card.querySelectorAll(":scope > [data-content-group]")];
			const group = groups[0];
			const groupBodies = group ? [...group.querySelectorAll(":scope > .content-group-body")] : [];
			const groupBody = groupBodies[0];
			const rows = groupBody ? [...groupBody.querySelectorAll(":scope > [data-group-post-id]")] : [];
			const allRows = [...card.querySelectorAll("[data-group-post-id]")];
			const combined = groupBody ? [...groupBody.querySelectorAll(":scope > [data-content-group-combined]")] : [];
			if (
				groups.length !== (groupIds.length > 1 ? 1 : 0)
				|| groupBodies.length !== (group ? 1 : 0)
				|| rows.length !== (group ? groupIds.length : 0)
				|| allRows.length !== rows.length
			) throw new Error("A content group is incomplete.");
			if (
				rows.some((row, index) => row.dataset.groupPostId !== groupIds[index])
				|| new Set(rows.map((row) => row.dataset.groupPostId)).size !== rows.length
				|| combined.length !== (group ? 1 : 0)
			) {
				throw new Error("A content group has inconsistent rows.");
			}

			const forms = [...card.querySelectorAll(":scope form[data-hide-form]")];
			const form = forms[0];
			const buttons = form ? [...form.querySelectorAll(`[data-hide-post="${CSS.escape(postId)}"]`)] : [];
			const returnTargets = form ? [...form.querySelectorAll('input[type="hidden"][name="return_to"]')] : [];
			const button = buttons[0];
			const returnTo = returnTargets[0];
			const action = form ? new URL(form.getAttribute("action") || "", window.location.origin) : null;
			if (
				forms.length !== 1
				|| buttons.length !== 1
				|| form.querySelectorAll("[data-hide-post]").length !== 1
				|| returnTargets.length !== 1
				|| form.method.toLowerCase() !== "post"
				|| action.origin !== window.location.origin
				|| action.pathname !== `/posts/${encodeURIComponent(postId)}/hide`
			) {
				throw new Error("A listing card is missing its native Hide form.");
			}
			return { entry, card, postId };
		});
		if (entries.length !== visibleCount || root.querySelectorAll("article.post[data-post-id]").length !== visibleCount) {
			throw new Error("The listing fragment count does not match its cards.");
		}
		pagination.querySelectorAll("a[href]").forEach(validateSameOriginLink);
		return { root, collection, pagination, statusMessage, status, visibleCount, entries, renderKind: expectedRenderKind };
	};

	const copyAttribute = (current, fresh, name) => {
		if (!current || !fresh) return;
		if (fresh.hasAttribute(name)) current.setAttribute(name, fresh.getAttribute(name));
		else current.removeAttribute(name);
	};

	const patchTextElement = (currentCard, freshCard, selector, attributes = []) => {
		const current = currentCard.querySelector(selector);
		const fresh = freshCard.querySelector(selector);
		if (!current || !fresh) return;
		for (const attribute of attributes) copyAttribute(current, fresh, attribute);
		current.textContent = fresh.textContent;
	};

	const patchGroupRow = (current, fresh) => {
		current.className = fresh.className;
		copyAttribute(current, fresh, "data-group-post-id");
		patchTextElement(current, fresh, ".community-avatar");
		patchTextElement(current, fresh, ":scope > div > a", ["href"]);
		patchTextElement(current, fresh, ":scope > div > small");
		patchTextElement(current, fresh, ":scope > a", ["href"]);
		patchTextElement(current, fresh, ":scope > span:last-child");
	};

	const patchContentGroup = (currentCard, freshCard) => {
		const current = currentCard.querySelector(":scope > [data-content-group]");
		const fresh = freshCard.querySelector(":scope > [data-content-group]");
		if (!current && !fresh) return;
		if (!fresh) {
			current.remove();
			return;
		}
		if (!current) {
			currentCard.insertBefore(fresh, currentCard.querySelector(":scope > .post_inline_panel"));
			return;
		}

		const wasOpen = current.open;
		const currentCounts = current.querySelectorAll("[data-content-group-count]");
		const freshCounts = fresh.querySelectorAll("[data-content-group-count]");
		currentCounts.forEach((count, index) => {
			if (freshCounts[index]) count.textContent = freshCounts[index].textContent;
		});
		const currentCombinedCount = current.querySelector("[data-content-group-combined-count]");
		const freshCombinedCount = fresh.querySelector("[data-content-group-combined-count]");
		if (currentCombinedCount && freshCombinedCount) currentCombinedCount.textContent = freshCombinedCount.textContent;

		const body = current.querySelector(":scope > .content-group-body");
		const freshBody = fresh.querySelector(":scope > .content-group-body");
		if (!body || !freshBody) {
			fresh.open = wasOpen;
			current.replaceWith(fresh);
			return;
		}
		const currentRows = new Map([...body.querySelectorAll(":scope > [data-group-post-id]")].map((row) => [row.dataset.groupPostId, row]));
		const desiredRows = [...freshBody.querySelectorAll(":scope > [data-group-post-id]")].map((freshRow) => {
			const currentRow = currentRows.get(freshRow.dataset.groupPostId);
			if (!currentRow) return freshRow;
			patchGroupRow(currentRow, freshRow);
			return currentRow;
		});
		const freshCombined = freshBody.querySelector(":scope > [data-content-group-combined]");
		const currentCombined = body.querySelector(":scope > [data-content-group-combined]");
		const combined = currentCombined || freshCombined;
		if (currentCombined && freshCombined) {
			copyAttribute(currentCombined, freshCombined, "href");
			currentCombined.replaceChildren(...[...freshCombined.childNodes].map((node) => node.cloneNode(true)));
		}
		const desiredNodes = [...desiredRows, combined];
		const desiredSet = new Set(desiredNodes);
		for (const child of [...body.children]) {
			if (!desiredSet.has(child)) child.remove();
		}
		let insertionPoint = body.firstElementChild;
		for (const desired of desiredNodes) {
			if (desired === insertionPoint) {
				insertionPoint = insertionPoint.nextElementSibling;
				continue;
			}
			if (desired.parentNode === body && typeof body.moveBefore === "function") body.moveBefore(desired, insertionPoint);
			else body.insertBefore(desired, insertionPoint);
		}
		current.open = wasOpen;
	};

	const patchSurvivingCard = (current, fresh) => {
		for (const className of ["stickied", "post_blurred"]) current.classList.toggle(className, fresh.classList.contains(className));
		for (const attribute of [
			"data-post-type",
			"data-post-permalink",
			"data-content-key",
			"data-group-ids",
			"data-post-community",
			"data-post-title",
			"data-post-score",
			"data-post-comments",
			"data-post-created",
		]) copyAttribute(current, fresh, attribute);
		patchTextElement(current, fresh, ".post_subreddit", ["href"]);
		patchTextElement(current, fresh, ".post_author", ["href", "class"]);
		patchTextElement(current, fresh, ".created", ["title"]);
		patchTextElement(current, fresh, ".post_title_link", ["href"]);
		patchTextElement(current, fresh, "[data-post-score-value]");
		patchTextElement(current, fresh, ".post_comments", ["href", "title", "aria-label"]);

		const currentHideForm = current.querySelector(":scope form[data-hide-form]");
		const freshHideForm = fresh.querySelector(":scope form[data-hide-form]");
		copyAttribute(currentHideForm, freshHideForm, "action");
		const currentReturn = currentHideForm?.querySelector('input[name="return_to"]');
		const freshReturn = freshHideForm?.querySelector('input[name="return_to"]');
		copyAttribute(currentReturn, freshReturn, "value");
		const currentHide = currentHideForm?.querySelector("[data-hide-post]");
		const freshHide = freshHideForm?.querySelector("[data-hide-post]");
		for (const attribute of ["data-hide-post", "aria-label"]) copyAttribute(currentHide, freshHide, attribute);
		patchContentGroup(current, fresh);
	};

	const captureListingAnchor = (cards) => {
		refreshMobileFeedContext?.();
		const line = effectiveTopInset();
		const visibleCards = cards.filter((card) => !card.hidden);
		const index = visibleCards.findIndex((card) => card.getBoundingClientRect().bottom > line);
		const card = index >= 0 ? visibleCards[index] : visibleCards.at(-1);
		return card ? { card, visibleIndex: Math.max(index, 0), top: card.getBoundingClientRect().top } : null;
	};

	const patchPagination = (current, fresh) => {
		copyAttribute(current, fresh, "aria-label");
		const currentByRelation = new Map(
			[...current.querySelectorAll(":scope > a[rel]")].map((link) => [link.getAttribute("rel"), link]),
		);
		const desired = [...fresh.querySelectorAll(":scope > a[rel]")].map((freshLink) => {
			const currentLink = currentByRelation.get(freshLink.getAttribute("rel"));
			if (!currentLink) return freshLink;
			for (const attribute of ["href", "rel", "accesskey", "aria-label", "title"]) copyAttribute(currentLink, freshLink, attribute);
			currentLink.textContent = freshLink.textContent;
			return currentLink;
		});
		const desiredSet = new Set(desired);
		for (const child of [...current.children]) {
			if (!desiredSet.has(child)) child.remove();
		}
		let insertionPoint = current.firstElementChild;
		for (const link of desired) {
			if (link === insertionPoint) {
				insertionPoint = insertionPoint.nextElementSibling;
				continue;
			}
			if (link.parentNode === current && typeof current.moveBefore === "function") current.moveBefore(link, insertionPoint);
			else current.insertBefore(link, insertionPoint);
		}
	};

	const reconcilePostsFragment = (fragment, environment) => {
		const focusedElement = (
			document.activeElement
			&& (environment.collection.contains(document.activeElement) || environment.pagination.contains(document.activeElement))
			&& typeof document.activeElement.focus === "function"
		) ? document.activeElement : null;
		const currentEntries = [...environment.collection.children];
		const currentRecords = currentEntries
			.map((entry) => ({ entry, card: cardFromListingEntry(entry, environment.renderKind) }))
			.filter(({ card }) => Boolean(card));
		const currentCards = currentRecords.map(({ card }) => card);
		const reconciliationPlan = keyedReconciliationPlan(
			currentCards.map((card) => card.dataset.postId),
			fragment.entries.map(({ postId }) => postId),
		);
		const currentById = new Map(currentRecords.map((record) => [record.card.dataset.postId, record]));
		const groupOpenByContentKey = new Map();
		for (const card of currentCards) {
			const group = card.querySelector(":scope > [data-content-group]");
			if (group && card.dataset.contentKey) groupOpenByContentKey.set(card.dataset.contentKey, group.open);
		}
		const anchor = captureListingAnchor(currentCards);
		const newCards = [];
		const desiredEntries = fragment.entries.map(({ entry, card: freshCard, postId }, index) => {
			const currentRecord = reconciliationPlan.ordered[index].action === "reuse" ? currentById.get(postId) : null;
			if (currentRecord) {
				patchSurvivingCard(currentRecord.card, freshCard);
				setCardHidden(currentRecord.card, false);
				if (environment.renderKind === "search") {
					patchTextElement(currentRecord.entry, entry, ":scope > .search-result-context");
				}
				return currentRecord.entry;
			}
			const group = freshCard.querySelector(":scope > [data-content-group]");
			if (group && freshCard.dataset.contentKey && groupOpenByContentKey.has(freshCard.dataset.contentKey)) group.open = groupOpenByContentKey.get(freshCard.dataset.contentKey);
			newCards.push(freshCard);
			return entry;
		});

		const desiredSet = new Set(desiredEntries);
		for (const child of [...environment.collection.children]) {
			if (desiredSet.has(child)) continue;
			destroyCardMedia(cardFromListingEntry(child, environment.renderKind));
			child.remove();
		}
		let insertionPoint = environment.collection.firstElementChild;
		for (const desired of desiredEntries) {
			if (desired === insertionPoint) {
				insertionPoint = insertionPoint.nextElementSibling;
				continue;
			}
			if (desired.parentNode === environment.collection && typeof environment.collection.moveBefore === "function") {
				environment.collection.moveBefore(desired, insertionPoint);
			} else {
				environment.collection.insertBefore(desired, insertionPoint);
			}
		}
		patchPagination(environment.pagination, fragment.pagination);
		environment.statusMessage.textContent = fragment.statusMessage.textContent;
		environment.statusMessage.hidden = fragment.statusMessage.hidden;
		environment.collection.dataset.listingStatus = fragment.status;
		environment.collection.dataset.visibleCount = String(fragment.visibleCount);
		if (focusedElement?.isConnected && document.activeElement !== focusedElement) focusedElement.focus({ preventScroll: true });

		if (anchor) {
			const fallback = [...environment.collection.querySelectorAll(":scope > article.post[data-post-id], :scope > .search-result-entry > article.post[data-post-id]")]
				.filter((card) => !card.hidden)[anchor.visibleIndex];
			const settledAnchor = anchor.card.isConnected && !anchor.card.hidden ? anchor.card : fallback;
			if (settledAnchor) window.scrollBy(0, settledAnchor.getBoundingClientRect().top - anchor.top);
		}
		if (activeCard && !activeCard.isConnected) activeCard = null;
		for (const card of newCards) {
			try {
				card.querySelectorAll("[data-inline-toggle]").forEach((button) => syncInlineToggle(button, button.getAttribute("aria-expanded") === "true"));
				card.querySelectorAll('video[data-vale-media][data-media-deferred="false"]').forEach((video) => window.ValeMedia?.initialize(video));
			} catch (error) {
				console.warn("Vale committed a listing card but could not finish its optional media enhancement.", error);
			}
		}
		scheduleNavigationStateWrite();
		return { status: fragment.status, visibleCount: fragment.visibleCount };
	};

	const fetchPostsFragment = async (reason = "refresh") => {
		const environment = listingEnvironment();
		if (!environment) return null;
		if (listingFragmentPromise) {
			listingRefreshQueued = true;
			listingRefreshReason = reason;
			return listingFragmentPromise;
		}

		const requestEpoch = hiddenMutationEpoch;
		const controller = new AbortController();
		listingFragmentController = controller;
		const startedAt = performance.now();
		let timedOut = false;
		const timeout = window.setTimeout(() => {
			timedOut = true;
			controller.abort();
		}, POSTS_FRAGMENT_TIMEOUT);
		listingFragmentPromise = (async () => {
			try {
				const response = await fetch(window.location.href, {
					method: "GET",
					credentials: "same-origin",
					cache: "no-store",
					headers: { Accept: "text/html", "X-Vale-Fragment": POSTS_FRAGMENT_VERSION },
					signal: controller.signal,
				});
				if (
					!response.ok
					|| response.redirected
					|| response.headers.get("X-Vale-Fragment") !== POSTS_FRAGMENT_VERSION
					|| !/^text\/html(?:;|$)/i.test(response.headers.get("content-type") || "")
				) {
					throw new Error(`The listing fragment response was rejected (${response.status}).`);
				}
				const html = await readBoundedFragment(response, controller.signal);
				if (performance.now() - startedAt > POSTS_FRAGMENT_TIMEOUT) throw new Error("The listing refresh exceeded its deadline.");
				if (requestEpoch !== hiddenMutationEpoch || postMutationIds.size) return { stale: true };
				const fragment = parsePostsFragment(html, environment.renderKind);
				if (performance.now() - startedAt > POSTS_FRAGMENT_TIMEOUT) throw new Error("The listing refresh exceeded its deadline.");
				if (requestEpoch !== hiddenMutationEpoch || postMutationIds.size || !environment.collection.isConnected) return { stale: true };
				const result = reconcilePostsFragment(fragment, environment);
				if (result.status === "retry") {
					showToast("Vale could not finish replenishing this listing. The visible posts are current.", "Retry", () => queueListingRefresh("retry"), 9000);
				} else if (result.status === "end" && reason === "mutation") {
					showUndoToast("Post hidden. You’ve reached the end of this listing.");
				} else if (result.status === "complete" && reason === "mutation") {
					showUndoToast("Post hidden. Replacement loaded.");
				}
				return result;
			} catch (error) {
				const stale = requestEpoch !== hiddenMutationEpoch || (controller.signal.aborted && !timedOut);
				if (stale) return { stale: true };
				controller.abort();
				console.warn("Vale could not refresh the current listing.", error);
				showToast(timedOut ? "The listing refresh timed out. The current posts were left unchanged." : "Couldn’t refresh this listing. The current posts were left unchanged.", "Retry", () => queueListingRefresh("retry"), 9000);
				return { error };
			} finally {
				window.clearTimeout(timeout);
			}
		})();

		try {
			return await listingFragmentPromise;
		} finally {
			listingFragmentController = null;
			listingFragmentPromise = null;
			if (listingRefreshQueued) queueMicrotask(runQueuedListingRefresh);
		}
	};

	const waitForListingFragmentIdle = async () => {
		while (listingFragmentPromise) {
			await listingFragmentPromise;
			await Promise.resolve();
		}
	};

	let profileChannel = null;
	try {
		profileChannel = new BroadcastChannel("vale-profile-state-v1");
	} catch (_) {
		profileChannel = null;
	}

	const updatePostPageButton = (postId, hidden) => {
		document.querySelectorAll(`[data-post-page-hide="${postId}"]`).forEach((button) => {
			button.setAttribute("aria-pressed", String(hidden));
			const form = button.closest("form[data-post-page-hide-form]");
			if (form) form.action = `/posts/${encodeURIComponent(postId)}/${hidden ? "unhide" : "hide"}`;
			const label = button.querySelector("[data-post-page-hide-label]");
			const status = button.querySelector("[data-post-page-hide-status]");
			if (label) label.textContent = hidden ? "Hidden" : "Hide post";
			if (status) status.textContent = hidden ? "Press to restore this post to feeds" : "Press to remove this post from feeds";
		});
	};

	const setupReadingJump = () => {
		const control = document.querySelector("[data-reading-jump]");
		const comments = document.getElementById("comments");
		const post = document.getElementById("post-top");
		if (!control || !comments || !post) return;

		const label = control.querySelector("[data-reading-jump-label]");
		const icon = control.querySelector("[data-reading-jump-icon]");
		let frame = 0;
		const update = () => {
			frame = 0;
			const headerHeight = document.querySelector(".app-header")?.getBoundingClientRect().height || 0;
			const toolsHeight = window.matchMedia("(max-width: 1120px)").matches
				? document.querySelector(".reading-tools")?.getBoundingClientRect().height || 0
				: 0;
			const inComments = comments.getBoundingClientRect().top <= headerHeight + toolsHeight + 32;
			const nextLabel = inComments ? "Jump to post" : "Jump to comments";
			control.setAttribute("href", inComments ? "#post-top" : "#comments");
			control.setAttribute("aria-label", nextLabel);
			if (label) label.textContent = nextLabel;
			if (icon) icon.textContent = inComments ? "↑" : "↓";
		};
		const schedule = () => {
			if (!frame) frame = requestAnimationFrame(update);
		};
		control.addEventListener("click", (event) => {
			event.preventDefault();
			const target = control.getAttribute("href") === "#post-top" ? post : comments;
			history.pushState(null, "", `#${target.id}`);
			scrollElementWithInset(target, { block: "start" });
			target.focus({ preventScroll: true });
			schedule();
		});
		window.addEventListener("scroll", schedule, { passive: true });
		window.addEventListener("resize", schedule);
		window.addEventListener("pageshow", schedule);
		update();
	};

	const applyConfirmedHiddenState = (postId, hidden, { restoreExisting = false } = {}) => {
		document.querySelectorAll(".post[data-post-id]").forEach((card) => {
			if (card.dataset.postId !== postId) return;
			if (hidden) {
				setCardHidden(card, true);
				if (activeCard === card) {
					activeCard.classList.remove("is-keyboard-active");
					activeCard.removeAttribute("aria-current");
					activeCard = null;
				}
			} else if (restoreExisting) {
				setCardHidden(card, false);
			}
		});
		updatePostPageButton(postId, hidden);
	};

	const broadcastHiddenState = (postId, hidden) => {
		if (!profileChannel) return;
		broadcastSequence += 1;
		profileChannel.postMessage({ type: "hidden-post", source: profileSourceTab, sequence: broadcastSequence, postId, hidden });
	};

	const verifyHiddenStates = async (postIds) => {
		const requested = [...new Set(postIds)].filter(validPostId).slice(0, 250);
		if (!requested.length) return new Set();
		const controller = new AbortController();
		const timeout = window.setTimeout(() => controller.abort(), 10_000);
		try {
			const url = new URL("/hidden/state", window.location.origin);
			url.searchParams.set("ids", requested.join(","));
			const response = await fetch(url, {
				credentials: "same-origin",
				cache: "no-store",
				headers: { Accept: "application/json" },
				signal: controller.signal,
			});
			if (!response.ok || response.redirected || !/^application\/json(?:;|$)/i.test(response.headers.get("content-type") || "")) {
				throw new Error(`Hidden-state verification failed with ${response.status}.`);
			}
			const payload = await response.json();
			if (!Array.isArray(payload) || payload.some((id) => !requested.includes(id)) || new Set(payload).size !== payload.length) {
				throw new Error("Hidden-state verification returned an invalid payload.");
			}
			return new Set(payload);
		} finally {
			window.clearTimeout(timeout);
		}
	};

	const enhancedHiddenWrite = async (postId, hidden, signal) => {
		const response = await fetch(`/posts/${encodeURIComponent(postId)}/${hidden ? "hide" : "unhide"}`, {
			method: "POST",
			credentials: "same-origin",
			cache: "no-store",
			headers: { Accept: "text/plain", "X-Vale-Enhanced": "hide-v1" },
			signal,
		});
		if (response.status !== 204 || response.redirected) throw new Error(`The hidden-post write returned ${response.status}.`);
	};

	const resolveHiddenWaiters = (state, result) => {
		for (const waiter of state.waiters.splice(0)) {
			waiter.resolve(mutationWaiterOutcome(waiter, state, result));
		}
	};

	const resolveMutationDrain = () => {
		if (hiddenMutationWorker || hiddenMutationQueue.length) return;
		for (const resolve of hiddenMutationDrainWaiters.splice(0)) resolve();
		if (!bfcacheReconcileInProgress) runQueuedListingRefresh();
	};

	const queueHiddenMutationId = (postId, state) => {
		if (state.inFlight) return;
		hiddenMutationQueue.enqueue(postId);
	};

	const processHiddenMutations = async () => {
		while (hiddenMutationQueue.length) {
			const postId = hiddenMutationQueue.shift();
			const state = hiddenMutationStates.get(postId);
			if (!state || state.inFlight) continue;
			if (!hiddenIntentNeedsWrite(state.confirmed, state.desired, state.uncertain)) {
				const removedPendingShell = removedPendingCardIds.delete(postId);
				postMutationIds.delete(postId);
				applyConfirmedHiddenState(postId, state.confirmed, { restoreExisting: !state.confirmed });
				resolveHiddenWaiters(state, { ok: true, hidden: state.confirmed, collapsed: true });
				hiddenMutationStates.delete(postId);
				if (removedPendingShell && listingEnvironment()) queueListingRefresh("recovery");
				continue;
			}

			const sentHidden = state.desired;
			const sentEpoch = state.epoch;
			const wasUncertain = state.uncertain;
			state.inFlight = true;
			state.uncertain = false;
			const controller = new AbortController();
			state.controller = controller;
			let writeTimedOut = false;
			const writeTimeout = window.setTimeout(() => {
				writeTimedOut = true;
				controller.abort();
			}, HIDDEN_MUTATION_TIMEOUT);
			let writeConfirmed = false;
			let verified = false;
			let actualHidden = state.confirmed;
			try {
				try {
					await enhancedHiddenWrite(postId, sentHidden, controller.signal);
					writeConfirmed = true;
					verified = true;
					actualHidden = sentHidden;
				} finally {
					window.clearTimeout(writeTimeout);
				}
			} catch (writeError) {
				console.warn(writeTimedOut ? "Vale’s hidden-post write timed out; checking authoritative state." : "Vale could not confirm a hidden-post write; checking authoritative state.", writeError);
				try {
					actualHidden = (await verifyHiddenStates([postId])).has(postId);
					verified = true;
				} catch (verifyError) {
					console.warn("Vale could not verify the hidden-post write.", verifyError);
				}
			}
			state.inFlight = false;
			state.controller = null;

			if (!verified) {
				state.uncertain = true;
				postMutationIds.add(postId);
				applyConfirmedHiddenState(postId, true);
				resolveHiddenWaiters(state, { ok: false, hidden: true, uncertain: true });
				showToast("Vale couldn’t verify whether that change was saved. The post remains hidden for safety.", "Retry", () => retryUncertainMutation(postId), 10_000);
				continue;
			}

			const changed = state.confirmed !== actualHidden;
			const removedPendingShell = removedPendingCardIds.delete(postId);
			state.confirmed = actualHidden;
			applyConfirmedHiddenState(postId, actualHidden, { restoreExisting: !actualHidden && !listingEnvironment() });
			broadcastHiddenState(postId, actualHidden);
			if (mutationNeedsListingRecovery({ changed, wasUncertain, actualHidden, removedPendingShell, hasListing: Boolean(listingEnvironment()) })) {
				queueListingRefresh(changed && actualHidden ? "mutation" : "recovery");
			}

			if (state.epoch !== sentEpoch && state.desired !== actualHidden) {
				queueHiddenMutationId(postId, state);
				continue;
			}
			if (!writeConfirmed && state.desired === sentHidden && actualHidden !== sentHidden) state.desired = actualHidden;
			if (state.desired !== actualHidden) {
				queueHiddenMutationId(postId, state);
				continue;
			}
			postMutationIds.delete(postId);
			resolveHiddenWaiters(state, { ok: writeConfirmed || actualHidden === sentHidden, hidden: actualHidden, verified: true });
			hiddenMutationStates.delete(postId);
		}
	};

	const startHiddenMutationWorker = () => {
		if (hiddenMutationWorker) return;
		hiddenMutationWorker = processHiddenMutations()
			.catch((error) => console.error("Vale’s hidden-post queue stopped unexpectedly.", error))
			.finally(() => {
				hiddenMutationWorker = null;
				resolveMutationDrain();
				if (hiddenMutationQueue.length) startHiddenMutationWorker();
			});
	};

	const requestHiddenState = (postId, hidden, knownState = null) => {
		if (!validPostId(postId)) return Promise.resolve({ ok: false, hidden: true, invalid: true });
		invalidateBufferedForeignState(pendingForeignHiddenStates, postId);
		let state = hiddenMutationStates.get(postId);
		if (!state) {
			state = {
				confirmed: typeof knownState === "boolean" ? knownState : !hidden,
				desired: hidden,
				epoch: 0,
				inFlight: false,
				uncertain: false,
				controller: null,
				waiters: [],
			};
			hiddenMutationStates.set(postId, state);
		}
		state.desired = hidden;
		state.epoch = ++hiddenMutationEpoch;
		postMutationIds.add(postId);
		listingFragmentController?.abort();
		const promise = new Promise((resolve) => state.waiters.push({ resolve, hidden, epoch: state.epoch }));
		queueHiddenMutationId(postId, state);
		startHiddenMutationWorker();
		return promise;
	};

	function retryUncertainMutation(postId) {
		const state = hiddenMutationStates.get(postId);
		if (!state?.uncertain) return;
		requestHiddenState(postId, state.desired, state.confirmed).then((result) => {
			if (!result.ok && !result.uncertain) showToast("That change still could not be saved. Reload before relying on it.", "", null, 7000);
		});
	}

	const waitForHiddenMutationDrain = () => {
		if (!hiddenMutationWorker && hiddenMutationQueue.length === 0) return Promise.resolve();
		return new Promise((resolve) => hiddenMutationDrainWaiters.push(resolve));
	};

	function queueListingRefresh(reason = "refresh", delay = 0) {
		if (!listingEnvironment()) return;
		listingRefreshQueued = true;
		listingRefreshReason = reason;
		window.clearTimeout(listingRefreshTimer);
		listingRefreshTimer = window.setTimeout(runQueuedListingRefresh, delay);
	}

	function runQueuedListingRefresh(reason = null) {
		if (reason) listingRefreshReason = reason;
		if (!queuedListingRefreshCanStart({
			queued: listingRefreshQueued,
			pendingMutations: postMutationIds.size,
			workerActive: Boolean(hiddenMutationWorker),
			mutationQueueLength: hiddenMutationQueue.length,
			fragmentActive: Boolean(listingFragmentPromise),
			bfcacheActive: bfcacheReconcileInProgress,
		})) return null;
		const nextReason = listingRefreshReason;
		listingRefreshQueued = false;
		listingRefreshReason = "refresh";
		return fetchPostsFragment(nextReason);
	}

	const refreshListingAfterMutations = async (reason) => {
		await waitForHiddenMutationDrain();
		if (!listingEnvironment()) return null;
		listingRefreshQueued = true;
		listingRefreshReason = reason;
		let result = null;
		while (listingEnvironment() && (listingRefreshQueued || listingFragmentPromise)) {
			const pending = listingFragmentPromise || runQueuedListingRefresh();
			if (!pending) break;
			result = await pending;
			await Promise.resolve();
		}
		return result;
	};

	if (profileChannel) {
		profileChannel.addEventListener("message", (event) => {
			const message = event.data;
			if (
				!message
				|| message.type !== "hidden-post"
				|| message.source === profileSourceTab
				|| typeof message.source !== "string"
				|| !Number.isSafeInteger(message.sequence)
				|| !validPostId(message.postId)
				|| typeof message.hidden !== "boolean"
			) return;
			if (!acceptBroadcastSequence(foreignProfileSequences, message.source, message.sequence)) return;
			if (postMutationIds.has(message.postId)) return;
			pendingForeignHiddenStates.set(message.postId, message.hidden);
			window.clearTimeout(listingRefreshTimer);
			listingRefreshTimer = window.setTimeout(() => {
				let changed = false;
				for (const [postId, hidden] of pendingForeignHiddenStates) {
					pendingForeignHiddenStates.delete(postId);
					if (postMutationIds.has(postId)) continue;
					const state = hiddenMutationStates.get(postId);
					if (state) {
						state.confirmed = hidden;
						state.desired = hidden;
					}
					applyConfirmedHiddenState(postId, hidden, { restoreExisting: !hidden && !listingEnvironment() });
					changed = true;
				}
				if (changed) {
					hiddenMutationEpoch += 1;
					listingFragmentController?.abort();
					queueListingRefresh("broadcast");
				}
			}, 80);
		});
	}

	const neighboringCard = (card) => {
		const cards = feedCards();
		const index = cards.indexOf(card);
		return cards[index + 1] || cards[index - 1] || null;
	};

	const removeHiddenCardShell = (postId) => {
		document.querySelectorAll(".post[data-post-id]").forEach((card) => {
			if (card.dataset.postId !== postId) return;
			const plan = hiddenShellEvictionPlan(card.hidden, postMutationIds.has(postId));
			if (!plan.remove) return;
			if (plan.trackPending) removedPendingCardIds.add(postId);
			destroyCardMedia(card);
			card.closest('.search-result-entry[data-vale-search-result="1"]')?.remove();
			if (card.isConnected) card.remove();
		});
	};

	const pruneUndoStack = () => {
		const now = Date.now();
		for (let index = undoStack.length - 1; index >= 0; index -= 1) {
			const entry = undoStack[index];
			if (entry.expiresAt > now) continue;
			removeHiddenCardShell(entry.postId);
			undoStack.splice(index, 1);
		}
		while (undoStack.length > UNDO_LIMIT) {
			const entry = undoStack.shift();
			if (entry) removeHiddenCardShell(entry.postId);
		}
	};

	const pushUndo = (entry) => {
		const bounded = appendBoundedUndo(
			undoStack,
			{ postId: entry.postId, title: entry.title || "post", expiresAt: Date.now() + UNDO_LIFETIME },
			UNDO_LIMIT,
		);
		undoStack.splice(0, undoStack.length, ...bounded.entries);
		bounded.evicted.forEach(({ postId }) => removeHiddenCardShell(postId));
		pruneUndoStack();
		setTimeout(pruneUndoStack, UNDO_LIFETIME + 100);
	};

	const restoreCardWithoutJump = (card) => {
		const anchor = feedCards().find((candidate) => candidate.getBoundingClientRect().bottom > 0);
		const before = anchor?.getBoundingClientRect().top;
		setCardHidden(card, false);
		if (anchor && Number.isFinite(before)) window.scrollBy(0, anchor.getBoundingClientRect().top - before);
	};

	const showUndoToast = (message = "Post hidden.") => {
		pruneUndoStack();
		const count = undoStack.length;
		showToast(message, count ? `Undo${count > 1 ? ` (${count})` : ""}` : "", count ? undoLatest : null, 12_000);
	};

	async function undoLatest() {
		pruneUndoStack();
		const entry = undoStack.pop();
		if (!entry) return;
		showToast("Restoring the post…", "", null, 12_000);
		const result = await requestHiddenState(entry.postId, false, true);
		if (result.ok && result.hidden === false) {
			const snapshot = await refreshListingAfterMutations("undo");
			updatePostPageButton(entry.postId, false);
			if (snapshot?.error || snapshot?.stale) {
				showToast("Post restored, but the listing could not refresh. Its current cards were left unchanged.", "Retry", () => queueListingRefresh("retry"), 9000);
				return;
			}
			if (snapshot?.status === "retry") {
				showToast("Post restored. Vale could not finish replenishing the listing.", "Retry", () => queueListingRefresh("retry"), 9000);
				return;
			}
			showUndoToast(undoStack.length ? "Post restored. Undo another?" : "Post restored.");
			return;
		}
		pushUndo(entry);
		showToast(result.uncertain ? "Vale couldn’t verify the restore, so the post remains hidden." : "Couldn’t restore that post.", "Retry", undoLatest, 9000);
	}

	const hideCard = async (card, _keyboard = false, sourceButton = null) => {
		const postId = card?.dataset.postId;
		const environment = listingEnvironment();
		if (!validPostId(postId) || card.hidden || postMutationIds.has(postId)) return;
		if (!environment?.collection.contains(card)) {
			const form = sourceButton?.form || card.querySelector("form[data-hide-form]");
			if (sourceButton?.form === form) form.requestSubmit(sourceButton);
			else form?.requestSubmit();
			return;
		}
		const nextCard = neighboringCard(card);
		const shouldAdvanceFocus = activeCard === card || document.activeElement === sourceButton || card.contains(document.activeElement);
		pushUndo({ postId, title: card.dataset.postTitle });
		setCardHidden(card, true);
		if (shouldAdvanceFocus) setActiveCard(nextCard, Boolean(nextCard));
		showUndoToast();
		const result = await requestHiddenState(postId, true, false);
		if (result.superseded) return;
		if (!result.ok && !result.uncertain && result.hidden === false) {
			const index = undoStack.findIndex((entry) => entry.postId === postId);
			if (index >= 0) undoStack.splice(index, 1);
			restoreCardWithoutJump(card);
			showToast("Couldn’t hide that post. Its unchanged state was verified.", "", null, 6500);
		}
	};

	const togglePostPageHidden = async (button) => {
		const postId = button.dataset.postPageHide;
		if (!validPostId(postId)) return;
		const wasHidden = button.getAttribute("aria-pressed") === "true";
		const hidden = !wasHidden;
		updatePostPageButton(postId, hidden);
		if (hidden) {
			pushUndo({ postId, title: "post" });
			showUndoToast();
		} else {
			const index = undoStack.map((entry) => entry.postId).lastIndexOf(postId);
			if (index >= 0) undoStack.splice(index, 1);
		}
		const result = await requestHiddenState(postId, hidden, wasHidden);
		if (result.superseded) return;
		if (result.ok) {
			updatePostPageButton(postId, result.hidden);
			if (!result.hidden) showToast("Post restored.", "", null, 3500);
			return;
		}
		if (!result.uncertain) updatePostPageButton(postId, result.hidden);
	};

	const reconcileHiddenState = async ({ refreshListing = false } = {}) => {
		const domIds = [];
		document.querySelectorAll(".post[data-post-id]").forEach((card) => domIds.push(card.dataset.postId));
		document.querySelectorAll("[data-post-page-hide]").forEach((button) => domIds.push(button.dataset.postPageHide));
		const requested = boundedHiddenVerificationIds([domIds, [...postMutationIds], [...removedPendingCardIds]]);
		const eligibleListing = refreshListing && listingEnvironment();
		if (!requested.length && !eligibleListing) return false;
		const verificationEpoch = hiddenMutationEpoch;
		const pendingSnapshots = new Map();
		for (const postId of requested) {
			const state = hiddenMutationStates.get(postId);
			if (!state || !postMutationIds.has(postId)) continue;
			pendingSnapshots.set(postId, {
				state,
				uncertain: state.uncertain,
				queued: hiddenMutationQueue.has(postId),
				inFlight: state.inFlight,
				epoch: state.epoch,
			});
		}
		try {
			const hidden = requested.length ? await verifyHiddenStates(requested) : new Set();
			const verificationStayedCurrent = hiddenVerificationCanApply(verificationEpoch, hiddenMutationEpoch);
			for (const postId of requested) {
				const isHidden = hidden.has(postId);
				const state = hiddenMutationStates.get(postId);
				if (postMutationIds.has(postId)) {
					const snapshot = pendingSnapshots.get(postId);
					if (
						!snapshot
						|| state !== snapshot.state
						|| !quiescentUncertainStateCanRecover({
							snapshotUncertain: snapshot.uncertain,
							snapshotQueued: snapshot.queued,
							snapshotInFlight: snapshot.inFlight,
							snapshotEpoch: snapshot.epoch,
							currentUncertain: state?.uncertain,
							currentQueued: hiddenMutationQueue.has(postId),
							currentInFlight: state?.inFlight,
							currentEpoch: state?.epoch,
						})
					) continue;

					const changed = state.confirmed !== isHidden;
					state.confirmed = isHidden;
					state.desired = isHidden;
					state.uncertain = false;
					postMutationIds.delete(postId);
					removedPendingCardIds.delete(postId);
					hiddenMutationStates.delete(postId);
					applyConfirmedHiddenState(postId, isHidden, { restoreExisting: !isHidden && !listingEnvironment() });
					if (changed) broadcastHiddenState(postId, isHidden);
					continue;
				}
				if (!verificationStayedCurrent) continue;
				if (state) {
					state.confirmed = isHidden;
					state.desired = isHidden;
				}
				applyConfirmedHiddenState(postId, isHidden, { restoreExisting: !isHidden && !listingEnvironment() });
			}
			if (eligibleListing) {
				hiddenMutationEpoch += 1;
				listingFragmentController?.abort();
				await waitForListingFragmentIdle();
				if (postMutationIds.size || hiddenMutationWorker || hiddenMutationQueue.length) {
					listingRefreshQueued = true;
					if (listingRefreshReason === "refresh") listingRefreshReason = "bfcache";
				} else {
					listingRefreshQueued = false;
					listingRefreshReason = "bfcache";
					await fetchPostsFragment("bfcache");
				}
			}
			return true;
		} catch (error) {
			console.warn("Vale could not reconcile hidden posts after page restore.", error);
			showToast("Vale couldn’t verify hidden posts after restoring this page. Reload to confirm the latest state.", "", null, 8000);
			return false;
		}
	};

	const normalizedKey = (event) => {
		if (event.ctrlKey || event.metaKey || event.altKey) return "";
		let key = event.key;
		if (key === " ") key = "Space";
		if (["Shift", "Control", "Alt", "Meta", "Dead", "Unidentified"].includes(key)) return "";
		if (key.length === 1 && !event.shiftKey) key = key.toLowerCase();
		return event.shiftKey ? `Shift+${key}` : key;
	};

	const shortcutMatches = (configured, event) => configured && configured.toLowerCase() === normalizedKey(event).toLowerCase();
	const editableTarget = (target) => target.closest('input, textarea, select, [contenteditable="true"]');
	const interactiveTarget = (target) => target.closest('input, textarea, select, button, a, summary, video, [contenteditable="true"]');

	const visibleSearch = () => [...document.querySelectorAll('input[type="search"], form[action="/search"] input[name="q"], .context-search input')].find((input) => {
		const box = input.getBoundingClientRect();
		const style = getComputedStyle(input);
		return box.width > 0 && box.height > 0 && style.visibility !== "hidden" && style.display !== "none";
	});

	const navigatePost = (direction) => {
		const cards = feedCards();
		if (!cards.length) return;
		if (!activeCard || !cards.includes(activeCard)) {
			setActiveCard(direction > 0 ? cards[0] : cards[cards.length - 1], true);
			return;
		}
		const index = Math.max(0, Math.min(cards.length - 1, cards.indexOf(activeCard) + direction));
		setActiveCard(cards[index], true);
	};

	const captureShortcut = (event, input) => {
		if (event.key === "Tab") return;
		if (event.key === "Escape") {
			event.preventDefault();
			input.blur();
			return;
		}
		event.preventDefault();
		const status = document.querySelector("[data-shortcut-status]");
		if (event.ctrlKey || event.metaKey || event.altKey) {
			if (status) status.textContent = "Use a single key or a Shift combination; browser command keys stay reserved.";
			return;
		}
		const key = normalizedKey(event);
		if (!key) return;
		input.value = key;
		if (status) status.textContent = `${input.getAttribute("aria-label")} is now ${key}. Save settings to apply it.`;
		refreshSettingsSaveBar();
	};

	document.addEventListener("click", (event) => {
		if (event.target.closest("[data-thread-parent-link]")) writeNavigationState();

		const offlineSaveButton = event.target.closest("[data-offline-save-submit]");
		if (offlineSaveButton) {
			event.preventDefault();
			submitOfflineSave(offlineSaveButton.form);
			return;
		}

		const previousCommentMatch = event.target.closest("[data-comment-search-previous]");
		if (previousCommentMatch) {
			event.preventDefault();
			const index = commentSearchState.matches.findIndex((comment) => comment.dataset.threadNodeId === commentSearchState.currentId);
			activateCommentSearchMatch(index - 1);
			return;
		}

		const nextCommentMatch = event.target.closest("[data-comment-search-next]");
		if (nextCommentMatch) {
			event.preventDefault();
			const index = commentSearchState.matches.findIndex((comment) => comment.dataset.threadNodeId === commentSearchState.currentId);
			activateCommentSearchMatch(index + 1);
			return;
		}

		const loadCommentSearchBranches = event.target.closest("[data-comment-search-load]");
		if (loadCommentSearchBranches) {
			event.preventDefault();
			searchRemainingCommentBranches(loadCommentSearchBranches);
			return;
		}

		const hideButton = event.target.closest("[data-hide-post]");
		if (hideButton) {
			event.preventDefault();
			const card = hideButton.closest('.post:not(.highlighted)[data-post-permalink]');
			if (card) hideCard(card, event.detail === 0, hideButton);
			return;
		}

		const postPageHide = event.target.closest("[data-post-page-hide]");
		if (postPageHide) {
			event.preventDefault();
			togglePostPageHidden(postPageHide);
			return;
		}

		const clickedCard = event.target.closest('.post:not(.highlighted)[data-post-permalink]');
		if (clickedCard && !clickedCard.hidden) setActiveCard(clickedCard, false, true);

		const shortcutReset = event.target.closest("[data-shortcut-reset]");
		if (shortcutReset) {
			event.preventDefault();
			document.querySelectorAll(".shortcut-capture[data-default-key]").forEach((input) => {
				input.value = input.dataset.defaultKey;
			});
			const status = document.querySelector("[data-shortcut-status]");
			if (status) status.textContent = "Default shortcuts restored. Save settings to apply them.";
			refreshSettingsSaveBar();
			requestAnimationFrame(refreshSettingsSaveBar);
			return;
		}

		const commentCollapse = event.target.closest("[data-comment-collapse]");
		if (commentCollapse) {
			event.preventDefault();
			setCommentState(commentCollapse, commentCollapse.getAttribute("aria-expanded") !== "true");
			return;
		}

		const repliesToggle = event.target.closest("[data-replies-toggle]");
		if (repliesToggle) {
			event.preventDefault();
			setRepliesState(repliesToggle, repliesToggle.getAttribute("aria-expanded") !== "true");
			return;
		}

		const filterToggle = event.target.closest("[data-comment-filter-toggle]");
		if (filterToggle) {
			event.preventDefault();
			const showAll = !document.body.classList.contains("comments-show-filtered");
			syncKeywordFilter(showAll, !showAll);
			return;
		}

		const commentReveal = event.target.closest("[data-comment-reveal]");
		if (commentReveal) {
			event.preventDefault();
			const comment = commentReveal.closest('.comment[data-keyword-filtered="true"]');
			if (!comment) return;
			comment.classList.toggle("is-keyword-revealed");
			syncKeywordFilter(false);
			return;
		}

		const inlineButton = event.target.closest("[data-inline-toggle]");
		if (inlineButton) {
			const panel = document.getElementById(inlineButton.dataset.inlineToggle);
			if (!panel) return;
			event.preventDefault();
			const expanding = panel.hidden;
			setInlineState(panel, expanding, inlineButton);
			return;
		}

		const repliesButton = event.target.closest("button.deeper_replies[data-comments-url]");
		if (repliesButton) {
			event.preventDefault();
			loadMoreReplies(repliesButton);
			return;
		}
	});

	document.addEventListener("change", (event) => {
		const sortSelect = event.target.closest("[data-feed-sort]");
		if (sortSelect) {
			const root = sortSelect.dataset.sortRoot || "";
			captureBeforeNavigation();
			window.location.assign(`${root}/${encodeURIComponent(sortSelect.value)}`);
			return;
		}
		const commentSortSelect = event.target.closest("[data-comment-sort]");
		if (commentSortSelect) commentSortSelect.form?.requestSubmit();
	});
	document.querySelectorAll(".feed-switcher-tabs").forEach((scroller) => {
		scroller.addEventListener("focusin", (event) => {
			const link = event.target.closest(".feed-switcher-tab");
			if (link) link.scrollIntoView({ block: "nearest", inline: "nearest" });
		});
	});

	document.querySelectorAll('.header-search input[name="q"]').forEach((input) => {
		input.addEventListener("keydown", (event) => {
			if (event.key !== "Enter" || event.isComposing) return;
			event.preventDefault();
			const submit = input.form?.querySelector('button[type="submit"]');
			if (submit) submit.click();
			else input.form?.requestSubmit();
		});
	});
	document.getElementById("comment-search-input")?.addEventListener("keydown", (event) => {
		if (event.key !== "Enter" || event.isComposing) return;
		event.preventDefault();
		event.currentTarget.form?.requestSubmit();
	});

	document.addEventListener("keydown", (event) => {
		const shortcutInput = event.target.closest(".shortcut-capture");
		if (shortcutInput) {
			captureShortcut(event, shortcutInput);
			return;
		}

		if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === "k" && !editableTarget(event.target)) {
			const search = visibleSearch();
			if (search) {
				event.preventDefault();
				search.focus();
				search.select();
			}
			return;
		}

		const disclosure = event.target.closest("[data-comment-collapse], [data-replies-toggle]");
		if (disclosure && (event.key === "Enter" || event.key === " ")) {
			event.preventDefault();
			disclosure.click();
			return;
		}

		if (document.body.dataset.keyboardNavigation !== "on" || event.defaultPrevented || interactiveTarget(event.target)) return;
		if (shortcutMatches(document.body.dataset.keyNextPost, event)) {
			event.preventDefault();
			navigatePost(1);
		} else if (shortcutMatches(document.body.dataset.keyPreviousPost, event)) {
			event.preventDefault();
			navigatePost(-1);
		} else if (activeCard && shortcutMatches(document.body.dataset.keyOpenPost, event) && !event.repeat) {
			event.preventDefault();
			captureBeforeNavigation();
			window.location.assign(activeCard.dataset.postPermalink);
		} else if (activeCard && shortcutMatches(document.body.dataset.keyTogglePreview, event) && !event.repeat) {
			const inlineToggle = activeCard.querySelector(".post_inline_toggle");
			if (!inlineToggle) return;
			event.preventDefault();
			inlineToggle.click();
		} else if (activeCard && shortcutMatches(document.body.dataset.keyHidePost, event) && !event.repeat) {
			event.preventDefault();
			hideCard(activeCard, true);
		}
	});

	document.addEventListener("error", (event) => {
		const image = event.target;
		if (!(image instanceof HTMLImageElement)) return;
		if (image.matches("[data-community-icon]")) {
			image.hidden = true;
			image.parentElement?.querySelector("[data-community-icon-fallback]")?.removeAttribute("hidden");
			return;
		}
		if (!image.matches("[data-thumbnail-preview]")) return;
		image.hidden = true;
		image.parentElement?.querySelector("[data-thumbnail-placeholder]")?.removeAttribute("hidden");
	}, true);
	document.querySelectorAll("img[data-community-icon]").forEach((image) => {
		if (image.complete && image.naturalWidth === 0) {
			image.hidden = true;
			image.parentElement?.querySelector("[data-community-icon-fallback]")?.removeAttribute("hidden");
		}
	});

	document.querySelectorAll("[data-inline-toggle]").forEach((button) => syncInlineToggle(button, button.getAttribute("aria-expanded") === "true"));

	document.body.classList.add("supports-feed-sort-select");
	setupMobileFeedContext();
	setupSettingsSaveBar();
	try {
		history.scrollRestoration = threadProjection() || document.querySelector('.post:not(.highlighted)[data-post-id]') ? "manual" : "auto";
	} catch (_) {
		// Browsers without the History scroll-restoration API keep their native behavior.
	}
	syncKeywordFilter(false, false, false);
	syncThreadProjection();
	updateThreadSummary();
	syncCommentSearch({ revealCurrent: Boolean(commentSearchQuery()), focus: false, scroll: false, announce: false });
	setupReadingJump();
	try {
		buildThreadModel();
	} catch (error) {
		console.error("Vale could not initialize the normalized thread projection.", error);
		setThreadStatus("This comment thread could not be initialized safely. Reload before loading more replies.");
	}
	document.addEventListener("focusin", () => {
		captureFocus();
		scheduleNavigationStateWrite();
	});
	window.addEventListener("scroll", scheduleNavigationStateWrite, { passive: true });
	window.addEventListener("hashchange", () => {
		correctMobileHomeHashTarget();
		scheduleNavigationStateWrite();
	});
	document.addEventListener(
		"click",
		(event) => {
			if (event.button > 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
			const link = event.target.closest?.("a[href]");
			if (!link || link.target || link.hasAttribute("download")) return;
			const destination = new URL(link.href, window.location.href);
			if (destination.origin !== window.location.origin || `${destination.pathname}${destination.search}` === navigationRouteKey()) return;
			captureBeforeNavigation();
		},
		true,
	);
	document.addEventListener(
		"submit",
		(event) => {
			if (event.target.matches?.("form[data-hide-form]")) {
				const card = event.target.closest('.post:not(.highlighted)[data-post-id]');
				if (card && listingEnvironment()?.collection.contains(card)) {
					event.preventDefault();
					hideCard(card, false, event.submitter || event.target.querySelector("[data-hide-post]"));
				} else {
					captureBeforeNavigation(event);
				}
				return;
			}
			if (event.target.matches?.("form[data-post-page-hide-form]")) {
				event.preventDefault();
				const button = event.submitter || event.target.querySelector("[data-post-page-hide]");
				if (button) togglePostPageHidden(button);
				return;
			}
			if (event.target.matches?.("[data-offline-save]")) {
				event.preventDefault();
				submitOfflineSave(event.target);
				return;
			}
			captureBeforeNavigation(event);
		},
		true,
	);
	window.addEventListener("pagehide", () => {
		listingFragmentController?.abort();
		for (const state of hiddenMutationStates.values()) state.controller?.abort();
		pendingForeignHiddenStates.clear();
		listingRefreshQueued = false;
		listingRefreshReason = "refresh";
		window.clearTimeout(listingRefreshTimer);
		window.clearTimeout(navigationWriteTimer);
		writeNavigationState();
	});
	window.addEventListener("pageshow", (event) => {
		navigationLeaving = false;
		refreshMobileFeedContext?.();
		refreshSettingsSaveBar();
		if (event.persisted) {
			(async () => {
				bfcacheReconcileInProgress = true;
				try {
					await waitForListingFragmentIdle();
					await waitForHiddenMutationDrain();
					listingRefreshQueued = false;
					listingRefreshReason = "bfcache";
					await reconcileHiddenState({ refreshListing: true });
					scheduleNavigationStateWrite();
				} finally {
					bfcacheReconcileInProgress = false;
					runQueuedListingRefresh();
				}
			})().catch((error) => console.warn("Vale could not finish BFCache reconciliation.", error));
		}
	});
	restoreNavigationState().then(focusSavedReturnTarget).catch((error) => {
		navigationRestoreInProgress = false;
		console.warn("Vale could not restore this page’s saved navigation state.", error);
		setThreadStatus("This page returned without its saved reading position. The current thread remains usable.");
		writeNavigationState();
	});
})();
