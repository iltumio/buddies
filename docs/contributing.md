# Development

```bash
cargo test --all-targets                    # unit and integration-style tests
cargo fmt --check                           # formatting
cargo clippy --all-targets -- -D warnings   # lints
```

CI runs all three, a Rust 1.91 compatibility check, and MCP transport/shutdown
smoke tests on every pull request and on pushes to `main`. Warnings are errors,
so run `cargo clippy` locally before opening a PR. Dependabot checks Cargo and
GitHub Actions weekly; a separate weekly job runs `cargo audit`.

To run the Linux transport test locally (requires Python 3, Git, and a debug build):

```bash
cargo build --locked
python3 scripts/smoke_mcp.py
```

Signature regression tests require `ssh-keygen`. All smoke-test data and watched
files are created in temporary directories.

Two things to know before changing the wire format in `src/protocol.rs`:

- `P2PMessageBody` is serialized with postcard, which encodes enum variants **by index**. Adding a variant anywhere but the end silently breaks compatibility with peers running an older build. Always append.
- Gossip frames are capped at 256 KiB (`GOSSIP_MAX_MESSAGE_SIZE` in `src/node.rs`). Exceeding the limit is fatal to the connection on both send and receive, so any new payload field needs an explicit size cap — see `FileActivityEntry::validate_received` in `src/activity.rs` for the pattern.

Fields that arrive from peers are attacker-controlled. Validate them at receipt (length, timestamps, NUL bytes) rather than trusting the sender, and do it *after* signature verification so unverified traffic can't reach the validation path.

## Documentation

Use Node.js 22 or newer.

```sh
npm ci
npm run docs:dev
npm run docs:build
npm run docs:preview
```

VitePress 1.6 uses a patched Vite 6 override (compatible with its Vue plugin) and a patched KaTeX override for Mermaid. Keep these overrides until the upstream dependency ranges include patched releases.

The build generates HTML, per-page Markdown, `llms.txt` and `llms-full.txt` from the same published pages. It checks links and generated agent exports. Edit the Markdown sources rather than the generated files in `docs/.vitepress/dist`. Historical design/review files under `docs/superpowers` and the repository review are excluded from the public site.

Pull requests build the docs without publishing. Pushes to `main` build and deploy through the Documentation workflow to GitHub Pages.

## Automatic releases

[release-plz](https://release-plz.dev/docs/config#the-git_only-field) manages
versions and `CHANGELOG.md` using the existing `v{{ version }}` tags. Releases
are GitHub-only; no crates.io token is required.

1. A push to `main` runs the shared CI checks, then release-plz opens or updates
   a release PR with the next version and changelog. Conventional Commits drive
   version selection. CI is explicitly dispatched on the generated branch.
2. Review and merge the release PR. Once CI passes, release-plz creates the tag
   and a draft GitHub release.
3. The workflow builds Linux x86_64 and ARM64 binaries from that exact tag,
   attaches both binaries, and publishes the release as latest. This keeps the
   installer's existing download URLs working.

Automation uses the repository's `GITHUB_TOKEN`. In **Settings → Actions →
General → Workflow permissions**, enable **Allow GitHub Actions to create and
approve pull requests**. No additional PAT or registry secret is needed.
The build is invoked as a reusable workflow because tags created with
`GITHUB_TOKEN` do not trigger another push workflow.

If binary compilation fails, the release stays a draft. Rerun the failed jobs,
or manually run the **Build** workflow with `release_tag` set to the existing
version tag. Uploads are idempotent. Regular CI can also be run manually through
its `workflow_dispatch` trigger.
