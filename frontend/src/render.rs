use crate::FoldHashMap;
use crate::state::RgbExt;
use bytemuck::{Pod, Zeroable};
use std::cell::{Cell, RefCell};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use web_sys::HtmlCanvasElement;
use wgpu::util::DeviceExt;

use walloftext_shared::{
    BOARD_HALF, CHUNK_H, CHUNK_W, CHUNK_W_USIZE, ChunkCoords, FontAtlasFile, WorldCoords,
};

use crate::chunks::{MAX_FLUSH, refresh_chunks};
use crate::dom::{El, debug_log, document, perf, window};
use crate::state::{
    CANVAS_DIRTY, DevicePt, DeviceRect, DeviceSize, mark_canvas_dirty, read_board, read_ui,
    read_vp, with_board, with_vp,
};

pub const LOD_GLYPH_THRESHOLD: f64 = 5.0;

const ATLAS_URL: &str = "/unifont.wtfont";

thread_local! {
    pub static WGPU_STATE: RefCell<Option<WgpuState>> = const { RefCell::new(None) };
    pub static RAF_PENDING: Cell<bool> = const { Cell::new(false) };
    pub static MOMENTUM:    Cell<bool> = const { Cell::new(false) };
    static INSTANCE_DATA:  RefCell<Vec<Instance>>         = const { RefCell::new(Vec::new()) };
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    grid_pos: [f32; 2],
    color: [f32; 3],
    atlas_xy: [f32; 2],
    glyph_wh: [f32; 2],
    cell_off: [f32; 2],
    extra_scale: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CellUniforms {
    cw: f32,
    ch: f32,
    vx: f32,
    vy: f32,
    dpr: f32,
    step: f32,
    canvas_w: f32,
    canvas_h: f32,
    atlas_cell_w: f32,
    atlas_cell_h: f32,
    atlas_w: f32,
    atlas_h: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DotUniforms {
    cw: f32,
    ch: f32,
    vx: f32,
    vy: f32,
    dpr: f32,
    step: f32,
    canvas_w: f32,
    canvas_h: f32,
    alpha: f32,
    _pad: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct UIRectInstance {
    screen_xy: [f32; 2],
    screen_wh: [f32; 2],
    fill_rgba: [f32; 4],
    border_rgba: [f32; 4],
    border_px: f32,
    dash_px: f32,
    flags: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct UIRectUniforms {
    canvas_w: f32,
    canvas_h: f32,
    dpr: f32,
    blink_on: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct UITextUniforms {
    canvas_w: f32,
    canvas_h: f32,
    dpr: f32,
    ui_scale: f32,
    atlas_w: f32,
    atlas_h: f32,
    _pad: [f32; 2],
}

const CELL_WGSL: &str = include_str!("shaders/cell.wgsl");
const DOT_WGSL: &str = include_str!("shaders/dot.wgsl");
const UI_RECT_WGSL: &str = include_str!("shaders/ui_rect.wgsl");
const UI_TEXT_WGSL: &str = include_str!("shaders/ui_text.wgsl");

pub struct WgpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    max_tex_dim: u32,
    quad_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_cap: usize,
    cell_pipeline: wgpu::RenderPipeline,
    cell_uniform_buf: wgpu::Buffer,
    atlas: Option<GlyphAtlas>,
    dot_pipeline: wgpu::RenderPipeline,
    dot_uniform_buf: wgpu::Buffer,
    dot_bind_group: wgpu::BindGroup,
    ui_rect_pipeline: wgpu::RenderPipeline,
    ui_rect_uniform_buf: wgpu::Buffer,
    ui_rect_bind_group: wgpu::BindGroup,
    ui_rect_buf: wgpu::Buffer,
    ui_rect_cap: usize,
    ui_text_pipeline: wgpu::RenderPipeline,
    ui_text_uniform_buf: wgpu::Buffer,
    ui_text_bind_group: Option<wgpu::BindGroup>,
    ui_text_buf: wgpu::Buffer,
    ui_text_cap: usize,
    pub canvas: HtmlCanvasElement,
    surface_w: u32,
    surface_h: u32,
    pub chunk_bufs: FoldHashMap<ChunkCoords, (wgpu::Buffer, u32, u32)>,
}

struct GlyphAtlas {
    pub font_file: FontAtlasFile,
    bind_group: wgpu::BindGroup,
    texture_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

impl GlyphAtlas {
    pub fn from_file(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        cell_bgl: &wgpu::BindGroupLayout,
        cell_uniform_buf: &wgpu::Buffer,
        f: FontAtlasFile,
    ) -> Self {
        let mut pixels_8bit = vec![0u8; (f.atlas_w as usize) * (f.atlas_h as usize)];
        for (i, &byte) in f.pixels_1bit.iter().enumerate() {
            for bit in 0..8 {
                if (byte >> bit) & 1 == 1 {
                    pixels_8bit[i * 8 + bit] = 255;
                }
            }
        }

        let atlas_w = f.atlas_w as u32;
        let atlas_h = f.atlas_h as u32;
        let mip_count = (atlas_w.max(atlas_h) as f32).log2().floor() as u32 + 1;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GlyphAtlas"),
            size: wgpu::Extent3d {
                width: atlas_w,
                height: atlas_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels_8bit,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas_w),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: atlas_w,
                height: atlas_h,
                depth_or_array_layers: 1,
            },
        );

        let texture_view = texture.create_view(&Default::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Atlas Sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: cell_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: cell_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        Self {
            font_file: f,
            bind_group,
            texture_view,
            sampler,
        }
    }
    #[inline]
    pub fn get_idx(&self, cp: u32) -> Option<usize> {
        self.font_file.codepoints.binary_search(&cp).ok()
    }
}

async fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window()?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| web_sys::console::error_1(&format!("fetch {url}: {:?}", e).into()))
        .ok()?
        .dyn_into()
        .ok()?;

    if !resp.ok() {
        web_sys::console::error_1(&format!("fetch {url}: HTTP {}", resp.status()).into());
        return None;
    }

    let buf = JsFuture::from(resp.array_buffer().ok()?).await.ok()?;
    Some(js_sys::Uint8Array::new(&buf).to_vec())
}

#[derive(Debug)]
pub struct WebDisplay;

impl wgpu::rwh::HasDisplayHandle for WebDisplay {
    fn display_handle(&self) -> Result<wgpu::rwh::DisplayHandle<'_>, wgpu::rwh::HandleError> {
        let raw = wgpu::rwh::WebDisplayHandle::new();
        Ok(unsafe { wgpu::rwh::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct WebWindow {
    display: WebDisplay,
}

impl wgpu::rwh::HasDisplayHandle for WebWindow {
    fn display_handle(&self) -> Result<wgpu::rwh::DisplayHandle<'_>, wgpu::rwh::HandleError> {
        self.display.display_handle()
    }
}

impl wgpu::rwh::HasWindowHandle for WebWindow {
    fn window_handle(&self) -> Result<wgpu::rwh::WindowHandle<'_>, wgpu::rwh::HandleError> {
        let raw = wgpu::rwh::WebWindowHandle::new(0);
        Ok(unsafe { wgpu::rwh::WindowHandle::borrow_raw(raw.into()) })
    }
}

async fn try_request_device(
    instance: &wgpu::Instance,
    compatible_surface: Option<&wgpu::Surface<'_>>,
) -> anyhow::Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await?;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                .using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::default(),
        })
        .await?;

    Ok((adapter, device, queue))
}

async fn acquire_gpu(
    #[cfg(not(windows))] canvas: &HtmlCanvasElement,
) -> anyhow::Result<(
    wgpu::Instance,
    wgpu::Surface<'static>,
    wgpu::Adapter,
    wgpu::Device,
    wgpu::Queue,
)> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.display = Some(Box::new(WebDisplay));
    let instance = wgpu::util::new_instance_with_webgpu_detection(desc).await;
    #[cfg(not(windows))]
    let surface = instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))?;
    #[cfg(windows)]
    let web_window = WebWindow {
        display: WebDisplay,
    };
    #[cfg(windows)]
    let surface = instance.create_surface(wgpu::SurfaceTarget::Window(Box::new(web_window)))?;
    let (adapter, device, queue) = try_request_device(&instance, Some(&surface)).await?;
    Ok((instance, surface, adapter, device, queue))
}

pub async fn init_wgpu() -> Option<WgpuState> {
    debug_log!("init_wgpu starting");
    macro_rules! step {
        ($label:expr, $e:expr) => {
            match $e {
                Some(v) => v,
                None => {
                    web_sys::console::error_1(&format!("wgpu init: {} failed", $label).into());
                    return None;
                }
            }
        };
        (res $label:expr, $e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("wgpu init: {} failed: {:?}", $label, e).into(),
                    );
                    return None;
                }
            }
        };
    }

    let canvas: HtmlCanvasElement = step!("get canvas", document().get_element_by_id("cv-gl"))
        .dyn_into()
        .ok()?;
    let width = canvas.width().max(1);
    let height = canvas.height().max(1);

    let (_instance, surface, adapter, device, queue) = step!(
        res "acquire_gpu",
        acquire_gpu(
            #[cfg(not(windows))]
            &canvas,
        ).await
    );

    let max_tex_dim = device.limits().max_texture_dimension_2d;
    let caps = surface.get_capabilities(&adapter);
    let format = if adapter.get_info().backend == wgpu::Backend::Gl {
        wgpu::TextureFormat::Rgba8Unorm
    } else {
        wgpu::TextureFormat::Bgra8Unorm
    };
    if !caps.formats.contains(&format) {
        return None;
    }

    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let quad_verts: [f32; 12] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
    let quad_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&quad_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    const INIT_INST_CAP: usize = 16384;
    let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (INIT_INST_CAP * std::mem::size_of::<Instance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let quad_layout = wgpu::VertexBufferLayout {
        array_stride: 8,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[wgpu::VertexAttribute {
            offset: 0,
            shader_location: 0,
            format: wgpu::VertexFormat::Float32x2,
        }],
    };

    let cell_inst_layout = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Instance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &[
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x3,
            },
            wgpu::VertexAttribute {
                offset: 20,
                shader_location: 3,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 28,
                shader_location: 4,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 36,
                shader_location: 5,
                format: wgpu::VertexFormat::Float32x2,
            },
        ],
    };

    let dot_inst_layout = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Instance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &[
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x3,
            },
        ],
    };

    let cell_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(CELL_WGSL.into()),
    });

    let cell_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: std::mem::size_of::<CellUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let cell_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
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
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });

    let cell_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&cell_bgl)],
        immediate_size: 0,
    });

    let cell_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&cell_pipeline_layout),
        vertex: wgpu::VertexState {
            module: &cell_shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(quad_layout.clone()), Some(cell_inst_layout)],
        },
        fragment: Some(wgpu::FragmentState {
            module: &cell_shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let dot_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(DOT_WGSL.into()),
    });

    let dot_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: std::mem::size_of::<DotUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let dot_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let dot_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &dot_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: dot_uniform_buf.as_entire_binding(),
        }],
    });

    let dot_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&dot_bgl)],
        immediate_size: 0,
    });

    let dot_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&dot_pipeline_layout),
        vertex: wgpu::VertexState {
            module: &dot_shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(quad_layout), Some(dot_inst_layout)],
        },
        fragment: Some(wgpu::FragmentState {
            module: &dot_shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let atlas = {
        let compressed = fetch_bytes(ATLAS_URL).await;
        compressed.and_then(|data| {
            let raw = crate::network::zstd_decompress(&data);
            let file: FontAtlasFile = bitcode::decode(&raw)
                .map_err(|e| web_sys::console::error_1(&format!("atlas decode: {e}").into()))
                .ok()?;
            Some(GlyphAtlas::from_file(
                &device,
                &queue,
                &cell_bgl,
                &cell_uniform_buf,
                file,
            ))
        })
    };

    let ui_rect_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("UIRect"),
        source: wgpu::ShaderSource::Wgsl(UI_RECT_WGSL.into()),
    });

    let ui_rect_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("UIRect Uniforms"),
        size: std::mem::size_of::<UIRectUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let ui_rect_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("UIRect BGL"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let ui_rect_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("UIRect BG"),
        layout: &ui_rect_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: ui_rect_uniform_buf.as_entire_binding(),
        }],
    });

    let ui_rect_inst_layout = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<UIRectInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &[
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 16,
                shader_location: 3,
                format: wgpu::VertexFormat::Float32x4,
            },
            wgpu::VertexAttribute {
                offset: 32,
                shader_location: 4,
                format: wgpu::VertexFormat::Float32x4,
            },
            wgpu::VertexAttribute {
                offset: 48,
                shader_location: 5,
                format: wgpu::VertexFormat::Float32,
            },
            wgpu::VertexAttribute {
                offset: 52,
                shader_location: 6,
                format: wgpu::VertexFormat::Float32,
            },
            wgpu::VertexAttribute {
                offset: 56,
                shader_location: 7,
                format: wgpu::VertexFormat::Float32,
            },
        ],
    };

    let ui_rect_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("UIRect Layout"),
        bind_group_layouts: &[Some(&ui_rect_bgl)],
        immediate_size: 0,
    });

    let ui_rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("UIRect"),
        layout: Some(&ui_rect_layout),
        vertex: wgpu::VertexState {
            module: &ui_rect_shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[
                Some(wgpu::VertexBufferLayout {
                    array_stride: 8,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        offset: 0,
                        shader_location: 0,
                        format: wgpu::VertexFormat::Float32x2,
                    }],
                }),
                Some(ui_rect_inst_layout),
            ],
        },
        fragment: Some(wgpu::FragmentState {
            module: &ui_rect_shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    const INIT_UI_CAP: usize = 512;
    let ui_rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("UIRect Instances"),
        size: (INIT_UI_CAP * std::mem::size_of::<UIRectInstance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let ui_text_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("UIText"),
        source: wgpu::ShaderSource::Wgsl(UI_TEXT_WGSL.into()),
    });

    let ui_text_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("UIText Uniforms"),
        size: std::mem::size_of::<UITextUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let ui_text_bind_group = atlas.as_ref().map(|a| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("UIText BG"),
            layout: &cell_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ui_text_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&a.texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&a.sampler),
                },
            ],
        })
    });

    let ui_text_inst_layout = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Instance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &[
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x3,
            },
            wgpu::VertexAttribute {
                offset: 20,
                shader_location: 3,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 28,
                shader_location: 4,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 36,
                shader_location: 5,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 44,
                shader_location: 6,
                format: wgpu::VertexFormat::Float32,
            },
        ],
    };

    let ui_text_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("UIText Layout"),
        bind_group_layouts: &[Some(&cell_bgl)],
        immediate_size: 0,
    });

    let ui_text_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("UIText"),
        layout: Some(&ui_text_layout),
        vertex: wgpu::VertexState {
            module: &ui_text_shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[
                Some(wgpu::VertexBufferLayout {
                    array_stride: 8,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        offset: 0,
                        shader_location: 0,
                        format: wgpu::VertexFormat::Float32x2,
                    }],
                }),
                Some(ui_text_inst_layout),
            ],
        },
        fragment: Some(wgpu::FragmentState {
            module: &ui_text_shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let ui_text_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("UIText Instances"),
        size: (INIT_UI_CAP * std::mem::size_of::<Instance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    Some(WgpuState {
        surface,
        device,
        queue,
        config,
        max_tex_dim,
        quad_buf,
        instance_buf,
        instance_cap: INIT_INST_CAP,
        cell_pipeline,
        cell_uniform_buf,
        atlas,
        dot_pipeline,
        dot_uniform_buf,
        dot_bind_group,
        ui_rect_pipeline,
        ui_rect_uniform_buf,
        ui_rect_bind_group,
        ui_rect_buf,
        ui_rect_cap: INIT_UI_CAP,
        ui_text_pipeline,
        ui_text_uniform_buf,
        ui_text_bind_group,
        ui_text_buf,
        ui_text_cap: INIT_UI_CAP,
        canvas,
        surface_w: width,
        surface_h: height,
        chunk_bufs: FoldHashMap::default(),
    })
}

pub fn build_chunk_buffer(chunk: ChunkCoords) {
    let instances = WGPU_STATE.with(|ws| -> Option<Vec<Instance>> {
        let borrow = ws.borrow();
        let state = borrow.as_ref()?;
        let atlas = state.atlas.as_ref()?;

        read_board(|b| {
            let node = b.chunk_manager.chunks.get(&chunk)?;
            let chunk_data = node.full_data.as_ref()?;

            let mut out = Vec::new();
            for idx in 0..crate::chunks::CHUNK_AREA {
                if !chunk_data.is_occupied(idx) {
                    continue;
                }
                let lx = (idx % CHUNK_W_USIZE) as i64;
                let ly = (idx / CHUNK_W_USIZE) as i64;
                let wx = chunk.x as i64 * CHUNK_W + lx;
                let wy = chunk.y as i64 * CHUNK_H + ly;

                let ch_c = chunk_data.chars[idx];
                let (r, g, b) = chunk_data.colors[idx].to_floats();
                let (atlas_xy, glyph_wh, cell_off) = atlas
                    .get_idx(ch_c as u32)
                    .map(|i| {
                        let f = &atlas.font_file;
                        (
                            [f.atlas_x[i] as f32, f.atlas_y[i] as f32],
                            [f.w[i] as f32, f.h[i] as f32],
                            [f.cell_off_x[i] as f32, f.cell_off_y[i] as f32],
                        )
                    })
                    .unwrap_or(([0.0; 2], [0.0; 2], [0.0; 2]));

                out.push(Instance {
                    grid_pos: [wx as f32, wy as f32],
                    color: [r, g, b],
                    atlas_xy,
                    glyph_wh,
                    cell_off,
                    extra_scale: 1.0,
                });
            }
            if out.is_empty() { None } else { Some(out) }
        })
    });

    WGPU_STATE.with(|ws| {
        let mut borrow = ws.borrow_mut();
        let state = match borrow.as_mut() {
            Some(s) => s,
            None => return,
        };

        match instances {
            None => {
                state.chunk_bufs.remove(&chunk);
            }
            Some(insts) => {
                let count = insts.len() as u32;
                let byte_slice = bytemuck::cast_slice(&insts);
                if let Some((buf, existing_count, cap)) = state.chunk_bufs.get_mut(&chunk)
                    && count <= *cap
                {
                    state.queue.write_buffer(buf, 0, byte_slice);
                    *existing_count = count;
                    return;
                }
                let buf = state
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: None,
                        contents: byte_slice,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                state.chunk_bufs.insert(chunk, (buf, count, count));
            }
        }
    });
}

fn render_cells_wgpu(
    rect_insts: Vec<UIRectInstance>,
    bg_rect_count: usize,
    text_insts: Vec<Instance>,
    blink_on: f32,
    ui_scale: f32,
) {
    let (dpr, zoom, cw, ch, vx, vy, bounds) = read_vp(|vp| {
        (
            vp.dpr,
            vp.zoom,
            vp.cw,
            vp.ch,
            vp.vx,
            vp.vy,
            vp.visible_world_bounds(),
        )
    });
    WGPU_STATE.with(|ws| {
        let mut borrow = match ws.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        let state = match borrow.as_mut() {
            Some(s) => s,
            None => return,
        };

        let canvas_w = state.surface_w;
        let canvas_h = state.surface_h;
        if canvas_w == 0 || canvas_h == 0 {
            return;
        }

        let glyph_mode = cw >= LOD_GLYPH_THRESHOLD;
        let raw_step = (1.0 / zoom).max(1.0) as u32;
        let step = raw_step.next_power_of_two() as i64;
        let cell_w_dev = (cw * dpr).max(1.0) as u32;
        let cell_h_dev = (ch * dpr).max(1.0) as u32;

        let frame = match state.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

        let lod_start = WorldCoords::from_i64(
            (bounds.a.x as i64).div_euclid(step) * step,
            (bounds.a.y as i64).div_euclid(step) * step,
        );
        let lod_end = WorldCoords::from_i64(
            (bounds.b.x as i64).div_euclid(step) * step,
            (bounds.b.y as i64).div_euclid(step) * step,
        );

        INSTANCE_DATA.with(|id| {
            let mut data = id.borrow_mut();
            data.clear();

            let mut flash_count = 0;

            let flash_deny_snap: Vec<(WorldCoords, f64)> = if glyph_mode {
                read_board(|b| b.flash_deny.iter().map(|(&k, &v)| (k, v)).collect())
            } else {
                vec![]
            };
            let screen_rect = DeviceRect {
                x: 0.0,
                y: 0.0,
                w: canvas_w as f32,
                h: canvas_h as f32,
            };
            let cell_size = DeviceSize {
                w: cell_w_dev as f32,
                h: cell_h_dev as f32,
            };

            let cw_dev = (cw * dpr) as f32;
            let ch_dev = (ch * dpr) as f32;
            let vx_dev = (vx * dpr) as f32;
            let vy_dev = (vy * dpr) as f32;
            let w2d = |wc: WorldCoords| -> DevicePt {
                DevicePt {
                    x: wc.x as f32 * cw_dev - vx_dev,
                    y: wc.y as f32 * ch_dev - vy_dev,
                }
            };

            if glyph_mode && !flash_deny_snap.is_empty() {
                let now = perf();
                for (pos, until) in &flash_deny_snap {
                    if now >= *until {
                        continue;
                    }
                    let rect = DeviceRect::from_pt_size(w2d(*pos), cell_size);
                    if screen_rect.intersects(&rect) {
                        data.push(Instance {
                            grid_pos: [pos.x as f32, pos.y as f32],
                            color: [0.78, 0.0, 0.0],
                            atlas_xy: [0.0, 0.0],
                            glyph_wh: [0.0, 0.0],
                            cell_off: [0.0, 0.0],
                            extra_scale: 1.0,
                        });
                        flash_count += 1;
                    }
                }
            }

            let cells_start = flash_count;
            let mut cells_count = 0;

            if !glyph_mode {
                let now = crate::dom::perf();
                with_board(|b| {
                    let start = data.len();
                    for cy in bounds.visible_chunk_rows() {
                        let from_y = (lod_start.y as i64).max(cy * CHUNK_H);
                        let off_y = from_y - lod_start.y as i64;
                        let y_start =
                            (lod_start.y as i64 + ((off_y + step - 1) / step) * step) as i16;
                        let y_end = lod_end.y.min((cy * CHUNK_H + CHUNK_H - 1) as i16);
                        if y_start > y_end {
                            continue;
                        }

                        for cx in bounds.visible_chunk_columns() {
                            let chunk = ChunkCoords {
                                x: cx as i8,
                                y: cy as i8,
                            };
                            let Some(node) = b.chunk_manager.chunks.get_mut(&chunk) else {
                                continue;
                            };
                            node.last_seen = now;
                            let node = &*node;

                            if node.full_data.is_none() && node.lod_data.is_none() {
                                continue;
                            }

                            let from_x = (lod_start.x as i64).max(cx * CHUNK_W);
                            let off_x = from_x - lod_start.x as i64;
                            let x_start =
                                (lod_start.x as i64 + ((off_x + step - 1) / step) * step) as i16;
                            let x_end = lod_end.x.min((cx * CHUNK_W + CHUNK_W - 1) as i16);
                            if x_start > x_end {
                                continue;
                            }

                            data.extend((y_start..=y_end).step_by(step as usize).flat_map(|wy| {
                                let ly = (wy as i64 - cy * CHUNK_H) as usize;
                                (x_start..=x_end)
                                    .step_by(step as usize)
                                    .filter_map(move |wx| {
                                        let idx = ly * CHUNK_W_USIZE
                                            + (wx as i64 - cx * CHUNK_W) as usize;
                                        let rgb = node.lod_color_at(idx)?;
                                        let (r, g, c) = rgb.to_floats();
                                        Some(Instance {
                                            grid_pos: [wx as f32, wy as f32],
                                            color: [r, g, c],
                                            atlas_xy: [0.0, 0.0],
                                            glyph_wh: [0.0, 0.0],
                                            cell_off: [0.0, 0.0],
                                            extra_scale: 1.0,
                                        })
                                    })
                            }));
                        }
                    }
                    cells_count += data.len() - start;
                });
            }

            let byte_len = (data.len() * std::mem::size_of::<Instance>()) as u64;
            if !data.is_empty() {
                if data.len() > state.instance_cap {
                    let new_cap = (data.len() * 2).max(data.len() + 1024);
                    state.instance_buf = state.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("Dynamic Instance Buffer"),
                        size: (new_cap * std::mem::size_of::<Instance>()) as u64,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    state.instance_cap = new_cap;
                }
                state
                    .queue
                    .write_buffer(&state.instance_buf, 0, bytemuck::cast_slice(&data));
            }

            if glyph_mode {
                let (full_atlas_w, full_atlas_h, atlas_cell_w, atlas_cell_h) = state
                    .atlas
                    .as_ref()
                    .map(|a| {
                        let f = &a.font_file;
                        (
                            f.atlas_w as f32,
                            f.atlas_h as f32,
                            f.cell_w as f32,
                            f.cell_h as f32,
                        )
                    })
                    .unwrap_or((1.0, 1.0, 16.0, 16.0));
                let uniforms = CellUniforms {
                    cw: cw as f32,
                    ch: ch as f32,
                    vx: vx as f32,
                    vy: vy as f32,
                    dpr: dpr as f32,
                    step: step as f32,
                    canvas_w: canvas_w as f32,
                    canvas_h: canvas_h as f32,
                    atlas_cell_w,
                    atlas_cell_h,
                    atlas_w: full_atlas_w,
                    atlas_h: full_atlas_h,
                };
                state
                    .queue
                    .write_buffer(&state.cell_uniform_buf, 0, bytemuck::bytes_of(&uniforms));
            } else if cells_count > 0 {
                let uniforms = DotUniforms {
                    cw: cw as f32,
                    ch: ch as f32,
                    vx: vx as f32,
                    vy: vy as f32,
                    dpr: dpr as f32,
                    step: step as f32,
                    canvas_w: canvas_w as f32,
                    canvas_h: canvas_h as f32,
                    alpha: 1.0,
                    _pad: [0.0; 3],
                };
                state
                    .queue
                    .write_buffer(&state.dot_uniform_buf, 0, bytemuck::bytes_of(&uniforms));
            }

            if flash_count > 0 {
                let uniforms = DotUniforms {
                    cw: cw as f32,
                    ch: ch as f32,
                    vx: vx as f32,
                    vy: vy as f32,
                    dpr: dpr as f32,
                    step: 1.0,
                    canvas_w: canvas_w as f32,
                    canvas_h: canvas_h as f32,
                    alpha: 0.4,
                    _pad: [0.0; 3],
                };
                state
                    .queue
                    .write_buffer(&state.dot_uniform_buf, 0, bytemuck::bytes_of(&uniforms));
            }

            if !rect_insts.is_empty() {
                if rect_insts.len() > state.ui_rect_cap {
                    let new_cap = rect_insts.len() * 2;
                    state.ui_rect_buf = state.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("UIRect Instances"),
                        size: (new_cap * std::mem::size_of::<UIRectInstance>()) as u64,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    state.ui_rect_cap = new_cap;
                }
                state.queue.write_buffer(
                    &state.ui_rect_buf,
                    0,
                    bytemuck::cast_slice(rect_insts.as_slice()),
                );
                state.queue.write_buffer(
                    &state.ui_rect_uniform_buf,
                    0,
                    bytemuck::bytes_of(&UIRectUniforms {
                        canvas_w: canvas_w as f32,
                        canvas_h: canvas_h as f32,
                        dpr: dpr as f32,
                        blink_on,
                    }),
                );
            }

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Unified Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.04706,
                            g: 0.04706,
                            b: 0.04706,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_vertex_buffer(0, state.quad_buf.slice(..));

            if bg_rect_count > 0 {
                pass.set_pipeline(&state.ui_rect_pipeline);
                pass.set_bind_group(0, &state.ui_rect_bind_group, &[]);
                pass.set_vertex_buffer(1, state.ui_rect_buf.slice(..));
                pass.draw(0..6, 0..bg_rect_count as u32);
                pass.set_vertex_buffer(0, state.quad_buf.slice(..));
            }

            if flash_count > 0 {
                let byte_end = (flash_count * std::mem::size_of::<Instance>()) as u64;
                pass.set_pipeline(&state.dot_pipeline);
                pass.set_bind_group(0, &state.dot_bind_group, &[]);
                pass.set_vertex_buffer(1, state.instance_buf.slice(0..byte_end));
                pass.draw(0..6, 0..flash_count as u32);
            }

            if glyph_mode {
                if let Some(atlas) = &state.atlas {
                    pass.set_pipeline(&state.cell_pipeline);
                    pass.set_bind_group(0, &atlas.bind_group, &[]);

                    let c_start = ChunkCoords::from_world(bounds.a);
                    let c_end = ChunkCoords::from_world(bounds.b);

                    let now = crate::dom::perf();
                    for cy in c_start.y..=c_end.y {
                        for cx in c_start.x..=c_end.x {
                            let coord = ChunkCoords { x: cx, y: cy };
                            if let Some((buf, count, _)) = state.chunk_bufs.get(&coord) {
                                pass.set_vertex_buffer(1, buf.slice(..));
                                pass.draw(0..6, 0..*count);
                            }
                            with_board(|b| {
                                if let Some(node) = b.chunk_manager.chunks.get_mut(&coord) {
                                    node.last_seen = now;
                                }
                            });
                        }
                    }
                }
            } else if cells_count > 0 {
                let byte_offset = (cells_start * std::mem::size_of::<Instance>()) as u64;
                let byte_end = byte_len;
                pass.set_pipeline(&state.dot_pipeline);
                pass.set_bind_group(0, &state.dot_bind_group, &[]);
                pass.set_vertex_buffer(1, state.instance_buf.slice(byte_offset..byte_end));
                pass.draw(0..6, 0..cells_count as u32);
            }

            let fg_rect_count = rect_insts.len() - bg_rect_count;
            if fg_rect_count > 0 {
                pass.set_pipeline(&state.ui_rect_pipeline);
                pass.set_bind_group(0, &state.ui_rect_bind_group, &[]);
                pass.set_vertex_buffer(0, state.quad_buf.slice(..));
                pass.set_vertex_buffer(1, state.ui_rect_buf.slice(..));
                pass.draw(0..6, bg_rect_count as u32..rect_insts.len() as u32);
            }

            if !text_insts.is_empty()
                && let Some(ui_text_bg) = &state.ui_text_bind_group
            {
                if text_insts.len() > state.ui_text_cap {
                    let new_cap = text_insts.len() * 2;
                    state.ui_text_buf = state.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("UIText Instances"),
                        size: (new_cap * std::mem::size_of::<Instance>()) as u64,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    state.ui_text_cap = new_cap;
                }
                state.queue.write_buffer(
                    &state.ui_text_buf,
                    0,
                    bytemuck::cast_slice(text_insts.as_slice()),
                );
                let (full_atlas_w, full_atlas_h) = state
                    .atlas
                    .as_ref()
                    .map(|a| (a.font_file.atlas_w as f32, a.font_file.atlas_h as f32))
                    .unwrap_or((1.0, 1.0));

                let uniforms = UITextUniforms {
                    canvas_w: canvas_w as f32,
                    canvas_h: canvas_h as f32,
                    dpr: dpr as f32,
                    ui_scale,
                    atlas_w: full_atlas_w,
                    atlas_h: full_atlas_h,
                    _pad: [0.0; 2],
                };
                state.queue.write_buffer(
                    &state.ui_text_uniform_buf,
                    0,
                    bytemuck::bytes_of(&uniforms),
                );
                pass.set_pipeline(&state.ui_text_pipeline);
                pass.set_bind_group(0, ui_text_bg, &[]);
                pass.set_vertex_buffer(0, state.quad_buf.slice(..));
                pass.set_vertex_buffer(1, state.ui_text_buf.slice(..));
                pass.draw(0..6, 0..text_insts.len() as u32);
            }

            drop(pass);
            state.queue.submit(std::iter::once(encoder.finish()));
            state.queue.present(frame);
        });
    });
}

fn emit_glyphs(
    pos: DevicePt,
    s: &str,
    color: [f32; 3],
    f: &FontAtlasFile,
    cell_w_dev: f32,
    extra_scale: f32,
    out: &mut Vec<Instance>,
) {
    let mut current_x = pos.x;
    let scaled_advance_width = cell_w_dev * extra_scale;

    for ch in s.chars() {
        if let Ok(i) = f.codepoints.binary_search(&(ch as u32)) {
            out.push(Instance {
                grid_pos: [current_x, pos.y],
                color,
                atlas_xy: [f.atlas_x[i] as f32, f.atlas_y[i] as f32],
                glyph_wh: [f.w[i] as f32, f.h[i] as f32],
                cell_off: [f.cell_off_x[i] as f32, f.cell_off_y[i] as f32],
                extra_scale,
            });
        }
        current_x += scaled_advance_width;
    }
}

pub fn render_frame() {
    let (w, h, dpr, vx, vy, cw, ch, zoom, bounds) = read_vp(|vp| {
        (
            vp.log_w(),
            vp.log_h(),
            vp.dpr,
            vp.vx,
            vp.vy,
            vp.cw,
            vp.ch,
            vp.zoom,
            vp.visible_world_bounds(),
        )
    });

    let blink_on = if ((perf() / 500.0) as u64).is_multiple_of(2) {
        1.0_f32
    } else {
        0.0_f32
    };
    let show_detail = cw >= LOD_GLYPH_THRESHOLD;

    let (cur_pos, color, sel_active, sel) = read_ui(|ui| {
        (
            ui.cursor.pos,
            ui.color,
            ui.cursor.sel.active,
            ui.cursor.sel.rect(),
        )
    });

    let regions = read_board(|b| b.regions.values().cloned().collect::<Vec<_>>());
    let my_id = read_ui(|ui| ui.my_id);
    let remote = read_board(|b| {
        b.remote_cursors
            .iter()
            .map(|(&uid, &(pos, color))| (b.author_name(uid), pos, color))
            .collect::<Vec<_>>()
    });

    let dpr_f = dpr as f32;
    let w_dev = w as f32 * dpr_f;
    let h_dev = h as f32 * dpr_f;
    let cw_dev = cw as f32 * dpr_f;
    let ch_dev = ch as f32 * dpr_f;
    let vx_dev = vx as f32 * dpr_f;
    let vy_dev = vy as f32 * dpr_f;

    let base_font_px = 9.0;
    let font_px = (base_font_px * zoom).max(base_font_px).round() as f32;
    let ui_scale = (font_px / 16.0) * dpr_f;
    let ruler_label_scale = 1.5_f32 / ui_scale;
    let remote_label_scale = 1.5_f32;

    let grid_line_thickness = 1.5 * dpr_f;
    let standard_border_px = 1.5 * dpr_f;
    let remote_border_px = 1.5 * dpr_f;
    let region_dash_px = 3.5 * dpr_f;
    let selection_dash_px = 2.5 * dpr_f;

    let ruler_text_offset = 1.0 * dpr_f;
    let remote_label_padding = 2.0 * dpr_f * ui_scale;

    const GRID_COLOR: [f32; 4] = [0.075, 0.075, 0.075, 1.0];
    const RULER_TEXT_COLOR: [f32; 3] = [0.227, 0.227, 0.227];
    const REGION_MINE_BORDER_HILT: [f32; 4] = [0.165, 0.376, 0.188, 1.0];
    const REGION_MINE_BORDER_IDLE: [f32; 4] = [0.102, 0.188, 0.125, 1.0];
    const REGION_MINE_FILL_HILT: [f32; 4] = [0.0, 0.314, 0.0, 0.07];
    const REGION_THEIR_BORDER_HILT: [f32; 4] = [0.478, 0.282, 0.0, 1.0];
    const REGION_THEIR_BORDER_IDLE: [f32; 4] = [0.227, 0.133, 0.0, 1.0];
    const REGION_THEIR_FILL_HILT: [f32; 4] = [0.314, 0.196, 0.0, 0.09];
    const COLOR_TRANSPARENT: [f32; 4] = [0.0; 4];

    let w2d = |wc: WorldCoords| -> DevicePt {
        DevicePt {
            x: wc.x as f32 * cw_dev - vx_dev,
            y: wc.y as f32 * ch_dev - vy_dev,
        }
    };
    let wsz2d = |cells_w: i64, cells_h: i64| -> DeviceSize {
        DeviceSize {
            w: cells_w as f32 * cw_dev,
            h: cells_h as f32 * ch_dev,
        }
    };

    let screen_rect = DeviceRect {
        x: 0.0,
        y: 0.0,
        w: w_dev,
        h: h_dev,
    };
    let cell_size = wsz2d(1, 1);
    let mut rect_insts: Vec<UIRectInstance> = Vec::new();

    for gx in bounds.visible_chunk_columns() {
        let sx = gx as f32 * CHUNK_W as f32 * cw_dev - vx_dev;
        rect_insts.push(UIRectInstance {
            screen_xy: [sx - grid_line_thickness * 0.5, 0.0],
            screen_wh: [grid_line_thickness, h_dev],
            fill_rgba: GRID_COLOR,
            border_rgba: [0.0; 4],
            border_px: 0.0,
            dash_px: 0.0,
            flags: 0.0,
            _pad: 0.0,
        });
    }
    for gy in bounds.visible_chunk_rows() {
        let sy = gy as f32 * CHUNK_H as f32 * ch_dev - vy_dev;
        rect_insts.push(UIRectInstance {
            screen_xy: [0.0, sy - grid_line_thickness * 0.5],
            screen_wh: [w_dev, grid_line_thickness],
            fill_rgba: GRID_COLOR,
            border_rgba: [0.0; 4],
            border_px: 0.0,
            dash_px: 0.0,
            flags: 0.0,
            _pad: 0.0,
        });
    }

    let canvas_tl = w2d(WorldCoords::from_i64(-BOARD_HALF, -BOARD_HALF));
    let canvas_br_x = BOARD_HALF as f32 * cw_dev - vx_dev;
    let canvas_br_y = BOARD_HALF as f32 * ch_dev - vy_dev;
    let canvas_border_px = grid_line_thickness.max(4.0 * dpr_f);
    let cbp = canvas_border_px;
    rect_insts.push(UIRectInstance {
        screen_xy: [canvas_tl.x - cbp, canvas_tl.y - cbp],
        screen_wh: [
            canvas_br_x - canvas_tl.x + cbp * 2.0,
            canvas_br_y - canvas_tl.y + cbp * 2.0,
        ],
        fill_rgba: COLOR_TRANSPARENT,
        border_rgba: GRID_COLOR,
        border_px: canvas_border_px,
        dash_px: 0.0,
        flags: 0.0,
        _pad: 0.0,
    });

    let bg_rect_count = rect_insts.len();

    if show_detail {
        let show_rg = crate::widgets::MiniTUI::is_open("panel-regions");
        for r in &regions {
            let region_rect = DeviceRect::from_pt_size(
                w2d(r.bounds.a),
                wsz2d(r.bounds.width(), r.bounds.height()),
            );
            if !screen_rect.intersects(&region_rect) {
                continue;
            }

            let mine = r.owner_id == my_id;
            let hilt = show_rg || r.bounds.contains(cur_pos);

            let (border_rgba, fill_rgba) = match (mine, hilt) {
                (true, true) => (REGION_MINE_BORDER_HILT, REGION_MINE_FILL_HILT),
                (true, false) => (REGION_MINE_BORDER_IDLE, COLOR_TRANSPARENT),
                (false, true) => (REGION_THEIR_BORDER_HILT, REGION_THEIR_FILL_HILT),
                (false, false) => (REGION_THEIR_BORDER_IDLE, COLOR_TRANSPARENT),
            };

            rect_insts.push(UIRectInstance {
                screen_xy: [region_rect.x, region_rect.y],
                screen_wh: [region_rect.w, region_rect.h],
                fill_rgba,
                border_rgba,
                border_px: standard_border_px,
                dash_px: region_dash_px,
                flags: 0.0,
                _pad: 0.0,
            });
        }

        if sel_active {
            let sp = w2d(sel.a);
            let ss = wsz2d(sel.width(), sel.height());

            rect_insts.push(UIRectInstance {
                screen_xy: [sp.x, sp.y],
                screen_wh: [ss.w, ss.h],
                fill_rgba: [0.784, 0.549, 0.0, 0.07],
                border_rgba: [0.600, 0.400, 0.0, 1.0],
                border_px: standard_border_px,
                dash_px: selection_dash_px,
                flags: 0.0,
                _pad: 0.0,
            });
        }

        let local_pt = w2d(cur_pos);
        rect_insts.push(UIRectInstance {
            screen_xy: [local_pt.x, local_pt.y],
            screen_wh: [cell_size.w, cell_size.h],
            fill_rgba: color.to_f32_alpha(0.2),
            border_rgba: color.to_f32_alpha(1.0),
            border_px: standard_border_px,
            dash_px: 0.0,
            flags: 1.0,
            _pad: 0.0,
        });

        for (_, pos, rc) in &remote {
            let cursor_rect = DeviceRect::from_pt_size(w2d(*pos), cell_size);
            if !screen_rect.intersects(&cursor_rect) {
                continue;
            }

            rect_insts.push(UIRectInstance {
                screen_xy: [cursor_rect.x, cursor_rect.y],
                screen_wh: [cursor_rect.w, cursor_rect.h],
                fill_rgba: rc.to_f32_alpha(0.15),
                border_rgba: rc.to_f32_alpha(1.0),
                border_px: remote_border_px,
                dash_px: 0.0,
                flags: 0.0,
                _pad: 0.0,
            });
        }
    }

    let density_factor = 4.0;
    let font_px = (8.0 * zoom).max(8.0).round();
    let min_step = ((density_factor * font_px) / cw).ceil() as usize;
    let step: usize = [
        1usize,
        2,
        5,
        10,
        20,
        50,
        100,
        200,
        500,
        1_000,
        2_000,
        BOARD_HALF as usize,
    ]
    .iter()
    .copied()
    .find(|&s| s >= min_step)
    .unwrap_or(BOARD_HALF as usize);

    let (label_rects, text_insts) = WGPU_STATE.with(|ws| {
        let borrow = ws.borrow();
        let Some(state) = borrow.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        let Some(atlas) = &state.atlas else {
            return (Vec::new(), Vec::new());
        };

        let f = &atlas.font_file;
        let cell_w_dev = f.cell_w as f32 * ui_scale;
        let cell_h_dev = f.cell_h as f32 * ui_scale;

        let mut extra_rects: Vec<UIRectInstance> = Vec::new();
        let mut out: Vec<Instance> = Vec::new();

        let sx0 = (bounds.a.x as f64 / step as f64).ceil() as i64 * step as i64;
        for wx64 in (sx0..=bounds.b.x as i64).step_by(step) {
            let pt = w2d(WorldCoords::from_i64(wx64, 0));
            let text_pt = DevicePt {
                x: pt.x + ruler_text_offset,
                y: ruler_text_offset,
            };
            emit_glyphs(
                text_pt,
                &format!("{}", wx64),
                RULER_TEXT_COLOR,
                f,
                cell_w_dev,
                ruler_label_scale,
                &mut out,
            );
        }

        let sy0 = (bounds.a.y as f64 / step as f64).ceil() as i64 * step as i64;
        for wy64 in (sy0..=bounds.b.y as i64).step_by(step) {
            if wy64 == 0 {
                continue;
            }
            let pt = w2d(WorldCoords::from_i64(0, wy64));
            let text_pt = DevicePt {
                x: ruler_text_offset,
                y: pt.y + ruler_text_offset,
            };
            emit_glyphs(
                text_pt,
                &format!("{}", wy64),
                RULER_TEXT_COLOR,
                f,
                cell_w_dev,
                ruler_label_scale,
                &mut out,
            );
        }

        if show_detail {
            for (name, pos, rc) in &remote {
                let cursor_pt = w2d(*pos);
                let cursor_rect = DeviceRect::from_pt_size(cursor_pt, cell_size);
                if !screen_rect.intersects(&cursor_rect) {
                    continue;
                }

                let text_height = cell_h_dev * remote_label_scale;
                let text_width = name.chars().count() as f32 * cell_w_dev * remote_label_scale;

                let bg_width = text_width + (remote_label_padding * 2.0);
                let bg_height = text_height + remote_label_padding;
                let bg_x = cursor_pt.x - remote_label_padding;
                let bg_y = cursor_pt.y - bg_height;

                let bg_r = (13.6 + rc.0 as f32 * 0.16) / 255.0;
                let bg_g = (13.6 + rc.1 as f32 * 0.16) / 255.0;
                let bg_b = (13.6 + rc.2 as f32 * 0.16) / 255.0;

                extra_rects.push(UIRectInstance {
                    screen_xy: [bg_x, bg_y],
                    screen_wh: [bg_width, bg_height],
                    fill_rgba: [bg_r, bg_g, bg_b, 0.8],
                    border_rgba: [0.0; 4],
                    border_px: 0.0,
                    dash_px: 0.0,
                    flags: 0.0,
                    _pad: 0.0,
                });

                let text_pt = DevicePt {
                    x: bg_x + remote_label_padding,
                    y: bg_y,
                };
                emit_glyphs(
                    text_pt,
                    name,
                    rc.to_f32_array(),
                    f,
                    cell_w_dev,
                    remote_label_scale,
                    &mut out,
                );
            }
        }

        (extra_rects, out)
    });

    rect_insts.extend(label_rects);
    render_cells_wgpu(rect_insts, bg_rect_count, text_insts, blink_on, ui_scale);
}

pub fn schedule_render() {
    mark_canvas_dirty();
    if RAF_PENDING.with(|r| r.get()) {
        return;
    }
    RAF_PENDING.with(|r| r.set(true));
    let cb = Closure::once(move || {
        RAF_PENDING.with(|r| r.set(false));
        if CANVAS_DIRTY.with(|d| d.replace(false)) {
            render_frame();
            let now = perf();
            with_board(|b| b.flash_deny.retain(|_, t| *t > now));
        }
        crate::widgets::render_dirty();
    });
    window()
        .request_animation_frame(cb.as_ref().unchecked_ref())
        .unwrap();
    cb.forget();
}

pub fn momentum_tick() {
    let (vel_x, vel_y) = read_vp(|vp| (vp.touch_vel_x, vp.touch_vel_y));
    if vel_x.abs() < 0.05 && vel_y.abs() < 0.05 {
        MOMENTUM.with(|m| m.set(false));
        return;
    }
    with_vp(|vp| {
        vp.vx -= vp.touch_vel_x * 16.0;
        vp.vy -= vp.touch_vel_y * 16.0;
        vp.clamp_viewport();
        vp.touch_vel_x *= 0.9;
        vp.touch_vel_y *= 0.9;
    });
    refresh_chunks(MAX_FLUSH);
    schedule_render();
    let cb = Closure::once(momentum_tick);
    window()
        .request_animation_frame(cb.as_ref().unchecked_ref())
        .unwrap();
    cb.forget();
}

pub fn start_momentum() {
    if MOMENTUM.with(|m| m.get()) {
        return;
    }
    MOMENTUM.with(|m| m.set(true));
    momentum_tick();
}

pub fn is_glyph_mode() -> bool {
    read_vp(|vp| vp.cw >= LOD_GLYPH_THRESHOLD)
}

pub fn resize_canvas() {
    let win = window();
    let dpr = win.device_pixel_ratio();
    let sb_h = El::from_id("statusbar")
        .map(|e| e.rect().height())
        .unwrap_or(22.0);
    let lw = win.inner_width().unwrap().as_f64().unwrap();
    let lh = win.inner_height().unwrap().as_f64().unwrap() - sb_h;
    let dev_w = (lw * dpr).round() as u32;
    let dev_h = (lh * dpr).round() as u32;
    debug_log!("resize_canvas: {}x{} (dpr={})", dev_w, dev_h, dpr);

    with_vp(|vp| {
        vp.dpr = dpr;
        vp.log_w = lw;
        vp.log_h = lh;
    });

    WGPU_STATE.with(|ws| {
        let mut b = match ws.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        if let Some(state) = b.as_mut() {
            let safe_w = dev_w.clamp(1, state.max_tex_dim);
            let safe_h = dev_h.clamp(1, state.max_tex_dim);
            state
                .canvas
                .style()
                .set_property("width", &format!("{}px", lw))
                .unwrap_or(());
            state
                .canvas
                .style()
                .set_property("height", &format!("{}px", lh))
                .unwrap_or(());
            state.canvas.set_width(safe_w);
            state.canvas.set_height(safe_h);
            state
                .canvas
                .style()
                .set_property("image-rendering", "smooth")
                .unwrap_or(());
            state.config.width = safe_w;
            state.config.height = safe_h;
            state.surface.configure(&state.device, &state.config);
            state.surface_w = safe_w;
            state.surface_h = safe_h;
        }
    });

    let is_conn = crate::state::read_net(|n| n.is_conn);
    if is_conn {
        refresh_chunks(MAX_FLUSH);
    }

    schedule_render();
}

pub fn drop_stale_chunk_bufs() {
    WGPU_STATE.with(|ws| {
        if let Ok(mut b) = ws.try_borrow_mut()
            && let Some(state) = b.as_mut()
        {
            read_board(|board| {
                state
                    .chunk_bufs
                    .retain(|c, _| board.chunk_manager.chunks.contains_key(c));
            });
        }
    });
}
