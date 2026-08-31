# ShuvGrok — fork of xai-org/grok-build

ShuvGrok is a fork of [xai-org/grok-build](https://github.com/xai-org/grok-build).
Upstream is Apache-2.0; that license, its notices, and the full commit history
are preserved here.

| | |
|---|---|
| Upstream | `xai-org/grok-build` |
| Fork | `shuv1337/shuvgrok` |
| Forked at | `eb267feff13129e568df38fb6fdf0ceb65f735d6` ("Synced from monorepo") |
| Last synced | `bc7f02eddd3d` / `SOURCE_REV` `d5a0335a47221e8c9519936cb693e9b6450227ec` (2026-08-28) |
| Product name | ShuvGrok |
| Command | `shuvgrok` |
| npm package | `@shuv1337/shuvgrok` |

Upstream publishes into this repo as periodic squashed snapshots rather than
per-change commits, so a resync is a merge of one large commit. The
compatibility boundary below exists mostly to keep those merges tractable.

## Why this fork exists

Upstream ships a single-provider CLI. This fork adds first-class Anthropic
(Claude Pro/Max) and OpenAI Codex (ChatGPT Plus/Pro) subscription providers, so
one terminal client can drive all three accounts.

## The boundary

Names fall into four classes. `scripts/check-fork-boundary.mjs` asserts the
first two on every release, so a future upstream merge cannot silently blur
them.

### 1. Canonical — renamed, and asserted to stay renamed

- Product display name (`xai_grok_version::PRODUCT_NAME`)
- Command name `shuvgrok` and its npm `bin` entry
- npm packages: `@shuv1337/shuvgrok` plus six platform packages
- Repository and documentation

### 2. Compatibility — deliberately NOT renamed, and asserted to stay

These are load-bearing for existing installs, running integrations, and merge
sanity. Renaming them would break working setups and buy nothing a user can
see.

| Surface | Kept as | Why |
|---|---|---|
| Config/state directory | `~/.grok` | Renaming orphans existing credentials, sessions, and config. The npm installer writes `shuvgrok`-named binaries into it, so both products can coexist. |
| Environment variables | `GROK_*` | Referenced by user shell profiles, CI, and scripts outside this repo. |
| ACP extension methods | `x.ai/...` | A wire protocol shared with ACP clients that are not this repo. |
| Auth scope keys | `anthropic::oauth`, `openai-codex::oauth`, `https://auth.x.ai::<id>` | Keys inside `auth.json`; renaming logs everyone out. |
| Rust crate names | `xai-grok-*` | Internal, unpublished, and touched by nearly every upstream diff. Renaming ~60 crates would make every future merge a manual conflict. |
| Cargo binary artifact | `xai-grok-pager` | Same reason; the user-facing name is applied by the npm `bin` mapping. |
| Model-facing system prompt | upstream wording | Prompt text is tuned, and the third-party de-branding rules match against it. This is behavior, not branding. |

### 3. Provenance — preserved

Apache-2.0 license, `NOTICE`/third-party notices, authorship, and full history.

### 4. Deliberate behavioral deltas

- **The background update check is off** (`xai_grok_update::SELF_UPDATE_ENABLED`),
  which removes the startup banner and the detached installer it spawned. This
  build is usually a local `cargo build`, and a background job that swaps in a
  published binary replaces the thing you are testing. The explicit path still
  works — `shuvgrok update` resolves `@shuv1337/shuvgrok`, not upstream's
  package, so it installs this fork.
- **Alternative providers** are on by default behind the
  `grok_build_alt_providers` feature; `--no-default-features` still builds.
- **Provider model visibility** is gated on that provider's own credential
  rather than on the xAI session token.

## Syncing with upstream

```bash
jj git fetch --remote upstream
jj new @ main@upstream          # or: jj rebase -d main@upstream
node scripts/check-fork-boundary.mjs
```

Expect conflicts concentrated in the files this fork actually changes:
`auth/`, `agent/config.rs`, the sampler wire shaping, and the `/usage` pane.
If a merge reintroduces upstream branding in a canonical surface, the boundary
check fails and names the file.

## Releasing

`node scripts/release.mjs patch` bumps `Cargo.toml` and all seven npm
manifests in lockstep, commits, tags `vX.Y.Z`, and pushes. The tag starts
`.github/workflows/release.yml`, which cross-compiles six targets, publishes to
npm via trusted publishing (OIDC, no token), creates the GitHub release, and
posts to Discord.

First-time setup is listed in [`docs/RELEASING.md`](docs/RELEASING.md).
