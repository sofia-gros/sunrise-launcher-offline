# Agents working on the Sunrise launcher

For AI coding agents (and anyone new). [CONTRIBUTING.md](CONTRIBUTING.md) and the
[README](README.md) are the source of truth; this file is the short version, plus what agents
tend to get wrong here.

## Workflow

- Start from a fresh base: `git fetch origin` and branch from `origin/main`. Don't build on a
  stale `main`.
- Sign off every commit (`git commit -s`). The DCO check fails a PR without it;
  `git rebase --signoff origin/main` fixes your own commits (not someone else's).
- Never push to `main`, force-push it, or move or delete release tags. `main` only changes
  through PRs, and each one needs a code owner's approval.
- One concern per PR. A move or rename goes in its own PR, with no behaviour change, so a
  reviewer can check that nothing else changed.
- Open a PR as a draft until it's ready for review. Don't merge PRs; the person you work for
  decides that.
- Don't rebase, push to or close someone else's branch or PR stack.

## Before you open a PR

A local checklist (the Rust version is in `tools/ci/rust-version`):

    cargo fmt --all --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked --workspace --lib --bins

`.github/workflows/ci.yml` is the full, platform-specific list. It also runs the Play, game
relay, lifecycle and modpack fence fixtures everywhere, `developer_install` only
on Windows, doc tests only on Linux, and the release signing tests on
Linux and macOS. Run the ones your change touches.

- The full `cargo test` takes a while, and the `storage` tests alone take about 90 s. A slow test
  isn't a hung one; wait for it to finish before deciding.
- Warnings that are already on `main` aren't yours to fix in an unrelated PR. Say which ones
  you saw.
- Ignored tests are listed in `tools/ci/manual-tests.txt`, with what each one needs.

## The PR description

Follow the PR template (`.github/pull_request_template.md`):

- **First line:** the disclosure that applies to you, saying who opened the PR and on whose
  behalf, e.g. `> 🤖 Opened by <name>'s AI agent, not by <name> personally.`
- **What changed** and **Why**. For a bug, include the cause and how you found it, not only the
  fix.
- **Self-check:** the exact head SHA, the commands you ran and on which OS, what you tried in
  the running launcher, and **Not checked**. Be honest in Not checked: an untested platform or
  path is fine to admit, and claiming it was tested when it wasn't is not.
- **Numbers over adjectives:** "frames every 48 ms before, 16.7 ms after" rather than
  "smoother".
- **UI or design changes:** a before (taken on `main`) and an after of each screen, at 1280×800
  and 800×600, in the same window size and state.
- **Stacked or split work:** "Based on #N" or "Extracted from #N (`sha`)", and what has to merge
  first.

## Running the launcher

- Point test runs at scratch folders, never the player's real data. Set all of these:
  - `SUNRISE_LAUNCHER_HOME` and `SUNRISE_HOME`, each to a fresh temp dir. With only the
    launcher home set, the game folder still resolves to the player's (No AI writes its
    `sunrise-extras.txt` there).
  - `SUNRISE_LAUNCHER_LOCATION_FILE` to a scratch path, or startup takes the migration lock
    beside the real location file.
  - `SUNRISE_NO_KEYCHAIN=1`. The scratch homes keep `accounts.json` apart, but the OS keychain
    is shared: signing in and out of an account in a test run can delete the regular launcher's
    saved sign-in for it. With this set, test credentials stay in memory.
  - `SUNRISE_LAUNCHER_NO_SELF_UPDATE=1`.
  - On Linux, `XDG_DATA_HOME` to an absolute scratch path, at least for release builds. A
    release build repairs the applications-menu entry under it on startup (`src/desktop.rs`),
    and a relative or unset value falls back to the real `~/.local/share`.

  `tools/lab.py` (`game_env`) sets up a scratch environment for scripts, but it keeps home
  overrides you've already exported.
- Judge performance only in a release build (`cargo build --release`). A debug build stutters on
  its own.
- To measure frames: `cargo build --release --features frame-stats`, then run with
  `SUNRISE_LAUNCHER_FRAME_OVERLAY=1`. `sunrise-launcher benchmark` and the session log in
  `benchmarks/` are covered in the README's "Performance log".
- GPUI caps an unfocused window to one frame per 33 ms (~21 fps on a 60 Hz display), except in
  benchmark mode (`WindowOptions` in `src/startup.rs`). The main window opts out of GPUI's
  whole-window power-saver cap; instead, repeating effects (a shuttle, a pulse, rain) hold
  themselves back while the system saves power (`motion::endless`). If an animation looks
  choppy, rule these out before tuning curves.
- Under reduced motion nothing animates: use `motion::eased` and `motion::still()` (`src/motion.rs`)
  rather than raw GPUI animations.

## Keep it organized

In November 2026 this repository needed a multi-day clean-up: an 8,586-line `backend.rs`, a
590-line `cli::execute`, 55 files over 1,000 lines, 55 `#[path]` hacks and tests that broke each
other. These rules keep it from happening again. [docs/refactoring.md](docs/refactoring.md) has
the long version, and the CI ratchet (`tools/ci/structure_check.py`) enforces the counts.

**Where code goes**

- New code goes in its domain's folder (`account/`, `mods/`, `servers/`, `backend/`…), next
  to the code it works with. **No new files directly under `src/`.** If no domain fits, ask
  before inventing one.
- A file's path is its module path: no `#[path]`, and no `mod.rs` (`foo.rs` next to `foo/`).
- Name things after what they do. No `glass_`, `blocks_`, `alt_`, `launcher_`, `new_`,
  `old_`, `_v2` (unless it names a file-format version) and no `_legacy`.
- `lib.rs` is the core shared by the window, CLI and MCP server, and never uses `gpui`. UI-only
  modules are declared only in `main.rs`.
- Don't add an import that makes two modules import each other. `structure_check.py` counts
  mutual pairs and import cycles, and they may only go down.

**Size**

- **Split before you grow.** A non-test file stays under 1,000 lines and a function under
  80. If your change would cross either limit, split first, in its own PR, then make the change.
  The ratchet counts both and fails a PR that raises either count.
- Tests live in `<module>/tests.rs`, or `<module>/tests/<group>.rs` once that grows too, not
  inline at the bottom of a big file.

**Errors, panics and tests**

- No `.unwrap()` in non-test code. Use `?` or `expect("why this can't fail")`.
- **Never decide on an error's text** (`e.contains("prepared")`): translated messages break
  it, as sunrise-org/sunrise-launcher#530 shows. If code acts on a case, give it a typed variant.
  Don't type errors nobody acts on: a message stays a `String`.
- Tests never set process-wide state (`std::env::set_var`, shared folders, fixed ports). Pass
  the value in (an `Option<&Path>`, a URL parameter, port 0) and give each test its own scratch
  folder.

**Before you move code**

- `grep` for tests that read source files (`include_str!`, `read_to_string("src/…")`) and for
  tests that re-run themselves by path (`--exact "module::tests::name"`). Those break silently
  when code moves. Keep their paths, or update them in the same commit.
- An ignored test that moves gets its line in `tools/ci/manual-tests.txt` updated.
- Run `cargo clippy --all-targets -- -D warnings`, not only `cargo check`. Clippy catches what
  a move breaks, like an item left below a test module.
- Run `python3 tools/ci/structure_check.py --base origin/main` and say in the PR which counts
  moved.

**Working alongside others**

- A move or rename is its own PR with no behaviour change, and its description says how you
  proved that (byte-identical bodies, the same test names, the same CLI `--json` output).
- Before you split or move a file, check that no open PR or pushed branch touches it
  (`gh pr list`, `git log origin/<branch>`). Claim it in Agent Chat (docs/refactoring.md §9).
- **Finish what you start.** Don't leave an old and a new version side by side, and don't leave
  a half-done migration on a branch. If you have to stop, push the branch with a "Not done" list
  in its notes, so nobody redoes the work.
- **Leave the file better than you found it**, but only the part you're working in. The
  broad clean-up is done. From here, tidy what your change touches and nothing more.

## Project specifics

- **User-facing text** goes through `tr("…")` / `trf("…", &[…])`, never a bare string. Add new
  keys to `lang/en.json` with `tools/launcher_add_lang_keys.py "Your string"` (the other
  catalogs hold only translations, which come through Crowdin). Change a key's English with
  `--rename OLD NEW` so its translations move with it. A label that reaches `tr` later, through a
  constant, a field, an array or a variable, is written `tr_key("…")` where it's defined, so
  the completeness test (`i18n::tests::the_english_catalog_lists_every_key`) sees it.
- **The window, CLI and MCP server share one core**, `src/backend.rs` and `src/backend/`. A
  behaviour change there affects all three, so check the CLI (`--json`) too.
- **Copies of game files:** when a change brings in a game change, update
  `tools/ci/sunrise-commit` in the same PR (CONTRIBUTING.md, "Copies of game files").
- **Licences:** contributions are under PolyForm Shield. The `scenery` crate is MIT. Don't paste
  in code you can't license that way.
- **Comments and commit messages:** plain prose explaining why, in the voice of the code around
  them.

## Where things are

- `src/frame.rs`: builds every frame (`Launcher::frame`): rail, header, page and the layers
  above them.
- `src/backend.rs`, `src/backend/`: installs, preparing and launching the game, releases,
  self-update and the data folder, shared by window, CLI and MCP.
- `src/cli.rs`, `src/cli/`, `src/cli_tools.rs`, `src/mcp.rs`: the command line (one handler per
  command under `cli/`) and the MCP tools generated from it.
- `src/mods/`, `src/browser/`: the Mods page and the mod browser.
- `src/servers_page/`: Servers and hosting.
- `src/motion.rs`, `crates/theme`: motion, curves and design tokens.
- `scenery/`: the backdrop (sky, rain, blur); MIT.
- `crates/`: shared crates split out of the game repository (see CONTRIBUTING.md, "History").
- `tools/`: release, CI and test helpers. `tools/ci/` is what the workflows call.
