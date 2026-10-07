//! `byoclaude config`: an interactive editor for config.json.
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::catalog::Model;
use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Model,
    SmallModel,
    Context,
    Transport,
    BehavesAs,
}

pub const FIELDS: [Field; 5] = [
    Field::Model,
    Field::SmallModel,
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
            Field::Model => "Model",
            Field::SmallModel => "Background model",
            Field::Context => "Context window",
            Field::Transport => "Transport",
            Field::BehavesAs => "Behaves as",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Model => {
                "Model Claude Code starts with. Switch any time inside Claude Code with /model."
            }
            Field::SmallModel => {
                "Handles Claude Code's background work, such as titles and summaries. A small model saves plan usage."
            }
            Field::Context => {
                "Window Claude Code compacts against. Auto uses the model's catalog value."
            }
            Field::Transport => {
                "How the bridge reaches OpenAI. WebSocket sends only new messages each turn. Takes effect after the bridge restarts."
            }
            Field::BehavesAs => {
                "Claude model whose handling Claude Code applies to GPT models: effort levels, thinking, prompt profile."
            }
        }
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

pub struct App {
    pub config: Config,
    saved: Config,
    pub models: Vec<Model>,
    pub cursor: usize,
    pub picker: Option<Picker>,
    pub status: String,
    confirm_quit: bool,
    pub done: bool,
    path: String,
}

pub enum Action {
    None,
    Save,
    Refresh,
}

fn k(tokens: u64) -> String {
    format!("{}k", tokens / 1000)
}

fn model_detail(m: &Model) -> String {
    let mut parts = vec![];
    if !m.display_name.is_empty() {
        parts.push(m.display_name.clone());
    }
    if m.context_window > 0 {
        parts.push(format!("{} context", k(m.context_window)));
    }
    if !m.effort_levels.is_empty() {
        parts.push(format!("effort {}", m.effort_levels.join("/")));
    }
    parts.join(" · ")
}

impl App {
    pub fn new(config: Config, models: Vec<Model>, path: String) -> Self {
        Self {
            saved: config.clone(),
            config,
            models,
            cursor: 0,
            picker: None,
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

    fn listed(&self) -> impl Iterator<Item = &Model> {
        self.models.iter().filter(|m| m.listed)
    }

    /// What the launcher will actually use for the current settings.
    fn resolved(&self) -> Option<crate::launch::Plan> {
        crate::launch::plan(&self.config, &self.models, None).ok()
    }

    /// Current value as shown in the settings list.
    pub fn display(&self, field: Field) -> String {
        let plan = self.resolved();
        let auto = |resolved: Option<String>| match resolved {
            Some(r) => format!("auto → {r}"),
            None => "auto".into(),
        };
        match field {
            Field::Model if self.config.model.is_empty() => auto(plan.map(|p| p.model)),
            Field::Model => self.config.model.clone(),
            Field::SmallModel if self.config.background.is_empty() => {
                auto(plan.map(|p| p.small_model))
            }
            Field::SmallModel => self.config.background.clone(),
            Field::Context if self.config.context_tokens == 0 => {
                auto(plan.map(|p| k(p.context_tokens)))
            }
            Field::Context => k(self.config.context_tokens),
            Field::Transport if self.config.transport == "http" => "http (HTTP/SSE only)".into(),
            Field::Transport => "auto (WebSocket, HTTP fallback)".into(),
            Field::BehavesAs => self.config.behaves_as.clone(),
        }
    }

    pub fn choices(&self, field: Field) -> Vec<Choice> {
        let plan = self.resolved();
        let models = |auto_detail: String| {
            let mut choices = vec![Choice {
                value: None,
                label: "Auto".into(),
                detail: auto_detail,
            }];
            choices.extend(self.listed().map(|m| Choice {
                value: Some(m.slug.clone()),
                label: m.slug.clone(),
                detail: model_detail(m),
            }));
            choices
        };
        match field {
            Field::Model => models(match self.listed().next() {
                Some(m) => format!("First model in your plan: {}", m.slug),
                None => "First model in your plan".into(),
            }),
            Field::SmallModel => models(match plan {
                Some(p) => format!("Smallest listed model: {}", p.small_model),
                None => "Smallest listed model".into(),
            }),
            Field::Context => {
                let catalog =
                    crate::catalog::find(&self.models, &plan.map(|p| p.model).unwrap_or_default())
                        .map(|m| m.context_window)
                        .filter(|w| *w > 0);
                let mut choices = vec![Choice {
                    value: None,
                    label: "Auto".into(),
                    detail: match catalog {
                        Some(w) => format!("Model's catalog window: {}", k(w)),
                        None => "Model's catalog window".into(),
                    },
                }];
                let mut sizes = vec![128_000u64, 200_000, 272_000];
                if let Some(w) = catalog
                    && !sizes.contains(&w)
                {
                    sizes.push(w);
                    sizes.sort();
                }
                if self.config.context_tokens > 0 && !sizes.contains(&self.config.context_tokens) {
                    sizes.push(self.config.context_tokens);
                    sizes.sort();
                }
                choices.extend(sizes.into_iter().map(|s| Choice {
                    value: Some(s.to_string()),
                    label: k(s),
                    detail: if Some(s) == catalog {
                        "The model's full window".into()
                    } else if catalog.is_some_and(|w| s < w) {
                        "Compacts earlier; smaller, faster turns".into()
                    } else {
                        String::new()
                    },
                }));
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
            Field::BehavesAs => {
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
            Field::SmallModel => self.config.background.clone(),
            Field::Context if self.config.context_tokens == 0 => String::new(),
            Field::Context => self.config.context_tokens.to_string(),
            Field::Transport => self.config.transport.clone(),
            Field::BehavesAs => self.config.behaves_as.clone(),
        };
        (!value.is_empty()).then_some(value)
    }

    pub fn set(&mut self, field: Field, value: Option<String>) {
        let defaults = Config::default();
        match field {
            Field::Model => self.config.model = value.unwrap_or_default(),
            Field::SmallModel => self.config.background = value.unwrap_or_default(),
            Field::Context => {
                self.config.context_tokens = value.and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            Field::Transport => self.config.transport = value.unwrap_or(defaults.transport),
            Field::BehavesAs => self.config.behaves_as = value.unwrap_or(defaults.behaves_as),
        }
    }

    fn open(&mut self) {
        let field = FIELDS[self.cursor];
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
        let quit_pending = std::mem::take(&mut self.confirm_quit);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(FIELDS.len() - 1)
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                self.open()
            }
            KeyCode::Char('d') => {
                let field = FIELDS[self.cursor];
                self.set(field, None);
                self.status = format!("{} reset to default.", field.label());
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
            }
            _ => {}
        }
        Action::None
    }
}

const ACCENT: Color = Color::Cyan;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, help, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(FIELDS.len() as u16 + 2),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let mut title = vec![Span::from(" byoclaude config ").bold().fg(ACCENT)];
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
    let mut state = ListState::default().with_selected(Some(app.cursor));
    frame.render_stateful_widget(
        List::new(rows)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Settings "),
            )
            .highlight_symbol("› ")
            .highlight_style(Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
        body,
        &mut state,
    );

    // The picker covers this area; a partly hidden paragraph would show fragments.
    let mut info = if app.picker.is_some() {
        Vec::new()
    } else {
        vec![Line::from(FIELDS[app.cursor].help())]
    };
    if app.models.is_empty() && app.picker.is_none() {
        info.push(Line::from(""));
        info.push(
            Line::from("No model catalog yet. Sign in with byoclaude login, then press r.")
                .fg(Color::Yellow),
        );
    }
    if !app.status.is_empty() && app.picker.is_none() {
        info.push(Line::from(""));
        info.push(Line::from(app.status.clone()).fg(Color::Green));
    }
    frame.render_widget(
        Paragraph::new(info)
            .wrap(Wrap { trim: true })
            .block(Block::new().padding(ratatui::widgets::Padding::horizontal(1))),
        help,
    );

    let keys = if app.picker.is_some() {
        "↑↓ choose   enter select   esc back"
    } else {
        "↑↓ move   enter change   d default   r refresh models   s save   q quit"
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
        let area = popup(frame.area(), body.bottom(), picker.choices.len() as u16 + 2);
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
                .highlight_style(Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            area,
            &mut picker.state,
        );
    }
}

/// Picker area just below the settings box, so current values stay visible.
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
                "Saved. Transport applies after the bridge restarts: byoclaude stop, then byoclaude run.".into()
            } else {
                "Saved. The next byoclaude run uses these settings.".into()
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

/// Interactive editor. Runs on a blocking thread; catalog refreshes use the runtime handle.
pub fn run(runtime: tokio::runtime::Handle) -> Result<()> {
    let path = crate::store::config_path()?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "byoclaude config needs an interactive terminal; edit {} directly",
            path.display()
        );
    }
    let config = crate::config::load()?;
    let mut models = crate::catalog::cached().unwrap_or_default();
    if models.is_empty() {
        models = runtime
            .block_on(crate::catalog::fetch())
            .unwrap_or_default();
    }
    let mut app = App::new(config, models, tilde(&path));
    let mut terminal = ratatui::try_init().context("starting terminal UI")?;
    let result = (|| -> Result<()> {
        while !app.done {
            terminal.draw(|frame| draw(frame, &mut app))?;
            if let Event::Key(key) = event::read()? {
                match app.key(key) {
                    Action::Save => save(&mut app, &path),
                    Action::Refresh => {
                        app.status = "Refreshing models…".into();
                        terminal.draw(|frame| draw(frame, &mut app))?;
                        app.status = match runtime.block_on(crate::catalog::fetch()) {
                            Ok(models) => {
                                let count = models.iter().filter(|m| m.listed).count();
                                app.models = models;
                                format!("Loaded {count} models from your ChatGPT plan.")
                            }
                            Err(e) => format!("Refresh failed: {e:#}"),
                        };
                    }
                    Action::None => {}
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

    fn models() -> Vec<Model> {
        ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"]
            .iter()
            .map(|slug| Model {
                slug: (*slug).into(),
                display_name: slug.to_uppercase(),
                description: String::new(),
                context_window: 272_000,
                effort_levels: vec!["low".into(), "high".into()],
                default_effort: None,
                listed: true,
            })
            .collect()
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_resolved_defaults() {
        let mut app = App::new(Config::default(), models(), "/tmp/config.json".into());
        let text = screen(&mut app);
        assert!(text.contains("auto → gpt-6-astra"), "{text}");
        assert!(text.contains("auto → gpt-5.6-luna"));
        assert!(text.contains("auto → 272k"));
        assert!(!text.contains("modified"));
    }

    #[test]
    fn pick_model_and_save_only_changes() {
        let mut app = App::new(Config::default(), models(), "p".into());
        press(&mut app, KeyCode::Enter);
        assert!(screen(&mut app).contains("First model in your plan: gpt-6-astra"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.config.model, "gpt-5.6-sol");
        assert!(app.dirty());
        assert!(matches!(press(&mut app, KeyCode::Char('s')), Action::Save));
        let saved: serde_json::Value =
            serde_json::from_slice(&serialize(&app.config).unwrap()).unwrap();
        assert_eq!(saved, serde_json::json!({"model": "gpt-5.6-sol"}));
    }

    #[test]
    fn picker_opens_on_current_value_and_default_resets() {
        let config = Config {
            context_tokens: 200_000,
            ..Default::default()
        };
        let mut app = App::new(config, models(), "p".into());
        app.cursor = 2;
        press(&mut app, KeyCode::Enter);
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(
            picker.choices[picker.state.selected().unwrap()].label,
            "200k"
        );
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.config.context_tokens, 0);
    }

    #[test]
    fn quit_confirms_unsaved_changes() {
        let mut app = App::new(Config::default(), models(), "p".into());
        app.set(Field::Transport, Some("http".into()));
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.done);
        assert!(app.status.contains("Unsaved"));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.done);
    }

    #[test]
    fn works_without_a_catalog() {
        let mut app = App::new(Config::default(), Vec::new(), "p".into());
        let text = screen(&mut app);
        assert!(text.contains("No model catalog yet"), "{text}");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.picker.as_ref().unwrap().choices.len(), 1);
    }
}
