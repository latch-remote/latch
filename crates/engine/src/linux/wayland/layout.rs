//! Logical output layout for libinput's whole-desktop absolute device.
use crate::input::DisplayRect;
use anyhow::{ensure, Context};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use wayland_client::{
    protocol::{wl_callback, wl_output, wl_registry},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

#[derive(Default)]
struct Outputs {
    synced: bool,
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: BTreeMap<u32, wl_output::WlOutput>,
    logical: BTreeMap<u32, DisplayRect>,
    tracked: BTreeMap<u32, zxdg_output_v1::ZxdgOutputV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Outputs {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                state
                    .outputs
                    .insert(name, registry.bind(name, version.min(4), qh, ()));
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "zxdg_output_manager_v1" => {
                state.manager = Some(registry.bind(name, version.min(3), qh, ()));
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
                state.logical.remove(&name);
                if let Some(output) = state.tracked.remove(&name) {
                    output.destroy();
                }
            }
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Outputs: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(Outputs: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let rect = state.logical.entry(*id).or_default();
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                rect.x = x;
                rect.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } if width > 0 && height > 0 => {
                rect.width = width as u32;
                rect.height = height as u32;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Outputs {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}

fn track_outputs(state: &mut Outputs, qh: &QueueHandle<Outputs>) -> anyhow::Result<bool> {
    let manager = state
        .manager
        .as_ref()
        .context("the compositor has no logical output layout for absolute input")?;
    let mut added = false;
    for (&id, output) in &state.outputs {
        if let std::collections::btree_map::Entry::Vacant(entry) = state.tracked.entry(id) {
            entry.insert(manager.get_xdg_output(output, qh, id));
            added = true;
        }
    }
    Ok(added)
}

/// Bound compositor I/O so a stalled compositor cannot hold input cleanup.
fn roundtrip(
    conn: &Connection,
    queue: &mut EventQueue<Outputs>,
    state: &mut Outputs,
) -> anyhow::Result<()> {
    state.synced = false;
    conn.display().sync(&queue.handle(), ());
    queue.flush()?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while !state.synced {
        queue.dispatch_pending(state)?;
        if state.synced {
            break;
        }
        let Some(read) = queue.prepare_read() else {
            continue;
        };
        let left = deadline
            .checked_duration_since(Instant::now())
            .context("the compositor stopped answering output layout requests")?;
        let mut fd = libc::pollfd {
            fd: read.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1) as i32) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        ensure!(
            ready > 0,
            "the compositor stopped answering output layout requests"
        );
        read.read()?;
    }
    Ok(())
}

pub struct Layout {
    conn: Connection,
    queue: EventQueue<Outputs>,
    state: Outputs,
    selected: u32,
}

impl Layout {
    pub fn open(position: Option<(i32, i32)>) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env()
            .context("connect to Wayland for the absolute input layout")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = Outputs::default();
        roundtrip(&conn, &mut queue, &mut state)?;
        track_outputs(&mut state, &qh)?;
        roundtrip(&conn, &mut queue, &mut state)?;
        let selected = match position {
            Some((x, y)) => state
                .logical
                .iter()
                .find(|(_, r)| (r.x, r.y) == (x, y))
                .map(|(&id, _)| id),
            None if state.logical.len() == 1 => state.logical.keys().next().copied(),
            _ => None,
        }
        .context("the portal did not identify a captured output in the logical desktop layout")?;
        Ok(Self {
            conn,
            queue,
            state,
            selected,
        })
    }

    pub fn position(&mut self, x: i32, y: i32, display: DisplayRect) -> anyhow::Result<(i32, i32)> {
        roundtrip(&self.conn, &mut self.queue, &mut self.state)?;
        if track_outputs(&mut self.state, &self.queue.handle())? {
            roundtrip(&self.conn, &mut self.queue, &mut self.state)?;
        }
        let selected = *self
            .state
            .logical
            .get(&self.selected)
            .context("the captured output was removed")?;
        let desktop =
            bounds(self.state.logical.values().copied()).context("empty logical desktop")?;
        ensure!(
            selected.width > 0 && selected.height > 0,
            "empty captured output"
        );
        Ok(map_position(x, y, display, selected, desktop))
    }
}

fn map_position(
    x: i32,
    y: i32,
    display: DisplayRect,
    selected: DisplayRect,
    desktop: DisplayRect,
) -> (i32, i32) {
    let x = selected.x
        + ((i64::from(x - display.x) * i64::from(selected.width)) / i64::from(display.width.max(1)))
            as i32;
    let y = selected.y
        + ((i64::from(y - display.y) * i64::from(selected.height))
            / i64::from(display.height.max(1))) as i32;
    (
        super::input::absolute_axis(x, desktop.x, desktop.width),
        super::input::absolute_axis(y, desktop.y, desktop.height),
    )
}

fn bounds(outputs: impl Iterator<Item = DisplayRect>) -> Option<DisplayRect> {
    let outputs: Vec<_> = outputs.filter(|r| r.width > 0 && r.height > 0).collect();
    let x = outputs.iter().map(|r| r.x).min()?;
    let y = outputs.iter().map(|r| r.y).min()?;
    let right = outputs
        .iter()
        .map(|r| i64::from(r.x) + i64::from(r.width))
        .max()?;
    let bottom = outputs
        .iter()
        .map(|r| i64::from(r.y) + i64::from(r.height))
        .max()?;
    Some(DisplayRect {
        x,
        y,
        width: u32::try_from(right - i64::from(x)).ok()?,
        height: u32::try_from(bottom - i64::from(y)).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the private two-output sway desktop from scripts/test-wayland.sh"]
    fn live_layout_maps_each_monitor_into_the_whole_desktop() {
        assert_eq!(std::env::var("WLR_BACKENDS").as_deref(), Ok("headless"));
        let display = DisplayRect {
            x: 0,
            y: 0,
            width: 640,
            height: 360,
        };
        let mut left = Layout::open(Some((0, 0))).unwrap();
        let mut right = Layout::open(Some((640, 0))).unwrap();
        let (x, y) = left.position(320, 180, display).unwrap();
        assert!((16380..16400).contains(&x), "left center {x}");
        assert!((32800..32900).contains(&y), "vertical center {y}");
        let (x, _) = right.position(320, 180, display).unwrap();
        assert!((49150..49210).contains(&x), "right center {x}");
    }

    #[test]
    fn absolute_range_includes_both_monitors_and_negative_origins() {
        let left = DisplayRect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let right = DisplayRect { x: 0, ..left };
        let desktop = bounds([left, right].into_iter()).unwrap();
        assert_eq!(desktop.width, 3840);
        assert_eq!(desktop.x, -1920);
        let display = DisplayRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let (left_center, _) = map_position(960, 540, display, left, desktop);
        let (right_center, _) = map_position(960, 540, display, right, desktop);
        assert!((16380..16400).contains(&left_center));
        assert!((49140..49170).contains(&right_center));
    }
}
