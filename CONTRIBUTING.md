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
cargo test                                 # unit (headless GUI included), property and doc tests; never runs the ignored suites
cargo nextest run                          # optional runner (.config/nextest.toml); use cargo test --doc for doctests
cargo clippy --all-targets -- -D warnings  # must be clean
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo deny check licenses                  # licence policy in deny.toml
cargo build --examples && cargo e2e        # end-to-end suite (alias in .cargo/config.toml)
```

**The e2e suite and the `--ignored` `mcp` and `cli` tests open real windows and start real resident processes on
your desktop.** Close your own games and dialogs first, run them with `--test-threads=1` (the `cargo e2e` alias
does), and do not touch the keyboard or mouse while they run. Each case gets its own instance
(`BORDERLESS_SHATTER_INSTANCE`), and its child processes get a temporary `APPDATA` and `LOCALAPPDATA` under
`target\e2e-tmp`. The `--ignored` `ipc::pipe` test opens no window, but it binds the real pipe name, so close any
running resident before it. The tiers and what each needs are listed in `AGENTS.md`, "Testing & QA".

Before a PR: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and
`cargo deny check licenses` must pass; these are the checks CI runs (`.github/workflows/test.yml`). That workflow
runs on pull requests, on demand (Actions tab, `workflow_dispatch`) and when `release.yml` calls it; a push to
`main` outside the release paths is not tested again.

## e2e on GitHub Actions

`.github/workflows/e2e.yml` (workflow `e2e`) runs on `windows-latest`. It runs the property suite, then a
session probe (`examples/session_probe.rs`). If the probe reports no usable desktop, the job fails before the
suites. Otherwise it runs the e2e, `mcp`/`cli` and `ipc::pipe` suites twice each with `--test-threads=1`,
and the release `mcp` suite once. It runs on pull requests that touch `src/**`, `tests/**`, `examples/**`,
`locales/**`, `.cargo/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` or `.github/workflows/e2e.yml`, and on demand:

```text
gh workflow run e2e.yml --ref <branch>
```

On-demand runs need the workflow file on `main`. If `gh` refuses the branch, use the run of the pull request
or run the workflow on `main` after the merge.

A run takes about 7 minutes on a warm cache; the release `mcp` case builds without LTO and with 16 codegen
units (`CARGO_PROFILE_RELEASE_*` env on that step only). The first run of a branch builds cold and saves the
cache; later pushes reuse it.

The run summary (`$GITHUB_STEP_SUMMARY`, shown on the run page) has three tables: the probe result, passed and
failed per suite and run, and the hygiene checks after each suite. The full logs are in the `e2e-logs`
artifact, kept for 14 days.

## Local fallback

Use this on your own desktop when the hosted run is not usable, or to check a change before the workflow runs.
It opens real windows on your desktop, so run it only with the owner's go-ahead. Run the commands from the
repository root, in this order (PowerShell):

```text
cargo build --all-targets
cargo test --test properties                                     # run twice; same passed count
$env:PROPTEST_CASES = 2000; cargo test --test properties; Remove-Item Env:PROPTEST_CASES
cargo e2e                                                        # run twice
cargo test --test mcp --test cli -- --ignored --test-threads=1   # run twice
cargo test --lib ipc::pipe -- --ignored --test-threads=1         # run twice
cargo build --release --examples
cargo test --release --test mcp -- --ignored --test-threads=1
```

Checklist:

- Before the first run, quit the resident from the tray menu, close any game or app dialog that could take focus,
  and do not touch the keyboard or mouse until the run finishes.
- Before the first run, record the `borderless-shatter.exe` PIDs (`tasklist /FI "IMAGENAME eq borderless-shatter.exe"`)
  and the state of `%APPDATA%\BorderlessShatter`: absent, or the SHA-256 of each file
  (`Get-ChildItem "$env:APPDATA\BorderlessShatter" -File | Get-FileHash`).
- After each run, check that `tasklist /FI "IMAGENAME eq testwin.exe"` finds no process, the
  `borderless-shatter.exe` PIDs are the recorded ones, `target\e2e-tmp` is empty or absent, and
  `%APPDATA%\BorderlessShatter` is still absent or its hashes are unchanged.

## Code rules

- No `unwrap`/`expect` outside tests; every `unsafe` block has a `// SAFETY:` comment.
- Pure logic (`window_state`, `guard`, `islands`, GUI models) stays free of `windows` types and gets a unit
  test written first; test names read as sentences.
- GUI views are tested headless on GPUI's test platform (`#[gpui_kit::test]`, see `AGENTS.md`, "Testing & QA"):
  no OS window opens, and the pipe is never reached. An element a test clicks or finds needs an id and
  `.test_support()`.
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
  update `README.md`, `ARCHITECTURE.md`, `docs/GAMES.md` and this file when what the product does changes
  (they describe the product, not how it was built). `CHANGELOG.md` is generated; do not edit it.

## Releases

Releases are automatic. A push to `main` that touches `src/**`, `locales/**`, `Cargo.toml` or `Cargo.lock`
runs `.github/workflows/release.yml`: tests, then a build that picks the next version from the Conventional
Commits (`git-cliff`, `cliff.toml`), updates `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md`, builds
`borderless-shatter.exe --release --locked`, and a publish job that commits `chore(release): vX.Y.Z`, tags it
and creates the GitHub release with the exe and its `.sha256`. A release is cut only when the commits since
the last tag include a `feat`, `fix`, `perf` or a breaking change (`!` after the type or scope, or a
`BREAKING CHANGE:` footer); `docs`, `chore`, `test`, `ci`, `build`, `refactor` and `style` accumulate and
appear in the changelog of the next release. When none qualifies the run ends green with a notice. Do not edit the generated `CHANGELOG.md` sections or tag by hand.
`CHANGELOG.md` must start with the `header` of `cliff.toml` (CI checks it); change both together.

### Manual release

Use this for a forced version level, or to release docs-only changes.

1. On `main`, up to date with `origin/main`, with a clean tree (`git status --short` empty) and `cargo test`
   green. Cut releases from the main checkout, not from an issue worktree.
2. Next version: `git cliff --bumped-version`, or the level wanted from the latest tag. Never below the
   `version` already in `Cargo.toml`. On 0.x, a `feat` bumps only the patch in git-cliff's proposal, so set
   the level explicitly when a higher one is needed.
3. Set `version` in `Cargo.toml`, then `cargo update -p borderless-shatter` (refreshes only the package's
   `Cargo.lock` entry).
4. `git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md` (never `-o`: it would drop the hand-written
   entries below the header). If `git-cliff` is missing, install it with `scoop install git-cliff`.
5. Review the new CHANGELOG section and the version.
6. `git add Cargo.toml Cargo.lock CHANGELOG.md`, commit `chore(release): vX.Y.Z` (the workflow skips that
   message), then `git tag -a vX.Y.Z -m "borderless-shatter vX.Y.Z"`. Commit with the identity configured in
   the checkout.
7. `cargo build --release --locked`, then the `.sha256` (`sha256sum --text borderless-shatter.exe > borderless-shatter.exe.sha256`
   run inside `target/release`; `--text` keeps Windows from writing `*` before the name), before
   pushing, so a broken build leaves no tag. Push in one atomic push: `git push --atomic origin main vX.Y.Z`.
   Then `gh release create vX.Y.Z --title vX.Y.Z --notes-file <(git cliff --latest --strip all) target/release/borderless-shatter.exe target/release/borderless-shatter.exe.sha256`.

Never move or delete a pushed tag; a wrong release gets a new release.

### Recovering a failed release

- **Before the push** (a check, the build, or a push rejected because `main` moved): nothing landed. The next
  push to `main` that touches the release paths releases everything pending. Re-running the workflow helps
  only after a transient failure while `main` has not moved.
- **After the push** (`gh release create` failed): the tag exists without a release, the run says so with
  `::error`, and every later `publish` job fails before pushing until the release exists. Create the release
  from the failed run's artifact, which holds the exe built from exactly the tagged tree:
  `gh run download <run-id> -n release -D rel`, then
  `gh release create vX.Y.Z --title vX.Y.Z --notes-file rel/notes.md rel/borderless-shatter.exe rel/borderless-shatter.exe.sha256`.
  If the artifact has expired, build from the tag instead: `cargo build --release --locked` on a checkout of
  `vX.Y.Z`, run `sha256sum --text borderless-shatter.exe > borderless-shatter.exe.sha256` inside `target/release`, and attach those files. Then re-run the blocked
  `publish` job, or let the next push release.

## Clean-room and licence

Shattered Islands is a clean-room implementation: no code or text from GPL projects is copied, and behaviour
is documented from public sources. The project is MIT, so never copy or translate code or text from Borderless
Gaming (GPL-2.0), ihateborders (GPL-3.0) or Zed's GPL crates, and do not add dependencies whose licence
`cargo deny` rejects.
