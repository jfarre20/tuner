//! Cross-platform presenter on wgpu (Vulkan first, OpenGL fallback): the same two passes as the
//! D3D11 one in gpu.rs, ported to WGSL. Video arrives as yuv420p planes from a software decoder;
//! there is no zero-copy hardware path here yet. Used on non-Windows targets, or on Windows with
//! `--features wgpu` (handy for testing the port).

use std::sync::Arc;
use winit::window::Window;

pub type Result<T> = std::result::Result<T, String>;

const SHADER: &str = r#"
struct Cb {
    rect: vec4<f32>,   // video rect in 0..1 window coords: x0, y0, x1, y1
    snow: f32,         // 0..1 snow opacity
    seed: f32,         // changes per frame
    mode: f32,         // unused here (always 3-plane yuv420p)
    snow_full: f32,    // 1 = snow over the whole window, 0 = only inside rect
    has_video: f32,
    power: f32,        // 0..1 tube warm-up / collapse (1 = normal picture)
    settle: f32,       // 1..0 analog lock-in after a tune: line jitter, roll, flicker
    pad: f32,
};
@group(0) @binding(0) var<uniform> cb: Cb;
@group(0) @binding(1) var texY: texture_2d<f32>;
@group(0) @binding(2) var texU: texture_2d<f32>;
@group(0) @binding(3) var texV: texture_2d<f32>;
@group(0) @binding(4) var overlay: texture_2d<f32>;
@group(0) @binding(5) var lin: sampler;
@group(0) @binding(6) var pnt: sampler;

struct VSOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) id: u32) -> VSOut {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    var o: VSOut;
    o.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn hash(p0: vec2<f32>) -> f32 {
    var p = fract(p0 * vec2<f32>(123.34, 456.21));
    p = p + dot(p, p + 45.32);
    return fract(p.x * p.y);
}

@fragment
fn ps(i: VSOut) -> @location(0) vec4<f32> {
    var rgb = vec3<f32>(0.0);
    var uv = i.uv;
    // Tube warm-up: the raster opens from a bright horizontal line to the full height.
    let open = smoothstep(0.0, 1.0, cb.power);
    let c = uv - 0.5;
    if (cb.power < 1.0) {
        uv = vec2<f32>(c.x / mix(0.7, 1.0, open), c.y / max(open, 1e-3)) + 0.5;
    }
    var vuv = (uv - cb.rect.xy) / (cb.rect.zw - cb.rect.xy);
    if (cb.settle > 0.0) {
        // Horizontal sync hunting: each pair of lines is shoved sideways by its own noise, and
        // the whole picture rolls up once before it locks.
        let row = floor(i.pos.y * 0.5);
        vuv.x = vuv.x + (hash(vec2<f32>(row, cb.seed * 7.0)) - 0.5) * 0.06 * cb.settle * cb.settle;
        vuv.y = fract(vuv.y + cb.settle * cb.settle * cb.settle * 0.8);
    }
    let inside = all(vuv >= vec2<f32>(0.0)) && all(vuv <= vec2<f32>(1.0));
    if (cb.has_video > 0.5 && inside) {
        var y = textureSampleLevel(texY, lin, vuv, 0.0).r;
        var u = textureSampleLevel(texU, lin, vuv, 0.0).r;
        var v = textureSampleLevel(texV, lin, vuv, 0.0).r;
        // BT.601 limited range
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        u = (u - 0.5) * (255.0 / 224.0);
        v = (v - 0.5) * (255.0 / 224.0);
        rgb = saturate(vec3<f32>(y + 1.402 * v, y - 0.344136 * u - 0.714136 * v, y + 1.772 * u));
    }
    if (cb.snow > 0.0 && (cb.snow_full > 0.5 || inside)) {
        let blk = floor(i.pos.xy * 0.5); // 2x2 blocks, like analog snow
        let g = 16.0 / 255.0 + hash(blk + cb.seed) * (220.0 / 255.0);
        rgb = mix(rgb, vec3<f32>(g), cb.snow);
    }
    if (cb.settle > 0.0) {
        rgb = rgb * (1.0 - 0.35 * cb.settle * hash(vec2<f32>(cb.seed, 3.1)));
    }
    let ov = textureSampleLevel(overlay, pnt, uv, 0.0);
    rgb = mix(rgb, ov.rgb, ov.a);
    if (cb.power < 1.0) {
        // Everything outside the opened raster is dark; the collapsed line glows white-hot.
        let inside_y = step(abs(c.y), open * 0.5) * step(abs(c.x), mix(0.35, 0.5, open));
        let glow = exp(-abs(c.y) / (0.004 + open * 0.02)) * (1.0 - open);
        rgb = rgb * inside_y * (1.0 + (1.0 - open) * 1.2) + glow * 1.6;
    }
    return vec4<f32>(rgb, 1.0);
}

// ---------------------------------------------------------------- CRT pass

struct Crt {
    res: vec2<f32>,  // virtual tube resolution
    time: f32,
    curve: f32,      // barrel distortion
    scan: f32,       // scanline depth 0..1
    noise: f32,      // interference
    vignette: f32,
    pad: f32,
};
@group(0) @binding(0) var<uniform> crt: Crt;
@group(0) @binding(1) var scene: texture_2d<f32>;
@group(0) @binding(2) var slin: sampler;

const BRIGHTNESS: f32 = 2.15;
const BLACK_EMISSIVE: f32 = 0.01;
const VERTICAL_LINES: f32 = 483.0;
const OUTPUT_GAIN: f32 = 2.35;

fn pulseIntegral3(x: vec3<f32>, s1: f32, s2: f32) -> vec3<f32> { return clamp(x - s1, vec3<f32>(0.0), vec3<f32>(s2 - s1)); }

fn bayer(uv: vec2<f32>, blur: vec2<f32>) -> vec3<f32> {
    var x = vec3<f32>(uv.x);
    var y = vec3<f32>(uv.y);
    x = x + vec3<f32>(0.66, 0.33, 0.0);
    y = y + 0.5 * step(fract(x * 0.5), vec3<f32>(0.5));
    x = fract(x);
    y = fract(y);
    let size = vec2<f32>(0.16, 0.75);
    let vMin = 0.5 - size * 0.5;
    let vMax = 0.5 + size * 0.5;
    let vx = (pulseIntegral3(x + blur.x, vMin.x, vMax.x) - pulseIntegral3(x - blur.x, vMin.x, vMax.x)) / max(blur.x, 1e-4);
    let vy = (pulseIntegral3(y + blur.y, vMin.y, vMax.y) - pulseIntegral3(y - blur.y, vMin.y, vMax.y)) / max(blur.y, 1e-4);
    return min(vx, vy) * 5.0;
}

fn getPixelMatrix(uv: vec2<f32>) -> vec3<f32> {
    let dx = dpdx(uv);
    let dy = dpdy(uv);
    let dU = length(vec2<f32>(dx.x, dy.x));
    let dV = length(vec2<f32>(dx.y, dy.y));
    if (dU <= 0.0 || dV <= 0.0) { return vec3<f32>(1.0); }
    return bayer(uv, vec2<f32>(dU, dV));
}

fn scanline(y: f32, blur: f32) -> f32 {
    var s = sin(y * 10.0) * 0.45 + 0.55;
    s = mix(1.0, s, crt.scan);
    return mix(s, 1.0, min(1.0, blur));
}

fn getScanline(uv0: vec2<f32>) -> f32 {
    let uv = vec2<f32>(uv0.x, uv0.y * 0.25);
    let dx = dpdx(uv);
    let dy = dpdy(uv);
    let dV = length(vec2<f32>(dx.y, dy.y));
    if (dV <= 0.0) { return 1.0; }
    return scanline(uv.y, dV * 1.3);
}

fn interferenceHash(p: f32) -> f32 {
    var p3 = fract(vec3<f32>(p) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 19.19);
    return fract((p3.x + p3.y) * p3.z);
}

fn interferenceSmoothNoise1D(x: f32) -> f32 {
    let f0 = floor(x);
    let fr = fract(x);
    return mix(interferenceHash(f0), interferenceHash(f0 + 1.0), fr);
}

fn getInterference(uv: vec2<f32>) -> vec2<f32> {
    let scanLine = floor(uv.y * VERTICAL_LINES);
    let scanPos = scanLine + uv.x;
    let timeSeed = fract(crt.time * 123.78);
    let noise = interferenceSmoothNoise1D(scanPos * 234.5 + timeSeed * 12345.6);
    let scanRnd = interferenceHash(uv.y * 100.0 + fract(crt.time * 1234.0) * 12345.0);
    return vec2<f32>(noise, scanRnd);
}

fn sampleScreen(uv: vec2<f32>) -> vec3<f32> {
    let resolution = crt.res;
    let pixelCoord = uv * resolution;
    let pixelMatrix = getPixelMatrix(pixelCoord);
    let scan = getScanline(pixelCoord);
    var texUV = floor(uv * resolution * 2.0) / (resolution * 2.0);
    let interference = getInterference(texUV);
    texUV.x = texUV.x + (interference.y * 2.0 - 1.0) * 0.025 * crt.noise;
    // Outside the tube face is bezel (black), not smeared edge pixels.
    let outside = any(texUV < vec2<f32>(0.0)) || any(texUV > vec2<f32>(1.0));
    var col = select(textureSampleLevel(scene, slin, texUV, 0.0).rgb, vec3<f32>(0.0), outside);
    col = clamp(col + (interference.x - 0.5) * 2.0 * crt.noise, vec3<f32>(0.0), vec3<f32>(1.0));
    let result = (col * col * BRIGHTNESS + BLACK_EMISSIVE) * pixelMatrix * scan;
    return result / (1.0 + BRIGHTNESS + BLACK_EMISSIVE);
}

@fragment
fn ps_crt(i: VSOut) -> @location(0) vec4<f32> {
    // Barrel curvature: the tube face bulges, so straight lines bow outward at the edges.
    var p = i.uv * 2.0 - 1.0;
    let r2 = dot(p, p);
    p = p * (1.0 + crt.curve * r2);
    let uv = p * 0.5 + 0.5;
    let suv = uv * 1.1 - 0.05;
    var col = sampleScreen(suv);
    col = col * (1.0 - crt.vignette * r2); // vignette: phosphor is dimmer toward the corners
    let radius = 0.17;
    let b = vec2<f32>(1.0 - radius);
    let d = abs(p) - b;
    let dist = length(max(d, vec2<f32>(0.0))) - radius;
    let cornerMask = 1.0 - smoothstep(-0.02, 0.035, dist);
    col = col * cornerMask;
    col = sqrt(max(col, vec3<f32>(0.0)));
    col = pow(col, vec3<f32>(0.95));
    col = col * OUTPUT_GAIN;
    return vec4<f32>(col, 1.0);
}

// Straight copy of the composite when the CRT pass is off.
@fragment
fn ps_blit(i: VSOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSampleLevel(scene, slin, i.uv, 0.0).rgb, 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Cb {
    rect: [f32; 4],
    snow: f32,
    seed: f32,
    mode: f32,
    snow_full: f32,
    has_video: f32,
    power: f32,
    settle: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CrtCb {
    res: [f32; 2],
    time: f32,
    curve: f32,
    scan: f32,
    noise: f32,
    vignette: f32,
    _pad: f32,
}

/// User-tunable CRT look.
#[derive(Clone, Copy)]
pub struct CrtParams {
    pub curve: f32,
    pub scan: f32,
    pub noise: f32,
    pub vignette: f32,
}

/// Per-frame analog effects for `render`.
#[derive(Clone, Copy)]
pub struct Fx {
    pub snow: f32,
    pub snow_full: bool,
    pub seed: f32,
    pub crt: bool,
    pub crt_params: CrtParams,
    pub time: f32,
    pub power: f32,
    pub settle: f32,
}

/// One decoded picture: yuv420p planes from a software decoder.
pub enum Picture<'a> {
    Yuv420p { planes: [&'a [u8]; 3], strides: [usize; 3], width: u32, height: u32 },
}

fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

struct Tex {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    bgra: bool,
    width: u32,
    height: u32,
    main_pipe: wgpu::RenderPipeline,
    crt_pipe: wgpu::RenderPipeline,
    blit_pipe: wgpu::RenderPipeline,
    main_layout: wgpu::BindGroupLayout,
    post_layout: wgpu::BindGroupLayout,
    cb: wgpu::Buffer,
    cb_crt: wgpu::Buffer,
    lin: wgpu::Sampler,
    pnt: wgpu::Sampler,
    overlay: Tex,
    scene: Tex, // composite target (main pass output)
    out: Tex,   // final image (crt / blit output), copied to the swapchain and readable
    yuv: Option<(u32, u32, [Tex; 3])>,
    black: Tex, // 1x1 stand-in for the video planes when there is no picture
    main_bg: Option<wgpu::BindGroup>,
    post_bg: Option<wgpu::BindGroup>,
}

fn make_tex(device: &wgpu::Device, label: &str, w: u32, h: u32, format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> Tex {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { tex, view }
}

impl Gpu {
    pub fn new(window: Arc<Window>, width: u32, height: u32) -> Result<Gpu> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN | wgpu::Backends::GL, ..Default::default() });
        let surface = instance.create_surface(window).map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| format!("no Vulkan/OpenGL adapter: {e}"))?;
        let info = adapter.get_info();
        println!("video: wgpu on {} ({:?})", info.name, info.backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("tuner"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| format!("device: {e}"))?;
        let caps = surface.get_capabilities(&adapter);
        let format = if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
            wgpu::TextureFormat::Bgra8Unorm
        } else if caps.formats.contains(&wgpu::TextureFormat::Rgba8Unorm) {
            wgpu::TextureFormat::Rgba8Unorm
        } else {
            caps.formats[0]
        };
        let bgra = format == wgpu::TextureFormat::Bgra8Unorm;
        // We pace frames ourselves: never block in present if the driver allows it.
        let present_mode = [wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("tuner"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let sampler_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let uniform_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let main_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("main"),
            entries: &[uniform_entry(0), tex_entry(1), tex_entry(2), tex_entry(3), tex_entry(4), sampler_entry(5), sampler_entry(6)],
        });
        let post_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[uniform_entry(0), tex_entry(1), sampler_entry(2)],
        });
        let pipeline = |label: &str, layout: &wgpu::BindGroupLayout, entry: &str, target: wgpu::TextureFormat| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[layout], push_constant_ranges: &[] });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState { format: target, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let scene_fmt = wgpu::TextureFormat::Bgra8Unorm;
        let main_pipe = pipeline("main", &main_layout, "ps", scene_fmt);
        let crt_pipe = pipeline("crt", &post_layout, "ps_crt", format);
        let blit_pipe = pipeline("blit", &post_layout, "ps_blit", format);
        let ub = |label: &str, size: u64| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
        };
        let cb = ub("cb", std::mem::size_of::<Cb>() as u64);
        let cb_crt = ub("crt", std::mem::size_of::<CrtCb>() as u64);
        let sampler = |filter: wgpu::FilterMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            })
        };
        let lin = sampler(wgpu::FilterMode::Linear);
        let pnt = sampler(wgpu::FilterMode::Nearest);
        let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let overlay = make_tex(&device, "overlay", width, height, wgpu::TextureFormat::Bgra8Unorm, sampled);
        let scene = make_tex(&device, "scene", width, height, scene_fmt, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT);
        let out = make_tex(&device, "out", width, height, format, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let black = make_tex(&device, "black", 1, 1, wgpu::TextureFormat::R8Unorm, sampled);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &black.tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &[16u8],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(1), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        Ok(Gpu {
            device,
            queue,
            surface,
            config,
            bgra,
            width: width.max(1),
            height: height.max(1),
            main_pipe,
            crt_pipe,
            blit_pipe,
            main_layout,
            post_layout,
            cb,
            cb_crt,
            lin,
            pnt,
            overlay,
            scene,
            out,
            yuv: None,
            black,
            main_bg: None,
            post_bg: None,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if w == 0 || h == 0 || (w == self.width && h == self.height) {
            return Ok(());
        }
        self.width = w;
        self.height = h;
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        self.overlay = make_tex(&self.device, "overlay", w, h, wgpu::TextureFormat::Bgra8Unorm, sampled);
        self.scene = make_tex(&self.device, "scene", w, h, wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT);
        self.out = make_tex(&self.device, "out", w, h, self.config.format, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        self.main_bg = None;
        self.post_bg = None;
        Ok(())
    }

    /// Upload `w` x `h` texels of `bpp` bytes each from rows `stride` bytes apart.
    fn write_plane(&self, tex: &wgpu::Texture, w: u32, h: u32, bpp: usize, data: &[u8], stride: usize) {
        let needed = stride * (h as usize - 1) + w as usize * bpp;
        if data.len() < needed {
            return;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &data[..needed.max(stride * h as usize).min(data.len())],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Replace the overlay (window-sized ARGB, straight alpha; 0 = transparent).
    pub fn upload_overlay(&self, argb: &[u32]) -> Result<()> {
        if argb.len() != (self.width * self.height) as usize {
            return Err("overlay size".into());
        }
        let bytes = unsafe { std::slice::from_raw_parts(argb.as_ptr() as *const u8, argb.len() * 4) };
        self.write_plane(&self.overlay.tex, self.width, self.height, 4, bytes, self.width as usize * 4);
        Ok(())
    }

    fn bind_picture(&mut self, pic: Option<&Picture>) {
        if let Some(Picture::Yuv420p { planes, strides, width, height }) = pic {
            let (w, h) = (*width, *height);
            if self.yuv.as_ref().is_none_or(|y| y.0 != w || y.1 != h) {
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
                let mk = |n: &str, pw: u32, ph: u32| make_tex(&self.device, n, pw, ph, wgpu::TextureFormat::R8Unorm, sampled);
                self.yuv = Some((w, h, [mk("Y", w, h), mk("U", cw, ch), mk("V", cw, ch)]));
                self.main_bg = None;
            }
            let (_, _, texs) = self.yuv.as_ref().unwrap();
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            self.write_plane(&texs[0].tex, w, h, 1, planes[0], strides[0]);
            self.write_plane(&texs[1].tex, cw, ch, 1, planes[1], strides[1]);
            self.write_plane(&texs[2].tex, cw, ch, 1, planes[2], strides[2]);
        }
        if self.main_bg.is_none() {
            let (y, u, v) = match &self.yuv {
                Some((_, _, t)) => (&t[0].view, &t[1].view, &t[2].view),
                None => (&self.black.view, &self.black.view, &self.black.view),
            };
            self.main_bg = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("main"),
                layout: &self.main_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.cb.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(y) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(u) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(v) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&self.overlay.view) },
                    wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(&self.lin) },
                    wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::Sampler(&self.pnt) },
                ],
            }));
        }
        if self.post_bg.is_none() {
            self.post_bg = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post"),
                layout: &self.post_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.cb_crt.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.scene.view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.lin) },
                ],
            }));
        }
    }

    /// Draw one frame. `rect` is the video rect in 0..1 window coords (x0, y0, x1, y1).
    pub fn render(&mut self, pic: Option<&Picture>, rect: [f32; 4], fx: Fx) -> Result<()> {
        let Fx { snow, snow_full, seed, crt, crt_params, time, power, settle } = fx;
        self.bind_picture(pic);
        let cb = Cb { rect, snow, seed, mode: 1.0, snow_full: if snow_full { 1.0 } else { 0.0 }, has_video: if pic.is_some() { 1.0 } else { 0.0 }, power, settle, _pad: 0.0 };
        self.queue.write_buffer(&self.cb, 0, as_bytes(&cb));
        let aspect = self.width as f32 / self.height.max(1) as f32;
        let ccb = CrtCb { res: [(480.0 * aspect).round(), 480.0], time, curve: crt_params.curve, scan: crt_params.scan, noise: crt_params.noise, vignette: crt_params.vignette, _pad: 0.0 };
        self.queue.write_buffer(&self.cb_crt, 0, as_bytes(&ccb));

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.config);
                self.surface.get_current_texture().map_err(|e| format!("surface: {e}"))?
            }
            Err(e) => return Err(format!("surface: {e}")),
        };
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let pass = |enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, pipe: &wgpu::RenderPipeline, bg: &wgpu::BindGroup| {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(pipe);
            rp.set_bind_group(0, bg, &[]);
            rp.draw(0..3, 0..1);
        };
        pass(&mut enc, &self.scene.view, &self.main_pipe, self.main_bg.as_ref().unwrap());
        pass(&mut enc, &self.out.view, if crt { &self.crt_pipe } else { &self.blit_pipe }, self.post_bg.as_ref().unwrap());
        let size = wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 };
        enc.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.out.tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyTextureInfo { texture: &frame.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            size,
        );
        self.queue.submit([enc.finish()]);
        frame.present();
        Ok(())
    }

    /// Copy the last presented image to the CPU (BGRA). Debug/bench only.
    pub fn read_back(&self) -> Result<(u32, u32, Vec<u8>)> {
        let (w, h) = (self.width, self.height);
        let padded = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("readback"), size: (padded * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.out.tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::Wait).map_err(|e| format!("poll: {e:?}"))?;
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("map: {e:?}"))?;
        let data = slice.get_mapped_range();
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            let row = &data[y * padded as usize..][..(w * 4) as usize];
            let dst = &mut out[y * (w * 4) as usize..][..(w * 4) as usize];
            dst.copy_from_slice(row);
            if !self.bgra {
                for p in dst.chunks_exact_mut(4) {
                    p.swap(0, 2);
                }
            }
        }
        drop(data);
        buf.unmap();
        Ok((w, h, out))
    }
}
