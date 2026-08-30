if ("serviceWorker" in navigator) {
	window.addEventListener("load", () => {
		navigator.serviceWorker.register("/service-worker.js").catch(() => {
			// Installation is optional; the reader remains fully usable without it.
		});
	});
}
