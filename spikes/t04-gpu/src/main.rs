//! T04 spike: winit + wgpu instanced cell grid. Measures startup phases, frame cost, idle behaviour.
//! Throwaway; see .tasks/T04-spike-gpu-and-fonts.md Design §2.

mod atlas;
mod png;

use std::alloc::{GlobalAlloc, Layout, System};
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

// ---------- counting allocator ----------

static ALLOCS: AtomicU64 = AtomicU64::new(0);
struct Counting;
// SAFETY: forwards every call unchanged to the System allocator; only adds a relaxed counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: caller upholds GlobalAlloc::alloc's contract; passed through as-is.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: p was returned by System via this allocator with layout l.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: as alloc.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: p/l come from this allocator (i.e. System); caller upholds realloc's contract.
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

// ---------- CLOCK_MONOTONIC (same clock as the launcher) ----------
// std Instant is CLOCK_MONOTONIC on Linux but opaque, so it cannot be compared across processes.
// libc is linked by std anyway; declare clock_gettime directly instead of adding a crate.
// shortcut: timespec layout below is 64-bit Linux only (time_t = long = i64); macOS is T22.

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}
unsafe extern "C" {
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
}
const CLOCK_MONOTONIC: i32 = 1;

fn monotonic_ns() -> u64 {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: ts is a valid, writable timespec with the 64-bit Linux layout; CLOCK_MONOTONIC always exists.
    let r = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(r, 0, "clock_gettime failed");
    ts.sec as u64 * 1_000_000_000 + ts.nsec as u64
}

// ---------- no-executor block_on ----------

/// Native wgpu futures are `future::ready` (wgpu_core backend); one poll suffices.
fn block_on<F: Future>(f: F) -> F::Output {
    match pin!(f).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("wgpu future pending on native: unexpected (wasm path?)"),
    }
}

// ---------- args ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Static,
    Scroll,
    OneRow,
}

struct Args {
    backend: wgpu::Backends,
    present: wgpu::PresentMode,
    latency: u32,
    cols: u32,
    rows: u32,
    font_px: f32,
    mode: Mode,
    screenshot: Option<String>,
    run_secs: Option<u64>,
    exit_after_first: bool,
    launch_t0_ns: Option<u64>,
}

fn usage(msg: &str) -> ! {
    eprintln!(
        "error: {msg}\nusage: t04-gpu [--backend vulkan|gl] [--present fifo|mailbox|immediate] [--latency 1|2|3] \
         [--grid 200x60] [--font-px 18] [--mode static|scroll|onerow] [--screenshot PATH] [--run-secs N]\n\
         env: NT_EXIT_AFTER_FIRST_FRAME=1, NT_LAUNCH_T0_NS=<CLOCK_MONOTONIC ns>"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        backend: wgpu::Backends::VULKAN,
        present: wgpu::PresentMode::Fifo,
        latency: 1,
        cols: 200,
        rows: 60,
        font_px: 18.0,
        mode: Mode::Static,
        screenshot: None,
        run_secs: None,
        exit_after_first: std::env::var("NT_EXIT_AFTER_FIRST_FRAME").is_ok_and(|v| v == "1"),
        launch_t0_ns: std::env::var("NT_LAUNCH_T0_NS").ok().map(|v| {
            v.parse()
                .unwrap_or_else(|_| usage("NT_LAUNCH_T0_NS not an integer"))
        }),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .unwrap_or_else(|| usage(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--backend" => {
                a.backend = match val().as_str() {
                    "vulkan" => wgpu::Backends::VULKAN,
                    "gl" => wgpu::Backends::GL,
                    v => usage(&format!("bad backend {v}")),
                }
            }
            "--present" => {
                a.present = match val().as_str() {
                    "fifo" => wgpu::PresentMode::Fifo,
                    "mailbox" => wgpu::PresentMode::Mailbox,
                    "immediate" => wgpu::PresentMode::Immediate,
                    v => usage(&format!("bad present mode {v}")),
                }
            }
            "--latency" => {
                a.latency = match val().parse() {
                    Ok(n @ 1..=3) => n,
                    _ => usage("latency must be 1, 2 or 3"),
                }
            }
            "--grid" => {
                let v = val();
                let (c, r) = v
                    .split_once('x')
                    .unwrap_or_else(|| usage("grid must be COLSxROWS"));
                match (c.parse(), r.parse()) {
                    (Ok(c @ 1..=1000), Ok(r @ 1..=1000)) => (a.cols, a.rows) = (c, r),
                    _ => usage("grid dims must be 1..=1000"),
                }
            }
            "--font-px" => {
                a.font_px = match val().parse::<f32>() {
                    Ok(p) if (4.0..=200.0).contains(&p) => p,
                    _ => usage("font-px must be 4..=200"),
                }
            }
            "--mode" => {
                a.mode = match val().as_str() {
                    "static" => Mode::Static,
                    "scroll" => Mode::Scroll,
                    "onerow" => Mode::OneRow,
                    v => usage(&format!("bad mode {v}")),
                }
            }
            "--screenshot" => a.screenshot = Some(val()),
            "--run-secs" => {
                a.run_secs = Some(
                    val()
                        .parse()
                        .unwrap_or_else(|_| usage("run-secs must be an integer")),
                )
            }
            f => usage(&format!("unknown flag {f}")),
        }
    }
    a
}

// ---------- phase timers ----------

struct Phases {
    start: Instant,
    last: Duration,
    v: Vec<(&'static str, Duration, Duration)>,
}

impl Phases {
    fn mark(&mut self, name: &'static str) {
        let now = self.start.elapsed();
        self.v.push((name, now, now - self.last));
        self.last = now;
    }
    fn print(&self) {
        println!("phase\tcum_us\tdelta_us");
        for (n, c, d) in &self.v {
            println!("{n}\t{}\t{}", c.as_micros(), d.as_micros());
        }
    }
}

// ---------- instance layout (Design §2) ----------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BgInstance {
    col_row: [u16; 2],
    color: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    col_row: [u16; 2],
    uv_min: [u16; 2],
    uv_size: [u16; 2],
    offset: [i16; 2],
    fg: [u8; 4],
    flags: u32,
}

const _: () = assert!(size_of::<BgInstance>() == 8 && size_of::<GlyphInstance>() == 24);

#[derive(Clone, Copy, Default)]
struct GlyphEntry {
    uv_min: [u16; 2],
    uv_size: [u16; 2],
    offset: [i16; 2],
}

const LOOP_FRAMES: usize = 600;
const ATLAS: u32 = 1024;

const SAMPLE: &[&str] = &[
    "ASCII: The quick brown fox jumps over the lazy dog. 0123456789 g y p q j  !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
    "Descenders: gypsy jumpy query | Box: ┌──┬──┐ │ab│cd│ ├──┼──┤ └──┴──┘ ▀▄█▌▐░▒▓ | Powerline: \u{e0b0}\u{e0b1}\u{e0b2}\u{e0b3}",
    "CJK: 中文字 日本語 | Emoji: 😀🎉 | Combining: e\u{301} a\u{308} n\u{303} | Markers: **bold** _italic_ ***both***",
    "fn main() { let mut v: Vec<u8> = vec![0x7f; 16]; println!(\"{v:?} -> {}\", v.len()); } // ok",
];

// ---------- renderer ----------

struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    bg_pipe: wgpu::RenderPipeline,
    glyph_pipe: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    ubuf: wgpu::Buffer,
    bg_buf: wgpu::Buffer,
    glyph_buf: wgpu::Buffer,
    bg_stage: Vec<BgInstance>,
    glyph_stage: Vec<GlyphInstance>,
    ascii: [GlyphEntry; 128],
    placeholder: GlyphEntry,
    text: Vec<Vec<char>>,
    cell: [f32; 2],
    cols: u32,
    rows: u32,
    timestamps: Option<wgpu::QuerySet>,
}

impl Gpu {
    fn write_uniforms(&self, vw: u32, vh: u32) {
        let u: [f32; 8] = [
            self.cell[0],
            self.cell[1],
            0.0,
            0.0,
            ATLAS as f32,
            ATLAS as f32,
            vw as f32,
            vh as f32,
        ];
        self.queue
            .write_buffer(&self.ubuf, 0, bytemuck::cast_slice(&u));
    }

    /// Fill one row of both staging arrays in place. shift 0 = static content.
    fn fill_row(&mut self, row: u32, shift: u32) {
        let line = &self.text[((row + shift) as usize) % self.text.len()];
        let base = (row * self.cols) as usize;
        for col in 0..self.cols {
            let i = base + col as usize;
            let ch = line[((col + shift) as usize) % line.len()];
            let color = if shift == 0 {
                if row.is_multiple_of(2) {
                    [24, 24, 32, 255]
                } else {
                    [32, 32, 44, 255]
                }
            } else {
                [(col + shift) as u8, (row * 4 + shift) as u8, 48, 255]
            };
            let g = if (ch as u32) < 128 {
                self.ascii[ch as usize]
            } else {
                self.placeholder
            };
            let cr = [col as u16, row as u16];
            self.bg_stage[i] = BgInstance { col_row: cr, color };
            // Fixed slot per cell (spaces = zero-size quad) so one-row damage is one contiguous byte range.
            self.glyph_stage[i] = GlyphInstance {
                col_row: cr,
                uv_min: g.uv_min,
                uv_size: g.uv_size,
                offset: g.offset,
                fg: [220, 220, 220, 255],
                flags: 0,
            };
        }
    }

    fn upload_rows(&self, rows: std::ops::Range<u32>) {
        let r = (rows.start * self.cols) as usize..(rows.end * self.cols) as usize;
        self.queue.write_buffer(
            &self.bg_buf,
            (r.start * size_of::<BgInstance>()) as u64,
            bytemuck::cast_slice(&self.bg_stage[r.clone()]),
        );
        self.queue.write_buffer(
            &self.glyph_buf,
            (r.start * size_of::<GlyphInstance>()) as u64,
            bytemuck::cast_slice(&self.glyph_stage[r]),
        );
    }

    fn draw(
        &self,
        enc: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        ts: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let n = self.cols * self.rows;
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.05,
                        g: 0.05,
                        b: 0.07,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: ts,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_pipeline(&self.bg_pipe);
        pass.set_vertex_buffer(0, self.bg_buf.slice(..));
        pass.draw(0..4, 0..n);
        pass.set_pipeline(&self.glyph_pipe);
        pass.set_vertex_buffer(0, self.glyph_buf.slice(..));
        pass.draw(0..4, 0..n);
    }

    fn reconfigure(&mut self, w: u32, h: u32) {
        self.config.width = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.device, &self.config);
        self.write_uniforms(self.config.width, self.config.height);
    }

    /// Acquire, encode, submit, present. Returns false if the frame was skipped.
    /// `ph` (first frame only) marks first_encode after submit and first_present after present().
    /// `marks` (loop frames) receives Instants after acquire, after submit, after present.
    fn frame(
        &mut self,
        ts_index: Option<u32>,
        mut ph: Option<&mut Phases>,
        mut marks: Option<&mut [Instant; 3]>,
    ) -> bool {
        let st = loop {
            match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t) => break t,
                wgpu::CurrentSurfaceTexture::Suboptimal(t) => break t, // reconfigured on the next Resized
                wgpu::CurrentSurfaceTexture::Outdated => {
                    let s = self.window.inner_size();
                    self.reconfigure(s.width, s.height);
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return false;
                }
                e => panic!("surface acquire failed: {e:?}"),
            }
        };
        if let Some(m) = marks.as_deref_mut() {
            m[0] = Instant::now();
        }
        let view = st
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let ts = match (&self.timestamps, ts_index) {
            (Some(qs), Some(i)) => Some(wgpu::RenderPassTimestampWrites {
                query_set: qs,
                beginning_of_pass_write_index: Some(2 * i),
                end_of_pass_write_index: Some(2 * i + 1),
            }),
            _ => None,
        };
        self.draw(&mut enc, &view, ts);
        self.queue.submit([enc.finish()]);
        if let Some(p) = ph.as_deref_mut() {
            p.mark("first_encode");
        }
        if let Some(m) = marks.as_deref_mut() {
            m[1] = Instant::now();
        }
        self.window.pre_present_notify();
        self.queue.present(st);
        if let Some(p) = ph {
            p.mark("first_present");
        }
        if let Some(m) = marks {
            m[2] = Instant::now();
        }
        true
    }

    fn map_read(&self, buf: &wgpu::Buffer) -> Vec<u8> {
        buf.slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.expect("map_async failed"));
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll failed");
        let v = buf
            .slice(..)
            .get_mapped_range()
            .expect("mapped range")
            .to_vec();
        buf.unmap();
        v
    }

    fn screenshot(&self, path: &str) {
        let (w, h) = (
            self.cols * self.cell[0] as u32,
            self.rows * self.cell[1] as u32,
        );
        let fmt = self.config.format;
        let swap_rb = match fmt {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => true,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => false,
            f => panic!("screenshot: unsupported surface format {f:?}"),
        };
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shot"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: fmt,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let padded = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shot-read"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.write_uniforms(w, h);
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.draw(
            &mut enc,
            &tex.create_view(&wgpu::TextureViewDescriptor::default()),
            None,
        );
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            size,
        );
        self.queue.submit([enc.finish()]);
        let data = self.map_read(&buf);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for row in data.chunks_exact(padded as usize) {
            for px in row[..(w * 4) as usize].as_chunks::<4>().0 {
                rgba.extend_from_slice(&if swap_rb {
                    [px[2], px[1], px[0], 255]
                } else {
                    [px[0], px[1], px[2], 255]
                });
            }
        }
        std::fs::write(path, png::encode_rgba(w, h, &rgba)).expect("write png");
        println!("screenshot\t{path}\t{w}x{h}\tformat {fmt:?}");
        self.write_uniforms(self.config.width, self.config.height);
    }

    /// Resolve all loop-frame timestamps at once (no per-frame readback stall); returns GPU pass ns per frame.
    fn gpu_times(&self, frames: u32) -> Option<Vec<f64>> {
        let qs = self.timestamps.as_ref()?;
        let n = 2 * frames;
        let bytes = n as u64 * 8;
        let resolve = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.resolve_query_set(qs, 0..n, &resolve, 0);
        enc.copy_buffer_to_buffer(&resolve, 0, &read, 0, bytes);
        self.queue.submit([enc.finish()]);
        let raw = self.map_read(&read);
        let t: &[u64] = bytemuck::cast_slice(&raw);
        let period = self.queue.get_timestamp_period() as f64;
        Some(
            t.as_chunks::<2>()
                .0
                .iter()
                .map(|p| p[1].wrapping_sub(p[0]) as f64 * period)
                .collect(),
        )
    }
}

// ---------- app ----------

struct App {
    args: Args,
    ph: Phases,
    font: Option<(t04_fonts::Metrics, Vec<t04_fonts::Glyph>)>,
    gpu: Option<Gpu>,
    // frame loop stats
    loop_frame: u32,
    last_redraw: Option<Instant>,
    cpu_ns: Vec<f64>,
    interval_ns: Vec<f64>,
    /// CPU breakdown per loop frame: fill+upload, acquire, encode+submit, present.
    split_ns: [Vec<f64>; 4],
    render_allocs: u64,
    loop_allocs_start: u64,
    // idle stats (static mode)
    idle_redraws: u64,
    idle_resizes: u64,
    idle_start: Option<(Instant, u64)>,
    deadline: Option<Instant>,
}

/// utime + stime of this process, clock ticks (USER_HZ, 100 on Linux), from /proc/self/stat fields 14-15.
fn cpu_ticks() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
    let f: Vec<&str> = s[s.rfind(')').expect("stat format") + 2..]
        .split(' ')
        .collect();
    f[11].parse::<u64>().expect("utime") + f[12].parse::<u64>().expect("stime")
}

fn pct(v: &mut [f64]) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    (at(0.50), at(0.99), v[v.len() - 1])
}

impl App {
    fn init(&mut self, el: &ActiveEventLoop) {
        let (m, glyphs) = self.font.take().expect("font loaded in main");
        let (cols, rows) = (self.args.cols, self.args.rows);
        let ph = &mut self.ph;

        let attrs = Window::default_attributes()
            .with_title("t04-gpu")
            .with_inner_size(PhysicalSize::new(
                cols * m.cell_w as u32,
                rows * m.cell_h as u32,
            ));
        let window = Arc::new(el.create_window(attrs).expect("create_window"));
        ph.mark("window");

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: self.args.backend,
            display: Some(Box::new(el.owned_display_handle())),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        ph.mark("instance");

        let surface = instance
            .create_surface(window.clone())
            .expect("create_surface");
        ph.mark("surface");

        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .expect("request_adapter");
        ph.mark("adapter");

        let feats = adapter.features();
        let ts_ok = feats.contains(wgpu::Features::TIMESTAMP_QUERY);
        let want_ts = ts_ok && self.args.mode != Mode::Static;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: if want_ts {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            },
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .expect("request_device");
        ph.mark("device");

        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB target: colours pass through unchanged and blending is in gamma space (Alacritty-like).
        let format = *caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .unwrap_or(&caps.formats[0]);
        if !caps.present_modes.contains(&self.args.present) {
            eprintln!(
                "present mode {:?} unsupported; supported: {:?}",
                self.args.present, caps.present_modes
            );
            std::process::exit(3);
        }
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: self.args.present,
            desired_maximum_frame_latency: self.args.latency,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        ph.mark("configure");

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        ph.mark("shader_module");

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let bg_attrs = wgpu::vertex_attr_array![0 => Uint16x2, 1 => Unorm8x4];
        let glyph_attrs = wgpu::vertex_attr_array![0 => Uint16x2, 1 => Uint16x2, 2 => Uint16x2, 3 => Sint16x2, 4 => Unorm8x4, 5 => Uint32];
        let pipe = |vs: &str,
                    fs: &str,
                    stride: usize,
                    attrs: &[wgpu::VertexAttribute],
                    blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(vs),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: stride as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: attrs,
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let bg_pipe = pipe(
            "bg_vs",
            "bg_fs",
            size_of::<BgInstance>(),
            &bg_attrs,
            wgpu::BlendState::REPLACE,
        );
        let glyph_pipe = pipe(
            "glyph_vs",
            "glyph_fs",
            size_of::<GlyphInstance>(),
            &glyph_attrs,
            wgpu::BlendState::ALPHA_BLENDING,
        );
        ph.mark("pipelines");

        // Atlas: pack ASCII + a placeholder box (for every non-ASCII char), upload used rows once.
        let mut atlas = atlas::Atlas::new(ATLAS, ATLAS);
        let mut ascii = [GlyphEntry::default(); 128];
        let mut colour = 0;
        let place = |atlas: &mut atlas::Atlas, w: u16, h: u16, data: &[u8], offset: [i16; 2]| {
            if w == 0 || h == 0 {
                return GlyphEntry::default();
            }
            let (x, y) = atlas
                .alloc(w as u32, h as u32)
                .expect("atlas full (spike: 1024² is plenty for ASCII)");
            atlas.blit(x, y, w as u32, h as u32, data);
            GlyphEntry {
                uv_min: [x as u16, y as u16],
                uv_size: [w, h],
                offset,
            }
        };
        for g in &glyphs {
            if g.color {
                colour += 1; // shortcut: no RGBA8 atlas in the spike; ASCII from a mono face is never colour.
                continue;
            }
            if (g.ch as u32) < 128 {
                ascii[g.ch as usize] =
                    place(&mut atlas, g.w, g.h, &g.data, [g.left, m.ascent - g.top]);
            }
        }
        let (pw, phh) = (
            m.cell_w.saturating_sub(2).max(1),
            m.cell_h.saturating_sub(2).max(1),
        );
        let boxpx: Vec<u8> = (0..phh)
            .flat_map(|y| {
                (0..pw).map(move |x| {
                    if x == 0 || y == 0 || x == pw - 1 || y == phh - 1 {
                        255
                    } else {
                        0
                    }
                })
            })
            .collect();
        let placeholder = place(&mut atlas, pw, phh, &boxpx, [1, 1]);
        let used_h = atlas.used_h().max(1);
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas-r8"),
            size: wgpu::Extent3d {
                width: ATLAS,
                height: ATLAS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            atlas_tex.as_image_copy(),
            &atlas.pixels[..(ATLAS * used_h) as usize],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS),
                rows_per_image: Some(used_h),
            },
            wgpu::Extent3d {
                width: ATLAS,
                height: used_h,
                depth_or_array_layers: 1,
            },
        );
        let n = (cols * rows) as usize;
        let mk = |size: usize, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: size as u64,
                usage,
                mapped_at_creation: false,
            })
        };
        let ubuf = mk(
            32,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let bg_buf = mk(
            n * size_of::<BgInstance>(),
            wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        );
        let glyph_buf = mk(
            n * size_of::<GlyphInstance>(),
            wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ubuf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &atlas_tex.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
            ],
        });
        let timestamps = want_ts.then(|| {
            device.create_query_set(&wgpu::QuerySetDescriptor {
                label: None,
                ty: wgpu::QueryType::Timestamp,
                count: 2 * LOOP_FRAMES as u32,
            })
        });
        ph.mark("atlas_upload");

        let text = SAMPLE
            .iter()
            .map(|l| {
                l.chars()
                    .chain(std::iter::repeat(' '))
                    .take(cols as usize)
                    .collect()
            })
            .collect();
        let mut gpu = Gpu {
            window,
            surface,
            device,
            queue,
            config,
            bg_pipe,
            glyph_pipe,
            bind_group,
            ubuf,
            bg_buf,
            glyph_buf,
            bg_stage: vec![BgInstance::zeroed(); n],
            glyph_stage: vec![GlyphInstance::zeroed(); n],
            ascii,
            placeholder,
            text,
            cell: [m.cell_w as f32, m.cell_h as f32],
            cols,
            rows,
            timestamps,
        };
        gpu.write_uniforms(gpu.config.width, gpu.config.height);
        for r in 0..rows {
            gpu.fill_row(r, 0);
        }
        gpu.upload_rows(0..rows);
        while !gpu.frame(None, Some(ph), None) {}
        let launch = self
            .args
            .launch_t0_ns
            .map(|t0| monotonic_ns().saturating_sub(t0));

        // ---- report (not timed) ----
        ph.print();
        if let Some(ns) = launch {
            println!("launch_to_first_present_us\t{}", ns / 1000);
        }
        let info = adapter.get_info();
        println!(
            "adapter\t{} | {:?} | {:?} | driver {} {}",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        );
        println!(
            "surface_format\t{format:?}\tpresent_modes {:?}\talpha {:?}",
            caps.present_modes, caps.alpha_modes
        );
        println!(
            "present\t{:?}\tlatency {}",
            self.args.present, self.args.latency
        );
        println!(
            "features\tTIMESTAMP_QUERY {}\tTIMESTAMP_QUERY_INSIDE_PASSES {}\tPIPELINE_CACHE {}",
            ts_ok,
            feats.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES),
            feats.contains(wgpu::Features::PIPELINE_CACHE)
        );
        println!(
            "window\tscale {}\tsurface {}x{}\tcell {}x{} ascent {} descent {}\tglyphs {} colour_skipped {colour}\tatlas used {}x{used_h}",
            gpu.window.scale_factor(),
            gpu.config.width,
            gpu.config.height,
            m.cell_w,
            m.cell_h,
            m.ascent,
            m.descent,
            glyphs.len(),
            ATLAS
        );
        println!("csd_feature\t{}", cfg!(feature = "csd"));

        if self.args.exit_after_first {
            std::process::exit(0);
        }
        if let Some(p) = &self.args.screenshot {
            gpu.screenshot(p);
            std::process::exit(0);
        }
        match self.args.mode {
            Mode::Static => {
                self.idle_start = Some((Instant::now(), cpu_ticks()));
                if let Some(s) = self.args.run_secs {
                    let d = Instant::now() + Duration::from_secs(s);
                    self.deadline = Some(d);
                    el.set_control_flow(ControlFlow::WaitUntil(d));
                }
            }
            Mode::Scroll | Mode::OneRow => {
                self.loop_allocs_start = ALLOCS.load(Relaxed);
                gpu.window.request_redraw();
            }
        }
        self.gpu = Some(gpu);
    }

    fn finish_idle(&self) -> ! {
        let (t, ticks) = self.idle_start.expect("idle started");
        let secs = t.elapsed().as_secs_f64();
        let dt = cpu_ticks() - ticks;
        println!(
            "idle\t{secs:.2} s\tredraw_requested {}\tresized {}\tcpu_ticks {dt} (USER_HZ=100 assumed) = {:.3}% of one core",
            self.idle_redraws,
            self.idle_resizes,
            dt as f64 / 100.0 / secs * 100.0
        );
        std::process::exit(0)
    }

    fn finish_loop(&mut self) -> ! {
        let loop_allocs = ALLOCS.load(Relaxed) - self.loop_allocs_start;
        let gpu = self.gpu.as_ref().expect("gpu");
        let f = self.cpu_ns.len();
        let ms = |(a, b, c): (f64, f64, f64)| {
            format!("p50 {:.3}\tp99 {:.3}\tmax {:.3}", a / 1e6, b / 1e6, c / 1e6)
        };
        println!("frames\t{f}\tmode {:?}", self.args.mode);
        println!("cpu_redraw_to_present_ms\t{}", ms(pct(&mut self.cpu_ns)));
        for (name, v) in ["fill_upload", "acquire", "encode_submit", "present"]
            .iter()
            .zip(self.split_ns.iter_mut())
        {
            println!("  cpu_{name}_ms\t{}", ms(pct(v)));
        }
        println!(
            "interval_between_redraws_ms\t{}",
            ms(pct(&mut self.interval_ns))
        );
        match gpu.gpu_times(f as u32) {
            Some(mut g) => println!("gpu_pass_ms\t{}", ms(pct(&mut g))),
            None => println!("gpu_pass_ms\tunsupported (no TIMESTAMP_QUERY)"),
        }
        println!(
            "allocs_per_frame\trender {:.1}\tloop_total {:.1}",
            self.render_allocs as f64 / f as f64,
            loop_allocs as f64 / f as f64
        );
        std::process::exit(0)
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_none() {
            self.init(el);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => {
                if self.args.mode == Mode::Static && self.idle_start.is_some() {
                    self.finish_idle();
                }
                el.exit();
            }
            WindowEvent::Resized(s) => {
                if self.idle_start.is_some() {
                    self.idle_resizes += 1;
                }
                gpu.reconfigure(s.width, s.height);
                gpu.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let t = Instant::now();
                match self.args.mode {
                    Mode::Static => {
                        self.idle_redraws += 1;
                        gpu.frame(None, None, None);
                    }
                    Mode::Scroll | Mode::OneRow => {
                        if let Some(p) = self.last_redraw {
                            self.interval_ns.push((t - p).as_nanos() as f64);
                        }
                        self.last_redraw = Some(t);
                        let a0 = ALLOCS.load(Relaxed);
                        let k = self.loop_frame + 1; // shift 0 is the static frame
                        if self.args.mode == Mode::Scroll {
                            for r in 0..gpu.rows {
                                gpu.fill_row(r, k);
                            }
                            gpu.upload_rows(0..gpu.rows);
                        } else {
                            let r = k % gpu.rows;
                            gpu.fill_row(r, k);
                            gpu.upload_rows(r..r + 1);
                        }
                        let t_up = Instant::now();
                        let mut m = [t_up; 3];
                        if gpu.frame(Some(self.loop_frame), None, Some(&mut m)) {
                            self.cpu_ns.push((m[2] - t).as_nanos() as f64);
                            for (v, d) in self.split_ns.iter_mut().zip([
                                t_up - t,
                                m[0] - t_up,
                                m[1] - m[0],
                                m[2] - m[1],
                            ]) {
                                v.push(d.as_nanos() as f64);
                            }
                            self.render_allocs += ALLOCS.load(Relaxed) - a0;
                            self.loop_frame += 1;
                        }
                        if self.loop_frame as usize == LOOP_FRAMES {
                            self.finish_loop();
                        }
                        gpu.window.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            self.finish_idle();
        }
    }
}

fn main() {
    let start = Instant::now();
    let args = parse_args();
    let mut ph = Phases {
        start,
        last: Duration::ZERO,
        v: Vec::with_capacity(16),
    };
    // Font first: window size needs cell metrics. Measured as its own phase so it can be subtracted on paper.
    let font = t04_fonts::rasterize_ascii(args.font_px);
    ph.mark("font");
    let el = EventLoop::new().expect("event loop");
    ph.mark("event_loop");
    el.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        args,
        ph,
        font: Some(font),
        gpu: None,
        loop_frame: 0,
        last_redraw: None,
        cpu_ns: Vec::with_capacity(LOOP_FRAMES),
        interval_ns: Vec::with_capacity(LOOP_FRAMES),
        split_ns: std::array::from_fn(|_| Vec::with_capacity(LOOP_FRAMES)),
        render_allocs: 0,
        loop_allocs_start: 0,
        idle_redraws: 0,
        idle_resizes: 0,
        idle_start: None,
        deadline: None,
    };
    el.run_app(&mut app).expect("run_app");
}
