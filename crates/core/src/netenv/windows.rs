use super::proxy::{destination_url, winhttp, Proxy, ProxyDiscovery, ProxySource};
use super::CaptivePortal;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
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

// Numeric callback tokens avoid retaining pointers into timed-out callers.
// Closing an async resolver cancels WPAD/PAC. Late callbacks find no receiver.
fn callbacks() -> &'static Mutex<HashMap<usize, mpsc::Sender<bool>>> {
    static CALLBACKS: OnceLock<Mutex<HashMap<usize, mpsc::Sender<bool>>>> = OnceLock::new();
    CALLBACKS.get_or_init(Mutex::default)
}
unsafe extern "system" fn proxy_callback(
    _handle: *mut std::ffi::c_void,
    context: usize,
    status: u32,
    _info: *mut std::ffi::c_void,
    _length: u32,
) {
    if matches!(
        status,
        WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE | WINHTTP_CALLBACK_STATUS_REQUEST_ERROR
    ) {
        if let Ok(callbacks) = callbacks().lock() {
            if let Some(tx) = callbacks.get(&context) {
                let _ = tx.send(status == WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE);
            }
        }
    }
}
struct CallbackToken(usize);
impl Drop for CallbackToken {
    fn drop(&mut self) {
        callbacks().lock().unwrap().remove(&self.0);
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
    let target: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let pac: Vec<u16> = pac.encode_utf16().chain(Some(0)).collect();
    // Match WinHTTP/IE precedence. fAutoLogonIfChallenged stays false.
    for source in auto_sources(pac.len() > 1, auto_detect) {
        let detect = source == ProxySource::WindowsWpad;
        if Instant::now() >= deadline {
            break;
        }
        let mut handle = std::ptr::null_mut();
        if WinHttpCreateProxyResolver(session.0, &mut handle) != 0 {
            return None;
        }
        let resolver = Session(handle);
        WinHttpSetStatusCallback(
            resolver.0,
            Some(proxy_callback),
            WINHTTP_CALLBACK_FLAG_GETPROXYFORURL_COMPLETE | WINHTTP_CALLBACK_FLAG_REQUEST_ERROR,
            0,
        );
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
        static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
        let token = CallbackToken(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel();
        callbacks().lock().unwrap().insert(token.0, tx);
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Reserve time for explicit PAC when WPAD is absent or stalls.
        let attempt_deadline = if detect && pac.len() > 1 {
            Instant::now() + remaining / 2
        } else {
            deadline
        };
        let error = WinHttpGetProxyForUrlEx(resolver.0, PCWSTR(target.as_ptr()), &options, token.0);
        if error != windows::Win32::Foundation::ERROR_IO_PENDING.0
            || rx.recv_timeout(attempt_deadline.saturating_duration_since(Instant::now()))
                != Ok(true)
        {
            continue;
        }
        let mut info = WINHTTP_PROXY_RESULT::default();
        if WinHttpGetProxyResult(resolver.0, &mut info) != 0 {
            continue;
        }
        let result = proxy_result(&info, source);
        WinHttpFreeProxyResult(&mut info);
        return Some(result);
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
    use windows::Networking::Connectivity::{
        NetworkConnectivityLevel as Level, NetworkInformation,
    };
    use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
    eprintln!("portal before RoInitialize");
    if unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_err() {
        return CaptivePortal::Unknown;
    }
    eprintln!("portal before GetInternetConnectionProfile");
    let level = NetworkInformation::GetInternetConnectionProfile().and_then(|profile| {
        eprintln!("portal before GetNetworkConnectivityLevel");
        profile.GetNetworkConnectivityLevel()
    });
    eprintln!("portal level {level:?}");
    unsafe {
        eprintln!("portal before RoUninitialize");
        RoUninitialize();
        eprintln!("portal after RoUninitialize");
    }
    match level {
        Ok(Level::ConstrainedInternetAccess) => CaptivePortal::Captive { portal_url: None },
        Ok(Level::InternetAccess) => CaptivePortal::OpenInternet,
        Ok(Level::None) => CaptivePortal::NoConnectivity,
        _ => CaptivePortal::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn repeated_portal_worker_teardown() {
        for iteration in 0..16 {
            eprintln!("portal worker iteration {iteration}");
            std::thread::spawn(portal).join().unwrap();
        }
    }

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
}
