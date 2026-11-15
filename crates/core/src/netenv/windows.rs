use super::proxy::{destination_url, winhttp, Proxy, ProxyDiscovery, ProxySource};
use super::CaptivePortal;
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
        0,
    ));
    if session.0.is_null() {
        return None;
    }
    let milliseconds = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .clamp(1, 15000) as i32;
    let _ = WinHttpSetTimeouts(
        session.0,
        milliseconds,
        milliseconds,
        milliseconds,
        milliseconds,
    );
    let target: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let pac: Vec<u16> = pac.encode_utf16().chain(Some(0)).collect();
    // Explicit PAC first, then WPAD on failure. Never send current Windows
    // credentials to a discovered PAC server (fAutoLogonIfChallenged=false).
    for detect in [false, true] {
        if (!detect && pac.len() == 1) || (detect && !auto_detect) {
            continue;
        }
        let mut options = WINHTTP_AUTOPROXY_OPTIONS {
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
        let mut info = WINHTTP_PROXY_INFO::default();
        let success =
            WinHttpGetProxyForUrl(session.0, PCWSTR(target.as_ptr()), &mut options, &mut info)
                .is_ok();
        let proxy = take_string(info.lpszProxy);
        let bypass = take_string(info.lpszProxyBypass);
        if success {
            let source = if detect {
                ProxySource::WindowsWpad
            } else {
                ProxySource::WindowsPac
            };
            return Some(if info.dwAccessType == WINHTTP_ACCESS_TYPE_NO_PROXY {
                ProxyDiscovery::one(Proxy::Direct, source)
            } else {
                winhttp(
                    &proxy,
                    &bypass,
                    url::Url::parse(url).ok()?.host_str()?,
                    source,
                )
            });
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    None
}

pub(super) fn proxies(host: &str, deadline: Instant) -> ProxyDiscovery {
    let mut warnings = vec!["Windows proxy discovery reads the calling account's Internet settings. A background service may see different settings from the signed-in desktop. Configure an explicit proxy for that service when needed.".into()];
    unsafe {
        let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        let found_user = WinHttpGetIEProxyConfigForCurrentUser(&mut config).is_ok();
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
            warnings.push("Automatic proxy discovery failed or timed out. Static settings are used when available.".into());
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
        } else if !pac.is_empty() || config.fAutoDetect.as_bool() {
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
    if unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_err() {
        return CaptivePortal::Unknown;
    }
    let level = NetworkInformation::GetInternetConnectionProfile()
        .and_then(|profile| profile.GetNetworkConnectivityLevel());
    unsafe {
        RoUninitialize();
    }
    match level {
        Ok(Level::ConstrainedInternetAccess) => CaptivePortal::Captive { portal_url: None },
        Ok(Level::InternetAccess) => CaptivePortal::OpenInternet,
        Ok(Level::None) => CaptivePortal::NoConnectivity,
        _ => CaptivePortal::Unknown,
    }
}
