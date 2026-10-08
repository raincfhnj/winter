//! Reader side of the `winter ui` live pane dashboard.
//!
//! The controller atomically writes [`DashboardState`] to the shared state
//! file; this module polls that file and renders it. Interactive sessions
//! redraw an ANSI full-screen frame at roughly 4 Hz inside a Windows
//! Terminal pane, while `--once` (and any piped stdin) prints a single
//! plain-text frame so agents and scripts get the same information without
//! inheriting terminal modes.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleOutputCP,
    GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, GetStdHandle, INPUT_RECORD,
    KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    SetConsoleOutputCP,
};

use crate::dashboard::{
    DASHBOARD_STALE_AFTER_MS, DashboardPane, DashboardState, read_dashboard, unix_ms_now,
};
use crate::platform::windows::PlatformError;
use crate::{AppError, AppResult};

/// Cadence of the interactive redraw loop (4 Hz).
const FRAME_INTERVAL: Duration = Duration::from_millis(250);
/// Input poll granularity inside one frame interval.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Frame size used when the console size cannot be queried.
const DEFAULT_COLS: usize = 100;
/// Frame size used when the console size cannot be queried.
const DEFAULT_ROWS: usize = 30;
/// UTF-8 code page enabled while box characters are drawn.
const UTF8_CODE_PAGE: u32 = 65001;
/// Smallest width that still gets a pane map instead of a placeholder.
const MIN_MAP_COLS: usize = 10;
/// Smallest map height that still fits a three-line box.
const MIN_MAP_HEIGHT: usize = 3;
/// Records read per `ReadConsoleInputW` call.
const INPUT_BATCH: usize = 32;
/// Upper bound of input batches drained per poll.
const INPUT_BATCH_LIMIT: u64 = 64;
/// Frame width at which the two-zone sidebar layout replaces the legacy
/// single-column layout.
const TWO_ZONE_MIN_COLS: usize = 60;
/// Narrowest allowed left sidebar.
const SIDEBAR_MIN_WIDTH: usize = 24;
/// Widest allowed left sidebar.
const SIDEBAR_MAX_WIDTH: usize = 34;
/// Full-width banner shown at the top of an offline frame.
const OFFLINE_BANNER: &str = "!! CONTROLLER OFFLINE \u{2014} start it with 'winter run' !!";

const EDGE_UP: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_DOWN: u8 = 4;
const EDGE_LEFT: u8 = 8;
const FOCUS_MARKER: u8 = 16;

const EDGE_VERTICAL: u8 = EDGE_UP | EDGE_DOWN;
const EDGE_HORIZONTAL: u8 = EDGE_LEFT | EDGE_RIGHT;
const CORNER_BOTTOM_LEFT: u8 = EDGE_RIGHT | EDGE_UP;
const CORNER_TOP_LEFT: u8 = EDGE_RIGHT | EDGE_DOWN;
const CORNER_BOTTOM_RIGHT: u8 = EDGE_LEFT | EDGE_UP;
const CORNER_TOP_RIGHT: u8 = EDGE_LEFT | EDGE_DOWN;
const JOIN_T_RIGHT: u8 = EDGE_UP | EDGE_DOWN | EDGE_RIGHT;
const JOIN_T_LEFT: u8 = EDGE_UP | EDGE_DOWN | EDGE_LEFT;
const JOIN_T_UP: u8 = EDGE_LEFT | EDGE_RIGHT | EDGE_UP;
const JOIN_T_DOWN: u8 = EDGE_LEFT | EDGE_RIGHT | EDGE_DOWN;
const JOIN_CROSS: u8 = EDGE_UP | EDGE_DOWN | EDGE_LEFT | EDGE_RIGHT;

/// Options controlling one `winter ui` session.
#[derive(Debug, Clone)]
pub struct UiOptions {
    /// Dashboard state file to poll.
    pub path: PathBuf,
    /// Render exactly one plain-text frame and exit.
    pub once: bool,
}

/// Runs the dashboard until the user quits, or prints one plain frame when
/// `once` is set or stdin is not a console.
///
/// Exit status is always 0 for a completed render or quit: a missing, stale,
/// or stopped controller only produces an offline frame. Hard errors (such
/// as an unresolvable default state path) propagate to the caller as
/// [`AppError`].
pub fn run(options: UiOptions) -> AppResult<ExitCode> {
    if options.once || !std::io::stdin().is_terminal() {
        return Ok(render_plain(&options.path));
    }
    interactive(&options.path)
}

/// Renders one frame to stdout without ANSI sequences, then exits 0.
fn render_plain(path: &Path) -> ExitCode {
    let state = read_dashboard(path);
    let age_ms = current_age(state.as_ref());
    let (cols, rows) = console_size().unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    let frame = render(state.as_ref(), age_ms, cols, rows);
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{frame}");
    ExitCode::SUCCESS
}

/// Runs the interactive full-screen loop until a quit key or I/O failure.
fn interactive(path: &Path) -> AppResult<ExitCode> {
    let Ok(guard) = ConsoleGuard::setup() else {
        return Ok(render_plain(path));
    };
    let mut stdout = std::io::stdout();
    loop {
        let state = read_dashboard(path);
        let age_ms = current_age(state.as_ref());
        let (cols, rows) = guard.size().unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
        let frame = render(state.as_ref(), age_ms, cols, rows);
        if write_frame(&mut stdout, &frame).is_err() {
            break;
        }
        match poll_quit(&guard) {
            Ok(true) | Err(_) => break,
            Ok(false) => {}
        }
    }
    drop(guard);
    Ok(ExitCode::SUCCESS)
}

/// Milliseconds since the state was written; 0 without a state.
fn current_age(state: Option<&DashboardState>) -> u64 {
    state.map_or(0, |state| {
        unix_ms_now().saturating_sub(state.updated_unix_ms)
    })
}

/// Clears the screen, homes the cursor, and writes one frame.
fn write_frame(stdout: &mut impl Write, frame: &str) -> std::io::Result<()> {
    write!(stdout, "\x1b[2J\x1b[H{frame}")?;
    stdout.flush()
}

/// Polls console input for one frame interval, true when the user quit.
fn poll_quit(guard: &ConsoleGuard) -> AppResult<bool> {
    let deadline = Instant::now() + FRAME_INTERVAL;
    loop {
        if drain_input(guard.input)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Drains pending input records; true when a quit key was seen.
fn drain_input(input: HANDLE) -> AppResult<bool> {
    let mut records = [INPUT_RECORD::default(); INPUT_BATCH];
    for _ in 0..INPUT_BATCH_LIMIT {
        let pending = unsafe {
            let mut pending = 0u32;
            GetNumberOfConsoleInputEvents(input, &mut pending)
                .map_err(|error| win32("poll console input events", error))?;
            pending
        };
        if pending == 0 {
            return Ok(false);
        }
        let read = unsafe {
            let batch = pending.min(INPUT_BATCH as u32) as usize;
            let mut read = 0u32;
            ReadConsoleInputW(input, &mut records[..batch], &mut read)
                .map_err(|error| win32("read console input records", error))?;
            read as usize
        };
        for record in &records[..read] {
            if u32::from(record.EventType) != KEY_EVENT {
                continue;
            }
            let (down, code) = unsafe {
                let key = record.Event.KeyEvent;
                (key.bKeyDown.as_bool(), key.uChar.UnicodeChar)
            };
            if down && is_quit_key(code) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `q`, `Q`, Ctrl+C (0x03), and Esc (0x1B) all quit the dashboard.
const fn is_quit_key(code: u16) -> bool {
    matches!(code, 0x03 | 0x1B | 0x71 | 0x51)
}

/// Wraps a [`PlatformError`] for an operation inside this module.
fn win32(operation: &'static str, source: windows::core::Error) -> AppError {
    AppError::platform(PlatformError::win32(operation, source))
}

/// Console modes, code page, and handles saved for restoration on drop.
struct ConsoleGuard {
    input: HANDLE,
    output: HANDLE,
    input_mode: CONSOLE_MODE,
    output_mode: CONSOLE_MODE,
    output_cp: u32,
}

impl ConsoleGuard {
    /// Puts the console into raw VT-capable mode, rolling back partial
    /// changes so a failure never leaves the console altered.
    fn setup() -> AppResult<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE)
                .map_err(|error| win32("resolve the stdin console handle", error))?;
            let output = GetStdHandle(STD_OUTPUT_HANDLE)
                .map_err(|error| win32("resolve the stdout console handle", error))?;
            let input_mode = console_mode(input, "read the stdin console mode")?;
            let output_mode = console_mode(output, "read the stdout console mode")?;
            let output_cp = GetConsoleOutputCP();

            let next_input = CONSOLE_MODE(
                input_mode.0
                    & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0),
            );
            let next_output = CONSOLE_MODE(output_mode.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0);

            if let Err(error) = SetConsoleMode(input, next_input) {
                return Err(win32("switch stdin to character input", error));
            }
            if let Err(error) = SetConsoleMode(output, next_output) {
                let _ = SetConsoleMode(input, input_mode);
                return Err(win32("enable virtual terminal output", error));
            }
            if let Err(error) = SetConsoleOutputCP(UTF8_CODE_PAGE) {
                let _ = SetConsoleMode(output, output_mode);
                let _ = SetConsoleMode(input, input_mode);
                return Err(win32("switch the console output code page to UTF-8", error));
            }

            Ok(Self {
                input,
                output,
                input_mode,
                output_mode,
                output_cp,
            })
        }
    }

    /// Visible window size in columns and rows, if queryable.
    fn size(&self) -> Option<(usize, usize)> {
        window_size(self.output)
    }
}

impl Drop for ConsoleGuard {
    /// Restores every saved mode and code page on all exit paths.
    fn drop(&mut self) {
        unsafe {
            let _ = SetConsoleOutputCP(self.output_cp);
            let _ = SetConsoleMode(self.output, self.output_mode);
            let _ = SetConsoleMode(self.input, self.input_mode);
        }
    }
}

/// Reads the current mode of one console handle.
fn console_mode(handle: HANDLE, operation: &'static str) -> AppResult<CONSOLE_MODE> {
    let mut mode = CONSOLE_MODE(0);
    unsafe { GetConsoleMode(handle, &mut mode) }.map_err(|error| win32(operation, error))?;
    Ok(mode)
}

/// Visible window size of the stdout console, if it is a console.
fn console_size() -> Option<(usize, usize)> {
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.ok()?;
    window_size(output)
}

/// Queries [`GetConsoleScreenBufferInfo`] for the visible window extent.
fn window_size(output: HANDLE) -> Option<(usize, usize)> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    unsafe { GetConsoleScreenBufferInfo(output, &mut info) }.ok()?;
    let cols = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
    let rows = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
    if cols < 1 || rows < 1 {
        return None;
    }
    Some((cols as usize, rows as usize))
}

/// Renders one complete dashboard frame without any ANSI sequences.
///
/// Terminals with `cols >= 60` get a tmux-style two-zone frame: a left
/// status sidebar, a `│` gutter column, and the proportional pane map on
/// the right, optionally under a full-width offline banner. Narrower
/// terminals keep the legacy single-column layout. The output never
/// contains more than `rows` lines and no line ever exceeds `cols`
/// columns. A missing, stale, stopped, or pane-less state falls back to
/// placeholder text plus an offline banner instead of a pane map.
/// Degenerate sizes (0x0, 1x1, columns below 10) never panic.
pub fn render(state: Option<&DashboardState>, age_ms: u64, cols: usize, rows: usize) -> String {
    if cols >= TWO_ZONE_MIN_COLS {
        render_two_zone(state, age_ms, cols, rows)
    } else {
        render_narrow(state, age_ms, cols, rows)
    }
}

/// Legacy single-column frame used below [`TWO_ZONE_MIN_COLS`] columns.
fn render_narrow(state: Option<&DashboardState>, age_ms: u64, cols: usize, rows: usize) -> String {
    let (prefix, terminal, mouse) = match state {
        Some(state) => (
            if state.prefix_armed { "ARMED" } else { "idle" },
            if state.terminal_present {
                "connected"
            } else {
                "not found"
            },
            if state.mouse_resize_enabled {
                "on"
            } else {
                "off"
            },
        ),
        None => ("idle", "not found", "off"),
    };
    let footer = format!("refreshed {age_ms}ms ago \u{2014} q to quit");
    let mut lines = vec![format!(
        "winter ui  prefix: {prefix}  terminal: {terminal}  mouse resize: {mouse}"
    )];

    let offline = is_offline(state, age_ms);
    if offline {
        lines.push(OFFLINE_BANNER.to_owned());
    }

    let pane_count = state.map_or(0, |state| state.panes.len());
    let mapped = state
        .filter(|state| !offline && !state.panes.is_empty() && cols >= MIN_MAP_COLS)
        .and_then(|state| {
            let map_height = rows.saturating_sub(lines.len() + 5);
            let map_width = cols.saturating_sub(2);
            if map_height < MIN_MAP_HEIGHT || map_width < 3 {
                return None;
            }
            let map = draw_map(&state.panes, map_width, map_height);
            (map.len() == map_height).then_some(map)
        });

    if let Some(map) = mapped {
        lines.push(String::new());
        lines.extend(map);
        lines.push("* = focused pane".to_owned());
        lines.push(format!("{pane_count} panes"));
        lines.push(footer);
        return fit(lines, cols, rows);
    }

    lines.push(String::new());
    lines.push(placeholder(offline, pane_count));
    lines.push(footer);
    fit(lines, cols, rows)
}

/// Two-zone frame: sidebar, `│` gutter, and the pane map zone, placed
/// under a full-width offline banner whenever the controller is offline.
fn render_two_zone(
    state: Option<&DashboardState>,
    age_ms: u64,
    cols: usize,
    rows: usize,
) -> String {
    let sidebar_width = (cols / 3).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
    let right_width = cols - sidebar_width - 1;
    let offline = is_offline(state, age_ms);

    let mut frame: Vec<String> = Vec::new();
    if offline {
        frame.push(OFFLINE_BANNER.to_owned());
    }
    let budget = rows.saturating_sub(frame.len());
    let sidebar = fit_sidebar(sidebar_lines(state, age_ms, sidebar_width), budget);
    let right = right_zone_lines(state, age_ms, offline, right_width, budget);

    let height = sidebar.len().max(right.len());
    for index in 0..height {
        let left = sidebar.get(index).map(String::as_str).unwrap_or("");
        let zone = right.get(index).map(String::as_str).unwrap_or("");
        frame.push(format!(
            "{}\u{2502}{}",
            pad(left, sidebar_width),
            pad(zone, right_width)
        ));
    }
    fit(frame, cols, rows)
}

/// Right zone content: pane map (or placeholder), legend, and the footer,
/// every line cut to `width` and never more than `rows` lines.
fn right_zone_lines(
    state: Option<&DashboardState>,
    age_ms: u64,
    offline: bool,
    width: usize,
    rows: usize,
) -> Vec<String> {
    let mut lines = vec![String::new()];
    let pane_count = state.map_or(0, |state| state.panes.len());
    let mapped = state
        .filter(|state| !offline && !state.panes.is_empty() && width >= MIN_MAP_COLS)
        .and_then(|state| {
            let map_height = rows.saturating_sub(lines.len() + 5);
            let map_width = width.saturating_sub(2);
            if map_height < MIN_MAP_HEIGHT || map_width < 3 {
                return None;
            }
            let map = draw_map(&state.panes, map_width, map_height);
            (map.len() == map_height).then_some(map)
        });
    if let Some(map) = mapped {
        lines.extend(map);
        lines.push("* = focused pane".to_owned());
    } else {
        lines.push(placeholder(offline, pane_count));
    }
    lines.push(format!(
        "{pane_count} panes \u{B7} refreshed {age_ms}ms ago"
    ));
    fit_lines(lines, width, rows)
}

/// Builds the left sidebar: header, controller section, pane list, bottom
/// rule, and quit hint. Every line is cut to `width`; row budgeting is
/// the job of [`fit_sidebar`].
fn sidebar_lines(state: Option<&DashboardState>, age_ms: u64, width: usize) -> Vec<String> {
    let offline = is_offline(state, age_ms);
    let mut lines = vec!["winter ui".to_owned()];
    lines.push(section_rule(
        "\u{2500}\u{2500} controller \u{2500}\u{2500}",
        width,
    ));

    let uptime = match state
        .filter(|_| !offline)
        .and_then(DashboardState::uptime_ms)
    {
        Some(elapsed) => format_uptime(elapsed),
        None if offline => "OFFLINE \u{2190}".to_owned(),
        None => "unknown".to_owned(),
    };
    lines.push(sidebar_row("online", &uptime, width));

    let (prefix, hook, mouse, terminal, actions) = match state {
        Some(state) => {
            let hook = if !state.hook_active {
                "INACTIVE!".to_owned()
            } else if state.hook_panics > 0 {
                format!("{} panics!", state.hook_panics)
            } else {
                "ok".to_owned()
            };
            (
                if state.prefix_armed { "ARMED" } else { "idle" },
                hook,
                if state.mouse_resize_enabled {
                    "on"
                } else {
                    "off"
                },
                if state.terminal_present {
                    "connected"
                } else {
                    "missing"
                },
                format!(
                    "{} / {} fail / {} drop",
                    state.dispatched_actions, state.failed_actions, state.dropped_actions
                ),
            )
        }
        None => (
            "idle",
            "INACTIVE!".to_owned(),
            "off",
            "missing",
            "0 / 0 fail / 0 drop".to_owned(),
        ),
    };
    lines.push(sidebar_row("prefix", prefix, width));
    lines.push(sidebar_row("hook", &hook, width));
    lines.push(sidebar_row("mouse", mouse, width));
    lines.push(sidebar_row("terminal", terminal, width));
    lines.push(sidebar_row("actions", &actions, width));
    if let Some(error) = state.and_then(|state| state.last_dispatch_error.as_deref()) {
        lines.push(truncate(&format!("err: {error}"), width));
    }

    lines.push(section_rule(
        "\u{2500}\u{2500} panes \u{2500}\u{2500}",
        width,
    ));
    if let Some(state) = state {
        for (index, pane) in state.panes.iter().enumerate() {
            let marker = if pane.focused { '*' } else { ' ' };
            let title = if pane.title.is_empty() {
                "-"
            } else {
                pane.title.as_str()
            };
            let number = index + 1;
            lines.push(truncate(&format!(" {number} {marker}{title}"), width));
        }
    }
    lines.push("\u{2500}".repeat(width));
    lines.push("q quit".to_owned());
    lines
}

/// Shrinks a full sidebar to `rows` lines by dropping sections bottom-up
/// while keeping the header, the pane list, and the quit hint as long as
/// possible; as a last resort even those give way so the budget holds.
fn fit_sidebar(lines: Vec<String>, rows: usize) -> Vec<String> {
    let mut out = lines;
    // The bottom rule first.
    if out.len() > rows && out.len() >= 2 && is_rule(&out[out.len() - 2]) {
        out.remove(out.len() - 2);
    }
    // Controller rows, from the bottom of the section up.
    while out.len() > rows {
        let Some(panes) = out.iter().position(|line| line.starts_with("── panes")) else {
            break;
        };
        if panes <= 2 {
            break;
        }
        out.remove(panes - 1);
    }
    // The controller section header.
    if out.len() > rows
        && out
            .get(1)
            .is_some_and(|line| line.starts_with("── controller"))
    {
        out.remove(1);
    }
    // Pane entries, from the last one up.
    while out.len() > rows {
        let Some(panes) = out.iter().position(|line| line.starts_with("── panes")) else {
            break;
        };
        if out.len() <= panes + 2 {
            break;
        }
        out.remove(panes + 1);
    }
    // The pane section header, then the quit hint, then the header.
    if out.len() > rows
        && let Some(panes) = out.iter().position(|line| line.starts_with("── panes"))
    {
        out.remove(panes);
    }
    while out.len() > rows {
        out.pop();
    }
    out
}

/// One `label   value` sidebar line cut to `width`.
fn sidebar_row(label: &str, value: &str, width: usize) -> String {
    truncate(&format!("{label:<8} {value}"), width)
}

/// `── title ──` padded (or cut) with `─` to exactly `width` characters.
fn section_rule(title: &str, width: usize) -> String {
    title
        .chars()
        .chain(std::iter::repeat_n('\u{2500}', width))
        .take(width)
        .collect()
}

/// True for a line made only of `─` characters (the section bottom rule).
fn is_rule(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|character| character == '\u{2500}')
}

/// Human-readable controller uptime: `7s`, `3m 12s`, or `1h 2m 3s`.
fn format_uptime(ms: u64) -> String {
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Cuts `line` to `width` characters and pads it back to `width`.
fn pad(line: &str, width: usize) -> String {
    let mut padded = truncate(line, width);
    let missing = width.saturating_sub(padded.chars().count());
    padded.extend(std::iter::repeat_n(' ', missing));
    padded
}

/// True when the state is absent, stale, or the controller has stopped.
fn is_offline(state: Option<&DashboardState>, age_ms: u64) -> bool {
    match state {
        None => true,
        Some(state) => !state.controller_running || age_ms > DASHBOARD_STALE_AFTER_MS,
    }
}

/// Explanatory line used whenever the pane map cannot be drawn.
fn placeholder(offline: bool, pane_count: usize) -> String {
    if offline {
        "no live pane data \u{2014} the controller is not running".to_owned()
    } else if pane_count == 0 {
        "the controller reports no panes yet".to_owned()
    } else {
        "terminal is too small for the pane map".to_owned()
    }
}

/// A pane rectangle in grid coordinates, inclusive on all edges.
#[derive(Debug, Clone, Copy)]
struct PaneRect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    focused: bool,
}

/// Scales one screen coordinate onto the grid extent.
fn scale(value: f64, span: f64, extent: usize) -> usize {
    let scaled = (value * extent as f64 / span).round();
    if scaled <= 0.0 {
        0
    } else if scaled >= extent as f64 {
        extent
    } else {
        scaled as usize
    }
}

/// Proportionally maps every pane onto the `width` x `height` grid.
fn map_rects(panes: &[DashboardPane], width: usize, height: usize) -> Option<Vec<PaneRect>> {
    if panes.is_empty() || width < 3 || height < 3 {
        return None;
    }
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for pane in panes {
        let right = pane.x.saturating_add(pane.width.max(1));
        let bottom = pane.y.saturating_add(pane.height.max(1));
        min_x = min_x.min(pane.x);
        min_y = min_y.min(pane.y);
        max_x = max_x.max(right);
        max_y = max_y.max(bottom);
    }
    let span_x = (i64::from(max_x) - i64::from(min_x)).max(1) as f64;
    let span_y = (i64::from(max_y) - i64::from(min_y)).max(1) as f64;
    let extent_x = width - 1;
    let extent_y = height - 1;

    let mut rects = Vec::with_capacity(panes.len());
    for pane in panes {
        let right = pane.x.saturating_add(pane.width.max(1));
        let bottom = pane.y.saturating_add(pane.height.max(1));
        let mut x0 = scale(
            (i64::from(pane.x) - i64::from(min_x)) as f64,
            span_x,
            extent_x,
        );
        let mut y0 = scale(
            (i64::from(pane.y) - i64::from(min_y)) as f64,
            span_y,
            extent_y,
        );
        let mut x1 = scale(
            (i64::from(right) - i64::from(min_x)) as f64,
            span_x,
            extent_x,
        );
        let mut y1 = scale(
            (i64::from(bottom) - i64::from(min_y)) as f64,
            span_y,
            extent_y,
        );
        x1 = x1.max(x0 + 1).min(extent_x);
        y1 = y1.max(y0 + 1).min(extent_y);
        if x0 >= x1 {
            x0 = x1 - 1;
        }
        if y0 >= y1 {
            y0 = y1 - 1;
        }
        rects.push(PaneRect {
            x0,
            y0,
            x1,
            y1,
            focused: pane.focused,
        });
    }
    Some(rects)
}

/// ORs one horizontal border run (corners included) into the grid.
fn add_horizontal(grid: &mut [u8], width: usize, y: usize, x0: usize, x1: usize) {
    for x in x0..=x1 {
        let bits = if x == x0 {
            EDGE_RIGHT
        } else if x == x1 {
            EDGE_LEFT
        } else {
            EDGE_LEFT | EDGE_RIGHT
        };
        grid[y * width + x] |= bits;
    }
}

/// ORs one vertical border run (corners included) into the grid.
fn add_vertical(grid: &mut [u8], width: usize, x: usize, y0: usize, y1: usize) {
    for y in y0..=y1 {
        let bits = if y == y0 {
            EDGE_DOWN
        } else if y == y1 {
            EDGE_UP
        } else {
            EDGE_UP | EDGE_DOWN
        };
        grid[y * width + x] |= bits;
    }
}

/// Completes the outer frame on perimeter cells no pane border covered.
fn fill_frame(grid: &mut [u8], width: usize, height: usize) {
    let corners = [
        (0, CORNER_TOP_LEFT),
        (width - 1, CORNER_TOP_RIGHT),
        ((height - 1) * width, CORNER_BOTTOM_LEFT),
        (height * width - 1, CORNER_BOTTOM_RIGHT),
    ];
    for (index, bits) in corners {
        if grid[index] == 0 {
            grid[index] = bits;
        }
    }
    for x in 1..width - 1 {
        for y in [0, height - 1] {
            let index = y * width + x;
            if grid[index] == 0 {
                grid[index] = EDGE_LEFT | EDGE_RIGHT;
            }
        }
    }
    for y in 1..height - 1 {
        for x in [0, width - 1] {
            let index = y * width + x;
            if grid[index] == 0 {
                grid[index] = EDGE_UP | EDGE_DOWN;
            }
        }
    }
}

/// Draws the bordered pane map as exactly `height` lines of `width` chars.
fn draw_map(panes: &[DashboardPane], width: usize, height: usize) -> Vec<String> {
    let Some(rects) = map_rects(panes, width, height) else {
        return Vec::new();
    };
    let mut grid = vec![0u8; width * height];
    for rect in &rects {
        add_horizontal(&mut grid, width, rect.y0, rect.x0, rect.x1);
        add_horizontal(&mut grid, width, rect.y1, rect.x0, rect.x1);
        add_vertical(&mut grid, width, rect.x0, rect.y0, rect.y1);
        add_vertical(&mut grid, width, rect.x1, rect.y0, rect.y1);
    }
    fill_frame(&mut grid, width, height);
    for rect in rects.iter().filter(|rect| rect.focused) {
        let (x, y) = (rect.x0 + 1, rect.y0 + 1);
        if x < rect.x1 && y < rect.y1 && grid[y * width + x] == 0 {
            grid[y * width + x] = FOCUS_MARKER;
        }
    }
    grid.chunks(width)
        .map(|row| row.iter().map(|&bits| edge_char(bits)).collect())
        .collect()
}

/// Maps accumulated edge bits (plus the focus marker) onto a box character.
const fn edge_char(bits: u8) -> char {
    match bits {
        FOCUS_MARKER => '*',
        EDGE_VERTICAL => '\u{2502}',
        EDGE_HORIZONTAL => '\u{2500}',
        CORNER_BOTTOM_LEFT => '\u{2514}',
        CORNER_TOP_LEFT => '\u{250C}',
        CORNER_BOTTOM_RIGHT => '\u{2518}',
        CORNER_TOP_RIGHT => '\u{2510}',
        JOIN_T_RIGHT => '\u{251C}',
        JOIN_T_LEFT => '\u{2524}',
        JOIN_T_UP => '\u{2534}',
        JOIN_T_DOWN => '\u{252C}',
        JOIN_CROSS => '\u{253C}',
        _ => ' ',
    }
}

/// Trims lines to `rows` and every line to `cols`, then joins them.
fn fit(lines: Vec<String>, cols: usize, rows: usize) -> String {
    fit_lines(lines, cols, rows).join("\n")
}

/// Trims lines to `rows` and every line to `cols`.
fn fit_lines(lines: Vec<String>, cols: usize, rows: usize) -> Vec<String> {
    lines
        .into_iter()
        .take(rows)
        .map(|line| truncate(&line, cols))
        .collect()
}

/// Cuts a line to at most `cols` characters.
fn truncate(line: &str, cols: usize) -> String {
    if line.chars().count() <= cols {
        return line.to_owned();
    }
    line.chars().take(cols).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> DashboardState {
        DashboardState {
            schema_version: crate::dashboard::DASHBOARD_SCHEMA_VERSION,
            updated_unix_ms: unix_ms_now(),
            controller_running: true,
            prefix_armed: true,
            mouse_resize_enabled: true,
            terminal_present: true,
            controller_started_unix_ms: unix_ms_now().saturating_sub(192_000),
            hook_active: true,
            hook_panics: 2,
            dispatched_actions: 128,
            failed_actions: 1,
            dropped_actions: 2,
            last_dispatch_error: Some("injection failed".to_owned()),
            panes: vec![
                DashboardPane {
                    x: 0,
                    y: 0,
                    width: 40,
                    height: 12,
                    focused: false,
                    title: "root:~".to_owned(),
                },
                DashboardPane {
                    x: 40,
                    y: 0,
                    width: 40,
                    height: 12,
                    focused: true,
                    title: "agent-build".to_owned(),
                },
                DashboardPane {
                    x: 0,
                    y: 12,
                    width: 80,
                    height: 12,
                    focused: false,
                    title: String::new(),
                },
            ],
            dividers: Vec::new(),
        }
    }

    #[test]
    fn renders_a_live_fixture_within_bounds() {
        let frame = render(Some(&fixture()), 5, 100, 30);

        assert!(frame.contains("winter ui"), "missing header: {frame}");
        assert!(
            frame.contains("\u{2500}\u{2500} controller \u{2500}\u{2500}"),
            "missing controller rule: {frame}"
        );
        assert!(frame.contains("3m 12s"), "missing uptime: {frame}");
        assert!(frame.contains("prefix   ARMED"), "missing prefix: {frame}");
        assert!(frame.contains("2 panics"), "missing hook alarm: {frame}");
        assert!(frame.contains("mouse    on"), "missing mouse: {frame}");
        assert!(
            frame.contains("terminal connected"),
            "missing terminal: {frame}"
        );
        assert!(
            frame.contains("128 / 1 fail / 2 drop"),
            "missing action counters: {frame}"
        );
        assert!(
            frame.contains("err: injection failed"),
            "missing dispatch error: {frame}"
        );
        assert!(
            frame.contains("\u{2500}\u{2500} panes \u{2500}\u{2500}"),
            "missing panes rule: {frame}"
        );
        assert!(frame.contains(" 1  root:~"), "missing pane 1: {frame}");
        assert!(
            frame.contains(" 2 *agent-build"),
            "missing focused pane: {frame}"
        );
        assert!(frame.contains(" 3  -"), "missing dash title: {frame}");
        assert!(frame.contains("q quit"), "missing quit hint: {frame}");
        assert!(
            frame.contains("* = focused pane"),
            "missing legend: {frame}"
        );
        assert!(
            frame.contains("3 panes \u{B7} refreshed 5ms ago"),
            "missing footer: {frame}"
        );
        assert!(!frame.contains("CONTROLLER OFFLINE"), "live: {frame}");

        for character in [
            '\u{250C}', '\u{252C}', '\u{2510}', '\u{2502}', '\u{2500}', '\u{2514}', '\u{2518}',
            '\u{2534}',
        ] {
            assert!(
                frame.contains(character),
                "missing box character {character:?}: {frame}"
            );
        }
        assert!(
            frame.matches('*').count() >= 2,
            "the focused pane needs a marker beyond the legend: {frame}"
        );

        let lines: Vec<&str> = frame.split('\n').collect();
        assert!(lines.len() <= 30, "row budget exceeded: {}", lines.len());
        assert!(
            lines.iter().all(|line| line.chars().count() <= 100),
            "a line exceeded the column budget: {frame}"
        );
    }

    #[test]
    fn sidebar_titles_are_dashed_and_cut_to_the_width() {
        let mut state = fixture();
        state.panes[0].title.clear();
        state.panes[1].title = "a-very-long-pane-title-that-overflows-the-sidebar".to_owned();
        let lines = sidebar_lines(Some(&state), 5, SIDEBAR_MIN_WIDTH);

        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() <= SIDEBAR_MIN_WIDTH),
            "a sidebar line exceeded the width: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == " 1  -"),
            "an empty title must render as a dash: {lines:?}"
        );
        let long = lines
            .iter()
            .find(|line| line.starts_with(" 2 "))
            .expect("the focused pane entry should exist");
        assert_eq!(
            long.chars().count(),
            SIDEBAR_MIN_WIDTH,
            "the long title must be cut to the width: {long:?}"
        );
        assert!(
            long.contains("*a-very-long"),
            "the focus marker must survive truncation: {long:?}"
        );
    }

    #[test]
    fn drops_sidebar_sections_bottom_up_when_rows_are_scarce() {
        let frame = render(Some(&fixture()), 5, 100, 7);

        let lines: Vec<&str> = frame.split('\n').collect();
        assert_eq!(lines.len(), 7, "row budget exceeded: {frame}");
        assert!(frame.contains("winter ui"), "missing header: {frame}");
        assert!(
            frame.contains("\u{2500}\u{2500} panes \u{2500}\u{2500}"),
            "the pane section must survive: {frame}"
        );
        assert!(frame.contains("q quit"), "missing quit hint: {frame}");
        assert!(
            lines.iter().all(|line| line.chars().count() <= 100),
            "a line exceeded the column budget: {frame}"
        );
    }

    #[test]
    fn renders_the_legacy_single_column_layout_below_sixty_columns() {
        let frame = render(Some(&fixture()), 5, 50, 30);

        assert!(
            !frame.contains("\u{2500}\u{2500} controller \u{2500}\u{2500}"),
            "no sidebar below 60 columns: {frame}"
        );
        assert!(
            frame.contains("winter ui  prefix: ARMED"),
            "missing legacy status line: {frame}"
        );
        assert!(
            frame.contains("* = focused pane"),
            "missing legend: {frame}"
        );
        assert!(frame.contains("3 panes"), "missing pane count: {frame}");
        assert!(
            frame.contains("refreshed 5ms ago \u{2014} q to quit"),
            "missing footer: {frame}"
        );

        let lines: Vec<&str> = frame.split('\n').collect();
        assert!(lines.len() <= 30, "row budget exceeded: {}", lines.len());
        assert!(
            lines.iter().all(|line| line.chars().count() <= 50),
            "a line exceeded the column budget: {frame}"
        );
    }

    #[test]
    fn renders_offline_banner_without_a_state_file() {
        let frame = render(None, 0, 100, 30);

        assert!(
            frame.contains("!! CONTROLLER OFFLINE \u{2014} start it with 'winter run' !!"),
            "missing offline banner: {frame}"
        );
        assert!(
            frame.starts_with("!! CONTROLLER OFFLINE"),
            "the banner must be the top line: {frame}"
        );
        assert!(
            frame.contains("\u{2500}\u{2500} controller \u{2500}\u{2500}"),
            "missing controller rule: {frame}"
        );
        assert!(
            frame.contains("online   OFFLINE"),
            "the sidebar must show OFFLINE: {frame}"
        );
        assert!(frame.contains("prefix   idle"), "missing prefix: {frame}");
        assert!(
            frame.contains("terminal missing"),
            "missing terminal: {frame}"
        );
        assert!(frame.contains("q quit"), "missing quit hint: {frame}");

        let lines: Vec<&str> = frame.split('\n').collect();
        assert!(lines.len() <= 30, "row budget exceeded: {}", lines.len());
        assert!(
            lines.iter().all(|line| line.chars().count() <= 100),
            "a line exceeded the column budget: {frame}"
        );
    }

    #[test]
    fn renders_offline_banner_for_stale_or_stopped_state() {
        let stale = render(Some(&fixture()), DASHBOARD_STALE_AFTER_MS + 1, 100, 30);
        assert!(
            stale.contains("CONTROLLER OFFLINE"),
            "a stale state must show the banner: {stale}"
        );

        let mut stopped = fixture();
        stopped.controller_running = false;
        let stopped = render(Some(&stopped), 5, 100, 30);
        assert!(
            stopped.contains("CONTROLLER OFFLINE"),
            "a stopped controller must show the banner: {stopped}"
        );
    }

    #[test]
    fn renders_a_placeholder_when_the_controller_reports_no_panes() {
        let mut state = fixture();
        state.panes.clear();
        let frame = render(Some(&state), 5, 100, 30);

        assert!(frame.contains("no panes"), "missing placeholder: {frame}");
        assert!(
            !frame.contains("CONTROLLER OFFLINE"),
            "a fresh pane-less state is not offline: {frame}"
        );
        assert!(!frame.contains('\u{250C}'), "no map without panes: {frame}");
    }

    #[test]
    fn survives_degenerate_frame_sizes() {
        let state = fixture();

        assert_eq!(render(Some(&state), 5, 0, 0), "");
        let one = render(Some(&state), 5, 1, 1);
        assert!(one.split('\n').count() <= 1, "1x1 frame: {one:?}");
        assert!(one.chars().count() <= 1, "1x1 frame: {one:?}");

        let narrow = render(Some(&state), 5, 9, 40);
        assert!(
            !narrow.contains('\u{250C}'),
            "cols below 10 must not draw a box: {narrow}"
        );
        assert!(narrow.contains("winter ui"), "header: {narrow}");

        let short = render(Some(&state), 5, 40, 2);
        assert!(short.split('\n').count() <= 2, "2-row frame: {short:?}");
        let empty_panes: Vec<DashboardPane> = Vec::new();
        assert!(draw_map(&empty_panes, 40, 10).is_empty());
        assert!(draw_map(&state.panes, 2, 2).is_empty());
    }
}
