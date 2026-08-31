# locdo

A terminal todo app that is a thin wrapper around a markdown file.

The file is the single source of truth: edit it by hand, let your AI agents
append to it, or drive it from the TUI — locdo picks up external changes live
and writes plain, tidy markdown back. No database, no sync, no lock-in; if you
stop using locdo tomorrow, you still have a readable `todo.md`.

```
┌ todo.md — 3/7 done ────────────────────────────────────┐
│ Todo                                                   │
│ [ ] Ship the release              1/3                  │
│ [ ] Write announcement post        ≡                   │
│                                                        │
│ Later                                                  │
│ [ ] Refactor the parser                                │
│                                                        │
│ Done                                                   │
│ [x] Fix reorder bug                     Aug 30, 14:32  │
└────────────────────────────────────────────────────────┘
```

## Why markdown?

Checklist markdown (`- [ ]` / `- [x]`) is the one todo format every human,
editor, and LLM agent already understands. Agents add items by appending a
line; you review them in the TUI. File order is display order, so priority is
just position.

## Install

Requires a [Rust toolchain](https://rustup.rs).

```sh
git clone https://github.com/ljgrohn/locdo
cd locdo
cargo install --path .
```

Then, from anywhere:

```sh
locdo                 # opens ./todo.md, created if missing
locdo path/to/list.md # or point it at a specific file
```

Works on Windows, macOS, and Linux (crossterm + ratatui).

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` (or arrows) | move cursor |
| `space` / `enter` | toggle done (moves item to/from the Done section) |
| `tab` | open the notes/sub-todo editor for the item |
| `shift+tab` / `esc` | save notes and return to the list |
| `J` / `K` (Shift+arrows) | move item up/down |
| `l` | move item to Later, or back to Todo if it's already there |
| `h` | hide/show the Done section |
| `g` / `G` | jump to top / bottom |
| `r` | reload from disk (also happens automatically) |
| `q` / `esc` | quit |

## Behavior

- **Completing an item** stamps it with the local time (` @done(2026-08-30 14:32)`)
  and moves it to the top of a `## Done` section (created if missing), so Done
  is always newest-first. The stamp shows dim and right-aligned in the UI.
  Unchecking strips the stamp and sends the item back to the end of Todo.
- **Notes and sub-todos** live as indented lines under an item. The main list
  shows only top-level items, with a dim `1/3` sub-todo count and `≡` marker
  for notes; `tab` opens a free-form editor for the item's block. Reordering
  and section moves carry the whole block along.
- **External edits** (your editor, an agent, a script) are detected by polling
  the file's modified time every 250ms and reloaded in place. The app holds no
  unsaved state — every action writes through to the file immediately.

## File format

Standard markdown checklists. Everything locdo writes, it also reads.

```markdown
# Todo

- [ ] an open item
  - [ ] a sub-todo
  free-form note text, indented under its item

## Later

- [ ] parked items live here (toggle with `l`)

## Done

- [x] a finished item @done(2026-08-30 14:32)
```

Section headings named `Later` and `Done` (any heading level, case-insensitive)
are special; everything else is treated as part of Todo. Lines that aren't
tasks or headings are preserved untouched, so notes and structure survive
round-trips.

## Contributing

Issues and PRs are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). It's a
single-file app (`src/main.rs`) on purpose; it should stay easy to read top to
bottom. Forks that take it somewhere else entirely are equally welcome.

## License

[MIT](LICENSE)
