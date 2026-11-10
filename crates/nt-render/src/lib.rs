//! wgpu renderer: draws `nt_term::Frame`s into pixel viewports plus a minimal UI layer.
//!
//! Damage model (00-architecture §2): every presented frame redraws everything from GPU
//! buffers; only dirty rows are re-encoded and uploaded, and nothing is presented when
//! nothing changed.

mod atlas;
mod builtin;
mod encode;
mod font;
mod glyphs;
mod gpu;

use std::collections::HashMap;

use nt_term::{Frame, Rgba};

use crate::atlas::Page;
use crate::encode::{Encoded, GridLayer, Inst, encode_ui};
use crate::font::Font;
use crate::glyphs::GlyphCache;

/// Font selection. `family: None` = the platform's default monospace font.
#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    /// Family name as installed (e.g. `JetBrains Mono`); `None` = default monospace.
    pub family: Option<String>,
    /// Size in points; px = pt × dpi / 72 (dpi = 96 × scale on Linux, 72 × scale on macOS).
    pub size_pt: f32,
    /// Line height multiplier; 1.0 = the font's natural height.
    pub line_height: f32,
}

/// Cell size in integer device pixels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CellSize {
    /// Width.
    pub w: u16,
    /// Height.
    pub h: u16,
}

/// Rectangle in device pixels, window origin top-left.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct PxRect {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// One pane to draw this frame.
#[derive(Debug)]
pub struct PaneDraw<'f> {
    /// Stable per pane (nt-app passes `PaneId.0`); keys the pane's GPU buffers.
    pub id: u64,
    /// The pane's frame; `render` calls `clear_dirty` on it after a successful submit.
    pub frame: &'f mut Frame,
    /// Where the grid goes; content is clipped to it.
    pub viewport: PxRect,
    /// Unfocused panes draw a hollow cursor.
    pub focused: bool,
    /// Blink phase, decided by the app.
    pub cursor_visible: bool,
}

/// Everything to draw in one frame.
#[derive(Debug)]
pub struct Scene<'s, 'f> {
    /// Panes, drawn in order.
    pub panes: &'s mut [PaneDraw<'f>],
    /// UI drawn on top of the panes, unclipped.
    pub ui: &'s UiLayer,
    /// Color of pixels no pane or UI rect covers.
    pub clear: Rgba,
}

/// Solid rectangle in the UI layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UiRect {
    /// Where.
    pub rect: PxRect,
    /// Color (straight alpha, sRGB bytes).
    pub color: Rgba,
}

/// A monospace text run in the UI layer; its text is `UiLayer::text[start..end]`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct UiRun {
    x: u32,
    y: u32,
    start: u32,
    end: u32,
    color: Rgba,
    bold: bool,
    clip: PxRect,
}

/// Retained UI drawing list (rects + text runs at the cell font). Reuse it across frames:
/// `clear` keeps capacity, so steady-state rebuilds do not allocate.
#[derive(Debug, Default)]
pub struct UiLayer {
    rects: Vec<UiRect>,
    runs: Vec<UiRun>,
    text: String,
    generation: u64,
}

impl UiLayer {
    /// Removes everything, keeping capacity; bumps the generation.
    pub fn clear(&mut self) {
        self.rects.clear();
        self.runs.clear();
        self.text.clear();
        self.generation += 1;
    }

    /// Adds a solid rect drawn after earlier rects.
    pub fn rect(&mut self, rect: PxRect, color: Rgba) {
        self.rects.push(UiRect { rect, color });
        self.generation += 1;
    }

    /// Adds a monospace run at the cell font with its top-left at `(x, y)`, clipped to
    /// `clip`; the caller truncates or ellipsises.
    pub fn text(&mut self, x: u32, y: u32, text: &str, color: Rgba, bold: bool, clip: PxRect) {
        let start = self.text.len() as u32;
        self.text.push_str(text);
        let end = self.text.len() as u32;
        self.runs.push(UiRun {
            x,
            y,
            start,
            end,
            color,
            bold,
            clip,
        });
        self.generation += 1;
    }
}

/// Result of a successful `render` call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RenderOutcome {
    /// A frame was submitted and presented.
    Presented,
    /// Nothing changed, or the surface timed out; nothing was presented.
    Skipped,
    /// Zero-size or lost surface; render again after the next resize.
    SurfaceUnavailable,
}

/// Fatal renderer errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// No compatible GPU adapter.
    NoAdapter,
    /// Device creation or rebuild failed.
    Device(String),
    /// No usable font could be loaded.
    Font(String),
    /// The GPU ran out of memory.
    OutOfMemory,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter => f.write_str("no compatible GPU adapter"),
            Self::Device(e) => write!(f, "GPU device error: {e}"),
            Self::Font(e) => write!(f, "font error: {e}"),
            Self::OutOfMemory => f.write_str("GPU out of memory"),
        }
    }
}

impl std::error::Error for RenderError {}

/// The renderer: GPU state, glyph cache and per-pane layers.
#[derive(Debug)]
pub struct Renderer {
    gpu: gpu::Gpu,
    glyphs: GlyphCache,
    panes: HashMap<u64, Pane>,
    encs: Vec<(u64, Encoded)>,
    ui_inst: Vec<Inst>,
    ui_gen: Option<u64>,
    ui_buf: Option<wgpu::Buffer>,
    ui_cap: usize,
    font: FontConfig,
    scale: f64,
    last: Vec<(u64, PxRect)>,
    last_clear: Rgba,
    draw_scratch: Vec<(u64, usize, PxRect)>,
}

/// One pane's retained layer and the GPU buffer mirroring its instances.
struct Pane {
    layer: GridLayer,
    buf: Option<wgpu::Buffer>,
    buf_len: usize,
}

impl std::fmt::Debug for Pane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pane")
            .field("layer", &self.layer)
            .field("buf_len", &self.buf_len)
            .field("buf", &self.buf.is_some())
            .finish()
    }
}

impl Pane {
    fn new() -> Self {
        Self {
            layer: GridLayer::new(),
            buf: None,
            buf_len: 0,
        }
    }
}

impl Renderer {
    /// Creates the GPU device and surface for `target` (nt-app passes `Arc<winit::Window>`)
    /// and loads the primary font.
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        size_px: (u32, u32),
        scale: f64,
        font: &FontConfig,
    ) -> Result<Self, RenderError> {
        let gpu = gpu::Gpu::new_surface(target, size_px)?;
        let max_dim = gpu.max_texture_dimension();
        let font_object = Font::load(font, scale).map_err(RenderError::Font)?;
        let glyphs = GlyphCache::new(font_object, max_dim);
        Ok(Self::from_parts(gpu, glyphs, font.clone(), scale))
    }

    /// Current cell size.
    pub fn cell_size(&self) -> CellSize {
        metrics_cell_size(self.glyphs.metrics())
    }

    /// New surface size; 0 in either dimension suspends rendering until non-zero.
    pub fn resize(&mut self, size_px: (u32, u32)) {
        self.gpu.resize(size_px);
        self.last.clear();
    }

    /// New display scale: recomputes metrics, rebuilds the atlas, returns the new cell size.
    pub fn set_scale_factor(&mut self, scale: f64) -> CellSize {
        self.scale = scale;
        self.glyphs.font_mut().rescale(&self.font, scale);
        self.glyphs.reset();
        self.ui_gen = None;
        self.last.clear();
        self.cell_size()
    }

    /// New font: recomputes metrics, rebuilds the atlas, returns the new cell size.
    pub fn set_font(&mut self, font: &FontConfig) -> Result<CellSize, RenderError> {
        let font_object = Font::load(font, self.scale).map_err(RenderError::Font)?;
        self.font = font.clone();
        self.glyphs.set_font(font_object);
        self.ui_gen = None;
        self.last.clear();
        Ok(self.cell_size())
    }

    /// Encodes and uploads damage, then presents, or returns `Skipped` without touching
    /// the surface when nothing changed. Clears each frame's dirty rows only after submit.
    pub fn render(&mut self, scene: &mut Scene<'_, '_>) -> Result<RenderOutcome, RenderError> {
        if self.gpu.take_device_lost() {
            self.rebuild_after_loss()?;
        }
        if self.gpu.suspended() || !self.gpu.has_surface() {
            return Ok(RenderOutcome::SurfaceUnavailable);
        }

        let size = self.gpu.target_size();
        let ui_dirty = self.encode_scene(scene);
        if self.should_skip(scene, ui_dirty) {
            return Ok(RenderOutcome::Skipped);
        }
        self.flush_atlas();
        self.upload_all(scene, size, ui_dirty);

        let (texture, suboptimal) = match self.gpu.acquire() {
            gpu::Acquired::Texture(texture, suboptimal) => (texture, suboptimal),
            // Uploads already happened and layer state advanced, so the next call must not
            // be skipped even if nothing new changes: forget the last presented scene.
            gpu::Acquired::Skip => {
                self.last.clear();
                return Ok(RenderOutcome::Skipped);
            }
            gpu::Acquired::Unavailable => {
                self.last.clear();
                return Ok(RenderOutcome::SurfaceUnavailable);
            }
            gpu::Acquired::Validation => {
                self.last.clear();
                return Err(RenderError::Device(
                    "surface acquire failed validation".into(),
                ));
            }
        };
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.gpu.render_format()),
            ..wgpu::TextureViewDescriptor::default()
        });
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nt-render"),
            });
        let ui = self.ui_buf.as_ref().map(|buf| (buf, self.ui_inst.len()));
        self.gpu.encode_draw(
            &mut encoder,
            &view,
            size,
            &self.draw_scratch,
            |id| self.panes.get(&id).and_then(|pane| pane.buf.as_ref()),
            ui,
            scene.clear,
        );
        self.gpu.queue.submit([encoder.finish()]);
        self.gpu.present(texture);
        if suboptimal {
            self.gpu.resize(size);
        }
        self.finish_frame(scene);
        Ok(RenderOutcome::Presented)
    }

    /// Frees the GPU buffers of a closed pane.
    pub fn forget_pane(&mut self, id: u64) {
        self.panes.remove(&id);
    }

    /// Test-only: a renderer with no surface that draws into an offscreen
    /// `Rgba8Unorm` texture of `size_px`.
    #[doc(hidden)]
    pub fn new_headless(
        size_px: (u32, u32),
        scale: f64,
        font: &FontConfig,
        font_file: Option<&std::path::Path>,
    ) -> Result<Self, RenderError> {
        let gpu = gpu::Gpu::new_headless(size_px)?;
        let max_dim = gpu.max_texture_dimension();
        let font_object = match font_file {
            Some(path) => Font::from_file(path, font, scale),
            None => Font::load(font, scale),
        }
        .map_err(RenderError::Font)?;
        let glyphs = GlyphCache::new(font_object, max_dim);
        Ok(Self::from_parts(gpu, glyphs, font.clone(), scale))
    }

    /// Test-only: renders `scene` offscreen (headless renderer) and returns tightly packed
    /// RGBA8 rows, `w * h * 4` bytes. Same damage/clear_dirty semantics as `render`.
    #[doc(hidden)]
    pub fn render_offscreen(&mut self, scene: &mut Scene<'_, '_>) -> Result<Vec<u8>, RenderError> {
        if self.gpu.take_device_lost() {
            self.rebuild_after_loss()?;
        }
        let size = self.gpu.target_size();
        let ui_dirty = self.encode_scene(scene);
        self.flush_atlas();
        self.upload_all(scene, size, ui_dirty);

        if size.0 == 0 || size.1 == 0 || self.gpu.suspended() {
            for pane in scene.panes.iter_mut() {
                pane.frame.clear_dirty();
            }
            return Ok(Vec::new());
        }
        let Some(view) = self.gpu.offscreen_view() else {
            return Err(RenderError::Device("no offscreen target".into()));
        };

        let padded = (size.0 * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self
            .gpu
            .create_readback_buffer(u64::from(padded) * u64::from(size.1));
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nt-render offscreen"),
            });
        let ui = self.ui_buf.as_ref().map(|buf| (buf, self.ui_inst.len()));
        self.gpu.encode_draw(
            &mut encoder,
            &view,
            size,
            &self.draw_scratch,
            |id| self.panes.get(&id).and_then(|pane| pane.buf.as_ref()),
            ui,
            scene.clear,
        );
        self.gpu
            .copy_offscreen(&mut encoder, &readback, padded, size);
        self.gpu.queue.submit([encoder.finish()]);

        let padded_data = self.gpu.read_buffer(&readback)?;
        let row_bytes = (size.0 * 4) as usize;
        let mut tight = Vec::with_capacity(row_bytes * size.1 as usize);
        for row in padded_data.chunks_exact(padded as usize) {
            tight.extend_from_slice(&row[..row_bytes]);
        }
        self.finish_frame(scene);
        Ok(tight)
    }

    fn from_parts(gpu: gpu::Gpu, glyphs: GlyphCache, font: FontConfig, scale: f64) -> Self {
        Self {
            gpu,
            glyphs,
            panes: HashMap::new(),
            encs: Vec::new(),
            ui_inst: Vec::new(),
            ui_gen: None,
            ui_buf: None,
            ui_cap: 0,
            font,
            scale,
            last: Vec::new(),
            last_clear: Rgba::default(),
            draw_scratch: Vec::new(),
        }
    }

    /// Rebuilds GPU state after a device loss; one attempt, as recorded.
    // shortcut: one rebuild attempt; a second loss surfaces as Err(Device) to the app.
    fn rebuild_after_loss(&mut self) -> Result<(), RenderError> {
        let mask = self.glyphs.atlas_mut().size(Page::Mask);
        let color = self.glyphs.atlas_mut().size(Page::Color);
        self.gpu.rebuild(mask, color)?;
        self.glyphs.reset();
        self.panes.clear();
        self.ui_buf = None;
        self.ui_cap = 0;
        self.ui_gen = None;
        self.last.clear();
        Ok(())
    }

    /// Encodes every pane and, when needed, the UI. Returns true if the UI instances changed.
    fn encode_scene(&mut self, scene: &mut Scene<'_, '_>) -> bool {
        let mut ui_dirty = false;
        let mut force_ui = false;
        let mut epoch = self.glyphs.epoch();
        for pass in 0..2 {
            self.encs.clear();
            for draw in scene.panes.iter_mut() {
                let viewport = draw.viewport;
                let glyphs = &mut self.glyphs;
                let pane = self.panes.entry(draw.id).or_insert_with(Pane::new);
                let mut encoded = pane.layer.encode(
                    draw.frame,
                    (viewport.w, viewport.h),
                    draw.focused,
                    draw.cursor_visible,
                    glyphs,
                );
                // Pass 2 runs because the atlas was cleared mid-frame: a pane encoded fully in
                // pass 1 (it triggered the clear) may report only dirty runs now, so upload
                // every pane whole.
                encoded.full |= pass == 1;
                self.encs.push((draw.id, encoded));
            }
            if force_ui || self.ui_gen != Some(scene.ui.generation) {
                encode_ui(scene.ui, &mut self.glyphs, &mut self.ui_inst);
                self.ui_gen = Some(scene.ui.generation);
                ui_dirty = true;
            }
            let now = self.glyphs.epoch();
            if now == epoch {
                break;
            }
            epoch = now;
            force_ui = true;
        }
        ui_dirty
    }

    fn should_skip(&mut self, scene: &Scene<'_, '_>, ui_dirty: bool) -> bool {
        if ui_dirty || self.last_clear != scene.clear {
            return false;
        }
        if self.last.len() != scene.panes.len()
            || !self
                .last
                .iter()
                .zip(scene.panes.iter())
                .all(|((id, viewport), draw)| *id == draw.id && *viewport == draw.viewport)
        {
            return false;
        }
        if self.encs.iter().any(|(_, encoded)| !encoded.is_empty()) {
            return false;
        }
        !self.glyphs.atlas_mut().has_pending()
    }

    fn flush_atlas(&mut self) {
        let glyphs = &mut self.glyphs;
        let gpu = &mut self.gpu;
        glyphs.atlas_mut().drain(|op| gpu.apply_atlas(&op));
    }

    fn upload_all(&mut self, scene: &Scene<'_, '_>, size: (u32, u32), ui_dirty: bool) {
        for index in 0..self.encs.len() {
            let (id, encoded) = self.encs[index];
            self.upload_pane(id, encoded);
        }
        if ui_dirty {
            let len = self.ui_inst.len();
            if len > self.ui_cap {
                let cap = len.next_power_of_two();
                self.ui_buf = Some(
                    self.gpu
                        .create_inst_buffer(cap as u64 * size_of::<Inst>() as u64),
                );
                self.ui_cap = cap;
            }
            if len > 0
                && let Some(buffer) = &self.ui_buf
            {
                self.gpu
                    .write_buffer(buffer, 0, bytemuck::cast_slice(&self.ui_inst));
            }
        }

        let slots = scene.panes.len();
        self.gpu.ensure_slots(slots + 1);
        let screen = [size.0 as f32, size.1 as f32];
        for (slot, draw) in scene.panes.iter().enumerate() {
            self.gpu.write_slot(
                slot,
                [draw.viewport.x as f32, draw.viewport.y as f32],
                screen,
            );
        }
        self.gpu.write_slot(slots, [0.0, 0.0], screen);

        self.draw_scratch.clear();
        for draw in scene.panes.iter() {
            let len = self.panes.get(&draw.id).map_or(0, |pane| pane.buf_len);
            self.draw_scratch.push((draw.id, len, draw.viewport));
        }
    }

    fn upload_pane(&mut self, id: u64, encoded: Encoded) {
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let len = pane.layer.instances().len();
        let recreate = pane.buf.is_none() || pane.buf_len != len;
        if encoded.full || recreate {
            if recreate {
                pane.buf = Some(
                    self.gpu
                        .create_inst_buffer(len as u64 * size_of::<Inst>() as u64),
                );
                pane.buf_len = len;
            }
            if let Some(buffer) = &pane.buf {
                self.gpu
                    .write_buffer(buffer, 0, bytemuck::cast_slice(pane.layer.instances()));
            }
            return;
        }
        let Some(buffer) = &pane.buf else {
            return;
        };
        if encoded.head {
            let instances = pane.layer.instances();
            if !instances.is_empty() {
                self.gpu
                    .write_buffer(buffer, 0, bytemuck::cast_slice(&instances[..1]));
            }
        }
        for &(start, end) in encoded.runs.as_slice() {
            for range in pane.layer.row_ranges((start, end)) {
                if range.is_empty() {
                    continue;
                }
                let instances = pane.layer.instances();
                self.gpu.write_buffer(
                    buffer,
                    range.start as u64 * size_of::<Inst>() as u64,
                    bytemuck::cast_slice(&instances[range]),
                );
            }
        }
        if encoded.overlay {
            let range = pane.layer.overlay_range();
            if !range.is_empty() {
                let instances = pane.layer.instances();
                self.gpu.write_buffer(
                    buffer,
                    range.start as u64 * size_of::<Inst>() as u64,
                    bytemuck::cast_slice(&instances[range]),
                );
            }
        }
    }

    fn finish_frame(&mut self, scene: &mut Scene<'_, '_>) {
        for draw in scene.panes.iter_mut() {
            draw.frame.clear_dirty();
        }
        self.last.clear();
        for draw in scene.panes.iter() {
            self.last.push((draw.id, draw.viewport));
        }
        self.last_clear = scene.clear;
    }
}

fn metrics_cell_size(metrics: crate::font::Metrics) -> CellSize {
    CellSize {
        w: metrics.cell_w,
        h: metrics.cell_h,
    }
}

/// Test-only access to the CPU half (no wgpu types), for `tests/` and `benches/`.
#[doc(hidden)]
pub mod __test {
    pub use crate::atlas::{Atlas, AtlasOp, Page};
    pub use crate::builtin::{Deco, covers, deco, rasterize};
    pub use crate::encode::{
        DirtyRuns, Encoded, FLAG_COLOR, FLAG_MASK, FLAG_SOLID, GridLayer, Inst, OVERLAY_CAP,
        encode_ui,
    };
    pub use crate::font::{Font, Metrics, Raster, Raw, compute_metrics, px_size};
    pub use crate::glyphs::{GlyphCache, GlyphSlot, Style};
}
