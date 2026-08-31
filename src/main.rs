use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{Local, NaiveDateTime};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding};
use tui_textarea::TextArea;

const STARTER: &str = "# Todo\n\n- [ ] Add your first item\n\n## Later\n\n## Done\n";

fn main() -> io::Result<()> {
    let path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("todo.md"));

    if !path.exists() {
        fs::write(&path, STARTER)?;
    }

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let result = run(&path);
    io::stdout().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;
    result
}

/// A top-level task and the indented block (notes, sub-todos) beneath it.
#[derive(Clone, Copy)]
struct Item {
    start: usize,
    len: usize,
    done: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SectionKind {
    Todo,
    Later,
    Done,
}

struct NotesState {
    start: usize,
    child_len: usize,
    indent: usize,
    title: String,
    textarea: TextArea<'static>,
}

/// Single-line prompt for adding a new todo or editing an existing title.
struct InputState {
    editing: bool,
    textarea: TextArea<'static>,
}

struct App {
    path: PathBuf,
    lines: Vec<String>,
    crlf: bool,
    mtime: Option<SystemTime>,
    cursor: usize,
    hide_done: bool,
    status: String,
    notes: Option<NotesState>,
    input: Option<InputState>,
}

/// A renderable row in the main list.
enum Row {
    Header(String),
    Task {
        text: String,
        done: bool,
        stamp: Option<String>,
        subs: Option<(usize, usize)>,
        has_notes: bool,
    },
}

fn leading_ws(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_heading(line: &str) -> bool {
    line.starts_with('#')
}

fn section_kind(heading_text: &str) -> SectionKind {
    match heading_text.to_lowercase().as_str() {
        "later" => SectionKind::Later,
        "done" => SectionKind::Done,
        _ => SectionKind::Todo,
    }
}

/// If `line` is a task item, returns (done, byte index of the status char inside `[ ]`).
fn task_info(line: &str) -> Option<(bool, usize)> {
    let indent = leading_ws(line);
    let rest = &line[indent..];
    let rest = rest
        .strip_prefix("- [")
        .or_else(|| rest.strip_prefix("* ["))?;
    let mut chars = rest.chars();
    let status = chars.next()?;
    if chars.next() != Some(']') {
        return None;
    }
    let done = matches!(status, 'x' | 'X');
    if !done && status != ' ' {
        return None;
    }
    Some((done, indent + 3))
}

/// Splits a task's display text from its `@done(...)` stamp, if any.
fn split_stamp(text: &str) -> (String, Option<String>) {
    if let Some(pos) = text.find("@done(") {
        if let Some(end) = text[pos..].find(')') {
            let stamp = text[pos + 6..pos + end].to_string();
            let mut rest = text[..pos].to_string();
            rest.push_str(&text[pos + end + 1..]);
            return (rest.trim().to_string(), Some(stamp));
        }
    }
    (text.trim().to_string(), None)
}

fn fmt_stamp(stamp: &str) -> String {
    NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M")
        .map(|dt| dt.format("%b %-d, %H:%M").to_string())
        .unwrap_or_else(|_| stamp.to_string())
}

/// Top-level tasks with their child blocks. Interior blank lines belong to a
/// block only when more indented content follows; trailing blanks do not.
fn items(lines: &[String]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((done, _)) = task_info(&lines[i]) {
            if leading_ws(&lines[i]) == 0 {
                let mut j = i + 1;
                loop {
                    let mut k = j;
                    while k < lines.len() && lines[k].trim().is_empty() {
                        k += 1;
                    }
                    if k < lines.len() && !is_heading(&lines[k]) && leading_ws(&lines[k]) > 0 {
                        j = k + 1;
                    } else {
                        break;
                    }
                }
                out.push(Item {
                    start: i,
                    len: j - i,
                    done,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

impl App {
    fn load(path: &Path) -> io::Result<Self> {
        let mut app = App {
            path: path.to_path_buf(),
            lines: Vec::new(),
            crlf: false,
            mtime: None,
            cursor: 0,
            hide_done: false,
            status: String::new(),
            notes: None,
            input: None,
        };
        app.reload()?;
        Ok(app)
    }

    fn reload(&mut self) -> io::Result<()> {
        let raw = fs::read_to_string(&self.path)?;
        self.crlf = raw.contains("\r\n");
        self.lines = raw
            .replace("\r\n", "\n")
            .lines()
            .map(String::from)
            .collect();
        self.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        self.clamp_cursor();
        Ok(())
    }

    fn save(&mut self) -> io::Result<()> {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self.lines.join(eol);
        out.push_str(eol);
        fs::write(&self.path, out)?;
        self.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    fn externally_modified(&self) -> bool {
        let current = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        current.is_some() && current != self.mtime
    }

    fn visible_items(&self) -> Vec<Item> {
        items(&self.lines)
            .into_iter()
            .filter(|it| !(self.hide_done && it.done))
            .collect()
    }

    fn clamp_cursor(&mut self) {
        let n = self.visible_items().len();
        if n == 0 {
            self.cursor = 0;
        } else if self.cursor >= n {
            self.cursor = n - 1;
        }
    }

    fn section_at(&self, line_idx: usize) -> SectionKind {
        let mut kind = SectionKind::Todo;
        for line in self.lines.iter().take(line_idx + 1) {
            if is_heading(line) {
                kind = section_kind(line.trim_start_matches('#').trim());
            }
        }
        kind
    }

    fn heading_line(&self, name: &str) -> Option<usize> {
        self.lines.iter().position(|l| {
            is_heading(l) && l.trim_start_matches('#').trim().eq_ignore_ascii_case(name)
        })
    }

    /// First line index past a heading's blank-line padding.
    fn after_heading(&self, h: usize) -> usize {
        let mut i = h + 1;
        while i < self.lines.len() && self.lines[i].trim().is_empty() {
            i += 1;
        }
        i
    }

    /// End of the Todo region: just before the first Later/Done heading,
    /// backed up over any blank separator lines.
    fn todo_end(&self) -> usize {
        let end = self
            .lines
            .iter()
            .position(|l| {
                is_heading(l) && section_kind(l.trim_start_matches('#').trim()) != SectionKind::Todo
            })
            .unwrap_or(self.lines.len());
        self.back_over_blanks(end)
    }

    fn later_end(&self) -> usize {
        let h = self.heading_line("later").expect("later section exists");
        let end = self.lines[h + 1..]
            .iter()
            .position(|l| is_heading(l))
            .map(|p| h + 1 + p)
            .unwrap_or(self.lines.len());
        self.back_over_blanks(end)
    }

    fn back_over_blanks(&self, mut i: usize) -> usize {
        while i > 0 && self.lines[i - 1].trim().is_empty() {
            i -= 1;
        }
        i
    }

    /// Returns the heading line index, creating the section if missing.
    /// Later is created before Done; Done goes at the end of the file.
    fn ensure_section(&mut self, name: &str) -> usize {
        if let Some(h) = self.heading_line(name) {
            return h;
        }
        let heading = format!("## {name}");
        if name.eq_ignore_ascii_case("later") {
            if let Some(d) = self.heading_line("done") {
                self.lines.insert(d, heading);
                self.lines.insert(d + 1, String::new());
                return d;
            }
        }
        self.lines.push(String::new());
        self.lines.push(heading);
        self.lines.push(String::new());
        self.lines.len() - 2
    }

    fn move_block(&mut self, start: usize, len: usize, mut dest: usize) {
        let block: Vec<String> = self.lines.drain(start..start + len).collect();
        if dest > start {
            dest -= len;
        }
        for (k, line) in block.into_iter().enumerate() {
            self.lines.insert(dest + k, line);
        }
    }

    /// Normalizes blank lines: no runs of blanks, one blank around headings,
    /// no leading/trailing blanks.
    fn tidy(&mut self) {
        let mut out: Vec<String> = Vec::new();
        for line in &self.lines {
            let blank = line.trim().is_empty();
            let prev_blank = out.last().is_none_or(|p: &String| p.trim().is_empty());
            if blank && prev_blank {
                continue;
            }
            if is_heading(line) && !prev_blank {
                out.push(String::new());
            }
            out.push(line.clone());
        }
        let mut i = 0;
        while i < out.len() {
            if is_heading(&out[i]) && i + 1 < out.len() && !out[i + 1].trim().is_empty() {
                out.insert(i + 1, String::new());
            }
            i += 1;
        }
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        self.lines = out;
    }

    fn toggle(&mut self) -> io::Result<()> {
        let vis = self.visible_items();
        let Some(it) = vis.get(self.cursor).copied() else {
            return Ok(());
        };
        let (done, idx) = task_info(&self.lines[it.start]).unwrap();
        if !done {
            let stamp = Local::now().format("%Y-%m-%d %H:%M").to_string();
            {
                let line = &mut self.lines[it.start];
                line.replace_range(idx..idx + 1, "x");
                line.push_str(&format!(" @done({stamp})"));
            }
            let h = self.ensure_section("Done");
            let dest = self.after_heading(h);
            self.move_block(it.start, it.len, dest);
            self.status = "done".to_string();
        } else {
            {
                let line = &mut self.lines[it.start];
                line.replace_range(idx..idx + 1, " ");
                if let Some(pos) = line.find("@done(") {
                    if let Some(end) = line[pos..].find(')') {
                        line.replace_range(pos..pos + end + 1, "");
                    }
                }
                *line = line.trim_end().to_string();
            }
            let dest = self.todo_end();
            self.move_block(it.start, it.len, dest);
            self.status = "back to todo".to_string();
        }
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        Ok(())
    }

    /// Moves the current item Todo -> Later, or Later -> Todo.
    fn cycle_later(&mut self) -> io::Result<()> {
        let vis = self.visible_items();
        let Some(it) = vis.get(self.cursor).copied() else {
            return Ok(());
        };
        if it.done {
            self.status = "item is done — untick it first".to_string();
            return Ok(());
        }
        let dest = if self.section_at(it.start) == SectionKind::Later {
            self.status = "moved to todo".to_string();
            self.todo_end()
        } else {
            self.ensure_section("Later");
            self.status = "moved to later".to_string();
            self.later_end()
        };
        self.move_block(it.start, it.len, dest);
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        Ok(())
    }

    fn move_item(&mut self, delta: isize) -> io::Result<()> {
        let vis = self.visible_items();
        let target = self.cursor as isize + delta;
        if target < 0 || target as usize >= vis.len() {
            return Ok(());
        }
        let cur = vis[self.cursor];
        let tgt = vis[target as usize];
        let dest = if delta < 0 {
            tgt.start
        } else {
            tgt.start + tgt.len
        };
        self.move_block(cur.start, cur.len, dest);
        self.cursor = target as usize;
        self.tidy();
        self.save()
    }

    fn add_todo(&mut self, text: &str) -> io::Result<()> {
        let dest = self.todo_end();
        self.lines.insert(dest, format!("- [ ] {text}"));
        self.tidy();
        self.save()?;
        self.cursor = self
            .visible_items()
            .iter()
            .rposition(|it| self.section_at(it.start) == SectionKind::Todo)
            .unwrap_or(0);
        self.status = "added".to_string();
        Ok(())
    }

    fn delete_current(&mut self) -> io::Result<()> {
        let vis = self.visible_items();
        let Some(it) = vis.get(self.cursor).copied() else {
            return Ok(());
        };
        let (_, idx) = task_info(&self.lines[it.start]).unwrap();
        let (title, _) = split_stamp(&self.lines[it.start][idx + 2..]);
        self.lines.drain(it.start..it.start + it.len);
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        self.status = format!("deleted: {title}");
        Ok(())
    }

    fn edit_title(&mut self, text: &str) -> io::Result<()> {
        let vis = self.visible_items();
        let Some(it) = vis.get(self.cursor).copied() else {
            return Ok(());
        };
        let (_, idx) = task_info(&self.lines[it.start]).unwrap();
        let (_, stamp) = split_stamp(&self.lines[it.start][idx + 2..]);
        let mut line = format!("{} {}", &self.lines[it.start][..idx + 2], text);
        if let Some(st) = stamp {
            line.push_str(&format!(" @done({st})"));
        }
        self.lines[it.start] = line;
        self.save()
    }

    fn open_input(&mut self, editing: bool) {
        let mut textarea = if editing {
            let vis = self.visible_items();
            let Some(it) = vis.get(self.cursor).copied() else {
                return;
            };
            let (_, idx) = task_info(&self.lines[it.start]).unwrap();
            let (title, _) = split_stamp(&self.lines[it.start][idx + 2..]);
            TextArea::from(vec![title])
        } else {
            TextArea::default()
        };
        textarea.set_cursor_line_style(Style::default());
        textarea.move_cursor(tui_textarea::CursorMove::End);
        self.input = Some(InputState { editing, textarea });
    }

    fn commit_input(&mut self) -> io::Result<()> {
        let Some(is) = self.input.take() else {
            return Ok(());
        };
        let text = is.textarea.lines().join(" ").trim().to_string();
        if text.is_empty() {
            self.status = "cancelled".to_string();
            return Ok(());
        }
        if is.editing {
            self.edit_title(&text)?;
            self.status = "edited".to_string();
        } else {
            self.add_todo(&text)?;
        }
        Ok(())
    }

    fn open_notes(&mut self) {
        let vis = self.visible_items();
        let Some(it) = vis.get(self.cursor).copied() else {
            return;
        };
        let children: Vec<String> = self.lines[it.start + 1..it.start + it.len].to_vec();
        let indent = children
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| leading_ws(l))
            .min()
            .unwrap_or(2);
        let content: Vec<String> = children
            .iter()
            .map(|l| {
                let cut = indent.min(leading_ws(l));
                l[cut..].to_string()
            })
            .collect();
        let raw = &self.lines[it.start];
        let (_, idx) = task_info(raw).unwrap();
        let (title, _) = split_stamp(&raw[idx + 2..]);
        let mut textarea = if content.is_empty() {
            TextArea::default()
        } else {
            TextArea::from(content)
        };
        textarea.set_cursor_line_style(Style::default());
        textarea.move_cursor(tui_textarea::CursorMove::Bottom);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        self.notes = Some(NotesState {
            start: it.start,
            child_len: it.len - 1,
            indent,
            title,
            textarea,
        });
    }

    fn close_notes(&mut self) -> io::Result<()> {
        let Some(ns) = self.notes.take() else {
            return Ok(());
        };
        let mut children: Vec<String> = ns
            .textarea
            .lines()
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    format!("{}{}", " ".repeat(ns.indent), l)
                }
            })
            .collect();
        while children.last().is_some_and(|l| l.trim().is_empty()) {
            children.pop();
        }
        self.lines
            .splice(ns.start + 1..ns.start + 1 + ns.child_len, children);
        self.tidy();
        self.save()
    }

    fn rows(&self) -> Vec<Row> {
        let its = items(&self.lines);
        let mut rows = Vec::new();
        let mut it_idx = 0;
        let mut i = 0;
        while i < self.lines.len() {
            let line = &self.lines[i];
            if is_heading(line) {
                let name = line.trim_start_matches('#').trim().to_string();
                if !(self.hide_done && section_kind(&name) == SectionKind::Done) {
                    rows.push(Row::Header(name));
                }
                i += 1;
                continue;
            }
            if it_idx < its.len() && its[it_idx].start == i {
                let it = its[it_idx];
                it_idx += 1;
                i = it.start + it.len;
                if self.hide_done && it.done {
                    continue;
                }
                let src = &self.lines[it.start];
                let (_, idx) = task_info(src).unwrap();
                let (text, stamp) = split_stamp(&src[idx + 2..]);
                let child = &self.lines[it.start + 1..it.start + it.len];
                let mut sub_total = 0;
                let mut sub_done = 0;
                let mut has_notes = false;
                for l in child {
                    if let Some((d, _)) = task_info(l) {
                        sub_total += 1;
                        if d {
                            sub_done += 1;
                        }
                    } else if !l.trim().is_empty() {
                        has_notes = true;
                    }
                }
                rows.push(Row::Task {
                    text,
                    done: it.done,
                    stamp,
                    subs: (sub_total > 0).then_some((sub_done, sub_total)),
                    has_notes,
                });
                continue;
            }
            i += 1;
        }
        rows
    }
}

fn run(path: &Path) -> io::Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::load(path)?;

    loop {
        if app.notes.is_none() && app.input.is_none() && app.externally_modified() {
            app.reload()?;
            app.status = "reloaded (changed on disk)".to_string();
        }

        terminal.draw(|f| draw(f, &app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.notes.is_some() {
            if key.code == KeyCode::BackTab || key.code == KeyCode::Esc {
                app.close_notes()?;
                app.status = "notes saved".to_string();
            } else if let Some(ns) = app.notes.as_mut() {
                ns.textarea.input(key);
            }
            continue;
        }

        if app.input.is_some() {
            match key.code {
                KeyCode::Enter => app.commit_input()?,
                KeyCode::Esc => {
                    app.input = None;
                    app.status = "cancelled".to_string();
                }
                _ => {
                    if let Some(is) = app.input.as_mut() {
                        is.textarea.input(key);
                    }
                }
            }
            continue;
        }

        app.status.clear();
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('j') | KeyCode::Down if !shift => {
                let n = app.visible_items().len();
                if n > 0 && app.cursor < n - 1 {
                    app.cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up if !shift => {
                app.cursor = app.cursor.saturating_sub(1);
            }
            KeyCode::Char('J') | KeyCode::Down => app.move_item(1)?,
            KeyCode::Char('K') | KeyCode::Up => app.move_item(-1)?,
            KeyCode::Char('n') | KeyCode::Char('N') => app.open_input(false),
            KeyCode::Char('e') => app.open_input(true),
            KeyCode::Char('X') => app.delete_current()?,
            KeyCode::Tab => app.open_notes(),
            KeyCode::Char(' ') | KeyCode::Enter => app.toggle()?,
            KeyCode::Char('l') => app.cycle_later()?,
            KeyCode::Char('h') => {
                app.hide_done = !app.hide_done;
                app.clamp_cursor();
            }
            KeyCode::Char('g') => app.cursor = 0,
            KeyCode::Char('G') => {
                app.cursor = app.visible_items().len().saturating_sub(1);
            }
            KeyCode::Char('r') => {
                app.reload()?;
                app.status = "reloaded".to_string();
            }
            _ => {}
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());

    if let Some(ns) = &app.notes {
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(format!(" notes: {} ", ns.title));
        let inner = block.inner(main);
        f.render_widget(block, main);
        f.render_widget(&ns.textarea, inner);
        f.render_widget(
            Line::from(Span::styled(
                " shift+tab save & close · plain lines = notes · \"- [ ] …\" lines = sub-todos",
                Style::new().fg(Color::DarkGray),
            )),
            footer,
        );
        return;
    }

    let inner_width = (main.width as usize).saturating_sub(4);
    let rows = app.rows();
    let mut task_seen = 0usize;
    let mut selected_row = None;
    let items_ui: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(ri, row)| match row {
            Row::Header(text) => ListItem::new(Line::from(Span::styled(
                text.clone(),
                Style::new().fg(Color::Cyan).bold(),
            ))),
            Row::Task {
                text,
                done,
                stamp,
                subs,
                has_notes,
            } => {
                if task_seen == app.cursor {
                    selected_row = Some(ri);
                }
                task_seen += 1;
                let (mark, mark_style, text_style) = if *done {
                    (
                        "[x] ",
                        Style::new().fg(Color::DarkGray),
                        Style::new().fg(Color::DarkGray).crossed_out(),
                    )
                } else {
                    ("[ ] ", Style::new().fg(Color::Green), Style::new())
                };
                let mut spans = vec![
                    Span::styled(mark, mark_style),
                    Span::styled(text.clone(), text_style),
                ];
                let mut left = 4 + text.chars().count();
                if let Some((d, t)) = subs {
                    let s = format!("  {d}/{t}");
                    left += s.chars().count();
                    spans.push(Span::styled(s, Style::new().fg(Color::DarkGray)));
                }
                if *has_notes {
                    spans.push(Span::styled("  ≡", Style::new().fg(Color::DarkGray)));
                    left += 3;
                }
                if let Some(st) = stamp {
                    let disp = fmt_stamp(st);
                    let pad = inner_width
                        .saturating_sub(left + disp.chars().count())
                        .max(1);
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(disp, Style::new().fg(Color::DarkGray).dim()));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let all = items(&app.lines);
    let done = all.iter().filter(|it| it.done).count();
    let title = format!(
        " {} — {done}/{} done{} ",
        app.path.display(),
        all.len(),
        if app.hide_done { " (hiding done)" } else { "" }
    );

    let list = List::new(items_ui)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .padding(Padding::horizontal(1))
                .title(title),
        )
        .highlight_style(Style::new().bg(Color::Rgb(50, 55, 70)));

    let mut state = ListState::default();
    state.select(selected_row);
    f.render_stateful_widget(list, main, &mut state);

    if let Some(is) = &app.input {
        let h = 3.min(main.height);
        let area = Rect::new(main.x, main.y + main.height - h, main.width, h);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(if is.editing { " edit " } else { " new todo " });
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        f.render_widget(&is.textarea, inner);
    }

    let help = if app.input.is_some() {
        " enter save · esc cancel".to_string()
    } else if app.status.is_empty() {
        " j/k move · space done · n new · e edit · X del · J/K reorder · l later · tab notes · h hide done · q quit"
            .to_string()
    } else {
        format!(" {}", app.status)
    };
    f.render_widget(
        Line::from(Span::styled(help, Style::new().fg(Color::DarkGray))),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_from(lines: &[&str]) -> App {
        App {
            path: std::env::temp_dir().join(format!("locdo-test-{}.md", std::process::id())),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            crlf: false,
            mtime: None,
            cursor: 0,
            hide_done: false,
            status: String::new(),
            notes: None,
            input: None,
        }
    }

    #[test]
    fn parses_task_lines() {
        assert_eq!(task_info("- [ ] open"), Some((false, 3)));
        assert_eq!(task_info("- [x] done"), Some((true, 3)));
        assert_eq!(task_info("- [X] done caps"), Some((true, 3)));
        assert_eq!(task_info("  - [ ] nested"), Some((false, 5)));
        assert_eq!(task_info("* [ ] star bullet"), Some((false, 3)));
        assert_eq!(task_info("# heading"), None);
        assert_eq!(task_info("- plain bullet"), None);
        assert_eq!(task_info("- [?] weird status"), None);
        assert_eq!(task_info(""), None);
    }

    #[test]
    fn splits_stamps() {
        assert_eq!(split_stamp("plain task"), ("plain task".into(), None));
        assert_eq!(
            split_stamp("task @done(2026-08-30 14:32)"),
            ("task".into(), Some("2026-08-30 14:32".into()))
        );
    }

    #[test]
    fn items_capture_child_blocks() {
        let lines: Vec<String> = [
            "# Todo",
            "",
            "- [ ] parent",
            "  note line",
            "  - [ ] sub",
            "",
            "- [ ] second",
            "",
            "## Later",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let its = items(&lines);
        assert_eq!(its.len(), 2);
        assert_eq!((its[0].start, its[0].len), (2, 3));
        assert_eq!((its[1].start, its[1].len), (6, 1));
    }

    #[test]
    fn toggle_moves_to_done_top_and_back() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] first",
            "- [ ] second",
            "",
            "## Done",
            "",
            "- [x] older @done(2026-08-29 09:00)",
        ]);
        app.toggle().unwrap();
        let done_h = app.heading_line("done").unwrap();
        let top = &app.lines[app.after_heading(done_h)];
        assert!(top.starts_with("- [x] first @done("), "got: {top}");
        // untoggle it: it is now the second visible item (second, then first)
        app.cursor = 1;
        app.toggle().unwrap();
        let todo_tasks: Vec<_> = app
            .visible_items()
            .iter()
            .filter(|it| !it.done)
            .map(|it| app.lines[it.start].clone())
            .collect();
        assert_eq!(todo_tasks, vec!["- [ ] second", "- [ ] first"]);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn cycle_later_round_trips() {
        let mut app = app_from(&["# Todo", "", "- [ ] a", "- [ ] b", "", "## Later"]);
        app.cycle_later().unwrap();
        assert_eq!(
            app.section_at(app.heading_line("later").unwrap() + 2),
            SectionKind::Later
        );
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[1].start], "- [ ] a");
        assert_eq!(app.section_at(vis[1].start), SectionKind::Later);
        // move it back
        app.cursor = 1;
        app.cycle_later().unwrap();
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[1].start], "- [ ] a");
        assert_eq!(app.section_at(vis[1].start), SectionKind::Todo);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn add_todo_appends_to_todo_section() {
        let mut app = app_from(&["# Todo", "", "- [ ] a", "", "## Later", "", "- [ ] l1"]);
        app.add_todo("new task").unwrap();
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[1].start], "- [ ] new task");
        assert_eq!(app.section_at(vis[1].start), SectionKind::Todo);
        assert_eq!(app.cursor, 1);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn delete_removes_item_with_children() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] parent",
            "  note line",
            "  - [ ] sub",
            "",
            "- [ ] second",
        ]);
        app.delete_current().unwrap();
        assert!(!app.lines.iter().any(|l| l.contains("parent")));
        assert!(!app.lines.iter().any(|l| l.contains("note line")));
        assert!(!app.lines.iter().any(|l| l.contains("sub")));
        let vis = app.visible_items();
        assert_eq!(vis.len(), 1);
        assert_eq!(app.lines[vis[0].start], "- [ ] second");
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn edit_title_preserves_children_and_stamp() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] old title",
            "  note line",
            "",
            "## Done",
            "",
            "- [x] finished @done(2026-08-29 09:00)",
        ]);
        app.edit_title("new title").unwrap();
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[0].start], "- [ ] new title");
        assert_eq!(app.lines[vis[0].start + 1], "  note line");

        app.cursor = 1;
        app.edit_title("renamed").unwrap();
        let vis = app.visible_items();
        assert_eq!(
            app.lines[vis[1].start],
            "- [x] renamed @done(2026-08-29 09:00)"
        );
        let _ = fs::remove_file(&app.path);
    }
}
