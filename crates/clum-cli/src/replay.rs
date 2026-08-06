//! Interactive asciinema v2 (.cast) replay with seek, speed control, and pause.
//!
//! Uses `avt` (asciinema virtual terminal) for accurate terminal state at any
//! seek position. Renders via ratatui. Single-threaded event loop.

use chrono::TimeZone;

use std::io::{self, BufRead};
use std::path::Path;
use std::time::{Duration, Instant};

use avt::Vt;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use ratatui_crossterm::CrosstermBackend;

struct CastEvent {
    time: f64,
    data: String,
}

struct PlayerState {
    events: Vec<CastEvent>,
    width: usize,
    height: usize,
    current_idx: usize,
    speed: f64,
    idle_limit: Option<f64>,
    paused: bool,
    quit: bool,
    start: Instant,
    last_event_time: f64,
    next_event_at: Duration,
    recording_start: i64,
}

#[derive(Clone)]
pub struct ReplayOptions {
    pub speed: f64,
    pub idle_limit: Option<f64>,
}

pub fn replay(path: &Path, opts: &ReplayOptions) -> anyhow::Result<()> {
    let (events, vt, recording_start) = load_and_prepare(path)?;
    if events.is_empty() {
        eprintln!("no output events in recording");
        return Ok(());
    }

    let total_duration = events.last().map(|e| e.time).unwrap_or(0.0);

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_player(
        &mut terminal,
        vt,
        events,
        total_duration,
        recording_start,
        opts,
    );

    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    drain_stdin();
    result
}

fn load_and_prepare(path: &Path) -> anyhow::Result<(Vec<CastEvent>, Vt, i64)> {
    let file = std::fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    let mut lines = reader.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty cast file"))??;
    let header: serde_json::Value = serde_json::from_str(&header_line)?;
    let width = header["width"].as_u64().unwrap_or(80) as usize;
    let height = header["height"].as_u64().unwrap_or(24) as usize;
    let timestamp = header["timestamp"].as_i64().unwrap_or(0);

    let mut events: Vec<CastEvent> = Vec::new();
    let mut vt = Vt::new(width, height);

    for line in lines {
        let line = line?;
        let event: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let arr = match event.as_array() {
            Some(a) if a.len() >= 3 => a,
            _ => continue,
        };

        let time = arr[0].as_f64().unwrap_or(0.0);
        let kind = arr[1].as_str().unwrap_or("");
        let data = arr[2].as_str().unwrap_or("");

        match kind {
            "exit" => break,
            "o" => {
                vt.feed_str(data);
                events.push(CastEvent {
                    time,
                    data: data.to_string(),
                });
            }
            "r" => {
                if let Ok(size) = serde_json::from_value::<(u16, u16)>(arr[2].clone()) {
                    vt.resize(size.0 as usize, size.1 as usize);
                }
            }
            _ => {}
        }
    }

    Ok((events, vt, timestamp))
}

fn rebuild_vt(events: &[CastEvent], target_idx: usize, width: usize, height: usize) -> Vt {
    let mut vt = Vt::new(width, height);
    for (i, ev) in events.iter().enumerate() {
        if i > target_idx {
            break;
        }
        vt.feed_str(&ev.data);
    }
    vt
}

fn calc_delay(current: f64, previous: f64, speed: f64, idle_limit: Option<f64>) -> Duration {
    let raw = (current - previous) / speed;
    let secs = match idle_limit {
        Some(limit) => raw.min(limit),
        None => raw,
    };
    Duration::from_secs_f64(secs)
}

fn run_player(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut vt: Vt,
    events: Vec<CastEvent>,
    total_duration: f64,
    recording_start: i64,
    opts: &ReplayOptions,
) -> anyhow::Result<()> {
    let total_events = events.len();
    let w = vt.size().0;
    let h = vt.size().1;

    let mut state = PlayerState {
        width: w,
        height: h,
        events,
        current_idx: 0,
        speed: opts.speed,
        idle_limit: opts.idle_limit,
        paused: false,
        quit: false,
        start: Instant::now(),
        last_event_time: 0.0,
        next_event_at: Duration::ZERO,
        recording_start,
    };

    if !state.events.is_empty() {
        state.next_event_at = calc_delay(state.events[0].time, 0.0, state.speed, state.idle_limit);
    }

    let tick = Duration::from_millis(16);

    while !state.quit {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                handle_key(&mut state, &mut vt, key);
            }
        }

        if !state.paused && state.current_idx < state.events.len() {
            let elapsed = state.start.elapsed();
            while state.current_idx < state.events.len() && elapsed >= state.next_event_at {
                let ev = &state.events[state.current_idx];
                vt.feed_str(&ev.data);
                state.last_event_time = ev.time;
                state.current_idx += 1;
                if state.current_idx < state.events.len() {
                    let next = &state.events[state.current_idx];
                    state.next_event_at = elapsed
                        + calc_delay(
                            next.time,
                            state.last_event_time,
                            state.speed,
                            state.idle_limit,
                        );
                }
            }
        }

        terminal.draw(|f| {
            let area = f.area();
            render_frame(f, area, &vt, &state, total_duration, total_events);
        })?;

        if state.current_idx >= state.events.len() && !state.paused {
            if event::poll(Duration::from_secs(2))? {
                if let Event::Key(_) = event::read()? {
                    break;
                }
            } else {
                break;
            }
        }

        if event::poll(tick)? {}
    }

    Ok(())
}

fn handle_key(state: &mut PlayerState, vt: &mut Vt, key: crossterm::event::KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => state.quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => state.quit = true,
        KeyCode::Char(' ') => {
            state.paused = !state.paused;
            if !state.paused {
                state.start = Instant::now();
                let idx = state.current_idx.min(state.events.len().saturating_sub(1));
                state.last_event_time = state.events[idx].time;
                if state.current_idx < state.events.len() {
                    let next = &state.events[state.current_idx];
                    state.next_event_at = calc_delay(
                        next.time,
                        state.last_event_time,
                        state.speed,
                        state.idle_limit,
                    );
                }
            }
        }
        KeyCode::Right | KeyCode::Char('l') => seek(state, vt, 30.0),
        KeyCode::Left | KeyCode::Char('h') => seek(state, vt, -30.0),
        KeyCode::Up => {
            state.speed = (state.speed + 0.5).min(10.0);
            reset_timing(state);
        }
        KeyCode::Down => {
            state.speed = (state.speed - 0.5).max(0.25);
            reset_timing(state);
        }
        KeyCode::Char('0') => seek_abs(state, vt, 0.0),
        KeyCode::Char('$') | KeyCode::Char('G') => {
            let idx = state.events.len().saturating_sub(1);
            let t = state.events.last().map(|e| e.time).unwrap_or(0.0);
            state.current_idx = idx;
            state.last_event_time = t;
            *vt = rebuild_vt(&state.events, idx, state.width, state.height);
        }
        _ => {}
    }
}

fn seek(state: &mut PlayerState, vt: &mut Vt, delta: f64) {
    let current = if state.current_idx < state.events.len() {
        state.events[state.current_idx].time
    } else {
        state.events.last().map(|e| e.time).unwrap_or(0.0)
    };
    seek_abs(state, vt, (current + delta).max(0.0));
}

fn seek_abs(state: &mut PlayerState, vt: &mut Vt, target_time: f64) {
    let mut idx = 0;
    for (i, ev) in state.events.iter().enumerate() {
        if ev.time > target_time {
            break;
        }
        idx = i;
    }
    *vt = rebuild_vt(&state.events, idx, state.width, state.height);
    state.current_idx = idx + 1;
    state.last_event_time = state.events[idx].time;
    reset_timing(state);
}

fn reset_timing(state: &mut PlayerState) {
    state.start = Instant::now();
    if state.current_idx < state.events.len() {
        let next = &state.events[state.current_idx];
        state.next_event_at = calc_delay(
            next.time,
            state.last_event_time,
            state.speed,
            state.idle_limit,
        );
    } else {
        state.next_event_at = Duration::ZERO;
    }
}

fn render_frame(
    f: &mut ratatui::Frame,
    area: Rect,
    vt: &Vt,
    state: &PlayerState,
    total_duration: f64,
    total_events: usize,
) {
    let main_h = area.height.saturating_sub(2);
    let main_area = Rect {
        height: main_h,
        ..area
    };
    let status_area = Rect {
        y: main_area.bottom(),
        height: 2.min(area.height),
        ..area
    };

    let content = build_content(vt, main_area.width as usize, main_area.height as usize);
    f.render_widget(
        Paragraph::new(content).block(Block::default().borders(Borders::NONE)),
        main_area,
    );

    let current_time = state.last_event_time;

    let indicator = if state.paused { "⏸" } else { "▶" };
    let progress = format!("{} / {}", fmt_dur(current_time), fmt_dur(total_duration));
    let real_time = if state.recording_start > 0 {
        chrono::Utc
            .timestamp_opt(state.recording_start + current_time as i64, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let status_text = format!(
        " {} {:<12}  {:<22}  {} events  speed {:.1}x  ←→ seek  ↑↓ speed  space pause  q quit ",
        indicator, progress, real_time, total_events, state.speed,
    );

    let status_p = Paragraph::new(TuiLine::from(vec![Span::styled(
        status_text,
        Style::default().fg(Color::Gray).bg(Color::DarkGray),
    )]))
    .block(Block::default().style(Style::default().bg(Color::DarkGray)));

    f.render_widget(status_p, status_area);
}

fn fmt_dur(secs: f64) -> String {
    let h = (secs / 3600.0) as u64;
    let m = ((secs % 3600.0) / 60.0) as u64;
    let s = (secs % 60.0) as u64;
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

fn build_content(vt: &Vt, max_cols: usize, max_rows: usize) -> ratatui::text::Text<'static> {
    let mut lines: Vec<TuiLine> = Vec::with_capacity(max_rows);

    for (row_idx, line) in vt.view().enumerate() {
        if row_idx >= max_rows {
            break;
        }
        let cells = line.cells();
        if cells.is_empty() {
            lines.push(TuiLine::from(""));
        } else {
            let mut spans: Vec<Span> = Vec::new();
            let mut i: usize = 0;
            while i < cells.len() && i < max_cols {
                let style = convert_style(&cells[i]);
                let mut j = i + 1;
                while j < cells.len() && j < max_cols {
                    if convert_style(&cells[j]) != style {
                        break;
                    }
                    j += 1;
                }
                let text: String = cells[i..j].iter().map(|c| c.char().to_string()).collect();
                spans.push(Span::styled(text, style));
                i = j;
            }
            lines.push(TuiLine::from(spans));
        }
    }

    ratatui::text::Text::from(lines)
}

fn convert_style(cell: &avt::Cell) -> Style {
    let pen = cell.pen();
    let fg = pen.foreground().map(convert_color).unwrap_or(Color::White);
    let bg = pen.background().map(convert_color).unwrap_or(Color::Black);
    let mut style = Style::default().fg(fg).bg(bg);
    if pen.is_bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if pen.is_italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if pen.is_underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if pen.is_inverse() {
        style = Style::default().fg(bg).bg(fg);
    }
    style
}

fn convert_color(c: avt::Color) -> Color {
    match c {
        avt::Color::Indexed(i) => match i {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::White,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            15 => Color::Gray,
            _ => Color::Indexed(i),
        },
        avt::Color::RGB(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn drain_stdin() {
    #[cfg(unix)]
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
        use std::io::Read;
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        let mut buf = [0u8; 128];
        loop {
            match std::io::stdin().lock().read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    }
}
