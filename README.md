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
export LOCDO_FILE=~/todos/todo.md   # default file when no argument is given
```

Works on Windows, macOS, and Linux (crossterm + ratatui).

## Keep your todos in a private repo (multi-machine sync)

Your actual todo list doesn't belong in this (or any public) repo. Put it in
a small **private** GitHub repo instead, and locdo will sync it for you:

```sh
# one-time setup
gh repo create my-todos --private --clone   # or create + clone however you like
cd my-todos && touch todo.md && git add todo.md && git commit -m init && git push -u origin HEAD
echo 'export LOCDO_FILE=~/my-todos/todo.md' >> ~/.zshrc
```

Sync is **on by construction, off by default**: if the todo file's directory
is a git repo with a remote, locdo pulls (`--rebase --autostash`) on open,
commits and pushes the todo file and its sidecars on exit, and `S` syncs
mid-session. If the file isn't in a repo, locdo behaves exactly as before.
Everything is best-effort — offline or auth failures show a warning and
never block the app. On a second machine, clone the same repo, set
`LOCDO_FILE`, and your list (plus archive and done history) follows you.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` (or arrows) | move cursor (walks into a task's subtasks) |
| `space` / `enter` | toggle done (subtasks sink within their block; tasks move to Done) |
| `n` | add a new todo to the inbox |
| `s` | add a subtask under the current task (one level deep) |
| `e` | edit the current task/subtask title |
| `X` | delete the current task (with its block) or subtask |
| `m` | move task to a section via popup (also creates new sections) |
| `c` | collapse/expand the current section |
| `A` | archive the selected collapsed section to `<name>.archive.md` |
| `O` | expand/collapse all subtasks |
| `tab` | open the notes/sub-todo editor for the item |
| `shift+tab` / `esc` | save notes and return to the list |
| `J` / `K` (Shift+arrows) | move item up/down |
| `l` | move item to Later, or back to Todo if it's already there |
| `h` | hide/show the Done section |
| `D` | view done history (`<name>.done.md`, read-only) |
| `S` | git sync now (when the file lives in a repo with a remote) |
| `ctrl+z` | undo the last change |
| `~` | show all keybinds |
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
- **Done rotation**: at startup, Done items stamped more than 7 days ago move
  to a `<name>.done.md` sidecar, grouped by month, so the Done section only
  shows the last week. `D` browses the full history read-only.
- **Section archive**: `A` on a collapsed section appends it to
  `<name>.archive.md` and removes it from the main file.

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
