use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use lofty::prelude::AudioFile;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Row, Table, TableState},
    Terminal,
};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "wav", "flac", "ogg", "oga", "m4a", "aac", "mp4", "opus", "aiff", "aif", "mkv", "wma",
];
const SEEK_SMALL: Duration = Duration::from_secs(5);
const SEEK_BIG: Duration = Duration::from_secs(30);

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    created: Option<SystemTime>,
    duration: Option<Duration>,
    duration_pending: bool,
}

/// Result of the background duration probe: (scan generation, file, duration).
type DurationMsg = (u64, PathBuf, Option<Duration>);

struct Player {
    stream: OutputStream,
    sink: Option<Sink>,
    current: Option<PathBuf>,
    total: Option<Duration>,
    volume: f32,
}

impl Player {
    fn new() -> Result<Self> {
        let stream = OutputStreamBuilder::open_default_stream()?;
        Ok(Self {
            stream,
            sink: None,
            current: None,
            total: None,
            volume: 1.0,
        })
    }

    fn play(&mut self, path: &Path, known_duration: Option<Duration>) -> Result<()> {
        // try_from(File) builds a seekable decoder (it knows the byte length),
        // which plain Decoder::new does not.
        let source = Decoder::try_from(fs::File::open(path)?)?;
        let total = known_duration.or_else(|| source.total_duration());
        let sink = Sink::connect_new(self.stream.mixer());
        sink.set_volume(self.volume);
        sink.append(source);
        if let Some(old) = self.sink.replace(sink) {
            old.stop();
        }
        self.current = Some(path.to_path_buf());
        self.total = total;
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.current = None;
        self.total = None;
    }

    fn toggle_pause(&mut self) {
        if let Some(sink) = &self.sink {
            if sink.is_paused() {
                sink.play();
            } else {
                sink.pause();
            }
        }
    }

    fn is_paused(&self) -> bool {
        self.sink.as_ref().map(|s| s.is_paused()).unwrap_or(false)
    }

    /// True when a track was loaded and its queue has drained (finished playing).
    fn finished(&self) -> bool {
        self.current.is_some() && self.sink.as_ref().map(|s| s.empty()).unwrap_or(true)
    }

    fn position(&self) -> Duration {
        self.sink.as_ref().map(|s| s.get_pos()).unwrap_or_default()
    }

    fn seek_by(&mut self, forward: bool, step: Duration) -> Result<()> {
        let Some(sink) = &self.sink else {
            return Ok(());
        };
        let pos = sink.get_pos();
        let mut target = if forward {
            pos.saturating_add(step)
        } else {
            pos.saturating_sub(step)
        };
        if let Some(total) = self.total {
            let end = total.saturating_sub(Duration::from_millis(500));
            if target > end {
                target = end;
            }
        }
        sink.try_seek(target)
            .map_err(|e| anyhow::anyhow!("seek failed: {e:?}"))
    }

    fn adjust_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 2.0);
        if let Some(sink) = &self.sink {
            sink.set_volume(self.volume);
        }
    }
}

struct App {
    cwd: PathBuf,
    entries: Vec<Entry>,
    table: TableState,
    show_hidden: bool,
    scan_gen: u64,
    dur_tx: Sender<DurationMsg>,
    dur_rx: Receiver<DurationMsg>,
    player: Option<Player>,
    status: Option<String>,
    quit: bool,
}

impl App {
    fn new(cwd: PathBuf) -> Self {
        let (dur_tx, dur_rx) = mpsc::channel();
        let player = match Player::new() {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("audio output unavailable: {e}");
                None
            }
        };
        let mut app = Self {
            cwd,
            entries: Vec::new(),
            table: TableState::default(),
            show_hidden: false,
            scan_gen: 0,
            dur_tx,
            dur_rx,
            player,
            status: None,
            quit: false,
        };
        app.rescan(None);
        app
    }

    fn rescan(&mut self, select: Option<PathBuf>) {
        self.scan_gen += 1;
        self.entries.clear();

        let read = match fs::read_dir(&self.cwd) {
            Ok(r) => r,
            Err(e) => {
                self.status = Some(format!("cannot read {}: {e}", self.cwd.display()));
                return;
            }
        };

        for dent in read.flatten() {
            let path = dent.path();
            let name = dent.file_name().to_string_lossy().into_owned();
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir && !is_audio(&path) {
                continue;
            }
            let created = dent
                .metadata()
                .ok()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok());
            self.entries.push(Entry {
                name,
                path,
                is_dir,
                created,
                duration: None,
                duration_pending: !is_dir,
            });
        }

        self.entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        let idx = select
            .and_then(|p| self.entries.iter().position(|e| e.path == p))
            .unwrap_or(0);
        self.table
            .select(if self.entries.is_empty() { None } else { Some(idx) });

        // Probe durations off the UI thread; results are tagged with scan_gen so
        // late replies from an abandoned directory are ignored.
        let files: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.clone())
            .collect();
        if !files.is_empty() {
            let tx = self.dur_tx.clone();
            let gen = self.scan_gen;
            thread::spawn(move || {
                for path in files {
                    let duration = lofty::read_from_path(&path)
                        .ok()
                        .map(|f| f.properties().duration());
                    if tx.send((gen, path, duration)).is_err() {
                        return;
                    }
                }
            });
        }
    }

    fn drain_durations(&mut self) {
        while let Ok((gen, path, duration)) = self.dur_rx.try_recv() {
            if gen != self.scan_gen {
                continue;
            }
            if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
                entry.duration = duration;
                entry.duration_pending = false;
            }
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.table.selected().and_then(|i| self.entries.get(i))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let cur = self.table.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1);
        self.table.select(Some(next as usize));
    }

    fn enter_selected(&mut self) {
        let Some(entry) = self.selected_entry() else { return };
        if entry.is_dir {
            self.cwd = entry.path.clone();
            self.rescan(None);
        } else {
            self.play_path(entry.path.clone());
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            let from = std::mem::replace(&mut self.cwd, parent);
            self.rescan(Some(from));
        }
    }

    fn play_path(&mut self, path: PathBuf) {
        let known = self
            .entries
            .iter()
            .find(|e| e.path == path)
            .and_then(|e| e.duration);
        let Some(player) = &mut self.player else {
            self.status = Some("no audio output device".into());
            return;
        };
        match player.play(&path, known) {
            Ok(()) => self.status = None,
            Err(e) => self.status = Some(format!("cannot play {}: {e}", path.display())),
        }
    }

    fn seek(&mut self, forward: bool, step: Duration) {
        if let Some(p) = &mut self.player {
            if let Err(e) = p.seek_by(forward, step) {
                self.status = Some(e.to_string());
            }
        }
    }

    /// When a track ends, start the next audio file in the current listing.
    fn auto_advance(&mut self) {
        let finished = self.player.as_ref().map(|p| p.finished()).unwrap_or(false);
        if !finished {
            return;
        }
        let current = self.player.as_ref().and_then(|p| p.current.clone());
        let next = current.and_then(|cur| {
            let idx = self.entries.iter().position(|e| e.path == cur)?;
            self.entries[idx + 1..].iter().find(|e| !e.is_dir).map(|e| e.path.clone())
        });
        match next {
            Some(path) => {
                let idx = self.entries.iter().position(|e| e.path == path);
                if let Some(i) = idx {
                    self.table.select(Some(i));
                }
                self.play_path(path);
            }
            None => {
                if let Some(p) = &mut self.player {
                    p.stop();
                }
            }
        }
    }

    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(15),
            KeyCode::PageUp => self.move_selection(-15),
            KeyCode::Home | KeyCode::Char('g') => {
                if !self.entries.is_empty() {
                    self.table.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.entries.is_empty() {
                    self.table.select(Some(self.entries.len() - 1));
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => self.enter_selected(),
            KeyCode::Backspace | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char(' ') => {
                if let Some(p) = &mut self.player {
                    p.toggle_pause();
                }
            }
            KeyCode::Char('s') => {
                if let Some(p) = &mut self.player {
                    p.stop();
                }
            }
            KeyCode::Right => {
                self.seek(true, if shift { SEEK_BIG } else { SEEK_SMALL });
            }
            KeyCode::Left => {
                self.seek(false, if shift { SEEK_BIG } else { SEEK_SMALL });
            }
            KeyCode::Char(']') => self.seek(true, SEEK_BIG),
            KeyCode::Char('[') => self.seek(false, SEEK_BIG),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if let Some(p) = &mut self.player {
                    p.adjust_volume(0.1);
                }
            }
            KeyCode::Char('-') => {
                if let Some(p) = &mut self.player {
                    p.adjust_volume(-0.1);
                }
            }
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                let keep = self.selected_entry().map(|e| e.path.clone());
                self.rescan(keep);
            }
            KeyCode::Char('r') => {
                let keep = self.selected_entry().map(|e| e.path.clone());
                self.rescan(keep);
            }
            _ => {}
        }
    }
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn fmt_created(t: Option<SystemTime>) -> String {
    t.map(|t| DateTime::<Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [main_area, player_area, help_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // File table
    let header = Row::new(["Name", "Duration", "Created"])
        .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan));
    let rows: Vec<Row> = app
        .entries
        .iter()
        .map(|e| {
            let playing = app
                .player
                .as_ref()
                .and_then(|p| p.current.as_ref())
                .map(|c| *c == e.path)
                .unwrap_or(false);
            let name = if e.is_dir {
                format!("{}/", e.name)
            } else if playing {
                format!("▶ {}", e.name)
            } else {
                e.name.clone()
            };
            let duration = if e.is_dir {
                String::new()
            } else if let Some(d) = e.duration {
                fmt_duration(d)
            } else if e.duration_pending {
                "…".into()
            } else {
                "?".into()
            };
            let style = if playing {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else if e.is_dir {
                Style::default().fg(Color::Blue)
            } else {
                Style::default()
            };
            Row::new([name, duration, fmt_created(e.created)]).style(style)
        })
        .collect();

    let title = format!(" {} ", app.cwd.display());
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(9),
            Constraint::Length(16),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(table, main_area, &mut app.table);

    // Player bar
    let (label, ratio, line) = match app.player.as_ref().filter(|p| p.current.is_some()) {
        Some(p) => {
            let name = p
                .current
                .as_ref()
                .and_then(|c| c.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pos = p.position();
            let state = if p.is_paused() { "⏸" } else { "▶" };
            let (label, ratio) = match p.total {
                Some(total) if !total.is_zero() => (
                    format!("{} / {}", fmt_duration(pos), fmt_duration(total)),
                    (pos.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0),
                ),
                _ => (fmt_duration(pos), 0.0),
            };
            let line = Line::from(vec![
                Span::styled(
                    format!(" {state} {name}"),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("   vol {:>3.0}%", p.volume * 100.0),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            (label, ratio, line)
        }
        None => {
            let vol = app
                .player
                .as_ref()
                .map(|p| format!("   vol {:>3.0}%", p.volume * 100.0))
                .unwrap_or_default();
            let msg = app
                .status
                .clone()
                .unwrap_or_else(|| "nothing playing".into());
            (
                String::new(),
                0.0,
                Line::from(vec![
                    Span::styled(format!(" {msg}"), Style::default().fg(Color::DarkGray)),
                    Span::styled(vol, Style::default().fg(Color::DarkGray)),
                ]),
            )
        }
    };

    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(player_area);
    frame.render_widget(block, player_area);
    let [info_area, gauge_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(line, info_area);
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Green).bg(Color::Black))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, gauge_area);

    // Error status overrides the help line so problems are visible.
    let help = match (&app.status, app.player.as_ref().and_then(|p| p.current.as_ref())) {
        (Some(msg), Some(_)) => Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Red),
        )),
        _ => Line::from(Span::styled(
            " ↵ open/play  ⌫ up  ␣ pause  ←/→ ±5s  [/] ±30s  s stop  +/- vol  . hidden  r refresh  q quit",
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(help, help_area);
}

fn main() -> Result<()> {
    let cwd = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    // Restore the terminal even if we panic mid-draw.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut app = App::new(cwd);

    while !app.quit {
        app.drain_durations();
        app.auto_advance();
        terminal.draw(|f| draw(f, &mut app))?;
        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                app.on_key(key);
            }
        }
    }

    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
