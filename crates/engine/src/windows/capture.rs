//! The cursor uses a pixel shader compiled by the system d3dcompiler_47.dll.
//! This preserves XOR cursors without reading the desktop back to the CPU.
use super::display;
use super::{
    elevated,
    frames::{Frames, Resized},
};
use crate::windows_cursor::{CaptureState, Control, Owner, Plane, PointerSample, ShapeInfo};
use crate::{
    input::DisplayRect,
    software::{Bgra, CpuCapture},
};
use anyhow::{Context, Result};
use std::sync::{mpsc, Arc};
use std::{
    mem::ManuallyDrop,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{s, Interface},
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::{Fxc::*, *},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING},
    },
};

#[derive(Clone)]
pub(super) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: (i32, u32),
}
impl Gpu {
    pub fn new(adapter: &IDXGIAdapter1) -> Result<Self> {
        let (mut device, mut context) = (None, None);
        // SAFETY: Output pointers are valid; an explicit adapter requires UNKNOWN.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("create capture D3D11 device")?;
        let gpu = Self {
            device: device.context("D3D11 returned no device")?,
            context: context.context("D3D11 returned no context")?,
            // SAFETY: adapter is a live owned DXGI interface.
            adapter: unsafe {
                let luid = adapter.GetDesc1()?.AdapterLuid;
                (luid.HighPart, luid.LowPart)
            },
        };
        // SAFETY: MF may call the immediate context from driver threads.
        unsafe {
            let _ = gpu
                .context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
        }
        Ok(gpu)
    }
    pub fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: u32,
        staging: bool,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            BindFlags: if staging { 0 } else { bind },
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_READ.0 as u32
            } else {
                0
            },
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: Descriptor and output pointer are valid for this call.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("allocate D3D11 texture")?;
        texture.context("D3D11 returned no texture")
    }
    /// Runs `use_rows` on the first `rows` rows of `texture` as the CPU sees
    /// them, with their pitch. `staging` keeps the CPU-readable copy between
    /// calls; it is replaced when the texture's size or format changes.
    pub fn read<R>(
        &self,
        staging: &mut Option<ID3D11Texture2D>,
        texture: &ID3D11Texture2D,
        rows: usize,
        use_rows: impl FnOnce(&[u8], usize) -> R,
    ) -> Result<R> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: The texture and output descriptor are valid.
        unsafe {
            texture.GetDesc(&mut desc);
        }
        let mut have = D3D11_TEXTURE2D_DESC::default();
        if let Some(staging) = staging {
            // SAFETY: As above.
            unsafe { staging.GetDesc(&mut have) };
        }
        let stale = staging.is_none()
            || (have.Width, have.Height, have.Format) != (desc.Width, desc.Height, desc.Format);
        if stale {
            *staging = Some(self.texture(desc.Width, desc.Height, desc.Format, 0, true)?);
        }
        let staging = staging.as_ref().context("missing staging texture")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: CopyResource is ordered before Map. Map is paired with Unmap;
        // the row pitch describes all accessible bytes handed to `use_rows`.
        unsafe {
            self.context.CopyResource(staging, texture);
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map staging texture")?;
            let result = use_rows(
                std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>(),
                    mapped.RowPitch as usize * rows,
                ),
                mapped.RowPitch as usize,
            );
            self.context.Unmap(staging, 0);
            Ok(result)
        }
    }
}
struct CursorRenderer {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    shape: Option<ID3D11ShaderResourceView>,
    /// Views of the textures last drawn from and to, made once: a view holds
    /// its texture, so one cannot be mistaken for a new texture at the
    /// same address, and a new texture (after a resize) gets new views.
    source: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
}
const SHADER: &str = r#"
Texture2D<float4> desktop : register(t0);
Texture2D<uint> shape : register(t1);
cbuffer C : register(b0) { int2 origin; uint2 size; uint kind; uint rotation; uint2 rawsize; uint hdr; float white; uint output_pq; uint padding; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    return float4(id == 2 ? 3 : -1, id == 1 ? 3 : -1, 0, 1);
}
float3 pq(float3 lin) {
    float3 n = pow(clamp(lin,0,1000)/10000.0, 2610.0/16384.0);
    return pow((3424.0/4096.0 + (2413.0/128.0)*n)/(1+(2392.0/128.0)*n),2523.0/32.0);
}
float3 wide(float3 rgb) {
    return mul(float3x3(0.627404,0.329283,0.043313,0.069097,0.919540,0.011362,0.016391,0.088013,0.895595),rgb);
}
// latch_proto::color::hdr_to_sdr: BT.709 gamut toward the colour's own
// luminance, then a highlight shoulder on max(R,G,B) up to `peak`.
float3 hdr_to_sdr(float3 c, float peak) {
    float y = dot(c, float3(0.2126,0.7152,0.0722));
    if(y <= 0) return 0;
    float lo = min(min(c.r,c.g),c.b);
    if(lo < 0) c = y + (c - y)*(y/(y - lo));
    float m = max(max(c.r,c.g),c.b);
    if(m <= 0.75) return c;
    float x = (m - 0.75)/0.25, top = max((peak - 0.75)/0.25, 1.0);
    float g = min(x*(1 + x/(top*top))/(1 + x), 1.0);
    return saturate(c*((0.75 + 0.25*g)/m));
}
float3 linearize(float3 c) { return float3(c.r <= 0.04045 ? c.r/12.92 : pow((c.r+0.055)/1.055,2.4),c.g <= 0.04045 ? c.g/12.92 : pow((c.g+0.055)/1.055,2.4),c.b <= 0.04045 ? c.b/12.92 : pow((c.b+0.055)/1.055,2.4)); }
float4 ps(float4 p : SV_Position) : SV_Target {
    int2 pos = int2(p.xy); int2 src = pos;
    if(rotation == 2) src = int2(pos.y, rawsize.y - 1 - pos.x);
    if(rotation == 3) src = int2(rawsize.x - 1 - pos.x, rawsize.y - 1 - pos.y);
    if(rotation == 4) src = int2(rawsize.x - 1 - pos.y, pos.x);
    float4 bg = desktop.Load(int3(src,0));
    if(output_pq != 0) {
        bg.rgb = pq(wide(hdr != 0 ? bg.rgb*80.0 : linearize(bg.rgb)*white*80.0));
    } else if(hdr != 0) {
        // scRGB is linear BT.709, with SDR white given by the display's white
        // level. Keep highlights up to the 1000-nit HDR ceiling apart and in
        // hue rather than clipping each channel, then apply the sRGB transfer
        // before cursor blending.
        float3 lin = hdr_to_sdr(bg.rgb / white, 1000.0/80.0/white);
        bg.rgb = float3(lin.r <= 0.0031308 ? 12.92*lin.r : 1.055*pow(lin.r,1.0/2.4)-0.055,
                        lin.g <= 0.0031308 ? 12.92*lin.g : 1.055*pow(lin.g,1.0/2.4)-0.055,
                        lin.b <= 0.0031308 ? 12.92*lin.b : 1.055*pow(lin.b,1.0/2.4)-0.055);
    }
    int2 c = pos - origin;
    if(kind == 0 || any(c < 0) || any(c >= int2(size))) return bg;
    uint v = shape.Load(int3(c,0));
    uint3 rgb = uint3((v >> 16) & 255, (v >> 8) & 255, v & 255);
    uint a = v >> 24;
    if(output_pq != 0) {
        float3 cursor = pq(wide(linearize(float3(rgb)/255.0)*white*80.0));
        if(kind == 2) return float4(cursor*(a/255.0)+bg.rgb*(1-a/255.0),1);
        if(kind == 4) return a == 255 ? float4(abs(bg.rgb-cursor),1) : float4(cursor,1);
        uint3 screen = uint3(round(saturate(bg.rgb)*255));
        return float4(float3((screen & a) ^ rgb)/255.0,1);
    }
    if(kind == 2) return float4(float3(rgb)*(a/65025.0) + bg.rgb*(1-a/255.0), 1);
    uint3 screen = uint3(round(saturate(bg.rgb)*255));
    if(kind == 4) rgb = a == 255 ? screen ^ rgb : rgb;
    if(kind == 1) rgb = (screen & a) ^ rgb;
    return float4(float3(rgb)/255.0, 1);
}
"#;
fn compile_shader(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    // SAFETY: Shader source and entry names live through compilation;
    // compilation uses only the Windows system compiler, no native build.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors
            .map(|e| {
                // SAFETY: The compiler blob owns BufferSize readable bytes.
                String::from_utf8_lossy(unsafe {
                    std::slice::from_raw_parts(e.GetBufferPointer().cast::<u8>(), e.GetBufferSize())
                })
                .into_owned()
            })
            .unwrap_or_default();
        return Err(e).with_context(|| format!("compile cursor shader: {detail}"));
    }
    code.context("compiler returned no shader")
}

impl CursorRenderer {
    fn new(gpu: &Gpu) -> Result<Self> {
        let vs = compile_shader(s!("vs"), s!("vs_4_0"))?;
        let ps = compile_shader(s!("ps"), s!("ps_4_0"))?;
        let (mut vertex, mut pixel, mut constants) = (None, None, None);
        // SAFETY: Blobs own valid shader bytecode; all output slots are valid.
        unsafe {
            gpu.device
                .CreateVertexShader(
                    std::slice::from_raw_parts(vs.GetBufferPointer().cast(), vs.GetBufferSize()),
                    None,
                    Some(&mut vertex),
                )
                .context("create cursor vertex shader")?;
            gpu.device
                .CreatePixelShader(
                    std::slice::from_raw_parts(ps.GetBufferPointer().cast(), ps.GetBufferSize()),
                    None,
                    Some(&mut pixel),
                )
                .context("create cursor pixel shader")?;
            gpu.device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 48,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut constants),
                )
                .context("create cursor constants")?;
        }
        Ok(Self {
            vertex: vertex.context("missing vertex shader")?,
            pixel: pixel.context("missing pixel shader")?,
            constants: constants.context("missing cursor buffer")?,
            shape: None,
            source: None,
            target: None,
        })
    }
    fn shape(
        &mut self,
        gpu: &Gpu,
        info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        data: &[u8],
    ) -> Result<()> {
        let metadata = shape_info(info);
        let (_, height) = metadata.size().context("invalid DXGI pointer dimensions")?;
        let pixels = metadata
            .pixels(data)
            .context("invalid DXGI pointer buffer")?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: info.Width,
            Height: height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_UINT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: info.Width * 4,
            ..Default::default()
        };
        let mut texture = None;
        let mut view = None;
        // SAFETY: Initial data covers every row. CreateTexture2D copies it.
        unsafe {
            gpu.device
                .CreateTexture2D(&desc, Some(&initial), Some(&mut texture))
                .context("upload DXGI cursor")?;
            gpu.device
                .CreateShaderResourceView(
                    &texture.context("missing cursor texture")?,
                    None,
                    Some(&mut view),
                )
                .context("create cursor view")?;
        }
        self.shape = view;
        Ok(())
    }
    fn draw(
        &mut self,
        gpu: &Gpu,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
        cursor: &DXGI_OUTDUPL_POINTER_POSITION,
        shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        color: (DXGI_MODE_ROTATION, f32, bool),
    ) -> Result<()> {
        let (rotation, white, output_pq) = color;
        let (mut raw, mut dst) = (
            D3D11_TEXTURE2D_DESC::default(),
            D3D11_TEXTURE2D_DESC::default(),
        );
        // SAFETY: Views reference owned textures and constants contain exactly 48 bytes.
        unsafe {
            source.GetDesc(&mut raw);
            target.GetDesc(&mut dst);
            if self
                .source
                .as_ref()
                .is_none_or(|(texture, _)| texture.as_raw() != source.as_raw())
            {
                let mut view = None;
                gpu.device
                    .CreateShaderResourceView(source, None, Some(&mut view))
                    .context("desktop shader view")?;
                self.source = Some((source.clone(), view.context("missing desktop view")?));
            }
            if self
                .target
                .as_ref()
                .is_none_or(|(texture, _)| texture.as_raw() != target.as_raw())
            {
                let mut view = None;
                gpu.device
                    .CreateRenderTargetView(target, None, Some(&mut view))
                    .context("cursor render target")?;
                self.target = Some((target.clone(), view.context("missing cursor target")?));
            }
            let srv = self.source.as_ref().map(|(_, view)| view.clone());
            let rtv = self.target.as_ref().map(|(_, view)| view.clone());
            let constants: [u32; 12] = [
                cursor.Position.x as u32,
                cursor.Position.y as u32,
                shape.Width,
                if shape.Type == 1 {
                    shape.Height / 2
                } else {
                    shape.Height
                },
                if cursor.Visible.as_bool() && self.shape.is_some() {
                    shape.Type
                } else {
                    0
                },
                rotation.0 as u32,
                raw.Width,
                raw.Height,
                u32::from(raw.Format == DXGI_FORMAT_R16G16B16A16_FLOAT),
                white.to_bits(),
                u32::from(output_pq),
                0,
            ];
            gpu.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                constants.as_ptr().cast(),
                0,
                0,
            );
            gpu.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            gpu.context.VSSetShader(&self.vertex, None);
            gpu.context.PSSetShader(&self.pixel, None);
            gpu.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            gpu.context
                .PSSetShaderResources(0, Some(&[srv, self.shape.clone()]));
            gpu.context.OMSetRenderTargets(Some(&[rtv]), None);
            gpu.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: dst.Width as f32,
                Height: dst.Height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            gpu.context.Draw(3, 0);
            gpu.context.PSSetShaderResources(0, Some(&[None, None]));
            gpu.context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}
fn shape_info(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> ShapeInfo {
    ShapeInfo {
        kind: info.Type,
        width: info.Width,
        height: info.Height,
        pitch: info.Pitch,
        hot_x: info.HotSpot.x,
        hot_y: info.HotSpot.y,
    }
}

/// Which output a capture duplicates.
#[derive(Clone)]
pub(super) enum Target {
    /// The one the viewer chose (its monitor device path), else the default.
    Named(Option<String>),
    /// Exactly this one: the SYSTEM helper captures what the host names.
    Exact { luid: (i32, u32), name: [u16; 32] },
}
impl Target {
    fn select(&self) -> Result<display::Output> {
        match self {
            Self::Named(wanted) => display::select_named(wanted.as_deref()),
            Self::Exact { luid, name } => {
                // SAFETY: The factory and enumerated interfaces are owned COM references.
                unsafe {
                    let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
                    let adapter: IDXGIAdapter1 = factory
                        .EnumAdapterByLuid(LUID {
                            HighPart: luid.0,
                            LowPart: luid.1,
                        })
                        .context("find the stream's adapter")?;
                    for i in 0.. {
                        let output = match adapter.EnumOutputs(i) {
                            Ok(o) => o,
                            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                            Err(e) => return Err(e).context("enumerate outputs"),
                        };
                        let desc = output.GetDesc()?;
                        if desc.DeviceName == *name && desc.AttachedToDesktop.as_bool() {
                            return Ok(display::Output {
                                adapter,
                                output,
                                desc,
                                virtual_display: false,
                                id: String::new(),
                            });
                        }
                    }
                    anyhow::bail!("the stream's output is not attached to the desktop")
                }
            }
        }
    }
}

/// How long a stream waits for its own desktop to come back after Windows
/// took it, with no helper to show the secure desktop meanwhile.
const OWN_LIMIT: Duration = Duration::from_secs(5);
/// The same while the SYSTEM helper starts: the task and its agent in this
/// session, until it grants a mapping.
const HELPER_LIMIT: Duration = Duration::from_secs(15);
/// How long a mapping the helper granted may go without its first frame.
/// Counted from the grant: starting the helper may take longer than this.
const FIRST_FRAME: Duration = Duration::from_secs(5);
/// A loss that is not an access denial (a mode change, a driver reset)
/// usually ends by itself; the helper is asked only once it lasts this long.
const HELPER_AFTER: Duration = Duration::from_secs(1);

/// Where frames come from while the host cannot duplicate its own desktop.
/// Kept apart from the GPU work so the switching can be tested.
#[derive(Debug, Default)]
struct Fallback {
    /// When the host's own duplication went away (or, after a resize, when
    /// the helper was asked again); `None` while it works.
    lost: Option<Instant>,
    /// Setup installed the helper.
    installed: bool,
    /// The helper could not serve this loss: an older one, or it failed.
    /// The next loss asks again.
    failed: bool,
    phase: Phase,
}
/// How far the helper got with this loss.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    /// Asked; no mapping yet.
    Connecting,
    /// A mapping, granted then, but no frame in it yet. Until one comes,
    /// the stream ends [`FIRST_FRAME`] after the grant: a helper that
    /// connects and then sends nothing must not freeze the last picture
    /// forever.
    Awaiting(Instant),
    /// Frames came: the stream lasts as long as the secure desktop does,
    /// even while it is still and nothing new arrives.
    Serving,
}
impl Fallback {
    fn new(helper_installed: bool) -> Self {
        Self {
            installed: helper_installed,
            ..Default::default()
        }
    }
    /// No helper serves this loss, so the stream ends after [`OWN_LIMIT`]
    /// as it did before there was one.
    fn unavailable(&self) -> bool {
        !self.installed || self.failed
    }
    fn lose(&mut self, now: Instant) {
        self.lost.get_or_insert(now);
    }
    /// The host duplicates its own desktop again; any helper is let go.
    fn regain(&mut self) {
        self.lost = None;
        self.failed = false;
        self.phase = Phase::Idle;
    }
    /// Whether to ask the helper now. `denied`: the last attempt to
    /// duplicate was refused, as on the secure desktop.
    fn wants_helper(&self, now: Instant, denied: bool) -> bool {
        !self.unavailable()
            && self.phase == Phase::Idle
            && self
                .lost
                .is_some_and(|t| denied || now.saturating_duration_since(t) >= HELPER_AFTER)
    }
    fn connecting(&mut self) {
        self.phase = Phase::Connecting;
    }
    /// The helper sent a mapping at `now`; its first frame is still to come.
    fn connected(&mut self, now: Instant) {
        self.phase = Phase::Awaiting(now);
    }
    /// A frame from the helper arrived.
    fn served(&mut self) {
        self.phase = Phase::Serving;
    }
    fn serving(&self) -> bool {
        self.phase == Phase::Serving
    }
    /// The output changed size, which ends the helper's mapping: it is
    /// asked again, with a fresh [`HELPER_LIMIT`] for the new one.
    fn renegotiate(&mut self, now: Instant) {
        self.phase = Phase::Connecting;
        self.lost = Some(now);
    }
    fn failed(&mut self) {
        self.failed = true;
        self.phase = Phase::Idle;
    }
    /// The stream should end: neither its own desktop nor the helper's
    /// frames came back in time.
    fn expired(&self, now: Instant) -> bool {
        let Some(lost) = self.lost else {
            return false;
        };
        let since = |t: Instant| now.saturating_duration_since(t);
        if self.unavailable() {
            return since(lost) >= OWN_LIMIT;
        }
        match self.phase {
            Phase::Serving => false,
            Phase::Awaiting(granted) => since(granted) >= FIRST_FRAME,
            Phase::Idle | Phase::Connecting => since(lost) >= HELPER_LIMIT,
        }
    }
    /// Whether the last picture may be repeated meanwhile: only while the
    /// helper can still take over, so a stream without one never shows a
    /// stale desktop.
    fn keeps_picture(&self) -> bool {
        !self.unavailable()
    }
}

enum Helper {
    Off,
    Connecting(mpsc::Receiver<Result<Frames>>),
    Serving(Frames),
}

pub(super) struct Capture {
    plane: Plane,
    stream_size: (u32, u32),
    cursor_state: CaptureState,
    composite_pointer: DXGI_OUTDUPL_POINTER_POSITION,
    last_drawn: bool,
    pub hdr_output: bool,
    ten_bit: bool,
    pub gpu: Gpu,
    duplication: Option<IDXGIOutputDuplication>,
    source: ID3D11Texture2D,
    pub frame: ID3D11Texture2D,
    renderer: CursorRenderer,
    pointer: DXGI_OUTDUPL_POINTER_POSITION,
    shape: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    bitmap: Vec<u8>,
    output_id: String,
    output_name: [u16; 32],
    rotation: DXGI_MODE_ROTATION,
    pub display: DisplayRect,
    pub has_frame: bool,
    pub captured: Instant,
    cpu_seeded: bool,
    white: f32,
    retry: ReopenBackoff,
    staging: Option<ID3D11Texture2D>,
    /// The output to capture, kept across reopens.
    target: Target,
    fallback: Fallback,
    helper: Helper,
}
struct ReopenBackoff {
    at: Instant,
    delay: Duration,
}
impl ReopenBackoff {
    fn new() -> Self {
        Self {
            at: Instant::now(),
            delay: Duration::from_millis(50),
        }
    }
    fn defer(&mut self, now: Instant) {
        self.at = now + self.delay;
        self.delay = (self.delay * 2).min(Duration::from_secs(1));
    }
}
fn oriented_size(width: u32, height: u32, rotation: DXGI_MODE_ROTATION) -> (u32, u32) {
    if matches!(
        rotation,
        DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
    ) {
        (height, width)
    } else {
        (width, height)
    }
}
fn desktop_rect(desc: &DXGI_OUTDUPL_DESC, coordinates: RECT) -> DisplayRect {
    let (width, height) = oriented_size(desc.ModeDesc.Width, desc.ModeDesc.Height, desc.Rotation);
    DisplayRect {
        x: coordinates.left,
        y: coordinates.top,
        width,
        height,
    }
}
fn duplicate(output: &display::Output, gpu: &Gpu) -> Result<IDXGIOutputDuplication> {
    // SAFETY: the device belongs to this output. Once Output5 is available,
    // never fall back on a failed acquisition to legacy BGRA conversion: that
    // can silently turn an Advanced Color desktop black.
    unsafe {
        match output.output.cast::<IDXGIOutput5>() {
            Ok(output) => output.DuplicateOutput1(
                &gpu.device,
                0,
                &[
                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                ],
            ),
            Err(_) => output
                .output
                .cast::<IDXGIOutput1>()?
                .DuplicateOutput(&gpu.device),
        }
    }
    .map_err(|e| {
        // The lock screen, sign-in screen and administrator prompts are on
        // the secure desktop, which only SYSTEM may duplicate.
        let access_denied = e.code() == E_ACCESSDENIED;
        let e = anyhow::Error::new(e).context("duplicate desktop output");
        if access_denied {
            e.context(crate::video::DesktopUnavailable)
        } else {
            e
        }
    })
}
// SAFETY: D3D11/DXGI interfaces are free threaded and the immediate context has
// multithread protection. Capture is used exclusively by its video worker; no
// apartment-bound MF interfaces or COM initialization guard travel with it.
unsafe impl Send for Capture {}
impl Capture {
    #[cfg(test)]
    pub fn new() -> Result<Self> {
        Self::new_color(Default::default(), None)
    }
    /// Duplicates the output named `wanted` (see [`display::select_named`]).
    pub fn new_color(
        color: latch_proto::color::ColorDescription,
        wanted: Option<&str>,
    ) -> Result<Self> {
        let mut capture = Self::open_with(color, Target::Named(wanted.map(str::to_owned)), true)?;
        if capture.duplication.is_none() {
            // The secure desktop is up as the stream starts: it opens on the
            // helper's frames, or not at all.
            capture.first_helper_frame()?;
        }
        Ok(capture)
    }
    /// Duplicates `target`, as the SYSTEM helper does for a host.
    pub fn open(color: latch_proto::color::ColorDescription, target: Target) -> Result<Self> {
        Self::open_with(color, target, false)
    }
    /// `secure`: when Windows denies the duplication (the secure desktop),
    /// open without one and connect the helper, if it is set up.
    fn open_with(
        color: latch_proto::color::ColorDescription,
        target: Target,
        secure: bool,
    ) -> Result<Self> {
        let _dpi = display::DpiAwareness::physical();
        let _ = super::desktop::follow();
        let output = target.select()?;
        let ten_bit = color.bit_depth == 10;
        let hdr_output = color.is_hdr()
            && unsafe {
                output
                    .output
                    .cast::<IDXGIOutput6>()
                    .and_then(|o| o.GetDesc1())
                    .is_ok_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
            };
        let gpu = Gpu::new(&output.adapter)?;
        let installed = !super::desktop::following() && elevated::frames_installed();
        let (duplication, display, source_size, rotation) = match duplicate(&output, &gpu) {
            Ok(duplication) => {
                // SAFETY: The duplication owns its descriptor.
                let desc = unsafe { duplication.GetDesc() };
                (
                    Some(duplication),
                    desktop_rect(&desc, output.desc.DesktopCoordinates),
                    (
                        desc.ModeDesc.Width,
                        desc.ModeDesc.Height,
                        desc.ModeDesc.Format,
                    ),
                    desc.Rotation,
                )
            }
            Err(e) if secure && installed && crate::video::DesktopUnavailable::is(&e) => {
                // The helper's frames are upright BGRA of the desktop's size.
                let r = output.desc.DesktopCoordinates;
                let display = DisplayRect {
                    x: r.left,
                    y: r.top,
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                };
                anyhow::ensure!(display.width > 0 && display.height > 0, e);
                (
                    None,
                    display,
                    (display.width, display.height, DXGI_FORMAT_B8G8R8A8_UNORM),
                    output.desc.Rotation,
                )
            }
            Err(e) => return Err(e),
        };
        let source = gpu.texture(
            source_size.0,
            source_size.1,
            source_size.2,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let frame = gpu.texture(
            display.width,
            display.height,
            if ten_bit {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let renderer = CursorRenderer::new(&gpu)?;
        let mut fallback = Fallback::new(installed);
        if duplication.is_none() {
            fallback.lose(Instant::now());
        }
        Ok(Self {
            plane: Plane::new(Control::composited()),
            stream_size: (display.width, display.height),
            cursor_state: CaptureState::default(),
            composite_pointer: Default::default(),
            last_drawn: true,
            hdr_output,
            ten_bit,
            gpu,
            duplication,
            source,
            frame,
            renderer,
            pointer: Default::default(),
            shape: Default::default(),
            bitmap: Vec::new(),
            output_id: output.id.clone(),
            output_name: output.desc.DeviceName,
            rotation,
            display,
            has_frame: false,
            captured: Instant::now(),
            cpu_seeded: false,
            white: display::sdr_white_level(&output.desc.DeviceName),
            retry: ReopenBackoff::new(),
            staging: None,
            target,
            fallback,
            helper: Helper::Off,
        })
    }
    /// Opening on the secure desktop: waits for the helper's first frame,
    /// or fails as a denied duplication does without a helper. A resize
    /// meanwhile asks the helper again for a mapping of the new size.
    fn first_helper_frame(&mut self) -> Result<()> {
        let unavailable = || {
            anyhow::Error::msg("duplicate desktop output: the secure desktop is up")
                .context(crate::video::DesktopUnavailable)
        };
        let asked = Instant::now();
        loop {
            let frames = match elevated::connect_frames(self.want()) {
                Ok(frames) => frames,
                Err(e) => {
                    tracing::warn!(
                        "the secure desktop is up and the screen helper did not answer: {e:#}"
                    );
                    return Err(unavailable());
                }
            };
            self.helper = Helper::Serving(frames);
            // The first frame's time starts at the grant: starting a cold
            // helper may itself take longer than that.
            self.fallback.connected(Instant::now());
            let mut resized = false;
            while !self.fallback.expired(Instant::now()) {
                match self.helper_frame(Duration::from_millis(100)) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(e) if e.is::<Resized>() => {
                        resized = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("the screen helper stopped: {e:#}");
                        break;
                    }
                }
            }
            self.helper = Helper::Off;
            if !resized || asked.elapsed() >= HELPER_LIMIT {
                return Err(unavailable());
            }
            self.fallback.renegotiate(Instant::now());
        }
    }
    fn want(&self) -> super::frames::Want {
        super::frames::Want {
            luid: self.gpu.adapter,
            name: self.output_name,
        }
    }
    pub fn cursor_control(&mut self, control: Arc<Control>, size: (u32, u32)) {
        self.plane = Plane::new(control);
        self.stream_size = size;
    }
    fn cursor_status(&mut self) -> Option<bool> {
        let _dpi = display::DpiAwareness::physical();
        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: cursor is initialized with the SDK structure size.
        let known = unsafe { GetCursorInfo(&mut cursor) }.is_ok();
        let (_, on_output) = crate::windows_cursor::position(
            [cursor.ptScreenPos.x, cursor.ptScreenPos.y],
            self.display,
            self.stream_size,
        );
        let (visible, embedded) = crate::windows_cursor::visibility(
            known.then_some(cursor.flags == CURSOR_SHOWING),
            on_output,
            self.pointer.Visible.as_bool(),
            self.cursor_state.pointer_known,
        );
        self.composite_pointer = self.pointer;
        if known && !visible {
            self.composite_pointer.Visible = false.into();
        }
        // Keep a real system shape in the compositor whenever duplication has
        // no readable shape. A fallback shape never authorizes a separate plane.
        if visible
            && !embedded
            && (self.renderer.shape.is_none()
                || self.cursor_state.fallback_needed
                || !self.cursor_state.pointer_known)
        {
            let now = Instant::now();
            if (self.renderer.shape.is_none() || self.cursor_state.fallback_needed)
                && self
                    .cursor_state
                    .fallback_due(cursor.hCursor.0 as usize, now)
            {
                let mut actual_shape = true;
                let result = super::cursor::system_shape(cursor.hCursor)
                    .or_else(|_| {
                        actual_shape = false;
                        // SAFETY: LoadCursorW borrows the shared system arrow.
                        let arrow = unsafe {
                            windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
                                None,
                                windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
                            )
                        }?;
                        super::cursor::system_shape(arrow)
                    })
                    .and_then(|(shape, bytes)| {
                        self.renderer.shape(&self.gpu, &shape, &bytes)?;
                        self.shape = shape;
                        self.bitmap = bytes;
                        self.cursor_state.shape_uploaded();
                        Ok(())
                    });
                self.cursor_state
                    .fallback_read(result.is_ok() && actual_shape, now);
                if let Err(error) = result {
                    if !self.cursor_state.bitmap_warning {
                        tracing::warn!("system cursor fallback unavailable, continuing video with the last readable bitmap if available: {error:#}");
                        self.cursor_state.bitmap_warning = true;
                    }
                }
            }
            self.composite_pointer.Visible = self.renderer.shape.is_some().into();
            if known {
                self.composite_pointer.Position = POINT {
                    x: cursor.ptScreenPos.x - self.display.x - self.shape.HotSpot.x,
                    y: cursor.ptScreenPos.y - self.display.y - self.shape.HotSpot.y,
                };
            }
        }
        let separate = self.plane.separate(
            known && self.cursor_state.pointer_known && !self.cursor_state.fallback_needed,
            embedded,
        );
        let (_, owner) = self.cursor_state.update(
            PointerSample {
                os_visible: known.then_some(cursor.flags == CURSOR_SHOWING),
                on_output,
                dxgi_visible: self.pointer.Visible.as_bool(),
                position: [
                    self.composite_pointer.Position.x,
                    self.composite_pointer.Position.y,
                ],
            },
            separate,
        );
        self.composite_pointer.Visible =
            (owner == Owner::Host && self.renderer.shape.is_some()).into();
        self.plane.publish(visible, separate);
        // Deliver usable desktop pictures immediately while bitmap recovery
        // retries on its backoff. Missing pointer data never freezes video.
        self.cursor_state.can_render().then_some(separate)
    }

    fn render(&mut self, separate: bool) -> Result<()> {
        let mut pointer = self.composite_pointer;
        if separate {
            pointer.Visible = false.into();
        }
        let mut source = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: source owns its descriptor. Equal-format SDR pictures with
        // no pointer need only a GPU copy, preserving the existing fast path.
        unsafe {
            self.source.GetDesc(&mut source);
        }
        if !self.ten_bit
            && !pointer.Visible.as_bool()
            && source.Format == DXGI_FORMAT_B8G8R8A8_UNORM
            && matches!(
                self.rotation,
                DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
            )
        {
            // SAFETY: frame matches the unrotated SDR source dimensions/format.
            unsafe {
                self.gpu.context.CopyResource(&self.frame, &self.source);
            }
        } else {
            self.renderer.draw(
                &self.gpu,
                &self.source,
                &self.frame,
                &pointer,
                &self.shape,
                (self.rotation, self.white, self.hdr_output),
            )?;
        }
        self.cursor_state.rendered();
        self.has_frame = true;
        self.cpu_seeded = false;
        Ok(())
    }
    /// See [`Gpu::read`]; the staging texture is kept for the next call.
    pub fn read<R>(
        &mut self,
        texture: &ID3D11Texture2D,
        rows: usize,
        use_rows: impl FnOnce(&[u8], usize) -> R,
    ) -> Result<R> {
        self.gpu.read(&mut self.staging, texture, rows, use_rows)
    }
    fn same_output(&self, id: &str, name: [u16; 32]) -> bool {
        self.output_id == id && (!id.is_empty() || self.output_name == name)
    }
    fn reset_cursor(&mut self, same_output: bool) {
        let source = (self.display.width, self.display.height);
        let fit = self.display.fitted(self.stream_size.0, self.stream_size.1);
        self.plane
            .reopen(same_output, source, (fit.width, fit.height));
        if same_output {
            self.cursor_state.reopen();
        } else {
            self.cursor_state.restart();
            self.shape = Default::default();
            self.bitmap.clear();
            self.renderer.shape = None;
        }
        self.pointer = Default::default();
        self.composite_pointer = Default::default();
    }
    fn reopen(&mut self) -> Result<()> {
        let _dpi = display::DpiAwareness::physical();
        // The helper first moves to whichever desktop now has input.
        super::desktop::follow()?;
        let output = self.target.select()?;
        // Reuse the device and cursor shaders while the adapter is unchanged.
        // SAFETY: both descriptors are copied from live DXGI interfaces.
        let (old, new) = unsafe {
            (
                self.gpu
                    .device
                    .cast::<IDXGIDevice>()?
                    .GetAdapter()?
                    .GetDesc()?
                    .AdapterLuid,
                output.adapter.GetDesc1()?.AdapterLuid,
            )
        };
        if old.HighPart != new.HighPart || old.LowPart != new.LowPart {
            let mut color = latch_proto::color::ColorDescription {
                bit_depth: if self.ten_bit { 10 } else { 8 },
                ..Default::default()
            };
            if self.hdr_output {
                color.transfer = 16;
            }
            let hdr_output = self.hdr_output;
            let mut replacement = Self::open(color, self.target.clone())?;
            std::mem::swap(&mut replacement.plane, &mut self.plane);
            replacement.stream_size = self.stream_size;
            let same_output = self.same_output(&replacement.output_id, replacement.output_name);
            if same_output {
                replacement.shape = self.shape;
                replacement.bitmap = std::mem::take(&mut self.bitmap);
                replacement.cursor_state = std::mem::take(&mut self.cursor_state);
                if !replacement.bitmap.is_empty() {
                    if let Err(error) = replacement.renderer.shape(
                        &replacement.gpu,
                        &replacement.shape,
                        &replacement.bitmap,
                    ) {
                        tracing::warn!(
                            "retained cursor upload failed; continuing video: {error:#}"
                        );
                        replacement.plane.invalidate();
                        replacement.cursor_state.restart();
                    }
                }
            }
            replacement.reset_cursor(same_output);
            *self = replacement;
            self.hdr_output = hdr_output;
            return Ok(());
        }
        let duplication = duplicate(&output, &self.gpu)?;
        // SAFETY: duplication owns its descriptor.
        let desc = unsafe { duplication.GetDesc() };
        let source = self.gpu.texture(
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
            desc.ModeDesc.Format,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let display = desktop_rect(&desc, output.desc.DesktopCoordinates);
        let frame = self.gpu.texture(
            display.width,
            display.height,
            if self.ten_bit {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let same_output = self.same_output(&output.id, output.desc.DeviceName);
        self.output_id = output.id.clone();
        self.output_name = output.desc.DeviceName;
        self.white = display::sdr_white_level(&output.desc.DeviceName);
        self.source = source;
        self.frame = frame;
        self.display = display;
        self.rotation = desc.Rotation;
        self.reset_cursor(same_output);
        self.has_frame = false;
        self.cpu_seeded = false;
        self.duplication = Some(duplication);
        self.retry = ReopenBackoff::new();
        Ok(())
    }
    /// While the host cannot duplicate its own desktop: retries it on the
    /// backoff, shows the helper's frames meanwhile, and fails once neither
    /// came back in time. `None`: the own desktop is back; acquire from it.
    fn without_own_desktop(&mut self, timeout: Duration) -> Result<Option<bool>> {
        self.poll_helper();
        let now = Instant::now();
        if now >= self.retry.at {
            match self.reopen() {
                Ok(()) => {
                    if self.fallback.serving() {
                        tracing::info!("the desktop is back: capturing it directly again");
                    }
                    self.helper = Helper::Off;
                    self.fallback.regain();
                    return Ok(None);
                }
                Err(e) => {
                    tracing::debug!("desktop duplication still unavailable: {e:#}");
                    self.retry.defer(Instant::now());
                    if self
                        .fallback
                        .wants_helper(now, crate::video::DesktopUnavailable::is(&e))
                    {
                        tracing::info!("asking the SYSTEM helper for the secure desktop");
                        self.helper = Helper::Connecting(elevated::request_frames(self.want()));
                        self.fallback.connecting();
                    }
                }
            }
        }
        if matches!(self.helper, Helper::Serving(_)) {
            // Wake for the next own retry even when the helper sends nothing.
            let wait = timeout.min(self.retry.at.saturating_duration_since(Instant::now()));
            match self.helper_frame(wait) {
                Ok(changed) => {
                    // A helper that granted a mapping but sends no picture
                    // still ends the stream at its limit.
                    self.ensure_not_expired()?;
                    return Ok(Some(changed));
                }
                Err(e) if e.is::<Resized>() => {
                    // Keep the last picture while the helper maps the new size.
                    tracing::info!(
                        "the secure desktop changed size: asking the SYSTEM helper again"
                    );
                    self.helper = Helper::Connecting(elevated::request_frames(self.want()));
                    self.fallback.renegotiate(Instant::now());
                }
                Err(e) => {
                    tracing::warn!("the screen helper stopped: {e:#}");
                    self.helper = Helper::Off;
                    self.fallback.failed();
                    self.has_frame = false;
                }
            }
        }
        self.ensure_not_expired()?;
        let now = Instant::now();
        // Nothing to wait on: sleep out the caller's wait (or the backoff)
        // instead of having it poll in a loop.
        thread::sleep(timeout.min(self.retry.at.saturating_duration_since(now)));
        Ok(Some(false))
    }
    fn ensure_not_expired(&self) -> Result<()> {
        anyhow::ensure!(
            !self.fallback.expired(Instant::now()),
            "desktop duplication unavailable after access loss"
        );
        Ok(())
    }
    fn poll_helper(&mut self) {
        let Helper::Connecting(started) = &self.helper else {
            return;
        };
        match started.try_recv() {
            Ok(Ok(frames)) => {
                tracing::info!("the screen helper answered: waiting for its first frame");
                self.helper = Helper::Serving(frames);
                self.fallback.connected(Instant::now());
            }
            Ok(Err(e)) => {
                tracing::warn!("the screen helper is unavailable, so the lock screen and administrator prompts cannot be shown: {e:#}");
                self.helper = Helper::Off;
                self.fallback.failed();
                if self.duplication.is_none() {
                    self.has_frame = false;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.helper = Helper::Off;
                self.fallback.failed();
                if self.duplication.is_none() {
                    self.has_frame = false;
                }
            }
        }
    }
    /// Waits up to `wait` for a frame from the helper and renders it.
    fn helper_frame(&mut self, wait: Duration) -> Result<bool> {
        let Helper::Serving(frames) = &mut self.helper else {
            return Ok(false);
        };
        let (width, height) = frames.size();
        let mut have = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: The source owns its descriptor.
        unsafe { self.source.GetDesc(&mut have) };
        if (have.Width, have.Height, have.Format) != (width, height, DXGI_FORMAT_B8G8R8A8_UNORM) {
            self.source = self.gpu.texture(
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
                false,
            )?;
        }
        if (self.display.width, self.display.height) != (width, height) {
            self.frame = self.gpu.texture(
                width,
                height,
                if self.ten_bit {
                    DXGI_FORMAT_R10G10B10A2_UNORM
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                },
                D3D11_BIND_RENDER_TARGET.0 as u32,
                false,
            )?;
            self.display.width = width;
            self.display.height = height;
            let fit = self.display.fitted(self.stream_size.0, self.stream_size.1);
            self.plane.resize((width, height), (fit.width, fit.height));
            self.has_frame = false;
        }
        let (context, source) = (&self.gpu.context, &self.source);
        let uploaded = frames.next(wait, |pixels, stride| {
            // SAFETY: The reader hands `height` rows of `stride` bytes; the
            // runtime copies them during the call.
            unsafe {
                context.UpdateSubresource(source, 0, None, pixels.cast(), stride as u32, 0);
            }
        })?;
        if uploaded.is_none() {
            return Ok(false);
        }
        if !self.fallback.serving() {
            tracing::info!("the secure desktop is up: showing it through the SYSTEM helper");
            self.fallback.served();
        }
        self.captured = Instant::now();
        // The helper composited the cursor into an upright SDR picture.
        if !self.ten_bit && !self.hdr_output {
            // SAFETY: Both are BGRA textures of the same size.
            unsafe { self.gpu.context.CopyResource(&self.frame, &self.source) };
        } else {
            let hidden = DXGI_OUTDUPL_POINTER_POSITION::default();
            self.renderer.draw(
                &self.gpu,
                &self.source,
                &self.frame,
                &hidden,
                &self.shape,
                (DXGI_MODE_ROTATION_IDENTITY, self.white, self.hdr_output),
            )?;
        }
        self.has_frame = true;
        self.cpu_seeded = false;
        Ok(true)
    }
    /// Windows took the desktop away (access loss) and it has not been
    /// duplicated again since: a lock screen, an administrator prompt, or
    /// a mode change. Whatever fails meanwhile is not the encoder's fault.
    pub fn desktop_lost(&self) -> bool {
        self.duplication.is_none()
    }
    pub fn grab(&mut self, timeout: Duration) -> Result<bool> {
        let drawn = self
            .plane
            .control
            .drawn
            .load(std::sync::atomic::Ordering::Relaxed);
        if self.has_frame && drawn != self.last_drawn && self.duplication.is_some() {
            self.last_drawn = drawn;
            let Some(separate) = self.cursor_status() else {
                return Ok(false);
            };
            self.render(separate)?;
            return Ok(true);
        }
        self.last_drawn = drawn;
        if self.duplication.is_none() {
            if let Some(changed) = self.without_own_desktop(timeout)? {
                return Ok(changed);
            }
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: Owned duplication and valid output slots; acquisition is always
        // paired with ReleaseFrame before rendering or error propagation.
        let acquired = unsafe {
            self.duplication
                .as_ref()
                .context("missing desktop duplication")?
                .AcquireNextFrame(
                    timeout.as_millis().min(u32::MAX as u128) as u32,
                    &mut info,
                    &mut resource,
                )
        };
        match acquired {
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                let Some(separate) = self.cursor_status() else {
                    return Ok(false);
                };
                if self.cursor_state.needs_render() {
                    self.render(separate)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Release the old interface before DuplicateOutput: a process can
                // own only one duplication per output. Without a helper to take
                // over, mark the copy unavailable so repeated reopen failures
                // cannot stream a stale desktop forever; with one, the last
                // picture stands in until the helper's first frame or the limit.
                self.plane.suspend();
                self.cursor_state.reopen();
                self.duplication = None;
                self.fallback.lose(Instant::now());
                if !self.fallback.keeps_picture() {
                    self.has_frame = false;
                }
                self.cpu_seeded = false;
                self.retry = ReopenBackoff::new();
                self.retry.defer(Instant::now());
                return Ok(false);
            }
            Err(e) => return Err(e).context("acquire desktop frame"),
            Ok(()) => {}
        }
        self.captured = Instant::now();
        let mut shape_bytes = None;
        let mut desktop_changed = info.LastPresentTime != 0 || !self.cursor_state.source_ready;
        let copied = (|| -> Result<()> {
            let texture: ID3D11Texture2D = resource.context("DXGI returned no frame")?.cast()?;
            // Use the acquired texture as the final authority on physical size;
            // DesktopCoordinates can be virtualised for DPI-unaware processes.
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            let mut source = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: both descriptors are copied from live owned textures.
            unsafe {
                texture.GetDesc(&mut desc);
                self.source.GetDesc(&mut source);
            }
            if (desc.Width, desc.Height, desc.Format)
                != (source.Width, source.Height, source.Format)
            {
                self.source = self.gpu.texture(
                    desc.Width,
                    desc.Height,
                    desc.Format,
                    D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    false,
                )?;
                let (width, height) = oriented_size(desc.Width, desc.Height, self.rotation);
                self.frame = self.gpu.texture(
                    width,
                    height,
                    if self.ten_bit {
                        DXGI_FORMAT_R10G10B10A2_UNORM
                    } else {
                        DXGI_FORMAT_B8G8R8A8_UNORM
                    },
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                self.display.width = width;
                self.display.height = height;
                desktop_changed = true;
                let fit = self.display.fitted(self.stream_size.0, self.stream_size.1);
                self.plane.resize((width, height), (fit.width, fit.height));
            }
            if info.LastMouseUpdateTime != 0 {
                self.cursor_state.pointer_known = true;
                self.pointer = info.PointerPosition;
            }
            if info.PointerShapeBufferSize > 0 {
                let mut bytes = vec![0; info.PointerShapeBufferSize as usize];
                let mut required = 0;
                let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                // SAFETY: The shape buffer is sized from this acquired frame.
                let result = unsafe {
                    self.duplication
                        .as_ref()
                        .context("missing desktop duplication")?
                        .GetFramePointerShape(
                            bytes.len() as u32,
                            bytes.as_mut_ptr().cast(),
                            &mut required,
                            &mut shape,
                        )
                };
                if result.is_ok() && required as usize <= bytes.len() {
                    shape_bytes = Some((shape, bytes));
                } else {
                    tracing::warn!("DXGI pointer shape unreadable; retaining composited pointer");
                    self.plane.invalidate();
                    self.cursor_state.fallback_needed = true;
                }
            }
            if desktop_changed {
                // SAFETY: source is allocated with the acquired size and format.
                unsafe {
                    self.gpu.context.CopyResource(&self.source, &texture);
                }
                self.cursor_state.source_ready = true;
            }
            Ok(())
        })();
        // SAFETY: each successful acquisition is released, including copy errors.
        let released = unsafe {
            self.duplication
                .as_ref()
                .context("missing duplication")?
                .ReleaseFrame()
        }
        .context("release desktop frame");
        copied?;
        released?;
        anyhow::ensure!(
            !info.ProtectedContentMaskedOut.as_bool(),
            "Windows masked protected display content"
        );
        let shape_changed = shape_bytes.is_some();
        if let Some((shape, bytes)) = shape_bytes {
            if let Err(error) = self.renderer.shape(&self.gpu, &shape, &bytes) {
                tracing::warn!(
                    "DXGI pointer upload failed; retaining composited pointer: {error:#}"
                );
                self.plane.invalidate();
                self.cursor_state.fallback_needed = true;
            } else {
                self.cursor_state.dxgi_shape();
                self.shape = shape;
                let source = (self.display.width, self.display.height);
                let fit = self.display.fitted(self.stream_size.0, self.stream_size.1);
                self.plane
                    .shape(shape_info(&shape), &bytes, source, (fit.width, fit.height));
                self.bitmap = bytes;
            }
        }
        let Some(separate) = self.cursor_status() else {
            return Ok(false);
        };
        let changed = desktop_changed
            || self.cursor_state.needs_render()
            || (!separate && (info.LastMouseUpdateTime != 0 || shape_changed));
        if changed {
            self.render(separate)?;
        }
        Ok(changed)
    }
}
impl CpuCapture for Capture {
    fn size(&self) -> (u32, u32) {
        (self.display.width, self.display.height)
    }
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> Result<bool> {
        let timeout = if !self.cpu_seeded && self.has_frame {
            Duration::ZERO
        } else {
            timeout
        };
        let changed = self.grab(timeout)?;
        if !changed && (self.cpu_seeded || !self.has_frame) {
            return Ok(false);
        }
        // Fill the caller's buffer in place: a 1080p frame is 8 MB.
        let mut data = std::mem::take(&mut frame.data);
        let texture = self.frame.clone();
        let stride = self.read(&texture, self.display.height as usize, |bytes, stride| {
            data.clear();
            data.extend_from_slice(bytes);
            stride
        })?;
        self.cpu_seeded = true;
        *frame = Bgra {
            width: self.display.width,
            height: self.display.height,
            stride,
            data,
        };
        Ok(true)
    }
    fn display(&self) -> DisplayRect {
        self.display
    }
    fn captured_display(&self) -> Option<String> {
        (!self.output_id.is_empty()).then(|| self.output_id.clone())
    }
    fn name(&self) -> String {
        "DXGI Desktop Duplication".into()
    }
}

/// NV12 textures the converter rotates through. The encoder is handed one
/// per frame and, for a hardware MFT, holds it only until it has returned
/// that frame's bitstream (`Encoder::encode` waits for it), or for a
/// synchronous MFT that buffers its first inputs, a frame or two longer;
/// the converter does not write a texture again until three others have
/// been written since.
pub(super) const RING: usize = 4;

struct Slot {
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
}

/// A converted frame: the ring slot it is in and its texture.
pub(super) struct Converted {
    pub slot: usize,
    pub texture: ID3D11Texture2D,
}

pub(super) struct Converter {
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    slots: Vec<Slot>,
    next: usize,
    /// The view of the last input, made once. It holds its texture, so the
    /// texture cannot be freed and another allocated at the same address
    /// while this is cached; a new texture (after a resize) gets a new view.
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}
impl Converter {
    #[cfg(test)]
    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
        Self::with_color(gpu, input, output, fps, Default::default())
    }
    pub fn with_color(
        gpu: &Gpu,
        input: (u32, u32),
        output: (u32, u32),
        fps: u32,
        color: latch_proto::color::ColorDescription,
    ) -> Result<Self> {
        let format = if color.bit_depth == 10 {
            DXGI_FORMAT_P010
        } else {
            DXGI_FORMAT_NV12
        };
        // SAFETY: D3D video interfaces refer to the same device/context.
        unsafe {
            let video: ID3D11VideoDevice = gpu.device.cast().context("D3D11 video device")?;
            let context: ID3D11VideoContext = gpu.context.cast()?;
            let rate = DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            };
            let enumerator = video
                .CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputFrameRate: rate,
                    InputWidth: input.0,
                    InputHeight: input.1,
                    OutputFrameRate: rate,
                    OutputWidth: output.0,
                    OutputHeight: output.1,
                    Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                })
                .context("enumerate video processor")?;
            anyhow::ensure!(
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?
                    & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
                    != 0,
                "video processor cannot read BGRA"
            );
            anyhow::ensure!(
                enumerator.CheckVideoProcessorFormat(format)?
                    & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32
                    != 0,
                "video processor cannot produce NV12"
            );
            let processor = video
                .CreateVideoProcessor(&enumerator, 0)
                .context("create BGRA to NV12 processor")?;
            // Bitfield: YCbCr_Matrix=1 (709), nominal range=2 (16..235).
            context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (2 << 4),
                },
            );
            // RGB input is full range; the matrix bit selects 709 conversion.
            context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                },
            );
            if color.is_hdr() {
                context
                    .cast::<ID3D11VideoContext1>()
                    .context("HDR colour conversion requires D3D11.1")?;
            }
            if let Ok(context1) = context.cast::<ID3D11VideoContext1>() {
                context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    if color.is_hdr() {
                        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                    } else {
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
                    },
                );
                context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    if color.is_hdr() {
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020
                    } else {
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709
                    },
                );
            }
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let rect = RECT {
                left: 0,
                top: 0,
                right: output.0 as i32,
                bottom: output.1 as i32,
            };
            let fitted = DisplayRect {
                width: input.0,
                height: input.1,
                ..Default::default()
            }
            .fitted(output.0, output.1);
            let content = RECT {
                left: fitted.x,
                top: fitted.y,
                right: fitted.x + fitted.width as i32,
                bottom: fitted.y + fitted.height as i32,
            };
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&content));
            context.VideoProcessorSetOutputBackgroundColor(
                &processor,
                true,
                &D3D11_VIDEO_COLOR {
                    Anonymous: D3D11_VIDEO_COLOR_0 {
                        YCbCr: D3D11_VIDEO_COLOR_YCbCrA {
                            Y: 0.0,
                            Cb: 0.5,
                            Cr: 0.5,
                            A: 1.0,
                        },
                    },
                },
            );
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect));
            let mut slots = Vec::with_capacity(RING);
            for _ in 0..RING {
                let texture = gpu.texture(
                    output.0,
                    output.1,
                    format,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                let mut view = None;
                video
                    .CreateVideoProcessorOutputView(
                        &texture,
                        &enumerator,
                        &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                            },
                        },
                        Some(&mut view),
                    )
                    .context("video processor output view")?;
                slots.push(Slot {
                    texture,
                    view: view.context("missing processor output view")?,
                });
            }
            Ok(Self {
                processor,
                enumerator,
                video,
                context,
                slots,
                next: 0,
                input: None,
            })
        }
    }
    pub fn convert(&mut self, input: &ID3D11Texture2D) -> Result<Converted> {
        // SAFETY: Views and textures belong to the same video device. ManuallyDrop
        // fields in the Win32 ABI structs are explicitly released after the blit.
        unsafe {
            if self
                .input
                .as_ref()
                .is_none_or(|(texture, _)| texture.as_raw() != input.as_raw())
            {
                let mut view = None;
                self.video
                    .CreateVideoProcessorInputView(
                        input,
                        &self.enumerator,
                        &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                            FourCC: 0,
                            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                                Texture2D: D3D11_TEX2D_VPIV {
                                    MipSlice: 0,
                                    ArraySlice: 0,
                                },
                            },
                        },
                        Some(&mut view),
                    )
                    .context("video processor input view")?;
                self.input = Some((input.clone(), view.context("missing processor input view")?));
            }
            let slot = self.next;
            self.next = (self.next + 1) % RING;
            let view = self.input.as_ref().map(|(_, view)| view.clone());
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(view),
                ..Default::default()
            };
            let result = self
                .context
                .VideoProcessorBlt(
                    &self.processor,
                    &self.slots[slot].view,
                    0,
                    std::slice::from_ref(&stream),
                )
                .context("convert BGRA to NV12");
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            Ok(Converted {
                slot,
                texture: self.slots[slot].texture.clone(),
            })
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn physical_texture_size_and_reopen_backoff() {
        use super::*;
        assert_eq!(
            oriented_size(1920, 1080, DXGI_MODE_ROTATION_IDENTITY),
            (1920, 1080)
        );
        assert_eq!(
            oriented_size(1920, 1080, DXGI_MODE_ROTATION_ROTATE90),
            (1080, 1920)
        );
        assert_eq!(
            oriented_size(1920, 1080, DXGI_MODE_ROTATION_ROTATE270),
            (1080, 1920)
        );
        let desc = DXGI_OUTDUPL_DESC {
            ModeDesc: DXGI_MODE_DESC {
                Width: 1920,
                Height: 1080,
                ..Default::default()
            },
            Rotation: DXGI_MODE_ROTATION_IDENTITY,
            ..Default::default()
        };
        // A DPI-unaware output rectangle must never shrink the physical frame.
        let display = desktop_rect(
            &desc,
            RECT {
                left: -1280,
                top: 20,
                right: 0,
                bottom: 740,
            },
        );
        assert_eq!(
            display,
            DisplayRect {
                x: -1280,
                y: 20,
                width: 1920,
                height: 1080
            }
        );
        let mut retry = ReopenBackoff::new();
        let now = Instant::now();
        for millis in [50, 100, 200, 400, 800, 1000, 1000] {
            retry.defer(now);
            assert_eq!(retry.at.duration_since(now), Duration::from_millis(millis));
        }
    }
    #[test]
    fn without_a_helper_a_lost_desktop_ends_the_stream_after_five_seconds() {
        use super::*;
        let t = Instant::now();
        let mut f = Fallback::new(false);
        assert!(!f.expired(t + Duration::from_secs(60)), "nothing lost yet");
        f.lose(t);
        f.lose(t + Duration::from_secs(3)); // a second loss keeps the first time
        assert!(!f.keeps_picture(), "no stale desktop without a helper");
        assert!(!f.wants_helper(t + Duration::from_secs(4), true));
        assert!(!f.expired(t + Duration::from_millis(4999)));
        assert!(f.expired(t + OWN_LIMIT));
        f.regain();
        assert!(!f.expired(t + Duration::from_secs(60)));
    }
    #[test]
    fn the_helper_takes_over_a_denied_desktop_until_the_own_one_returns() {
        use super::*;
        let t = Instant::now();
        let mut f = Fallback::new(true);
        f.lose(t);
        assert!(f.keeps_picture());
        // A denial (the secure desktop) asks at once; another loss waits.
        assert!(f.wants_helper(t, true));
        assert!(!f.wants_helper(t + Duration::from_millis(500), false));
        assert!(f.wants_helper(t + HELPER_AFTER, false));
        f.connecting();
        assert!(
            !f.wants_helper(t + Duration::from_secs(2), true),
            "asked once"
        );
        // Starting the helper gets longer than a plain loss, not forever.
        assert!(!f.expired(t + OWN_LIMIT));
        assert!(f.expired(t + HELPER_LIMIT));
        let granted = t + Duration::from_secs(2);
        f.connected(granted);
        assert!(!f.wants_helper(granted, true));
        // Connected but no frame yet: the first frame's limit, so a helper
        // that never sends one cannot freeze the old picture.
        assert!(!f.expired(granted + FIRST_FRAME - Duration::from_millis(1)));
        assert!(f.expired(granted + FIRST_FRAME));
        f.served();
        // Serving frames, the stream lasts as long as the secure desktop.
        assert!(!f.expired(t + Duration::from_secs(3600)));
        f.regain();
        assert!(!f.serving() && f.lost.is_none() && f.phase == Phase::Idle);
        // The next prompt asks again.
        f.lose(t + Duration::from_secs(10));
        assert!(f.wants_helper(t + Duration::from_secs(10), true));
    }
    #[test]
    fn a_helper_that_fails_falls_back_to_the_old_limit_for_that_loss() {
        use super::*;
        let t = Instant::now();
        let mut f = Fallback::new(true);
        f.lose(t);
        f.connecting();
        f.failed();
        assert!(!f.keeps_picture());
        assert!(
            f.expired(t + OWN_LIMIT),
            "an older helper ends it as before"
        );
        assert!(
            !f.wants_helper(t + Duration::from_secs(6), true),
            "once per loss"
        );
        // The next prompt asks again: a helper that died may be back.
        f.regain();
        f.lose(t + Duration::from_secs(20));
        assert!(f.keeps_picture());
        assert!(f.wants_helper(t + Duration::from_secs(20), true));
        // Serving, then failing mid-way, also ends it once the limit passed.
        let mut f = Fallback::new(true);
        f.lose(t);
        f.connecting();
        f.connected(t);
        f.served();
        assert!(!f.expired(t + Duration::from_secs(30)));
        f.failed();
        assert!(f.expired(t + Duration::from_secs(30)));
    }
    #[test]
    fn a_resize_asks_the_helper_again_with_a_fresh_limit() {
        use super::*;
        let t = Instant::now();
        let mut f = Fallback::new(true);
        f.lose(t);
        f.connecting();
        f.connected(t);
        f.served();
        // Long into the secure desktop, its size changes: the helper ends
        // that mapping, and is asked for one of the new size.
        let resized = t + Duration::from_secs(600);
        f.renegotiate(resized);
        assert!(!f.unavailable() && f.keeps_picture(), "not a failure");
        assert!(!f.wants_helper(resized, true), "already asked again");
        assert!(!f.expired(resized + OWN_LIMIT));
        assert!(f.expired(resized + HELPER_LIMIT), "connecting is bounded");
        // The new mapping gets the same time to show its first frame...
        let granted = resized + Duration::from_secs(3);
        f.connected(granted);
        assert!(!f.expired(granted + FIRST_FRAME - Duration::from_millis(1)));
        assert!(f.expired(granted + FIRST_FRAME));
        // ...and once it does, serves on.
        f.served();
        assert!(!f.expired(resized + Duration::from_secs(3600)));
    }
    #[test]
    fn a_granted_mapping_that_sends_nothing_ends_the_stream() {
        use super::*;
        let t = Instant::now();
        let mut f = Fallback::new(true);
        f.lose(t);
        f.connecting();
        let granted = t + Duration::from_secs(1);
        f.connected(granted);
        // Well before the helper's start limit: the first frame's limit
        // counts from the grant, and nothing later lifts it but a frame.
        assert!(!f.expired(granted + FIRST_FRAME - Duration::from_millis(1)));
        assert!(f.expired(granted + FIRST_FRAME));
        assert!(f.expired(t + HELPER_LIMIT));
        assert!(f.expired(t + Duration::from_secs(3600)), "never freezes");
        assert!(!f.wants_helper(t + Duration::from_secs(3600), true));
    }
    #[test]
    fn a_slow_cold_start_still_gets_its_first_frame_time() {
        use super::*;
        // Opening on the secure desktop: the task, the agent and the grant
        // took longer than a frame's limit, and the grant still counts.
        let t = Instant::now();
        let mut f = Fallback::new(true);
        f.lose(t);
        let granted = t + Duration::from_secs(9);
        f.connected(granted);
        assert!(!f.expired(granted));
        assert!(!f.expired(granted + FIRST_FRAME - Duration::from_millis(1)));
        assert!(f.expired(granted + FIRST_FRAME));
        // So does a grant late in an active stream's start limit.
        let mut f = Fallback::new(true);
        f.lose(t);
        f.connecting();
        let granted = t + HELPER_LIMIT - Duration::from_millis(100);
        f.connected(granted);
        assert!(!f.expired(t + HELPER_LIMIT));
        assert!(f.expired(granted + FIRST_FRAME));
        f.served();
        assert!(!f.expired(granted + FIRST_FRAME));
    }
    /// Needs the helper set up (a build with LATCH_HELPER_TEST_DIR, its
    /// folder and task): pretends Windows took the desktop, takes the
    /// helper's frames into the stream's texture, then switches back.
    #[test]
    #[ignore]
    fn live_switches_to_the_helpers_frames_and_back() -> anyhow::Result<()> {
        use super::*;
        let mut capture = Capture::new()?;
        let (width, height) = (capture.display.width, capture.display.height);
        capture.duplication = None;
        capture.plane.suspend();
        capture.fallback.lose(Instant::now());
        capture.retry.at = Instant::now() + Duration::from_secs(60);
        capture.helper = Helper::Connecting(elevated::request_frames(capture.want()));
        capture.fallback.connecting();
        let started = Instant::now();
        let mut frames = 0;
        let mut first = None;
        while started.elapsed() < Duration::from_secs(20) && frames < 3 {
            if capture.grab(Duration::from_millis(100))? {
                frames += 1;
                first.get_or_insert(started.elapsed());
                anyhow::ensure!(matches!(capture.helper, Helper::Serving(_)));
            }
            if first.is_some_and(|t| started.elapsed() > t + Duration::from_secs(3)) {
                break;
            }
        }
        anyhow::ensure!(frames > 0, "no frame from the helper");
        anyhow::ensure!((capture.display.width, capture.display.height) == (width, height));
        let frame = capture.frame.clone();
        let lit = capture.read(&frame, height as usize, |rows, _| {
            rows.iter().filter(|&&b| b != 0).count()
        })?;
        eprintln!(
            "helper: first frame after {:?}, {frames} frames, {lit} non-zero bytes",
            first.unwrap()
        );
        anyhow::ensure!(lit > 0, "the helper's frame is black");
        capture.retry.at = Instant::now();
        capture.grab(Duration::from_millis(100))?;
        anyhow::ensure!(capture.duplication.is_some(), "own desktop not reopened");
        anyhow::ensure!(matches!(capture.helper, Helper::Off) && capture.fallback.lost.is_none());
        Ok(())
    }
    #[test]
    fn cursor_shaders_compile_without_a_gpu() -> anyhow::Result<()> {
        super::compile_shader(windows::core::s!("vs"), windows::core::s!("vs_4_0"))?;
        super::compile_shader(windows::core::s!("ps"), windows::core::s!("ps_4_0"))?;
        Ok(())
    }
    /// IEEE half from f32 for the normal, finite values a test uses.
    fn half(v: f32) -> u16 {
        if v == 0.0 {
            return 0;
        }
        let bits = v.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let mantissa = ((bits & 0x7f_ffff) + 0x1000) >> 13;
        sign | (((exponent as u16) << 10) + mantissa as u16)
    }
    /// An HDR (Advanced Color) desktop captured for an SDR stream: neutral
    /// 203/400/600/1000-nit patches stay apart, saturated and wide-gamut
    /// colours keep their hue, SDR content below the knee is unchanged and
    /// nothing goes black. Matches latch_proto's reference within a code.
    #[test]
    fn fp16_sc_rgb_hdr_desktop_keeps_highlights_in_sdr() -> anyhow::Result<()> {
        use super::*;
        use latch_proto::color::{bt2020_to_bt709, hdr_to_sdr, srgb_oetf};
        // This tests shader output, not capture or video acceleration. WARP
        // runs the shader even on CI adapters without D3D11 video support.
        let (mut device, mut context) = (None, None);
        // SAFETY: Valid output pointers; WARP requires no explicit adapter.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let gpu = Gpu {
            device: device.context("WARP returned no device")?,
            context: context.context("WARP returned no context")?,
            adapter: (0, 0),
        };
        let white_nits = 203.0f64;
        let nits = |n: f64| n / 80.0;
        let mut patches: Vec<[f64; 3]> = [203.0, 400.0, 600.0, 1000.0, 100.0, 0.0]
            .iter()
            .map(|&n| [nits(n); 3])
            .collect();
        patches.extend([
            [nits(1000.0), 0.0, 0.0],
            [nits(1000.0), nits(500.0), 0.0],
            [nits(40.0), nits(80.0), nits(600.0)],
            bt2020_to_bt709([0.0, nits(1000.0), 0.0]),
        ]);
        let width = patches.len() as u32;
        let source = gpu.texture(
            width,
            1,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let target = gpu.texture(
            width,
            1,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let pixels: Vec<u16> = patches
            .iter()
            .flat_map(|p| {
                [
                    half(p[0] as f32),
                    half(p[1] as f32),
                    half(p[2] as f32),
                    0x3c00,
                ]
            })
            .collect();
        // Half floats round: compare against what the shader actually reads.
        let read: Vec<[f64; 3]> = pixels
            .chunks(4)
            .map(|p| {
                [0, 1, 2].map(|i| {
                    let h = p[i];
                    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
                    let e = i32::from((h >> 10) & 31);
                    let m = f64::from(h & 1023) / 1024.0;
                    if e == 0 {
                        sign * m * 2f64.powi(-14)
                    } else {
                        sign * (1.0 + m) * 2f64.powi(e - 15)
                    }
                })
            })
            .collect();
        // SAFETY: Upload covers the complete one-row texture.
        unsafe {
            gpu.context
                .UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), width * 8, 0);
        }
        let white = (white_nits / 80.0) as f32;
        CursorRenderer::new(&gpu)?.draw(
            &gpu,
            &source,
            &target,
            &Default::default(),
            &Default::default(),
            (DXGI_MODE_ROTATION_IDENTITY, white, false),
        )?;
        let out = gpu.read(&mut None, &target, 1, |bytes, _| bytes.to_vec())?;
        let bgr: Vec<[i32; 3]> = out[..width as usize * 4]
            .chunks(4)
            .map(|p| [i32::from(p[2]), i32::from(p[1]), i32::from(p[0])])
            .collect();
        for (i, rgb) in read.iter().enumerate() {
            let expected = hdr_to_sdr(rgb.map(|v| v / f64::from(white)), 1000.0 / white_nits)
                .map(|v| (255.0 * srgb_oetf(v)).round() as i32);
            assert!(
                (0..3).all(|c| (bgr[i][c] - expected[c]).abs() <= 1),
                "patch {i} {rgb:?}: shader {:?}, reference {expected:?}",
                bgr[i]
            );
        }
        eprintln!("HDR desktop → SDR codes: {bgr:?}");
        let neutral: Vec<i32> = bgr[..4].iter().map(|p| p[0]).collect();
        assert!(neutral.windows(2).all(|w| w[0] < w[1]), "{neutral:?}");
        assert_eq!(bgr[5], [0, 0, 0], "black stays black");
        assert!(
            bgr[4][0] > 150,
            "100-nit grey is not darkened: {:?}",
            bgr[4]
        );
        assert!(
            bgr[6..].iter().all(|p| p.iter().any(|&c| c > 128)),
            "{bgr:?}"
        );
        Ok(())
    }
    #[test]
    fn desktop_duplication_frame_when_interactive() -> anyhow::Result<()> {
        let _runtime = match super::super::runtime::Runtime::new(false) {
            Ok(r) => r,
            Err(e) => {
                if super::super::unavailable(&e, "COM") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let mut capture = match super::Capture::new() {
            Ok(c) => c,
            Err(e) => {
                if super::super::unavailable(&e, "Desktop Duplication") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        match capture.grab(std::time::Duration::from_secs(1)) {
            Ok(true) => {
                assert!(capture.display.width > 0);
            }
            Ok(false) => eprintln!("SKIP desktop produced no frame"),
            Err(e) => {
                if !super::super::unavailable(&e, "interactive desktop") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod hdr_desktop_tests {
    use super::*;
    /// Live Advanced Color desktop → SDR stream capture: the HDR10 viewer
    /// test's 203/400/600/1000-nit patches (latch-client
    /// `hdr10_child_window_keeps_hdr_under_overlays` with
    /// LATCH_HDR_PATCH_DIR) come out of the SDR capture path brighter in
    /// order, never black and never all clipped to one white.
    #[test]
    #[ignore = "interactive HDR desktop with the client's HDR10 patch window up"]
    fn hdr_desktop_patches_reach_sdr_capture_apart() -> Result<()> {
        let dir = std::path::PathBuf::from(std::env::var("LATCH_HDR_PATCH_DIR")?);
        let start = Instant::now();
        while !dir.join("patches-ready").exists() {
            anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "no patch window");
            std::thread::sleep(Duration::from_millis(100));
        }
        let points: Vec<(i32, i32)> = std::fs::read_to_string(dir.join("patches-ready"))?
            .split(';')
            .map(|p| {
                let (x, y) = p.split_once(',').unwrap();
                (x.parse().unwrap(), y.parse().unwrap())
            })
            .collect();
        let result = (|| -> Result<Vec<[u8; 3]>> {
            let _runtime = super::super::Runtime::new(false)?;
            let mut capture = Capture::new()?;
            anyhow::ensure!(!capture.hdr_output, "an SDR stream must get SDR capture");
            let start = Instant::now();
            while !capture.grab(Duration::from_millis(200))? || !capture.has_frame {
                anyhow::ensure!(start.elapsed() < Duration::from_secs(5), "no desktop frame");
            }
            let display = capture.display;
            let frame = capture.frame.clone();
            capture.read(&frame, display.height as usize, |bytes, pitch| {
                points
                    .iter()
                    .map(|&(x, y)| {
                        let at = (y - display.y) as usize * pitch + (x - display.x) as usize * 4;
                        [bytes[at + 2], bytes[at + 1], bytes[at]]
                    })
                    .collect()
            })
        })();
        std::fs::write(dir.join("patches-done"), "done")?;
        let codes = result?;
        eprintln!(
            "SDR white {} nits; 203/400/600/1000-nit patches → {codes:?}",
            display::sdr_white_level(&display::select()?.desc.DeviceName) * 80.0
        );
        assert!(codes.iter().all(|c| c[0] > 0), "black capture: {codes:?}");
        assert!(codes.windows(2).all(|w| w[0][0] <= w[1][0]), "{codes:?}");
        assert!(
            codes[0][0] < codes[3][0],
            "highlights clipped together: {codes:?}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    /// Capture updates without conversion, encoding, or a pacer. Run with
    /// desktop animation to distinguish DXGI/DWM cadence from encode limits.
    #[test]
    #[ignore]
    fn hardware_capture_rate() -> Result<()> {
        let _runtime = super::super::Runtime::new(false)?;
        for (width, height, fps) in [(1920, 1080, 240), (2560, 1440, 240), (3840, 2160, 120)] {
            let previous = display::prepare(width, height, fps, None)?;
            let result = (|| -> Result<()> {
                let mut capture = Capture::new()?;
                let start = Instant::now();
                let mut frames = 0;
                while start.elapsed() < Duration::from_secs(3) {
                    frames += usize::from(capture.grab(Duration::from_millis(100))?);
                }
                eprintln!(
                    "DXGI capture only {width}x{height}@{fps}: {:.1} updates/s",
                    frames as f64 / start.elapsed().as_secs_f64()
                );
                Ok(())
            })();
            if let Some(previous) = previous {
                display::restore(&previous)?;
            }
            result?;
        }
        Ok(())
    }
    #[test]
    #[ignore]
    fn hardware_conversion_timing() -> Result<()> {
        let _runtime = super::super::Runtime::new(false)?;
        let output = display::select()?;
        let gpu = Gpu::new(&output.adapter)?;
        // SAFETY: queries and textures belong to this device; GetData sizes
        // match SDK types. Disjoint timestamps are discarded, waits bounded.
        unsafe {
            for (width, height) in [(1920, 1080), (2560, 1440), (3840, 2160)] {
                let input = gpu.texture(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                let mut converter = Converter::new(&gpu, (width, height), (width, height), 240)?;
                let query = |kind| -> Result<ID3D11Query> {
                    let mut query = None;
                    gpu.device.CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: kind,
                            MiscFlags: 0,
                        },
                        Some(&mut query),
                    )?;
                    query.context("missing timing query")
                };
                let disjoint = query(D3D11_QUERY_TIMESTAMP_DISJOINT)?;
                let before = query(D3D11_QUERY_TIMESTAMP)?;
                let after = query(D3D11_QUERY_TIMESTAMP)?;
                let mut times = Vec::new();
                for i in 0..40 {
                    gpu.context.Begin(&disjoint);
                    gpu.context.End(&before);
                    converter.convert(&input)?;
                    gpu.context.End(&after);
                    gpu.context.End(&disjoint);
                    gpu.context.Flush();
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let read = |query: &ID3D11Query,
                                data: *mut std::ffi::c_void,
                                size: u32|
                     -> Result<()> {
                        loop {
                            // Preserve S_FALSE: the generated Result<()> wrapper
                            // considers it success while query data is not ready.
                            let status = (gpu.context.vtable().GetData)(
                                gpu.context.as_raw(),
                                query.as_raw(),
                                data,
                                size,
                                0,
                            );
                            if status == S_OK {
                                return Ok(());
                            }
                            status.ok()?;
                            anyhow::ensure!(Instant::now() < deadline, "GPU timestamps timed out");
                            thread::yield_now();
                        }
                    };
                    let mut timing = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
                    read(
                        &disjoint,
                        (&mut timing as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT).cast(),
                        std::mem::size_of_val(&timing) as u32,
                    )?;
                    let (mut start, mut end) = (0u64, 0u64);
                    read(&before, (&mut start as *mut u64).cast(), 8)?;
                    read(&after, (&mut end as *mut u64).cast(), 8)?;
                    if i >= 10 && !timing.Disjoint.as_bool() {
                        times.push((end - start) as f64 * 1000.0 / timing.Frequency as f64);
                    }
                }
                anyhow::ensure!(!times.is_empty(), "all timestamps disjoint");
                times.sort_by(f64::total_cmp);
                eprintln!(
                    "GPU BGRA→NV12 {width}x{height}: p50={:.3}ms p95={:.3}ms",
                    times[times.len() / 2],
                    times[times.len() * 95 / 100]
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod hdr_probe {
    use super::*;
    use crate::video::{Codec, VideoConfig};

    #[test]
    #[ignore = "requires a hardware HEVC Main10 encoder; writes HDR probe artifacts"]
    fn scrgb_pq_p010_hardware_roundtrip() -> Result<()> {
        let _runtime = super::super::runtime::Runtime::new(true)?;
        let output = display::select()?;
        let gpu = Gpu::new(&output.adapter)?;
        let color = latch_proto::color::ColorDescription::negotiate(
            latch_proto::color::ColorCapabilities {
                hdr: true,
                ten_bit: true,
                peak_nits: 1000,
            },
            true,
            true,
        );
        let config = VideoConfig {
            color,
            codec: Codec::Hevc,
            width: 960,
            height: 540,
            fps: 60,
            bitrate_kbps: 20000,
            display: None,
        };
        let source = gpu.texture(
            960,
            540,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let target = gpu.texture(
            960,
            540,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let pixels: Vec<u16> = (0..960 * 540)
            .flat_map(|i| {
                let v = [0x3000, 0x3d00, 0x4a40][(i % 960) / 320]; // 10, 100, 1000 cd/m² in scRGB.
                [v, v, v, 0x3c00]
            })
            .collect();
        // SAFETY: tightly packed complete FP16 image; the system shader reads it.
        unsafe {
            gpu.context
                .UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), 960 * 8, 0);
        }
        let mut renderer = CursorRenderer::new(&gpu)?;
        renderer.draw(
            &gpu,
            &source,
            &target,
            &Default::default(),
            &Default::default(),
            (DXGI_MODE_ROTATION_IDENTITY, 1.0, true),
        )?;
        let mut converter = Converter::with_color(&gpu, (960, 540), (960, 540), 60, color)?;
        let picture = converter.convert(&target)?;
        gpu.read(&mut None, &picture.texture, 540 * 3 / 2, |bytes, stride| {
            for (x, nits) in [(160, 10.0), (480, 100.0), (800, 1000.0)] {
                let offset = 270 * stride + x * 2;
                let code = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) >> 6;
                let expected = 64.0 + 876.0 * latch_proto::color::pq_oetf(nits);
                assert!(
                    (f64::from(code) - expected).abs() < 4.0,
                    "FP16 -> PQ P010: {nits} nits -> {code}, expected {expected}"
                );
            }
        })?;
        let dir = std::env::var("LATCH_HDR_PROBE_DIR").unwrap_or_else(|_| ".".into());
        for mode in ["sdr8", "sdr10", "hdr"] {
            let color = latch_proto::color::ColorDescription::negotiate(
                latch_proto::color::ColorCapabilities {
                    ten_bit: mode != "sdr8",
                    hdr: mode == "hdr",
                    peak_nits: 1000,
                },
                true,
                mode == "hdr",
            );
            let config = VideoConfig {
                color,
                ..config.clone()
            };
            let rgb = gpu.texture(
                960,
                540,
                if color.bit_depth == 10 {
                    DXGI_FORMAT_R10G10B10A2_UNORM
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                },
                D3D11_BIND_RENDER_TARGET.0 as u32,
                false,
            )?;
            renderer.draw(
                &gpu,
                &source,
                &rgb,
                &Default::default(),
                &Default::default(),
                (DXGI_MODE_ROTATION_IDENTITY, 1.0, color.is_hdr()),
            )?;
            let mut converter = Converter::with_color(&gpu, (960, 540), (960, 540), 60, color)?;
            let picture = converter.convert(&rgb)?;
            for mf in [false, true] {
                let name = if mf { "mf" } else { "amf" };
                let mut stream = color
                    .hdr
                    .map(latch_proto::color::hevc_hdr_sei)
                    .unwrap_or_default();
                let mut times = Vec::new();
                if mf {
                    let mut encoder = super::super::encode::Encoder::new(&config, &gpu)?;
                    let sample = super::super::encode::Encoder::texture_sample(&picture.texture)?;
                    for i in 0..60 {
                        let at = Instant::now();
                        if let Some((bytes, _)) = encoder.encode(&sample, i == 0)? {
                            stream.extend(bytes);
                        }
                        times.push(at.elapsed().as_secs_f64() * 1000.0);
                    }
                } else {
                    let mut encoder = super::super::amf::Encoder::new(&config, &gpu)?;
                    for i in 0..60 {
                        let at = Instant::now();
                        let (bytes, _) = encoder.encode(&picture.texture, i == 0)?;
                        times.push(at.elapsed().as_secs_f64() * 1000.0);
                        stream.extend(bytes);
                    }
                }
                times.sort_by(f64::total_cmp);
                eprintln!(
                "COLOR {mode} {name}: frames=60 bytes={} encode_p50_ms={:.3} encode_p95_ms={:.3}",
                stream.len(),
                times[30],
                times[57]
            );
                std::fs::write(
                    std::path::Path::new(&dir).join(format!("{mode}-{name}.hevc")),
                    stream,
                )?;
            }
        }
        Ok(())
    }
}
