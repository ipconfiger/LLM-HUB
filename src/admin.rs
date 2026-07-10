//! Interactive TUI configuration editor for `llm-hub`.
//!
//! Presents the loaded [`Config`] as a navigable, editable terminal UI:
//! a list of backends on the left and a field editor (name, base_url, keys,
//! models) on the right. Edits are committed back to the config and saved to
//! disk on demand or automatically on quit.
//!
//! A dedicated `f` key auto-fetches the selected backend's model list from its
//! `/v1/models` (falling back to `/models`) endpoint and populates the
//! `models` field. The fetch runs on a spawned task so the UI stays
//! responsive while the request is in flight.
//!
//! The terminal is always restored on exit — even on error or panic — via an
//! RAII [`TerminalGuard`] whose [`Drop`] impl calls [`ratatui::restore`].

use crate::config::{Backend, Config};
use crate::error::{self, Error};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};

use futures_util::StreamExt;
use tokio::sync::mpsc;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

/// Field indices within the per-backend editor.
struct Field;
impl Field {
    const NAME: usize = 0;
    const URL: usize = 1;
    const KEYS: usize = 2;
    const MODELS: usize = 3;
    const N: usize = 4;
    /// Human-facing labels for the four editable fields.
    const LABELS: [&'static str; Self::N] =
        ["name", "base_url", "keys  (逗号分隔)", "models  (逗号分隔)"];
}

/// Outcome of an in-flight model-list fetch, tagged with the backend index it
/// was issued for so stale results can be discarded after navigation.
type FetchOutcome = (usize, std::result::Result<Vec<String>, String>);

/// Run the interactive configuration editor.
///
/// Loads the config via [`Config::load`]. On quit, saves automatically if any
/// unsaved changes exist. Returns `Ok(())` on a clean quit.
///
/// The event loop is fully async, driven by [`tokio::select!`] over crossterm's
/// [`EventStream`] (terminal input) and an mpsc channel (fetch results). This
/// keeps the UI responsive while HTTP fetches run on spawned tasks — unlike a
/// blocking `read()`, an `EventStream` future can be polled concurrently.
pub async fn run() -> error::Result<()> {
    let mut terminal = ratatui::init();
    // Restore the terminal no matter how we leave this function.
    let _guard = TerminalGuard;

    let mut app = App::from_config(Config::load()?);

    // A shared HTTP client (rustls is the configured TLS backend). Cloning a
    // `reqwest::Client` is cheap (it is `Arc` internally) and shares the
    // connection pool across fetch tasks.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;

    let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchOutcome>(8);
    let mut events = EventStream::new();

    loop {
        terminal.draw(|frame| app.draw(frame))?;

        tokio::select! {
            // Terminal input (keys, resize, …).
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(ev)) => {
                    if !app.handle_event(ev, &client, &fetch_tx) {
                        break;
                    }
                }
                // Propagate the I/O failure with context rather than bare.
                Some(Err(e)) => {
                    return Err(Error::Other(format!("terminal event stream error: {e}")));
                }
                None => break,
            },
            // A fetch task reported back.
            Some(outcome) = fetch_rx.recv() => {
                app.apply_fetch(outcome);
            }
        }
    }

    Ok(())
}

/// RAII guard that restores the terminal on drop.
///
/// Created right after [`ratatui::init`]; ensures raw mode / the alternate
/// screen are always left on any exit path (normal return, error, or panic).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// A single-line, Unicode-safe text editor with a char-based cursor.
#[derive(Clone, Default)]
struct Editor {
    text: String,
    /// Cursor position as a count of `char`s from the start.
    cursor: usize,
}

impl Editor {
    fn from_str(s: &str) -> Self {
        Self {
            text: s.to_string(),
            cursor: s.chars().count(),
        }
    }

    fn insert(&mut self, c: char) {
        let mut chars: Vec<char> = self.text.chars().collect();
        let idx = self.cursor.min(chars.len());
        chars.insert(idx, c);
        self.text = chars.into_iter().collect();
        self.cursor = idx + 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut chars: Vec<char> = self.text.chars().collect();
        chars.remove(self.cursor - 1);
        self.text = chars.into_iter().collect();
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        let max = self.text.chars().count();
        if self.cursor >= max {
            return;
        }
        let mut chars: Vec<char> = self.text.chars().collect();
        chars.remove(self.cursor);
        self.text = chars.into_iter().collect();
    }

    fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn right(&mut self) {
        if self.cursor < self.text.chars().count() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }
}

/// Mutable application state for the editor.
struct App {
    config: Config,
    /// True if the in-memory config differs from what's on disk.
    dirty: bool,
    /// Index of the selected backend in `config.backends`.
    selected: usize,
    /// True while editing a backend's fields.
    editing: bool,
    /// Active field index while editing (see [`Field`]).
    field: usize,
    /// Working copies of the selected backend's fields (always reflects the
    /// current selection; modified live while editing).
    edits: [Editor; Field::N],
    /// Snapshot of the backend as it was when editing started, used to detect
    /// whether a commit actually changed anything.
    snapshot: Option<Backend>,
    /// Index of the backend a fetch is currently in flight for, if any.
    pending_fetch_for: Option<usize>,
    /// Transient status message shown in the help bar.
    status: String,
    /// Set to true to break the event loop.
    quit: bool,
}

impl App {
    fn from_config(mut config: Config) -> Self {
        // Seed an empty backend if none exist, so the user isn't stuck with an
        // uneditable screen. This is a UI convenience only and is *not* marked
        // dirty: quitting without edits will not write a file.
        if config.backends.is_empty() {
            config.backends.push(Backend {
                name: String::new(),
                base_url: String::new(),
                keys: Vec::new(),
                models: Vec::new(),
            });
        }

        let mut app = Self {
            config,
            dirty: false,
            selected: 0,
            editing: false,
            field: Field::NAME,
            edits: Default::default(),
            snapshot: None,
            pending_fetch_for: None,
            status: String::new(),
            quit: false,
        };
        app.sync_edits();
        app
    }

    /// Copy the selected backend's values into the working `edits` buffers.
    fn sync_edits(&mut self) {
        match self.config.backends.get(self.selected) {
            Some(b) => {
                self.edits[Field::NAME] = Editor::from_str(&b.name);
                self.edits[Field::URL] = Editor::from_str(&b.base_url);
                self.edits[Field::KEYS] = Editor::from_str(&to_csv(&b.keys));
                self.edits[Field::MODELS] = Editor::from_str(&to_csv(&b.models));
            }
            None => {
                for e in self.edits.iter_mut() {
                    *e = Editor::default();
                }
            }
        }
    }

    /// Dispatch one terminal event. Returns `false` to request a quit.
    fn handle_event(
        &mut self,
        event: Event,
        client: &reqwest::Client,
        fetch_tx: &mpsc::Sender<FetchOutcome>,
    ) -> bool {
        if let Event::Key(key) = event {
            // Ctrl+C quits from anywhere.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c'))
            {
                self.quit();
                return false;
            }
            // 'f' = fetch models. Browse mode only: in edit mode 'f' must remain
            // a normal character so that base_url / keys text containing 'f'
            // (e.g. "https://api.siliconflow.cn") still types correctly.
            if !self.editing && matches!(key.code, KeyCode::Char('f')) {
                self.request_fetch(client, fetch_tx);
                return true;
            }
            if self.editing {
                self.handle_edit_key(key);
            } else {
                self.handle_browse_key(key);
            }
        }
        // Resize / mouse / focus / paste events are ignored; the next redraw
        // handles them naturally.
        !self.quit
    }

    fn handle_browse_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Enter => self.begin_edit(),
            KeyCode::Char('a') => self.add_backend(),
            KeyCode::Char('d') => self.delete_backend(),
            KeyCode::Char('s') => self.save(),
            KeyCode::Char('q') => self.quit(),
            _ => {}
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.commit_edit(),
            KeyCode::Tab => self.cycle_field(1),
            KeyCode::BackTab => self.cycle_field(-1),
            KeyCode::Up => self.cycle_field(-1),
            KeyCode::Down => self.cycle_field(1),
            KeyCode::Backspace => self.current_editor_mut().backspace(),
            KeyCode::Delete => self.current_editor_mut().delete(),
            KeyCode::Left => self.current_editor_mut().left(),
            KeyCode::Right => self.current_editor_mut().right(),
            KeyCode::Home => self.current_editor_mut().home(),
            KeyCode::End => self.current_editor_mut().end(),
            KeyCode::Char(c) => self.current_editor_mut().insert(c),
            _ => {}
        }
    }

    fn current_editor_mut(&mut self) -> &mut Editor {
        let idx = self.field.min(Field::N - 1);
        &mut self.edits[idx]
    }

    fn move_selection(&mut self, delta: i32) {
        let n = self.config.backends.len();
        if n == 0 {
            return;
        }
        let mut next = self.selected as i32 + delta;
        if next < 0 {
            next = 0;
        }
        if next as usize >= n {
            next = (n - 1) as i32;
        }
        let next = next as usize;
        if next != self.selected {
            self.selected = next;
            self.sync_edits();
        }
    }

    fn begin_edit(&mut self) {
        if self.config.backends.get(self.selected).is_none() {
            return;
        }
        self.sync_edits();
        self.snapshot = self.config.backends.get(self.selected).cloned();
        self.field = Field::NAME;
        self.editing = true;
        self.status.clear();
    }

    fn commit_edit(&mut self) {
        let new_backend = Backend {
            name: self.edits[Field::NAME].text.trim().to_string(),
            base_url: self.edits[Field::URL].text.trim().to_string(),
            keys: from_csv(&self.edits[Field::KEYS].text),
            models: from_csv(&self.edits[Field::MODELS].text),
        };

        let changed = self
            .snapshot
            .as_ref()
            .is_none_or(|old| !backend_eq(old, &new_backend));

        if changed {
            if let Some(slot) = self.config.backends.get_mut(self.selected) {
                *slot = new_backend;
            }
            self.dirty = true;
            self.status = "已应用".to_string();
        }

        self.editing = false;
        self.snapshot = None;
        // Refresh the working buffers so the (possibly trimmed) committed
        // values are what's displayed.
        self.sync_edits();
    }

    fn cycle_field(&mut self, delta: i32) {
        let n = Field::N as i32;
        let raw = self.field as i32 + delta;
        // Wrap around within [0, n).
        self.field = (((raw % n) + n) % n) as usize;
    }

    fn add_backend(&mut self) {
        self.config.backends.push(Backend {
            name: String::new(),
            base_url: String::new(),
            keys: Vec::new(),
            models: Vec::new(),
        });
        self.selected = self.config.backends.len() - 1;
        self.dirty = true;
        self.status = "已新增后端，请编辑".to_string();
        self.begin_edit();
    }

    fn delete_backend(&mut self) {
        if self.config.backends.is_empty() {
            return;
        }
        let idx = self.selected.min(self.config.backends.len() - 1);
        self.config.backends.remove(idx);
        if self.config.backends.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.config.backends.len() {
            self.selected = self.config.backends.len() - 1;
        }
        self.dirty = true;
        self.status = format!("已删除后端 #{idx}");
        self.sync_edits();
    }

    fn save(&mut self) {
        match self.config.save() {
            Ok(()) => {
                self.dirty = false;
                self.status = "已保存".to_string();
            }
            Err(err) => {
                // Surface the error in the status bar rather than aborting.
                self.status = format!("保存失败: {err}");
            }
        }
    }

    fn quit(&mut self) {
        // Auto-save on quit if there are unsaved changes.
        if self.dirty && self.config.save().is_ok() {
            self.dirty = false;
        }
        self.quit = true;
    }

    /// Kick off a model-list fetch for the selected backend (browse mode).
    ///
    /// Requires a non-empty `base_url` and at least one non-empty key; otherwise
    /// shows a hint and does nothing. The actual HTTP work happens on a spawned
    /// task so the UI keeps redrawing.
    fn request_fetch(&mut self, client: &reqwest::Client, fetch_tx: &mpsc::Sender<FetchOutcome>) {
        let backend_index = self.selected;

        // Extract owned copies so the borrow of `self.config` ends before we
        // mutate `self.status` / `self.pending_fetch_for` below.
        let (base, first_key) = match self.config.backends.get(backend_index) {
            Some(b) => {
                let base = b.base_url.trim().trim_end_matches('/').to_string();
                let first_key = b
                    .keys
                    .iter()
                    .map(|k| k.trim())
                    .find(|k| !k.is_empty())
                    .map(str::to_string);
                (base, first_key)
            }
            None => (String::new(), None),
        };

        if base.is_empty() {
            self.status = "请先填写 base_url 和 key".to_string();
            return;
        }
        let Some(key) = first_key else {
            self.status = "请先填写 base_url 和 key".to_string();
            return;
        };

        let client = client.clone();
        let tx = fetch_tx.clone();
        self.status = "正在获取模型列表…".to_string();
        self.pending_fetch_for = Some(backend_index);
        tokio::spawn(async move {
            let res = fetch_model_list(&client, &base, &key).await;
            // Sending can only fail if the receiver was dropped (app exited),
            // in which case there is nothing useful to do.
            let _ = tx.send((backend_index, res)).await;
        });
    }

    /// Apply a completed fetch outcome, if still relevant.
    ///
    /// Stale results (user navigated away, or the fetch was superseded) are
    /// discarded. On failure the existing model list is left untouched.
    fn apply_fetch(&mut self, outcome: FetchOutcome) {
        let (idx, res) = outcome;

        // Only honor this if it is still the pending fetch for that backend.
        if self.pending_fetch_for.take() != Some(idx) {
            return;
        }
        // Ignore if the user navigated away from the fetched backend.
        if idx != self.selected {
            return;
        }

        match res {
            Ok(models) => {
                let count = models.len();
                // Write straight into the backend (source of truth) so the
                // result survives even a quit-without-commit while editing.
                if let Some(b) = self.config.backends.get_mut(idx) {
                    b.models = models;
                }
                if self.editing {
                    // Reflect it in the live editor buffer too.
                    let csv = self
                        .config
                        .backends
                        .get(idx)
                        .map_or(String::new(), |b| to_csv(&b.models));
                    self.edits[Field::MODELS] = Editor::from_str(&csv);
                } else {
                    self.sync_edits();
                }
                self.dirty = true;
                self.status = if count == 0 {
                    "未获取到模型（响应中无 data.id）".to_string()
                } else {
                    format!("已获取 {count} 个模型")
                };
            }
            Err(reason) => {
                self.status = format!("获取失败: {reason}");
            }
        }
    }

    // ----- rendering -------------------------------------------------------

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_title(frame, chunks[0]);
        self.draw_body(frame, chunks[1]);
        self.draw_help(frame, chunks[2]);
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect) {
        let path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<未知配置路径>".to_string());
        let dirty_mark = if self.dirty { " *" } else { "" };

        let line = Line::from(vec![
            Span::styled(
                format!(" llm-hub — 配置编辑器{dirty_mark} "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(path, Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);

        self.draw_backend_list(frame, cols[0]);
        self.draw_editor(frame, cols[1]);
    }

    fn draw_backend_list(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("后端列表  (Backends)");

        let items: Vec<ListItem> = self
            .config
            .backends
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let name = if b.name.trim().is_empty() {
                    "(未命名)".to_string()
                } else {
                    b.name.clone()
                };
                let url = if b.base_url.trim().is_empty() {
                    "(无 URL)".to_string()
                } else {
                    b.base_url.clone()
                };
                ListItem::new(format!("{i}. {name}\n   {url}"))
            })
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");

        let mut state = ListState::default();
        if !self.config.backends.is_empty() {
            state.select(Some(self.selected.min(self.config.backends.len() - 1)));
        }
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_editor(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("字段编辑  (Editor)");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Four bordered single-line inputs, stacked vertically.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(inner);

        let indices = [Field::NAME, Field::URL, Field::KEYS, Field::MODELS];
        for (row_idx, &field_idx) in indices.iter().enumerate() {
            self.draw_field(frame, rows[row_idx], field_idx);
        }

        // Place the text cursor on the active field while editing.
        if self.editing {
            let fi = self.field.min(Field::N - 1);
            let ed = &self.edits[fi];
            let row = rows[fi];
            // Inside a bordered block, the content line sits at y+1, x+1.
            let inner_x = row.x.saturating_add(1);
            let inner_y = row.y.saturating_add(1);
            let before: String = ed.text.chars().take(ed.cursor).collect();
            let col = display_width(&before);
            let col_u16 = if col > u16::MAX as usize {
                u16::MAX
            } else {
                col as u16
            };
            let max_cx = row.x.saturating_add(row.width.saturating_sub(2));
            let cx = inner_x.saturating_add(col_u16).min(max_cx);
            frame.set_cursor_position((cx, inner_y));
        }
    }

    fn draw_field(&self, frame: &mut Frame, area: Rect, field_idx: usize) {
        let active = self.editing && self.field == field_idx;
        let border_style = if active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Field::LABELS[field_idx])
            .border_style(border_style);

        let text = self.edits[field_idx].text.as_str();
        let paragraph = if text.is_empty() {
            let placeholder = match field_idx {
                Field::NAME => "名称…",
                Field::URL => "https://api.example.com",
                Field::KEYS => "sk-key-1, sk-key-2",
                _ => "model-a, model-b",
            };
            Paragraph::new(placeholder).style(Style::default().fg(Color::DarkGray))
        } else {
            Paragraph::new(text)
        };
        frame.render_widget(paragraph.block(block), area);
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let hint = if self.editing {
            " Esc/Enter 提交  Tab/↑↓ 切换字段  ←→ Home/End 移动光标  Backspace/Delete 删除"
        } else {
            " ↑↓ 选择后端  Enter 编辑字段  a 新增  d 删除  s 保存  f 获取模型  q 退出"
        };
        let status = if self.status.is_empty() {
            String::new()
        } else {
            format!("[{}]  ", self.status)
        };
        let dirty = if self.dirty { " ● 未保存" } else { "" };

        let line = Line::from(vec![
            Span::styled(status, Style::default().fg(Color::Yellow)),
            Span::raw(hint),
            Span::styled(dirty, Style::default().fg(Color::Magenta)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

// ----- helpers -----------------------------------------------------------

/// Join a list of strings into a single comma-separated line for display.
fn to_csv(items: &[String]) -> String {
    items.join(", ")
}

/// Split a comma-separated line back into trimmed, non-empty strings.
fn from_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Field-wise equality for [`Backend`] (which doesn't derive `PartialEq`).
fn backend_eq(a: &Backend, b: &Backend) -> bool {
    a.name == b.name && a.base_url == b.base_url && a.keys == b.keys && a.models == b.models
}

/// Approximate monospace display width of a string, counting wide (CJK /
/// fullwidth / emoji) characters as 2 columns.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

/// Heuristic wide-character test covering common CJK, Hangul, fullwidth, and
/// emoji ranges — good enough for cursor placement without a unicode-width dep.
fn is_wide(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD
    )
}

// ----- model-list fetching -----------------------------------------------

/// Parse an OpenAI-style `/v1/models` JSON body into the list of model ids.
///
/// Accepts a JSON object of the shape `{"data":[{"id":"..."}, ...]}`. Ids are
/// collected in order, de-duplicated (first occurrence kept), and empty ids are
/// dropped. Any malformed JSON, missing `data`, or non-array `data` yields an
/// empty `Vec` (never an error).
fn parse_model_ids(body: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(data) = value.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in data {
        let Some(id) = item.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let owned = id.to_string();
        if seen.insert(owned.clone()) {
            out.push(owned);
        }
    }
    out
}

/// Fetch the model list for a backend.
///
/// Tries `{base_url}/v1/models` first, falling back to `{base_url}/models` if
/// the first attempt fails (transport error or non-2xx status). The first 2xx
/// response is parsed with [`parse_model_ids`] and returned — including an
/// empty list if the body contained no `data.id` entries. If both URLs fail,
/// returns `Err` carrying a short, human-readable reason.
async fn fetch_model_list(
    client: &reqwest::Client,
    base_url: &str,
    key: &str,
) -> std::result::Result<Vec<String>, String> {
    let base = base_url.trim_end_matches('/');
    let mut last_reason = String::from("两个端点均不可用");

    for path in ["/v1/models", "/models"] {
        let url = format!("{base}{path}");
        match client.get(&url).bearer_auth(key).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(body) => return Ok(parse_model_ids(&body)),
                Err(e) => {
                    tracing::warn!("models body read failed for {url}: {e}");
                    last_reason = e.to_string();
                }
            },
            Ok(resp) => {
                tracing::debug!("models request {url} -> HTTP {}", resp.status());
                last_reason = format!("HTTP {}", resp.status());
            }
            Err(e) => {
                tracing::debug!("models request {url} failed: {e}");
                last_reason = e.to_string();
            }
        }
    }

    Err(last_reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn parse_model_ids_dedupes_and_drops_empty() {
        let body = r#"{"object":"list","data":[{"id":"gpt-4o"},{"id":"gpt-4o"},{"id":""},{"id":"text-embedding-3-small"}]}"#;
        let ids = parse_model_ids(body);
        assert_eq!(
            ids,
            vec!["gpt-4o".to_string(), "text-embedding-3-small".to_string()]
        );
    }

    #[test]
    fn parse_model_ids_malformed_json_is_empty() {
        assert_eq!(parse_model_ids("not json"), Vec::<String>::new());
        assert_eq!(parse_model_ids("{ broken"), Vec::<String>::new());
        assert_eq!(parse_model_ids(""), Vec::<String>::new());
    }

    #[test]
    fn parse_model_ids_missing_data_is_empty() {
        assert_eq!(
            parse_model_ids(r#"{"object":"list"}"#),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_model_ids(r#"{"data":"not-an-array"}"#),
            Vec::<String>::new()
        );
        assert_eq!(parse_model_ids(r#"{"data":[]}"#), Vec::<String>::new());
    }

    #[test]
    fn parse_model_ids_keeps_first_occurrence_order() {
        let body = r#"{"data":[{"id":"b"},{"id":"a"},{"id":"b"},{"id":"c"},{"id":"a"}]}"#;
        assert_eq!(
            parse_model_ids(body),
            vec!["b".to_string(), "a".to_string(), "c".to_string()]
        );
    }

    /// Spawn a tiny single-threaded HTTP server on a free port. For each
    /// connection it reads the request line and replies 200 + `body` when the
    /// path equals `ok_path`, otherwise 404. Returns the base URL.
    fn spawn_mock(ok_path: &str, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ok_path = ok_path.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            // Serve a few connections so the fallback path works.
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { continue };
                // Read until the end of the request headers.
                let mut buf = [0u8; 1024];
                let mut req = Vec::new();
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 || req.len() > 8192 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req_text = String::from_utf8_lossy(&req);
                let path = req_text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let resp = if path == ok_path {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn fetch_model_list_succeeds_on_v1_models() {
        let body = r#"{"object":"list","data":[{"id":"gpt-4o"},{"id":"claude-3-5-sonnet"}]}"#;
        let base = spawn_mock("/v1/models", body);
        let client = reqwest::Client::new();
        let res = fetch_model_list(&client, &base, "test-key").await;
        assert!(res.is_ok(), "expected Ok, got {:?}", res);
        assert_eq!(
            res.unwrap(),
            vec!["gpt-4o".to_string(), "claude-3-5-sonnet".to_string()]
        );
    }

    #[tokio::test]
    async fn fetch_model_list_falls_back_to_models() {
        // /v1/models is not the `ok_path`, so it 404s; /models returns the JSON.
        let body = r#"{"data":[{"id":"llama-3.1-70b"},{"id":"qwen2.5-72b"}]}"#;
        let base = spawn_mock("/models", body);
        let client = reqwest::Client::new();
        let res = fetch_model_list(&client, &base, "k").await;
        assert!(res.is_ok(), "expected Ok, got {:?}", res);
        assert_eq!(
            res.unwrap(),
            vec!["llama-3.1-70b".to_string(), "qwen2.5-72b".to_string()]
        );
    }

    #[tokio::test]
    async fn fetch_model_list_both_fail_is_err() {
        // Neither path matches → both 404.
        let base = spawn_mock("/never", "{}");
        let client = reqwest::Client::new();
        let res = fetch_model_list(&client, &base, "k").await;
        assert!(res.is_err(), "expected Err, got {:?}", res);
        assert!(
            res.unwrap_err().contains("HTTP"),
            "reason should mention HTTP"
        );
    }
}
