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
/// The output never contains more than `rows` lines and no line ever
/// exceeds `cols` columns. A missing, stale, stopped, or pane-less state
/// falls back to placeholder text plus an offline banner instead of a pane
/// map. Degenerate sizes (0x0, 1x1, columns below 10) never panic.
pub fn render(state: Option<&DashboardState>, age_ms: u64, cols: usize, rows: usize) -> String {
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
        lines.push("!! CONTROLLER OFFLINE \u{2014} start it with 'winter run' !!".to_owned());
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
    lines
        .into_iter()
        .take(rows)
        .map(|line| truncate(&line, cols))
        .collect::<Vec<String>>()
        .join("\n")
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
            panes: vec![
                DashboardPane {
                    x: 0,
                    y: 0,
                    width: 40,
                    height: 12,
                    focused: false,
                },
                DashboardPane {
                    x: 40,
                    y: 0,
                    width: 40,
                    height: 12,
                    focused: true,
                },
                DashboardPane {
                    x: 0,
                    y: 12,
                    width: 80,
                    height: 12,
                    focused: false,
                },
            ],
            dividers: Vec::new(),
        }
    }

    #[test]
    fn renders_a_live_fixture_within_bounds() {
        let frame = render(Some(&fixture()), 5, 100, 30);

        assert!(frame.contains("winter ui"), "missing header: {frame}");
        assert!(frame.contains("prefix: ARMED"), "missing prefix: {frame}");
        assert!(
            frame.contains("terminal: connected"),
            "missing terminal: {frame}"
        );
        assert!(
            frame.contains("mouse resize: on"),
            "missing mouse resize: {frame}"
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
    fn renders_offline_banner_without_a_state_file() {
        let frame = render(None, 0, 100, 30);

        assert!(
            frame.contains("!! CONTROLLER OFFLINE \u{2014} start it with 'winter run' !!"),
            "missing offline banner: {frame}"
        );
        assert!(frame.contains("winter ui"), "missing header: {frame}");
        assert!(frame.contains("prefix: idle"), "missing header: {frame}");
        assert!(
            frame.contains("terminal: not found"),
            "missing header: {frame}"
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
