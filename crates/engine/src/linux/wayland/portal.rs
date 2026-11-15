//! The xdg-desktop-portal ScreenCast and RemoteDesktop interfaces, spoken
//! over the session bus with the pure-Rust `zbus`.
//!
//! A session is a remote-desktop one (screen and input together, one
//! approval) wherever the portal has RemoteDesktop, else a screencast. Its
//! restore token goes back to [`super::token`], so a later session starts
//! without asking (`persist_mode` 2: until the owner revokes it). The
//! owner approves the first one on this machine's screen; until then a
//! viewer's session waits for that prompt, up to [`APPROVAL`].

use super::token::{self, Kind};
use anyhow::{bail, ensure, Context};
use futures::StreamExt as _;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

thread_local! {
    static STARTUP: RefCell<Option<(TcpStream, Duration)>> = const { RefCell::new(None) };
}

/// Scope the portal prompt to the viewer's connection and advertised deadline.
pub fn during_startup<T>(
    tcp: &TcpStream,
    limit: Duration,
    start: impl FnOnce() -> T,
) -> std::io::Result<T> {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STARTUP.with(|s| {
                s.borrow_mut().take();
            });
        }
    }
    let tcp = tcp.try_clone()?;
    STARTUP.with(|s| *s.borrow_mut() = Some((tcp, limit)));
    let _reset = Reset;
    Ok(start())
}

fn viewer_gone() -> bool {
    STARTUP.with(|s| {
        s.borrow().as_ref().is_some_and(|(tcp, _)| {
            let mut byte = 0u8;
            // MSG_PEEK preserves protocol bytes. MSG_DONTWAIT affects this call
            // only, so the shared socket stays blocking for the session.
            let n = unsafe {
                libc::recv(
                    tcp.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            n == 0
                || (n < 0
                    && !matches!(
                        std::io::Error::last_os_error().kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ))
        })
    })
}

async fn wait_for_viewer() -> anyhow::Result<()> {
    loop {
        ensure!(
            !viewer_gone(),
            "the viewer disconnected while waiting for screen-sharing approval"
        );
        async_io::Timer::after(Duration::from_millis(100)).await;
    }
}

use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";

/// How long the owner has to answer the portal's prompt.
pub const APPROVAL: Duration = Duration::from_secs(90);
/// Leave 45 seconds for takeover and encoder setup within the viewer's
/// deadline. Legacy viewers have a 60-second budget.
pub fn approval_budget(viewer_seconds: u32) -> Duration {
    Duration::from_secs(u64::from(
        viewer_seconds.clamp(60, 180).saturating_sub(45).min(90),
    ))
}

/// How long any other portal request may take.
const REPLY: Duration = Duration::from_secs(10);

/// `SelectDevices` device types.
pub const KEYBOARD: u32 = 1;
pub const POINTER: u32 = 2;
/// `SelectSources` source types and cursor modes.
const MONITOR: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
/// Kept until the owner revokes it.
const PERSIST: u32 = 2;

/// One screencast stream the portal started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// The PipeWire node to connect to.
    pub node: u32,
    /// Where the monitor is in the compositor's logical layout, and its
    /// logical size: absolute pointer positions are in this space.
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

/// A started portal session. Closed when dropped.
pub struct Session {
    conn: zbus::Connection,
    handle: OwnedObjectPath,
    pub kind: Kind,
    /// Granted `SelectDevices` types (0 for a screencast).
    pub devices: u32,
    pub streams: Vec<Stream>,
    pub pixel_coordinates: bool,
    pub kept_approval: bool,
    closed: Arc<AtomicBool>,
}

/// What the portal offers here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offer {
    pub screen_cast: u32,
    pub remote_desktop: u32,
    pub cursor_modes: u32,
}

fn run<T>(future: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    zbus::block_on(future)
}

async fn timeout<T>(
    limit: Duration,
    what: &str,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let timer = async {
        async_io::Timer::after(limit).await;
    };
    futures::pin_mut!(future);
    futures::pin_mut!(timer);
    match futures::future::select(future, timer).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => bail!("{what} took longer than {limit:?}"),
    }
}

async fn proxy<'a>(conn: &zbus::Connection, interface: &'a str) -> anyhow::Result<zbus::Proxy<'a>> {
    Ok(zbus::Proxy::new(conn, DESTINATION, PATH, interface).await?)
}

async fn version(conn: &zbus::Connection, interface: &str) -> u32 {
    match proxy(conn, interface).await {
        Ok(p) => p.get_property::<u32>("version").await.unwrap_or(0),
        Err(_) => 0,
    }
}

/// Which interfaces the portal has, and their versions; 0 for none.
pub fn offer() -> anyhow::Result<Offer> {
    run(async {
        let conn = timeout(REPLY, "connecting to the session bus", async {
            Ok(zbus::Connection::session().await?)
        })
        .await
        .context("no D-Bus session bus (DBUS_SESSION_BUS_ADDRESS)")?;
        offer_on(&conn).await
    })
}

async fn offer_on(conn: &zbus::Connection) -> anyhow::Result<Offer> {
    timeout(REPLY, "asking the portal what it offers", async {
        let screen_cast = version(conn, SCREEN_CAST).await;
        let remote_desktop = version(conn, REMOTE_DESKTOP).await;
        let cursor_modes = match proxy(conn, SCREEN_CAST).await {
            Ok(p) if screen_cast > 0 => p
                .get_property::<u32>("AvailableCursorModes")
                .await
                .unwrap_or(0),
            _ => 0,
        };
        Ok(Offer {
            screen_cast,
            remote_desktop,
            cursor_modes,
        })
    })
    .await
}

/// The kind of session the portal can give: remote desktop with input
/// when it can, unless `input` is false.
pub fn kind_for(offer: Offer, input: bool) -> anyhow::Result<Kind> {
    ensure!(
        offer.screen_cast > 0,
        "the desktop portal has no ScreenCast interface: install the portal for this desktop \
         (xdg-desktop-portal-gnome, -kde or -wlr) and log in again"
    );
    Ok(if input && offer.remote_desktop > 0 {
        Kind::RemoteDesktop
    } else {
        Kind::ScreenCast
    })
}

/// The `SelectSources` options: one monitor with the cursor drawn in when
/// the portal can, restorable when the interface is (a remote-desktop
/// session persists through `SelectDevices` instead, and the portal
/// refuses `persist_mode` here).
pub fn source_options(
    kind: Kind,
    offer: Offer,
    restore: Option<&str>,
) -> HashMap<&'static str, Value<'static>> {
    let mut options: HashMap<&'static str, Value<'static>> = HashMap::new();
    options.insert("types", MONITOR.into());
    options.insert("multiple", false.into());
    if offer.cursor_modes & CURSOR_EMBEDDED != 0 {
        options.insert("cursor_mode", CURSOR_EMBEDDED.into());
    }
    if kind == Kind::ScreenCast && offer.screen_cast >= 4 {
        options.insert("persist_mode", PERSIST.into());
        if let Some(token) = restore {
            options.insert("restore_token", Value::from(token.to_string()));
        }
    }
    options
}

/// The `SelectDevices` options: keyboard and pointer, restorable from
/// version 2.
pub fn device_options(
    offer: Offer,
    restore: Option<&str>,
) -> HashMap<&'static str, Value<'static>> {
    let mut options: HashMap<&'static str, Value<'static>> = HashMap::new();
    options.insert("types", (KEYBOARD | POINTER).into());
    if offer.remote_desktop >= 2 {
        options.insert("persist_mode", PERSIST.into());
        if let Some(token) = restore {
            options.insert("restore_token", Value::from(token.to_string()));
        }
    }
    options
}

/// The streams in `Start`'s results.
pub fn streams(results: &HashMap<String, OwnedValue>) -> anyhow::Result<Vec<Stream>> {
    let value = results
        .get("streams")
        .context("the portal started no screencast stream")?;
    let raw = Vec::<(u32, HashMap<String, OwnedValue>)>::try_from(value.try_clone()?)
        .context("the portal's streams are malformed")?;
    let pair = |props: &HashMap<String, OwnedValue>, key: &str| {
        props
            .get(key)
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| <(i32, i32)>::try_from(v).ok())
    };
    let streams: Vec<Stream> = raw
        .iter()
        .map(|(node, props)| Stream {
            node: *node,
            position: pair(props, "position"),
            size: pair(props, "size").filter(|&(w, h)| w > 0 && h > 0),
        })
        .collect();
    ensure!(
        !streams.is_empty(),
        "the portal started no screencast stream"
    );
    Ok(streams)
}

fn string(results: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    results
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
}

fn u32_of(results: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    results
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| u32::try_from(v).ok())
}

/// The bus name in a portal request's object path: `:1.42` is `1_42`.
pub fn sender_path(unique: &str) -> String {
    unique.trim_start_matches(':').replace('.', "_")
}

fn handle_token() -> String {
    format!("latch{:016x}", rand::random::<u64>())
}

/// Calls a portal method that answers through a Request object, and waits
/// up to `limit` for its Response. `body` takes the handle token to put
/// in its options.
async fn request<B>(
    conn: &zbus::Connection,
    interface: &'static str,
    method: &'static str,
    limit: Duration,
    body: impl FnOnce(String) -> B,
) -> anyhow::Result<HashMap<String, OwnedValue>>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let token = handle_token();
    let unique = conn
        .unique_name()
        .context("no unique name on the session bus")?
        .to_string();
    let path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{token}",
        sender_path(&unique)
    );
    // Listen before asking: the answer can come before the call returns.
    let request = zbus::Proxy::new(
        conn,
        DESTINATION,
        ObjectPath::try_from(path.clone())?,
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut responses = request.receive_signal("Response").await?;
    let portal = proxy(conn, interface).await?;
    let reply = timeout(REPLY, method, async {
        Ok(portal.call_method(method, &body(token)).await?)
    })
    .await
    .with_context(|| format!("{interface}.{method}"))?;
    let handle: OwnedObjectPath = reply.body().deserialize()?;
    if handle.as_str() != path {
        // Portals before 0.9 chose their own path.
        tracing::debug!("portal request at {} rather than {path}", handle.as_str());
        let request = zbus::Proxy::new(
            conn,
            DESTINATION,
            handle.clone(),
            "org.freedesktop.portal.Request",
        )
        .await?;
        responses = request.receive_signal("Response").await?;
    }
    let response = timeout(limit, method, async {
        let response = async {
            responses
                .next()
                .await
                .context("the portal closed the request without answering")
        };
        futures::pin_mut!(response);
        let gone = wait_for_viewer();
        futures::pin_mut!(gone);
        match futures::future::select(response, gone).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right((result, _)) => {
                result?;
                unreachable!()
            }
        }
    })
    .await
    .with_context(|| format!("{interface}.{method}"));
    if response.is_err() {
        // Closing a timed-out request dismisses its permission dialog too.
        let _ = timeout(REPLY, "Request.Close", async {
            conn.call_method(
                Some(DESTINATION),
                handle,
                Some("org.freedesktop.portal.Request"),
                "Close",
                &(),
            )
            .await?;
            Ok(())
        })
        .await;
    }
    let message = response?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    match code {
        0 => Ok(results),
        1 => bail!("sharing this screen was declined on this machine"),
        _ => bail!("the portal ended {interface}.{method} (response {code})"),
    }
}

async fn watch_closed(
    conn: &zbus::Connection,
    handle: OwnedObjectPath,
    closed_state: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let session_proxy =
        zbus::Proxy::new(conn, DESTINATION, handle, "org.freedesktop.portal.Session").await?;
    let mut closed = session_proxy.receive_signal("Closed").await?;
    let alive = Arc::downgrade(closed_state);
    std::thread::Builder::new()
        .name("portal-session".into())
        .spawn(move || {
            zbus::block_on(async move {
                loop {
                    let signal = closed.next();
                    let tick = async_io::Timer::after(Duration::from_millis(100));
                    futures::pin_mut!(signal, tick);
                    match futures::future::select(signal, tick).await {
                        futures::future::Either::Left(_) => {
                            if let Some(alive) = alive.upgrade() {
                                alive.store(true, Ordering::Release);
                            }
                            break;
                        }
                        futures::future::Either::Right(_) if alive.strong_count() == 0 => break,
                        _ => {}
                    }
                }
            });
        })?;
    Ok(())
}

async fn gnome_session(conn: &zbus::Connection, handle: &OwnedObjectPath) -> bool {
    timeout(REPLY, "identifying the session portal", async {
        let bus = zbus::fdo::DBusProxy::new(conn).await?;
        let name = zbus::names::BusName::try_from("org.freedesktop.impl.portal.desktop.gnome")?;
        if !bus.name_has_owner(name).await? {
            return Ok(false);
        }
        let reply = conn
            .call_method(
                Some("org.freedesktop.impl.portal.desktop.gnome"),
                handle,
                Some("org.freedesktop.DBus.Introspectable"),
                "Introspect",
                &(),
            )
            .await?;
        let xml: String = reply.body().deserialize()?;
        Ok(xml.contains("org.freedesktop.impl.portal.Session"))
    })
    .await
    .unwrap_or(false)
}

impl Session {
    /// Starts a session, asking the owner unless a stored token restores
    /// one. `input`: with keyboard and pointer where the portal has them.
    pub fn open(input: bool, approval: Duration) -> anyhow::Result<Self> {
        let approval = STARTUP.with(|s| {
            s.borrow()
                .as_ref()
                .map_or(approval, |(_, limit)| approval.min(*limit))
        });
        run(async move {
            ensure!(
                !viewer_gone(),
                "the viewer disconnected before screen-sharing approval"
            );
            let conn = timeout(REPLY, "connecting to the session bus", async {
                Ok(zbus::Connection::session().await?)
            })
            .await
            .context("no D-Bus session bus (DBUS_SESSION_BUS_ADDRESS)")?;
            let offer = offer_on(&conn).await?;
            let kind = kind_for(offer, input)?;
            let file = token::path();
            let _lock = file.as_deref().map(token::lock).transpose()?;
            let mut stored = file.as_deref().map(token::load).unwrap_or_default();
            let restore = stored.get(kind).map(str::to_string);
            let interface = match kind {
                Kind::RemoteDesktop => REMOTE_DESKTOP,
                Kind::ScreenCast => SCREEN_CAST,
            };
            let created = request(&conn, interface, "CreateSession", REPLY, |token| {
                let mut options: HashMap<&str, Value> = HashMap::new();
                options.insert("handle_token", Value::from(token));
                options.insert("session_handle_token", Value::from(handle_token()));
                (options,)
            })
            .await?;
            let handle = string(&created, "session_handle")
                .context("the portal created a session without a handle")?;
            let handle = OwnedObjectPath::try_from(handle)?;
            let mut session = Session {
                conn: conn.clone(),
                handle: handle.clone(),
                kind,
                devices: 0,
                streams: Vec::new(),
                pixel_coordinates: false,
                kept_approval: false,
                closed: Arc::new(AtomicBool::new(false)),
            };
            watch_closed(&conn, handle.clone(), &session.closed).await?;
            if kind == Kind::RemoteDesktop {
                request(&conn, REMOTE_DESKTOP, "SelectDevices", REPLY, |token| {
                    let mut options = device_options(offer, restore.as_deref());
                    options.insert("handle_token", Value::from(token));
                    (&handle, options)
                })
                .await?;
            }
            request(&conn, SCREEN_CAST, "SelectSources", REPLY, |token| {
                let mut options = source_options(kind, offer, restore.as_deref());
                options.insert("handle_token", Value::from(token));
                (&handle, options)
            })
            .await?;
            if restore.is_none() {
                tracing::info!("asking to share this screen; approve it on this machine");
            }
            let started = request(&conn, interface, "Start", approval, |token| {
                let mut options: HashMap<&str, Value> = HashMap::new();
                options.insert("handle_token", Value::from(token));
                (&handle, "", options)
            })
            .await?;
            session.devices = u32_of(&started, "devices").unwrap_or(0);
            let given = string(&started, "restore_token");
            if let Some(file) = &file {
                // A token is good once: the one given replaces it, and a
                // portal that gives none has forgotten it.
                stored.set(kind, given.clone());
                match token::save(file, &stored) {
                    Ok(()) => session.kept_approval = given.is_some(),
                    Err(e) => tracing::warn!("could not keep the screen-sharing approval: {e:#}"),
                }
            }
            session.streams = streams(&started)?;
            // GNOME 46–48 forwards absolute coordinates to Mutter, whose
            // monitor stream divides buffer pixels by the monitor scale.
            // Test this session's backend object, since desktop environment
            // variables can disagree with portals.conf routing.
            session.pixel_coordinates = gnome_session(&conn, &handle).await;
            session.check()?;
            tracing::info!(
                "{} session started: {} stream(s), devices {:#x}, {}",
                match kind {
                    Kind::RemoteDesktop => "remote desktop",
                    Kind::ScreenCast => "screencast",
                },
                session.streams.len(),
                session.devices,
                if given.is_some() {
                    "restorable"
                } else {
                    "not restorable"
                }
            );
            Ok(session)
        })
    }

    pub fn check(&self) -> anyhow::Result<()> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            crate::video::CaptureEnded("screen sharing was stopped in the desktop portal".into())
        );
        Ok(())
    }

    /// A PipeWire connection that sees this session's streams.
    pub fn pipewire(&self) -> anyhow::Result<OwnedFd> {
        run(async {
            let portal = proxy(&self.conn, SCREEN_CAST).await?;
            let options: HashMap<&str, Value> = HashMap::new();
            let fd: zbus::zvariant::OwnedFd = timeout(REPLY, "OpenPipeWireRemote", async {
                Ok(portal
                    .call("OpenPipeWireRemote", &(&self.handle, options))
                    .await?)
            })
            .await?;
            Ok(fd.into())
        })
    }

    /// Calls a RemoteDesktop `Notify*` method with what `args` makes of
    /// the session handle (it puts an empty options dictionary,
    /// [`no_options`], after it).
    pub fn notify<B>(
        &self,
        method: &'static str,
        args: impl FnOnce(OwnedObjectPath) -> B,
    ) -> anyhow::Result<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        ensure!(
            self.kind == Kind::RemoteDesktop,
            "this screencast has no input"
        );
        let body = args(self.handle.clone());
        run(timeout(REPLY, method, async {
            self.conn
                .call_method(Some(DESTINATION), PATH, Some(REMOTE_DESKTOP), method, &body)
                .await?;
            Ok(())
        }))
    }
}

/// The options of a `Notify*` call: none.
pub fn no_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

impl Drop for Session {
    fn drop(&mut self) {
        let conn = self.conn.clone();
        let handle = self.handle.clone();
        let closed = run(async move {
            let session =
                zbus::Proxy::new(&conn, DESTINATION, handle, "org.freedesktop.portal.Session")
                    .await?;
            timeout(REPLY, "Session.Close", async {
                session.call_method("Close", &()).await?;
                Ok(())
            })
            .await
        });
        if let Err(e) = closed {
            tracing::debug!("closing the portal session: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_fits_both_current_and_legacy_viewer_deadlines() {
        assert_eq!(approval_budget(0), Duration::from_secs(15));
        assert_eq!(approval_budget(60), Duration::from_secs(15));
        assert_eq!(approval_budget(180), APPROVAL);
        assert_eq!(approval_budget(u32::MAX), APPROVAL);
    }

    struct Bus(std::process::Child, String);
    impl Bus {
        fn new() -> Self {
            use std::io::BufRead;
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut address = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            Self(child, address.trim().to_string())
        }
    }
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct PendingRequest(Arc<AtomicBool>);
    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl PendingRequest {
        fn close(&self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct Prompt {
        started: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
    }
    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl Prompt {
        async fn start(
            &self,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(object_server)] server: &zbus::ObjectServer,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let token = String::try_from(options["handle_token"].try_clone().unwrap()).unwrap();
            let path = OwnedObjectPath::try_from(format!(
                "/org/freedesktop/portal/desktop/request/{}/{token}",
                sender_path(header.sender().unwrap().as_str())
            ))
            .unwrap();
            server
                .at(path.clone(), PendingRequest(self.closed.clone()))
                .await?;
            self.started.store(true, Ordering::Release);
            Ok(path)
        }
    }
    struct TestSession;
    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl TestSession {
        fn close(&self) {}
    }

    #[test]
    fn disconnect_cancels_the_actual_portal_request_and_dismisses_its_prompt() {
        use std::net::{Shutdown, TcpListener};
        let bus = Bus::new();
        run(async {
            let started = Arc::new(AtomicBool::new(false));
            let closed = Arc::new(AtomicBool::new(false));
            let _server = zbus::connection::Builder::address(bus.1.as_str())?
                .name(DESTINATION)?
                .serve_at(
                    PATH,
                    Prompt {
                        started: started.clone(),
                        closed: closed.clone(),
                    },
                )?
                .build()
                .await?;
            let conn = zbus::connection::Builder::address(bus.1.as_str())?
                .build()
                .await?;
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let peer = TcpStream::connect(listener.local_addr()?)?;
            let (tcp, _) = listener.accept()?;
            let disconnect = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !started.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                peer.shutdown(Shutdown::Both).unwrap();
            });
            let now = std::time::Instant::now();
            let result = during_startup(&tcp, APPROVAL, || {
                run(request(&conn, SCREEN_CAST, "Start", APPROVAL, |token| {
                    let mut options = no_options();
                    options.insert("handle_token", token.into());
                    (options,)
                }))
            })?;
            disconnect.join().unwrap();
            assert!(format!("{:#}", result.unwrap_err()).contains("viewer disconnected"));
            assert!(
                closed.load(Ordering::Acquire),
                "Request.Close must dismiss the prompt"
            );
            assert!(now.elapsed() < Duration::from_secs(3));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn session_closed_is_observed_even_without_pipewire_activity() {
        let bus = Bus::new();
        run(async {
            let path = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/test")?;
            let server = zbus::connection::Builder::address(bus.1.as_str())?
                .name(DESTINATION)?
                .serve_at(path.clone(), TestSession)?
                .build()
                .await?;
            let conn = zbus::connection::Builder::address(bus.1.as_str())?
                .build()
                .await?;
            let closed = Arc::new(AtomicBool::new(false));
            watch_closed(&conn, path.clone(), &closed).await?;
            let session = Session {
                conn,
                handle: path.clone(),
                closed: closed.clone(),
                kind: Kind::ScreenCast,
                devices: 0,
                streams: Vec::new(),
                pixel_coordinates: false,
                kept_approval: false,
            };
            server
                .emit_signal(
                    None::<&str>,
                    path,
                    "org.freedesktop.portal.Session",
                    "Closed",
                    &no_options(),
                )
                .await?;
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !closed.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
            assert!(session
                .check()
                .unwrap_err()
                .is::<crate::video::CaptureEnded>());
            Ok(())
        })
        .unwrap();
    }

    fn offer(screen_cast: u32, remote_desktop: u32, cursor_modes: u32) -> Offer {
        Offer {
            screen_cast,
            remote_desktop,
            cursor_modes,
        }
    }

    #[test]
    fn a_portal_without_remote_desktop_gives_a_screencast() {
        assert_eq!(kind_for(offer(5, 2, 7), true).unwrap(), Kind::RemoteDesktop);
        assert_eq!(kind_for(offer(5, 2, 7), false).unwrap(), Kind::ScreenCast);
        assert_eq!(kind_for(offer(4, 0, 7), true).unwrap(), Kind::ScreenCast);
        assert!(kind_for(offer(0, 2, 0), true).is_err());
    }

    #[test]
    fn a_screencast_persists_through_its_sources_and_a_remote_desktop_through_its_devices() {
        let cast = source_options(Kind::ScreenCast, offer(4, 0, 7), Some("t1"));
        assert_eq!(cast.get("persist_mode"), Some(&Value::from(2u32)));
        assert_eq!(cast.get("restore_token"), Some(&Value::from("t1")));
        assert_eq!(cast.get("cursor_mode"), Some(&Value::from(2u32)));
        assert_eq!(cast.get("types"), Some(&Value::from(1u32)));
        assert_eq!(cast.get("multiple"), Some(&Value::from(false)));

        let old = source_options(Kind::ScreenCast, offer(3, 0, 1), Some("t1"));
        assert!(
            !old.contains_key("persist_mode"),
            "version 3 cannot persist"
        );
        assert!(!old.contains_key("restore_token"));
        assert!(
            !old.contains_key("cursor_mode"),
            "no embedded cursor offered"
        );

        let rd = source_options(Kind::RemoteDesktop, offer(5, 2, 7), Some("t1"));
        assert!(!rd.contains_key("persist_mode"));
        assert!(!rd.contains_key("restore_token"));
        let devices = device_options(offer(5, 2, 7), Some("t2"));
        assert_eq!(devices.get("types"), Some(&Value::from(3u32)));
        assert_eq!(devices.get("persist_mode"), Some(&Value::from(2u32)));
        assert_eq!(devices.get("restore_token"), Some(&Value::from("t2")));
        let first = device_options(offer(5, 2, 7), None);
        assert!(first.contains_key("persist_mode"));
        assert!(!first.contains_key("restore_token"));
        let v1 = device_options(offer(5, 1, 7), Some("t2"));
        assert!(!v1.contains_key("persist_mode"));
    }

    #[test]
    fn request_paths_use_the_escaped_bus_name() {
        assert_eq!(sender_path(":1.42"), "1_42");
        assert_eq!(sender_path(":1.2.3"), "1_2_3");
        let token = handle_token();
        assert!(token.starts_with("latch") && token.len() == 21);
        assert!(token.bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn started_streams_are_read_with_their_layout() {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert(
            "position".into(),
            Value::from((1920i32, 0i32)).try_into().unwrap(),
        );
        props.insert(
            "size".into(),
            Value::from((2560i32, 1440i32)).try_into().unwrap(),
        );
        props.insert("source_type".into(), Value::from(1u32).try_into().unwrap());
        let mut bare: HashMap<String, OwnedValue> = HashMap::new();
        bare.insert("size".into(), Value::from((0i32, 0i32)).try_into().unwrap());
        let list = vec![(42u32, props), (43u32, bare)];
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert("streams".into(), Value::from(list).try_into().unwrap());
        results.insert(
            "restore_token".into(),
            Value::from("abc").try_into().unwrap(),
        );
        results.insert("devices".into(), Value::from(3u32).try_into().unwrap());
        assert_eq!(
            streams(&results).unwrap(),
            vec![
                Stream {
                    node: 42,
                    position: Some((1920, 0)),
                    size: Some((2560, 1440)),
                },
                Stream {
                    node: 43,
                    position: None,
                    size: None,
                },
            ]
        );
        assert_eq!(string(&results, "restore_token").as_deref(), Some("abc"));
        assert_eq!(u32_of(&results, "devices"), Some(3));
        assert!(streams(&HashMap::new()).is_err());
        let mut wrong: HashMap<String, OwnedValue> = HashMap::new();
        wrong.insert("streams".into(), Value::from("no").try_into().unwrap());
        assert!(streams(&wrong).is_err());
    }
}
