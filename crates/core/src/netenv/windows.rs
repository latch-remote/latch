use super::proxy::{destination_url, winhttp, Proxy, ProxyDiscovery, ProxySource};
use super::CaptivePortal;
use std::sync::{mpsc, Arc};
use std::time::Instant;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
use windows::Win32::Networking::WinHttp::*;

unsafe fn take_string(value: PWSTR) -> String {
    if value.is_null() {
        return String::new();
    }
    let result = value.to_string().unwrap_or_default();
    let _ = GlobalFree(HGLOBAL(value.0.cast()));
    result
}
struct Session(*mut std::ffi::c_void);
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

struct ProxyRequest {
    completion: mpsc::Sender<bool>,
    target: Vec<u16>,
    _pac: Vec<u16>,
    options: WINHTTP_AUTOPROXY_OPTIONS,
    _session: Session,
}
// WinHTTP reads these immutable inputs until HANDLE_CLOSING. The pointers in
// options refer to the owned UTF-16 buffers. The parent session is closed only
// after the resolver's final callback, and completion is a thread-safe sender.
unsafe impl Send for ProxyRequest {}
unsafe impl Sync for ProxyRequest {}

unsafe extern "system" fn proxy_callback(
    _handle: *mut std::ffi::c_void,
    context: usize,
    status: u32,
    _info: *mut std::ffi::c_void,
    _length: u32,
) {
    if context == 0 {
        return;
    }
    let request = context as *const ProxyRequest;
    match status {
        WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING => {
            // This is the last callback and cannot overlap other callbacks for
            // this handle. Release exactly the reference transferred to WinHTTP.
            drop(Arc::from_raw(request));
        }
        WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE | WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
            let _ = (*request)
                .completion
                .send(status == WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE);
        }
        _ => {}
    }
}

struct Resolver {
    // Close the child before dropping our reference to its inputs and parent.
    handle: Session,
    request: Arc<ProxyRequest>,
    completion: mpsc::Receiver<bool>,
}
impl Resolver {
    unsafe fn new(target: &str, pac: &str, detect: bool) -> Option<Self> {
        let session = Session(WinHttpOpen(
            w!("Latch network discovery"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            WINHTTP_FLAG_ASYNC,
        ));
        if session.0.is_null() {
            return None;
        }
        let previous = WinHttpSetStatusCallback(
            session.0,
            Some(proxy_callback),
            WINHTTP_CALLBACK_FLAG_GETPROXYFORURL_COMPLETE
                | WINHTTP_CALLBACK_FLAG_REQUEST_ERROR
                // WINHTTP_CALLBACK_FLAG_HANDLES from winhttp.h.
                | WINHTTP_CALLBACK_STATUS_HANDLE_CREATED
                | WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING,
            0,
        );
        if previous.is_some_and(|callback| callback as usize == usize::MAX) {
            return None;
        }
        let mut handle = std::ptr::null_mut();
        if WinHttpCreateProxyResolver(session.0, &mut handle) != 0 {
            return None;
        }
        let handle = Session(handle);
        let target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let pac: Vec<u16> = pac.encode_utf16().chain(Some(0)).collect();
        let options = WINHTTP_AUTOPROXY_OPTIONS {
            dwFlags: if detect {
                WINHTTP_AUTOPROXY_AUTO_DETECT
            } else {
                WINHTTP_AUTOPROXY_CONFIG_URL
            },
            dwAutoDetectFlags: if detect {
                WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A
            } else {
                0
            },
            lpszAutoConfigUrl: if detect {
                PCWSTR::null()
            } else {
                PCWSTR(pac.as_ptr())
            },
            ..Default::default()
        };
        let (tx, rx) = mpsc::channel();
        let request = Arc::new(ProxyRequest {
            completion: tx,
            target,
            _pac: pac,
            options,
            _session: session,
        });
        let context = Arc::into_raw(request.clone()) as usize;
        // Set the handle context before starting, so synchronous failures also
        // deliver HANDLE_CLOSING with the reference that WinHTTP owns.
        if WinHttpSetOption(
            Some(handle.0),
            WINHTTP_OPTION_CONTEXT_VALUE,
            Some(&context.to_ne_bytes()),
        )
        .is_err()
        {
            drop(handle);
            drop(Arc::from_raw(context as *const ProxyRequest));
            return None;
        }
        Some(Self {
            handle,
            request,
            completion: rx,
        })
    }

    unsafe fn start(&self) -> u32 {
        WinHttpGetProxyForUrlEx(
            self.handle.0,
            PCWSTR(self.request.target.as_ptr()),
            &self.request.options,
            Arc::as_ptr(&self.request) as usize,
        )
    }
}

struct ProxyResult(WINHTTP_PROXY_RESULT);
impl Drop for ProxyResult {
    fn drop(&mut self) {
        unsafe { WinHttpFreeProxyResult(&mut self.0) }
    }
}

fn auto_sources(explicit_pac: bool, auto_detect: bool) -> Vec<ProxySource> {
    let mut sources = Vec::new();
    if auto_detect {
        sources.push(ProxySource::WindowsWpad);
    }
    if explicit_pac {
        sources.push(ProxySource::WindowsPac);
    }
    sources
}

unsafe fn auto_proxy(
    url: &str,
    pac: &str,
    auto_detect: bool,
    deadline: Instant,
) -> Option<ProxyDiscovery> {
    // Match WinHTTP/IE precedence. fAutoLogonIfChallenged stays false.
    for source in auto_sources(!pac.is_empty(), auto_detect) {
        let detect = source == ProxySource::WindowsWpad;
        if Instant::now() >= deadline {
            break;
        }
        let resolver = Resolver::new(url, pac, detect)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Reserve time for explicit PAC when WPAD is absent or stalls.
        let attempt_deadline = if detect && !pac.is_empty() {
            Instant::now() + remaining / 2
        } else {
            deadline
        };
        let error = resolver.start();
        if error != windows::Win32::Foundation::ERROR_IO_PENDING.0
            || resolver
                .completion
                .recv_timeout(attempt_deadline.saturating_duration_since(Instant::now()))
                != Ok(true)
        {
            continue;
        }
        let mut info = WINHTTP_PROXY_RESULT::default();
        if WinHttpGetProxyResult(resolver.handle.0, &mut info) != 0 {
            continue;
        }
        let info = ProxyResult(info);
        return Some(proxy_result(&info.0, source));
    }
    None
}

unsafe fn proxy_result(info: &WINHTTP_PROXY_RESULT, source: ProxySource) -> ProxyDiscovery {
    let mut result = ProxyDiscovery::default();
    if info.pEntries.is_null() {
        return result;
    }
    for entry in std::slice::from_raw_parts(info.pEntries, info.cEntries as usize) {
        if entry.fBypass.as_bool() || !entry.fProxy.as_bool() {
            result.candidates.push(super::ProxyCandidate {
                proxy: Proxy::Direct,
                source,
            });
        } else if entry.ProxyScheme == WINHTTP_INTERNET_SCHEME_HTTP {
            if let Ok(host) = entry.pwszProxy.to_string() {
                if !host.is_empty() && entry.ProxyPort != 0 {
                    result.candidates.push(super::ProxyCandidate {
                        proxy: Proxy::Http {
                            host,
                            port: entry.ProxyPort,
                            credentials: None,
                        },
                        source,
                    });
                }
            }
        } else {
            result
                .warnings
                .push(super::NetworkWarning::UnsupportedPacProtocol);
        }
    }
    result
}

fn settings_warnings(found_user: bool, explicit_pac_failed: bool) -> Vec<super::NetworkWarning> {
    let mut warnings = Vec::new();
    if !found_user {
        warnings.push(super::NetworkWarning::WindowsServiceSettings);
    }
    if explicit_pac_failed {
        warnings.push(super::NetworkWarning::PacFailed);
    }
    warnings
}

pub(super) fn proxies(host: &str, deadline: Instant) -> ProxyDiscovery {
    unsafe {
        let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        let found_user = WinHttpGetIEProxyConfigForCurrentUser(&mut config).is_ok();
        let mut warnings = settings_warnings(found_user, false);
        let pac = take_string(config.lpszAutoConfigUrl);
        let proxy = take_string(config.lpszProxy);
        let bypass = take_string(config.lpszProxyBypass);
        if found_user && (!pac.is_empty() || config.fAutoDetect.as_bool()) {
            if let Some(mut result) = destination_url(host).and_then(|url| {
                auto_proxy(url.as_str(), &pac, config.fAutoDetect.as_bool(), deadline)
            }) {
                result.warnings.extend(warnings);
                return result;
            }
            // No WPAD is an ordinary direct network. Only an explicitly
            // configured PAC failure asks the user to fix settings.
            warnings = settings_warnings(found_user, !pac.is_empty());
        }
        if found_user && !proxy.is_empty() {
            let mut result = winhttp(&proxy, &bypass, host, ProxySource::WindowsUser);
            result.warnings.extend(warnings);
            return result;
        }
        // Services often have no per-user configuration. The machine WinHTTP
        // default is useful there and may also be explicitly set by an admin.
        let mut info = WINHTTP_PROXY_INFO::default();
        let found_machine = WinHttpGetDefaultProxyConfiguration(&mut info).is_ok();
        let proxy = take_string(info.lpszProxy);
        let bypass = take_string(info.lpszProxyBypass);
        let mut result = if found_machine && info.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY {
            winhttp(&proxy, &bypass, host, ProxySource::WindowsMachine)
        } else if !pac.is_empty() {
            ProxyDiscovery::default()
        } else {
            ProxyDiscovery::one(
                Proxy::Direct,
                if found_user {
                    ProxySource::WindowsUser
                } else {
                    ProxySource::Unconfigured
                },
            )
        };
        result.warnings.extend(warnings);
        result
    }
}

pub(super) fn portal() -> CaptivePortal {
    use windows::core::{Interface, RuntimeName, HSTRING};
    use windows::Networking::Connectivity::{
        ConnectionProfile, INetworkInformationStatics, NetworkConnectivityLevel as Level,
        NetworkInformation,
    };
    use windows::Win32::System::WinRT::{
        RoGetActivationFactory, RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
    };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { RoUninitialize() }
        }
    }
    if unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_err() {
        return CaptivePortal::Unknown;
    }
    let _apartment = Apartment;
    fn connectivity_level() -> windows::core::Result<Level> {
        let name = HSTRING::from(NetworkInformation::NAME);
        // Scope the factory to this apartment. The generated static accessor
        // caches an agile factory beyond RoUninitialize and worker teardown.
        let factory: INetworkInformationStatics = unsafe { RoGetActivationFactory(&name)? };
        let mut raw = std::ptr::null_mut();
        unsafe {
            (factory.vtable().GetInternetConnectionProfile)(factory.as_raw(), &mut raw).ok()?;
        }
        if raw.is_null() {
            return Ok(Level::None);
        }
        let profile = unsafe { ConnectionProfile::from_raw(raw) };
        profile.GetNetworkConnectivityLevel()
    }
    // Release every WinRT object and COM-backed error before closing the
    // apartment. Only the Rust diagnosis is returned to the caller.
    let level = connectivity_level();
    let portal = match level {
        Ok(Level::ConstrainedInternetAccess) => CaptivePortal::Captive { portal_url: None },
        Ok(Level::InternetAccess) => CaptivePortal::OpenInternet,
        Ok(Level::None) => CaptivePortal::NoConnectivity,
        _ => CaptivePortal::Unknown,
    };
    drop(level);
    portal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn auto_sources_follow_windows_precedence() {
        use ProxySource::{WindowsPac, WindowsWpad};
        assert_eq!(auto_sources(true, true), [WindowsWpad, WindowsPac]);
        assert_eq!(auto_sources(true, false), [WindowsPac]);
        assert_eq!(auto_sources(false, true), [WindowsWpad]);
        assert!(auto_sources(false, false).is_empty());
    }

    #[test]
    fn absent_wpad_has_no_warning_and_explicit_pac_failure_is_actionable() {
        use super::super::{NetworkWarning as W, WarningSeverity};
        assert!(settings_warnings(true, false).is_empty());
        assert_eq!(settings_warnings(true, true), [W::PacFailed]);
        assert_eq!(W::PacFailed.severity(), WarningSeverity::Actionable);
        assert_eq!(settings_warnings(false, false), [W::WindowsServiceSettings]);
        assert_eq!(
            W::WindowsServiceSettings.severity(),
            WarningSeverity::Informational
        );
    }

    #[test]
    fn real_auto_detection_obeys_the_deadline() {
        let start = Instant::now();
        unsafe {
            let _ = auto_proxy(
                "https://relay.test/",
                "",
                true,
                start + Duration::from_millis(250),
            );
        }
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn real_async_pac_download_and_stalled_server() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let location = format!("http://{}/proxy.pac", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            crate::http::read_request(&mut stream).unwrap().unwrap();
            let script =
                "function FindProxyForURL(url, host) { return 'PROXY first.test:8080; DIRECT'; }";
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}", script.len()).unwrap();
        });
        let result = unsafe {
            auto_proxy(
                "https://relay.test/",
                &location,
                false,
                Instant::now() + Duration::from_secs(3),
            )
        }
        .expect("WinHTTP evaluates local PAC");
        server.join().unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].source, ProxySource::WindowsPac);
        assert_eq!(
            result.candidates[0].proxy.to_string(),
            "http://first.test:8080"
        );
        assert_eq!(result.candidates[1].proxy, Proxy::Direct);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let location = format!("http://{}/stalled.pac", listener.local_addr().unwrap());
        let start = Instant::now();
        let result = unsafe {
            auto_proxy(
                "https://relay.test/",
                &location,
                false,
                start + Duration::from_millis(100),
            )
        };
        assert!(result.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn async_result_preserves_order_and_direct_choice() {
        let host: Vec<u16> = "proxy.test".encode_utf16().chain(Some(0)).collect();
        let mut entries = [
            WINHTTP_PROXY_RESULT_ENTRY {
                fProxy: true.into(),
                ProxyScheme: WINHTTP_INTERNET_SCHEME_HTTP,
                pwszProxy: PWSTR(host.as_ptr().cast_mut()),
                ProxyPort: 8080,
                ..Default::default()
            },
            WINHTTP_PROXY_RESULT_ENTRY::default(),
        ];
        let info = WINHTTP_PROXY_RESULT {
            cEntries: 1,
            pEntries: entries.as_mut_ptr(),
        };
        let result = unsafe { proxy_result(&info, ProxySource::WindowsPac) };
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(
            result.candidates[0].proxy.to_string(),
            "http://proxy.test:8080"
        );
        let result = unsafe {
            proxy_result(
                &WINHTTP_PROXY_RESULT {
                    cEntries: 2,
                    ..info
                },
                ProxySource::WindowsPac,
            )
        };
        assert_eq!(result.candidates[1].proxy, Proxy::Direct);
    }
    struct PacServer {
        location: String,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl PacServer {
        fn new() -> Self {
            use std::io::Write;
            use std::sync::atomic::{AtomicBool, Ordering};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let location = format!("http://{}/proxy.pac", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = stop.clone();
            let thread = std::thread::spawn(move || {
                let mut requests = 0;
                while !server_stop.load(Ordering::Relaxed) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("PAC listener failed: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    if !matches!(crate::http::read_request(&mut stream), Ok(Some(_))) {
                        continue;
                    }
                    requests += 1;
                    std::thread::sleep(Duration::from_millis(requests % 7));
                    let script = "function FindProxyForURL(url, host) { return 'PROXY stress.test:8080; DIRECT'; }";
                    // Cancellation can close the connection before this response.
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}", script.len());
                }
            });
            Self {
                location,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for PacServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn concurrent_pac_resolution_releases_cancelled_and_rejected_requests() {
        let server = PacServer::new();
        let pending = std::thread::scope(|scope| {
            let workers: Vec<_> = (1..=4)
                .map(|worker| {
                    let location = &server.location;
                    scope.spawn(move || {
                        let mut random = worker * 7919_u32;
                        let mut pending = Vec::new();
                        let mut cancelled = 0;
                        for attempt in 0..32 {
                            random ^= random << 13;
                            random ^= random >> 17;
                            random ^= random << 5;
                            let pac = format!("{location}?worker={worker}&attempt={attempt}");
                            unsafe {
                                // The real resolver is started even for a zero wait.
                                let resolver = Resolver::new("https://relay.test/", &pac, false)
                                    .expect("create WinHTTP resolver");
                                pending.push(Arc::downgrade(&resolver.request));
                                assert_eq!(
                                    resolver.start(),
                                    windows::Win32::Foundation::ERROR_IO_PENDING.0
                                );
                                if resolver
                                    .completion
                                    .recv_timeout(Duration::from_micros(u64::from(random % 32000)))
                                    == Err(mpsc::RecvTimeoutError::Timeout)
                                {
                                    cancelled += 1;
                                }
                                drop(resolver);
                                // Also close handles after synchronous URL rejection.
                                let resolver = Resolver::new("invalid URL", &pac, false).unwrap();
                                pending.push(Arc::downgrade(&resolver.request));
                                let error = resolver.start();
                                if error == windows::Win32::Foundation::ERROR_IO_PENDING.0 {
                                    let _ =
                                        resolver.completion.recv_timeout(Duration::from_millis(1));
                                }
                            }
                            // Exercise the production WPAD/PAC entry point concurrently.
                            unsafe {
                                let _ = auto_proxy(
                                    "https://relay.test/",
                                    &pac,
                                    attempt % 8 == 0,
                                    Instant::now()
                                        + Duration::from_millis(u64::from(random % 16 + 1)),
                                );
                            }
                        }
                        (pending, cancelled)
                    })
                })
                .collect();
            let mut pending = Vec::new();
            let mut cancelled = 0;
            for worker in workers {
                let (requests, timeouts) = worker.join().unwrap();
                pending.extend(requests);
                cancelled += timeouts;
            }
            assert!(cancelled > 0, "stress must exercise deadline cancellation");
            pending
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending.iter().any(|request| request.strong_count() != 0) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            pending.iter().all(|request| request.strong_count() == 0),
            "HANDLE_CLOSING must release every callback reference and its inputs"
        );
        let result = unsafe {
            auto_proxy(
                "https://relay.test/",
                &server.location,
                false,
                Instant::now() + Duration::from_secs(3),
            )
        }
        .expect("PAC still resolves after concurrent cancellation");
        assert_eq!(
            result.candidates[0].proxy.to_string(),
            "http://stress.test:8080"
        );
        assert_eq!(result.candidates[1].proxy, Proxy::Direct);
    }

    #[test]
    fn connectivity_survives_repeated_worker_apartment_teardown() {
        for _ in 0..8 {
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| {
                        for _ in 0..8 {
                            let _ = portal();
                        }
                    });
                }
            });
        }
    }
}
