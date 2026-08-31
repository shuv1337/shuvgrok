#!/usr/bin/env node
/**
 * Local release driver for ShuvGrok.
 *
 * Usage:
 *   node scripts/release.mjs <major|minor|patch>
 *   node scripts/release.mjs <x.y.z>
 *   node scripts/release.mjs <bump> --dry-run   # edit files, skip commit/tag/push
 *
 * Steps:
 *   1. Refuse to run on a dirty worktree
 *   2. Compute the next version from the [workspace.package] version in Cargo.toml
 *   3. Write it to Cargo.toml and all 7 npm package.json files (lockstep,
 *      including the meta package's exact optionalDependencies pins)
 *   4. cargo fmt --all --check, then cargo check --workspace (refreshes Cargo.lock)
 *   5. Commit, tag vX.Y.Z, push branch + tag
 *      (CI force-moves the floating `latest` tag after a successful publish)
 *   6. Announce on Discord (best effort; skipped when the webhook is unset)
 *
 * The heavy lifting happens in CI: pushing the tag triggers
 * .github/workflows/release.yml, which builds the six binaries, publishes the
 * npm packages via trusted publishing, and cuts the GitHub release. Deliberately
 * no test-suite run here — it is far too slow for a release driver.
 */

import { execFileSync, execSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { notifyDiscordRelease } from "./notify-discord-release.mjs";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const cargoTomlPath = join(repoRoot, "Cargo.toml");
const npmRoot = join(repoRoot, "crates", "codegen", "xai-grok-pager", "npm");

const PLATFORM_KEYS = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-arm64", "win32-x64"];
const META_PACKAGE = "@shuv1337/shuvgrok";
const BUMP_TYPES = new Set(["major", "minor", "patch"]);
const SEMVER_RE = /^\d+\.\d+\.\d+$/;

const args = process.argv.slice(2);
const dryRun = args.includes("--dry-run");
const target = args.find((arg) => !arg.startsWith("--"));

if (!target || (!BUMP_TYPES.has(target) && !SEMVER_RE.test(target))) {
	console.error("Usage: node scripts/release.mjs <major|minor|patch|x.y.z> [--dry-run]");
	process.exit(1);
}

function run(command, options = {}) {
	console.log(`$ ${command}`);
	try {
		return execSync(command, { cwd: repoRoot, encoding: "utf8", stdio: options.silent ? "pipe" : "inherit", ...options });
	} catch (error) {
		if (options.ignoreError) {
			return null;
		}
		console.error(`Command failed: ${command}`);
		if (options.silent && error?.stderr) {
			console.error(error.stderr);
		}
		process.exit(1);
	}
}

function git(args, options = {}) {
	return execFileSync("git", args, { cwd: repoRoot, encoding: "utf8", ...options });
}

/** The [workspace.package] version in the root Cargo.toml is the source of truth. */
const WORKSPACE_VERSION_RE = /(\[workspace\.package\][\s\S]*?\nversion = ")([^"]+)(")/;

function readCargoVersion() {
	const match = readFileSync(cargoTomlPath, "utf8").match(WORKSPACE_VERSION_RE);
	if (!match) {
		throw new Error(`Could not find a version in [workspace.package] in ${cargoTomlPath}`);
	}
	return match[2];
}

function writeCargoVersion(version) {
	const contents = readFileSync(cargoTomlPath, "utf8");
	writeFileSync(cargoTomlPath, contents.replace(WORKSPACE_VERSION_RE, `$1${version}$3`));
	console.log(`  Cargo.toml [workspace.package] version -> ${version}`);
}

function npmPackageDirectories() {
	return [join(npmRoot, "shuvgrok"), ...PLATFORM_KEYS.map((key) => join(npmRoot, `shuvgrok-${key}`))];
}

function writeNpmVersions(version) {
	for (const directory of npmPackageDirectories()) {
		const manifestPath = join(directory, "package.json");
		const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
		manifest.version = version;

		if (manifest.optionalDependencies) {
			for (const name of Object.keys(manifest.optionalDependencies)) {
				// Exact pins: the meta package must resolve the platform packages
				// built from this very tag.
				manifest.optionalDependencies[name] = version;
			}
		}

		// Match the existing 4-space formatting of these manifests.
		writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 4)}\n`);
		console.log(`  ${relative(repoRoot, manifestPath)} -> ${version}`);
	}
}

function nextVersion(current, bump) {
	if (SEMVER_RE.test(bump)) {
		if (compareVersions(bump, current) <= 0) {
			console.error(`Error: explicit version ${bump} must be greater than current version ${current}.`);
			process.exit(1);
		}
		return bump;
	}

	const [major, minor, patch] = current.split(".").map(Number);
	if (bump === "major") return `${major + 1}.0.0`;
	if (bump === "minor") return `${major}.${minor + 1}.0`;
	return `${major}.${minor}.${patch + 1}`;
}

function compareVersions(a, b) {
	const aParts = a.split(".").map(Number);
	const bParts = b.split(".").map(Number);
	for (let i = 0; i < 3; i++) {
		const diff = (aParts[i] || 0) - (bParts[i] || 0);
		if (diff !== 0) {
			return diff;
		}
	}
	return 0;
}

console.log("\n=== ShuvGrok Release ===\n");

// 1. Clean worktree.
console.log("Checking for uncommitted changes...");
const status = git(["status", "--porcelain"]);
if (status.trim()) {
	console.error("Error: uncommitted changes detected. Commit or stash first.");
	console.error(status);
	process.exit(1);
}
console.log("  Working directory clean\n");

// This repo is normally driven through jj, which keeps git HEAD *detached*.
// `rev-parse --abbrev-ref HEAD` then returns the literal "HEAD", and
// `git push origin HEAD` fails with "not a full refname". Fall back to the
// remote's default branch and push an explicit refspec, which works the same
// on a plain git checkout and under jj.
const headRef = git(["rev-parse", "--abbrev-ref", "HEAD"]).trim();
const branch =
	headRef === "HEAD"
		? git(["rev-parse", "--abbrev-ref", "origin/HEAD"])
				.trim()
				.replace(/^origin\//, "") || "main"
		: headRef;
if (headRef === "HEAD") {
	console.log(`  Detached HEAD (jj); releasing onto ${branch}\n`);
}

// 2. Compute the next version.
const currentVersion = readCargoVersion();
const version = nextVersion(currentVersion, target);
const tag = `v${version}`;
console.log(`Version: ${currentVersion} -> ${version} (tag ${tag})\n`);

const existingTag = git(["tag", "--list", tag]).trim();
if (existingTag) {
	console.error(`Error: tag ${tag} already exists.`);
	process.exit(1);
}

// 3. Write the version everywhere (Cargo + the 7 npm manifests, in lockstep).
console.log("Updating versions...");
writeCargoVersion(version);
writeNpmVersions(version);
console.log();

// 4. Fast checks only. The full test suite is far too slow to gate a release on.
// Cheap, and the failure it catches (an upstream merge quietly restoring
// upstream branding, or a rename reaching a compatibility surface) is one that
// would otherwise ship.
console.log("Checking fork boundary...");
run("node scripts/check-fork-boundary.mjs");

console.log("Running cargo fmt...");
run("cargo fmt --all -- --check");
console.log();

console.log("Running cargo check (also refreshes Cargo.lock)...");
run("cargo check --workspace");
console.log();

if (dryRun) {
	console.log(`Dry run: files updated to ${version}; no commit, tag, or push. Revert with: git checkout -- .`);
	process.exit(0);
}

// 5. Commit, tag, push. The tag push is what starts the release workflow.
console.log("Committing and tagging...");
run(`git add -A`);
run(`git commit -m "Release ${tag}"`);
run(`git tag ${tag}`);
console.log();

console.log("Pushing to remote...");
// Explicit refspec: HEAD is detached under jj, so `git push origin <branch>`
// would look for a local branch of that name and find none.
run(`git push origin HEAD:refs/heads/${branch}`);
run(`git push origin ${tag}`);
console.log();

// 6. Discord announcement: best effort, never fails the release.
console.log("Announcing release on Discord...");
try {
	const result = await notifyDiscordRelease({ version });
	if (result.skipped) {
		console.log(`  Skipped: ${result.reason}`);
	} else {
		console.log("  Posted:");
		console.log(result.content);
	}
} catch (error) {
	console.error(`  Failed: ${error instanceof Error ? error.message : error}`);
}
console.log();

console.log(`=== Released ${tag}: pushed ${branch} + ${tag}; CI publishes ${META_PACKAGE} and the GitHub release ===`);
