//! D3D11 presenter. Takes a decoded frame (NV12 texture slice from D3D11VA, or yuv420p planes
//! from a software decoder), draws it into the swapchain with one fullscreen shader pass that does
//! YUV->RGB, letterbox/stretch, snow (hash noise), and an ARGB overlay (OSD / guide) on top.
//! Present is tearing-enabled so it never blocks the UI thread.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub type Result<T> = windows::core::Result<T>;

const SHADER: &str = r#"
cbuffer CB : register(b0) {
    float4 rect;      // video rect in 0..1 window coords: x0, y0, x1, y1
    float  snow;      // 0..1 snow opacity
    float  seed;      // changes per frame
    float  mode;      // 0 = NV12 (t0 = Y, t1 = UV), 1 = yuv420p (t0 = Y, t1 = U, t2 = V)
    float  snow_full; // 1 = snow over the whole window, 0 = only inside rect
    float  has_video;
    float  power;     // 0..1 tube warm-up / collapse (1 = normal picture)
    float  settle;    // 1..0 analog "lock-in" after a tune: line jitter, roll, flicker
    float  pad;
};
Texture2D texY : register(t0);
Texture2D texU : register(t1);
Texture2D texV : register(t2);
Texture2D overlay : register(t3);
SamplerState lin : register(s0);
SamplerState pnt : register(s1);

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut vs(uint id : SV_VertexID) {
    float2 uv = float2((id << 1) & 2, id & 2);
    VSOut o;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = uv;
    return o;
}

float hash(float2 p) {
    p = frac(p * float2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return frac(p.x * p.y);
}

float4 ps(VSOut i) : SV_Target {
    float3 rgb = float3(0, 0, 0);
    float2 uv = i.uv;
    // Tube warm-up: the raster opens from a bright horizontal line to the full height.
    float open = smoothstep(0.0, 1.0, power);
    float2 c = uv - 0.5;
    if (power < 1.0) {
        uv = float2(c.x / lerp(0.7, 1.0, open), c.y / max(open, 1e-3)) + 0.5;
    }
    float2 vuv = (uv - rect.xy) / (rect.zw - rect.xy);
    if (settle > 0.0) {
        // Horizontal sync hunting: each pair of lines is shoved sideways by its own noise, and
        // the whole picture rolls up once before it locks.
        float row = floor(i.pos.y * 0.5);
        vuv.x += (hash(float2(row, seed * 7.0)) - 0.5) * 0.06 * settle * settle;
        vuv.y = frac(vuv.y + settle * settle * settle * 0.8);
    }
    bool inside = all(vuv >= 0) && all(vuv <= 1);
    if (has_video > 0.5 && inside) {
        float y = texY.Sample(lin, vuv).r;
        float u, v;
        if (mode < 0.5) { float2 c = texU.Sample(lin, vuv).rg; u = c.r; v = c.g; }
        else { u = texU.Sample(lin, vuv).r; v = texV.Sample(lin, vuv).r; }
        // BT.601 limited range
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        u = (u - 0.5) * (255.0 / 224.0);
        v = (v - 0.5) * (255.0 / 224.0);
        rgb = saturate(float3(y + 1.402 * v, y - 0.344136 * u - 0.714136 * v, y + 1.772 * u));
    }
    if (snow > 0 && (snow_full > 0.5 || inside)) {
        float2 blk = floor(i.pos.xy * 0.5); // 2x2 blocks, like analog snow
        float g = 16.0 / 255.0 + hash(blk + seed) * (220.0 / 255.0);
        rgb = lerp(rgb, float3(g, g, g), snow);
    }
    if (settle > 0.0) {
        rgb *= 1.0 - 0.35 * settle * hash(float2(seed, 3.1));
    }
    float4 ov = overlay.Sample(pnt, uv);
    rgb = lerp(rgb, ov.rgb, ov.a);
    if (power < 1.0) {
        // Everything outside the opened raster is dark; the collapsed line glows white-hot.
        float inside_y = step(abs(c.y), open * 0.5) * step(abs(c.x), lerp(0.35, 0.5, open));
        float glow = exp(-abs(c.y) / (0.004 + open * 0.02)) * (1.0 - open);
        rgb = rgb * inside_y * (1.0 + (1.0 - open) * 1.2) + glow * 1.6;
    }
    return float4(rgb, 1);
}

// ---------------------------------------------------------------- CRT pass
// Ported from "Room Foundation" tv.js (three.js ShaderMaterial). The JS uniforms that were
// constants there are constants here; uResolution is the virtual tube resolution.

cbuffer CRT : register(b0) {
    float2 uResolution;
    float  uTime; // seconds
    float  uCurve;    // barrel distortion
    float  uScan;     // scanline depth 0..1
    float  uNoiseIntensity;
    float  uVignette;
    float  crt_pad;
};
Texture2D uScene : register(t0);

static const float uAmbientEmissive = 0.0;
static const float uBlackEmissive = 0.01;
static const float uResolutionScale = 1.0;
static const float uBrightness = 2.15;
static const float uScanlineInterference = 1.0;
static const float uVerticalLines = 483.0;
static const float uBorders = 1.0;
static const float uOutputGain = 2.35;

float3 pulseIntegral3(float3 x, float s1, float s2) { return clamp(x - s1, 0.0, s2 - s1); }

float3 bayer(float2 uv, float2 blur) {
    float3 x = uv.xxx;
    float3 y = uv.yyy;
    x += float3(0.66, 0.33, 0.0);
    y += 0.5 * step(frac(x * 0.5), 0.5);
    x = frac(x);
    y = frac(y);
    float2 size = float2(0.16, 0.75);
    float2 vMin = 0.5 - size * 0.5;
    float2 vMax = 0.5 + size * 0.5;
    float3 vx = (pulseIntegral3(x + blur.x, vMin.x, vMax.x) - pulseIntegral3(x - blur.x, vMin.x, vMax.x)) / max(blur.x, 1e-4);
    float3 vy = (pulseIntegral3(y + blur.y, vMin.y, vMax.y) - pulseIntegral3(y - blur.y, vMin.y, vMax.y)) / max(blur.y, 1e-4);
    return min(vx, vy) * 5.0;
}

float3 getPixelMatrix(float2 uv) {
    float2 dx = ddx(uv);
    float2 dy = ddy(uv);
    float dU = length(float2(dx.x, dy.x));
    float dV = length(float2(dx.y, dy.y));
    if (dU <= 0.0 || dV <= 0.0) return float3(1, 1, 1);
    return bayer(uv, float2(dU, dV));
}

float scanline(float y, float blur) {
    float s = sin(y * 10.0) * 0.45 + 0.55;
    s = lerp(1.0, s, uScan);
    return lerp(s, 1.0, min(1.0, blur));
}

float getScanline(float2 uv) {
    uv.y *= 0.25;
    float2 dx = ddx(uv);
    float2 dy = ddy(uv);
    float dV = length(float2(dx.y, dy.y));
    if (dV <= 0.0) return 1.0;
    return scanline(uv.y, dV * 1.3);
}

float interferenceHash(float p) {
    float3 p3 = frac(p.xxx * 0.1031);
    p3 += dot(p3, p3.yzx + 19.19);
    return frac((p3.x + p3.y) * p3.z);
}

float interferenceSmoothNoise1D(float x) {
    float f0 = floor(x);
    float fr = frac(x);
    return lerp(interferenceHash(f0), interferenceHash(f0 + 1.0), fr);
}

float2 getInterference(float2 uv) {
    float scanLine = floor(uv.y * uVerticalLines);
    float scanPos = scanLine + uv.x;
    float timeSeed = frac(uTime * 123.78);
    float noise = interferenceSmoothNoise1D(scanPos * 234.5 + timeSeed * 12345.6);
    float scanRnd = interferenceHash(uv.y * 100.0 + frac(uTime * 1234.0) * 12345.0);
    return float2(noise, scanRnd);
}

float3 sampleScreen(float2 uv) {
    float2 resolution = uResolution * uResolutionScale;
    float2 pixelCoord = uv * resolution;
    float3 pixelMatrix = getPixelMatrix(pixelCoord);
    float scan = getScanline(pixelCoord);
    float2 texUV = floor(uv * resolution * 2.0) / (resolution * 2.0);
    float2 interference = getInterference(texUV);
    if (uScanlineInterference > 0.5) {
        texUV.x += (interference.y * 2.0 - 1.0) * 0.025 * uNoiseIntensity;
    }
    // Outside the tube face is bezel (black), not smeared edge pixels.
    float3 col = (any(texUV < 0.0) || any(texUV > 1.0)) ? float3(0, 0, 0) : uScene.SampleLevel(lin, texUV, 0).rgb;
    col = clamp(col + (interference.x - 0.5) * 2.0 * uNoiseIntensity, 0.0, 1.0);
    float3 result = (col * col * uBrightness + uBlackEmissive) * pixelMatrix * scan + uAmbientEmissive;
    return result / (1.0 + uBrightness + uBlackEmissive);
}

float4 ps_crt(VSOut i) : SV_Target {
    // Barrel curvature: the tube face bulges, so straight lines bow outward at the edges.
    float2 p = i.uv * 2.0 - 1.0;
    float r2 = dot(p, p);
    p *= 1.0 + uCurve * r2;
    float2 uv = p * 0.5 + 0.5;
    float2 suv = uBorders > 0.5 ? uv * 1.1 - 0.05 : uv;
    float3 col = sampleScreen(suv);
    col *= 1.0 - uVignette * r2; // vignette: phosphor is dimmer toward the corners
    float radius = 0.17;
    float2 b = 1.0 - radius;
    float2 d = abs(p) - b;
    float dist = length(max(d, 0.0)) - radius;
    float cornerMask = 1.0 - smoothstep(-0.02, 0.035, dist);
    col *= cornerMask;
    col = sqrt(max(col, 0.0));
    col = pow(col, 0.95);
    col *= uOutputGain;
    return float4(col, 1.0);
}
"#;

#[repr(C)]
struct CrtCb {
    resolution: [f32; 2],
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

#[repr(C)]
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

/// Per-frame analog effects for `render`.
#[derive(Clone, Copy)]
pub struct Fx {
    pub snow: f32,      // 0..1 snow opacity
    pub snow_full: bool,
    pub seed: f32,
    pub crt: bool,
    pub crt_params: CrtParams,
    pub time: f32,
    pub power: f32,     // 0..1 tube warm-up / collapse
    pub settle: f32,    // 1..0 lock-in jitter after a tune
}

/// One decoded picture, either still on the GPU or as CPU planes.
pub enum Picture<'a> {
    /// D3D11VA output: an NV12 texture array and the slice holding this frame.
    Nv12 { texture: *mut c_void, index: u32, width: u32, height: u32 },
    /// Software decode output (yuv420p).
    Yuv420p { planes: [&'a [u8]; 3], strides: [usize; 3], width: u32, height: u32 },
}

pub struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    lin: ID3D11SamplerState,
    pnt: ID3D11SamplerState,
    tearing: bool,
    width: u32,
    height: u32,
    overlay: ID3D11Texture2D,
    overlay_srv: ID3D11ShaderResourceView,
    // Optional CRT pass: composite goes to `scene`, then ps_crt draws it to the backbuffer.
    ps_crt: ID3D11PixelShader,
    cb_crt: ID3D11Buffer,
    scene: Option<(ID3D11Texture2D, ID3D11RenderTargetView, ID3D11ShaderResourceView)>,
    // Our own shader-readable NV12 copy of the decoder's surface (decoder textures are DECODER-bind only).
    nv12: Option<(u32, u32, ID3D11Texture2D, ID3D11ShaderResourceView, ID3D11ShaderResourceView)>,
    yuv: Option<(u32, u32, [ID3D11Texture2D; 3], [ID3D11ShaderResourceView; 3])>,
}

// D3D11 devices are free-threaded; we only ever use the Gpu from the UI thread anyway.
unsafe impl Send for Gpu {}

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut err: Option<ID3DBlob> = None;
        let r = D3DCompile(
            SHADER.as_ptr() as *const c_void,
            SHADER.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut err),
        );
        if let Err(e) = r {
            if let Some(err) = err {
                let msg = std::slice::from_raw_parts(err.GetBufferPointer() as *const u8, err.GetBufferSize());
                eprintln!("shader compile error: {}", String::from_utf8_lossy(msg));
            }
            return Err(e);
        }
        let code = code.unwrap();
        Ok(std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()).to_vec())
    }
}

fn tex_desc(w: u32, h: u32, format: DXGI_FORMAT, dynamic: bool) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if dynamic { D3D11_USAGE_DYNAMIC } else { D3D11_USAGE_DEFAULT },
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: if dynamic { D3D11_CPU_ACCESS_WRITE.0 as u32 } else { 0 },
        MiscFlags: 0,
    }
}

impl Gpu {
    /// Wrap the ID3D11Device ffmpeg created for D3D11VA (adds a reference; ffmpeg keeps its own).
    pub unsafe fn device_from_raw(ptr: *mut c_void) -> ID3D11Device {
        (*ManuallyDrop::new(ID3D11Device::from_raw(ptr))).clone()
    }

    /// Stand-alone device for the software-decode fallback.
    pub fn create_device() -> Result<ID3D11Device> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            Ok(device.unwrap())
        }
    }

    pub fn new(device: ID3D11Device, hwnd: HWND, width: u32, height: u32) -> Result<Gpu> {
        unsafe {
            let ctx = device.GetImmediateContext()?;
            let dxgi_dev: IDXGIDevice = device.cast()?;
            let adapter = dxgi_dev.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let mut tearing = false;
            if let Ok(f5) = factory.cast::<IDXGIFactory5>() {
                let mut allow: u32 = 0;
                if f5.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut u32 as *mut c_void, 4).is_ok() {
                    tearing = allow != 0;
                }
            }
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 },
            };
            let swapchain = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?;
            // We pace frames ourselves; don't let DXGI queue up presents behind our back.
            if let Ok(sc2) = swapchain.cast::<IDXGISwapChain2>() {
                let _ = sc2.SetMaximumFrameLatency(1);
            }

            let vsb = compile(s!("vs"), s!("vs_5_0"))?;
            let psb = compile(s!("ps"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(&psb, None, Some(&mut ps))?;
            let crtb = compile(s!("ps_crt"), s!("ps_5_0"))?;
            let mut ps_crt = None;
            device.CreatePixelShader(&crtb, None, Some(&mut ps_crt))?;

            let make_cb = |bytes: usize| -> Result<ID3D11Buffer> {
                let mut cb = None;
                device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: bytes as u32,
                        Usage: D3D11_USAGE_DYNAMIC,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut cb),
                )?;
                Ok(cb.unwrap())
            };
            let cb = Some(make_cb(std::mem::size_of::<Cb>())?);
            let cb_crt = make_cb(std::mem::size_of::<CrtCb>())?;

            let sampler = |filter: D3D11_FILTER| -> Result<ID3D11SamplerState> {
                let mut s = None;
                device.CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: filter,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        MipLODBias: 0.0,
                        MaxAnisotropy: 1,
                        ComparisonFunc: D3D11_COMPARISON_NEVER,
                        BorderColor: [0.0; 4],
                        MinLOD: 0.0,
                        MaxLOD: 0.0,
                    },
                    Some(&mut s),
                )?;
                Ok(s.unwrap())
            };
            let lin = sampler(D3D11_FILTER_MIN_MAG_MIP_LINEAR)?;
            let pnt = sampler(D3D11_FILTER_MIN_MAG_MIP_POINT)?;

            let (overlay, overlay_srv) = Self::make_overlay(&device, width, height)?;
            let mut gpu = Gpu {
                device,
                ctx,
                swapchain,
                rtv: None,
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                cb: cb.unwrap(),
                lin,
                pnt,
                tearing,
                width,
                height,
                overlay,
                overlay_srv,
                ps_crt: ps_crt.unwrap(),
                cb_crt,
                scene: None,
                nv12: None,
                yuv: None,
            };
            gpu.make_rtv()?;
            gpu.make_scene()?;
            Ok(gpu)
        }
    }

    fn make_overlay(device: &ID3D11Device, w: u32, h: u32) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        unsafe {
            let mut tex = None;
            device.CreateTexture2D(&tex_desc(w.max(1), h.max(1), DXGI_FORMAT_B8G8R8A8_UNORM, true), None, Some(&mut tex))?;
            let tex = tex.unwrap();
            let mut srv = None;
            device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            Ok((tex, srv.unwrap()))
        }
    }

    fn make_rtv(&mut self) -> Result<()> {
        unsafe {
            let bb: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let mut rtv = None;
            self.device.CreateRenderTargetView(&bb, None, Some(&mut rtv))?;
            self.rtv = rtv;
            Ok(())
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if w == 0 || h == 0 || (w == self.width && h == self.height) {
            return Ok(());
        }
        unsafe {
            self.rtv = None;
            self.ctx.OMSetRenderTargets(None, None);
            let flags = if self.tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
            self.swapchain.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, flags)?;
            self.width = w;
            self.height = h;
            self.make_rtv()?;
            let (t, s) = Self::make_overlay(&self.device, w, h)?;
            self.overlay = t;
            self.overlay_srv = s;
            self.make_scene()?;
        }
        Ok(())
    }

    /// Offscreen composite target for the CRT pass (window-sized BGRA).
    fn make_scene(&mut self) -> Result<()> {
        unsafe {
            let mut desc = tex_desc(self.width.max(1), self.height.max(1), DXGI_FORMAT_B8G8R8A8_UNORM, false);
            desc.BindFlags |= D3D11_BIND_RENDER_TARGET.0 as u32;
            let mut tex = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.unwrap();
            let mut rtv = None;
            self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
            let mut srv = None;
            self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            self.scene = Some((tex, rtv.unwrap(), srv.unwrap()));
            Ok(())
        }
    }

    fn upload(&self, tex: &ID3D11Texture2D, rows: usize, row_bytes: usize, src: &[u8], src_stride: usize) -> Result<()> {
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            let dst = m.pData as *mut u8;
            let pitch = m.RowPitch as usize;
            for y in 0..rows {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(y * src_stride), dst.add(y * pitch), row_bytes);
            }
            self.ctx.Unmap(tex, 0);
        }
        Ok(())
    }

    /// Replace the overlay (window-sized ARGB, straight alpha; 0 = transparent).
    pub fn upload_overlay(&self, argb: &[u32]) -> Result<()> {
        let bytes = unsafe { std::slice::from_raw_parts(argb.as_ptr() as *const u8, argb.len() * 4) };
        let row = self.width as usize * 4;
        self.upload(&self.overlay, self.height as usize, row, bytes, row)
    }

    fn srv_for(&self, tex: &ID3D11Texture2D, format: DXGI_FORMAT) -> Result<ID3D11ShaderResourceView> {
        unsafe {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
            };
            let mut srv = None;
            self.device.CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))?;
            Ok(srv.unwrap())
        }
    }

    /// Bind the picture's planes to t0..t2, uploading or copying as needed. Returns the shader mode.
    fn bind_picture(&mut self, pic: &Picture) -> Result<f32> {
        unsafe {
            match *pic {
                Picture::Nv12 { texture, index, width, height } => {
                    if self.nv12.as_ref().is_none_or(|n| n.0 != width || n.1 != height) {
                        let mut tex = None;
                        self.device.CreateTexture2D(&tex_desc(width, height, DXGI_FORMAT_NV12, false), None, Some(&mut tex))?;
                        let tex = tex.unwrap();
                        let y = self.srv_for(&tex, DXGI_FORMAT_R8_UNORM)?;
                        let uv = self.srv_for(&tex, DXGI_FORMAT_R8G8_UNORM)?;
                        self.nv12 = Some((width, height, tex, y, uv));
                    }
                    let (_, _, tex, y, uv) = self.nv12.as_ref().unwrap();
                    let src = ManuallyDrop::new(ID3D11Texture2D::from_raw(texture));
                    self.ctx.CopySubresourceRegion(tex, 0, 0, 0, 0, &*src, index, None);
                    self.ctx.PSSetShaderResources(0, Some(&[Some(y.clone()), Some(uv.clone()), None, Some(self.overlay_srv.clone())]));
                    Ok(0.0)
                }
                Picture::Yuv420p { planes, strides, width, height } => {
                    if self.yuv.as_ref().is_none_or(|n| n.0 != width || n.1 != height) {
                        let mk = |w: u32, h: u32| -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
                            let mut tex = None;
                            self.device.CreateTexture2D(&tex_desc(w, h, DXGI_FORMAT_R8_UNORM, true), None, Some(&mut tex))?;
                            let tex = tex.unwrap();
                            let srv = self.srv_for(&tex, DXGI_FORMAT_R8_UNORM)?;
                            Ok((tex, srv))
                        };
                        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                        let (ty, sy) = mk(width, height)?;
                        let (tu, su) = mk(cw, ch)?;
                        let (tv, sv) = mk(cw, ch)?;
                        self.yuv = Some((width, height, [ty, tu, tv], [sy, su, sv]));
                    }
                    let (_, _, texs, srvs) = self.yuv.as_ref().unwrap();
                    let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
                    self.upload(&texs[0], height as usize, width as usize, planes[0], strides[0])?;
                    self.upload(&texs[1], ch, cw, planes[1], strides[1])?;
                    self.upload(&texs[2], ch, cw, planes[2], strides[2])?;
                    self.ctx.PSSetShaderResources(0, Some(&[Some(srvs[0].clone()), Some(srvs[1].clone()), Some(srvs[2].clone()), Some(self.overlay_srv.clone())]));
                    Ok(1.0)
                }
            }
        }
    }

    /// Draw one frame. `rect` is the video rect in 0..1 window coords (x0, y0, x1, y1).
    /// With `fx.crt` the composite goes through the CRT pass; `fx.time` drives its interference.
    pub fn render(&mut self, pic: Option<&Picture>, rect: [f32; 4], fx: Fx) -> Result<()> {
        let Fx { snow, snow_full, seed, crt, crt_params, time, power, settle } = fx;
        unsafe {
            let mode = match pic {
                Some(p) => self.bind_picture(p)?,
                None => {
                    self.ctx.PSSetShaderResources(0, Some(&[None, None, None, Some(self.overlay_srv.clone())]));
                    0.0
                }
            };
            let cb = Cb { rect, snow, seed, mode, snow_full: if snow_full { 1.0 } else { 0.0 }, has_video: if pic.is_some() { 1.0 } else { 0.0 }, power, settle, _pad: 0.0 };
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            std::ptr::copy_nonoverlapping(&cb as *const Cb as *const u8, m.pData as *mut u8, std::mem::size_of::<Cb>());
            self.ctx.Unmap(&self.cb, 0);

            let target = if crt { self.scene.as_ref().map(|s| s.1.clone()) } else { self.rtv.clone() };
            self.ctx.OMSetRenderTargets(Some(&[target]), None);
            self.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            self.ctx.IASetInputLayout(None);
            self.ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx.PSSetSamplers(0, Some(&[Some(self.lin.clone()), Some(self.pnt.clone())]));
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.ctx.Draw(3, 0);
            self.ctx.PSSetShaderResources(0, Some(&[None, None, None, None]));
            if crt {
                if let Some((_, _, scene_srv)) = &self.scene {
                    // 480 scanlines always; horizontal count follows the window aspect so the
                    // virtual pixels stay square (853x480 on 16:9) instead of stretching.
                    let aspect = self.width as f32 / self.height.max(1) as f32;
                    let ccb = CrtCb {
                        resolution: [(480.0 * aspect).round(), 480.0],
                        time,
                        curve: crt_params.curve,
                        scan: crt_params.scan,
                        noise: crt_params.noise,
                        vignette: crt_params.vignette,
                        _pad: 0.0,
                    };
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    self.ctx.Map(&self.cb_crt, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
                    std::ptr::copy_nonoverlapping(&ccb as *const CrtCb as *const u8, m.pData as *mut u8, std::mem::size_of::<CrtCb>());
                    self.ctx.Unmap(&self.cb_crt, 0);
                    self.ctx.OMSetRenderTargets(Some(&[self.rtv.clone()]), None);
                    self.ctx.PSSetShader(&self.ps_crt, None);
                    self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb_crt.clone())]));
                    self.ctx.PSSetShaderResources(0, Some(&[Some(scene_srv.clone())]));
                    self.ctx.Draw(3, 0);
                    self.ctx.PSSetShaderResources(0, Some(&[None]));
                }
            }
            let flags = if self.tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
            self.swapchain.Present(0, flags).ok()?;
        }
        Ok(())
    }

    /// Copy the current backbuffer to the CPU (BGRA). Debug/bench only.
    pub fn read_back(&self) -> Result<(u32, u32, Vec<u8>)> {
        unsafe {
            let bb: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            bb.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.unwrap();
            self.ctx.CopyResource(&staging, &bb);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let (w, h) = (desc.Width as usize, desc.Height as usize);
            let mut out = vec![0u8; w * h * 4];
            for y in 0..h {
                std::ptr::copy_nonoverlapping((m.pData as *const u8).add(y * m.RowPitch as usize), out.as_mut_ptr().add(y * w * 4), w * 4);
            }
            self.ctx.Unmap(&staging, 0);
            Ok((desc.Width, desc.Height, out))
        }
    }
}
