// ===========================================================================
// Overlays: what opens over the timeline and takes the keyboard until closed.
//
//   Ctrl+O  model search     fuzzy search across every provider, stars, prices
//   Ctrl+K  command palette  every action, command and agent in one search box
//   Ctrl+G  sessions         search saved chats and reopen one
//   Ctrl+B  processes        commands the agents ran: live output, stop
//   Alt+M   MCP servers      status, tools, reconnect, disable, remove
//   Tab     tool inspector   every tool call's arguments and output
//   F1      keys
// ===========================================================================

use super::view::{fit, relative_time};
use super::*;
use crate::config::providers::{ModelInfo, Vision};
use crate::tools::{mcp, shell};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::Text,
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};

/// Rows PgUp / PgDn move through a list.
const PAGE: isize = 8;
/// Bytes of a command's log shown in the process monitor.
const LOG_TAIL: u64 = 16 * 1024;
/// Commands the palette fills in rather than runs: they need an argument.
const NEEDS_ARGUMENT: &[&str] = &["/msg", "/loops"];
/// Commands the palette offers under a friendlier name.
const NAMED_ACTIONS: &[&str] = &[
    "/model",
    "/provider",
    "/mcp",
    "/processes",
    "/session list",
    "/keys",
    "/copy",
    "/copy chat",
    "/copy code",
    "/agents split",
    "/agents grid",
    "/agents stop",
];

pub(super) enum Overlay {
    Help {
        scroll: u16,
    },
    /// `selected` counts back from the newest tool call.
    Inspector {
        selected: usize,
        scroll: u16,
    },
    Models(Box<ModelPicker>),
    Palette {
        query: String,
        selected: usize,
    },
    Sessions {
        query: String,
        selected: usize,
    },
    Processes {
        selected: usize,
    },
    /// `armed`: the server the next `x` removes.
    Mcp {
        selected: usize,
        armed: Option<String>,
    },
}

// ── Fuzzy search ──────────────────────────────────────────────────────────

fn folded(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// How well one search term matches, and the characters it matched. An exact
/// run of letters beats scattered ones, the start of a word beats the middle
/// of one, and the earlier the better.
fn term_score(text: &[char], term: &[char]) -> Option<(i64, Vec<usize>)> {
    if term.is_empty() || term.len() > text.len() {
        return None;
    }
    let mut best: Option<(i64, usize)> = None;
    for pos in 0..=text.len() - term.len() {
        if text[pos..pos + term.len()] != *term {
            continue;
        }
        let word_start = pos == 0 || !text[pos - 1].is_alphanumeric();
        let mut score = 1000 - pos.min(500) as i64;
        if word_start {
            score += 500;
        }
        if pos == 0 {
            score += 300;
        }
        if term.len() == text.len() {
            score += 2000;
        }
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, pos));
        }
    }
    if let Some((score, pos)) = best {
        return Some((score, (pos..pos + term.len()).collect()));
    }

    // In-order letters: the tightest stretch wins.
    let mut tightest: Option<Vec<usize>> = None;
    for start in 0..text.len() {
        if text[start] != term[0] {
            continue;
        }
        let mut hits = vec![start];
        for (i, &c) in text.iter().enumerate().skip(start + 1) {
            if hits.len() == term.len() {
                break;
            }
            if c == term[hits.len()] {
                hits.push(i);
            }
        }
        if hits.len() == term.len() {
            let span = |h: &[usize]| h[h.len() - 1] - h[0];
            if tightest.as_ref().is_none_or(|t| span(&hits) < span(t)) {
                tightest = Some(hits);
            }
        }
    }
    let hits = tightest?;
    let span = hits[hits.len() - 1] - hits[0];
    Some((400 - span.min(300) as i64, hits))
}

/// Match every word of `query` against `text`. Returns the score and the
/// matched characters, or None if a word doesn't match.
pub(super) fn fuzzy_match(text: &str, query: &str) -> Option<(i64, Vec<usize>)> {
    let text = folded(text);
    let mut total = 0;
    let mut hits = Vec::new();
    for term in query.split_whitespace() {
        let (score, found) = term_score(&text, &folded(term))?;
        total += score;
        hits.extend(found);
    }
    Some((total, hits))
}

/// Spans of `text` with the characters in `hits` picked out.
fn highlighted(text: &str, hits: &[usize], base: Style, hit: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_is_hit = false;
    for (i, ch) in text.chars().enumerate() {
        let is_hit = hits.contains(&i);
        if is_hit != run_is_hit && !run.is_empty() {
            let style = if run_is_hit { hit } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_is_hit = is_hit;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_is_hit { hit } else { base }));
    }
    spans
}

/// Edit a search box. Returns true if the text changed.
fn edit_query(query: &mut String, key: KeyCode, ctrl: bool, alt: bool) -> bool {
    match key {
        KeyCode::Char('u' | 'l') if ctrl && !alt => query.clear(),
        KeyCode::Char('w') if ctrl && !alt => {
            let kept = query.trim_end().rfind(char::is_whitespace);
            query.truncate(kept.map_or(0, |i| i + 1));
        }
        KeyCode::Char(c) if !ctrl || alt => query.push(c),
        KeyCode::Backspace => {
            query.pop();
        }
        _ => return false,
    }
    true
}

/// Move a list selection; Up and Down wrap around, paging stops at the ends.
fn move_selection(selected: &mut usize, key: KeyCode, len: usize) -> bool {
    if len == 0 {
        return matches!(key, KeyCode::Up | KeyCode::Down);
    }
    let last = len - 1;
    let current = (*selected).min(last);
    *selected = match key {
        KeyCode::Up => current.checked_sub(1).unwrap_or(last),
        KeyCode::Down if current == last => 0,
        KeyCode::Down => current + 1,
        KeyCode::PageUp => current.saturating_sub(PAGE as usize),
        KeyCode::PageDown => (current as isize + PAGE).min(last as isize) as usize,
        KeyCode::Home => 0,
        KeyCode::End => last,
        _ => return false,
    };
    true
}

// ── Model search ──────────────────────────────────────────────────────────

pub(super) struct ModelRow {
    pub provider: String,
    pub provider_name: String,
    pub id: String,
    pub family: String,
    pub context: usize,
    pub cost_in: f64,
    pub cost_out: f64,
    pub vision: bool,
    pub tools: bool,
    pub images: bool,
    pub reasoning: bool,
    pub has_key: bool,
    pub env_key: String,
}

pub(super) enum ModelEntry {
    Header(String),
    Row {
        index: usize,
        hits: Vec<usize>,
    },
    /// The typed id isn't listed: use it anyway on this provider.
    Custom {
        provider: String,
        id: String,
    },
}

#[derive(Default)]
pub(super) struct ModelPicker {
    pub query: String,
    pub selected: usize,
    /// Show one provider only.
    pub filter: Option<String>,
    pub rows: Vec<ModelRow>,
    /// (name, display name) in registry order.
    pub providers: Vec<(String, String)>,
    pub stars: Vec<String>,
    pub current_model: String,
    pub current_provider: String,
}

impl ModelPicker {
    fn is_starred(&self, row: &ModelRow) -> bool {
        self.stars.contains(&row.id)
    }

    fn is_current(&self, row: &ModelRow) -> bool {
        row.id == self.current_model && row.provider == self.current_provider
    }

    fn provider_has_key(&self, provider: &str) -> bool {
        self.rows
            .iter()
            .find(|r| r.provider == provider)
            .is_some_and(|r| r.has_key)
    }

    /// Score a row: every word must match its id, or else its provider or family.
    fn score(&self, row: &ModelRow, query: &str) -> Option<(i64, Vec<usize>)> {
        let id = folded(&row.id);
        let meta = format!("{} {} {}", row.provider, row.provider_name, row.family);
        let meta = folded(&meta);
        let mut total = 0;
        let mut hits = Vec::new();
        for term in query.split_whitespace() {
            let term = folded(term);
            if let Some((score, found)) = term_score(&id, &term) {
                total += score;
                hits.extend(found);
            } else if meta.windows(term.len()).any(|w| w == term.as_slice()) {
                total += 200;
            } else {
                return None;
            }
        }
        Some((total, hits))
    }

    /// The list as shown. With no search it is grouped: starred models, the
    /// active provider, providers with a key, then the rest. With a search it
    /// is flat and ranked.
    pub(super) fn entries(&self) -> Vec<ModelEntry> {
        let query = self.query.trim();
        let allowed = |row: &ModelRow| self.filter.as_ref().is_none_or(|p| &row.provider == p);
        let mut out = Vec::new();

        if query.is_empty() {
            let starred: Vec<usize> = (0..self.rows.len())
                .filter(|&i| allowed(&self.rows[i]) && self.is_starred(&self.rows[i]))
                .collect();
            if !starred.is_empty() {
                out.push(ModelEntry::Header("★ Starred".into()));
                out.extend(starred.into_iter().map(|index| ModelEntry::Row {
                    index,
                    hits: Vec::new(),
                }));
            }
            let mut order: Vec<&(String, String)> = self.providers.iter().collect();
            order.sort_by_key(|(name, _)| {
                (*name != self.current_provider, !self.provider_has_key(name))
            });
            for (name, display) in order {
                let rows: Vec<usize> = (0..self.rows.len())
                    .filter(|&i| {
                        let row = &self.rows[i];
                        &row.provider == name && allowed(row) && !self.is_starred(row)
                    })
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                out.push(ModelEntry::Header(if self.provider_has_key(name) {
                    display.clone()
                } else {
                    format!("{display} · no API key")
                }));
                out.extend(rows.into_iter().map(|index| ModelEntry::Row {
                    index,
                    hits: Vec::new(),
                }));
            }
            return out;
        }

        let mut scored: Vec<(i64, usize, Vec<usize>)> = (0..self.rows.len())
            .filter(|&i| allowed(&self.rows[i]))
            .filter_map(|i| {
                self.score(&self.rows[i], query)
                    .map(|(score, hits)| (score, i, hits))
            })
            .collect();
        scored.sort_by_key(|(score, i, _)| {
            let row = &self.rows[*i];
            (
                std::cmp::Reverse(*score),
                !self.is_starred(row),
                !row.has_key,
                std::cmp::Reverse(row.context),
            )
        });
        let exact = self
            .rows
            .iter()
            .any(|r| allowed(r) && r.id.eq_ignore_ascii_case(query));
        out.extend(
            scored
                .into_iter()
                .map(|(_, index, hits)| ModelEntry::Row { index, hits }),
        );
        if !exact && !query.contains(char::is_whitespace) {
            out.push(ModelEntry::Custom {
                provider: self
                    .filter
                    .clone()
                    .unwrap_or_else(|| self.current_provider.clone()),
                id: query.to_string(),
            });
        }
        out
    }

    /// Point at the active model, if it is listed.
    fn select_current(&mut self) {
        let position = self
            .entries()
            .iter()
            .filter(|e| !matches!(e, ModelEntry::Header(_)))
            .position(|e| match e {
                ModelEntry::Row { index, .. } => self.is_current(&self.rows[*index]),
                _ => false,
            });
        self.selected = position.unwrap_or(0);
    }

    /// Step through All, then each provider.
    pub(super) fn cycle_filter(&mut self, dir: isize) {
        let slots = self.providers.len() as isize + 1;
        let current = self
            .filter
            .as_ref()
            .and_then(|f| self.providers.iter().position(|(name, _)| name == f))
            .map_or(0, |i| i as isize + 1);
        let next = (current + dir).rem_euclid(slots);
        self.filter = (next > 0).then(|| self.providers[next as usize - 1].0.clone());
        self.selected = 0;
    }

    fn toggle_star(&mut self) {
        let entries = self.entries();
        let chosen = entries
            .iter()
            .filter(|e| !matches!(e, ModelEntry::Header(_)))
            .nth(self.selected);
        let Some(ModelEntry::Row { index, .. }) = chosen else {
            return;
        };
        let id = self.rows[*index].id.clone();
        if self.stars.contains(&id) {
            self.stars.retain(|s| s != &id);
        } else {
            self.stars.push(id.clone());
        }
        // The grouped list just changed shape: stay on the same model.
        let position = self
            .entries()
            .iter()
            .filter(|e| !matches!(e, ModelEntry::Header(_)))
            .position(|e| match e {
                ModelEntry::Row { index, .. } => self.rows[*index].id == id,
                _ => false,
            });
        self.selected = position.unwrap_or(0);
        persist_stars(&self.stars);
    }
}

/// Stars are shared with the desktop app, in the saved settings.
fn persist_stars(stars: &[String]) {
    if cfg!(test) {
        return;
    }
    if let Ok(mut settings) = crate::config::Settings::load() {
        settings.starred_models = stars.to_vec();
        let _ = settings.save();
    }
}

fn model_row(
    provider: &str,
    display: &str,
    env_key: &str,
    has_key: bool,
    m: &ModelInfo,
) -> ModelRow {
    ModelRow {
        provider: provider.to_string(),
        provider_name: display.to_string(),
        id: m.id.clone(),
        family: m.family.clone(),
        context: m.context_window,
        cost_in: m.cost_per_1k_input,
        cost_out: m.cost_per_1k_output,
        vision: matches!(m.vision, Vision::Yes),
        tools: m.tool_calling,
        images: m.image_gen,
        reasoning: m.adaptive_thinking || !m.effort_levels.is_empty(),
        has_key,
        env_key: env_key.to_string(),
    }
}

// ── Command palette ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Action {
    Models,
    Sessions,
    Processes,
    Mcp,
    Help,
    Inspector,
    Split,
    Grid,
    Reasoning,
    CopyReply,
    CopyCode,
    CopyChat,
    Focus(Option<usize>),
    Stop(usize),
    Slash(&'static str),
}

struct PaletteItem {
    label: String,
    detail: String,
    keys: &'static str,
    action: Action,
}

impl App {
    fn palette_items(&self) -> Vec<PaletteItem> {
        let named = |label: &str, detail: &str, keys: &'static str, action: Action| PaletteItem {
            label: label.to_string(),
            detail: detail.to_string(),
            keys,
            action,
        };
        let mut items = vec![
            named(
                "Switch model",
                "search every provider",
                "Ctrl+O",
                Action::Models,
            ),
            named(
                "Open session",
                "reopen a saved chat",
                "Ctrl+G",
                Action::Sessions,
            ),
            named(
                "Process monitor",
                "watch or stop commands",
                "Ctrl+B",
                Action::Processes,
            ),
            named(
                "Tool inspector",
                "arguments and output of tool calls",
                "Tab",
                Action::Inspector,
            ),
            named(
                "MCP servers",
                "status, reconnect, disable, remove",
                "Alt+M",
                Action::Mcp,
            ),
            named(
                "Toggle reasoning",
                "expand or collapse",
                "Ctrl+T",
                Action::Reasoning,
            ),
            named("Copy last reply", "", "Ctrl+Y", Action::CopyReply),
            named("Copy last code block", "", "Alt+Y", Action::CopyCode),
            named("Copy whole chat", "", "Ctrl+A", Action::CopyChat),
        ];
        if !self.subagents.is_empty() {
            items.push(named(
                "Split view",
                "main beside a sub-agent",
                "Ctrl+S",
                Action::Split,
            ));
            items.push(named(
                "Grid view",
                "every agent at once",
                "Alt+G",
                Action::Grid,
            ));
            items.push(named("Go to main", "", "Alt+0", Action::Focus(None)));
        }
        for (i, sub) in self.subagents.iter().enumerate() {
            items.push(PaletteItem {
                label: format!("Go to agent {}", sub.name),
                detail: sub.state_word().to_string(),
                keys: "",
                action: Action::Focus(Some(i)),
            });
            if sub.is_running() {
                items.push(PaletteItem {
                    label: format!("Stop agent {}", sub.name),
                    detail: String::new(),
                    keys: "",
                    action: Action::Stop(i),
                });
            }
        }
        for (cmd, detail) in SLASH_COMMANDS {
            if !NAMED_ACTIONS.contains(cmd) {
                items.push(PaletteItem {
                    label: cmd.to_string(),
                    detail: detail.to_string(),
                    keys: "",
                    action: Action::Slash(cmd),
                });
            }
        }
        items.push(named("Keyboard shortcuts", "", "F1", Action::Help));
        items
    }

    /// Palette items for a search, best first, with the matched label characters.
    fn palette_matches(&self, query: &str) -> Vec<(PaletteItem, Vec<usize>)> {
        let items = self.palette_items();
        if query.trim().is_empty() {
            return items.into_iter().map(|item| (item, Vec::new())).collect();
        }
        let mut scored: Vec<(i64, PaletteItem, Vec<usize>)> = items
            .into_iter()
            .filter_map(|item| {
                if let Some((score, hits)) = fuzzy_match(&item.label, query) {
                    return Some((score, item, hits));
                }
                fuzzy_match(&item.detail, query).map(|(score, _)| (score / 4, item, Vec::new()))
            })
            .collect();
        scored.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
        scored
            .into_iter()
            .map(|(_, item, hits)| (item, hits))
            .collect()
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Models => self.open_models(None),
            Action::Sessions => self.open_sessions(),
            Action::Processes => self.open_processes(),
            Action::Mcp => self.open_mcp(),
            Action::Help => self.overlay = Some(Overlay::Help { scroll: 0 }),
            Action::Inspector => self.open_inspector(None),
            Action::Split => self.toggle_split(),
            Action::Grid => self.toggle_grid(),
            Action::Reasoning => self.toggle_reasoning(),
            Action::CopyReply => self.copy_last_reply(),
            Action::CopyCode => self.copy_last_code_block(),
            Action::CopyChat => self.copy_transcript(),
            Action::Focus(view) => self.set_focus(view),
            Action::Stop(i) => self.cancel_subagent(Some(i)),
            Action::Slash(cmd) if NEEDS_ARGUMENT.contains(&cmd) => {
                self.set_input(format!("{cmd} "));
            }
            Action::Slash(cmd) => {
                if let Some(message) = self.submit(cmd.to_string()) {
                    self.outbox.push(message);
                }
            }
        }
    }

    pub(super) fn toggle_reasoning(&mut self) {
        self.show_reasoning = !self.show_reasoning;
        self.notify(if self.show_reasoning {
            "Showing full reasoning"
        } else {
            "Reasoning collapsed"
        });
    }

    // ── Opening ───────────────────────────────────────────────────────────

    /// Keys that open an overlay from anywhere. Returns true if handled.
    pub(super) fn overlay_shortcut(&mut self, key: KeyCode, ctrl: bool, alt: bool) -> bool {
        match key {
            KeyCode::Char('k' | 'p') if ctrl && !alt => {
                self.overlay = Some(Overlay::Palette {
                    query: String::new(),
                    selected: 0,
                });
            }
            KeyCode::Char('o') if ctrl && !alt => self.open_models(None),
            KeyCode::Char('g') if ctrl && !alt => self.open_sessions(),
            KeyCode::Char('b') if ctrl && !alt => self.open_processes(),
            KeyCode::Char('m') if alt && !ctrl => self.open_mcp(),
            _ => return false,
        }
        true
    }

    pub(super) fn open_models(&mut self, filter: Option<String>) {
        use crate::config::providers;
        let settings = crate::config::Settings::load().ok();
        let registry = match &settings {
            Some(s) => providers::build_registry_with_settings(s),
            None => providers::build_registry(),
        };
        let has_key = |provider: &str| {
            settings
                .as_ref()
                .and_then(|s| s.api_key_for(provider))
                .is_some_and(|k| !k.trim().is_empty())
        };
        let (current_model, current_provider) = self
            .model_info
            .rsplit_once(" / ")
            .map(|(m, p)| (m.to_string(), p.to_string()))
            .unwrap_or_else(|| (self.model_info.clone(), String::new()));
        let mut picker = ModelPicker {
            filter,
            stars: settings
                .as_ref()
                .map(|s| s.starred_models.clone())
                .unwrap_or_default(),
            current_model,
            current_provider,
            ..ModelPicker::default()
        };
        for provider in &registry {
            picker
                .providers
                .push((provider.name.clone(), provider.display_name.clone()));
            let key = has_key(&provider.name);
            for model in &provider.models {
                picker.rows.push(model_row(
                    &provider.name,
                    &provider.display_name,
                    &provider.env_key,
                    key,
                    model,
                ));
            }
        }
        picker.select_current();
        self.overlay = Some(Overlay::Models(Box::new(picker)));

        // Ollama's models are whatever is installed: ask it, without waiting.
        if let (Ok(runtime), Some(settings)) = (tokio::runtime::Handle::try_current(), settings) {
            let url = settings
                .ollama_base_url
                .clone()
                .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
            let (tx, rx) = std::sync::mpsc::channel();
            runtime.spawn(async move {
                if let Some(models) = crate::ai::ollama::discover_models(&url).await {
                    let _ = tx.send(models);
                }
            });
            self.pending_models = Some(rx);
        }
    }

    /// Local models arrived while the model search is open.
    pub(super) fn add_local_models(&mut self, models: Vec<ModelInfo>) {
        let Some(Overlay::Models(picker)) = &mut self.overlay else {
            return;
        };
        if !models.is_empty() && !picker.providers.iter().any(|(name, _)| name == "ollama") {
            picker
                .providers
                .push(("ollama".to_string(), "Ollama".to_string()));
        }
        for model in &models {
            let known = picker
                .rows
                .iter()
                .any(|r| r.provider == "ollama" && r.id == model.id);
            if !known {
                picker
                    .rows
                    .push(model_row("ollama", "Ollama", "", true, model));
            }
        }
    }

    pub(super) fn open_sessions(&mut self) {
        self.refresh_sessions();
        self.overlay = Some(Overlay::Sessions {
            query: String::new(),
            selected: 0,
        });
    }

    pub(super) fn open_processes(&mut self) {
        self.overlay = Some(Overlay::Processes { selected: 0 });
    }

    pub(super) fn open_mcp(&mut self) {
        mcp::start();
        self.overlay = Some(Overlay::Mcp {
            selected: 0,
            armed: None,
        });
    }

    /// Open the inspector on tool call `id`, or on the newest one.
    pub(super) fn open_inspector(&mut self, id: Option<&str>) {
        let log = &self.focused().tool_log;
        if log.is_empty() {
            self.notify("No tool calls in this view yet");
            return;
        }
        let selected = id
            .and_then(|id| log.iter().rev().position(|t| t.id == id))
            .unwrap_or(0);
        self.overlay = Some(Overlay::Inspector {
            selected,
            scroll: 0,
        });
    }

    /// Reopen saved session `index`: show it here and load it into the agent.
    pub(super) fn load_session(&mut self, index: usize) {
        if self.is_processing {
            self.notify("Stop the current run before switching sessions");
            return;
        }
        let Some(summary) = self.recent_sessions.get(index) else {
            return;
        };
        let id = summary.id.clone();
        match crate::agent::session::Session::load_by_id(&self.project_root, &id) {
            Some(session) => {
                self.replay_messages(&session.messages);
                self.set_focus(None);
                self.send_quiet(format!("/session load {id}"));
                self.notify(format!(
                    "Reopened a chat of {} messages",
                    session.messages.len()
                ));
            }
            None => self.notify("That session could not be read"),
        }
    }

    pub(super) fn sessions_matching(&self, query: &str) -> Vec<(usize, Vec<usize>)> {
        let mut found: Vec<(i64, usize, Vec<usize>)> = self
            .recent_sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| fuzzy_match(&s.title, query).map(|(score, hits)| (score, i, hits)))
            .collect();
        // With no search everything scores 0 and stays newest first.
        found.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
        found.into_iter().map(|(_, i, hits)| (i, hits)).collect()
    }

    // ── Input ─────────────────────────────────────────────────────────────

    /// Pasted text goes into the open overlay's search box, if it has one.
    /// Returns true when the overlay took it.
    pub(super) fn overlay_paste(&mut self, text: &str) -> bool {
        let clean = text.replace(['\r', '\n'], " ");
        match &mut self.overlay {
            Some(Overlay::Models(picker)) => {
                picker.query.push_str(&clean);
                picker.selected = 0;
            }
            Some(Overlay::Palette { query, selected })
            | Some(Overlay::Sessions { query, selected }) => {
                query.push_str(&clean);
                *selected = 0;
            }
            Some(_) => {}
            None => return false,
        }
        true
    }

    pub(super) fn overlay_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);
        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        let keep = match &mut overlay {
            Overlay::Help { scroll } => match key {
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('q' | '?') => false,
                KeyCode::Up => {
                    *scroll = scroll.saturating_sub(1);
                    true
                }
                KeyCode::Down => {
                    *scroll = scroll.saturating_add(1);
                    true
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(10);
                    true
                }
                KeyCode::PageDown => {
                    *scroll = scroll.saturating_add(10);
                    true
                }
                _ => true,
            },
            Overlay::Inspector { selected, scroll } => {
                let len = self.focused().tool_log.len();
                match key {
                    KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => false,
                    KeyCode::Char('c') | KeyCode::Char('y') => {
                        self.copy_tool_output(*selected);
                        true
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(10);
                        true
                    }
                    KeyCode::PageDown => {
                        *scroll = scroll.saturating_add(10);
                        true
                    }
                    _ => {
                        if move_selection(selected, key, len) {
                            *scroll = 0;
                        }
                        true
                    }
                }
            }
            Overlay::Models(picker) => match key {
                KeyCode::Esc => false,
                KeyCode::Enter => {
                    self.accept_model(picker);
                    false
                }
                KeyCode::Tab => {
                    picker.cycle_filter(1);
                    true
                }
                KeyCode::BackTab => {
                    picker.cycle_filter(-1);
                    true
                }
                KeyCode::Char('f') if ctrl && !alt => {
                    picker.toggle_star();
                    true
                }
                _ => {
                    let selectable = picker
                        .entries()
                        .iter()
                        .filter(|e| !matches!(e, ModelEntry::Header(_)))
                        .count();
                    if !move_selection(&mut picker.selected, key, selectable)
                        && edit_query(&mut picker.query, key, ctrl, alt)
                    {
                        picker.selected = 0;
                    }
                    true
                }
            },
            Overlay::Palette { query, selected } => match key {
                KeyCode::Esc => false,
                KeyCode::Enter => {
                    let action = self
                        .palette_matches(query)
                        .into_iter()
                        .nth(*selected)
                        .map(|(item, _)| item.action);
                    if let Some(action) = action {
                        self.run_action(action);
                    }
                    false
                }
                _ => {
                    let len = self.palette_matches(query).len();
                    if !move_selection(selected, key, len) && edit_query(query, key, ctrl, alt) {
                        *selected = 0;
                    }
                    true
                }
            },
            Overlay::Sessions { query, selected } => match key {
                KeyCode::Esc => false,
                KeyCode::Enter => {
                    let chosen = self
                        .sessions_matching(query)
                        .get(*selected)
                        .map(|(i, _)| *i);
                    if let Some(index) = chosen {
                        self.load_session(index);
                    }
                    false
                }
                _ => {
                    let len = self.sessions_matching(query).len();
                    if !move_selection(selected, key, len) && edit_query(query, key, ctrl, alt) {
                        *selected = 0;
                    }
                    true
                }
            },
            Overlay::Processes { selected } => {
                let runs = sorted_runs();
                let run = runs.get(*selected);
                match key {
                    KeyCode::Esc | KeyCode::Char('q') => false,
                    KeyCode::Char('x') => {
                        if let Some(run) = run.filter(|r| r.ended.is_none()) {
                            shell::stop_command(run.id);
                            self.notify("Stopping…");
                        }
                        true
                    }
                    KeyCode::Char('c') => {
                        shell::clear_command_feed();
                        *selected = 0;
                        true
                    }
                    KeyCode::Char('y') => {
                        if let Some(run) = run {
                            let log = strip_ansi(&shell::read_log_tail(&run.log_path, LOG_TAIL));
                            self.copy_text(log, "Copied the command's output");
                        }
                        true
                    }
                    _ => {
                        move_selection(selected, key, runs.len());
                        true
                    }
                }
            }
            Overlay::Mcp { selected, armed } => {
                let servers = mcp::list();
                let server = servers.get(*selected);
                let was_armed = armed.take();
                match key {
                    KeyCode::Esc | KeyCode::Char('q') => false,
                    KeyCode::Char('a') => {
                        self.set_input("/mcp add ".to_string());
                        false
                    }
                    KeyCode::Char('r') => {
                        if let Some(server) = server {
                            match mcp::reconnect(&server.name) {
                                Ok(()) => self.notify(format!("Reconnecting {}", server.name)),
                                Err(e) => self.notify(e),
                            }
                        }
                        true
                    }
                    KeyCode::Char('e') => {
                        if let Some(server) = server {
                            let enable = matches!(server.status, mcp::Status::Disabled);
                            if let Err(e) = mcp::set_enabled(&server.name, enable) {
                                self.notify(e);
                            }
                        }
                        true
                    }
                    KeyCode::Char('x') => {
                        if let Some(server) = server {
                            if was_armed.as_deref() == Some(server.name.as_str()) {
                                match mcp::remove(&server.name) {
                                    Ok(()) => self.notify(format!("Removed {}", server.name)),
                                    Err(e) => self.notify(e),
                                }
                                *selected = selected.saturating_sub(1);
                            } else {
                                *armed = Some(server.name.clone());
                            }
                        }
                        true
                    }
                    _ => {
                        move_selection(selected, key, servers.len());
                        true
                    }
                }
            }
        };
        // An action may have opened a different overlay in place of this one.
        if keep && self.overlay.is_none() {
            self.overlay = Some(overlay);
        }
    }

    /// Use the highlighted model.
    fn accept_model(&mut self, picker: &ModelPicker) {
        let entries = picker.entries();
        let chosen = entries
            .iter()
            .filter(|e| !matches!(e, ModelEntry::Header(_)))
            .nth(picker.selected);
        let (provider, id) = match chosen {
            Some(ModelEntry::Row { index, .. }) => {
                let row = &picker.rows[*index];
                (row.provider.clone(), row.id.clone())
            }
            Some(ModelEntry::Custom { provider, id }) => (provider.clone(), id.clone()),
            _ => return,
        };
        if id == picker.current_model && provider == picker.current_provider {
            self.notify(format!("Already using {id}"));
            return;
        }
        self.send_quiet(format!("/use {provider} {id}"));
    }

    fn overlay_selected(&mut self) -> Option<&mut usize> {
        match self.overlay.as_mut()? {
            Overlay::Help { .. } => None,
            Overlay::Models(picker) => Some(&mut picker.selected),
            Overlay::Inspector { selected, .. }
            | Overlay::Palette { selected, .. }
            | Overlay::Sessions { selected, .. }
            | Overlay::Processes { selected }
            | Overlay::Mcp { selected, .. } => Some(selected),
        }
    }

    pub(super) fn overlay_mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        match kind {
            MouseEventKind::ScrollUp => self.overlay_key(KeyCode::Up, KeyModifiers::NONE),
            MouseEventKind::ScrollDown => self.overlay_key(KeyCode::Down, KeyModifiers::NONE),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.overlay_area.get();
                let inside = column >= area.x
                    && column < area.x + area.width
                    && row >= area.y
                    && row < area.y + area.height;
                if !inside {
                    self.overlay = None;
                    return;
                }
                let hit = self
                    .overlay_rows
                    .borrow()
                    .iter()
                    .find(|(y, _)| *y == row)
                    .map(|(_, i)| *i);
                let Some(hit) = hit else {
                    return;
                };
                if let Some(selected) = self.overlay_selected() {
                    if *selected == hit {
                        // A second click on the highlighted row picks it.
                        self.overlay_key(KeyCode::Enter, KeyModifiers::NONE);
                    } else {
                        *selected = hit;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Running commands first, then the newest.
fn sorted_runs() -> Vec<shell::CommandRun> {
    let mut runs = shell::command_feed();
    runs.reverse();
    runs.sort_by_key(|run| run.ended.is_some());
    runs
}

/// Terminal colour codes and other control characters don't belong in a pane.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for code in chars.by_ref() {
                        if code.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

// ── Drawing ───────────────────────────────────────────────────────────────

pub(super) fn draw(frame: &mut Frame, area: Rect, app: &App, overlay: &Overlay) {
    app.overlay_rows.borrow_mut().clear();
    match overlay {
        Overlay::Help { scroll } => draw_help(frame, area, app, *scroll),
        Overlay::Inspector { selected, scroll } => {
            draw_inspector(frame, area, app, *selected, *scroll)
        }
        Overlay::Models(picker) => draw_models(frame, area, app, picker),
        Overlay::Palette { query, selected } => draw_palette(frame, area, app, query, *selected),
        Overlay::Sessions { query, selected } => draw_sessions(frame, area, app, query, *selected),
        Overlay::Processes { selected } => draw_processes(frame, area, app, *selected),
        Overlay::Mcp { selected, armed } => draw_mcp(frame, area, app, *selected, armed.as_deref()),
    }
}

/// A rounded, raised card centered in `area`. Returns the space inside it.
fn card(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    size: (u16, u16),
    title: &str,
    keys: &str,
) -> Rect {
    let theme = &app.theme;
    let width = size.0.min(area.width);
    let height = size.1.min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    app.overlay_area.set(rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent_color))
        .style(Style::default().bg(theme.surface).fg(theme.fg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            format!(" {} ", fit(keys, (width as usize).saturating_sub(4))),
            Style::default().fg(theme.dim_color),
        ));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    inner
}

/// The search box row; the terminal cursor sits at its end.
fn draw_query(frame: &mut Frame, area: Rect, app: &App, query: &str, placeholder: &str) {
    let theme = &app.theme;
    if area.height == 0 || area.width < 4 {
        return;
    }
    let shown = fit(query, area.width.saturating_sub(4) as usize);
    let text = if query.is_empty() {
        Span::styled(
            fit(placeholder, area.width.saturating_sub(4) as usize),
            Style::default().fg(theme.dim_color),
        )
    } else {
        Span::styled(shown.clone(), Style::default().fg(theme.input_color))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(theme.accent_color)),
            text,
        ])),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if app.approvals.is_empty() {
        frame.set_cursor_position((area.x + 2 + shown.width() as u16, area.y));
    }
}

/// First row to draw so `selected` stays in the middle of a list of `total`.
fn window_start(selected: usize, height: usize, total: usize) -> usize {
    selected
        .saturating_sub(height / 2)
        .min(total.saturating_sub(height))
}

/// Split a card's inside into a search row, a one-row gap and the rest.
fn query_and_list(inner: Rect) -> (Rect, Rect) {
    let query = Rect::new(inner.x, inner.y, inner.width, inner.height.min(1));
    let top = inner.y + inner.height.min(2);
    (
        query,
        Rect::new(
            inner.x,
            top,
            inner.width,
            inner.height.saturating_sub(top - inner.y),
        ),
    )
}

/// `200K`, `1M`, `1.5M`.
fn short_count(n: usize) -> String {
    match n {
        0 => "–".to_string(),
        1..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}K", n / 1_000),
        _ if n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Price per million tokens, in and out.
fn price(row: &ModelRow) -> String {
    if row.cost_in <= 0.0 && row.cost_out <= 0.0 {
        return "–".to_string();
    }
    format!("${:.2}/${:.2}", row.cost_in * 1000.0, row.cost_out * 1000.0)
}

fn draw_models(frame: &mut Frame, area: Rect, app: &App, picker: &ModelPicker) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let width = area.width.saturating_sub(4).min(110);
    let height = area.height.saturating_sub(2).min(30);
    let inner = card(
        frame,
        area,
        app,
        (width, height),
        "Models",
        "↑↓ pick · Enter use · Tab provider · Ctrl+F star · Esc close",
    );
    let (query_area, rest) = query_and_list(inner);
    draw_query(
        frame,
        query_area,
        app,
        &picker.query,
        "Search by name, family or provider",
    );

    // Provider filter chips, then the list, then the highlighted model's details.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(rest);
    let mut chips = vec![("All".to_string(), picker.filter.is_none())];
    chips.extend(picker.providers.iter().map(|(name, display)| {
        (
            display.clone(),
            picker.filter.as_deref() == Some(name.as_str()),
        )
    }));
    let total_w: usize = chips.iter().map(|(c, _)| c.width() + 2).sum();
    let skip = if total_w > rows[0].width as usize {
        chips.iter().position(|(_, on)| *on).unwrap_or(0)
    } else {
        0
    };
    let chip_spans: Vec<Span> = chips
        .iter()
        .skip(skip)
        .map(|(chip, on)| {
            if *on {
                Span::styled(
                    format!(" {chip} "),
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent_color)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {chip} "), dim)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(chip_spans)), rows[0]);

    let entries = picker.entries();
    let list = rows[1];
    let line_w = list.width as usize;
    let selectable: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !matches!(e, ModelEntry::Header(_)))
        .map(|(i, _)| i)
        .collect();
    let selected = picker.selected.min(selectable.len().saturating_sub(1));
    let selected_row = selectable.get(selected).copied().unwrap_or(0);
    let first = window_start(selected_row, list.height as usize, entries.len());
    let mut lines = Vec::new();
    let mut hit_rows = app.overlay_rows.borrow_mut();
    let mut selectable_seen = selectable.iter().filter(|&&r| r < first).count();
    for (i, entry) in entries
        .iter()
        .enumerate()
        .skip(first)
        .take(list.height as usize)
    {
        let y = list.y + (i - first) as u16;
        match entry {
            ModelEntry::Header(title) => {
                lines.push(Line::from(Span::styled(
                    title.clone(),
                    dim.add_modifier(Modifier::BOLD),
                )));
            }
            ModelEntry::Row { index, hits } => {
                hit_rows.push((y, selectable_seen));
                let is_selected = selectable_seen == selected;
                selectable_seen += 1;
                lines.push(model_line(
                    picker,
                    &picker.rows[*index],
                    hits,
                    is_selected,
                    line_w,
                    theme,
                ));
            }
            ModelEntry::Custom { provider, id } => {
                hit_rows.push((y, selectable_seen));
                let is_selected = selectable_seen == selected;
                selectable_seen += 1;
                let text = fit(
                    &format!("Use “{id}” on {provider}"),
                    line_w.saturating_sub(4),
                );
                let mut style = Style::default().fg(theme.accent_color);
                if is_selected {
                    style = style.bg(theme.selection);
                }
                let gap = line_w.saturating_sub(4 + text.width());
                lines.push(Line::from(vec![
                    Span::styled("    ", style),
                    Span::styled(text, style),
                    Span::styled(" ".repeat(gap), style),
                ]));
            }
        }
    }
    drop(hit_rows);
    if entries.is_empty() {
        lines.push(Line::from(Span::styled("No models match.", dim)));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list);

    let chosen = entries.get(selected_row).and_then(|e| match e {
        ModelEntry::Row { index, .. } => picker.rows.get(*index),
        _ => None,
    });
    if let Some(row) = chosen {
        let mut facts = vec![row.provider_name.clone()];
        if row.context > 0 {
            facts.push(format!("{} context", short_count(row.context)));
        }
        if row.cost_in > 0.0 || row.cost_out > 0.0 {
            facts.push(format!("{} per 1M in/out", price(row)));
        }
        let mut abilities = Vec::new();
        if row.tools {
            abilities.push("tools");
        }
        if row.vision {
            abilities.push("vision");
        }
        if row.reasoning {
            abilities.push("reasoning");
        }
        if row.images {
            abilities.push("image output");
        }
        let second = if row.has_key {
            Line::from(Span::styled(abilities.join(" · "), dim))
        } else if row.env_key.is_empty() {
            Line::from(Span::styled(
                "No API key set for this provider",
                Style::default().fg(theme.tool_color),
            ))
        } else {
            Line::from(Span::styled(
                format!("Set {} to use this provider", row.env_key),
                Style::default().fg(theme.tool_color),
            ))
        };
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    facts.join(" · "),
                    Style::default().fg(theme.fg),
                )),
                second,
            ])),
            rows[2],
        );
    }
}

fn model_line(
    picker: &ModelPicker,
    row: &ModelRow,
    hits: &[usize],
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let bg = if selected {
        theme.selection
    } else {
        theme.surface
    };
    let dim = Style::default().fg(theme.dim_color).bg(bg);
    let flat = !picker.query.trim().is_empty();

    // Right-hand columns, dropped as the card narrows.
    let mut right = String::new();
    if width >= 50 {
        right.push_str(&format!("{:>6}", short_count(row.context)));
    }
    if width >= 76 {
        right.push_str(&format!("  {:>15}", price(row)));
    }
    let caps = format!(
        "  {}{}{}{}",
        if row.tools { "T" } else { " " },
        if row.vision { "V" } else { " " },
        if row.reasoning { "R" } else { " " },
        if row.images { "I" } else { " " },
    );
    if width >= 60 {
        right.push_str(&caps);
    }

    let (star, star_style) = if picker.is_starred(row) {
        ("★ ", Style::default().fg(theme.tool_color).bg(bg))
    } else {
        ("  ", dim)
    };
    let (dot, dot_style) = if picker.is_current(row) {
        ("● ", Style::default().fg(theme.success_color).bg(bg))
    } else {
        ("  ", dim)
    };
    let id_room = width.saturating_sub(4 + right.width() + 1);
    let id = fit(&row.id, id_room);
    let mut base = Style::default().fg(if row.has_key {
        theme.fg
    } else {
        theme.dim_color
    });
    base = base.bg(bg);
    if selected {
        base = base.add_modifier(Modifier::BOLD);
    }
    let hit = base.fg(theme.accent_color).add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(star, star_style), Span::styled(dot, dot_style)];
    spans.extend(highlighted(&id, hits, base, hit));
    let mut used = 4 + id.width();
    // In a ranked list the groups are gone, so say which provider each row is.
    if flat {
        let provider = fit(
            &format!("  {}", row.provider),
            id_room.saturating_sub(id.width()),
        );
        used += provider.width();
        spans.push(Span::styled(provider, dim));
    }
    let gap = width.saturating_sub(used + right.width());
    spans.push(Span::styled(" ".repeat(gap), dim));
    spans.push(Span::styled(right, dim));
    Line::from(spans)
}

fn draw_palette(frame: &mut Frame, area: Rect, app: &App, query: &str, selected: usize) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let width = area.width.saturating_sub(4).min(84);
    let height = area.height.saturating_sub(2).min(22);
    let inner = card(
        frame,
        area,
        app,
        (width, height),
        "Commands",
        "↑↓ pick · Enter run · Esc close",
    );
    let (query_area, list) = query_and_list(inner);
    draw_query(
        frame,
        query_area,
        app,
        query,
        "Search actions, commands and agents",
    );

    let matches = app.palette_matches(query);
    let selected = selected.min(matches.len().saturating_sub(1));
    let first = window_start(selected, list.height as usize, matches.len());
    let line_w = list.width as usize;
    let mut lines = Vec::new();
    let mut hit_rows = app.overlay_rows.borrow_mut();
    for (i, (item, hits)) in matches
        .iter()
        .enumerate()
        .skip(first)
        .take(list.height as usize)
    {
        hit_rows.push((list.y + (i - first) as u16, i));
        let bg = if i == selected {
            theme.selection
        } else {
            theme.surface
        };
        let base = Style::default().fg(theme.fg).bg(bg);
        let hit = base.fg(theme.accent_color).add_modifier(Modifier::BOLD);
        let keys = item.keys;
        let label = fit(&item.label, line_w.saturating_sub(keys.width() + 2));
        let mut used = label.width();
        let mut spans = highlighted(&label, hits, base, hit);
        let room = line_w.saturating_sub(used + keys.width() + 3);
        if !item.detail.is_empty() && room > 8 {
            let detail = format!("  {}", fit(&item.detail, room - 2));
            used += detail.width();
            spans.push(Span::styled(detail, dim.bg(bg)));
        }
        let gap = line_w.saturating_sub(used + keys.width());
        spans.push(Span::styled(" ".repeat(gap), base));
        spans.push(Span::styled(keys, dim.bg(bg)));
        lines.push(Line::from(spans));
    }
    if matches.is_empty() {
        lines.push(Line::from(Span::styled("Nothing matches.", dim)));
    }
    drop(hit_rows);
    frame.render_widget(Paragraph::new(Text::from(lines)), list);
}

fn draw_sessions(frame: &mut Frame, area: Rect, app: &App, query: &str, selected: usize) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let width = area.width.saturating_sub(4).min(92);
    let height = area.height.saturating_sub(2).min(24);
    let inner = card(
        frame,
        area,
        app,
        (width, height),
        "Sessions",
        "↑↓ pick · Enter open · Esc close",
    );
    let (query_area, list) = query_and_list(inner);
    draw_query(frame, query_area, app, query, "Search saved chats");

    let matches = app.sessions_matching(query);
    let selected = selected.min(matches.len().saturating_sub(1));
    let first = window_start(selected, list.height as usize, matches.len());
    let line_w = list.width as usize;
    let mut lines = Vec::new();
    let mut hit_rows = app.overlay_rows.borrow_mut();
    for (i, (index, hits)) in matches
        .iter()
        .enumerate()
        .skip(first)
        .take(list.height as usize)
    {
        hit_rows.push((list.y + (i - first) as u16, i));
        let session = &app.recent_sessions[*index];
        let bg = if i == selected {
            theme.selection
        } else {
            theme.surface
        };
        let base = Style::default().fg(theme.fg).bg(bg);
        let hit = base.fg(theme.accent_color).add_modifier(Modifier::BOLD);
        let facts = format!(
            "{} msgs · {}",
            session.message_count,
            relative_time(&session.updated_at)
        );
        let room = line_w.saturating_sub(facts.width() + 2);
        let title = if session.title.is_empty() {
            "(untitled)".to_string()
        } else {
            fit(&session.title, room)
        };
        let gap = line_w.saturating_sub(title.width() + facts.width());
        let mut spans = highlighted(&title, hits, base, hit);
        spans.push(Span::styled(" ".repeat(gap), base));
        spans.push(Span::styled(facts, dim.bg(bg)));
        lines.push(Line::from(spans));
    }
    if matches.is_empty() {
        let text = if app.pending_sessions.is_some() {
            "Loading…"
        } else if query.is_empty() {
            "No saved chats for this project yet."
        } else {
            "Nothing matches."
        };
        lines.push(Line::from(Span::styled(text, dim)));
    }
    drop(hit_rows);
    frame.render_widget(Paragraph::new(Text::from(lines)), list);
}

fn draw_processes(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let width = area.width.saturating_sub(4).min(120);
    let height = area.height.saturating_sub(2).min(36);
    let inner = card(
        frame,
        area,
        app,
        (width, height),
        "Processes",
        "↑↓ pick · x stop · y copy output · c clear finished · Esc close",
    );
    let runs = sorted_runs();
    if runs.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("No commands have run yet.", dim)),
            inner,
        );
        return;
    }
    let selected = selected.min(runs.len() - 1);
    let list_height = (runs.len() as u16).min(inner.height / 2).max(1);
    let list = Rect::new(inner.x, inner.y, inner.width, list_height);
    let line_w = list.width as usize;
    let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
    let first = window_start(selected, list_height as usize, runs.len());
    let mut lines = Vec::new();
    let mut hit_rows = app.overlay_rows.borrow_mut();
    for (i, run) in runs
        .iter()
        .enumerate()
        .skip(first)
        .take(list_height as usize)
    {
        hit_rows.push((list.y + (i - first) as u16, i));
        let bg = if i == selected {
            theme.selection
        } else {
            theme.surface
        };
        let (mark, color, took) = match run.ended {
            None => (spinner, theme.tool_color, run.started.elapsed()),
            Some((Some(0), took)) => ("✓", theme.success_color, took),
            Some((Some(_), took)) => ("✗", theme.error_color, took),
            Some((None, took)) => ("■", theme.dim_color, took),
        };
        let took = format_duration(took);
        let agent = fit(&run.agent, 12);
        let room = line_w.saturating_sub(2 + agent.width() + 2 + took.width() + 2);
        let command = fit(&compact_detail(&run.command, 400), room);
        let gap = room.saturating_sub(command.width());
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(color).bg(bg)),
            Span::styled(
                format!("{agent}  "),
                Style::default().fg(theme.dim_color).bg(bg),
            ),
            Span::styled(command, Style::default().fg(theme.fg).bg(bg)),
            Span::styled(" ".repeat(gap + 2), Style::default().bg(bg)),
            Span::styled(took, Style::default().fg(theme.dim_color).bg(bg)),
        ]));
    }
    drop(hit_rows);
    frame.render_widget(Paragraph::new(Text::from(lines)), list);

    // The live end of the highlighted command's output.
    let run = &runs[selected];
    let log_top = list.y + list_height + 1;
    let log_height = (inner.y + inner.height).saturating_sub(log_top);
    if log_height == 0 {
        return;
    }
    let mut title = match run.ended {
        None => "running".to_string(),
        Some((Some(code), _)) => format!("exited with {code}"),
        Some((None, _)) => "stopped".to_string(),
    };
    if run.background {
        title.push_str(" · background");
    }
    if let Some(pid) = run.pid {
        title.push_str(&format!(" · pid {pid}"));
    }
    frame.render_widget(
        Paragraph::new(Span::styled(title, dim.add_modifier(Modifier::BOLD))),
        Rect::new(inner.x, log_top - 1, inner.width, 1),
    );
    let log = strip_ansi(&shell::read_log_tail(&run.log_path, LOG_TAIL));
    let tail: Vec<Line> = {
        let all: Vec<&str> = log.lines().collect();
        all[all.len().saturating_sub(log_height as usize)..]
            .iter()
            .map(|l| Line::from(fit(l, inner.width as usize)))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(Text::from(tail)),
        Rect::new(inner.x, log_top, inner.width, log_height),
    );
}

fn draw_mcp(frame: &mut Frame, area: Rect, app: &App, selected: usize, armed: Option<&str>) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let width = area.width.saturating_sub(4).min(100);
    let height = area.height.saturating_sub(2).min(26);
    let inner = card(
        frame,
        area,
        app,
        (width, height),
        "MCP servers",
        "↑↓ pick · r reconnect · e enable/disable · x remove · a add · Esc close",
    );
    let servers = mcp::list();
    if servers.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled("No MCP servers yet.", dim)),
                Line::default(),
                Line::from(Span::styled(
                    "Press a to add one, or ask the agent to install it.",
                    Style::default().fg(theme.fg),
                )),
            ])),
            inner,
        );
        return;
    }
    let selected = selected.min(servers.len() - 1);
    let line_w = inner.width as usize;
    let mut lines = Vec::new();
    let mut hit_rows = app.overlay_rows.borrow_mut();
    for (i, server) in servers.iter().enumerate() {
        hit_rows.push((inner.y + i as u16, i));
        let bg = if i == selected {
            theme.selection
        } else {
            theme.surface
        };
        let (mark, color, word) = match &server.status {
            mcp::Status::Connected => ("●", theme.success_color, "connected"),
            mcp::Status::Connecting => ("◌", theme.tool_color, "connecting"),
            mcp::Status::Failed(_) => ("✗", theme.error_color, "failed"),
            mcp::Status::Disabled => ("○", theme.dim_color, "disabled"),
        };
        let right = format!("{} tools  {word}", server.tools.len());
        let name = fit(&server.name, line_w.saturating_sub(right.width() + 4));
        let gap = line_w.saturating_sub(2 + name.width() + right.width());
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(color).bg(bg)),
            Span::styled(
                name,
                Style::default()
                    .fg(theme.fg)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(gap), Style::default().bg(bg)),
            Span::styled(right, dim.bg(bg)),
        ]));
    }
    drop(hit_rows);

    let server = &servers[selected];
    let config = &server.config;
    let target = config.url.clone().unwrap_or_else(|| {
        format!(
            "{} {}",
            config.command.clone().unwrap_or_default(),
            config.args.join(" ")
        )
    });
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(fit(target.trim(), line_w), dim)));
    if let mcp::Status::Failed(why) = &server.status {
        lines.push(Line::from(Span::styled(
            compact_detail(why, 400),
            Style::default().fg(theme.error_color),
        )));
    }
    if armed == Some(server.name.as_str()) {
        lines.push(Line::from(Span::styled(
            format!("Press x again to remove {}.", server.name),
            Style::default().fg(theme.tool_color),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_inspector(frame: &mut Frame, area: Rect, app: &App, selected: usize, scroll: u16) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let log = &app.focused().tool_log;
    let title = match app.focus.and_then(|i| app.subagents.get(i)) {
        Some(sub) => format!("Tools · {}", sub.name),
        None => "Tools".to_string(),
    };
    let width = area.width.saturating_sub(2).min(150);
    let height = area.height.saturating_sub(2).min(44);
    let inner = card(
        frame,
        area,
        app,
        (width, height),
        &title,
        "↑↓ pick · PgUp/PgDn scroll output · c copy output · Esc close",
    );
    if log.is_empty() {
        return;
    }
    let selected = selected.min(log.len() - 1);
    let wide = inner.width >= 90;
    let parts = Layout::default()
        .direction(if wide {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints(if wide {
            [Constraint::Percentage(36), Constraint::Percentage(64)]
        } else {
            [Constraint::Percentage(35), Constraint::Percentage(65)]
        })
        .split(inner);
    let list = parts[0];
    let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
    let first = window_start(selected, list.height as usize, log.len());
    let line_w = (list.width as usize).saturating_sub(1);
    let mut lines = Vec::new();
    let mut hit_rows = app.overlay_rows.borrow_mut();
    for (i, entry) in log
        .iter()
        .rev()
        .enumerate()
        .skip(first)
        .take(list.height as usize)
    {
        hit_rows.push((list.y + (i - first) as u16, i));
        let bg = if i == selected {
            theme.selection
        } else {
            theme.surface
        };
        let (mark, color) = tool_mark(entry.status, spinner, theme);
        let name = fit(&entry.name, line_w.saturating_sub(2));
        let room = line_w.saturating_sub(2 + name.width() + 2);
        let summary = compact_detail(
            &clean_paths(&summarize_args(&entry.name, &entry.args), &app.project_root),
            200,
        );
        let summary = fit(&summary, room);
        let gap = line_w.saturating_sub(2 + name.width() + 2 + summary.width());
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(color).bg(bg)),
            Span::styled(name, Style::default().fg(theme.fg).bg(bg)),
            Span::styled("  ", Style::default().bg(bg)),
            Span::styled(summary, dim.bg(bg)),
            Span::styled(" ".repeat(gap), Style::default().bg(bg)),
        ]));
    }
    drop(hit_rows);
    frame.render_widget(Paragraph::new(Text::from(lines)), list);

    // The highlighted call in full.
    let Some(entry) = log.iter().rev().nth(selected) else {
        return;
    };
    let (mark, color) = tool_mark(entry.status, spinner, theme);
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let mut text = vec![
        Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(color)),
            Span::styled(
                entry.name.clone(),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {}",
                    format_duration(entry.duration.unwrap_or_else(|| entry.started.elapsed()))
                ),
                dim,
            ),
        ]),
        Line::default(),
        heading("ARGUMENTS"),
    ];
    let args = serde_json::from_str::<serde_json::Value>(&entry.args)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| entry.args.clone());
    for l in crate::util::truncate_chars(&args, 4000).lines() {
        text.push(Line::from(l.to_string()));
    }
    text.push(Line::default());
    text.push(heading("OUTPUT"));
    let output = strip_ansi(&clean_paths(&entry.output, &app.project_root));
    for l in crate::util::truncate_chars(&output, 12_000).lines() {
        text.push(Line::from(l.to_string()));
    }
    let details = parts[1];
    let details = if wide {
        Rect::new(
            details.x + 1,
            details.y,
            details.width.saturating_sub(1),
            details.height,
        )
    } else {
        Rect::new(
            details.x,
            details.y + 1,
            details.width,
            details.height.saturating_sub(1),
        )
    };
    frame.render_widget(
        Paragraph::new(text)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        details,
    );
}

fn tool_mark<'a>(status: ToolStatus, spinner: &'a str, theme: &Theme) -> (&'a str, Color) {
    match status {
        ToolStatus::Running => (spinner, theme.tool_color),
        ToolStatus::Success => ("✓", theme.success_color),
        ToolStatus::Failed => ("✗", theme.error_color),
    }
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App, scroll: u16) {
    let theme = &app.theme;
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let row = |left: &str, right: &str| {
        Line::from(vec![
            Span::styled(format!("{left:<24}"), Style::default().fg(theme.fg)),
            Span::styled(right.to_string(), Style::default().fg(theme.dim_color)),
        ])
    };
    let mut lines: Vec<Line> = Vec::new();
    for (keys, action) in KEY_HELP {
        if action.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(heading(keys));
        } else {
            lines.push(row(keys, action));
        }
    }
    lines.push(Line::default());
    lines.push(heading("COMMANDS"));
    for (cmd, action) in SLASH_COMMANDS {
        lines.push(row(cmd, action));
    }

    let width = area.width.saturating_sub(4).min(84);
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let visible = height.saturating_sub(2);
    let scroll = scroll.min((lines.len() as u16).saturating_sub(visible));
    let keys = if (lines.len() as u16) > visible {
        "↑↓ scroll · Esc close"
    } else {
        "Esc close"
    };
    let inner = card(frame, area, app, (width, height), "Keys", keys);
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), inner);
}
