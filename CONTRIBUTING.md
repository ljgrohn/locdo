# Contributing to locdo

Thanks for your interest! locdo is intentionally small, and contributions that
keep it that way are the most likely to land.

## Ground rules

- **The markdown file is the only state.** Features must round-trip through
  plain markdown that a human or an agent could have written by hand. No
  sidecar files, no databases, no proprietary syntax beyond what's in the
  README (`- [ ]` checklists, `@done(...)` stamps, `Later`/`Done` headings).
- **Stay a single file.** `src/main.rs` should remain readable top to bottom.
  If your change genuinely needs a module split, propose it in an issue first.
- **Don't lose user data.** Lines locdo doesn't understand are preserved, not
  dropped. Any change to parsing or the tidy pass needs tests proving unknown
  content survives a round-trip.

## Workflow

1. Fork and branch from `main`.
2. `cargo test` — add tests for parser/file-manipulation changes (the existing
   tests in `main.rs` show the pattern; they run against in-memory line
   vectors, no terminal needed).
3. `cargo fmt` and `cargo clippy -- -D warnings` — CI enforces both.
4. Open a PR with a short description of the behavior change. A before/after
   snippet of the markdown file is worth a thousand words.

For anything bigger than a bug fix, opening an issue first will save you time.

## Ideas that would fit

- In-app item creation (`a` to add)
- Archiving Done to a separate file
- Due dates (`@due(...)`) with sort/highlight
- Multiple named lists / quick switcher

## Ideas that probably won't

- Sync services, accounts, or servers
- Config files with more than a handful of options
- Anything that makes the markdown unreadable in a plain editor
