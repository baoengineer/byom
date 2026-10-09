//! `byom config`: a tabbed home for models, providers, roles and usage.
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
        TableState, Tabs, Wrap,
    },
};
use serde_json::Value;

use crate::catalog::Entry;
use crate::config::Config;
use crate::providers::{Auth, Provider};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Models,
    Providers,
    Roles,
    Usage,
}

const TABS: [Tab; 4] = [Tab::Models, Tab::Providers, Tab::Roles, Tab::Usage];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Models => "Models",
            Tab::Providers => "Providers",
            Tab::Roles => "Roles",
            Tab::Usage => "Usage",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Model,
    Background,
    Subagent,
    Opus,
    Sonnet,
    Haiku,
    Relay,
    Context,
    Transport,
    BehavesAs,
}

pub const FIELDS: [Field; 10] = [
    Field::Model,
    Field::Background,
    Field::Subagent,
    Field::Opus,
    Field::Sonnet,
    Field::Haiku,
    Field::Relay,
    Field::Context,
    Field::Transport,
    Field::BehavesAs,
];

/// Claude models this Claude Code release knows, offered for `behaves_as`.
const CLAUDE_PROFILES: [(&str, &str); 4] = [
    (
        "claude-opus-5-5",
        "Full effort range (low to max), adaptive thinking. Recommended.",
    ),
    ("claude-sonnet-5-5", "Same features, Sonnet prompt profile."),
    ("claude-opus-5", "Previous Opus profile."),
    ("claude-sonnet-5", "Previous Sonnet profile."),
];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Model => "Main model",
            Field::Background => "Background model",
            Field::Subagent => "Subagent default",
            Field::Opus => "opus alias",
            Field::Sonnet => "sonnet alias",
            Field::Haiku => "haiku alias",
            Field::Relay => "Claude relay",
            Field::Context => "Context window",
            Field::Transport => "ChatGPT transport",
            Field::BehavesAs => "Behaves as",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Model => {
                "Model Claude Code starts with. Switch any time inside Claude Code with /model."
            }
            Field::Background => {
                "Handles Claude Code's background work, such as titles and summaries. A small model saves usage."
            }
            Field::Subagent => {
                "Model for subagents that don't name one. Auto keeps Claude Code's default."
            }
            Field::Opus | Field::Sonnet | Field::Haiku => {
                "Model that agents and workflows asking for this Claude alias get. Auto keeps Claude when the relay is on."
            }
            Field::Relay => {
                "Relay Claude models to Anthropic on Claude Code's own sign-in, so Claude and other models share a session."
            }
            Field::Context => {
                "Window Claude Code compacts against for non-Claude main models. Auto uses the model's catalog value."
            }
            Field::Transport => {
                "How the bridge reaches the ChatGPT plan. WebSocket sends only new messages each turn. Applies after the bridge restarts."
            }
            Field::BehavesAs => {
                "Claude model whose handling Claude Code applies to other models: effort levels, thinking, prompt profile."
            }
        }
    }

    fn is_model(self) -> bool {
        matches!(
            self,
            Field::Model
                | Field::Background
                | Field::Subagent
                | Field::Opus
                | Field::Sonnet
                | Field::Haiku
        )
    }
}

/// One selectable value. `None` means the field's automatic default.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub value: Option<String>,
    pub label: String,
    pub detail: String,
}

pub struct Picker {
    pub field: Field,
    pub choices: Vec<Choice>,
    pub state: ListState,
}

/// A masked key being typed for a provider.
pub struct KeyInput {
    pub provider: String,
    pub text: String,
}

pub enum Action {
    None,
    Save,
    Refresh,
    /// Leave the UI to run an interactive sign-in.
    Login(String),
    SaveKey(String, String),
    RemoveKey(String),
}

pub struct App {
    pub config: Config,
    saved: Config,
    pub tab: Tab,
    pub roster: Vec<Entry>,
    pub log: Vec<Value>,
    pub claude: bool,
    pub models_cursor: usize,
    pub filter: String,
    pub filtering: bool,
    pub providers_cursor: usize,
    pub roles_cursor: usize,
    pub picker: Option<Picker>,
    pub key_input: Option<KeyInput>,
    pub usage_days: u64,
    pub status: String,
    confirm_quit: bool,
    pub done: bool,
    path: String,
}

fn k(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else {
        format!("{}k", tokens / 1000)
    }
}

impl App {
    pub fn new(config: Config, roster: Vec<Entry>, path: String) -> Self {
        Self {
            saved: config.clone(),
            config,
            tab: Tab::Models,
            roster,
            log: Vec::new(),
            claude: false,
            models_cursor: 0,
            filter: String::new(),
            filtering: false,
            providers_cursor: 0,
            roles_cursor: 0,
            picker: None,
            key_input: None,
            usage_days: 1,
            status: String::new(),
            confirm_quit: false,
            done: false,
            path,
        }
    }

    pub fn dirty(&self) -> bool {
        self.config != self.saved
    }

    pub fn mark_saved(&mut self) {
        self.saved = self.config.clone();
    }

    /// Whether Claude models are available: the relay, or an Anthropic API key.
    fn claude_models(&self) -> bool {
        self.config.relay && self.claude || crate::providers::claude_key(&self.config)
    }

    /// Every provider, including disabled ones, so they can be re-enabled here.
    pub fn providers(&self) -> Vec<Provider> {
        let mut view = self.config.clone();
        for provider in view.providers.values_mut() {
            provider.disabled = false;
        }
        crate::providers::all(&view)
    }

    fn provider_status(&self, provider: &Provider) -> String {
        if self
            .config
            .providers
            .get(&provider.id)
            .is_some_and(|c| c.disabled)
        {
            return "disabled".into();
        }
        crate::accounts::status(&self.config, provider, self.claude)
    }

    fn provider_ready(&self, id: &str) -> bool {
        let Some(provider) = crate::providers::find(&self.config, id) else {
            return false;
        };
        match provider.auth {
            Auth::ClaudeCode => {
                self.claude_models()
                    || crate::providers::api_key(&self.config, &provider)
                        .ok()
                        .flatten()
                        .is_some()
            }
            Auth::ChatGpt => crate::store::auth::get(id).ok().flatten().is_some(),
            Auth::ApiKey => crate::providers::api_key(&self.config, &provider)
                .ok()
                .flatten()
                .is_some(),
            Auth::None => true,
        }
    }

    /// Models shown in the Models tab: non-Claude first, then Claude when relayed.
    pub fn visible_models(&self) -> Vec<&Entry> {
        let filter = self.filter.to_lowercase();
        let mut models: Vec<&Entry> = self
            .roster
            .iter()
            .filter(|e| {
                if crate::providers::is_claude(&e.id) {
                    self.claude_models()
                } else {
                    e.listed
                }
            })
            .filter(|e| {
                filter.is_empty()
                    || e.id.to_lowercase().contains(&filter)
                    || e.name.to_lowercase().contains(&filter)
            })
            .collect();
        models.sort_by_key(|e| crate::providers::is_claude(&e.id));
        models
    }

    fn plan(&self) -> Option<crate::launch::Plan> {
        crate::launch::plan(
            &self.config,
            &crate::catalog::cached().unwrap_or_default(),
            &self.roster,
            None,
            self.claude_models(),
        )
        .ok()
    }

    fn role_of(&self, plan: Option<&crate::launch::Plan>, id: &str) -> String {
        let Some(plan) = plan else {
            return String::new();
        };
        let mut marks = Vec::new();
        if plan.model.as_deref() == Some(id) {
            marks.push("main");
        }
        if plan.background.as_deref() == Some(id) {
            marks.push("background");
        }
        if plan.subagent.as_deref() == Some(id) {
            marks.push("subagent");
        }
        marks.join(", ")
    }

    /// Current value as shown in the Roles tab.
    pub fn display(&self, field: Field) -> String {
        let plan = self.plan();
        let fallback = if self.claude_models() {
            "Claude Code default"
        } else {
            "-"
        };
        let auto = |resolved: Option<String>| {
            format!("auto → {}", resolved.unwrap_or_else(|| fallback.to_owned()))
        };
        let set = |value: &str| (!value.is_empty()).then(|| crate::providers::canonical(value));
        match field {
            Field::Model => {
                set(&self.config.model).unwrap_or_else(|| auto(plan.and_then(|p| p.model)))
            }
            Field::Background => set(&self.config.background)
                .unwrap_or_else(|| auto(plan.and_then(|p| p.background))),
            Field::Subagent => {
                set(&self.config.subagent).unwrap_or_else(|| auto(plan.and_then(|p| p.subagent)))
            }
            Field::Opus => {
                set(&self.config.aliases.opus).unwrap_or_else(|| auto(plan.and_then(|p| p.opus)))
            }
            Field::Sonnet => set(&self.config.aliases.sonnet)
                .unwrap_or_else(|| auto(plan.and_then(|p| p.sonnet))),
            Field::Haiku => set(&self.config.aliases.haiku)
                .unwrap_or_else(|| auto(plan.and_then(|p| p.background))),
            Field::Relay => match (self.config.relay, self.claude) {
                (true, true) => "on (Claude Code signed in)".into(),
                (true, false) => "on, inactive (Claude Code not signed in)".into(),
                (false, _) => "off".into(),
            },
            Field::Context if self.config.context_tokens == 0 => {
                auto(plan.and_then(|p| p.context_tokens).map(k))
            }
            Field::Context => k(self.config.context_tokens),
            Field::Transport if self.config.transport == "http" => "http (HTTP/SSE only)".into(),
            Field::Transport => "auto (WebSocket, HTTP fallback)".into(),
            Field::BehavesAs => self.config.behaves_as.clone(),
        }
    }

    pub fn choices(&self, field: Field) -> Vec<Choice> {
        if field.is_model() {
            let mut choices = vec![Choice {
                value: None,
                label: "Auto".into(),
                detail: if self.claude_models() {
                    "Claude Code's default".into()
                } else {
                    "Chosen by byom".into()
                },
            }];
            choices.extend(
                self.visible_models()
                    .into_iter()
                    .filter(|e| self.provider_ready(&e.provider))
                    .map(|e| Choice {
                        value: Some(e.id.clone()),
                        label: e.id.clone(),
                        detail: model_detail(e),
                    }),
            );
            return choices;
        }
        match field {
            Field::Relay => vec![
                Choice {
                    value: Some("true".into()),
                    label: "on".into(),
                    detail:
                        "Claude and other models in one session (when Claude Code is signed in)"
                            .into(),
                },
                Choice {
                    value: Some("false".into()),
                    label: "off".into(),
                    detail:
                        "Claude Code talks to Anthropic directly; byom serves other models only"
                            .into(),
                },
            ],
            Field::Context => {
                let mut choices = vec![Choice {
                    value: None,
                    label: "Auto".into(),
                    detail: "The main model's catalog window".into(),
                }];
                for size in [128_000u64, 200_000, 272_000, 1_000_000] {
                    choices.push(Choice {
                        value: Some(size.to_string()),
                        label: k(size),
                        detail: String::new(),
                    });
                }
                choices
            }
            Field::Transport => vec![
                Choice {
                    value: Some("auto".into()),
                    label: "auto".into(),
                    detail: "WebSocket per session, HTTP fallback. Fastest.".into(),
                },
                Choice {
                    value: Some("http".into()),
                    label: "http".into(),
                    detail: "HTTP/SSE only; resends the full conversation every turn.".into(),
                },
            ],
            _ => {
                let mut choices: Vec<Choice> = CLAUDE_PROFILES
                    .iter()
                    .map(|(id, detail)| Choice {
                        value: Some((*id).into()),
                        label: (*id).into(),
                        detail: (*detail).into(),
                    })
                    .collect();
                if !CLAUDE_PROFILES
                    .iter()
                    .any(|(id, _)| *id == self.config.behaves_as)
                {
                    choices.push(Choice {
                        value: Some(self.config.behaves_as.clone()),
                        label: self.config.behaves_as.clone(),
                        detail: "Current custom value".into(),
                    });
                }
                choices
            }
        }
    }

    fn current(&self, field: Field) -> Option<String> {
        let value = match field {
            Field::Model => self.config.model.clone(),
            Field::Background => self.config.background.clone(),
            Field::Subagent => self.config.subagent.clone(),
            Field::Opus => self.config.aliases.opus.clone(),
            Field::Sonnet => self.config.aliases.sonnet.clone(),
            Field::Haiku => self.config.aliases.haiku.clone(),
            Field::Relay => self.config.relay.to_string(),
            Field::Context if self.config.context_tokens == 0 => String::new(),
            Field::Context => self.config.context_tokens.to_string(),
            Field::Transport => self.config.transport.clone(),
            Field::BehavesAs => self.config.behaves_as.clone(),
        };
        (!value.is_empty()).then(|| {
            if field.is_model() {
                crate::providers::canonical(&value)
            } else {
                value
            }
        })
    }

    pub fn set(&mut self, field: Field, value: Option<String>) {
        let defaults = Config::default();
        let model = value.clone().unwrap_or_default();
        match field {
            Field::Model => self.config.model = model,
            Field::Background => self.config.background = model,
            Field::Subagent => self.config.subagent = model,
            Field::Opus => self.config.aliases.opus = model,
            Field::Sonnet => self.config.aliases.sonnet = model,
            Field::Haiku => self.config.aliases.haiku = model,
            Field::Relay => {
                self.config.relay = value.map(|v| v == "true").unwrap_or(defaults.relay)
            }
            Field::Context => {
                self.config.context_tokens = value.and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            Field::Transport => self.config.transport = value.unwrap_or(defaults.transport),
            Field::BehavesAs => self.config.behaves_as = value.unwrap_or(defaults.behaves_as),
        }
    }

    fn open_picker(&mut self) {
        let field = FIELDS[self.roles_cursor];
        let choices = self.choices(field);
        let current = self.current(field);
        let selected = choices.iter().position(|c| c.value == current).unwrap_or(0);
        self.picker = Some(Picker {
            field,
            choices,
            state: ListState::default().with_selected(Some(selected)),
        });
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.done = true;
            return Action::None;
        }
        if let Some(input) = &mut self.key_input {
            match key.code {
                KeyCode::Enter => {
                    let input = self.key_input.take().expect("key input is open");
                    if input.text.trim().is_empty() {
                        self.status = "No key entered.".into();
                        return Action::None;
                    }
                    return Action::SaveKey(input.provider, input.text.trim().to_owned());
                }
                KeyCode::Esc => {
                    self.key_input = None;
                    self.status = "Cancelled.".into();
                }
                KeyCode::Backspace => {
                    input.text.pop();
                }
                KeyCode::Char(c) => input.text.push(c),
                _ => {}
            }
            return Action::None;
        }
        if let Some(picker) = &mut self.picker {
            let last = picker.choices.len().saturating_sub(1);
            let at = picker.state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => picker.state.select(Some(at.saturating_sub(1))),
                KeyCode::Down | KeyCode::Char('j') => picker.state.select(Some((at + 1).min(last))),
                KeyCode::Home | KeyCode::Char('g') => picker.state.select(Some(0)),
                KeyCode::End | KeyCode::Char('G') => picker.state.select(Some(last)),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let field = picker.field;
                    let value = picker.choices[at].value.clone();
                    self.picker = None;
                    self.set(field, value);
                    self.status = format!("{} updated. Press s to save.", field.label());
                }
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') | KeyCode::Char('h') => {
                    self.picker = None
                }
                _ => {}
            }
            return Action::None;
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.models_cursor = 0;
            return Action::None;
        }
        let quit_pending = std::mem::take(&mut self.confirm_quit);
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
                self.tab = TABS[(i + 1) % TABS.len()];
                return Action::None;
            }
            KeyCode::BackTab | KeyCode::Left => {
                let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
                self.tab = TABS[(i + TABS.len() - 1) % TABS.len()];
                return Action::None;
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.tab = TABS[(c as usize) - ('1' as usize)];
                return Action::None;
            }
            KeyCode::Char('s') => return Action::Save,
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Esc | KeyCode::Char('q') => {
                if !self.dirty() || quit_pending {
                    self.done = true;
                } else {
                    self.confirm_quit = true;
                    self.status = "Unsaved changes. Press s to save, or q again to discard.".into();
                }
                return Action::None;
            }
            _ => {}
        }
        match self.tab {
            Tab::Models => self.models_key(key.code),
            Tab::Providers => self.providers_key(key.code),
            Tab::Roles => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.roles_cursor = self.roles_cursor.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.roles_cursor = (self.roles_cursor + 1).min(FIELDS.len() - 1)
                    }
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') => self.open_picker(),
                    KeyCode::Char('d') => {
                        let field = FIELDS[self.roles_cursor];
                        self.set(field, None);
                        self.status = format!("{} reset to default.", field.label());
                    }
                    _ => {}
                }
                Action::None
            }
            Tab::Usage => {
                if key.code == KeyCode::Char('w') {
                    self.usage_days = match self.usage_days {
                        1 => 7,
                        7 => 30,
                        _ => 1,
                    };
                }
                Action::None
            }
        }
    }

    fn models_key(&mut self, code: KeyCode) -> Action {
        let count = self.visible_models().len();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.models_cursor = self.models_cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.models_cursor = (self.models_cursor + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Enter | KeyCode::Char('m') | KeyCode::Char('b') | KeyCode::Char('a') => {
                let Some((id, provider)) = self
                    .visible_models()
                    .get(self.models_cursor)
                    .map(|e| (e.id.clone(), e.provider.clone()))
                else {
                    return Action::None;
                };
                if !self.provider_ready(&provider) {
                    self.status =
                        format!("{provider} is not usable yet; sign in on the Providers tab.");
                    return Action::None;
                }
                let field = match code {
                    KeyCode::Char('b') => Field::Background,
                    KeyCode::Char('a') => Field::Subagent,
                    _ => Field::Model,
                };
                self.set(field, Some(id.clone()));
                self.status = format!("{} set to {id}. Press s to save.", field.label());
            }
            _ => {}
        }
        Action::None
    }

    fn providers_key(&mut self, code: KeyCode) -> Action {
        let providers = self.providers();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.providers_cursor = self.providers_cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.providers_cursor =
                    (self.providers_cursor + 1).min(providers.len().saturating_sub(1))
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                let Some(p) = providers.get(self.providers_cursor) else {
                    return Action::None;
                };
                match p.auth {
                    Auth::ChatGpt => return Action::Login(p.id.clone()),
                    Auth::ApiKey | Auth::ClaudeCode => {
                        self.key_input = Some(KeyInput {
                            provider: p.id.clone(),
                            text: String::new(),
                        });
                    }
                    Auth::None => self.status = format!("{} needs no sign-in. {}", p.name, p.note),
                }
            }
            KeyCode::Char('x') => {
                if let Some(p) = providers.get(self.providers_cursor) {
                    return Action::RemoveKey(p.id.clone());
                }
            }
            KeyCode::Char('e') => {
                if let Some(p) = providers.get(self.providers_cursor) {
                    let entry = self.config.providers.entry(p.id.clone()).or_default();
                    entry.disabled = !entry.disabled;
                    let state = if entry.disabled {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    if *entry == crate::config::ProviderConfig::default() {
                        self.config.providers.remove(&p.id);
                    }
                    self.status = format!("{} {state}. Press s to save.", p.id);
                }
            }
            _ => {}
        }
        Action::None
    }
}

fn model_detail(e: &Entry) -> String {
    let mut parts = Vec::new();
    if !e.name.is_empty() {
        parts.push(e.name.clone());
    }
    if e.context_window > 0 {
        parts.push(format!("{} context", k(e.context_window)));
    }
    if !e.effort_levels.is_empty() {
        parts.push(format!("effort {}", e.effort_levels.join("/")));
    } else if e.reasoning {
        parts.push("reasoning".into());
    }
    parts.join(" · ")
}

/// Per model within the last `days`: requests, input tokens, output tokens, estimated cost.
pub fn usage(log: &[Value], roster: &[Entry], days: u64) -> Vec<(String, u64, u64, u64, f64)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut rows: Vec<(String, u64, u64, u64, f64)> = Vec::new();
    for entry in log {
        let at = entry["at"].as_u64().unwrap_or(0);
        if now.saturating_sub(at) > days * 86_400 {
            continue;
        }
        let model = crate::providers::canonical(entry["model"].as_str().unwrap_or("?"));
        let usage = &entry["usage"];
        let input = usage["input_tokens"].as_u64().unwrap_or(0)
            + usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
            + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let output = usage["output_tokens"].as_u64().unwrap_or(0);
        let cost = roster.iter().find(|e| e.id == model).map_or(0.0, |p| {
            (input as f64 * p.cost_input + output as f64 * p.cost_output) / 1_000_000.0
        });
        match rows.iter_mut().find(|r| r.0 == model) {
            Some(row) => {
                row.1 += 1;
                row.2 += input;
                row.3 += output;
                row.4 += cost;
            }
            None => rows.push((model, 1, input, output, cost)),
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    rows
}

const ACCENT: Color = Color::Cyan;

fn highlight() -> Style {
    Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, tabs, body, info, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let mut title = vec![
        Span::from(" byom ").bold().fg(ACCENT),
        Span::from("bring your own model ").dim(),
    ];
    if app.dirty() {
        title.push(Span::from(" modified ").fg(Color::Black).bg(Color::Yellow));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(title),
            Line::from(format!(" {}", app.path)).dim(),
        ]),
        header,
    );

    let selected = TABS.iter().position(|t| *t == app.tab).unwrap_or(0);
    frame.render_widget(
        Tabs::new(
            TABS.iter()
                .enumerate()
                .map(|(i, t)| format!("{} {}", i + 1, t.title())),
        )
        .select(selected)
        .highlight_style(
            Style::new()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .divider(" "),
        tabs,
    );

    let help = match app.tab {
        Tab::Models => draw_models(frame, app, body),
        Tab::Providers => draw_providers(frame, app, body),
        Tab::Roles => draw_roles(frame, app, body),
        Tab::Usage => draw_usage(frame, app, body),
    };
    let mut lines = vec![Line::from(help)];
    if !app.status.is_empty() {
        lines.push(Line::from(app.status.clone()).fg(Color::Green));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::new().padding(ratatui::widgets::Padding::horizontal(1))),
        info,
    );

    let keys = if app.key_input.is_some() {
        "type or paste the key   enter save   esc cancel"
    } else if app.picker.is_some() {
        "↑↓ choose   enter select   esc back"
    } else if app.filtering {
        "type to filter   enter done"
    } else {
        match app.tab {
            Tab::Models => {
                "tab next   ↑↓ move   enter main   b background   a subagent   / filter   r refresh   s save   q quit"
            }
            Tab::Providers => {
                "tab next   ↑↓ move   enter sign in / add key   x remove key   e enable/disable   s save   q quit"
            }
            Tab::Roles => "tab next   ↑↓ move   enter change   d default   s save   q quit",
            Tab::Usage => "tab next   w window   r refresh   q quit",
        }
    };
    frame.render_widget(Paragraph::new(format!(" {keys}")).dim(), footer);

    if let Some(picker) = &mut app.picker {
        let label_width = picker
            .choices
            .iter()
            .map(|c| c.label.len())
            .max()
            .unwrap_or(0)
            + 2;
        let items: Vec<ListItem> = picker
            .choices
            .iter()
            .map(|c| {
                ListItem::new(Line::from(vec![
                    Span::from(format!("{:label_width$}", c.label)),
                    Span::from(c.detail.clone()).dim(),
                ]))
            })
            .collect();
        let area = popup(frame.area(), body.y + 1, picker.choices.len() as u16 + 2);
        frame.render_widget(Clear, area);
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::new().fg(ACCENT))
                        .title(format!(" {} ", picker.field.label())),
                )
                .highlight_symbol("› ")
                .highlight_style(highlight()),
            area,
            &mut picker.state,
        );
    }
    if let Some(input) = &app.key_input {
        let area = popup(frame.area(), body.y + 2, 3);
        frame.render_widget(Clear, area);
        let masked = "•".repeat(input.text.chars().count());
        frame.render_widget(
            Paragraph::new(format!("{masked}▏")).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(ACCENT))
                    .title(format!(" API key for {} ", input.provider)),
            ),
            area,
        );
    }
}

fn draw_models(frame: &mut Frame, app: &App, area: Rect) -> String {
    let models = app.visible_models();
    let plan = app.plan();
    let rows: Vec<Row> = models
        .iter()
        .map(|e| {
            let ready = app.provider_ready(&e.provider);
            let price = if e.cost_input == 0.0 && e.cost_output == 0.0 {
                "-".into()
            } else {
                format!("{:.2}/{:.2}", e.cost_input, e.cost_output)
            };
            Row::new(vec![
                Cell::from(e.id.clone()),
                Cell::from(if e.context_window == 0 {
                    "-".into()
                } else {
                    k(e.context_window)
                }),
                Cell::from(price),
                Cell::from(if ready { "ready" } else { "sign in" }).style(if ready {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new().dim()
                }),
                Cell::from(app.role_of(plan.as_ref(), &e.id)).style(Style::new().fg(ACCENT)),
            ])
        })
        .collect();
    let title = if app.filter.is_empty() {
        " Models ".to_owned()
    } else {
        format!(" Models matching \"{}\" ", app.filter)
    };
    let mut state =
        TableState::default().with_selected((!models.is_empty()).then_some(app.models_cursor));
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Min(30),
                Constraint::Length(8),
                Constraint::Length(14),
                Constraint::Length(8),
                Constraint::Length(22),
            ],
        )
        .header(
            Row::new(["MODEL", "CONTEXT", "$/M IN/OUT", "STATUS", "ROLE"])
                .style(Style::new().dim()),
        )
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(title),
        )
        .highlight_symbol("› ")
        .row_highlight_style(highlight()),
        area,
        &mut state,
    );
    match models.get(app.models_cursor) {
        Some(e) => {
            let detail = model_detail(e);
            let provider = crate::providers::find(&app.config, &e.provider)
                .map(|p| p.name)
                .unwrap_or_default();
            if detail.is_empty() {
                provider
            } else {
                format!("{provider} · {detail}")
            }
        }
        None if app.roster.is_empty() => {
            "No models yet. Sign in on the Providers tab, then press r.".into()
        }
        None => "No models match.".into(),
    }
}

fn draw_providers(frame: &mut Frame, app: &App, area: Rect) -> String {
    let providers = app.providers();
    let rows: Vec<Row> = providers
        .iter()
        .map(|p| {
            let status = app.provider_status(p);
            let good = ["signed in", "key", "via Claude", "local"]
                .iter()
                .any(|s| status.starts_with(s));
            let models = app
                .roster
                .iter()
                .filter(|e| e.provider == p.id && e.listed)
                .count();
            Row::new(vec![
                Cell::from(p.id.clone()),
                Cell::from(p.name.clone()),
                Cell::from(p.protocol.as_str()),
                Cell::from(status).style(if good {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new().dim()
                }),
                Cell::from(if models == 0 {
                    "-".into()
                } else {
                    models.to_string()
                }),
            ])
        })
        .collect();
    let mut state = TableState::default().with_selected(Some(app.providers_cursor));
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Min(22),
                Constraint::Length(17),
                Constraint::Length(28),
                Constraint::Length(6),
            ],
        )
        .header(
            Row::new(["PROVIDER", "NAME", "PROTOCOL", "STATUS", "MODELS"])
                .style(Style::new().dim()),
        )
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Providers "),
        )
        .highlight_symbol("› ")
        .row_highlight_style(highlight()),
        area,
        &mut state,
    );
    providers
        .get(app.providers_cursor)
        .map(|p| {
            if p.signup.is_empty() {
                p.note.clone()
            } else {
                format!("{} {}", p.note, p.signup)
            }
        })
        .unwrap_or_default()
}

fn draw_roles(frame: &mut Frame, app: &App, area: Rect) -> String {
    let width = FIELDS.iter().map(|f| f.label().len()).max().unwrap_or(0) + 2;
    let rows: Vec<ListItem> = FIELDS
        .iter()
        .map(|f| {
            ListItem::new(Line::from(vec![
                Span::from(format!("{:width$}", f.label())),
                Span::from(app.display(*f)).fg(ACCENT),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.roles_cursor));
    frame.render_stateful_widget(
        List::new(rows)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Roles and settings "),
            )
            .highlight_symbol("› ")
            .highlight_style(highlight()),
        area,
        &mut state,
    );
    FIELDS[app.roles_cursor].help().into()
}

fn draw_usage(frame: &mut Frame, app: &App, area: Rect) -> String {
    let rows = usage(&app.log, &app.roster, app.usage_days);
    let total: f64 = rows.iter().map(|r| r.4).sum();
    let table_rows: Vec<Row> = rows
        .iter()
        .map(|(model, requests, input, output, cost)| {
            Row::new(vec![
                Cell::from(model.clone()),
                Cell::from(requests.to_string()),
                Cell::from(k(*input)),
                Cell::from(k(*output)),
                Cell::from(if *cost == 0.0 {
                    "-".into()
                } else {
                    format!("${cost:.2}")
                }),
            ])
        })
        .collect();
    let window = match app.usage_days {
        1 => "last 24 hours",
        7 => "last 7 days",
        _ => "last 30 days",
    };
    frame.render_widget(
        Table::new(
            table_rows,
            [
                Constraint::Min(30),
                Constraint::Length(9),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
        )
        .header(
            Row::new(["MODEL", "REQUESTS", "TOKENS IN", "OUT", "EST. COST"])
                .style(Style::new().dim()),
        )
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" Usage, {window} ")),
        ),
        area,
    );
    if rows.is_empty() {
        "No requests in this window.".into()
    } else {
        format!(
            "Estimated at API prices: ${total:.2}. Plan usage (ChatGPT, coding plans) is not billed per token."
        )
    }
}

fn popup(area: Rect, top: u16, height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(100);
    let top = if top + height <= area.bottom() {
        top
    } else {
        area.bottom().saturating_sub(height).max(area.y)
    };
    Rect {
        x: area.x + (area.width - width) / 2,
        y: top,
        width,
        height: height.min(area.bottom().saturating_sub(top)),
    }
}

/// config.json content with only the fields that differ from the defaults.
pub fn serialize(config: &Config) -> Result<Vec<u8>> {
    let defaults = serde_json::to_value(Config::default())?;
    let mut value = serde_json::to_value(config)?;
    if let Some(map) = value.as_object_mut() {
        map.retain(|key, v| defaults.get(key) != Some(v));
    }
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn save(app: &mut App, path: &std::path::Path) {
    let result = app
        .config
        .validate()
        .and_then(|_| serialize(&app.config))
        .and_then(|bytes| crate::store::write_private(path, &bytes));
    match result {
        Ok(()) => {
            let transport_changed = app.config.transport != app.saved.transport;
            app.mark_saved();
            app.status = if transport_changed {
                "Saved. Transport applies after the bridge restarts: byom restart.".into()
            } else {
                "Saved. The next byom run uses these settings.".into()
            };
        }
        Err(e) => app.status = format!("Save failed: {e:#}"),
    }
}

/// Show paths under the home directory as `~/...`.
fn tilde(path: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// Interactive home. Runs on a blocking thread; network work uses the runtime handle.
pub fn run(runtime: tokio::runtime::Handle) -> Result<()> {
    let path = crate::store::config_path()?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "byom config needs an interactive terminal; use `byom config set` or edit {}",
            path.display()
        );
    }
    let config = crate::config::load()?;
    let mut roster = crate::catalog::roster_cached();
    if !crate::catalog::roster_exists() {
        roster = runtime.block_on(crate::catalog::refresh(&config));
    }
    let mut app = App::new(config, roster, tilde(&path));
    app.claude = crate::launch::claude_signed_in();
    app.log = crate::roster::log_entries();
    let mut terminal = ratatui::try_init().context("starting terminal UI")?;
    let result = (|| -> Result<()> {
        while !app.done {
            terminal.draw(|frame| draw(frame, &mut app))?;
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Paste(text) => {
                    if let Some(input) = &mut app.key_input {
                        input.text.push_str(text.trim());
                    }
                    continue;
                }
                _ => continue,
            };
            match app.key(key) {
                Action::None => {}
                Action::Save => save(&mut app, &path),
                Action::Refresh => {
                    app.status = "Refreshing models…".into();
                    terminal.draw(|frame| draw(frame, &mut app))?;
                    app.roster = runtime.block_on(crate::catalog::refresh(&app.config));
                    app.log = crate::roster::log_entries();
                    let count = app.roster.iter().filter(|e| e.listed).count();
                    app.status = format!("Loaded {count} models.");
                }
                Action::SaveKey(provider, key) => {
                    let saved = crate::providers::resolve_key(&key).and_then(|_| {
                        runtime.block_on(async {
                            let _lock = crate::store::lock().await?;
                            crate::store::auth::set(&provider, serde_json::json!({"key": key}))
                        })
                    });
                    app.status = match saved {
                        Ok(()) => {
                            app.roster = runtime.block_on(crate::catalog::refresh(&app.config));
                            format!("Key saved for {provider}; models refreshed.")
                        }
                        Err(e) => format!("Could not save key: {e:#}"),
                    };
                }
                Action::RemoveKey(provider) => {
                    let removed = runtime.block_on(async {
                        let _lock = crate::store::lock().await?;
                        crate::store::auth::remove(&provider)
                    });
                    app.status = match removed {
                        Ok(true) => {
                            app.roster = runtime.block_on(crate::catalog::refresh(&app.config));
                            format!("Removed the saved sign-in for {provider}.")
                        }
                        Ok(false) => format!("{provider} has no saved sign-in."),
                        Err(e) => format!("Could not remove: {e:#}"),
                    };
                }
                Action::Login(provider) => {
                    ratatui::restore();
                    let outcome = runtime.block_on(crate::accounts::login(Some(provider.clone())));
                    if let Err(e) = &outcome {
                        eprintln!("Sign-in failed: {e:#}");
                    }
                    println!("\nPress Enter to return to byom config.");
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    terminal = ratatui::try_init().context("restarting terminal UI")?;
                    app.roster = crate::catalog::roster_cached();
                    app.status = match outcome {
                        Ok(()) => format!("Signed in to {provider}."),
                        Err(e) => format!("Sign-in failed: {e:#}"),
                    };
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result?;
    if app.dirty() {
        println!("Discarded unsaved changes.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn roster() -> Vec<Entry> {
        let entry = |id: &str, provider: &str| Entry {
            id: id.into(),
            provider: provider.into(),
            name: id.to_uppercase(),
            context_window: 272_000,
            cost_input: 1.0,
            cost_output: 2.0,
            listed: true,
            ..Default::default()
        };
        vec![
            entry("ollama/qwen3", "ollama"),
            entry("ollama/llama", "ollama"),
            entry("zai/glm-5", "zai"),
        ]
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app() -> App {
        // A key reference that never resolves keeps zai unusable whatever is saved locally.
        let mut config = Config {
            relay: false,
            ..Default::default()
        };
        config.providers.insert(
            "zai".into(),
            crate::config::ProviderConfig {
                api_key: "$BYOM_TEST_NEVER_SET".into(),
                ..Default::default()
            },
        );
        App::new(config, roster(), "~/.byom/config.json".into())
    }

    #[test]
    fn models_tab_lists_and_filters() {
        let mut app = app();
        let text = screen(&mut app);
        assert!(
            text.contains("ollama/qwen3") && text.contains("zai/glm-5"),
            "{text}"
        );
        assert!(text.contains("1.00/2.00"));
        press(&mut app, KeyCode::Char('/'));
        for c in "qwen".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.visible_models().len(), 1);
        assert!(screen(&mut app).contains("matching \"qwen\""));
    }

    #[test]
    fn models_tab_sets_roles_for_usable_providers_only() {
        let mut app = app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.config.model, "ollama/qwen3");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.config.background, "ollama/llama");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.config.subagent, "", "zai has no key");
        assert!(app.status.contains("not usable"));
        let saved: Value = serde_json::from_slice(&serialize(&app.config).unwrap()).unwrap();
        assert_eq!(saved["model"], "ollama/qwen3");
        assert_eq!(saved["background"], "ollama/llama");
    }

    #[test]
    fn tabs_switch_and_render() {
        let mut app = app();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Providers);
        let text = screen(&mut app);
        assert!(
            text.contains("PROTOCOL")
                && text.contains("openai-chat")
                && text.contains("local, no sign-in"),
            "{text}"
        );
        press(&mut app, KeyCode::Char('3'));
        let text = screen(&mut app);
        assert!(
            text.contains("Claude relay") && text.contains("haiku alias"),
            "{text}"
        );
        press(&mut app, KeyCode::Char('4'));
        assert!(screen(&mut app).contains("No requests in this window."));
    }

    #[test]
    fn provider_key_input_is_masked_and_saved_as_action() {
        let mut app = app();
        app.tab = Tab::Providers;
        app.providers_cursor = app.providers().iter().position(|p| p.id == "zai").unwrap();
        press(&mut app, KeyCode::Enter);
        for c in "sk-secret".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let text = screen(&mut app);
        assert!(text.contains("•••••••••") && !text.contains("sk-secret"));
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Action::SaveKey(p, k) if p == "zai" && k == "sk-secret")
        );
    }

    #[test]
    fn roles_picker_and_quit_confirmation() {
        let mut app = app();
        app.tab = Tab::Roles;
        app.roles_cursor = FIELDS.iter().position(|f| *f == Field::Relay).unwrap();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        assert!(app.config.relay);
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.done && app.status.contains("Unsaved"));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.done);
    }

    #[test]
    fn usage_sums_tokens_and_cost() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let log = vec![
            serde_json::json!({"at": now, "model": "zai/glm-5", "usage": {"input_tokens": 400_000, "cache_read_input_tokens": 600_000, "output_tokens": 500_000}}),
            serde_json::json!({"at": now, "model": "zai/glm-5", "usage": null}),
            serde_json::json!({"at": now - 3 * 86_400, "model": "zai/glm-5", "usage": {"input_tokens": 5}}),
        ];
        let rows = usage(&log, &roster(), 1);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].1, rows[0].2, rows[0].3), (2, 1_000_000, 500_000));
        assert!((rows[0].4 - 2.0).abs() < 1e-9);
        assert_eq!(usage(&log, &roster(), 7)[0].1, 3);
    }
}
