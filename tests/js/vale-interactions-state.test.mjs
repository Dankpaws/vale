import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("../../static/vale-interactions.js", import.meta.url), "utf8");
const sandbox = { module: { exports: {} } };
vm.createContext(sandbox);
vm.runInContext(source, sandbox, { filename: "vale-interactions.js" });

const {
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
} = sandbox.module.exports;

test("the mutation queue is FIFO by post and collapses an unsent duplicate key", () => {
	const queue = new OrderedKeyQueue();
	assert.equal(queue.enqueue("post-a"), true);
	assert.equal(queue.enqueue("post-a"), false);
	assert.equal(queue.enqueue("post-b"), true);
	assert.equal(queue.length, 2);
	assert.equal(queue.has("post-a"), true);
	assert.equal(queue.shift(), "post-a");
	assert.equal(queue.has("post-a"), false);
	assert.equal(queue.shift(), "post-b");
	assert.equal(queue.length, 0);

	assert.equal(hiddenIntentNeedsWrite(false, false), false, "hide then unhide may collapse before a write");
	assert.equal(hiddenIntentNeedsWrite(false, true), true);
	assert.equal(hiddenIntentNeedsWrite(false, false, true), true, "an uncertain write must be verified/retried");
});

test("mutation waiters identify an older opposing intent as superseded", () => {
	const state = { confirmed: false, epoch: 3 };
	assert.equal(mutationWaiterOutcome({ hidden: true, epoch: 2 }, state, { ok: true }).superseded, true);
	assert.equal(mutationWaiterOutcome({ hidden: false, epoch: 3 }, state, { ok: true }).superseded, false);
	const verifiedOpposingResult = { ok: false, hidden: false, verified: true };
	assert.equal(mutationWaiterOutcome({ hidden: true, epoch: 2 }, state, verifiedOpposingResult).ok, false);
	assert.equal(mutationWaiterOutcome({ hidden: false, epoch: 3 }, state, verifiedOpposingResult).ok, true);
	assert.equal(mutationWaiterOutcome({ hidden: false, epoch: 3 }, state, { ok: false, hidden: true, uncertain: true }).ok, false);
});

test("keyed reconciliation reuses survivors, inserts replacements, and removes vacancies", () => {
	const plan = keyedReconciliationPlan(["a", "b", "c"], ["b", "d", "c"]);
	assert.equal(JSON.stringify(plan.ordered), JSON.stringify([
		{ id: "b", action: "reuse" },
		{ id: "d", action: "insert" },
		{ id: "c", action: "reuse" },
	]));
	assert.equal(JSON.stringify(plan.removed), JSON.stringify(["a"]));
	assert.throws(() => keyedReconciliationPlan(["a"], ["a", "a"]));
	assert.throws(() => keyedReconciliationPlan([], Array.from({ length: 26 }, (_, index) => `p${index}`)));
});

test("Undo shells are unique and bounded to the newest twelve", () => {
	let entries = [];
	let evicted = [];
	for (let index = 0; index < 13; index += 1) {
		({ entries, evicted } = appendBoundedUndo(entries, { postId: `p${index}` }, 12));
	}
	assert.equal(entries.length, 12);
	assert.equal(entries[0].postId, "p1");
	assert.equal(evicted[0].postId, "p0");
	({ entries } = appendBoundedUndo(entries, { postId: "p5", marker: "new" }, 12));
	assert.equal(entries.length, 12);
	assert.equal(entries.at(-1).postId, "p5");
	assert.equal(entries.at(-1).marker, "new");
});

test("cross-tab sequence filtering rejects stale and duplicate messages per source", () => {
	const sequences = new Map();
	assert.equal(acceptBroadcastSequence(sequences, "left", 1), true);
	assert.equal(acceptBroadcastSequence(sequences, "left", 1), false);
	assert.equal(acceptBroadcastSequence(sequences, "left", 0), false);
	assert.equal(acceptBroadcastSequence(sequences, "right", 1), true);
	assert.equal(acceptBroadcastSequence(sequences, "left", 2), true);
	const pending = new Map([["post-a", false], ["post-b", true]]);
	assert.equal(invalidateBufferedForeignState(pending, "post-a"), true, "a newer local intent invalidates an older debounced foreign state");
	assert.equal(pending.has("post-a"), false);
	assert.equal(pending.get("post-b"), true);
});

test("mobile feed context pins at the exact header boundary", () => {
	assert.equal(mobileFeedContextShouldPin(101, 100), false);
	assert.equal(mobileFeedContextShouldPin(100, 100), true);
	assert.equal(mobileFeedContextShouldPin(99, 100), true);
	assert.equal(mobileHomeTopInset(100, false, 48), 112);
	assert.equal(mobileHomeTopInset(100, true, 48), 160);
	assert.equal(mobileHomeTopInset(100, false, 48), 112, "restoration removes the context height without double-counting the gutter");
});

test("settings dirty state is exact and the save bar uses the strict native-save boundary", () => {
	assert.equal(serializedFormIsDirty("theme=dark&nsfw=off", "theme=dark&nsfw=off"), false);
	assert.equal(serializedFormIsDirty("theme=dark&nsfw=off", "theme=light&nsfw=off"), true);
	const baseline = "theme=dark&show_nsfw=off&show_nsfw=on&wide=off";
	assert.equal(serializedFormIsDirty(baseline, "theme=dark&show_nsfw=off&wide=off"), true, "hidden checkbox companions remain part of the exact baseline");
	assert.equal(serializedFormIsDirty(baseline, baseline), false, "reverting every control restores the clean state");

	const geometry = {
		mobile: true,
		dirty: true,
		formTop: 10,
		formBottom: 1200,
		viewportTop: 0,
		viewportBottom: 800,
		barHeight: 56,
	};
	assert.equal(settingsSaveBarShouldActivate({ ...geometry, saveTop: 733 }), true);
	assert.equal(settingsSaveBarShouldActivate({ ...geometry, saveTop: 732 }), false, "equality hides the enhancement bar");
	assert.equal(settingsSaveBarShouldActivate({ ...geometry, saveTop: 731 }), false);
	assert.equal(settingsSaveBarShouldActivate({ ...geometry, saveTop: 733, dirty: false }), false);
	assert.equal(settingsSaveBarShouldActivate({ ...geometry, saveTop: 733, mobile: false }), false);
	assert.equal(settingsSavedCleanTarget(true, "/settings", "?saved=1", "#preferences"), "/settings#preferences");
	assert.equal(settingsSavedCleanTarget(false, "/settings", "?saved=1", "#preferences"), "");
	assert.equal(settingsSavedCleanTarget(true, "/settings", "?saved=1", "#archive"), "", "unrelated return targets are never rewritten");
});

test("BFCache recovery consumes only an unchanged quiescent uncertain mutation", () => {
	const recoverable = {
		snapshotUncertain: true,
		snapshotQueued: false,
		snapshotInFlight: false,
		snapshotEpoch: 7,
		currentUncertain: true,
		currentQueued: false,
		currentInFlight: false,
		currentEpoch: 7,
	};
	assert.equal(quiescentUncertainStateCanRecover(recoverable), true);
	assert.equal(quiescentUncertainStateCanRecover({ ...recoverable, currentEpoch: 8 }), false);
	assert.equal(quiescentUncertainStateCanRecover({ ...recoverable, currentQueued: true }), false);
	assert.equal(quiescentUncertainStateCanRecover({ ...recoverable, currentInFlight: true }), false);
	assert.equal(quiescentUncertainStateCanRecover({ ...recoverable, currentUncertain: false }), false);
	assert.equal(hiddenVerificationCanApply(7, 7), true);
	assert.equal(hiddenVerificationCanApply(7, 8), false, "a local mutation racing the batch makes the older response inapplicable");

	const refreshGate = {
		queued: true,
		pendingMutations: 0,
		workerActive: false,
		mutationQueueLength: 0,
		fragmentActive: false,
		bfcacheActive: false,
	};
	assert.equal(queuedListingRefreshCanStart(refreshGate), true);
	assert.equal(queuedListingRefreshCanStart({ ...refreshGate, bfcacheActive: true }), false, "BFCache owns its single snapshot window");
	assert.equal(queuedListingRefreshCanStart({ ...refreshGate, pendingMutations: 1 }), false, "a quiescent uncertain mutation defers without spinning");
	assert.equal(
		JSON.stringify(boundedHiddenVerificationIds([["visible"], ["invisible-pending"], ["visible", "removed-shell"]])),
		JSON.stringify(["visible", "invisible-pending", "removed-shell"]),
		"BFCache includes pending IDs whose Undo shells are no longer in the DOM",
	);
	assert.equal(boundedHiddenVerificationIds([Array.from({ length: 251 }, (_, index) => `p-${index}`)]).length, 250);
});

test("uncertain recovery and Undo eviction keep listings authoritative and bounded", () => {
	assert.equal(mutationNeedsListingRecovery({
		changed: false,
		wasUncertain: true,
		actualHidden: false,
		removedPendingShell: false,
		hasListing: true,
	}), true, "verified-unhidden uncertainty requires a fresh listing snapshot");
	assert.equal(mutationNeedsListingRecovery({
		changed: false,
		wasUncertain: false,
		actualHidden: true,
		removedPendingShell: true,
		hasListing: true,
	}), true, "settling an evicted pending shell requires reconciliation");
	assert.equal(JSON.stringify(hiddenShellEvictionPlan(true, true)), JSON.stringify({ remove: true, trackPending: true }));
	assert.equal(JSON.stringify(hiddenShellEvictionPlan(true, false)), JSON.stringify({ remove: true, trackPending: false }));
	assert.equal(JSON.stringify(hiddenShellEvictionPlan(false, true)), JSON.stringify({ remove: false, trackPending: false }));
});
