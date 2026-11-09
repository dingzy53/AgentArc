//! Keyboard-driven ratatui UI. No animations.

use crate::archive::{self, SyncProgress};
use crate::config::Config;
use crate::index::{Index, IngestOptions, IngestProgress};
use crate::model::Agent;
use crate::pricing::Pricing;
use crate::stats::{self, GroupBy, Range, StatsResult};
use crate::util;
use anyhow::Result;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, List, ListItem, Paragraph, Row, Table, Tabs, Wrap};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const TABS: [&str; 3] = ["Overview", "Timeline", "Models"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Popup {
    Agents,
    Hosts,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncKind {
    Sync,
    Index,
}

#[derive(Clone, Debug)]
struct SyncView {
    phase: String,
    total: u64,
    done: u64,
    copied: u64,
    bytes: u64,
    versioned: u64,
    current: String,
    started_ms: i64,
}

impl SyncView {
    fn new(kind: SyncKind) -> Self {
        Self {
            phase: match kind {
                SyncKind::Sync => "starting".to_string(),
                SyncKind::Index => "indexing".to_string(),
            },
            total: 0,
            done: 0,
            copied: 0,
            bytes: 0,
            versioned: 0,
            current: String::new(),
            started_ms: util::now_ms(),
        }
    }

    fn elapsed(&self) -> String {
        let secs = ((util::now_ms() - self.started_ms).max(0) / 1000) as u64;
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m{:02}s", secs / 60, secs % 60)
        }
    }
}

enum SyncUpdate {
    Sync(SyncProgress),
    Index(IngestProgress),
    Finished { ok: bool, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigRow {
    ArchiveDir,
    HostName,
    Timezone,
    SyncOnStart,
    AllowCloud,
    Agent(Agent),
    Save,
    SaveSync,
    Cancel,
}

fn config_rows() -> Vec<ConfigRow> {
    let mut rows = vec![
        ConfigRow::ArchiveDir,
        ConfigRow::HostName,
        ConfigRow::Timezone,
        ConfigRow::SyncOnStart,
        ConfigRow::AllowCloud,
    ];
    rows.extend(Agent::ALL.into_iter().map(ConfigRow::Agent));
    rows.push(ConfigRow::Save);
    rows.push(ConfigRow::SaveSync);
    rows.push(ConfigRow::Cancel);
    rows
}

fn config_row_label(row: ConfigRow) -> &'static str {
    match row {
        ConfigRow::ArchiveDir => "Archive folder",
        ConfigRow::HostName => "Host name",
        ConfigRow::Timezone => "Timezone",
        ConfigRow::SyncOnStart => "Sync on start",
        ConfigRow::AllowCloud => "Allow cloud archive",
        ConfigRow::Agent(agent) => agent.display_name(),
        ConfigRow::Save => "Save",
        ConfigRow::SaveSync => "Save & sync",
        ConfigRow::Cancel => "Cancel",
    }
}

struct ConfigForm {
    cfg: Config,
    selected: usize,
    editing: Option<String>,
    first_run: bool,
    message: String,
}

impl ConfigForm {
    fn new(cfg: Config, first_run: bool) -> Self {
        Self {
            cfg,
            selected: 0,
            editing: None,
            first_run,
            message: String::new(),
        }
    }

    fn rows(&self) -> Vec<ConfigRow> {
        config_rows()
    }

    fn current(&self) -> ConfigRow {
        self.rows()[self.selected]
    }

    fn next(&mut self, delta: i32) {
        let n = self.rows().len();
        if n == 0 {
            return;
        }
        let cur = self.selected as i32;
        self.selected = ((cur + delta).rem_euclid(n as i32)) as usize;
    }

    fn begin_edit(&mut self) {
        self.editing = match self.current() {
            ConfigRow::ArchiveDir => Some(self.cfg.archive_dir.clone()),
            ConfigRow::HostName => Some(self.cfg.host_name.clone()),
            ConfigRow::Timezone => Some(self.cfg.timezone.clone()),
            _ => None,
        };
    }

    fn commit_edit(&mut self, buffer: String) {
        match self.current() {
            ConfigRow::ArchiveDir => {
                let v = buffer.trim();
                if !v.is_empty() {
                    self.cfg.archive_dir = v.to_string();
                }
            }
            ConfigRow::HostName => self.cfg.host_name = buffer.trim().to_string(),
            ConfigRow::Timezone => self.cfg.timezone = buffer.trim().to_string(),
            _ => {}
        }
        self.editing = None;
    }

    fn value(&self, row: ConfigRow) -> String {
        match row {
            ConfigRow::ArchiveDir => self.cfg.archive_dir.clone(),
            ConfigRow::HostName => self.cfg.host_name.clone(),
            ConfigRow::Timezone => {
                if self.cfg.timezone.trim().is_empty() {
                    "(system timezone)".to_string()
                } else {
                    self.cfg.timezone.clone()
                }
            }
            ConfigRow::SyncOnStart => on_off(self.cfg.sync_on_start),
            ConfigRow::AllowCloud => on_off(self.cfg.allow_cloud_archive),
            ConfigRow::Agent(agent) => on_off(self.cfg.source_config(agent).enabled),
            ConfigRow::Save => String::new(),
            ConfigRow::SaveSync => String::new(),
            ConfigRow::Cancel => String::new(),
        }
    }

    fn toggle(&mut self, row: ConfigRow) {
        match row {
            ConfigRow::SyncOnStart => self.cfg.sync_on_start = !self.cfg.sync_on_start,
            ConfigRow::AllowCloud => self.cfg.allow_cloud_archive = !self.cfg.allow_cloud_archive,
            ConfigRow::Agent(agent) => {
                let source = self.cfg.source_config_mut(agent);
                source.enabled = !source.enabled;
            }
            _ => {}
        }
    }

    fn save(&self) -> Result<std::path::PathBuf> {
        self.cfg.save()
    }
}

fn on_off(v: bool) -> String {
    if v {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

pub struct App {
    cfg: Config,
    tz: TimeZone,
    pricing: Pricing,
    index: Index,
    base_events: Vec<crate::model::UsageEvent>,
    base_sessions: Vec<crate::index::SessionInfo>,
    visible_events: Vec<crate::model::UsageEvent>,
    timeline: StatsResult,
    models: StatsResult,
    agents: Vec<Agent>,
    selected_agents: HashSet<Agent>,
    all_hosts: Vec<String>,
    selected_hosts: HashSet<String>,
    range: Range,
    group_idx: usize,
    tab: usize,
    selected: usize,
    popup: Option<Popup>,
    popup_selected: usize,
    heatmap_by_cost: bool,
    status: String,
    last_sync: Option<String>,
    quit: bool,
    sync_rx: Option<Receiver<SyncUpdate>>,
    sync_view: Option<SyncView>,
    config_page: Option<ConfigForm>,
}

impl App {
    fn new(cfg: Config, pricing: Pricing, index: Index) -> Result<Self> {
        let tz = cfg.timezone();
        let agents = Agent::ALL.to_vec();
        let last_sync = index
            .get_meta("last_sync")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("at").and_then(|a| a.as_i64()))
            .map(|ms| util::fmt_ts(ms, &tz, "%Y-%m-%d %H:%M"));
        let first_run = !util::config_path().exists();
        let config_form = first_run.then(|| ConfigForm::new(cfg.clone(), true));
        let mut app = App {
            cfg,
            tz,
            pricing,
            index,
            base_events: Vec::new(),
            base_sessions: Vec::new(),
            visible_events: Vec::new(),
            timeline: StatsResult::default(),
            models: StatsResult::default(),
            selected_agents: agents.iter().copied().collect(),
            agents,
            all_hosts: Vec::new(),
            selected_hosts: HashSet::new(),
            range: Range::ThisYear,
            group_idx: 2,
            tab: 0,
            selected: 0,
            popup: None,
            popup_selected: 0,
            heatmap_by_cost: false,
            status: String::new(),
            last_sync,
            quit: false,
            sync_rx: None,
            sync_view: None,
            config_page: config_form,
        };
        app.reload()?;
        app.recompute();
        Ok(app)
    }

    fn reload(&mut self) -> Result<()> {
        let filters = crate::index::Filters::default();
        self.base_events = self.index.load_events(&filters)?;
        self.base_sessions = self.index.load_sessions(&filters)?;
        self.all_hosts = self
            .base_sessions
            .iter()
            .map(|s| s.host.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.all_hosts.sort();
        if self.selected_hosts.is_empty() {
            self.selected_hosts = self.all_hosts.iter().cloned().collect();
        }
        Ok(())
    }

    fn active_agents(&self) -> Vec<Agent> {
        self.agents
            .iter()
            .copied()
            .filter(|a| self.selected_agents.contains(a))
            .collect()
    }

    fn active_hosts(&self) -> Vec<String> {
        self.all_hosts
            .iter()
            .filter(|h| self.selected_hosts.contains(*h))
            .cloned()
            .collect()
    }

    fn recompute(&mut self) {
        let now = util::now_ms();
        let start = self.range.start_ms(&self.tz, now);
        let agents = self.active_agents();
        let hosts = self.active_hosts();
        let agent_set: HashSet<Agent> = agents.iter().copied().collect();
        let host_set: HashSet<String> = hosts.iter().cloned().collect();
        self.visible_events = self
            .base_events
            .iter()
            .filter(|e| {
                agent_set.contains(&e.agent)
                    && host_set.contains(&e.host)
                    && start.is_none_or(|s| e.ts >= s)
            })
            .cloned()
            .collect();
        let session_map: HashMap<String, crate::index::SessionInfo> = self
            .base_sessions
            .iter()
            .map(|s| (s.key.clone(), s.clone()))
            .collect();
        self.timeline = stats::compute(
            &self.visible_events,
            &session_map,
            GroupBy::from_tui(self.group_idx),
            &self.tz,
            &self.pricing,
        );
        self.models = stats::compute(
            &self.visible_events,
            &session_map,
            GroupBy::Model,
            &self.tz,
            &self.pricing,
        );
        let max = match self.tab {
            1 => self.timeline.rows.len(),
            2 => self.models.rows.len(),
            _ => 0,
        };
        if self.selected >= max {
            self.selected = max.saturating_sub(1);
        }
    }

    // -----------------------------------------------------------------------
    // Sync job
    // -----------------------------------------------------------------------

    fn start_sync(&mut self) {
        self.start_job(SyncKind::Sync);
    }

    fn start_index(&mut self) {
        self.start_job(SyncKind::Index);
    }

    fn start_job(&mut self, kind: SyncKind) {
        if self.sync_rx.is_some() {
            self.status = "a sync/index job is already running".to_string();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.sync_rx = Some(rx);
        self.sync_view = Some(SyncView::new(kind));
        self.status = match kind {
            SyncKind::Sync => "syncing in the background…".to_string(),
            SyncKind::Index => "indexing in the background…".to_string(),
        };
        let cfg = self.cfg.clone();
        std::thread::spawn(move || match kind {
            SyncKind::Sync => run_sync_job(cfg, tx),
            SyncKind::Index => run_index_job(cfg, tx, None),
        });
    }

    fn poll_sync(&mut self) {
        let mut messages = Vec::new();
        if let Some(rx) = &self.sync_rx {
            loop {
                match rx.try_recv() {
                    Ok(msg) => messages.push(msg),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }
        for msg in messages {
            match msg {
                SyncUpdate::Sync(p) => {
                    if let Some(view) = &mut self.sync_view {
                        view.phase = p.phase.clone();
                        view.total = p.total;
                        view.done = p.processed;
                        view.copied = p.copied;
                        view.bytes = p.bytes;
                        view.versioned = p.versioned;
                        view.current = p.current.clone();
                    }
                    if p.phase == "syncing" {
                        self.status = format!(
                            "syncing {}/{} · {}",
                            p.processed,
                            p.total,
                            util::human_bytes(p.bytes)
                        );
                    } else if p.phase == "discovering" {
                        self.status = "discovering source files…".to_string();
                    }
                }
                SyncUpdate::Index(p) => {
                    if let Some(view) = &mut self.sync_view {
                        view.phase = "indexing".to_string();
                        view.total = p.total;
                        view.done = p.done;
                        view.current = p.current.clone();
                    }
                    self.status = format!("indexing {}/{}", p.done, p.total);
                }
                SyncUpdate::Finished { ok, message } => {
                    self.sync_rx = None;
                    self.sync_view = None;
                    self.status = message;
                    if ok {
                        self.last_sync =
                            Some(util::fmt_ts(util::now_ms(), &self.tz, "%Y-%m-%d %H:%M"));
                        if let Err(e) = self.reload() {
                            self.status = format!("reload failed: {e:#}");
                        }
                        self.recompute();
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        if self.config_page.is_some() {
            return self.on_config_key(key);
        }
        if self.popup.is_some() {
            return self.on_popup_key(key);
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => {
                self.popup = Some(Popup::Help);
                self.popup_selected = 0;
            }
            KeyCode::Char('1') => self.switch_tab(0),
            KeyCode::Char('2') => self.switch_tab(1),
            KeyCode::Char('3') => self.switch_tab(2),
            KeyCode::Tab => self.switch_tab((self.tab + 1) % TABS.len()),
            KeyCode::BackTab => self.switch_tab((self.tab + TABS.len() - 1) % TABS.len()),
            KeyCode::Char('c') => {
                self.config_page = Some(ConfigForm::new(self.cfg.clone(), false));
            }
            KeyCode::Char('r') => {
                self.range = self.range.next();
                self.selected = 0;
                self.recompute();
            }
            KeyCode::Char('a') => {
                self.popup = Some(Popup::Agents);
                self.popup_selected = 0;
            }
            KeyCode::Char('h') => {
                self.popup = Some(Popup::Hosts);
                self.popup_selected = 0;
            }
            KeyCode::Char('g') if self.tab == 1 => {
                self.group_idx = (self.group_idx + 1) % 3;
                self.selected = 0;
                self.recompute();
            }
            KeyCode::Char('t') if self.tab == 0 => self.heatmap_by_cost = !self.heatmap_by_cost,
            KeyCode::Char('s') => self.start_sync(),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                let max = self.row_count();
                if max > 0 {
                    self.selected = (self.selected + 1).min(max - 1);
                }
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => {
                let max = self.row_count();
                if max > 0 {
                    self.selected = (self.selected + 10).min(max - 1);
                }
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.row_count().saturating_sub(1),
            _ => {}
        }
        Ok(())
    }

    fn row_count(&self) -> usize {
        match self.tab {
            1 => self.timeline.rows.len(),
            2 => self.models.rows.len(),
            _ => 0,
        }
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.selected = 0;
    }

    fn on_popup_key(&mut self, key: KeyEvent) -> Result<()> {
        let popup = self.popup.unwrap();
        if popup == Popup::Help {
            self.popup = None;
            return Ok(());
        }
        let len = match popup {
            Popup::Agents => self.agents.len(),
            Popup::Hosts => self.all_hosts.len(),
            Popup::Help => 0,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('h') => {
                self.popup = None;
                self.recompute();
            }
            KeyCode::Up => self.popup_selected = self.popup_selected.saturating_sub(1),
            KeyCode::Down => {
                if len > 0 {
                    self.popup_selected = (self.popup_selected + 1).min(len - 1);
                }
            }
            KeyCode::Char(' ') => {
                if popup == Popup::Agents {
                    if let Some(agent) = self.agents.get(self.popup_selected).copied() {
                        toggle(&mut self.selected_agents, agent);
                    }
                } else if popup == Popup::Hosts {
                    if let Some(host) = self.all_hosts.get(self.popup_selected).cloned() {
                        toggle(&mut self.selected_hosts, host);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_config_key(&mut self, key: KeyEvent) -> Result<()> {
        let mut save_action: Option<bool> = None;
        let mut close = false;
        {
            let Some(form) = self.config_page.as_mut() else {
                return Ok(());
            };
            if let Some(buffer) = form.editing.clone() {
                match key.code {
                    KeyCode::Enter => form.commit_edit(buffer),
                    KeyCode::Esc => form.editing = None,
                    KeyCode::Tab => {
                        form.commit_edit(buffer);
                        form.next(1);
                    }
                    KeyCode::BackTab => {
                        form.commit_edit(buffer);
                        form.next(-1);
                    }
                    KeyCode::Backspace => {
                        let mut b = buffer;
                        b.pop();
                        form.editing = Some(b);
                    }
                    KeyCode::Char(c) => {
                        let mut b = buffer;
                        b.push(c);
                        form.editing = Some(b);
                    }
                    _ => {}
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => close = true,
                KeyCode::Tab | KeyCode::Down => form.next(1),
                KeyCode::BackTab | KeyCode::Up => form.next(-1),
                KeyCode::Enter | KeyCode::Char(' ') => match form.current() {
                    ConfigRow::ArchiveDir | ConfigRow::HostName | ConfigRow::Timezone => {
                        form.begin_edit()
                    }
                    row
                    @ (ConfigRow::SyncOnStart | ConfigRow::AllowCloud | ConfigRow::Agent(_)) => {
                        form.toggle(row)
                    }
                    ConfigRow::Save => save_action = Some(false),
                    ConfigRow::SaveSync => save_action = Some(true),
                    ConfigRow::Cancel => close = true,
                },
                _ => {}
            }
        }

        let mut run_sync_after = false;
        if let Some(sync) = save_action {
            let result = self
                .config_page
                .as_ref()
                .map(|form| form.save())
                .unwrap_or_else(|| Ok(std::path::PathBuf::new()));
            match result {
                Ok(path) => {
                    if let Some(form) = self.config_page.as_mut() {
                        form.message = format!("saved {}", path.display());
                    }
                    if let Some(form) = self.config_page.as_ref() {
                        self.apply_config(form.cfg.clone());
                    }
                    if sync {
                        close = true;
                        run_sync_after = true;
                    }
                }
                Err(e) => {
                    if let Some(form) = self.config_page.as_mut() {
                        form.message = format!("save failed: {e:#}");
                    }
                }
            }
        }
        if close {
            self.config_page = None;
            if run_sync_after {
                self.start_sync();
            }
        }
        Ok(())
    }

    fn apply_config(&mut self, cfg: Config) {
        self.cfg = cfg;
        self.tz = self.cfg.timezone();
        self.recompute();
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        if self.config_page.is_some() {
            self.draw_config_page(f, area);
            if let Some(view) = &self.sync_view {
                draw_sync_popup(f, area, view);
            }
            return;
        }
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);
        self.draw_header(f, chunks[0]);
        self.draw_tabs(f, chunks[1]);
        match self.tab {
            0 => self.draw_overview(f, chunks[2]),
            1 => self.draw_group_table(f, chunks[2], "Timeline", &self.timeline),
            2 => self.draw_group_table(f, chunks[2], "Models", &self.models),
            _ => {}
        }
        self.draw_footer(f, chunks[3]);
        if let Some(view) = &self.sync_view {
            draw_sync_popup(f, area, view);
        }
        if let Some(popup) = self.popup {
            self.draw_popup(f, area, popup);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let agents = if self.selected_agents.len() == self.agents.len() {
            "all".to_string()
        } else {
            self.active_agents()
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        let hosts = if self.selected_hosts.len() == self.all_hosts.len() {
            "all".to_string()
        } else {
            self.active_hosts().join(",")
        };
        let line = Line::from(vec![
            Span::styled(
                " agentarc ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · range: "),
            Span::styled(
                format!("[{}]", self.range.label()),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" · agents: "),
            Span::styled(agents, Style::default().fg(Color::Green)),
            Span::raw(" · hosts: "),
            Span::styled(hosts, Style::default().fg(Color::Green)),
            Span::raw(" · last sync "),
            Span::styled(
                self.last_sync
                    .clone()
                    .unwrap_or_else(|| "never".to_string()),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let titles: Vec<Line> = TABS
            .iter()
            .enumerate()
            .map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t)))
            .collect();
        let tabs = Tabs::new(titles)
            .select(self.tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .divider("");
        f.render_widget(tabs, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let status = if self.status.is_empty() {
            if self.sync_view.is_some() {
                "sync/index running in the background…".to_string()
            } else {
                "ready".to_string()
            }
        } else {
            self.status.clone()
        };
        let status_color = if self.sync_view.is_some() {
            Color::Yellow
        } else {
            Color::Green
        };
        let lines = Text::from(vec![
            Line::from(vec![
                Span::styled(" status: ", Style::default().fg(Color::DarkGray)),
                Span::styled(status, Style::default().fg(status_color)),
            ]),
            Line::from(Span::styled(
                " Tab/1-3 tabs · r range · a agents · h hosts · g group · s sync · c config · t heatmap · ? help · q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_overview(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Min(8),
        ])
        .split(area);
        let t = &self.timeline.totals;
        let totals = Text::from(vec![
            Line::from(vec![
                Span::styled("TOTAL ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    util::human_tokens(t.tokens.total),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" tokens  ·  "),
                Span::styled(
                    util::human_usd(t.cost.usd),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                if t.cost.unknown_events > 0 {
                    Span::styled(
                        format!(" (+{} unpriced)", t.cost.unknown_events),
                        Style::default().fg(Color::Red),
                    )
                } else {
                    Span::raw("")
                },
                Span::raw("  ·  "),
                Span::styled(
                    format!("{} sessions", t.sessions),
                    Style::default().fg(Color::Green),
                ),
            ]),
            Line::from(vec![
                Span::raw("      "),
                Span::styled(
                    format!("fresh {}", util::human_tokens(t.tokens.fresh_input)),
                    Style::default().fg(Color::Gray),
                ),
                Span::raw("  ·  "),
                Span::styled(
                    format!("cache read {}", util::human_tokens(t.tokens.cache_read)),
                    Style::default().fg(Color::Blue),
                ),
                Span::raw("  ·  "),
                Span::styled(
                    format!("cache write {}", util::human_tokens(t.tokens.cache_write)),
                    Style::default().fg(Color::Magenta),
                ),
                Span::raw("  ·  "),
                Span::styled(
                    format!("output {}", util::human_tokens(t.tokens.output)),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  ·  "),
                Span::styled(
                    format!("reasoning {}", util::human_tokens(t.tokens.reasoning)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        ]);
        f.render_widget(
            Paragraph::new(totals).block(Block::bordered().title(" Totals ")),
            chunks[0],
        );

        let agent_rows = stats::agent_totals(&self.visible_events, &self.pricing);
        let total_tokens = t.tokens.total.max(1);
        let rows: Vec<Row> = agent_rows
            .iter()
            .map(|(agent, tokens, cost, sessions)| {
                Row::new(vec![
                    Cell::from(agent.display_name()),
                    Cell::from(util::human_tokens(tokens.total)),
                    Cell::from(util::human_tokens(tokens.fresh_input)),
                    Cell::from(util::human_tokens(tokens.cache_read)),
                    Cell::from(util::human_tokens(tokens.cache_write)),
                    Cell::from(util::human_tokens(tokens.output)),
                    Cell::from(util::human_usd(cost.usd)),
                    Cell::from(if cost.unknown_events > 0 {
                        "?".to_string()
                    } else {
                        format!("{:.1}%", tokens.total as f64 / total_tokens as f64 * 100.0)
                    }),
                    Cell::from(sessions.to_string()),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(21),
                Constraint::Percentage(11),
                Constraint::Percentage(10),
                Constraint::Percentage(11),
                Constraint::Percentage(11),
                Constraint::Percentage(10),
                Constraint::Percentage(11),
                Constraint::Percentage(7),
                Constraint::Percentage(8),
            ],
        )
        .header(
            Row::new(vec![
                "agent", "tokens", "fresh", "cache rd", "cache wr", "output", "cost", "share",
                "sessions",
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::bordered().title(" By agent "));
        f.render_widget(table, chunks[1]);

        let lines = self.heatmap_lines();
        let title = format!(
            " Heatmap · last 53 weeks · {} ",
            if self.heatmap_by_cost {
                "cost"
            } else {
                "tokens"
            }
        );
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(title))
                .wrap(Wrap { trim: false }),
            chunks[2],
        );
    }

    fn heatmap_lines(&self) -> Vec<Line<'static>> {
        let now = util::now_ms();
        let today_start = util::today_start_ms(&self.tz, now);
        let offset = match jiff::Timestamp::from_millisecond(today_start).map(|t| {
            t.to_zoned(self.tz.clone())
                .date()
                .weekday()
                .to_monday_zero_offset()
        }) {
            Ok(v) => v as i64,
            Err(_) => 0,
        };
        let start = today_start - (52 * 7 + offset) * 86_400_000;
        let series = stats::daily_series(&self.visible_events, &self.tz, &self.pricing);
        let value = |date: &str| -> f64 {
            series
                .get(date)
                .map(|(tokens, cost)| {
                    if self.heatmap_by_cost {
                        *cost
                    } else {
                        *tokens as f64
                    }
                })
                .unwrap_or(0.0)
        };
        let mut values: Vec<f64> = series
            .iter()
            .map(|(_, (tokens, cost))| {
                if self.heatmap_by_cost {
                    *cost
                } else {
                    *tokens as f64
                }
            })
            .filter(|v| *v > 0.0)
            .collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |p: f64| -> f64 {
            if values.is_empty() {
                0.0
            } else {
                values[((values.len() as f64 * p) as usize).min(values.len() - 1)]
            }
        };
        let thresholds = [q(0.2), q(0.4), q(0.6), q(0.8)];
        let chars = ['·', '░', '▒', '▓', '█'];
        let mut lines = Vec::new();
        for row in 0..7 {
            let label = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][row];
            let mut spans = vec![Span::styled(
                format!(" {label} "),
                Style::default().fg(Color::DarkGray),
            )];
            for col in 0..53 {
                let ms = start + ((col * 7 + row) as i64) * 86_400_000;
                let date = util::date_str(ms, &self.tz);
                let v = value(&date);
                let level = if v <= 0.0 {
                    0
                } else {
                    let mut lvl = 1;
                    for th in thresholds {
                        if v > th {
                            lvl += 1;
                        }
                    }
                    lvl.min(4)
                };
                let color = match level {
                    0 => Color::DarkGray,
                    1 => Color::Green,
                    2 => Color::LightGreen,
                    3 => Color::Yellow,
                    _ => Color::LightRed,
                };
                spans.push(Span::styled(
                    chars[level].to_string(),
                    Style::default().fg(color),
                ));
            }
            lines.push(Line::from(spans));
        }
        let mut max = 0.0f64;
        for (tokens, cost) in series.values() {
            let v = if self.heatmap_by_cost {
                *cost
            } else {
                *tokens as f64
            };
            max = max.max(v);
        }
        lines.push(Line::from(vec![
            Span::styled("      ", Style::default()),
            Span::styled(
                format!(
                    "· none   ░ ▒ ▓ █  quantiles   peak {}",
                    if self.heatmap_by_cost {
                        util::human_usd(max)
                    } else {
                        util::human_tokens(max as i64)
                    }
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines
    }

    fn draw_group_table(&self, f: &mut Frame, area: Rect, title: &str, stats: &StatsResult) {
        let headers = match stats.group_by.as_str() {
            "day" | "week" | "month" => vec![
                "period", "tokens", "fresh", "cache rd", "cache wr", "output", "cost", "sessions",
            ],
            _ => vec![
                "name", "tokens", "fresh", "cache rd", "cache wr", "output", "cost", "sessions",
            ],
        };
        let capacity = area.height.saturating_sub(3) as usize;
        let offset = self.selected.saturating_sub(capacity.saturating_sub(1));
        let rows: Vec<Row> = stats
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(capacity.max(1))
            .map(|(i, r)| {
                let mut row = Row::new(vec![
                    Cell::from(r.key.clone()),
                    Cell::from(util::human_tokens(r.tokens.total)),
                    Cell::from(util::human_tokens(r.tokens.fresh_input)),
                    Cell::from(util::human_tokens(r.tokens.cache_read)),
                    Cell::from(util::human_tokens(r.tokens.cache_write)),
                    Cell::from(util::human_tokens(r.tokens.output)),
                    Cell::from(if r.cost.unknown_events > 0 {
                        format!("{} ?", util::human_usd(r.cost.usd))
                    } else {
                        util::human_usd(r.cost.usd)
                    }),
                    Cell::from(r.sessions.to_string()),
                ]);
                if i == self.selected {
                    row = row.style(Style::default().bg(Color::DarkGray));
                }
                row
            })
            .collect();
        let block = Block::bordered().title(format!(
            " {title} · group: {} · {}{} ",
            stats.group_by,
            if stats.unpriced_events > 0 {
                format!("{} unpriced · ", stats.unpriced_events)
            } else {
                String::new()
            },
            util::human_usd(stats.totals.cost.usd)
        ));
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(28),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(12),
                Constraint::Percentage(10),
            ],
        )
        .header(
            Row::new(headers).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(block);
        f.render_widget(table, area);
    }

    fn draw_config_page(&self, f: &mut Frame, area: Rect) {
        let Some(form) = self.config_page.as_ref() else {
            return;
        };
        let inner_width = area.width.saturating_sub(4) as usize;
        let mut lines: Vec<Line> = Vec::new();
        if form.first_run {
            lines.push(Line::from(Span::styled(
                " First-time setup — choose where to archive and which agents to sync.",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(" config: ", Style::default().fg(Color::DarkGray)),
            Span::raw(util::config_path().display().to_string()),
        ]));
        lines.push(Line::from(""));
        let rows = form.rows();
        for (idx, row) in rows.iter().enumerate() {
            if matches!(row, ConfigRow::Agent(_))
                && !matches!(rows.get(idx.wrapping_sub(1)), Some(ConfigRow::Agent(_)))
            {
                lines.push(Line::from(Span::styled(
                    " Agents to sync (already-archived data is always counted):",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            let selected = idx == form.selected;
            let editing = selected && form.editing.is_some();
            let label = config_row_label(*row);
            let text = match row {
                ConfigRow::Save => "  [ Save ]".to_string(),
                ConfigRow::SaveSync => "  [ Save & sync ]".to_string(),
                ConfigRow::Cancel => "  [ Cancel ]".to_string(),
                ConfigRow::SyncOnStart | ConfigRow::AllowCloud | ConfigRow::Agent(_) => {
                    format!(
                        "  {label:<22} [{}]",
                        if form.value(*row) == "on" { "x" } else { " " }
                    )
                }
                _ => {
                    let value = if editing {
                        format!("{}▌", form.editing.clone().unwrap_or_default())
                    } else {
                        form.value(*row)
                    };
                    let max = inner_width.saturating_sub(26);
                    format!("  {label:<22} {}", util::truncate_chars(&value, max.max(8)))
                }
            };
            let mut style = Style::default();
            if selected {
                style = style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            }
            if editing {
                style = style.fg(Color::Yellow);
            }
            lines.push(Line::from(Span::styled(
                util::truncate_chars(&text, inner_width.max(20)),
                style,
            )));
        }
        lines.push(Line::from(""));
        if form.cfg.cloud_archive_reason().is_some() && !form.cfg.allow_cloud_archive {
            lines.push(Line::from(Span::styled(
                " note: archive folder is inside a cloud-sync location; enable Allow cloud archive or sync will refuse.",
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::from(Span::styled(
            " ↑↓/Tab move · Enter/Space edit or toggle · editing: Enter commit, Esc cancel",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            " Esc/c cancel and go back · Save & sync starts archiving immediately",
            Style::default().fg(Color::DarkGray),
        )));
        if !form.message.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" {}", form.message),
                Style::default().fg(Color::Green),
            )));
        }
        let title = if form.first_run {
            " Configuration — first-time setup "
        } else {
            " Configuration "
        };
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(title))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_popup(&self, f: &mut Frame, area: Rect, popup: Popup) {
        let (width, height) = match popup {
            Popup::Agents => (46, (self.agents.len() as u16 + 4).min(area.height)),
            Popup::Hosts => (46, (self.all_hosts.len() as u16 + 4).min(area.height)),
            Popup::Help => (76, 18),
        };
        let popup_area = centered(area, width, height);
        f.render_widget(Clear, popup_area);
        match popup {
            Popup::Agents | Popup::Hosts => {
                let items: Vec<ListItem> = if popup == Popup::Agents {
                    self.agents
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let on = self.selected_agents.contains(a);
                            ListItem::new(format!(
                                " {} {:<22} {}",
                                if i == self.popup_selected { ">" } else { " " },
                                a.display_name(),
                                if on { "[x]" } else { "[ ]" }
                            ))
                        })
                        .collect()
                } else {
                    self.all_hosts
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let on = self.selected_hosts.contains(h);
                            ListItem::new(format!(
                                " {} {:<22} {}",
                                if i == self.popup_selected { ">" } else { " " },
                                h,
                                if on { "[x]" } else { "[ ]" }
                            ))
                        })
                        .collect()
                };
                let title = if popup == Popup::Agents {
                    " Agents (space toggles, enter closes) "
                } else {
                    " Hosts (space toggles, enter closes) "
                };
                f.render_widget(
                    List::new(items).block(Block::bordered().title(title)),
                    popup_area,
                );
            }
            Popup::Help => {
                let text = Text::from(vec![
                    Line::from(" agentarc — keyboard help"),
                    Line::from(""),
                    Line::from(" Tab / 1-3    cycle Overview → Timeline → Models → Overview"),
                    Line::from(" Shift+Tab    cycle the other way"),
                    Line::from(" c            open the configuration page"),
                    Line::from(" r            cycle range"),
                    Line::from(" a / h        toggle agents / hosts"),
                    Line::from(" g            Timeline grouping: day / week / month"),
                    Line::from(" s            sync now (runs in the background)"),
                    Line::from(" t            Overview heatmap metric (tokens/cost)"),
                    Line::from(" ↑↓/PgUp/PgDn  scroll the current table"),
                    Line::from(" ?            this help"),
                    Line::from(" q / Esc      quit"),
                    Line::from(""),
                    Line::from(" The configuration page is also shown on first run."),
                ]);
                f.render_widget(
                    Paragraph::new(text)
                        .block(Block::bordered().title(" Help "))
                        .wrap(Wrap { trim: false }),
                    popup_area,
                );
            }
        }
    }
}

fn draw_sync_popup(f: &mut Frame, area: Rect, view: &SyncView) {
    let width = 92.min(area.width);
    let height = 8.min(area.height);
    let popup_area = centered(area, width, height);
    f.render_widget(Clear, popup_area);
    let ratio = if view.total == 0 {
        0.0
    } else {
        (view.done as f64 / view.total as f64).clamp(0.0, 1.0)
    };
    let bar_width = (width.saturating_sub(24) as usize).max(10);
    let filled = (ratio * bar_width as f64).round() as usize;
    let bar = format!(
        "[{}{}] {:>3.0}%",
        "█".repeat(filled),
        "░".repeat(bar_width.saturating_sub(filled)),
        ratio * 100.0
    );
    let phase = match view.phase.as_str() {
        "discovering" => "discovering source files",
        "syncing" => "copying changed files",
        "indexing" | "parsing" => "indexing parsed files",
        "done" => "finishing",
        other => other,
    };
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled(
                format!(" {phase} "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(format!(
            " files {}/{} · copied {} · {} · versioned {}",
            view.done,
            view.total,
            view.copied,
            util::human_bytes(view.bytes),
            view.versioned
        )),
        Line::from(format!(
            " current: {}",
            if view.current.is_empty() {
                "—".to_string()
            } else {
                util::truncate_chars(&view.current, width.saturating_sub(14) as usize)
            }
        )),
        Line::from(format!(" elapsed: {}", view.elapsed())),
    ]);
    f.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(" Sync progress "))
            .wrap(Wrap { trim: true }),
        popup_area,
    );
}

fn toggle<T: std::hash::Hash + Eq + Clone>(set: &mut HashSet<T>, value: T) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn run_sync_job(cfg: Config, tx: Sender<SyncUpdate>) {
    let tx_progress = tx.clone();
    let opts = archive::SyncOptions::default();
    let result = archive::run_sync_with_progress(&cfg, &opts, move |p| {
        let _ = tx_progress.send(SyncUpdate::Sync(p.clone()));
    });
    match result {
        Ok(summary) => run_index_job(cfg, tx, Some(summary.summary_line())),
        Err(e) => {
            let _ = tx.send(SyncUpdate::Finished {
                ok: false,
                message: format!("sync failed: {e:#}"),
            });
        }
    }
}

fn run_index_job(cfg: Config, tx: Sender<SyncUpdate>, sync_line: Option<String>) {
    let index_path = cfg.index_path();
    let mut idx = match Index::open(&index_path) {
        Ok(idx) => idx,
        Err(e) => {
            let _ = tx.send(SyncUpdate::Finished {
                ok: false,
                message: format!("index open failed: {e:#}"),
            });
            return;
        }
    };
    let tx_progress = tx.clone();
    let result = idx.ingest_with_progress(&cfg, &IngestOptions::default(), move |p| {
        let _ = tx_progress.send(SyncUpdate::Index(p.clone()));
    });
    match result {
        Ok(stats) => {
            let _ = idx.set_meta(
                "last_sync",
                &serde_json::json!({
                    "at": util::now_ms(),
                    "host": cfg.host_name(),
                    "summary": sync_line,
                })
                .to_string(),
            );
            let suffix = sync_line.map(|s| format!(" · {s}")).unwrap_or_default();
            let _ = tx.send(SyncUpdate::Finished {
                ok: true,
                message: format!(
                    "indexed {} file(s), {} usage event(s){suffix}",
                    stats.files_parsed, stats.usage
                ),
            });
        }
        Err(e) => {
            let _ = tx.send(SyncUpdate::Finished {
                ok: false,
                message: format!("index failed: {e:#}"),
            });
        }
    }
}

pub fn run(cfg: Config) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "the TUI needs an interactive terminal; use a subcommand (try `agentarc --help`)"
        );
    }
    let index = Index::open(&cfg.index_path())?;
    let pricing = Pricing::load(&cfg, true);
    let mut app = App::new(cfg, pricing, index)?;

    let mut terminal = ratatui::init();
    if app.config_page.is_none() {
        if app.cfg.sync_on_start {
            app.start_sync();
        } else if app.index.last_index_ms() == 0 {
            app.start_index();
        }
    }
    let result = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_sync();
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                app.on_key(key)?;
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}
