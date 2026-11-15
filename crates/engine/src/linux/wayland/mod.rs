//! Sharing a Wayland desktop: the xdg-desktop-portal ScreenCast (and,
//! where the portal has it, RemoteDesktop) session over D-Bus, frames
//! from its PipeWire stream into the same CPU pipeline X11 feeds, and
//! input through the portal or uinput.
//!
//! One portal session serves a stream's capture and input: the capture
//! opens it, the input sink finds it open and holds it too, and it closes
//! when both are gone. A capture reopened during the stream (a resize, an
//! encoder that gave way) finds it open as well, so it is approved once.

pub mod backend;
mod frame;
mod input;
mod layout;
mod pipewire;
mod pod;
pub(crate) mod portal;
pub mod token;

use crate::input::{DisplayRect, InputSink};
use crate::software::{Bgra, CpuCapture, Rect};
use anyhow::Context;
use parking_lot::Mutex;
use portal::Session;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// How long a new stream may take to send its first picture.
const FIRST_PICTURE: Duration = Duration::from_secs(10);

static SESSION: Mutex<Weak<Session>> = Mutex::new(Weak::new());
// Encoder replacement can briefly overlap captures. Keep one PipeWire consumer
// so an idle producer still supplies the latest picture to its replacement.
static CONSUMER: Mutex<Weak<Mutex<pipewire::Consumer>>> = Mutex::new(Weak::new());
/// The last screencast's size, for the status (`capture_size`) without
/// opening a session, which could prompt.
static LAST_SIZE: Mutex<Option<[u32; 2]>> = Mutex::new(None);

/// The open session, or a new one (which may ask the owner to approve).
fn session() -> anyhow::Result<Arc<Session>> {
    let mut current = SESSION.lock();
    if let Some(session) = current.upgrade() {
        return Ok(session);
    }
    let session = Arc::new(Session::open(true, portal::APPROVAL)?);
    *current = Arc::downgrade(&session);
    Ok(session)
}

/// The screen of a portal screencast.
pub struct WaylandCapture {
    consumer: Arc<Mutex<pipewire::Consumer>>,
    first: bool,
    /// The monitor's logical size, where absolute pointer positions land.
    logical: Option<(i32, i32)>,
    initial_size: (u32, u32),
    damage: Option<Vec<Rect>>,
    session: Arc<Session>,
}

impl WaylandCapture {
    pub fn new(fps: u32) -> anyhow::Result<Self> {
        let session = session().context("start the screen-sharing portal session")?;
        let stream = session.streams[0].clone();
        let mut current = CONSUMER.lock();
        let consumer = match current.upgrade() {
            Some(consumer) => consumer,
            None => {
                let remote = session
                    .pipewire()
                    .context("open the portal's PipeWire remote")?;
                let consumer = Arc::new(Mutex::new(pipewire::Consumer::connect(
                    remote,
                    stream.node,
                    fps,
                    FIRST_PICTURE,
                )?));
                *current = Arc::downgrade(&consumer);
                consumer
            }
        };
        drop(current);
        let (w, h) = consumer.lock().size();
        *LAST_SIZE.lock() = Some([w, h]);
        tracing::info!(
            "Wayland screencast of node {} at {w} × {h} ({:?}), logical {:?}",
            stream.node,
            consumer.lock().format(),
            stream.size
        );
        Ok(Self {
            consumer,
            first: true,
            logical: stream.size.map(|_| {
                let (width, height) = pointer_size(false, stream.size, (w, h));
                (width as i32, height as i32)
            }),
            initial_size: (w, h),
            damage: None,
            session,
        })
    }
}

impl CpuCapture for WaylandCapture {
    fn size(&self) -> (u32, u32) {
        self.consumer.lock().size()
    }

    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        self.session.check()?;
        match self.consumer.lock().take(frame, timeout, self.first)? {
            Some(damage) => {
                self.first = false;
                self.damage = damage;
                let size = [frame.width, frame.height];
                let mut last = LAST_SIZE.lock();
                if *last != Some(size) {
                    *last = Some(size);
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn display(&self) -> DisplayRect {
        let (w, h) = self.consumer.lock().size();
        let logical = self.logical.map(|(lw, lh)| {
            (
                (u64::from(w) * lw as u64 / u64::from(self.initial_size.0.max(1))) as i32,
                (u64::from(h) * lh as u64 / u64::from(self.initial_size.1.max(1))) as i32,
            )
        });
        let (width, height) = pointer_size(self.session.pixel_coordinates, logical, (w, h));
        DisplayRect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn damage(&self) -> Option<Vec<Rect>> {
        self.damage.clone()
    }

    fn name(&self) -> String {
        match self.session.kind {
            token::Kind::RemoteDesktop => "Wayland remote desktop portal".into(),
            token::Kind::ScreenCast => "Wayland screencast portal".into(),
        }
    }
}

/// Keyboard and mouse into the Wayland desktop: the stream's portal
/// session when it has input, else uinput.
pub fn open_input() -> anyhow::Result<Box<dyn InputSink>> {
    let open = SESSION.lock().upgrade();
    let session = match open {
        Some(s) => Some(s),
        // Nothing is being captured (a test, a tool): open one only where
        // it brings input.
        None if portal::offer().is_ok_and(|o| o.remote_desktop > 0) => Some(session()?),
        None => None,
    };
    Ok(Box::new(input::Sink::open(session)?))
}

/// The size of the last screencast, if there was one in this process.
pub fn last_size() -> Option<[u32; 2]> {
    *LAST_SIZE.lock()
}

/// Whether a restore token is kept, so that a viewer's session starts
/// without the owner at this machine.
pub fn approved() -> bool {
    token::path().is_some_and(|path| {
        let tokens = token::load(&path);
        tokens.get(token::Kind::RemoteDesktop).is_some()
            || tokens.get(token::Kind::ScreenCast).is_some()
    })
}

/// Asks the owner to approve sharing (the portal's prompt, on this
/// screen) and keeps the approval for the service's sessions.
pub fn approve() -> anyhow::Result<Option<String>> {
    let session = Session::open(true, portal::APPROVAL)?;
    let note = approval_note(session.kept_approval);
    drop(session);
    Ok(note)
}

fn pointer_size(pixels: bool, logical: Option<(i32, i32)>, buffer: (u32, u32)) -> (u32, u32) {
    if pixels {
        buffer
    } else {
        logical
            .map(|(w, h)| {
                let (w, h) = (w as u32, h as u32);
                if (w > h) != (buffer.0 > buffer.1) {
                    (h, w)
                } else {
                    (w, h)
                }
            })
            .unwrap_or(buffer)
    }
}

fn approval_note(persistent: bool) -> Option<String> {
    (!persistent).then(|| "The desktop portal approved this session only. Approve sharing on this screen at each connection.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_only_approval_succeeds_with_a_setup_note() {
        assert!(approval_note(false).unwrap().contains("each connection"));
        assert!(approval_note(true).is_none());
    }

    #[test]
    fn gnome_uses_buffer_pixels_and_kde_uses_logical_coordinates() {
        let pixels = (3840, 2160);
        let logical = Some((1920, 1080));
        assert_eq!(pointer_size(true, logical, pixels), pixels);
        assert_eq!(pointer_size(false, logical, pixels), (1920, 1080));
        assert_eq!(pointer_size(false, logical, (2160, 3840)), (1080, 1920));
        let gnome = DisplayRect {
            x: 0,
            y: 0,
            width: pixels.0,
            height: pixels.1,
        };
        assert_eq!(gnome.map(960, 540, 1920, 1080), (1920, 1080));
    }
}
