# S1: alacritty_terminal 0.26.0 / vte 0.15.0 API facts (verified by Opus). Paths: alac = spikes/vendor/alacritty_terminal-0.26.0, vte = spikes/vendor/vte-0.15.0

1. `src/term/mod.rs:410` `pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T>`; resize `src/term/mod.rs:655` `pub fn resize<S: Dimensions>(&mut self, size: S)`.
   vte `src/ansi.rs:271` `pub struct Processor<T: Timeout = StdSyncHandler>`; `src/ansi.rs:287` `pub fn new() -> Self`; `src/ansi.rs:298` `pub fn advance<H>(&mut self, handler: &mut H, bytes: &[u8]) where H: Handler`.
   Sync handler is generic `T: Timeout` (`src/ansi.rs:478`, default `StdSyncHandler` at `src/ansi.rs:443`); `Term<T: EventListener>` implements `Handler` (`src/term/mod.rs:1059`).

2. `src/term/mod.rs:458` `pub fn damage(&mut self) -> TermDamage<'_>`; `:489` `pub fn reset_damage(&mut self)`; enum `TermDamage<'a> { Full, Partial(TermDamageIterator<'a>) }` `src/term/mod.rs:178`; `LineDamageBounds` `src/term/mod.rs:137`.
   Full damage starts `true` (`src/term/mod.rs:233`) and is forced by: `resize` (:704), `set_options` (:525), `scroll_display` when offset changes (:406), `swap_alt` (:734), `scroll_up/down_relative` (:762,:789), `deccolm` (:802), `decaln` (:1152), `set_color`/`reset_color` non-cursor (:1667,:1697), `clear_screen` (:1817), `reset_state` (:1857), Insert mode entry/unset (:462,:2128).

3. `Cell` `src/term/cell.rs:134` fields `c: char, fg: Color, bg: Color, flags: Flags, extra: Option<Arc<CellExtra>>`; `CellExtra` `:125` fields (private) `zerowidth: Vec<char>, underline_color: Option<Color>, hyperlink: Option<Hyperlink>`.
   `Flags` `src/term/cell.rs:15`: INVERSE, BOLD, ITALIC, BOLD_ITALIC, UNDERLINE, WRAPLINE, WIDE_CHAR, WIDE_CHAR_SPACER, DIM, DIM_BOLD, HIDDEN, STRIKEOUT, LEADING_WIDE_CHAR_SPACER, DOUBLE_UNDERLINE, UNDERCURL, DOTTED_UNDERLINE, DASHED_UNDERLINE, ALL_UNDERLINES.
   `Color` `vte src/ansi.rs:1128` `{ Named(NamedColor), Spec(Rgb), Indexed(u8) }`; `Term::colors()` `src/term/mod.rs:952` returns `&Colors` (`[Option<Rgb>;269]`, `src/term/color.rs:22`, indexed by usize/NamedColor; `None`=fallback, set via `set_color`/`reset_color`).

4. `RenderableContent` `src/term/mod.rs:2393` fields `display_iter: GridIterator<'a, Cell>, selection: Option<SelectionRange>, cursor: RenderableCursor, display_offset: usize, colors: &'a Colors, mode: TermMode`; getter `src/term/mod.rs:637`.
   `RenderableCursor` `:2365` `{ shape: CursorShape, point: Point }`; visibility/shape from `TermMode::SHOW_CURSOR` + `cursor_style()` (`:2380-2384`, `:942`); grid offset via `grid().display_offset()`.

5. `src/event.rs:103` `pub trait EventListener { fn send_event(&self, _event: Event) {} }`. Variants `src/event.rs:14`: `MouseCursorDirty, Title(String), ResetTitle, ClipboardStore(ClipboardType,String), ClipboardLoad(ClipboardType,Arc<dyn Fn(&str)->String+Sync+Send>), ColorRequest(usize,Arc<dyn Fn(Rgb)->String+Sync+Send>), PtyWrite(String), TextAreaSizeRequest(Arc<dyn Fn(WindowSize)->String+Sync+Send>), CursorBlinkingChange, Wakeup, Bell, Exit, ChildExit(ExitStatus)`.

6. `TermMode` `src/term/mod.rs:55`: NONE, SHOW_CURSOR, APP_CURSOR, APP_KEYPAD, MOUSE_REPORT_CLICK, BRACKETED_PASTE, SGR_MOUSE, MOUSE_MOTION, LINE_WRAP, LINE_FEED_NEW_LINE, ORIGIN, INSERT, FOCUS_IN_OUT, ALT_SCREEN, MOUSE_DRAG, UTF8_MOUSE, ALTERNATE_SCROLL, VI, URGENCY_HINTS, plus kitty flags DISAMBIGUATE_ESC_CODES, REPORT_EVENT_TYPES, REPORT_ALTERNATE_KEYS, REPORT_ALL_KEYS_AS_ESC, REPORT_ASSOCIATED_TEXT, KITTY_KEYBOARD_PROTOCOL, MOUSE_MODE, ANY.
   Kitty present (`:75-85`), gated by `Config::kitty_keyboard` (`:350`); set by Handler CSI-u methods `report_keyboard_mode`/`push_keyboard_mode`/`pop_keyboard_modes`/`set_keyboard_mode` (`src/term/mod.rs:1275,1288,1308,1323`) → TermMode at `:1029`; dispatch in `vte src/ansi.rs:1746-1763`.

7. Buffered in vte `ProcessorState.sync_state.buffer` (`vte src/ansi.rs:248-258`), 2 MiB cap `SYNC_BUFFER_SIZE` (`:39`); timeout `SYNC_UPDATE_TIMEOUT = Duration::from_millis(150)` (`:36`).
   On expiry caller must call `Processor::stop_sync(&mut term)` (`vte src/ansi.rs:315`), detecting expiry via `sync_timeout()` (`:292`) / `StdSyncHandler::sync_timeout() -> Option<Instant>` (`:451`); reference usage `src/event_loop.rs:229-248`.

8. `vte src/ansi.rs:1329` OSC handled: 0, 2, 4, 8, 10, 11, 12, 22, 50, 52, 104, 110, 111, 112. 7, 9, 99, 777 are **absent** (fall through to `_ => unhandled` at `:1523`). Caps: `MAX_OSC_PARAMS = 16`, `MAX_OSC_RAW = 1024` (`vte src/lib.rs:45-46`), `MAX_PARAMS = 32` (`vte src/params.rs:5`).

9. Ground state only scans for ESC (`vte src/lib.rs:597-600`); 0x9B/0x9C/0x9D are invalid standalone UTF-8 ≤0x9F so go to `performer.execute(byte)` (`src/lib.rs:633-634`). `execute` handles only C0 HT/BS/CR/LF/VT/FF/BEL/SUB/SI/SO (`vte src/ansi.rs:1296-1307`), so these C1 bytes are not treated as CSI/ST/OSC introducers and are dropped.

10. Features `Cargo.toml:34-40`: `default = ["serde"]`, only `serde`; no `tty`, `event_loop` or similar features exist. `pub mod event_loop` and `pub mod tty` are unconditional (`src/lib.rs:8,15`), so they cannot be disabled via features.

11. `SelectionType` `src/selection.rs:93` `{ Simple, Block, Semantic, Lines }`; `Selection::new` `:125`, `to_range` `:271`; `selection_to_string` `src/term/mod.rs:529` `pub fn selection_to_string(&self) -> Option<String>`.
   Search uses `regex-automata` hybrid DFA (`src/term/search.rs:7-11`; dep `Cargo.toml:71`); `RegexSearch::new` `:34`, `search_next` `:121`.

12. Standalone: yes, `tty` independent of `event_loop`. `src/tty/unix.rs:195` `pub fn new(config: &Options, window_size: WindowSize, window_id: u64) -> Result<Pty>`; `:202` `pub fn from_fd(config: &Options, window_id: u64, master: OwnedFd, slave: OwnedFd) -> Result<Pty>`.
   Access `:110` `pub fn child(&self) -> &Child`, `:114` `pub fn file(&self) -> &File`; read/write via `EventedReadWrite::reader/writer -> &mut File` (`:372,377`); resize `OnResize::on_resize(&mut self, window_size: WindowSize)` (`:411`). Note it forces nonblocking (`:293`).

CORRECTION to 8: alacritty enables vte feature "std", under which Parser.osc_raw is an unbounded Vec (vte src/lib.rs:63-64, push at :551). MAX_OSC_RAW=1024 applies only without std. MAX_OSC_PARAMS=16 still applies.
Use vte only via the re-export alacritty_terminal::vte (no direct vte dep).
