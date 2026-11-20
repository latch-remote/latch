use crate::{display::Previous, video::DesktopUnavailable};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Devices::Display::*,
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, E_ACCESSDENIED, HWND, WIN32_ERROR,
        },
        Graphics::{Dxgi::*, Gdi::*},
        System::StationsAndDesktops::{
            CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
        },
    },
};

// Scope awareness to engine operations: the background service may never
// create a winit window, and DXGI/input metrics must still use physical pixels.
#[link(name = "user32")]
extern "system" {
    fn SetThreadDpiAwarenessContext(context: isize) -> isize;
}
pub(super) struct DpiAwareness(isize);
impl DpiAwareness {
    pub fn physical() -> Self {
        // SAFETY: -4 is DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2. The previous
        // thread context is restored when this engine operation finishes.
        Self(unsafe { SetThreadDpiAwarenessContext(-4) })
    }
}
impl Drop for DpiAwareness {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: this is the context returned for the same thread above.
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

// Keep paths by adapter/target identity, since Windows can renumber DISPLAYn.
struct Saved {
    previous: Previous,
    output_id: String,
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    adapters: BTreeMap<String, (i32, u32)>,
    default_adapter: (i32, u32),
}

struct DisplayState {
    generation: u64,
    saved: Option<Saved>,
    pending_restore: Option<u64>,
    deferred_prepare: Option<ModeRequest>,
}

struct ModeRequest {
    width: u32,
    height: u32,
    fps: u32,
    display: Option<String>,
}

// The same lock orders generation changes and all Windows display writes.
static STATE: Mutex<DisplayState> = Mutex::new(DisplayState {
    generation: 0,
    saved: None,
    pending_restore: None,
    deferred_prepare: None,
});

impl DisplayState {
    fn begin_prepare(&mut self) -> u64 {
        self.generation += 1;
        self.deferred_prepare = None;
        self.generation
    }

    fn recover_idle(&mut self, restore: impl FnOnce(&Saved) -> Result<()>) -> Option<Previous> {
        // Give the new preparation a chance to recover the idle topology,
        // including any detached physical displays, before selecting its output.
        self.pending_restore?;
        match self.attempt(self.generation, restore) {
            Some(Err(error)) if DesktopUnavailable::is(&error) => {
                let mut previous = self.saved.as_ref()?.previous.clone();
                previous.generation = self.generation;
                Some(previous)
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "could not recover the idle display topology before preparing");
                None
            }
            _ => None,
        }
    }

    fn prepare_session(
        &mut self,
        restore: impl FnOnce(&Saved) -> Result<()>,
        prepare: impl FnOnce(&mut Self) -> Result<Option<Preparation>>,
    ) -> Result<Option<Preparation>> {
        if let Some(previous) = self.recover_idle(restore) {
            return Ok(Some(Preparation {
                previous,
                deferred: true,
            }));
        }
        let prepared = prepare(self)?;
        if prepared.is_none() {
            self.saved = None;
        }
        Ok(prepared)
    }

    fn retry_prepare(
        &mut self,
        generation: u64,
        stopping: &AtomicBool,
        apply: impl FnOnce(&mut Self, &ModeRequest) -> Result<bool>,
    ) -> Option<Result<bool>> {
        if generation != self.generation || stopping.load(Ordering::Acquire) {
            return None;
        }
        let request = self.deferred_prepare.take()?;
        let applied = apply(self, &request);
        if matches!(applied, Ok(false)) {
            self.deferred_prepare = Some(request);
        }
        Some(applied)
    }

    fn attempt(
        &mut self,
        generation: u64,
        restore: impl FnOnce(&Saved) -> Result<()>,
    ) -> Option<Result<()>> {
        if generation != self.generation {
            return None;
        }
        let result = restore(self.saved.as_ref()?);
        if !result.as_ref().is_err_and(DesktopUnavailable::is) {
            self.saved = None;
            self.pending_restore = None;
        }
        Some(result)
    }

    fn restore(&mut self, generation: u64) -> Result<()> {
        if generation != self.generation {
            return Ok(());
        }
        self.deferred_prepare = None;
        if self.pending_restore == Some(generation) {
            return Ok(());
        }
        match self.attempt(generation, Saved::restore) {
            Some(Err(error)) if DesktopUnavailable::is(&error) => {
                tracing::warn!("desktop is inaccessible; deferring display restore: {error:#}");
                self.pending_restore = Some(generation);
                thread::Builder::new()
                    .name("display-restore".into())
                    .spawn(move || retry_restore(generation))
                    .context("start desktop restore worker")?;
                Ok(())
            }
            result => result.unwrap_or(Ok(())),
        }
    }
}

fn retry_restore(generation: u64) {
    for _ in 0..60 {
        thread::sleep(Duration::from_secs(5));
        match STATE.lock().attempt(generation, Saved::restore) {
            None => return,
            Some(Ok(())) => {
                tracing::info!("restored the desktop after it became accessible");
                return;
            }
            Some(Err(error)) if DesktopUnavailable::is(&error) => {}
            Some(Err(error)) => {
                tracing::warn!("could not restore the desktop: {error:#}");
                return;
            }
        }
    }
    tracing::debug!("desktop restore retry window ended");
}

impl Previous {
    pub(crate) fn start_deferred(&self, stopping: Arc<AtomicBool>) {
        let generation = self.generation;
        let state = STATE.lock();
        if state.generation != generation || state.deferred_prepare.is_none() {
            return;
        }
        drop(state);
        if let Err(error) = thread::Builder::new()
            .name("display-prepare".into())
            .spawn(move || retry_prepare(generation, &stopping))
        {
            tracing::warn!(%error, "could not start the deferred display preparation worker");
        }
    }

    pub(crate) fn begin_custom_prepare() {
        let mut state = STATE.lock();
        state.begin_prepare();
        state.recover_idle(Saved::restore);
        // The custom command and its own undo now own the display.
        state.saved = None;
        state.pending_restore = None;
    }
}

fn topology() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    for _ in 0..4 {
        let (mut np, mut nm) = (0, 0);
        // SAFETY: Sized output arrays remain live through the query.
        unsafe {
            query_result(
                "GetDisplayConfigBufferSizes",
                GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm),
            )?;
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            let rc = QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            );
            if rc == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            query_result("QueryDisplayConfig", rc)?;
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            return Ok((paths, modes));
        }
    }
    bail!("display topology kept changing")
}
/// Windows refuses to read the topology to a process without access to the
/// input desktop (a lock, an administrator prompt): the same as a denied
/// change, which defers.
fn query_result(call: &'static str, rc: WIN32_ERROR) -> Result<()> {
    if rc == ERROR_ACCESS_DENIED {
        return Err(anyhow::anyhow!("{call} returned {}", rc.0).context(DesktopUnavailable));
    }
    rc.ok().context(call)
}
fn apply_topology(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<()> {
    // SAFETY: Paths and their mode indices came from QueryDisplayConfig.
    let rc = unsafe {
        SetDisplayConfig(
            Some(paths),
            Some(modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    };
    topology_result(rc)
}

fn topology_result(rc: i32) -> Result<()> {
    match rc {
        0 => Ok(()),
        rc if rc == ERROR_ACCESS_DENIED.0 as i32 => {
            Err(anyhow::anyhow!("SetDisplayConfig returned {rc}").context(DesktopUnavailable))
        }
        _ => bail!("SetDisplayConfig returned {rc}"),
    }
}
fn only_virtual(
    name: &[u16; 32],
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<()> {
    for path in paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: Sized device-info packet and validated source mode index.
        unsafe {
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || source.viewGdiDeviceName != *name
            {
                continue;
            }
            let mut modes = modes.to_vec();
            let index = path.sourceInfo.Anonymous.modeInfoIdx as usize;
            let mode = modes
                .get_mut(index)
                .context("virtual display has no source mode")?;
            anyhow::ensure!(
                mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                "invalid display source mode"
            );
            let position = mode.Anonymous.sourceMode.position;
            if paths.len() == 1 && position.x == 0 && position.y == 0 {
                return Ok(());
            }
            mode.Anonymous.sourceMode.position = Default::default();
            return apply_topology(&[*path], &modes);
        }
    }
    bail!("virtual display is missing from active display topology")
}

pub(super) struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub desc: DXGI_OUTPUT_DESC,
    pub virtual_display: bool,
    pub id: String,
}
fn text(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}
fn virtual_name(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("virtual display")
        || s.contains("mttvdd")
        || s.contains("vdd by mtt")
        || s.contains("mtt1337")
}
fn is_virtual(name: &[u16], adapter: &str) -> bool {
    if virtual_name(adapter) {
        return true;
    }
    // SAFETY: The device name is terminated; EnumDisplayDevices fills its sized output.
    unsafe {
        let mut device = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        for i in 0.. {
            if !EnumDisplayDevicesW(PCWSTR(name.as_ptr()), i, &mut device, 0).as_bool() {
                break;
            }
            if virtual_name(&text(&device.DeviceString)) || virtual_name(&text(&device.DeviceID)) {
                return true;
            }
        }
    }
    false
}
fn outputs() -> Result<Vec<Output>> {
    let _dpi = DpiAwareness::physical();
    // SAFETY: The factory and enumerated interfaces have owned COM references.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("create DXGI factory")?;
        let mut outputs = Vec::new();
        let monitors = monitor_names();
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(a) => a,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("enumerate adapters"),
            };
            let description = text(&adapter.GetDesc1()?.Description);
            for i in 0.. {
                let output = match adapter.EnumOutputs(i) {
                    Ok(o) => o,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e).context("enumerate outputs"),
                };
                let desc = output.GetDesc()?;
                if desc.AttachedToDesktop.as_bool() {
                    let virtual_display = is_virtual(&desc.DeviceName, &description);
                    outputs.push(Output {
                        adapter: adapter.clone(),
                        output,
                        desc,
                        virtual_display,
                        id: monitors
                            .iter()
                            .find(|(gdi, _, _)| *gdi == desc.DeviceName)
                            .map(|(_, id, _)| id.clone())
                            .unwrap_or_default(),
                    });
                }
            }
        }
        Ok(outputs)
    }
}
pub(super) fn sdr_white_level(name: &[u16; 32]) -> f32 {
    let Ok((paths, _)) = topology() else {
        return 1.0;
    };
    for path in paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: Both device-info packets have the SDK size and matching IDs.
        unsafe {
            if DisplayConfigGetDeviceInfo(&mut source.header) == 0
                && source.viewGdiDeviceName == *name
                && DisplayConfigGetDeviceInfo(&mut level.header) == 0
                && level.SDRWhiteLevel > 0
            {
                return level.SDRWhiteLevel as f32 / 1000.0;
            }
        }
    }
    1.0
}
fn selection_rank(virtual_display: bool, primary: bool) -> (bool, bool) {
    (!virtual_display, !primary)
}
fn primary(output: &Output) -> bool {
    output.desc.DesktopCoordinates.left == 0 && output.desc.DesktopCoordinates.top == 0
}
/// Which of `outputs` a session shares: the one named `wanted` while it is
/// attached, else the virtual display, else the primary one.
fn pick(outputs: &[(String, bool, bool)], wanted: Option<&str>) -> Option<usize> {
    wanted
        .filter(|w| !w.is_empty())
        .and_then(|w| outputs.iter().position(|(name, _, _)| name == w))
        .or_else(|| {
            outputs
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, v, p))| selection_rank(*v, *p))
                .map(|(i, _)| i)
        })
}
fn keys(outputs: &[Output]) -> Vec<(String, bool, bool)> {
    outputs
        .iter()
        .map(|o| (o.id.clone(), o.virtual_display, primary(o)))
        .collect()
}
#[cfg(test)]
pub(super) fn select() -> Result<Output> {
    select_named(None)
}
/// The output named `wanted` (its monitor device path), or the one
/// [`select`] takes when that is `None` or not attached now.
pub(super) fn select_named(wanted: Option<&str>) -> Result<Output> {
    let mut outputs = outputs()?;
    let Some(index) = pick(&keys(&outputs), wanted) else {
        bail!("no DXGI output attached to the desktop");
    };
    Ok(outputs.swap_remove(index))
}

/// Probe the adapter a display will use after the active session restores its
/// topology. Physical outputs disabled for virtual capture remain selectable.
pub(super) fn encoding_adapter(wanted: Option<&str>) -> Result<IDXGIAdapter1> {
    let saved_adapter = STATE
        .lock()
        .saved
        .as_ref()
        .map(|saved| adapter_choice(&saved.adapters, saved.default_adapter, wanted));
    let Some((high, low)) = saved_adapter else {
        return Ok(select_named(wanted)?.adapter);
    };
    // SAFETY: The saved LUID identifies an adapter, independent of active outputs.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        Ok(factory.EnumAdapterByLuid(windows::Win32::Foundation::LUID {
            HighPart: high,
            LowPart: low,
        })?)
    }
}

fn adapter_choice(
    adapters: &BTreeMap<String, (i32, u32)>,
    default: (i32, u32),
    wanted: Option<&str>,
) -> (i32, u32) {
    wanted
        .and_then(|id| adapters.get(id))
        .copied()
        .unwrap_or(default)
}

/// Join DXGI's current GDI sources to persistent monitor identities and labels.
fn monitor_names() -> Vec<([u16; 32], String, String)> {
    let Ok((paths, _)) = topology() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for path in paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: Both device-info packets have the SDK size and matching IDs.
        unsafe {
            if DisplayConfigGetDeviceInfo(&mut source.header) == 0
                && DisplayConfigGetDeviceInfo(&mut target.header) == 0
            {
                let name = text(&target.monitorFriendlyDeviceName);
                let device_path = text(&target.monitorDevicePath);
                let luid = path.targetInfo.adapterId;
                let id = monitor_identity(
                    &device_path,
                    (luid.HighPart, luid.LowPart, path.targetInfo.id),
                );
                names.push((source.viewGdiDeviceName, id, name));
            }
        }
    }
    names
}
fn monitor_identity(device_path: &str, target: (i32, u32, u32)) -> String {
    if device_path.is_empty() {
        format!("target:{:x}:{:x}:{}", target.0, target.1, target.2)
    } else {
        device_path.to_ascii_lowercase()
    }
}

/// Every output attached to the desktop, for a viewer to choose from. The
/// one [`select`] takes is the default.
pub(super) fn list() -> Result<Vec<latch_proto::control::HostDisplay>> {
    let outputs = outputs()?;
    let default = pick(&keys(&outputs), None);
    let friendly = monitor_names();
    Ok(outputs
        .iter()
        .enumerate()
        .filter(|(_, o)| !o.id.is_empty())
        .map(|(i, o)| {
            let id = o.id.clone();
            let r = o.desc.DesktopCoordinates;
            let name = if o.virtual_display {
                "Virtual display".to_string()
            } else {
                friendly
                    .iter()
                    .find(|(gdi, _, name)| *gdi == o.desc.DeviceName && !name.is_empty())
                    .map(|(_, _, name)| name.clone())
                    .unwrap_or_else(|| {
                        text(&o.desc.DeviceName)
                            .trim_start_matches(r"\\.\")
                            .to_string()
                    })
            };
            latch_proto::control::HostDisplay {
                id,
                name,
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                primary: primary(o),
                default: Some(i) == default,
            }
        })
        .collect())
}

fn current(output: &Output) -> Result<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: Output's name is terminated and mode has its required size.
    if !unsafe {
        EnumDisplaySettingsW(
            PCWSTR(output.desc.DeviceName.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    }
    .as_bool()
    {
        bail!("read display mode: {}", windows::core::Error::from_win32());
    }
    Ok(mode)
}
fn input_desktop_denied() -> bool {
    // SAFETY: This only checks access. The thread stays on its own desktop.
    unsafe {
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(desktop) => {
                let _ = CloseDesktop(desktop);
                false
            }
            Err(error) => error.code() == E_ACCESSDENIED,
        }
    }
}

fn change(name: &[u16], mode: &DEVMODEW) -> Result<()> {
    // SAFETY: The device name and DEVMODE remain valid for the call. No registry changes.
    let status = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(mode),
            HWND::default(),
            CDS_TYPE(0),
            None,
        )
    };
    mode_result(status, input_desktop_denied)
}

fn mode_result(status: DISP_CHANGE, input_denied: impl FnOnce() -> bool) -> Result<()> {
    if status == DISP_CHANGE_SUCCESSFUL {
        return Ok(());
    }
    let error = anyhow::anyhow!("ChangeDisplaySettingsExW returned {}", status.0);
    if status == DISP_CHANGE_FAILED && input_denied() {
        return Err(error.context(DesktopUnavailable));
    }
    Err(error)
}

fn set_stream_mode(
    display_name: &[u16; 32],
    (width, height): (u32, u32),
    change: impl Fn() -> Result<()>,
    read_size: impl Fn() -> Result<(u32, u32)>,
) -> Result<()> {
    change()?;
    if read_size().is_ok_and(|size| size == (width, height)) {
        return Ok(());
    }
    let retry = change();
    if retry.as_ref().is_err_and(DesktopUnavailable::is) {
        return retry;
    }
    let (actual_width, actual_height) = read_size().context("verify the virtual display size")?;
    if (actual_width, actual_height) != (width, height) {
        tracing::warn!(
            display = %text(display_name),
            retry_error = ?retry.err(),
            "requested virtual display size {width}x{height}, got {actual_width}x{actual_height}; scaling capture"
        );
    }
    Ok(())
}

// Prefer the lowest sufficient refresh, otherwise the highest available.
fn refresh_rank(hz: u32, requested: u32) -> (bool, u32) {
    (
        hz < requested,
        if hz >= requested { hz } else { u32::MAX - hz },
    )
}

struct Preparation {
    previous: Previous,
    deferred: bool,
}

impl Preparation {
    fn from_change(previous: Previous, result: Result<()>) -> Result<Self> {
        let deferred = result.as_ref().is_err_and(DesktopUnavailable::is);
        if !deferred {
            result?;
        }
        Ok(Self { previous, deferred })
    }
}

pub(super) fn prepare(
    width: u32,
    height: u32,
    fps: u32,
    display: Option<&str>,
) -> Result<Option<Previous>> {
    let mut state = STATE.lock();
    let generation = state.begin_prepare();
    match state.prepare_session(Saved::restore, |state| {
        prepare_mode(state, width, height, fps, display)
    }) {
        Ok(prepared) => Ok(prepared.map(|prepared| {
            if prepared.deferred {
                tracing::info!(
                    generation,
                    "desktop is inaccessible; deferring display preparation"
                );
                state.deferred_prepare = Some(ModeRequest {
                    width,
                    height,
                    fps,
                    display: display.map(str::to_owned),
                });
            }
            prepared.previous
        })),
        Err(error) => {
            if let Err(restore) = state.restore(generation) {
                tracing::warn!(
                    "could not restore the desktop after preparation failed: {restore:#}"
                );
            }
            Err(error)
        }
    }
}

fn retry_prepare(generation: u64, stopping: &AtomicBool) {
    loop {
        thread::sleep(Duration::from_secs(1));
        let applied = STATE
            .lock()
            .retry_prepare(generation, stopping, |state, request| {
                if input_desktop_denied() {
                    return Ok(false);
                }
                let prepared = state.prepare_session(Saved::restore, |state| {
                    prepare_mode(
                        state,
                        request.width,
                        request.height,
                        request.fps,
                        request.display.as_deref(),
                    )
                })?;
                Ok(!prepared.is_some_and(|p| p.deferred))
            });
        match applied {
            None => return,
            Some(Ok(false)) => {}
            Some(Ok(true)) => {
                tracing::info!(generation, "applied deferred display preparation");
                return;
            }
            Some(Err(error)) => {
                tracing::warn!("could not apply deferred display preparation: {error:#}");
                return;
            }
        }
    }
}

fn prepare_mode(
    state: &mut DisplayState,
    width: u32,
    height: u32,
    fps: u32,
    display: Option<&str>,
) -> Result<Option<Preparation>> {
    // A viewer that chose a real monitor shares it as it is: the virtual
    // display keeps its size and every monitor stays on.
    let output = select_named(display)?;
    if !output.virtual_display {
        return Ok(None);
    }
    let original = current(&output)?;
    let mut best: Option<DEVMODEW> = None;
    for i in 0.. {
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: Both output buffers and the display name are valid.
        if !unsafe {
            EnumDisplaySettingsW(
                PCWSTR(output.desc.DeviceName.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut mode,
            )
        }
        .as_bool()
        {
            break;
        }
        if mode.dmPelsWidth == width
            && mode.dmPelsHeight == height
            && best.as_ref().is_none_or(|b| {
                refresh_rank(mode.dmDisplayFrequency, fps) < refresh_rank(b.dmDisplayFrequency, fps)
            })
        {
            best = Some(mode);
        }
    }
    let mut mode = best.unwrap_or(original);
    if mode.dmDisplayFrequency < fps {
        tracing::info!(
            requested_fps = fps,
            refresh = mode.dmDisplayFrequency,
            "using highest available virtual display refresh rate"
        );
    }
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    let active = topology();
    let mut previous = state.saved.as_ref().map_or(
        Previous {
            width: original.dmPelsWidth,
            height: original.dmPelsHeight,
            refresh_hz: original.dmDisplayFrequency,
            generation: state.generation,
        },
        |saved| saved.previous.clone(),
    );
    previous.generation = state.generation;
    // Denied (a lock, an administrator prompt): this generation defers
    // before it changed or saved anything, and its retry reads and saves
    // the topology once the desktop is back.
    let (paths, modes) = match active {
        Ok(topology) => topology,
        Err(error) => return Preparation::from_change(previous, Err(error)).map(Some),
    };
    let adapters = if state.saved.is_none() {
        // Record adapter identity before only_virtual detaches physical outputs.
        let attached = outputs()?;
        let default = pick(&keys(&attached), None).context("no default display")?;
        let adapter_id = |output: &Output| -> Result<(i32, u32)> {
            // SAFETY: Output owns this adapter interface.
            let luid = unsafe { output.adapter.GetDesc1()? }.AdapterLuid;
            Ok((luid.HighPart, luid.LowPart))
        };
        let default_adapter = adapter_id(&attached[default])?;
        let adapters = attached
            .iter()
            .map(|output| Ok((output.id.clone(), adapter_id(output)?)))
            .collect::<Result<_>>()?;
        Some((adapters, default_adapter))
    } else {
        None
    };
    let result = (|| {
        only_virtual(&output.desc.DeviceName, &paths, &modes)?;
        if let Some((adapters, default_adapter)) = adapters {
            state.saved = Some(Saved {
                previous: previous.clone(),
                output_id: output.id.clone(),
                paths,
                modes,
                adapters,
                default_adapter,
            });
        }
        let name = &output.desc.DeviceName;
        set_stream_mode(
            name,
            (width, height),
            || change(name, &mode),
            || {
                let mode = current(&output)?;
                Ok((mode.dmPelsWidth, mode.dmPelsHeight))
            },
        )
    })();
    Preparation::from_change(previous, result).map(Some)
}

pub(super) fn restore(previous: &Previous) -> Result<()> {
    STATE.lock().restore(previous.generation)
}

impl Saved {
    fn restore(&self) -> Result<()> {
        let error = match apply_topology(&self.paths, &self.modes) {
            Ok(()) => return Ok(()),
            Err(error) if DesktopUnavailable::is(&error) => return Err(error),
            Err(error) => error,
        };
        let output = select_named(Some(&self.output_id))?;
        if self.output_id.is_empty() || output.id != self.output_id || !output.virtual_display {
            return Err(error);
        }
        tracing::warn!(
            "restore the saved display topology: {error:#}; setting the size back instead"
        );
        let mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
            dmPelsWidth: self.previous.width,
            dmPelsHeight: self.previous.height,
            dmDisplayFrequency: self.previous.refresh_hz,
            ..Default::default()
        };
        change(&output.desc.DeviceName, &mode)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn saved_display() -> DisplayState {
        DisplayState {
            generation: 1,
            saved: Some(Saved {
                previous: Previous {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                    generation: 1,
                },
                output_id: "virtual".into(),
                paths: Vec::new(),
                modes: Vec::new(),
                adapters: BTreeMap::new(),
                default_adapter: (0, 0),
            }),
            pending_restore: None,
            deferred_prepare: None,
        }
    }

    #[test]
    fn a_late_undo_cannot_restore_another_preparations_display() {
        let mut state = saved_display();
        let old = state.generation;
        let new = state.begin_prepare();
        assert!(state
            .attempt(old, |_| panic!("stale undo changed the display"))
            .is_none());
        assert!(state.saved.is_some());
        state.attempt(new, |_| Ok(())).unwrap().unwrap();
        assert!(state
            .attempt(new, |_| panic!("undo already completed"))
            .is_none());
    }

    #[test]
    fn a_new_prepare_can_inherit_the_original_topology_after_access_denied() {
        let mut state = saved_display();
        let old = state.generation;
        assert!(state
            .attempt(old, |_| Err(DesktopUnavailable.into()))
            .unwrap()
            .is_err());
        state.pending_restore = Some(old);

        // Access is still denied during the new preparation's recovery attempt.
        let new = state.begin_prepare();
        let preparation = state
            .prepare_session(
                |_| Err(DesktopUnavailable.into()),
                |_| panic!("preparation changed the active topology"),
            )
            .unwrap()
            .unwrap();
        assert!(preparation.deferred);
        assert_eq!(preparation.previous.generation, new);
        assert_eq!(state.pending_restore, Some(old));
        assert!(state
            .attempt(old, |_| panic!("an old retry changed the display"))
            .is_none());
        state
            .prepare_session(
                |saved| {
                    assert_eq!(saved.previous.width, 1920);
                    assert_eq!(saved.previous.height, 1080);
                    Ok(())
                },
                |state| {
                    assert!(
                        state.saved.is_none(),
                        "recover before selecting the display"
                    );
                    Ok(None)
                },
            )
            .unwrap();
        assert!(state.saved.is_none());
    }

    #[test]
    fn only_access_denied_defers_a_new_display_change() {
        let previous = saved_display().saved.unwrap().previous;
        assert!(
            !Preparation::from_change(previous.clone(), Ok(()))
                .unwrap()
                .deferred
        );
        assert!(
            Preparation::from_change(previous.clone(), Err(DesktopUnavailable.into()))
                .unwrap()
                .deferred
        );
        let err = Preparation::from_change(previous, Err(anyhow::anyhow!("invalid mode")))
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "invalid mode");
    }

    #[test]
    fn deferred_preparation_retries_only_for_the_running_generation() {
        let mut state = saved_display();
        state.saved = None;
        let old = state.generation;
        let generation = state.begin_prepare();
        state.deferred_prepare = Some(ModeRequest {
            width: 1280,
            height: 720,
            fps: 60,
            display: Some("virtual".into()),
        });
        let stopping = AtomicBool::new(false);
        assert!(state
            .retry_prepare(old, &stopping, |_, _| panic!("old retry"))
            .is_none());
        state.restore(old).unwrap();
        assert!(!state
            .retry_prepare(generation, &stopping, |_, _| Ok(false))
            .unwrap()
            .unwrap());
        stopping.store(true, Ordering::Release);
        assert!(state
            .retry_prepare(generation, &stopping, |_, _| panic!("stopping"))
            .is_none());
        stopping.store(false, Ordering::Release);
        assert!(state
            .retry_prepare(generation, &stopping, |_, mode| {
                assert_eq!((mode.width, mode.height, mode.fps), (1280, 720, 60));
                assert_eq!(mode.display.as_deref(), Some("virtual"));
                Ok(true)
            })
            .unwrap()
            .unwrap());
        assert!(state
            .retry_prepare(generation, &stopping, |_, _| panic!("already applied"))
            .is_none());
    }

    #[test]
    fn undo_cancels_deferred_preparation_even_without_a_saved_topology() {
        let mut state = saved_display();
        state.saved = None;
        let generation = state.generation;
        state.deferred_prepare = Some(ModeRequest {
            width: 1280,
            height: 720,
            fps: 60,
            display: None,
        });
        state.restore(generation).unwrap();
        assert!(state
            .retry_prepare(generation, &AtomicBool::new(false), |_, _| panic!("undone"))
            .is_none());
    }

    #[test]
    fn only_access_denied_keeps_a_snapshot_for_retry() {
        let mut state = saved_display();
        let generation = state.generation;
        assert!(state
            .attempt(generation, |_| Err(DesktopUnavailable.into()))
            .unwrap()
            .is_err());
        assert!(state.saved.is_some());
        assert!(state
            .attempt(generation, |_| bail!("display was removed"))
            .unwrap()
            .is_err());
        assert!(state.saved.is_none());
    }

    #[test]
    fn the_requested_size_is_read_back_and_a_mismatch_is_retried_once() {
        let requested = (3024, 1964);
        for sizes in [
            [requested, requested],
            [(800, 600), requested],
            [(800, 600); 2],
        ] {
            let writes = Cell::new(0);
            let reads = Cell::new(0);
            set_stream_mode(
                &[0; 32],
                requested,
                || {
                    writes.set(writes.get() + 1);
                    Ok(())
                },
                || {
                    let size = sizes[reads.get()];
                    reads.set(reads.get() + 1);
                    Ok(size)
                },
            )
            .unwrap();
            assert_eq!(writes.get(), if sizes[0] == requested { 1 } else { 2 });
            assert_eq!(reads.get(), writes.get());
        }
    }

    #[test]
    fn a_denied_topology_read_defers_like_a_denied_change() {
        use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_SUCCESS};

        assert!(query_result("QueryDisplayConfig", ERROR_SUCCESS).is_ok());
        let denied = query_result("QueryDisplayConfig", ERROR_ACCESS_DENIED).unwrap_err();
        assert!(DesktopUnavailable::is(&denied));
        let other = query_result("GetDisplayConfigBufferSizes", ERROR_INVALID_PARAMETER);
        assert!(!DesktopUnavailable::is(&other.unwrap_err()));
        // As prepare_mode passes it on: a deferral of this generation, not
        // an error that refuses the stream.
        let mut state = saved_display();
        state.saved = None;
        let generation = state.begin_prepare();
        let previous = Previous {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            generation,
        };
        let prepared = state
            .prepare_session(
                |_| panic!("nothing to recover"),
                |_| Preparation::from_change(previous.clone(), Err(denied)).map(Some),
            )
            .unwrap()
            .unwrap();
        assert!(prepared.deferred);
        assert_eq!(prepared.previous.generation, generation);
        assert!(state.saved.is_none(), "nothing saved while denied");
    }

    #[test]
    fn a_deferral_without_a_saved_topology_saves_it_when_applied() {
        let mut state = saved_display();
        let mut original = state.saved.take().unwrap();
        let stale = state.begin_prepare();
        state.deferred_prepare = Some(ModeRequest {
            width: 1280,
            height: 720,
            fps: 60,
            display: None,
        });
        // A newer session drops the stale deferral: it never applies there.
        let generation = state.begin_prepare();
        let stopping = AtomicBool::new(false);
        assert!(state
            .retry_prepare(stale, &stopping, |_, _| panic!("stale deferral"))
            .is_none());
        assert!(state
            .retry_prepare(generation, &stopping, |_, _| panic!("dropped"))
            .is_none());
        state.deferred_prepare = Some(ModeRequest {
            width: 1280,
            height: 720,
            fps: 60,
            display: None,
        });
        // The current one applies once the desktop is back, saving the
        // topology then, and its undo restores that.
        assert!(state
            .retry_prepare(generation, &stopping, |state, _| {
                assert!(state.saved.is_none());
                original.previous.generation = generation;
                state.saved = Some(original);
                Ok(true)
            })
            .unwrap()
            .unwrap());
        let restored = Cell::new(false);
        assert!(state
            .attempt(stale, |_| panic!("a stale undo restored"))
            .is_none());
        state
            .attempt(generation, |saved| {
                assert_eq!(saved.previous.generation, generation);
                restored.set(true);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert!(restored.get() && state.saved.is_none());
    }

    #[test]
    fn access_denied_defers_topology_restore() {
        use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;

        assert!(topology_result(0).is_ok());
        assert!(crate::video::DesktopUnavailable::is(
            &topology_result(ERROR_ACCESS_DENIED.0 as i32).unwrap_err()
        ));
        assert!(!crate::video::DesktopUnavailable::is(
            &topology_result(ERROR_INVALID_PARAMETER.0 as i32).unwrap_err()
        ));
    }

    #[test]
    fn a_generic_mode_change_failure_requires_input_desktop_access_denial() {
        assert!(mode_result(DISP_CHANGE_SUCCESSFUL, || panic!("no check needed")).is_ok());
        assert!(DesktopUnavailable::is(
            &mode_result(DISP_CHANGE_FAILED, || true).unwrap_err()
        ));
        assert!(!DesktopUnavailable::is(
            &mode_result(DISP_CHANGE_FAILED, || false).unwrap_err()
        ));
        assert!(!DesktopUnavailable::is(
            &mode_result(DISP_CHANGE_BADMODE, || panic!("no check needed")).unwrap_err()
        ));
    }
    #[test]
    #[ignore]
    fn hardware_display_modes() -> anyhow::Result<()> {
        use super::*;
        let _runtime = super::super::Runtime::new(false)?;
        let output = select()?;
        let mut mode = current(&output)?;
        if let Ok(requested) = std::env::var("LATCH_TEST_DISPLAY_MODE") {
            let parts: Vec<u32> = requested
                .split(',')
                .map(str::parse)
                .collect::<std::result::Result<_, _>>()?;
            anyhow::ensure!(parts.len() == 3, "expected width,height,refresh");
            mode.dmPelsWidth = parts[0];
            mode.dmPelsHeight = parts[1];
            mode.dmDisplayFrequency = parts[2];
            mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
            change(&output.desc.DeviceName, &mode)?;
        }
        eprintln!(
            "current {} {}x{}@{}",
            text(&output.desc.DeviceName),
            mode.dmPelsWidth,
            mode.dmPelsHeight,
            mode.dmDisplayFrequency
        );
        for i in 0.. {
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: the output's terminated name and sized descriptor are live.
            if !unsafe {
                EnumDisplaySettingsW(
                    PCWSTR(output.desc.DeviceName.as_ptr()),
                    ENUM_DISPLAY_SETTINGS_MODE(i),
                    &mut mode,
                )
            }
            .as_bool()
            {
                break;
            }
            eprintln!(
                "mode {}x{}@{}",
                mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency
            );
        }
        Ok(())
    }
    #[test]
    fn a_chosen_output_wins_while_it_is_attached() {
        let outputs = [
            (r"\\.\DISPLAY1".to_string(), false, true),
            (r"\\.\DISPLAY2".to_string(), true, false),
            (r"\\.\DISPLAY3".to_string(), false, false),
        ];
        assert_eq!(super::pick(&outputs, None), Some(1), "virtual by default");
        assert_eq!(super::pick(&outputs, Some(r"\\.\DISPLAY3")), Some(2));
        assert_eq!(super::pick(&outputs, Some(r"\\.\DISPLAY1")), Some(0));
        assert_eq!(
            super::pick(&outputs, Some(r"\\.\DISPLAY9")),
            Some(1),
            "a display that has gone falls back to the default"
        );
        assert_eq!(super::pick(&[], Some("x")), None);
        assert_eq!(
            super::pick(
                &[
                    (String::new(), false, false),
                    ("virtual".into(), true, false)
                ],
                Some("")
            ),
            Some(1)
        );
    }
    #[test]
    fn remembered_monitors_follow_device_paths_after_sources_are_renumbered() {
        let a = super::monitor_identity(r"\\?\DISPLAY#AAA#serial-a", (0, 1, 1));
        let b = super::monitor_identity(r"\\?\DISPLAY#BBB#serial-b", (0, 1, 2));
        let before = vec![(a.clone(), false, true), (b.clone(), false, false)];
        let after = vec![(b.clone(), false, true), (a.clone(), false, false)];
        assert_eq!(super::pick(&before, Some(&b)), Some(1));
        assert_eq!(super::pick(&after, Some(&b)), Some(0));
        assert_eq!(super::pick(&after[1..], Some(&b)), Some(0));
        assert_eq!(super::pick(&after, Some(r"\\.\DISPLAY2")), Some(0));
        assert_eq!(
            a,
            super::monitor_identity(r"\\?\display#aaa#serial-a", (9, 9, 9))
        );
        assert_ne!(
            super::monitor_identity("", (0, 1, 1)),
            super::monitor_identity("", (0, 1, 2))
        );
    }

    #[test]
    fn virtual_output_wins_over_primary() {
        let candidates = [(false, true), (true, false)];
        assert_eq!(
            candidates
                .iter()
                .min_by_key(|(v, p)| selection_rank(*v, *p)),
            Some(&(true, false))
        );
        assert!(selection_rank(false, true) < selection_rank(false, false));
    }
    #[test]
    fn refresh_falls_back_without_refusing_high_fps() {
        for (requested, expected) in [
            (30, 60),
            (60, 60),
            (90, 120),
            (144, 144),
            (240, 240),
            (360, 360),
            (500, 360),
        ] {
            assert_eq!(
                [60, 120, 144, 240, 360]
                    .into_iter()
                    .min_by_key(|hz| super::refresh_rank(*hz, requested)),
                Some(expected)
            );
        }
    }
    #[test]
    fn recognises_virtual_display_driver_names() {
        for name in [
            "Virtual Display Driver",
            "MttVDD",
            "VDD by MTT",
            r"MONITOR\MTT1337",
        ] {
            assert!(super::virtual_name(name));
        }
        assert!(!super::virtual_name("NVIDIA GeForce RTX 4070"));
    }
}

#[cfg(test)]
mod hdr_live_test {
    use super::*;
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Info {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        flags: u32,
        encoding: u32,
        bits: u32,
        mode: u32,
    }
    #[repr(C)]
    struct State {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        enabled: u32,
    }
    fn info(path: &DISPLAYCONFIG_PATH_INFO) -> Result<Info> {
        let mut info = Info {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(15),
                size: std::mem::size_of::<Info>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        anyhow::ensure!(
            unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } == 0,
            "GET_ADVANCED_COLOR_INFO_2 failed"
        );
        Ok(info)
    }
    fn set(path: &DISPLAYCONFIG_PATH_INFO, kind: i32, on: bool) -> Result<()> {
        let state = State {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(kind),
                size: std::mem::size_of::<State>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            enabled: u32::from(on),
        };
        anyhow::ensure!(
            unsafe { DisplayConfigSetDeviceInfo(&state.header) } == 0,
            "set colour state failed"
        );
        Ok(())
    }
    struct Restore(DISPLAYCONFIG_PATH_INFO, Info);
    impl Drop for Restore {
        fn drop(&mut self) {
            set(&self.0, 16, self.1.flags & (1 << 5) != 0).expect("restore HDR setting");
            set(&self.0, 17, self.1.flags & (1 << 7) != 0).expect("restore WCG setting");
            let after = info(&self.0).expect("verify restored display");
            assert_eq!(
                after.flags & ((1 << 5) | (1 << 7)),
                self.1.flags & ((1 << 5) | (1 << 7)),
                "display settings were not restored"
            );
            eprintln!("restored HDR/WCG flags={} mode={}", after.flags, after.mode);
        }
    }
    #[test]
    #[ignore = "interactive HDR display test; restores both HDR and WCG settings"]
    fn hold_hdr_display_for_live_test() -> Result<()> {
        let output = select()?;
        let (paths, _) = topology()?;
        let path = paths
            .into_iter()
            .find(|p| {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: p.sourceInfo.adapterId,
                        id: p.sourceInfo.id,
                    },
                    ..Default::default()
                };
                unsafe {
                    DisplayConfigGetDeviceInfo(&mut source.header) == 0
                        && source.viewGdiDeviceName == output.desc.DeviceName
                }
            })
            .context("selected display path")?;
        let before = info(&path)?;
        eprintln!(
            "initial HDR/WCG flags={} mode={}",
            before.flags, before.mode
        );
        let _restore = Restore(path, before);
        set(&path, 16, true)?;
        let after = info(&path)?;
        eprintln!("enabled HDR flags={} mode={}", after.flags, after.mode);
        anyhow::ensure!(after.mode == 2, "this display cannot enter HDR mode");
        std::fs::write("hdr-toggle-ready", "HDR enabled")?;
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(120)
            && !std::path::Path::new("hdr-toggle-stop").exists()
        {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(())
    }
}

#[cfg(test)]
mod adapter_choice_tests {
    use super::*;

    #[test]
    fn disabled_physical_outputs_keep_their_adapter_for_negotiation() {
        let primary = (0, 10);
        let secondary = (1, 20);
        let virtual_gpu = (0, 30);
        let adapters = BTreeMap::from([
            ("av1-primary".into(), primary),
            ("h264-secondary".into(), secondary),
            ("virtual".into(), virtual_gpu),
        ]);
        assert_eq!(
            adapter_choice(&adapters, virtual_gpu, Some("h264-secondary")),
            secondary
        );
        assert_eq!(
            adapter_choice(&adapters, virtual_gpu, Some("av1-primary")),
            primary
        );
        assert_eq!(adapter_choice(&adapters, virtual_gpu, None), virtual_gpu);
        assert_eq!(
            adapter_choice(&adapters, virtual_gpu, Some("unplugged")),
            virtual_gpu
        );
    }
}
