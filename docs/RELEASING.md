# Releasing ShuvGrok

```bash
node scripts/release.mjs patch     # or: minor | major | 1.2.3
```

That bumps `Cargo.toml` and all seven npm manifests in lockstep, runs the fork
boundary check, `cargo fmt --all --check`, and `cargo check --workspace`,
commits `Release vX.Y.Z`, tags, and pushes. Pushing the tag is what starts CI.

`--dry-run` writes the version files and stops before committing.

## What CI does

`.github/workflows/release.yml`, on a `v*` tag:

1. **build** — cross-compiles six targets. Exports `GROK_VERSION=${TAG#v}`;
   without it `option_env!("GROK_VERSION")` is `None`, which the codebase reads
   as "dev build" and uses to auto-trust workspace folders. Do not remove it.
2. **publish-npm** — assembles the platform packages (brotli-compressed; the
   raw binaries exceed npm's tarball limit) and publishes the six platform
   packages *before* the meta package, which pins them by exact version.
   Authenticates by OIDC trusted publishing — there is no npm token.
3. **publish-github-release** — `gh release create --generate-notes` against
   the tag's commit SHA, then force-moves the floating `latest` git tag onto
   that commit. `latest` is updated here, not in `release.mjs`, so a failed
   publish cannot steal it.
4. **notify-discord** — posts the release to the shared webhook.

## One-time setup — done 2026-08-16

Recorded for the next fork or a registry reset; nothing here needs repeating.

All seven package names were reserved with `0.0.0` placeholders, because npm
trusted publishing cannot perform a package's first publish — the settings that
authorize CI do not exist until the package does. Placeholders were used rather
than a real first release so the bootstrap did not require six cross-compiled
binaries on one machine. CI supersedes them with the first real version.

Trusted publishers were configured with the **`npm trust` CLI**, not the
website:

```bash
npm trust github "@shuv1337/<pkg>" \
  --file release.yml --repo shuv1337/shuvgrok \
  --env npm-publish --allow-publish -y
```

Verify any of them with `npm trust list @shuv1337/shuvgrok`.

Also created: the `npm-publish` GitHub environment, and the
`DISCORD_RELEASE_WEBHOOK_URL` repository secret.

### If you ever repeat this

The account has 2FA set to `auth-and-writes`, so all fourteen operations
(7 publishes + 7 trust configs) are individually challenged. On the WebAuthn
page, tick **"Do not challenge npm publish, npm trust operations from IP
address … for the next 5 minutes"** before touching the key — that turns
fourteen key touches into one. Stage every package directory first so the whole
batch fits inside the window.

Two traps worth knowing:

- `npm publish` opens the URL in your **default** browser. If your automation
  drives a different browser, pass `--browser false` so npm only prints the URL
  and polls; opening the same auth id twice invalidates it.
- A brand-new package's packument 404s on `registry.npmjs.org` for a while
  after a successful publish. `npm access get status <pkg>` is the reliable
  existence check; `npm view` is not.

### If you rename the GitHub repo

npm authorizes a release by matching the OIDC claim against the repository
recorded on each package, and GitHub's redirect does **not** cover that claim.
A rename therefore breaks publishing silently: git keeps working, the diff
looks fine, and the next release fails at the token exchange.

Re-bind all seven. npm permits one trust config per package, so replacing means
revoke then create:

```bash
for n in shuvgrok-darwin-arm64 shuvgrok-darwin-x64 shuvgrok-linux-arm64 \
         shuvgrok-linux-x64 shuvgrok-win32-arm64 shuvgrok-win32-x64 shuvgrok; do
  pkg="@shuv1337/$n"
  id=$(npm trust list "$pkg" | awk '/^id:/{print $2}')
  [ -n "$id" ] && npm trust revoke "$pkg" --id="$id"
  npm trust github "$pkg" --file release.yml --repo shuv1337/shuvgrok \
    --env npm-publish --allow-publish -y
done
```

That is 14 challenged operations, so tick the 5-minute cooldown box before
touching the key. Verify with `npm trust list` per package rather than trusting
the loop's output. `scripts/check-fork-boundary.mjs` asserts the manifests agree
with the canonical slug, but it cannot see the registry-side config — that
check is manual.

## Validating without releasing

The cross-compile matrix is the least-proven part of this pipeline. Exercise it
without publishing anything:

```bash
gh workflow run release.yml -f tag=v1.0.3 -f source_ref=main -f dry_run=true
```

`dry_run` builds all six targets, assembles and brotli-compresses the platform
packages, and runs `publish-npm.mjs --dry-run`, then stops. No npm publish, no
GitHub release, no Discord post. Pass a `tag` whose version matches the current
`shuvgrok/package.json`, since the job asserts they agree.

## Not yet exercised

The `aarch64-pc-windows-msvc` and `aarch64-unknown-linux-gnu` matrix legs are
written but have never run. Expect the first release to need a fix there —
Windows-on-ARM cross-links from an x64 image and may need extra MSVC ARM64
components.
