//! Device, surface, pipeline, buffers, uploads and draw. The only module with wgpu types.

use std::future::Future;
use std::num::NonZeroU64;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use bytemuck::{Pod, Zeroable};
use nt_term::Rgba;

use crate::atlas::{AtlasOp, Page};
use crate::{PxRect, RenderError};

/// Bytes per instance; matches `encode::Inst` (asserted there).
const INSTANCE_SIZE: u64 = size_of::<crate::encode::Inst>() as u64;

/// Uniform slot payload: pane origin plus target size.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Uniforms {
    origin: [f32; 2],
    screen: [f32; 2],
}

/// Result of trying to acquire the next surface texture.
pub(crate) enum Acquired {
    /// A texture; the bool is `true` when the surface reported `Suboptimal`.
    Texture(wgpu::SurfaceTexture, bool),
    /// The surface timed out or is occluded; keep damage state.
    Skip,
    /// The surface is gone for now.
    Unavailable,
    /// A validation error escaped the surface.
    Validation,
}

/// GPU state shared by the surface and headless paths.
pub(crate) struct Gpu {
    instance: wgpu::Instance,
    surface: Option<wgpu::Surface<'static>>,
    adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    config: Option<wgpu::SurfaceConfiguration>,
    render_format: wgpu::TextureFormat,
    target_size: (u32, u32),
    suspended: bool,
    offscreen: Option<wgpu::Texture>,
    pipeline: wgpu::RenderPipeline,
    uniform_bgl: wgpu::BindGroupLayout,
    tex_bgl: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
    uniform_bg: wgpu::BindGroup,
    uniform_slots: usize,
    slot_stride: u32,
    mask_tex: wgpu::Texture,
    color_tex: wgpu::Texture,
    mask_size: u32,
    color_size: u32,
    tex_bg: wgpu::BindGroup,
    device_lost: Arc<AtomicBool>,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu")
            .field("target_size", &self.target_size)
            .field("render_format", &self.render_format)
            .field("suspended", &self.suspended)
            .field("uniform_slots", &self.uniform_slots)
            .finish_non_exhaustive()
    }
}

fn default_backends() -> wgpu::Backends {
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
    #[cfg(not(target_os = "macos"))]
    {
        wgpu::Backends::VULKAN
    }
}

fn poll_once<F: Future>(future: F) -> Result<F::Output, RenderError> {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => Ok(value),
        Poll::Pending => Err(RenderError::Device("adapter future pending".into())),
    }
}

/// Instance, adapter and device created without a surface, so it can run on a thread before
/// the window exists (D-T08-5).
#[derive(Debug)]
pub(crate) struct Started {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl Started {
    /// Creates the instance and picks an adapter and device with no surface constraint.
    pub(crate) fn request() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or(default_backends()),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let (adapter, device, queue) = request_adapter_device(&instance, None)?;
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
        })
    }
}

impl Gpu {
    /// Surface-backed GPU state for a window of `size_px`, on a device from `Started::request`.
    pub(crate) fn new_surface(
        dev: Started,
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        size_px: (u32, u32),
    ) -> Result<Self, RenderError> {
        let Started {
            instance,
            mut adapter,
            mut device,
            mut queue,
        } = dev;
        let surface = instance
            .create_surface(target)
            .map_err(|e| RenderError::Device(e.to_string()))?;
        if !adapter.is_surface_supported(&surface) {
            // The surfaceless pick cannot present here (e.g. a multi-GPU machine): pick again
            // with the surface; costs a second device creation on those machines only.
            (adapter, device, queue) = request_adapter_device(&instance, Some(&surface))?;
        }

        let caps = surface.get_capabilities(&adapter);
        let (config_format, view_formats, render_format) = choose_surface_format(&adapter, &caps);
        let width = size_px.0.max(1);
        let height = size_px.1.max(1);
        let suspended = size_px.0 == 0 || size_px.1 == 0;
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: config_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats,
        };

        let gpu = Self::assemble(
            instance,
            Some(surface),
            adapter,
            device,
            queue,
            Some(config),
            render_format,
            size_px,
            suspended,
        );
        gpu.reconfigure();
        Ok(gpu)
    }

    /// Headless GPU state drawing into an offscreen `Rgba8Unorm` texture.
    pub(crate) fn new_headless(size_px: (u32, u32)) -> Result<Self, RenderError> {
        let Started {
            instance,
            adapter,
            device,
            queue,
        } = Started::request()?;
        let suspended = size_px.0 == 0 || size_px.1 == 0;
        let mut gpu = Self::assemble(
            instance,
            None,
            adapter,
            device,
            queue,
            None,
            wgpu::TextureFormat::Rgba8Unorm,
            size_px,
            suspended,
        );
        gpu.recreate_offscreen();
        Ok(gpu)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one-shot constructor mirroring the field set"
    )]
    fn assemble(
        instance: wgpu::Instance,
        surface: Option<wgpu::Surface<'static>>,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: Option<wgpu::SurfaceConfiguration>,
        render_format: wgpu::TextureFormat,
        target_size: (u32, u32),
        suspended: bool,
    ) -> Self {
        let device_lost = Arc::new(AtomicBool::new(false));
        let flag = device_lost.clone();
        device.set_device_lost_callback(move |_reason, _message| {
            flag.store(true, Ordering::Relaxed);
        });

        let slot_stride = uniform_stride(&device);
        let (uniform_bgl, tex_bgl) = create_layouts(&device);
        let pipeline = create_pipeline(&device, render_format, &uniform_bgl, &tex_bgl);
        let uniform_slots = 1;
        let uniforms = create_uniform_buffer(&device, slot_stride);
        let uniform_bg = create_uniform_bind_group(&device, &uniform_bgl, &uniforms);

        let init = device.limits().max_texture_dimension_2d.clamp(1, 1024);
        let mask_tex = create_atlas_texture(&device, Page::Mask, init);
        let color_tex = create_atlas_texture(&device, Page::Color, init);
        let tex_bg = create_texture_bind_group(&device, &tex_bgl, &mask_tex, &color_tex);

        Self {
            instance,
            surface,
            adapter,
            device,
            queue,
            config,
            render_format,
            target_size,
            suspended,
            offscreen: None,
            pipeline,
            uniform_bgl,
            tex_bgl,
            uniforms,
            uniform_bg,
            uniform_slots,
            slot_stride,
            mask_tex,
            color_tex,
            mask_size: init,
            color_size: init,
            tex_bg,
            device_lost,
        }
    }

    /// True (and cleared) when the device-lost callback fired since the last check.
    pub(crate) fn take_device_lost(&self) -> bool {
        self.device_lost.swap(false, Ordering::Relaxed)
    }

    /// Rebuilds the adapter, device, pipeline, buffers and atlas textures on the same instance
    /// and surface. `mask_size`/`color_size` are the atlas page sizes to allocate.
    pub(crate) fn rebuild(&mut self, mask_size: u32, color_size: u32) -> Result<(), RenderError> {
        let surface = self.surface.take();
        let result = request_adapter_device(&self.instance, surface.as_ref());
        self.surface = surface;
        let (adapter, device, queue) = result?;

        let device_lost = Arc::new(AtomicBool::new(false));
        let flag = device_lost.clone();
        device.set_device_lost_callback(move |_reason, _message| {
            flag.store(true, Ordering::Relaxed);
        });

        let slot_stride = uniform_stride(&device);
        let (uniform_bgl, tex_bgl) = create_layouts(&device);
        let pipeline = create_pipeline(&device, self.render_format, &uniform_bgl, &tex_bgl);
        let uniforms = create_uniform_buffer(&device, slot_stride);
        let uniform_bg = create_uniform_bind_group(&device, &uniform_bgl, &uniforms);

        let mask_size = mask_size.max(1);
        let color_size = color_size.max(1);
        let mask_tex = create_atlas_texture(&device, Page::Mask, mask_size);
        let color_tex = create_atlas_texture(&device, Page::Color, color_size);
        let tex_bg = create_texture_bind_group(&device, &tex_bgl, &mask_tex, &color_tex);

        self.adapter = adapter;
        self.device = device;
        self.queue = queue;
        self.device_lost = device_lost;
        self.pipeline = pipeline;
        self.uniform_bgl = uniform_bgl;
        self.tex_bgl = tex_bgl;
        self.uniforms = uniforms;
        self.uniform_bg = uniform_bg;
        self.uniform_slots = 1;
        self.slot_stride = slot_stride;
        self.mask_tex = mask_tex;
        self.color_tex = color_tex;
        self.mask_size = mask_size;
        self.color_size = color_size;
        self.tex_bg = tex_bg;
        self.offscreen = None;

        self.reconfigure();
        self.recreate_offscreen();
        Ok(())
    }

    /// Target edge in device pixels.
    pub(crate) fn target_size(&self) -> (u32, u32) {
        self.target_size
    }

    /// Render target format (non-sRGB unless the platform offers nothing else).
    pub(crate) fn render_format(&self) -> wgpu::TextureFormat {
        self.render_format
    }

    /// True when rendering must not touch the target (zero size).
    pub(crate) fn suspended(&self) -> bool {
        self.suspended
    }

    /// True when this GPU has a surface (the non-headless path).
    pub(crate) fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// The device's maximum 2D texture dimension.
    pub(crate) fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// New target size; zero in either dimension suspends rendering.
    pub(crate) fn resize(&mut self, size_px: (u32, u32)) {
        self.target_size = size_px;
        self.suspended = size_px.0 == 0 || size_px.1 == 0;
        if self.suspended {
            return;
        }
        if let Some(config) = &mut self.config {
            config.width = size_px.0;
            config.height = size_px.1;
            self.reconfigure();
        }
        self.recreate_offscreen();
    }

    fn reconfigure(&self) {
        if self.suspended {
            return;
        }
        if let (Some(surface), Some(config)) = (&self.surface, &self.config)
            && config.width > 0
            && config.height > 0
        {
            surface.configure(&self.device, config);
        }
    }

    fn recreate_offscreen(&mut self) {
        if self.surface.is_some() {
            return;
        }
        let (w, h) = self.target_size;
        if w == 0 || h == 0 {
            self.offscreen = None;
            return;
        }
        self.offscreen = Some(create_render_target(
            &self.device,
            (w, h),
            self.render_format,
        ));
    }

    /// The offscreen render target view, if headless and sized.
    pub(crate) fn offscreen_view(&self) -> Option<wgpu::TextureView> {
        self.offscreen.as_ref().map(|texture| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.render_format),
                ..wgpu::TextureViewDescriptor::default()
            })
        })
    }

    /// Creates a host-readable copy destination for `bytes`.
    pub(crate) fn create_readback_buffer(&self, bytes: u64) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nt-render readback"),
            size: bytes.max(4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Records a copy from the offscreen target into `destination`.
    pub(crate) fn copy_offscreen(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        destination: &wgpu::Buffer,
        padded_bytes_per_row: u32,
        size: (u32, u32),
    ) {
        let Some(texture) = &self.offscreen else {
            return;
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: destination,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(size.1),
                },
            },
            wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Maps `buffer` for reading and returns its bytes.
    pub(crate) fn read_buffer(&self, buffer: &wgpu::Buffer) -> Result<Vec<u8>, RenderError> {
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let mapped = slice
            .get_mapped_range()
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let data = mapped.to_vec();
        drop(mapped);
        buffer.unmap();
        Ok(data)
    }

    /// Acquires the next surface texture, reconfiguring once on `Outdated`/`Lost`.
    pub(crate) fn acquire(&mut self) -> Acquired {
        let Some(surface) = &self.surface else {
            return Acquired::Unavailable;
        };
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => Acquired::Texture(texture, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => Acquired::Texture(texture, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Acquired::Skip
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.reconfigure();
                match surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(texture) => {
                        Acquired::Texture(texture, false)
                    }
                    wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                        Acquired::Texture(texture, true)
                    }
                    _ => Acquired::Unavailable,
                }
            }
            wgpu::CurrentSurfaceTexture::Validation => Acquired::Validation,
        }
    }

    /// Presents a previously acquired surface texture.
    pub(crate) fn present(&self, texture: wgpu::SurfaceTexture) {
        self.queue.present(texture);
    }

    /// Creates an instance-step vertex buffer.
    pub(crate) fn create_inst_buffer(&self, size: u64) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nt-render instances"),
            size: size.max(INSTANCE_SIZE),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Queues `data` into `buffer` at `offset`.
    pub(crate) fn write_buffer(&self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        if !data.is_empty() {
            self.queue.write_buffer(buffer, offset, data);
        }
    }

    /// Makes sure at least `slots` dynamic-uniform slots exist, growing the buffer if needed.
    pub(crate) fn ensure_slots(&mut self, slots: usize) {
        if slots <= self.uniform_slots {
            return;
        }
        let mut new_slots = self.uniform_slots.max(1);
        while new_slots < slots {
            new_slots *= 2;
        }
        self.uniforms = create_uniform_buffer_capacity(&self.device, self.slot_stride, new_slots);
        self.uniform_bg =
            create_uniform_bind_group(&self.device, &self.uniform_bgl, &self.uniforms);
        self.uniform_slots = new_slots;
    }

    /// Writes one uniform slot.
    pub(crate) fn write_slot(&self, slot: usize, origin: [f32; 2], screen: [f32; 2]) {
        let uniforms = Uniforms { origin, screen };
        let bytes = bytemuck::bytes_of(&uniforms);
        self.queue.write_buffer(
            &self.uniforms,
            slot as u64 * u64::from(self.slot_stride),
            bytes,
        );
    }

    /// Replays one pending atlas operation.
    pub(crate) fn apply_atlas(&mut self, op: &AtlasOp<'_>) {
        match op {
            AtlasOp::Grow { page, size } => self.grow_page(*page, *size),
            AtlasOp::Upload {
                page,
                x,
                y,
                w,
                h,
                data,
            } => {
                let (page, x, y, w, h) = (*page, *x, *y, *w, *h);
                let texture = match page {
                    Page::Mask => &self.mask_tex,
                    Page::Color => &self.color_tex,
                };
                let bytes_per_row = w * page.bpp() as u32;
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x, y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(bytes_per_row),
                        rows_per_image: Some(h),
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
    }

    fn grow_page(&mut self, page: Page, new_size: u32) {
        let new_size = new_size.max(1);
        let old_size = match page {
            Page::Mask => self.mask_size,
            Page::Color => self.color_size,
        };
        let new_tex = create_atlas_texture(&self.device, page, new_size);
        let old_tex = match page {
            Page::Mask => &self.mask_tex,
            Page::Color => &self.color_tex,
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nt-render atlas grow"),
            });
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: old_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &new_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: old_size.min(new_size),
                height: old_size.min(new_size),
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        match page {
            Page::Mask => {
                self.mask_tex = new_tex;
                self.mask_size = new_size;
            }
            Page::Color => {
                self.color_tex = new_tex;
                self.color_size = new_size;
            }
        }
        self.tex_bg =
            create_texture_bind_group(&self.device, &self.tex_bgl, &self.mask_tex, &self.color_tex);
    }

    /// Encodes the clear plus every pane and the UI into one render pass.
    #[allow(clippy::too_many_arguments, reason = "single draw entry point")]
    pub(crate) fn encode_draw<'a>(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: (u32, u32),
        panes: &[(u64, usize, PxRect)],
        buffer_for: impl Fn(u64) -> Option<&'a wgpu::Buffer>,
        ui: Option<(&wgpu::Buffer, usize)>,
        clear: Rgba,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("nt-render"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color(clear)),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..wgpu::RenderPassDescriptor::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(1, &self.tex_bg, &[]);

        for (slot, &(id, len, viewport)) in panes.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let Some(buffer) = buffer_for(id) else {
                continue;
            };
            let Some((x, y, w, h)) = clip(viewport, size) else {
                continue;
            };
            pass.set_bind_group(0, &self.uniform_bg, &[slot as u32 * self.slot_stride]);
            pass.set_scissor_rect(x, y, w, h);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..6, 0..len as u32);
        }

        if let Some((buffer, len)) = ui
            && len > 0
        {
            pass.set_bind_group(
                0,
                &self.uniform_bg,
                &[panes.len() as u32 * self.slot_stride],
            );
            pass.set_scissor_rect(0, 0, size.0, size.1);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..6, 0..len as u32);
        }
    }
}

fn request_adapter_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), RenderError> {
    let adapter = poll_once(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: surface,
        apply_limit_buckets: false,
    }))?
    .map_err(|_| RenderError::NoAdapter)?;

    let (device, queue) = poll_once(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("nt-render"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        ..wgpu::DeviceDescriptor::default()
    }))?
    .map_err(|e| RenderError::Device(e.to_string()))?;
    Ok((adapter, device, queue))
}

fn choose_surface_format(
    adapter: &wgpu::Adapter,
    caps: &wgpu::SurfaceCapabilities,
) -> (
    wgpu::TextureFormat,
    Vec<wgpu::TextureFormat>,
    wgpu::TextureFormat,
) {
    let preferred = caps.formats.iter().copied().find(|format| {
        !format.is_srgb()
            && matches!(
                format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
            )
    });
    if let Some(format) = preferred.or_else(|| caps.formats.iter().copied().find(|f| !f.is_srgb()))
    {
        return (format, Vec::new(), format);
    }

    let srgb = caps
        .formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
    let view_formats_supported = adapter
        .get_downlevel_capabilities()
        .flags
        .contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS);
    if view_formats_supported {
        let format = srgb.remove_srgb_suffix();
        (srgb, vec![format], format)
    } else {
        // shortcut: no non-sRGB format and no view reinterpretation: blending happens in
        // linear space (thinner light-on-dark text). No known Vulkan/Metal target hits this.
        (srgb, Vec::new(), srgb)
    }
}

fn uniform_stride(device: &wgpu::Device) -> u32 {
    let alignment = device.limits().min_uniform_buffer_offset_alignment.max(1);
    let size = 16_u32;
    size.div_ceil(alignment) * alignment
}

fn create_layouts(device: &wgpu::Device) -> (wgpu::BindGroupLayout, wgpu::BindGroupLayout) {
    let uniform = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("nt-render uniforms"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: NonZeroU64::new(16),
            },
            count: None,
        }],
    });
    let textures = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("nt-render atlas"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
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
    (uniform, textures)
}

// shortcut: grayscale AA only (no dual-source subpixel blending); revisit if Linux users ask.
fn create_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    uniform_bgl: &wgpu::BindGroupLayout,
    tex_bgl: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::include_wgsl!("shaders/quad.wgsl"));
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("nt-render"),
        bind_group_layouts: &[Some(uniform_bgl), Some(tex_bgl)],
        immediate_size: 0,
    });
    let blend = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    };
    let attributes = wgpu::vertex_attr_array![
        0 => Sint16x2,
        1 => Uint16x4,
        2 => Unorm8x4,
        3 => Uint32,
    ];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("nt-render quads"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: INSTANCE_SIZE,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &attributes,
            })],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..wgpu::PrimitiveState::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState {
                    color: blend,
                    alpha: blend,
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_uniform_buffer(device: &wgpu::Device, stride: u32) -> wgpu::Buffer {
    create_uniform_buffer_capacity(device, stride, 1)
}

fn create_uniform_buffer_capacity(
    device: &wgpu::Device,
    stride: u32,
    slots: usize,
) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nt-render uniform slots"),
        size: u64::from(stride) * slots as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_uniform_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("nt-render uniforms"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: NonZeroU64::new(16),
            }),
        }],
    })
}

fn create_atlas_texture(device: &wgpu::Device, page: Page, size: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("nt-render atlas"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: match page {
            Page::Mask => wgpu::TextureFormat::R8Unorm,
            Page::Color => wgpu::TextureFormat::Rgba8Unorm,
        },
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn create_texture_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    mask: &wgpu::Texture,
    color: &wgpu::Texture,
) -> wgpu::BindGroup {
    let mask_view = mask.create_view(&wgpu::TextureViewDescriptor::default());
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("nt-render atlas"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&mask_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&color_view),
            },
        ],
    })
}

fn create_render_target(
    device: &wgpu::Device,
    size: (u32, u32),
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("nt-render offscreen"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn clip(viewport: PxRect, size: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let x1 = viewport.x.saturating_add(viewport.w).min(size.0);
    let y1 = viewport.y.saturating_add(viewport.h).min(size.1);
    if x1 <= viewport.x || y1 <= viewport.y {
        None
    } else {
        Some((viewport.x, viewport.y, x1 - viewport.x, y1 - viewport.y))
    }
}

fn clear_color(clear: Rgba) -> wgpu::Color {
    let [r, g, b, a] = clear.0;
    let alpha = f64::from(a) / 255.0;
    let premultiply = |channel: u8| f64::from(channel) / 255.0 * alpha;
    wgpu::Color {
        r: premultiply(r),
        g: premultiply(g),
        b: premultiply(b),
        a: alpha,
    }
}
