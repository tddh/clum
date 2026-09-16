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

/// A single event from an asciinema v2 recording.
///
/// Input (`"i"`) events are intentionally not represented — they are never
/// rendered. Resize events DO participate in playback (and therefore in seeks),
/// so they are part of the replayed sequence instead of being applied at load
/// time.
enum CastEvent {
    /// `"o"` — terminal output as UTF-8 text.
    Output { time: f64, data: String },
    /// `"ob"` — lossless terminal output as base64-encoded raw bytes.
    OutputBytes { time: f64, data: Vec<u8> },
    /// `"r"` — terminal geometry change (`vt.resize`).
    Resize { time: f64, cols: usize, rows: usize },
}

impl CastEvent {
    fn time(&self) -> f64 {
        match self {
            CastEvent::Output { time, .. }
            | CastEvent::OutputBytes { time, .. }
            | CastEvent::Resize { time, .. } => *time,
        }
    }
}

/// Feeds raw output bytes to the `Vt`, buffering multi-byte UTF-8 sequences that
/// are split across recording events.
///
/// `avt` only exposes `feed_str` / `feed(char)` (there is no byte API), so a
/// trailing incomplete sequence is retained until a later event completes it.
/// Bytes that can never form a valid character are dropped.
#[derive(Default)]
struct ByteDecoder {
    pending: Vec<u8>,
}

impl ByteDecoder {
    /// Append `bytes`, feeding every complete character to `vt` in order.
    fn feed(&mut self, vt: &mut Vt, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        loop {
            let pending = std::mem::take(&mut self.pending);
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    if !text.is_empty() {
                        vt.feed_str(text);
                    }
                    break;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid > 0 {
                        // `valid_up_to` is always a valid UTF-8 boundary.
                        let text = std::str::from_utf8(&pending[..valid]).unwrap_or("");
                        vt.feed_str(text);
                    }
                    match err.error_len() {
                        // Definitively invalid bytes: unrenderable, drop them.
                        Some(len) => self.pending = pending[valid + len..].to_vec(),
                        // Incomplete trailing sequence: keep it for the next event.
                        None => {
                            self.pending = pending[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Drop any trailing bytes that can never complete a character.
    fn discard_pending(&mut self) {
        if !self.pending.is_empty() {
            tracing::debug!(
                bytes = self.pending.len(),
                "discarding incomplete trailing output bytes at end of stream"
            );
            self.pending.clear();
        }
    }
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut padded = false;

    for &byte in input.as_bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padded = true;
            continue;
        }
        if padded {
            return None;
        }
        let value = sextet(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }

    if bits >= 6 {
        return None;
    }
    Some(out)
}

/// Parse an asciinema `"r"` payload.
///
/// The v2 format uses the string `"COLSxROWS"`; some hand-made files use the
/// legacy `[cols, rows]` array. Both forms are accepted.
fn parse_resize(payload: &serde_json::Value) -> Option<(usize, usize)> {
    let (cols, rows) = if let Some(text) = payload.as_str() {
        let text = text.trim();
        let (cols, rows) = text.split_once('x').or_else(|| text.split_once('X'))?;
        let cols: usize = cols.trim().parse().ok()?;
        let rows: usize = rows.trim().parse().ok()?;
        (cols, rows)
    } else {
        let arr = payload.as_array()?;
        if arr.len() < 2 {
            return None;
        }
        (arr[0].as_u64()? as usize, arr[1].as_u64()? as usize)
    };

    if cols == 0 || rows == 0 {
        return None;
    }
    Some((cols, rows))
}

struct PlayerState {
    events: Vec<CastEvent>,
    header_cols: usize,
    header_rows: usize,
    current_idx: usize,
    speed: f64,
    idle_limit: Option<f64>,
    paused: bool,
    quit: bool,
    start: Instant,
    last_event_time: f64,
    next_event_at: Duration,
    recording_start: i64,
    decoder: ByteDecoder,
}

#[derive(Clone)]
pub struct ReplayOptions {
    pub speed: f64,
    pub idle_limit: Option<f64>,
}

pub fn replay(path: &Path, opts: &ReplayOptions) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    replay_file(file, opts)
}

pub fn replay_file(file: std::fs::File, opts: &ReplayOptions) -> anyhow::Result<()> {
    let (events, vt, recording_start) = load_and_prepare(file)?;
    if events.is_empty() {
        eprintln!("no output events in recording");
        return Ok(());
    }

    let total_duration = events.last().map(|e| e.time()).unwrap_or(0.0);

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

fn load_and_prepare(file: std::fs::File) -> anyhow::Result<(Vec<CastEvent>, Vt, i64)> {
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
            // The `"exit"` line marks the end of the recording. It carries a
            // timestamp the session then idled for, so it must terminate the
            // parse rather than become an event: otherwise the final idle gap
            // would inflate the total duration and hold playback open.
            "exit" => break,
            "o" => events.push(CastEvent::Output {
                time,
                data: data.to_string(),
            }),
            "ob" => match decode_base64(data) {
                Some(bytes) => events.push(CastEvent::OutputBytes { time, data: bytes }),
                None => tracing::debug!("skipping malformed base64 \"ob\" payload"),
            },
            "r" => {
                if let Some((cols, rows)) = parse_resize(&arr[2]) {
                    events.push(CastEvent::Resize { time, cols, rows });
                }
            }
            _ => {}
        }
    }

    // The returned terminal is pristine — header geometry only, no events
    // applied. Feeding events here would make every rendered frame a composite
    // of the file's end state and the current replay position.
    Ok((events, Vt::new(width, height), timestamp))
}

fn rebuild_vt(
    events: &[CastEvent],
    target_idx: usize,
    header_cols: usize,
    header_rows: usize,
) -> (Vt, ByteDecoder) {
    let mut vt = Vt::new(header_cols, header_rows);
    let mut decoder = ByteDecoder::default();
    replay_into(&mut vt, &mut decoder, events, target_idx);
    (vt, decoder)
}

fn replay_into(vt: &mut Vt, decoder: &mut ByteDecoder, events: &[CastEvent], target_idx: usize) {
    for (i, ev) in events.iter().enumerate() {
        if i > target_idx {
            break;
        }
        apply_event(vt, decoder, ev);
    }
}

fn apply_event(vt: &mut Vt, decoder: &mut ByteDecoder, ev: &CastEvent) {
    match ev {
        CastEvent::Output { data, .. } => decoder.feed(vt, data.as_bytes()),
        CastEvent::OutputBytes { data, .. } => decoder.feed(vt, data),
        CastEvent::Resize { cols, rows, .. } => {
            vt.resize(*cols, *rows);
        }
    }
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
    let (w, h) = vt.size();

    let mut state = PlayerState {
        header_cols: w,
        header_rows: h,
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
        decoder: ByteDecoder::default(),
    };

    if !state.events.is_empty() {
        state.next_event_at =
            calc_delay(state.events[0].time(), 0.0, state.speed, state.idle_limit);
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
                let idx = state.current_idx;
                apply_event(&mut vt, &mut state.decoder, &state.events[idx]);
                state.last_event_time = state.events[idx].time();
                state.current_idx = idx + 1;
                if state.current_idx < state.events.len() {
                    let next_time = state.events[state.current_idx].time();
                    state.next_event_at = elapsed
                        + calc_delay(
                            next_time,
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
            state.decoder.discard_pending();
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
                state.last_event_time = state.events[idx].time();
                if state.current_idx < state.events.len() {
                    let next = &state.events[state.current_idx];
                    state.next_event_at = calc_delay(
                        next.time(),
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
            if state.events.is_empty() {
                return;
            }
            let idx = state.events.len() - 1;
            let (rebuilt, decoder) =
                rebuild_vt(&state.events, idx, state.header_cols, state.header_rows);
            *vt = rebuilt;
            state.decoder = decoder;
            state.current_idx = idx + 1;
            state.last_event_time = state.events[idx].time();
            reset_timing(state);
        }
        _ => {}
    }
}

fn seek(state: &mut PlayerState, vt: &mut Vt, delta: f64) {
    let current = if state.current_idx < state.events.len() {
        state.events[state.current_idx].time()
    } else {
        state.events.last().map(|e| e.time()).unwrap_or(0.0)
    };
    seek_abs(state, vt, (current + delta).max(0.0));
}

fn seek_abs(state: &mut PlayerState, vt: &mut Vt, target_time: f64) {
    let mut target_idx: Option<usize> = None;
    for (i, ev) in state.events.iter().enumerate() {
        if ev.time() > target_time {
            break;
        }
        target_idx = Some(i);
    }

    match target_idx {
        Some(idx) => {
            let (rebuilt, decoder) =
                rebuild_vt(&state.events, idx, state.header_cols, state.header_rows);
            *vt = rebuilt;
            // The decoder must match the rebuilt Vt exactly — a stale pending
            // tail would corrupt the next event after the seek.
            state.decoder = decoder;
            state.current_idx = idx + 1;
            state.last_event_time = state.events[idx].time();
        }
        None => {
            *vt = Vt::new(state.header_cols, state.header_rows);
            state.decoder = ByteDecoder::default();
            state.current_idx = 0;
            state.last_event_time = 0.0;
        }
    }
    reset_timing(state);
}

fn reset_timing(state: &mut PlayerState) {
    state.start = Instant::now();
    if state.current_idx < state.events.len() {
        let next = &state.events[state.current_idx];
        state.next_event_at = calc_delay(
            next.time(),
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
            continue;
        }

        let mut spans: Vec<Span> = Vec::new();
        // Count display columns, not cells: a wide (CJK) char is a width-2 head
        // cell plus a width-0 tail cell. Counting cells adds a spurious space and
        // truncates too early.
        let mut columns: usize = 0;
        let mut i: usize = 0;
        while i < cells.len() {
            // Width-0 cells are the continuation half of the preceding wide char
            // and carry no glyph of their own, so they are never emitted.
            if cells[i].width() == 0 {
                i += 1;
                continue;
            }

            let style = convert_style(&cells[i]);
            let mut text = String::new();
            let mut j = i;
            while j < cells.len() {
                let cell = &cells[j];
                let cell_width = usize::from(cell.width());
                if cell_width == 0 {
                    j += 1;
                    continue;
                }
                // Check the column budget before appending so a wide char that
                // would straddle the boundary is dropped whole, not half.
                if columns + cell_width > max_cols {
                    break;
                }
                if convert_style(cell) != style {
                    break;
                }
                text.push(cell.char());
                columns += cell_width;
                j += 1;
            }

            if text.is_empty() {
                break;
            }
            spans.push(Span::styled(text, style));
            i = j;
        }
        lines.push(TuiLine::from(spans));
    }

    ratatui::text::Text::from(lines)
}

fn convert_style(cell: &avt::Cell) -> Style {
    let pen = cell.pen();
    // `None` means "terminal default": leave the field unset so the terminal's
    // own palette shows through. Forcing a fallback here (e.g. white-on-black)
    // makes every `ESC[0m` region — extremely common in real recordings — clash
    // with the recording's actual 256-color theme.
    let mut style = apply_colors(
        Style::default(),
        pen.foreground(),
        pen.background(),
        pen.is_inverse(),
    );
    if pen.is_bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if pen.is_italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if pen.is_underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// Applies the pen's foreground/background to `style`, honouring inverse video.
///
/// Inverse swaps the *original* options before any fallback is applied, so an
/// explicit pair renders exactly as the terminal would have drawn it. When both
/// sides are absent there is nothing to swap, so `Modifier::REVERSED` is set
/// instead and the terminal performs the swap using its own palette.
fn apply_colors(
    mut style: Style,
    fg: Option<avt::Color>,
    bg: Option<avt::Color>,
    inverse: bool,
) -> Style {
    let (fg, bg) = if inverse { (bg, fg) } else { (fg, bg) };
    if let Some(c) = fg {
        style = style.fg(convert_color(c));
    }
    if let Some(c) = bg {
        style = style.bg(convert_color(c));
    }
    if inverse && fg.is_none() && bg.is_none() {
        style = style.add_modifier(Modifier::REVERSED);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    fn cast_file(contents: &str) -> std::fs::File {
        let mut file = tempfile::tempfile().expect("create temp cast");
        file.write_all(contents.as_bytes()).expect("write cast");
        file.seek(std::io::SeekFrom::Start(0)).expect("rewind cast");
        file
    }

    fn vt_row(vt: &Vt, row: usize) -> String {
        vt.view()
            .nth(row)
            .map(|line| line.cells().iter().map(|c| c.char()).collect())
            .unwrap_or_default()
    }

    fn render_all(vt: &Vt) -> Vec<String> {
        vt.view()
            .map(|line| line.cells().iter().map(|c| c.char()).collect())
            .collect()
    }

    fn rendered_line(text: &ratatui::text::Text<'_>, row: usize) -> String {
        text.lines
            .get(row)
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .unwrap_or_default()
    }

    /// Display width for assertions only: CJK ideographs count as 2 columns,
    /// everything else (ASCII, box-drawing) as 1.
    fn display_width(s: &str) -> usize {
        s.chars().map(|c| if is_cjk(c) { 2 } else { 1 }).sum()
    }

    fn is_cjk(c: char) -> bool {
        matches!(
            c as u32,
            0x1100..=0x115f
                | 0x2e80..=0xa4cf
                | 0xac00..=0xd7a3
                | 0xf900..=0xfaff
                | 0xfe30..=0xfe4f
                | 0xff00..=0xff60
                | 0xffe0..=0xffe6
        )
    }

    fn test_state(events: Vec<CastEvent>) -> PlayerState {
        PlayerState {
            events,
            header_cols: 80,
            header_rows: 24,
            current_idx: 0,
            speed: 1.0,
            idle_limit: None,
            paused: false,
            quit: false,
            start: Instant::now(),
            last_event_time: 0.0,
            next_event_at: Duration::ZERO,
            recording_start: 0,
            decoder: ByteDecoder::default(),
        }
    }

    #[test]
    fn load_parses_header_and_returns_unfed_vt() {
        let contents = concat!(
            "{\"version\":2,\"width\":108,\"height\":31,\"timestamp\":1700000000}\n",
            "[0.01,\"o\",\"ROW1-AAAA\"]\n",
            "[100.0,\"o\",\"\\u001b[3;1HROW3-CCCC\"]\n",
        );
        let (events, vt, timestamp) = load_and_prepare(cast_file(contents)).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(vt.size(), (108, 31));
        assert_eq!(timestamp, 1_700_000_000);
        assert_eq!(
            vt_row(&vt, 0).trim_end(),
            "",
            "load_and_prepare must not pre-feed the Vt"
        );

        let (first, _) = rebuild_vt(&events, 0, 108, 31);
        assert!(vt_row(&first, 0).contains("ROW1-AAAA"));
        assert!(
            !vt_row(&first, 2).contains("ROW3-CCCC"),
            "the first frame must not contain the file's end state"
        );
    }

    #[test]
    fn parse_resize_accepts_spec_string_and_legacy_array() {
        assert_eq!(parse_resize(&serde_json::json!("108x31")), Some((108, 31)));
        assert_eq!(parse_resize(&serde_json::json!([100, 40])), Some((100, 40)));
        assert_eq!(parse_resize(&serde_json::json!("0x0")), None);
        assert_eq!(parse_resize(&serde_json::json!("garbage")), None);
        assert_eq!(parse_resize(&serde_json::json!([])), None);
    }

    #[test]
    fn resize_events_are_recorded_and_replayed_in_order() {
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.0,\"o\",\"A\"]\n",
            "[1.0,\"r\",\"108x31\"]\n",
            "[2.0,\"o\",\"B\"]\n",
            "[3.0,\"r\",[60,20]]\n",
        );
        let (events, vt, _) = load_and_prepare(cast_file(contents)).unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[1],
            CastEvent::Resize {
                cols: 108,
                rows: 31,
                ..
            }
        ));
        assert!(matches!(
            events[3],
            CastEvent::Resize {
                cols: 60,
                rows: 20,
                ..
            }
        ));

        let (after_first, _) = rebuild_vt(&events, 1, vt.size().0, vt.size().1);
        assert_eq!(after_first.size(), (108, 31));
        let (after_second, _) = rebuild_vt(&events, 3, vt.size().0, vt.size().1);
        assert_eq!(after_second.size(), (60, 20));
    }

    #[test]
    fn rebuild_starts_from_header_geometry_not_current_size() {
        let events = vec![
            CastEvent::Resize {
                time: 0.0,
                cols: 10,
                rows: 5,
            },
            CastEvent::Resize {
                time: 1.0,
                cols: 60,
                rows: 20,
            },
        ];
        let (vt, _) = rebuild_vt(&events, 1, 200, 50);
        assert_eq!(vt.size(), (60, 20));
        let (vt_first, _) = rebuild_vt(&events, 0, 200, 50);
        assert_eq!(vt_first.size(), (10, 5));
    }

    #[test]
    fn ob_and_o_events_feed_exactly_one_character_in_order() {
        // "ww==" is base64 for 0xE4, a 3-byte character's lead byte; the
        // following "o" supplies a complete character. Exactly one character
        // is fed, and no garbage is emitted for the incomplete prefix.
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.0,\"ob\",\"ww==\"]\n",
            "[1.0,\"o\",\"你\"]\n",
        );
        let (events, vt, _) = load_and_prepare(cast_file(contents)).unwrap();
        assert_eq!(events.len(), 2);
        let (rebuilt, _) = rebuild_vt(&events, 1, vt.size().0, vt.size().1);
        assert_eq!(vt_row(&rebuilt, 0).trim_end(), "你");
    }

    #[test]
    fn ob_split_across_two_events_completes_one_character() {
        // "ww==" = 0xC3, "qQ==" = 0xA9 — together the UTF-8 encoding of "é",
        // which must be fed exactly once (not two replacement characters).
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.0,\"ob\",\"ww==\"]\n",
            "[1.0,\"ob\",\"qQ==\"]\n",
        );
        let (events, vt, _) = load_and_prepare(cast_file(contents)).unwrap();
        let (rebuilt, _) = rebuild_vt(&events, 1, vt.size().0, vt.size().1);
        assert_eq!(vt_row(&rebuilt, 0).trim_end(), "é");
    }

    #[test]
    fn truncated_ob_tail_is_discarded_without_panicking() {
        // "ww==" never completes: a lone lead byte can never form a character.
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.0,\"ob\",\"ww==\"]\n",
        );
        let (events, vt, _) = load_and_prepare(cast_file(contents)).unwrap();
        assert_eq!(events.len(), 1);
        let (rebuilt, _) = rebuild_vt(&events, 0, vt.size().0, vt.size().1);
        assert_eq!(vt_row(&rebuilt, 0).trim_end(), "");
    }

    #[test]
    fn malformed_ob_payload_is_skipped() {
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.0,\"ob\",\"not base64!!\"]\n",
            "[1.0,\"o\",\"ok\"]\n",
        );
        let (events, vt, _) = load_and_prepare(cast_file(contents)).unwrap();
        assert_eq!(events.len(), 1);
        let (rebuilt, _) = rebuild_vt(&events, 0, vt.size().0, vt.size().1);
        assert_eq!(vt_row(&rebuilt, 0).trim_end(), "ok");
    }

    #[test]
    fn late_exit_line_does_not_extend_playback_duration() {
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.5,\"o\",\"A\"]\n",
            "[1.5,\"o\",\"B\"]\n",
            "[3600.0,\"exit\",\"\"]\n",
            "[3601.0,\"o\",\"AFTER-EXIT\"]\n",
        );
        let (events, _, _) = load_and_prepare(cast_file(contents)).unwrap();

        assert_eq!(events.len(), 2, "the exit line must not become an event");
        assert_eq!(
            events.iter().map(CastEvent::time).collect::<Vec<_>>(),
            vec![0.5, 1.5],
            "no exit entry may appear in the parsed events"
        );

        let total_duration = events.last().map(|e| e.time()).unwrap_or(0.0);
        assert_eq!(
            total_duration, 1.5,
            "playback must end at the last real event, not the idle exit gap"
        );

        let (rebuilt, _) = rebuild_vt(&events, events.len() - 1, 80, 24);
        let rendered = vt_row(&rebuilt, 0);
        assert_eq!(rendered.trim_end(), "AB");
        assert!(
            !rendered.contains("AFTER-EXIT"),
            "lines after the exit terminator must not be parsed"
        );
    }

    #[test]
    fn incomplete_ob_tail_before_exit_is_discarded_at_end_of_stream() {
        let contents = concat!(
            "{\"version\":2,\"width\":80,\"height\":24}\n",
            "[0.0,\"o\",\"ok\"]\n",
            "[1.0,\"ob\",\"ww==\"]\n",
            "[2.0,\"exit\",\"\"]\n",
        );
        let (events, header_vt, _) = load_and_prepare(cast_file(contents)).unwrap();
        assert_eq!(events.len(), 2, "the exit line must not become an event");

        let mut vt = Vt::new(header_vt.size().0, header_vt.size().1);
        let mut decoder = ByteDecoder::default();
        for ev in &events {
            apply_event(&mut vt, &mut decoder, ev);
        }

        assert_eq!(decoder.pending.len(), 1, "the lone lead byte stays pending");
        decoder.discard_pending();
        assert!(
            decoder.pending.is_empty(),
            "end-of-stream discard must clear it"
        );
        assert_eq!(vt_row(&vt, 0).trim_end(), "ok");
    }

    #[test]
    fn rebuild_matches_sequential_playback_at_every_index() {
        let events = vec![
            CastEvent::Output {
                time: 0.0,
                data: "AAA".into(),
            },
            CastEvent::Resize {
                time: 1.0,
                cols: 100,
                rows: 30,
            },
            CastEvent::Output {
                time: 2.0,
                data: "BBB".into(),
            },
            CastEvent::Output {
                time: 3.0,
                data: "CCC".into(),
            },
        ];

        let mut sequential = Vt::new(80, 24);
        let mut decoder = ByteDecoder::default();
        for (k, ev) in events.iter().enumerate() {
            apply_event(&mut sequential, &mut decoder, ev);
            let (rebuilt, _) = rebuild_vt(&events, k, 80, 24);
            assert_eq!(
                rebuilt.size(),
                sequential.size(),
                "size mismatch at index {k}"
            );
            assert_eq!(
                render_all(&rebuilt),
                render_all(&sequential),
                "render mismatch at index {k}"
            );
        }
    }

    #[test]
    fn seek_before_first_event_yields_fresh_state() {
        let mut state = test_state(vec![CastEvent::Output {
            time: 10.0,
            data: "LATE".into(),
        }]);
        let mut vt = Vt::new(80, 24);
        vt.feed_str("STALE");

        seek_abs(&mut state, &mut vt, 5.0);

        assert_eq!(state.current_idx, 0);
        assert_eq!(state.last_event_time, 0.0);
        assert_eq!(vt_row(&vt, 0).trim_end(), "");
        assert_eq!(vt.size(), (80, 24));
    }

    #[test]
    fn seek_to_end_does_not_double_apply_last_event() {
        let events = vec![
            CastEvent::Output {
                time: 0.0,
                data: "A".into(),
            },
            CastEvent::Output {
                time: 1.0,
                data: "B".into(),
            },
            CastEvent::Output {
                time: 2.0,
                data: "C".into(),
            },
        ];
        let mut state = test_state(events);
        let mut vt = Vt::new(80, 24);

        handle_key(
            &mut state,
            &mut vt,
            crossterm::event::KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
        );

        assert_eq!(state.current_idx, 3, "nothing may remain to replay");
        let (expected, _) = rebuild_vt(&state.events, 2, 80, 24);
        assert_eq!(render_all(&vt), render_all(&expected));
        assert_eq!(vt_row(&vt, 0).trim_end(), "ABC");
    }

    #[test]
    fn cjk_wide_chars_render_without_spurious_spaces() {
        let mut vt = Vt::new(8, 2);
        vt.feed_str("中文测试");

        let content = build_content(&vt, 8, 2);
        let rendered = rendered_line(&content, 0);

        assert_eq!(rendered, "中文测试");
        assert!(
            !rendered.contains(' '),
            "no wide-tail continuation cell may leak a space"
        );
    }

    #[test]
    fn mixed_ascii_and_cjk_uses_display_columns_not_cell_count() {
        let mut vt = Vt::new(6, 2);
        vt.feed_str("ab中文");

        let content = build_content(&vt, 6, 2);
        let rendered = rendered_line(&content, 0);

        assert_eq!(rendered, "ab中文");
        assert_eq!(rendered.chars().count(), 4, "4 glyphs emitted from 6 cells");
        assert_eq!(
            display_width(&rendered),
            6,
            "the rendered spans span 6 display columns"
        );
    }

    #[test]
    fn cjk_line_truncates_on_display_column_boundary() {
        let mut vt = Vt::new(12, 2);
        vt.feed_str("中文测试中文");

        let five = build_content(&vt, 5, 2);
        assert_eq!(
            rendered_line(&five, 0),
            "中文",
            "a third 2-column char cannot fit within 5 columns"
        );

        let six = build_content(&vt, 6, 2);
        assert_eq!(rendered_line(&six, 0), "中文测");
    }

    #[test]
    fn single_width_box_drawing_chars_render_unchanged() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("─┼┤├");

        let content = build_content(&vt, 4, 2);
        let rendered = rendered_line(&content, 0);

        assert_eq!(rendered, "─┼┤├");
        assert_eq!(display_width(&rendered), 4);
    }

    fn cell_style(vt: &Vt, row: usize, col: usize) -> Style {
        let cell = vt
            .view()
            .nth(row)
            .expect("row exists")
            .cells()
            .get(col)
            .expect("col exists");
        convert_style(cell)
    }

    #[test]
    fn reset_leaves_colors_unset_so_terminal_default_applies() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("\x1b[0mA");

        let style = cell_style(&vt, 0, 0);

        assert_eq!(
            style.fg, None,
            "reset cell must inherit the terminal default fg"
        );
        assert_eq!(
            style.bg, None,
            "reset cell must inherit the terminal default bg"
        );
    }

    #[test]
    fn indexed_256_color_pair_is_passed_through() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("\x1b[38;5;231m\x1b[48;5;232mX");

        let style = cell_style(&vt, 0, 0);

        assert_eq!(style.fg, Some(Color::Indexed(231)));
        assert_eq!(style.bg, Some(Color::Indexed(232)));
    }

    #[test]
    fn basic_color_maps_to_named_red_family() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("\x1b[31mR");

        let style = cell_style(&vt, 0, 0);

        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, None);
    }

    #[test]
    fn inverse_swaps_the_colors_actually_present() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("\x1b[31m\x1b[44m\x1b[7mI");

        let style = cell_style(&vt, 0, 0);

        assert_eq!(style.fg, Some(Color::Blue), "fg becomes the original bg");
        assert_eq!(style.bg, Some(Color::Red), "bg becomes the original fg");
    }

    #[test]
    fn inverse_without_colors_uses_reversed_modifier() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("\x1b[7mI");

        let style = cell_style(&vt, 0, 0);

        assert_eq!(style.fg, None);
        assert_eq!(style.bg, None);
        assert!(
            style.add_modifier.contains(Modifier::REVERSED),
            "no colors to swap, so the terminal must do the reversal itself"
        );
    }
}
