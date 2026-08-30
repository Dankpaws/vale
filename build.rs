use std::process::Command;

fn git_output(arguments: &[&str]) -> Option<String> {
	Command::new("git")
		.args(arguments)
		.output()
		.ok()
		.filter(|output| output.status.success())
		.and_then(|output| String::from_utf8(output.stdout).ok())
		.map(|output| output.trim().to_string())
}

fn emit_rerun_paths() {
	if let Some(tracked) = git_output(&["ls-files", "-z"]) {
		for path in tracked.split('\0').filter(|path| !path.is_empty()) {
			println!("cargo:rerun-if-changed={path}");
		}
	} else {
		// Source archives and container build contexts deliberately have no Git
		// metadata. Keep ordinary source changes observable in that case.
		println!("cargo:rerun-if-changed=src/");
		println!("cargo:rerun-if-changed=build.rs");
	}

	// A commit can advance without changing any tracked file (for example, an
	// annotated rebuild). Watch both detached and symbolic HEAD layouts so an
	// incremental native build never reports the preceding revision.
	for git_path in [git_output(&["rev-parse", "--git-path", "HEAD"]), git_output(&["rev-parse", "--git-path", "packed-refs"])]
		.into_iter()
		.flatten()
	{
		println!("cargo:rerun-if-changed={git_path}");
	}
	if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
		if let Some(git_path) = git_output(&["rev-parse", "--git-path", &head_ref]) {
			println!("cargo:rerun-if-changed={git_path}");
		}
	}
}

fn main() {
	emit_rerun_paths();
	println!("cargo:rerun-if-env-changed=VALE_BUILD_COMMIT");

	let explicit = std::env::var("VALE_BUILD_COMMIT").unwrap_or_default();
	let clean_checkout = Command::new("git")
		.args(["status", "--porcelain", "--untracked-files=no"])
		.output()
		.ok()
		.filter(|output| output.status.success())
		.is_some_and(|output| output.stdout.is_empty());
	let discovered = clean_checkout
		.then(|| Command::new("git").args(["rev-parse", "HEAD"]).output().ok())
		.flatten()
		.filter(|output| output.status.success())
		.and_then(|output| String::from_utf8(output.stdout).ok())
		.unwrap_or_default();
	let candidate = if explicit.trim().is_empty() { discovered.trim() } else { explicit.trim() };
	let git_hash = if (7..=64).contains(&candidate.len()) && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		candidate
	} else {
		"dev"
	};
	println!("cargo:rustc-env=GIT_HASH={git_hash}");
}
