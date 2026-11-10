//! The winit application: one window, one pane, damage-driven redraw (T08).

use std::sync::Arc;
use std::time::Instant;

use nt_pty::SpawnSpec;
use nt_render::{FontConfig, GpuStart, PaneDraw, PxRect, RenderOutcome, Renderer, Scene, UiLayer};
use nt_term::{Rgba, TermConfig, TermEvent};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{UserAttentionType, Window, WindowId};

use crate::geometry::{GridSize, Padding, grid_from_pixels};
use crate::keys;
use crate::pane::{PaneId, PaneRuntime, Sink};

/// Events sent to the UI thread from pane threads (00-architecture §3.6).
#[derive(Debug)]
pub(crate) enum UserEvent {
    /// The pane's terminal changed; redraw. At most one pending per pane.
    Wake(#[expect(dead_code, reason = "one pane in T08; T15 redraws per pane")] PaneId),
    /// A parser side effect other than `PtyWrite` (handled on the reader thread).
    Term(PaneId, TermEvent),
    /// The pane's reader saw EOF; sent exactly once.
    PaneExited(PaneId),
}

const TITLE: &str = "newTerm";
const PAD_LOGICAL: f64 = 4.0;
/// Grid used to spawn the shell before the font (and so the cell size) is known.
const SPAWN_GRID: GridSize = GridSize { cols: 80, rows: 24 };
const ERROR_FG: Rgba = Rgba([0xff, 0x6b, 0x6b, 0xff]);
const ERROR_BG: Rgba = Rgba([0x18, 0x18, 0x18, 0xff]);

/// Application state; `run` is `None` until the first `resumed`.
#[derive(Debug)]
pub(crate) struct App {
    t0: Instant,
    exit_after_first_frame: bool,
    proxy: EventLoopProxy<UserEvent>,
    /// Taken by the first `resumed`.
    gpu: Option<GpuStart>,
    run: Option<Running>,
    /// Process exit code once the loop ends.
    pub(crate) exit_code: i32,
}

#[derive(Debug)]
struct Running {
    window: Arc<Window>,
    renderer: Renderer,
    /// `Err` = spawn failed; the message is drawn in the window.
    pane: Result<PaneRuntime, String>,
    ui: UiLayer,
    pad: Padding,
    focused: bool,
    mods: ModifiersState,
    keybuf: Vec<u8>,
    shown: bool,
}

impl App {
    /// `t0` is the process start, for `first_frame_ms`; `gpu` was started at that moment.
    pub(crate) fn new(t0: Instant, gpu: GpuStart, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            t0,
            exit_after_first_frame: std::env::var_os("NT_EXIT_AFTER_FIRST_FRAME")
                .is_some_and(|v| v == "1"),
            proxy,
            gpu: Some(gpu),
            run: None,
            exit_code: 0,
        }
    }

    fn start(&mut self, el: &ActiveEventLoop) -> Result<Running, String> {
        let attrs = Window::default_attributes()
            .with_title(TITLE)
            .with_inner_size(LogicalSize::new(960.0, 600.0))
            // Mapped after the first present (X11; Wayland maps on first buffer anyway).
            .with_visible(false);
        let window = Arc::new(
            el.create_window(attrs)
                .map_err(|e| format!("window: {e}"))?,
        );

        // The GPU device is already being created (main); spawn now so the shell's own
        // startup also overlaps it.
        let pane = spawn_pane(PaneId(1), &self.proxy);

        let scale = window.scale_factor();
        let size = window.inner_size();
        let font = FontConfig {
            family: None,
            size_pt: 12.0,
            line_height: 1.0,
        };
        let gpu = self.gpu.take().ok_or("GPU init already consumed")?;
        let renderer =
            Renderer::with_gpu(gpu, window.clone(), (size.width, size.height), scale, &font)
                .map_err(|e| format!("renderer: {e}"))?;
        let mut run = Running {
            window,
            renderer,
            pane,
            ui: UiLayer::default(),
            pad: padding(scale),
            focused: true,
            mods: ModifiersState::empty(),
            keybuf: Vec::with_capacity(16),
            shown: false,
        };
        run.relayout(size);
        Ok(run)
    }

    fn exit(&mut self, el: &ActiveEventLoop) {
        if let Some(Ok(pane)) = self.run.as_mut().map(|r| &mut r.pane) {
            let status = pane.shutdown();
            log::info!("pane {} exited: {status:?}", pane.id.0);
        }
        el.exit();
    }

    fn redraw(&mut self, el: &ActiveEventLoop) {
        let Some(run) = &mut self.run else { return };
        match run.render() {
            Ok(RenderOutcome::Presented) if !run.shown => {
                run.shown = true;
                run.window.set_visible(true);
                if self.exit_after_first_frame {
                    let ms = self.t0.elapsed().as_secs_f64() * 1e3;
                    #[expect(clippy::print_stderr, reason = "benchmark output contract (T08/T11)")]
                    {
                        eprintln!("first_frame_ms={ms:.2}");
                    }
                    self.exit(el);
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::error!("render failed: {e}");
                self.exit_code = 1;
                self.exit(el);
            }
        }
    }
}

impl Running {
    /// Recomputes the grid for a window of `size` px; resizes the PTY only if it changed.
    fn relayout(&mut self, size: PhysicalSize<u32>) {
        self.renderer.resize((size.width, size.height));
        let cell = self.renderer.cell_size();
        let grid = grid_from_pixels(
            size.width,
            size.height,
            cell.w.into(),
            cell.h.into(),
            self.pad,
        );
        match &mut self.pane {
            Ok(pane) => pane.resize(grid, (cell.w, cell.h)),
            Err(msg) => {
                self.ui.clear();
                let full = PxRect {
                    x: 0,
                    y: 0,
                    w: size.width,
                    h: size.height,
                };
                self.ui
                    .text(self.pad.x, self.pad.y, msg, ERROR_FG, false, full);
            }
        }
        self.window.request_redraw();
    }

    fn render(&mut self) -> Result<RenderOutcome, nt_render::RenderError> {
        let cell = self.renderer.cell_size();
        let (pad, focused) = (self.pad, self.focused);
        let Ok(pane) = &mut self.pane else {
            let mut scene = Scene {
                panes: &mut [],
                ui: &self.ui,
                clear: ERROR_BG,
            };
            return self.renderer.render(&mut scene);
        };
        pane.fill_frame();
        let viewport = PxRect {
            x: pad.x,
            y: pad.y,
            w: u32::from(pane.grid.cols) * u32::from(cell.w),
            h: u32::from(pane.grid.rows) * u32::from(cell.h),
        };
        let clear = pane.frame.default_bg;
        let mut panes = [PaneDraw {
            id: pane.id.0,
            frame: &mut pane.frame,
            viewport,
            focused,
            cursor_visible: true,
        }];
        let mut scene = Scene {
            panes: &mut panes,
            ui: &self.ui,
            clear,
        };
        self.renderer.render(&mut scene)
    }
}

fn spawn_pane(id: PaneId, proxy: &EventLoopProxy<UserEvent>) -> Result<PaneRuntime, String> {
    let mut spec = SpawnSpec::default_shell();
    spec.env
        .push(("NEWTERM_PANE_ID".into(), id.0.to_string().into()));
    let config = TermConfig {
        hostname: rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned(),
        ..TermConfig::default()
    };
    let proxy = proxy.clone();
    let sink: Sink = Box::new(move |ev| proxy.send_event(ev).is_ok());
    PaneRuntime::spawn(id, &spec, SPAWN_GRID, (0, 0), &config, sink).map_err(|e| {
        log::error!("spawn {} failed: {e}", spec.program.display());
        format!("newTerm: could not start {}: {e}", spec.program.display())
    })
}

#[expect(clippy::cast_sign_loss, reason = "winit scale factors are positive")]
fn padding(scale: f64) -> Padding {
    let p = (PAD_LOGICAL * scale).round() as u32;
    Padding { x: p, y: p }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.run.is_some() {
            return;
        }
        el.set_control_flow(ControlFlow::Wait);
        match self.start(el) {
            Ok(run) => self.run = Some(run),
            Err(e) => {
                log::error!("startup failed: {e}");
                self.exit_code = 1;
                el.exit();
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        let Some(run) = &mut self.run else { return };
        match event {
            UserEvent::Wake(_) => run.window.request_redraw(),
            UserEvent::Term(_, TermEvent::Title(t)) => run.window.set_title(&t),
            UserEvent::Term(_, TermEvent::ResetTitle) => run.window.set_title(TITLE),
            UserEvent::Term(_, TermEvent::Bell) => {
                if !run.focused {
                    run.window
                        .request_user_attention(Some(UserAttentionType::Informational));
                }
            }
            // Cwd (T16), Notify (T18), Clipboard (T10): not handled yet.
            UserEvent::Term(id, ev) => log::debug!("pane {}: unhandled {ev:?}", id.0),
            // v1: the last pane closing closes the window (configurable in T14).
            UserEvent::PaneExited(id) => {
                log::info!("pane {} output ended", id.0);
                self.exit(el);
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(run) = &mut self.run else { return };
        match event {
            WindowEvent::RedrawRequested => self.redraw(el),
            WindowEvent::Resized(size) => run.relayout(size),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                run.renderer.set_scale_factor(scale_factor);
                run.pad = padding(scale_factor);
                run.relayout(run.window.inner_size());
            }
            WindowEvent::Focused(focused) => {
                run.focused = focused;
                run.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => run.mods = m.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state == ElementState::Pressed => {
                if let Ok(pane) = &run.pane {
                    run.keybuf.clear();
                    let app_cursor = pane.modes().app_cursor;
                    keys::encode_basic(&event, run.mods, app_cursor, &mut run.keybuf);
                    if !run.keybuf.is_empty() {
                        pane.write(&run.keybuf);
                    }
                }
            }
            WindowEvent::CloseRequested => self.exit(el),
            _ => {}
        }
    }
}
