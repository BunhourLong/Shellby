//! Synthetic grid demo: colors, box drawing, blocks, wide chars, decorations and a cursor.

#![expect(clippy::print_stderr, reason = "example")]

use std::sync::Arc;

use nt_render::{CellSize, FontConfig, PaneDraw, PxRect, Renderer, Scene, UiLayer};
use nt_term::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::Key;
use winit::window::{Window, WindowId};

const COLORS: [Rgba; 8] = [
    Rgba([255, 255, 255, 255]),
    Rgba([255, 120, 120, 255]),
    Rgba([120, 255, 120, 255]),
    Rgba([255, 220, 100, 255]),
    Rgba([120, 160, 255, 255]),
    Rgba([255, 120, 255, 255]),
    Rgba([100, 230, 230, 255]),
    Rgba([200, 200, 200, 255]),
];

const BOX: &str = "┌─┬─┐│├┼┤└┴┘╭╮╰╯═║▀▄█▌▐░▒▓";
const WIDE: &str = "中文";
const ASCII: &str = "The quick brown fox jumps over the lazy dog 0123456789";
const SHAPES: [CursorShape; 5] = [
    CursorShape::Block,
    CursorShape::Underline,
    CursorShape::Beam,
    CursorShape::HollowBlock,
    CursorShape::Hidden,
];

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    frame: Frame,
    ui: UiLayer,
    size: (u32, u32),
    font: FontConfig,
    shape: usize,
    focused: bool,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            renderer: None,
            frame: Frame::default(),
            ui: UiLayer::default(),
            size: (0, 0),
            font: FontConfig {
                family: None,
                size_pt: 12.0,
                line_height: 1.0,
            },
            shape: 0,
            focused: true,
        }
    }

    fn cell_size(&self) -> CellSize {
        self.renderer
            .as_ref()
            .map_or(CellSize { w: 1, h: 1 }, Renderer::cell_size)
    }

    fn rebuild(&mut self) {
        let cell = self.cell_size();
        let cols = self.size.0 / u32::from(cell.w.max(1));
        let rows = self.size.1 / u32::from(cell.h.max(1));
        let cols = cols.clamp(1, u32::from(u16::MAX)) as u16;
        let rows = rows.clamp(1, u32::from(u16::MAX)) as u16;

        let bg = Rgba([16, 18, 24, 255]);
        let mut cells = vec![
            Cell {
                ch: ' ',
                fg: COLORS[0],
                bg,
                underline: COLORS[0],
                flags: CellFlags(0),
                extra: 0,
            };
            usize::from(cols) * usize::from(rows)
        ];

        for (r, row) in cells.chunks_mut(usize::from(cols)).enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                let chars: Vec<char> = ASCII.chars().collect();
                cell.ch = chars[(c + r) % chars.len()];
                cell.fg = COLORS[(c + r) % COLORS.len()];
            }
        }

        if rows > 1 {
            let row = &mut cells[usize::from(cols)..usize::from(cols) * 2];
            for (c, cell) in row.iter_mut().enumerate() {
                cell.ch = ' ';
                cell.bg = COLORS[c % COLORS.len()];
                cell.fg = bg;
            }
        }

        if rows > 2 {
            let start = usize::from(cols) * 2;
            let row = &mut cells[start..start + usize::from(cols)];
            let chars: Vec<char> = BOX.chars().collect();
            for (c, cell) in row.iter_mut().enumerate() {
                cell.ch = chars[c % chars.len()];
                cell.fg = COLORS[(c + 3) % COLORS.len()];
            }
        }

        if rows > 3 {
            let start = usize::from(cols) * 3;
            let row = &mut cells[start..start + usize::from(cols)];
            let chars: Vec<char> = WIDE.chars().collect();
            for (index, cell) in row.iter_mut().enumerate() {
                let c = index / 2;
                if index % 2 == 0 {
                    cell.ch = chars[c % chars.len()];
                    cell.fg = COLORS[1];
                    cell.flags = CellFlags(CellFlags::WIDE);
                } else {
                    cell.ch = ' ';
                    cell.flags = CellFlags(CellFlags::WIDE_SPACER);
                }
            }
        }

        if rows > 4 {
            let start = usize::from(cols) * 4;
            let row = &mut cells[start..start + usize::from(cols)];
            let flags = [
                CellFlags::UNDERLINE,
                CellFlags::DOUBLE_UNDERLINE,
                CellFlags::UNDERCURL,
                CellFlags::STRIKEOUT,
                CellFlags::DOTTED_UNDERLINE,
                CellFlags::DASHED_UNDERLINE,
            ];
            for (c, cell) in row.iter_mut().enumerate() {
                cell.ch = 'u';
                cell.fg = COLORS[0];
                cell.underline = COLORS[c % COLORS.len()];
                cell.flags = CellFlags(flags[c % flags.len()]);
            }
        }

        let cursor = Cursor {
            col: 2.min(cols.saturating_sub(1)),
            row: 2.min(rows.saturating_sub(1)),
            shape: SHAPES[self.shape],
            color: COLORS[0],
            blinking: false,
        };

        self.frame = Frame {
            cols,
            rows,
            cells,
            dirty_rows: vec![true; usize::from(rows)],
            zerowidth: vec![Vec::new(); usize::from(rows)],
            cursor,
            default_bg: bg,
            generation: 1,
        };
    }

    fn redraw(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let viewport = PxRect {
            x: 0,
            y: 0,
            w: self.size.0,
            h: self.size.1,
        };
        let mut panes = [PaneDraw {
            id: 1,
            frame: &mut self.frame,
            viewport,
            focused: self.focused,
            cursor_visible: true,
        }];
        let mut scene = Scene {
            panes: &mut panes,
            ui: &self.ui,
            clear: Rgba([12, 14, 18, 255]),
        };
        if let Err(error) = renderer.render(&mut scene) {
            eprintln!("render error: {error}");
        }
    }

    fn key(&mut self, key: &Key) {
        let Key::Character(text) = key else {
            return;
        };
        match text.as_str() {
            "c" => {
                self.shape = (self.shape + 1) % SHAPES.len();
                self.frame.cursor.shape = SHAPES[self.shape];
            }
            "+" | "=" => {
                self.font.size_pt = (self.font.size_pt + 1.0).min(200.0);
                if let Some(renderer) = &mut self.renderer
                    && let Err(error) = renderer.set_font(&self.font)
                {
                    eprintln!("font error: {error}");
                    return;
                }
                self.rebuild();
            }
            "-" => {
                self.font.size_pt = (self.font.size_pt - 1.0).max(4.0);
                if let Some(renderer) = &mut self.renderer
                    && let Err(error) = renderer.set_font(&self.font)
                {
                    eprintln!("font error: {error}");
                    return;
                }
                self.rebuild();
            }
            "f" => self.focused = !self.focused,
            _ => return,
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("nt-render grid")
            .with_inner_size(PhysicalSize::new(1000, 700));
        let window = Arc::new(event_loop.create_window(attributes).expect("create window"));
        let size = window.inner_size();
        let scale = window.scale_factor();
        let renderer = Renderer::new(window.clone(), (size.width, size.height), scale, &self.font)
            .expect("create renderer");
        self.size = (size.width, size.height);
        self.renderer = Some(renderer);
        self.window = Some(window.clone());
        self.rebuild();
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.size = (size.width, size.height);
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(self.size);
                }
                self.rebuild();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_scale_factor(scale_factor);
                }
                if let Some(window) = self.window.clone() {
                    let size = window.inner_size();
                    self.size = (size.width, size.height);
                    if let Some(renderer) = &mut self.renderer {
                        renderer.resize(self.size);
                    }
                    self.rebuild();
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed && !event.repeat {
                    self.key(&event.logical_key);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("run event loop");
}
