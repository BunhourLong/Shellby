//! VT parsing and grid state: wraps `alacritty_terminal` (D-T02-1) and turns PTY bytes into
//! a reusable render [`Frame`] plus validated [`TermEvent`]s. No alacritty type is public
//! except the [`FairMutex`] re-export used by [`SharedTerminal`].
#![deny(unsafe_code)]

mod color;
mod events;
mod fill;
mod frame;
mod osc;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::{ClipboardType, Osc52, TermMode};
use alacritty_terminal::vte::ansi::Processor;

pub use alacritty_terminal::sync::FairMutex;
pub use color::Palette;
pub use frame::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};

use osc::Scanned;
use osc::notify::K99Out;

/// Grid size plus cell size in pixels (pixels answer `CSI 14 t`-style size queries).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TermSize {
    /// Columns, ≥ 1.
    pub cols: u16,
    /// Rows, ≥ 1.
    pub rows: u16,
    /// Cell width in pixels.
    pub cell_w_px: u16,
    /// Cell height in pixels.
    pub cell_h_px: u16,
}

/// Terminal settings supplied by the app.
#[derive(Clone, Debug)]
pub struct TermConfig {
    /// Scrollback lines; clamped to 100,000.
    pub scrollback_lines: usize,
    /// Colors.
    pub palette: Palette,
    /// Bold text in ANSI colors 0-7 uses the bright variant (xterm behaviour).
    pub bold_is_bright: bool,
    /// This machine's hostname, used to accept OSC 7 reports; supplied by `nt-app`.
    pub hostname: String,
}

impl Default for TermConfig {
    /// 10,000 lines of scrollback, default palette, bold-is-bright, empty hostname.
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
            palette: Palette::default(),
            bold_is_bright: true,
            hostname: String::new(),
        }
    }
}

/// Mouse reporting requested by the program.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    /// No reporting.
    #[default]
    None,
    /// Button presses (1000).
    Click,
    /// Presses and drags (1002).
    Drag,
    /// All motion (1003).
    Motion,
}

/// Terminal modes the input encoder (T09) needs.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors independent DEC modes"
)]
pub struct TermModes {
    /// DECCKM application cursor keys.
    pub app_cursor: bool,
    /// DECKPAM application keypad.
    pub app_keypad: bool,
    /// Bracketed paste (2004).
    pub bracketed_paste: bool,
    /// Mouse reporting level.
    pub mouse: MouseMode,
    /// SGR mouse encoding (1006).
    pub mouse_sgr: bool,
    /// UTF-8 mouse encoding (1005).
    pub mouse_utf8: bool,
    /// Focus in/out reporting (1004).
    pub focus_reporting: bool,
    /// Alternate screen active.
    pub alt_screen: bool,
    /// Wheel sends arrow keys on the alt screen (1007).
    pub alternate_scroll: bool,
    /// Cursor visible (DECTCEM).
    pub show_cursor: bool,
    /// LNM: Enter sends CR LF.
    pub line_feed_new_line: bool,
    /// Kitty keyboard progressive-enhancement flags (bits 0..=4).
    pub kitty_keyboard: u8,
}

/// Which program-side protocol produced a notification.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NotifySource {
    /// iTerm2 `OSC 9`.
    Osc9,
    /// kitty `OSC 99`.
    Osc99,
    /// rxvt `OSC 777 ; notify`.
    Osc777,
}

/// OSC 52 clipboard selection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClipboardTarget {
    /// The system clipboard.
    Clipboard,
    /// The primary selection.
    Primary,
}

/// A validated OSC 7 working-directory report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CwdReport {
    /// `""` for this machine.
    pub host: String,
    /// Absolute, NUL-free, ≤ 4096 bytes. Not checked against the filesystem.
    pub path: PathBuf,
}

/// Side effects of parsing; every string is sanitised and length-capped.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TermEvent {
    /// Window title (OSC 0/2), ≤ 1024 chars.
    Title(String),
    /// Title reset to the default.
    ResetTitle,
    /// BEL.
    Bell,
    /// OSC 7.
    Cwd(CwdReport),
    /// OSC 9 / 99 / 777 notification. `body` is non-empty.
    Notify {
        /// OSC 99 `i=`, for replace/close.
        id: Option<String>,
        /// Title, ≤ 256 chars.
        title: Option<String>,
        /// Body, ≤ 4096 chars.
        body: String,
        /// Protocol.
        source: NotifySource,
    },
    /// OSC 99 `p=close`.
    NotifyClose {
        /// The notification to close.
        id: String,
    },
    /// OSC 52 store, decoded, ≤ 1 MiB. The app applies policy.
    ClipboardStore {
        /// Selection.
        target: ClipboardTarget,
        /// Decoded bytes.
        data: Vec<u8>,
    },
    /// OSC 52 query. The app applies policy (default deny).
    ClipboardLoad {
        /// Selection.
        target: ClipboardTarget,
    },
    /// Protocol reply bytes the app must write to the PTY, in order.
    PtyWrite(Vec<u8>),
    /// Not produced by the parser; app-level.
    ChildExit,
}

/// Counters for review and tests.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TermStats {
    /// Events dropped by the per-`advance` caps.
    pub dropped_events: u64,
}

/// A terminal shared between its reader thread (`lock_unfair`) and the renderer (`lock`).
pub type SharedTerminal = Arc<FairMutex<Terminal>>;

/// Grid dimensions handed to the core; the viewport is the whole grid.
struct SizeDims {
    rows: usize,
    cols: usize,
}

impl Dimensions for SizeDims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Expected length of the UTF-8 sequence led by `lead`; 1 for ASCII and invalid leads.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}

fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// Bytes at the end of `chunk` that start a UTF-8 sequence it does not finish.
fn incomplete_tail(chunk: &[u8]) -> usize {
    for i in (chunk.len().saturating_sub(3)..chunk.len()).rev() {
        if !is_continuation(chunk[i]) {
            let have = chunk.len() - i;
            return if utf8_len(chunk[i]) > have { have } else { 0 };
        }
    }
    0
}

/// Feeds the core so that no `advance` call ends inside a UTF-8 sequence.
/// Works around vte 0.15 `advance_partial_utf8` (lib.rs:680-693): a char completed across
/// calls and followed by an invalid byte within 3 bytes skips the valid bytes after it.
/// Remove when `alacritty_terminal` ships a fixed vte (test `c5_split_utf8_then_invalid_byte_keeps_text`).
#[derive(Debug, Default)]
struct Utf8Carry {
    buf: [u8; 4],
    len: usize,
}

impl Utf8Carry {
    fn feed(
        &mut self,
        parser: &mut Processor,
        core: &mut alacritty_terminal::Term<events::EventSink>,
        chunk: &[u8],
    ) {
        let mut chunk = chunk;
        if self.len > 0 {
            let need = utf8_len(self.buf[0]) - self.len;
            let n = chunk
                .iter()
                .take(need)
                .take_while(|&&b| is_continuation(b))
                .count();
            self.buf[self.len..self.len + n].copy_from_slice(&chunk[..n]);
            self.len += n;
            chunk = &chunk[n..];
            if n < need && chunk.is_empty() {
                return; // still incomplete; wait for more bytes
            }
            // Complete, or cut short by a non-continuation byte: either way a char boundary.
            parser.advance(core, &self.buf[..self.len]);
            self.len = 0;
        }
        let keep = incomplete_tail(chunk);
        let (now, later) = chunk.split_at(chunk.len() - keep);
        if !now.is_empty() {
            parser.advance(core, now);
        }
        self.buf[..keep].copy_from_slice(later);
        self.len = keep;
    }
}

/// One terminal: parser, grid, scrollback and OSC side-channel.
pub struct Terminal {
    core: alacritty_terminal::Term<events::EventSink>,
    parser: Processor,
    osc: osc::OscScanner,
    utf8: Utf8Carry,
    kitty: osc::notify::Kitty99,
    size: TermSize,
    palette: Palette,
    bold_is_bright: bool,
    hostname: String,
    last_cursor_row: Option<u16>,
    last_display_offset: usize,
    force_full: bool,
    generation: u64,
    stats: TermStats,
    pending: Arc<Mutex<Vec<Event>>>,
    drain_buf: Vec<Event>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

fn clamped(size: TermSize) -> TermSize {
    TermSize {
        cols: size.cols.max(1),
        rows: size.rows.max(1),
        ..size
    }
}

fn dims(size: TermSize) -> SizeDims {
    SizeDims {
        rows: size.rows as usize,
        cols: size.cols as usize,
    }
}

fn clipboard_target(ty: ClipboardType) -> ClipboardTarget {
    match ty {
        ClipboardType::Clipboard => ClipboardTarget::Clipboard,
        ClipboardType::Selection => ClipboardTarget::Primary,
    }
}

/// Per-`advance` coalescing / bounds, counted across core and OSC events.
struct EventCaps<'a> {
    events: &'a mut Vec<TermEvent>,
    title: Option<usize>,
    bell: bool,
    notify: u32,
    pty_bytes: usize,
    dropped: u64,
}

impl<'a> EventCaps<'a> {
    fn new(events: &'a mut Vec<TermEvent>) -> Self {
        Self {
            events,
            title: None,
            bell: false,
            notify: 0,
            pty_bytes: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, event: TermEvent) {
        match event {
            TermEvent::Bell => self.push_bell(),
            TermEvent::Title(_) | TermEvent::ResetTitle => self.push_title(event),
            TermEvent::Notify { .. } | TermEvent::NotifyClose { .. } => {
                if self.notify < 64 {
                    self.notify += 1;
                    self.events.push(event);
                } else {
                    self.dropped += 1;
                }
            }
            TermEvent::PtyWrite(bytes) => self.push_pty(bytes),
            _ => self.events.push(event),
        }
    }

    fn push_bell(&mut self) {
        if self.bell {
            self.dropped += 1;
        } else {
            self.bell = true;
            self.events.push(TermEvent::Bell);
        }
    }

    fn push_title(&mut self, event: TermEvent) {
        if let Some(index) = self.title {
            self.events[index] = event;
        } else {
            self.title = Some(self.events.len());
            self.events.push(event);
        }
    }

    fn push_pty(&mut self, bytes: Vec<u8>) {
        if self.pty_bytes + bytes.len() <= (1 << 20) {
            self.pty_bytes += bytes.len();
            self.events.push(TermEvent::PtyWrite(bytes));
        } else {
            self.dropped += 1;
        }
    }
}

const HOST_CAP: usize = 1 << 20;

fn dispatch_osc(
    body: &[u8],
    kitty: &mut osc::notify::Kitty99,
    hostname: &str,
    caps: &mut EventCaps<'_>,
) {
    let (command, rest) = match body.iter().position(|&byte| byte == b';') {
        Some(index) => (&body[..index], &body[index + 1..]),
        None => (body, &body[body.len()..]),
    };
    match command {
        b"7" => {
            if let Some(report) = osc::cwd::parse(rest, hostname) {
                caps.push(TermEvent::Cwd(report));
            }
        }
        b"9" => {
            if let Some(event) = osc::notify::parse_osc9(rest) {
                caps.push(event);
            }
        }
        b"777" => {
            if let Some(event) = osc::notify::parse_osc777(rest) {
                caps.push(event);
            }
        }
        b"99" => match kitty.feed(rest) {
            K99Out::None => {}
            K99Out::Event(event) => caps.push(event),
            K99Out::Reply(bytes) => caps.push(TermEvent::PtyWrite(bytes)),
        },
        _ => {}
    }
}

impl Terminal {
    /// A blank terminal. Sizes below 1x1 are raised to 1x1.
    pub fn new(size: TermSize, config: &TermConfig) -> Self {
        let size = clamped(size);
        let core_config = alacritty_terminal::term::Config {
            scrolling_history: config.scrollback_lines.min(100_000),
            kitty_keyboard: true,
            osc52: Osc52::CopyPaste,
            ..Default::default()
        };
        let pending = Arc::new(Mutex::new(Vec::new()));
        let sink = events::EventSink::new(Arc::clone(&pending));
        let core = alacritty_terminal::Term::new(core_config, &dims(size), sink);
        Self {
            core,
            parser: Processor::default(),
            osc: osc::OscScanner::default(),
            utf8: Utf8Carry::default(),
            kitty: osc::notify::Kitty99::default(),
            size,
            palette: config.palette.clone(),
            bold_is_bright: config.bold_is_bright,
            hostname: config.hostname.clone(),
            last_cursor_row: None,
            last_display_offset: 0,
            force_full: false,
            generation: 0,
            stats: TermStats::default(),
            pending,
            drain_buf: Vec::new(),
        }
    }

    /// Parses PTY output and appends side effects to `events` (caller-owned, reused).
    /// Per call: ≤ 1 `Bell`, only the last `Title`/`ResetTitle`, ≤ 64 notifications,
    /// `PtyWrite` never dropped but ≤ 1 MiB total. Allocation-free unless an event fires.
    pub fn advance(&mut self, bytes: &[u8], events: &mut Vec<TermEvent>) {
        let mut caps = EventCaps::new(events);
        {
            let Terminal {
                core,
                parser,
                osc,
                utf8,
                kitty,
                hostname,
                ..
            } = self;
            osc.advance(bytes, &mut |scanned| match scanned {
                Scanned::Core(chunk) => utf8.feed(parser, core, chunk),
                Scanned::Osc(body) => dispatch_osc(body, kitty, hostname, &mut caps),
            });
            if let Some(deadline) = parser.sync_timeout().sync_timeout()
                && deadline <= Instant::now()
            {
                parser.stop_sync(core);
            }
        }
        self.drain_sink(&mut caps);
        self.stats.dropped_events += caps.dropped;
    }

    /// Core parse only, without the OSC side-channel. Bench baseline; not for app use.
    #[doc(hidden)]
    pub fn advance_core_only(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.core, bytes);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn drain_sink(&mut self, caps: &mut EventCaps<'_>) {
        let mut buffer = std::mem::take(&mut self.drain_buf);
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::swap(&mut *pending, &mut buffer);
        }
        for event in buffer.drain(..) {
            self.map_core_event(event, caps);
        }
        self.drain_buf = buffer;
    }

    fn map_core_event(&self, event: Event, caps: &mut EventCaps<'_>) {
        match event {
            Event::Title(title) => {
                caps.push(TermEvent::Title(osc::sanitize::sanitize(&title, 1024)));
            }
            Event::ResetTitle => caps.push(TermEvent::ResetTitle),
            Event::Bell => caps.push(TermEvent::Bell),
            Event::PtyWrite(text) => caps.push(TermEvent::PtyWrite(text.into_bytes())),
            Event::ClipboardStore(ty, text) => {
                if text.len() <= HOST_CAP {
                    caps.push(TermEvent::ClipboardStore {
                        target: clipboard_target(ty),
                        data: text.into_bytes(),
                    });
                } else {
                    caps.dropped += 1;
                }
            }
            Event::ClipboardLoad(ty, _) => {
                caps.push(TermEvent::ClipboardLoad {
                    target: clipboard_target(ty),
                });
            }
            Event::ColorRequest(index, format) => {
                let rgb = color::palette_rgb(self.core.colors(), &self.palette, index);
                caps.push(TermEvent::PtyWrite(format(rgb).into_bytes()));
            }
            Event::TextAreaSizeRequest(format) => {
                let window = WindowSize {
                    num_lines: self.size.rows,
                    num_cols: self.size.cols,
                    cell_width: self.size.cell_w_px,
                    cell_height: self.size.cell_h_px,
                };
                caps.push(TermEvent::PtyWrite(format(window).into_bytes()));
            }
            Event::Wakeup
            | Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }

    /// Resizes the grid (reflowing); the next `fill_frame` marks everything dirty.
    pub fn resize(&mut self, size: TermSize) {
        let size = clamped(size);
        self.core.resize(dims(size));
        self.size = size;
        self.force_full = true;
    }

    /// Copies damaged rows into `frame` and resets internal damage. Never clears
    /// `frame.dirty_rows`. Allocates only when the size changed or a row has zero-width marks.
    pub fn fill_frame(&mut self, frame: &mut Frame) {
        fill::fill_frame(self, frame);
    }

    /// Scrolls the view into scrollback: positive = up (older lines), clamped.
    pub fn scroll_display(&mut self, delta_lines: i32) {
        self.core.scroll_display(Scroll::Delta(delta_lines));
    }

    /// Current input-relevant modes.
    pub fn modes(&self) -> TermModes {
        let mode = self.core.mode();
        let mouse = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseMode::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseMode::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::Click
        } else {
            MouseMode::None
        };
        let mut kitty_keyboard = 0u8;
        for (bit, value) in [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ] {
            if mode.contains(bit) {
                kitty_keyboard |= value;
            }
        }
        TermModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            mouse,
            mouse_sgr: mode.contains(TermMode::SGR_MOUSE),
            mouse_utf8: mode.contains(TermMode::UTF8_MOUSE),
            focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            show_cursor: mode.contains(TermMode::SHOW_CURSOR),
            line_feed_new_line: mode.contains(TermMode::LINE_FEED_NEW_LINE),
            kitty_keyboard,
        }
    }

    /// Deadline of a pending synchronized update (DEC 2026); the reader must call
    /// [`Terminal::end_expired_sync`] by then even without input.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Ends a synchronized update whose deadline is `<= now`, parsing the held bytes
    /// (side effects go to `events`, same caps as `advance`). Returns true if it ended.
    pub fn end_expired_sync(&mut self, now: Instant, events: &mut Vec<TermEvent>) -> bool {
        match self.sync_deadline() {
            Some(deadline) if deadline <= now => {}
            _ => return false,
        }
        self.parser.stop_sync(&mut self.core);
        let mut caps = EventCaps::new(events);
        self.drain_sink(&mut caps);
        self.stats.dropped_events += caps.dropped;
        true
    }

    /// Counters.
    pub fn stats(&self) -> TermStats {
        self.stats
    }
}
