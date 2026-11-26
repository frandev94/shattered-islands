# Contributing

Read [ARCHITECTURE.md](ARCHITECTURE.md) first (processes, modules, invariants) and [AGENTS.md](AGENTS.md)
(conventions, test tiers). This file covers the workflow.

## Toolchain

Windows, x86_64 MSVC. `rust-toolchain.toml` pins the channel (`1.100.0`) with `rustfmt` and `clippy`; rustup
installs it on first use. The crate is Rust 2024 edition.

## Commands

```text
cargo build                                # debug build, console visible
cargo build --release                      # windows subsystem, LTO, stripped
cargo test                                 # unit, property and doc tests; never runs the ignored suites
cargo nextest run                          # optional runner (.config/nextest.toml); use cargo test --doc for doctests
cargo clippy --all-targets -- -D warnings  # must be clean
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo deny check licenses                  # licence policy in deny.toml
cargo build --examples && cargo e2e        # end-to-end suite (alias in .cargo/config.toml)
```

**The e2e suite (and the `--ignored` `mcp`, `cli` and `ipc::pipe` tests) opens real windows and starts real
resident processes on your desktop.** Close your own games and apps' dialogs first, run it single-threaded
(the alias does) and do not touch the keyboard or mouse while it runs. Each case uses an isolated instance
(`BORDERLESS_SHATTER_INSTANCE`, 1 to 32 characters of `A-Za-z0-9-`; an invalid value is refused with exit code 1, or 2 for the CLI; the resident and GUI print `fatal:` to stderr, which is invisible unless started from a console) and a temporary profile directory, and only touches windows it created. CI never
runs ignored tests. The tiers and what each needs are listed in `AGENTS.md`, "Testing & QA".

Before a PR: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and
`cargo deny check licenses` must pass; these are the checks CI runs (`.github/workflows/test.yml`).

## Code rules

- No `unwrap`/`expect` outside tests; every `unsafe` block has a `// SAFETY:` comment.
- Pure logic (`window_state`, `guard`, `islands`, GUI models) stays free of `windows` types and gets a unit
  test written first; test names read as sentences.
- Foreign-window calls happen only on the resident thread (see ARCHITECTURE.md, "Event loop").

## Text and languages

- Code, identifiers, comments, commit messages and docs are English. The only Spanish in the repo is the
  Spanish catalog.
- User-facing text (GUI, tray, CLI output) goes through the catalogs in `locales/`: `en` is the default and
  the source of truth, `es` is the second shipped language, and a missing key falls back to English. Never
  hard-code a user-facing string; add the key to `en` and `es` together.
- Protocol text stays English: MCP tool descriptions and the engine's `ErrorBody.message`. Clients match
  `ErrorBody.code`, never the message.

## Issues, branches, commits

- One issue = one branch = one worktree. Branch `<type>/<n>-<name>` (`type` is a Conventional Commits type,
  `n` the issue number), created from `main` in a worktree outside the checkout:
  `git worktree add ../borderless-shatter-worktrees/<n>-<name> -b <type>/<n>-<name> main`.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/): `feat(gui): ...`,
  `fix(engine): ...`, `docs: ...`. Put `!` after the type (or a `BREAKING CHANGE:` footer) on a breaking
  change; while the version is 0.x that bumps the minor, anything else bumps the patch.
- Never put a version number in a commit message or a branch name; versions come from the release tooling.
- Open the PR against `main` with `Closes #<n>` in the description. Docs follow behaviour in the same PR:
  update `README.md`, `CHANGELOG.md` and `docs/` when what the product does changes (they describe the
  product, not how it was built).

## Releases

Releases are automatic. A push to `main` that touches `src/**`, `locales/**`, `Cargo.toml` or `Cargo.lock`
runs `.github/workflows/release.yml`: tests, then a build that picks the next version from the Conventional
Commits (`git-cliff`, `cliff.toml`), updates `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md`, builds
`borderless-shatter.exe --release --locked`, and a publish job that commits `chore(release): vX.Y.Z`, tags it
and creates the GitHub release with the exe and its `.sha256`. Docs-only and test-only merges release nothing;
the next release includes them. Do not edit the generated `CHANGELOG.md` sections or tag by hand.
`CHANGELOG.md` must start with the `header` of `cliff.toml` (CI checks it); change both together.

## Clean-room and licence

The project is MIT. Never copy or translate code or text from Borderless Gaming (GPL-2.0), ihateborders
(GPL-3.0) or Zed's GPL crates, and do not add dependencies whose licence `cargo deny` rejects.
