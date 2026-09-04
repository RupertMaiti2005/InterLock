//! Live dashboard: agents, files, events. Bare `interlock` opens this. SPEC §7.

use anyhow::Result;
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use interlock_core::client::Client;
use interlock_core::{fmt_ms, Event, Request, Response, SessionState, StatusSnapshot};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Agents,
    Files,
}

struct App {
    repo_root: PathBuf,
    snap: Option<StatusSnapshot>,
    events: VecDeque<Event>,
    pane: Pane,
    sel_agent: usize,
    sel_file: usize,
    flash: Option<(String, Instant)>,
    last_ok: Option<Instant>,
}

fn short(s: &str) -> String {
    interlock_core::short_id(s)
}

/// Run the dashboard until the user quits. `stop` lets a driver (demo) end it.
pub fn run(repo_root: &Path, stop: Option<Arc<std::sync::atomic::AtomicBool>>) -> Result<()> {
    let events: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_subscriber(repo_root.to_path_buf(), events.clone());

    let mut terminal = ratatui::init();
    let mut app = App {
        repo_root: repo_root.to_path_buf(),
        snap: None,
        events: VecDeque::new(),
        pane: Pane::Agents,
        sel_agent: 0,
        sel_file: 0,
        flash: None,
        last_ok: None,
    };
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    let result = loop {
        if stop.as_ref().map(|s| s.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(false) {
            break Ok(());
        }
        if last_poll.elapsed() >= Duration::from_millis(400) {
            app.snap = fetch_status(&app.repo_root);
            if app.snap.is_some() {
                app.last_ok = Some(Instant::now());
            }
            last_poll = Instant::now();
        }
        {
            let mut ev = events.lock().unwrap();
            while let Some(e) = ev.pop_front() {
                app.events.push_front(e);
            }
            while app.events.len() > 500 {
                app.events.pop_back();
            }
        }
        if let Err(e) = terminal.draw(|f| draw(f, &app)) {
            break Err(e.into());
        }
        if event::poll(Duration::from_millis(100))? {
            if let CEvent::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => break Ok(()),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => break Ok(()),
                    (KeyCode::Tab, _) => app.pane = if app.pane == Pane::Agents { Pane::Files } else { Pane::Agents },
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) if false => {}
                    (KeyCode::Up, _) => app.move_sel(-1),
                    (KeyCode::Down, _) => app.move_sel(1),
                    (KeyCode::Char('r'), _) => app.release_selected(),
                    (KeyCode::Char('u'), _) => app.undo_selected(),
                    (KeyCode::Char('k'), _) => app.reap_selected(),
                    _ => {}
                }
            }
        }
    };
    ratatui::restore();
    result
}

fn spawn_subscriber(repo_root: PathBuf, sink: Arc<Mutex<VecDeque<Event>>>) {
    std::thread::spawn(move || loop {
        match Client::connect(&repo_root, Duration::from_millis(300)) {
            Ok(c) => {
                if let Ok(iter) = c.subscribe() {
                    for ev in iter.flatten() {
                        sink.lock().unwrap().push_back(ev);
                    }
                }
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(800));
    });
}

fn fetch_status(repo_root: &Path) -> Option<StatusSnapshot> {
    let mut c = Client::connect(repo_root, Duration::from_millis(200)).ok()?;
    match c.request(&Request::Status, Duration::from_secs(1)).ok()? {
        Response::Status(s) => Some(s),
        _ => None,
    }
}

impl App {
    fn move_sel(&mut self, d: i32) {
        let Some(s) = &self.snap else { return };
        match self.pane {
            Pane::Agents => {
                let n = s.sessions.len();
                if n > 0 {
                    self.sel_agent = (self.sel_agent as i32 + d).rem_euclid(n as i32) as usize;
                }
            }
            Pane::Files => {
                let n = s.files.len();
                if n > 0 {
                    self.sel_file = (self.sel_file as i32 + d).rem_euclid(n as i32) as usize;
                }
            }
        }
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn request(&mut self, req: Request) -> Option<Response> {
        let mut c = Client::connect(&self.repo_root, Duration::from_millis(300)).ok()?;
        c.request(&req, Duration::from_secs(3)).ok()
    }

    fn release_selected(&mut self) {
        let Some(s) = &self.snap else { return };
        let cwd = self.repo_root.to_string_lossy().to_string();
        let path = match self.pane {
            Pane::Files => s.files.get(self.sel_file).filter(|f| f.holder.is_some()).map(|f| f.path.clone()),
            Pane::Agents => s.sessions.get(self.sel_agent).and_then(|a| a.held.first().cloned()),
        };
        match path {
            Some(p) => match self.request(Request::ForceRelease { path: p.clone(), cwd }) {
                Some(Response::Ok) => self.flash(format!("released {p}")),
                Some(Response::Error { message }) => self.flash(message),
                _ => self.flash("daemon unreachable"),
            },
            None => self.flash("nothing leased on the selection"),
        }
    }

    fn undo_selected(&mut self) {
        let Some(s) = &self.snap else { return };
        let cwd = self.repo_root.to_string_lossy().to_string();
        let Some(p) = s.files.get(self.sel_file).map(|f| f.path.clone()) else {
            self.flash("select a file (Tab) to undo");
            return;
        };
        match self.request(Request::Undo { path: p.clone(), cwd, steps: 1 }) {
            Some(Response::Restored { blob_oid, .. }) => self.flash(format!("restored {p} from {}", &blob_oid[..8])),
            Some(Response::Error { message }) => self.flash(message),
            _ => self.flash("daemon unreachable"),
        }
    }

    fn reap_selected(&mut self) {
        let Some(s) = &self.snap else { return };
        let Some(id) = s.sessions.get(self.sel_agent).map(|a| a.id.clone()) else { return };
        match self.request(Request::Reap { session: id.clone() }) {
            Some(Response::Ok) => self.flash(format!("reaped {}", short(&id))),
            Some(Response::Error { message }) => self.flash(message),
            _ => self.flash("daemon unreachable"),
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(agents_height(app)),
            Constraint::Length(files_height(app)),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(f, chunks[0], app);
    draw_agents(f, chunks[1], app);
    draw_files(f, chunks[2], app);
    draw_events(f, chunks[3], app);
    draw_footer(f, chunks[4], app);
}

fn agents_height(app: &App) -> u16 {
    let n = app.snap.as_ref().map(|s| s.sessions.len()).unwrap_or(0).max(1) as u16;
    (n + 2).min(12)
}

fn files_height(app: &App) -> u16 {
    let n = app.snap.as_ref().map(|s| s.files.len()).unwrap_or(0).max(1) as u16;
    (n + 2).min(10)
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let name = app.repo_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let line = match &app.snap {
        Some(s) => {
            let mut spans = vec![
                Span::styled(" interlock ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("· {name} · {} agent{} · ", s.sessions.len(), if s.sessions.len() == 1 { "" } else { "s" })),
            ];
            if s.degraded.is_empty() {
                spans.push(Span::styled("daemon ok", Style::default().fg(Color::Green)));
            } else {
                spans.push(Span::styled(format!("DEGRADED: {}", s.degraded.join("; ")), Style::default().fg(Color::Yellow)));
            }
            spans.push(Span::raw(format!(" · up {} · {} hotspot{}", fmt_ms(s.uptime_ms), s.hotspots.len(), if s.hotspots.len() == 1 { "" } else { "s" })));
            Line::from(spans)
        }
        None => Line::from(vec![
            Span::styled(" interlock ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("· {name} · ")),
            Span::styled(
                "DAEMON NOT RUNNING — agent writes are passing through unchecked",
                Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_agents(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Agents;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" AGENTS ")
        .border_style(if focused { Style::default().fg(Color::Cyan) } else { Style::default() });
    let rows: Vec<Row> = match &app.snap {
        Some(s) if !s.sessions.is_empty() => s
            .sessions
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let (mark, mark_style, state, detail) = match &a.state {
                    SessionState::Editing { paths } => ("●", Style::default().fg(Color::Green), "EDITING", paths.join(", ")),
                    SessionState::Waiting { path, holder, waited_ms, cap_ms } => (
                        "◐",
                        Style::default().fg(Color::Yellow),
                        "WAITING",
                        format!("{path}  ← {}   {} / {}", short(holder), fmt_ms(*waited_ms), fmt_ms(*cap_ms)),
                    ),
                    SessionState::Idle => ("○", Style::default().fg(Color::DarkGray), "IDLE", format!("last seen {} ago", fmt_ms(a.last_seen_ms))),
                };
                let label = a.label.as_deref().map(|l| format!("\"{l}\"")).unwrap_or_default();
                let mut row = Row::new(vec![
                    Cell::from(Span::styled(mark, mark_style)),
                    Cell::from(a.harness.short()),
                    Cell::from(short(&a.id)),
                    Cell::from(label),
                    Cell::from(Span::styled(state, mark_style.add_modifier(Modifier::BOLD))),
                    Cell::from(detail),
                ]);
                if focused && i == app.sel_agent {
                    row = row.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                row
            })
            .collect(),
        _ => vec![Row::new(vec![Cell::from(""), Cell::from(Span::styled("no agents yet — the first hooked tool call registers one", Style::default().fg(Color::DarkGray)))])],
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(34),
            Constraint::Length(8),
            Constraint::Min(20),
        ],
    )
    .block(block);
    f.render_widget(table, area);
}

fn draw_files(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Files;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" FILES ")
        .border_style(if focused { Style::default().fg(Color::Cyan) } else { Style::default() });
    let rows: Vec<Row> = match &app.snap {
        Some(s) if !s.files.is_empty() => s
            .files
            .iter()
            .enumerate()
            .map(|(i, fi)| {
                let holder = match &fi.holder {
                    Some(h) => Span::styled(format!("held by {} for {}", short(h), fmt_ms(fi.held_for_ms)), Style::default().fg(Color::Green)),
                    None => Span::styled("free", Style::default().fg(Color::DarkGray)),
                };
                let waiting = if fi.waiters.is_empty() {
                    Span::raw("")
                } else {
                    Span::styled(format!("{} waiting", fi.waiters.len()), Style::default().fg(Color::Yellow))
                };
                let hot = if fi.hotspot { Span::styled("⚠ hotspot", Style::default().fg(Color::Magenta)) } else { Span::raw("") };
                let snaps = if fi.snapshots > 0 { format!("{} snapshot{}", fi.snapshots, if fi.snapshots == 1 { "" } else { "s" }) } else { String::new() };
                let mut row = Row::new(vec![
                    Cell::from(fi.path.clone()),
                    Cell::from(holder),
                    Cell::from(waiting),
                    Cell::from(hot),
                    Cell::from(snaps),
                ]);
                if focused && i == app.sel_file {
                    row = row.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                row
            })
            .collect(),
        _ => vec![Row::new(vec![Cell::from(Span::styled("nothing leased", Style::default().fg(Color::DarkGray)))])],
    };
    let table = Table::new(
        rows,
        [Constraint::Min(30), Constraint::Length(30), Constraint::Length(11), Constraint::Length(10), Constraint::Length(13)],
    )
    .block(block);
    f.render_widget(table, area);
}

fn draw_events(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" EVENTS ");
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = app
        .events
        .iter()
        .take(inner_h)
        .map(|ev| {
            let secs = ev.at_ms / 1000;
            let ts = format!("{:02}:{:02}:{:02}  ", (secs / 3600) % 24, (secs / 60) % 60, secs % 60);
            let text = ev.kind.describe();
            let style = match &ev.kind {
                interlock_core::EventKind::StaleBlocked { .. } => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                interlock_core::EventKind::Deadlock { .. } => Style::default().fg(Color::Red),
                interlock_core::EventKind::Waiting { .. } | interlock_core::EventKind::WaitTimeout { .. } => Style::default().fg(Color::Yellow),
                interlock_core::EventKind::Granted { .. } | interlock_core::EventKind::Acquired { .. } => Style::default().fg(Color::Green),
                interlock_core::EventKind::Restored { .. } | interlock_core::EventKind::Snapshot { .. } => Style::default().fg(Color::Blue),
                interlock_core::EventKind::Degraded { .. } => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                interlock_core::EventKind::SessionReaped { .. } => Style::default().fg(Color::Magenta),
                _ => Style::default(),
            };
            Line::from(vec![Span::styled(ts, Style::default().fg(Color::DarkGray)), Span::styled(text, style)])
        })
        .collect();
    let lines = if lines.is_empty() {
        vec![Line::from(Span::styled("no events yet", Style::default().fg(Color::DarkGray)))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = match &app.flash {
        Some((msg, at)) if at.elapsed() < Duration::from_secs(4) => {
            Line::from(Span::styled(format!(" {msg}"), Style::default().fg(Color::Cyan)))
        }
        _ => Line::from(vec![
            Span::styled(" [Tab]", Style::default().fg(Color::Cyan)),
            Span::raw(" pane  "),
            Span::styled("[↑↓]", Style::default().fg(Color::Cyan)),
            Span::raw(" select  "),
            Span::styled("[r]", Style::default().fg(Color::Cyan)),
            Span::raw(" release lease  "),
            Span::styled("[u]", Style::default().fg(Color::Cyan)),
            Span::raw(" undo file  "),
            Span::styled("[k]", Style::default().fg(Color::Cyan)),
            Span::raw(" reap session  "),
            Span::styled("[q]", Style::default().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]),
    };
    f.render_widget(Paragraph::new(line), area);
}
