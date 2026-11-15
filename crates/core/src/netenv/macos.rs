//! System frameworks only. Every Create/Copy value is owned by `Owned`.
use super::proxy::{destination_url, Proxy, ProxyDiscovery, ProxySource};
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

type Ref = *const c_void;
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: Ref);
    fn CFRetain(value: Ref) -> Ref;
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFStringCreateWithCString(allocator: Ref, text: *const c_char, encoding: u32) -> Ref;
    fn CFStringGetCString(value: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
    fn CFDataGetLength(data: Ref) -> isize;
    fn CFDataGetBytePtr(data: Ref) -> *const u8;
    fn CFDictionaryGetValue(dictionary: Ref, key: Ref) -> Ref;
    fn CFArrayGetCount(array: Ref) -> isize;
    fn CFArrayGetValueAtIndex(array: Ref, index: isize) -> Ref;
    fn CFEqual(a: Ref, b: Ref) -> bool;
    fn CFNumberGetValue(number: Ref, kind: isize, value: *mut c_void) -> bool;
    fn CFURLCreateWithString(allocator: Ref, string: Ref, base: Ref) -> Ref;
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRunLoopSourceInvalidate(source: Ref);
    fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
    static kCFRunLoopDefaultMode: Ref;
}
#[link(name = "SystemConfiguration", kind = "framework")]
extern "C" {
    fn SCDynamicStoreCopyDHCPInfo(store: Ref, service_id: Ref) -> Ref;
    fn DHCPInfoGetOptionData(info: Ref, code: u8) -> Ref;
}

#[repr(C)]
struct ClientContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    release: Option<unsafe extern "C" fn(*mut c_void)>,
    copy_description: Option<unsafe extern "C" fn(*mut c_void) -> Ref>,
}
#[link(name = "CFNetwork", kind = "framework")]
extern "C" {
    fn CFNetworkCopySystemProxySettings() -> Ref;
    fn CFNetworkCopyProxiesForURL(url: Ref, settings: Ref) -> Ref;
    fn CFNetworkExecuteProxyAutoConfigurationURL(
        pac: Ref,
        target: Ref,
        callback: unsafe extern "C" fn(*mut c_void, Ref, Ref),
        context: *mut ClientContext,
    ) -> Ref;
    fn CFNetworkExecuteProxyAutoConfigurationScript(
        script: Ref,
        target: Ref,
        callback: unsafe extern "C" fn(*mut c_void, Ref, Ref),
        context: *mut ClientContext,
    ) -> Ref;
    static kCFProxyTypeKey: Ref;
    static kCFProxyHostNameKey: Ref;
    static kCFProxyPortNumberKey: Ref;
    static kCFProxyUsernameKey: Ref;
    static kCFProxyPasswordKey: Ref;
    static kCFProxyAutoConfigurationURLKey: Ref;
    static kCFProxyAutoConfigurationJavaScriptKey: Ref;
    static kCFProxyTypeNone: Ref;
    static kCFProxyTypeHTTP: Ref;
    static kCFProxyTypeHTTPS: Ref;
    static kCFProxyTypeAutoConfigurationURL: Ref;
    static kCFProxyTypeAutoConfigurationJavaScript: Ref;
}
struct Owned(Ref);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
}
fn string(value: &str) -> Option<Owned> {
    let text = std::ffi::CString::new(value).ok()?;
    let value = unsafe { CFStringCreateWithCString(ptr::null(), text.as_ptr(), 0x08000100) };
    (!value.is_null()).then_some(Owned(value))
}
unsafe fn text(value: Ref) -> Option<String> {
    if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
        return None;
    }
    let mut buffer = [0u8; 16384];
    if !CFStringGetCString(
        value,
        buffer.as_mut_ptr().cast(),
        buffer.len() as isize,
        0x08000100,
    ) {
        return None;
    }
    Some(
        std::ffi::CStr::from_ptr(buffer.as_ptr().cast())
            .to_string_lossy()
            .into_owned(),
    )
}
fn url(value: &str) -> Option<Owned> {
    let value = string(value)?;
    let url = unsafe { CFURLCreateWithString(ptr::null(), value.0, ptr::null()) };
    (!url.is_null()).then_some(Owned(url))
}
#[derive(Default)]
struct PacResult {
    completed: bool,
    proxies: Option<Owned>,
}
// The callback supplies an immutable proxy array retained with CFRetain. It can
// be transferred between threads, with all access serialized by the mutex.
unsafe impl Send for PacResult {}

unsafe extern "C" fn retain_pac(info: *mut c_void) -> *mut c_void {
    Arc::increment_strong_count(info.cast::<Mutex<PacResult>>());
    info
}
unsafe extern "C" fn release_pac(info: *mut c_void) {
    drop(Arc::from_raw(info.cast::<Mutex<PacResult>>()));
}
unsafe extern "C" fn pac_callback(info: *mut c_void, proxies: Ref, _error: Ref) {
    if let Ok(mut result) = (*info.cast::<Mutex<PacResult>>()).lock() {
        result.completed = true;
        if !proxies.is_null() {
            result.proxies = Some(Owned(CFRetain(proxies)));
        }
    }
}
unsafe fn evaluate(pac: Ref, script: bool, target: Ref, deadline: Instant) -> Option<Owned> {
    let result = Arc::new(Mutex::new(PacResult::default()));
    let mut context = ClientContext {
        version: 0,
        info: Arc::as_ptr(&result).cast_mut().cast(),
        retain: Some(retain_pac),
        release: Some(release_pac),
        copy_description: None,
    };
    let source = Owned(if script {
        CFNetworkExecuteProxyAutoConfigurationScript(pac, target, pac_callback, &mut context)
    } else {
        CFNetworkExecuteProxyAutoConfigurationURL(pac, target, pac_callback, &mut context)
    });
    if source.0.is_null() {
        return None;
    }
    let run_loop = CFRunLoopGetCurrent();
    CFRunLoopAddSource(run_loop, source.0, kCFRunLoopDefaultMode);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if result.lock().unwrap().completed || remaining.is_zero() {
            break;
        }
        let status = CFRunLoopRunInMode(kCFRunLoopDefaultMode, remaining.as_secs_f64(), true);
        if matches!(status, 1 | 2) {
            break;
        } // Finished or stopped.
    }
    // CFNetwork delivers the callback on this run loop. Invalidation cancels
    // delivery, while the context retain/release callbacks keep state alive
    // for any native work still holding it after the deadline.
    CFRunLoopSourceInvalidate(source.0);
    CFRunLoopRemoveSource(run_loop, source.0, kCFRunLoopDefaultMode);
    drop(source);
    let proxies = result.lock().unwrap().proxies.take();
    proxies
}

unsafe fn number(settings: Ref, key: &str) -> i32 {
    let mut value: i32 = 0;
    if let Some(key) = string(key) {
        let raw = CFDictionaryGetValue(settings, key.0);
        if !raw.is_null() && CFGetTypeID(raw) == CFNumberGetTypeID() {
            CFNumberGetValue(raw, 3, (&mut value as *mut i32).cast());
        }
    }
    value
}

unsafe fn bypassed(settings: Ref, host: &str) -> bool {
    let mut exceptions = Vec::new();
    if let Some(key) = string("ExceptionsList") {
        let array = CFDictionaryGetValue(settings, key.0);
        if !array.is_null() && CFGetTypeID(array) == CFArrayGetTypeID() {
            for index in 0..CFArrayGetCount(array).min(4096) {
                if let Some(value) = text(CFArrayGetValueAtIndex(array, index)) {
                    exceptions.push(value);
                }
            }
        }
    }
    super::proxy::bypass(&exceptions.join(","), host, 443, true)
        || (number(settings, "ExcludeSimpleHostnames") != 0
            && !host.contains('.')
            && host.parse::<std::net::IpAddr>().is_err())
}

unsafe fn collect(
    array: Ref,
    target: Ref,
    source: ProxySource,
    deadline: Instant,
    result: &mut ProxyDiscovery,
    depth: u8,
) {
    if array.is_null() || CFGetTypeID(array) != CFArrayGetTypeID() || depth > 1 {
        return;
    }
    for index in 0..CFArrayGetCount(array).min(128) {
        let entry = CFArrayGetValueAtIndex(array, index);
        if entry.is_null() || CFGetTypeID(entry) != CFDictionaryGetTypeID() {
            continue;
        }
        let kind = CFDictionaryGetValue(entry, kCFProxyTypeKey);
        if kind.is_null() {
            continue;
        }
        if CFEqual(kind, kCFProxyTypeNone) {
            result.add("direct", source);
        } else if CFEqual(kind, kCFProxyTypeHTTP) || CFEqual(kind, kCFProxyTypeHTTPS) {
            let host = text(CFDictionaryGetValue(entry, kCFProxyHostNameKey));
            let number = CFDictionaryGetValue(entry, kCFProxyPortNumberKey);
            let mut port: i32 = 0;
            if number.is_null()
                || CFGetTypeID(number) != CFNumberGetTypeID()
                || !CFNumberGetValue(number, 3, (&mut port as *mut i32).cast())
            {
                continue;
            }
            if let (Some(host), Ok(port)) = (host, u16::try_from(port)) {
                if port == 0 {
                    continue;
                }
                let username = text(CFDictionaryGetValue(entry, kCFProxyUsernameKey));
                let password = text(CFDictionaryGetValue(entry, kCFProxyPasswordKey));
                let credentials = system_credentials(username, password, &mut result.warnings);
                result.candidates.push(super::ProxyCandidate {
                    proxy: Proxy::Http {
                        host,
                        port,
                        credentials,
                    },
                    source,
                });
            }
        } else if CFEqual(kind, kCFProxyTypeAutoConfigurationURL)
            || CFEqual(kind, kCFProxyTypeAutoConfigurationJavaScript)
        {
            let script = CFEqual(kind, kCFProxyTypeAutoConfigurationJavaScript);
            let pac = CFDictionaryGetValue(
                entry,
                if script {
                    kCFProxyAutoConfigurationJavaScriptKey
                } else {
                    kCFProxyAutoConfigurationURLKey
                },
            );
            if depth == 0 && !pac.is_null() {
                if let Some(evaluated) = evaluate(pac, script, target, deadline) {
                    collect(
                        evaluated.0,
                        target,
                        ProxySource::MacPac,
                        deadline,
                        result,
                        depth + 1,
                    );
                    // The PAC result owns the complete ordered decision.
                    // CFNetwork's static tail must not add a DIRECT option.
                    return;
                } else {
                    result.warnings.push(super::NetworkWarning::PacFailed);
                }
            }
        } else {
            result
                .warnings
                .push(super::NetworkWarning::UnsupportedSystemProtocol);
        }
    }
}

fn system_credentials(
    username: Option<String>,
    password: Option<String>,
    warnings: &mut Vec<super::NetworkWarning>,
) -> Option<super::Credentials> {
    match (username, password) {
        (username, Some(password)) => Some(super::Credentials::new(
            username.unwrap_or_default(),
            password,
        )),
        (Some(_), None) => {
            warnings.push(super::NetworkWarning::KeychainCredentialsUnread);
            None
        }
        (None, None) => None,
    }
}

unsafe fn finish_wpad(
    evaluated: Option<Owned>,
    fallback: Vec<super::ProxyCandidate>,
    target: Ref,
    deadline: Instant,
    result: &mut ProxyDiscovery,
) {
    if let Some(wpad) = evaluated {
        collect(wpad.0, target, ProxySource::MacWpad, deadline, result, 1);
    } else {
        result.warnings.push(super::NetworkWarning::WpadFailed);
        result.candidates = fallback;
    }
}

pub(super) fn proxies(host: &str, deadline: Instant) -> ProxyDiscovery {
    let mut result = ProxyDiscovery::default();
    let Some(target) = destination_url(host).and_then(|target| url(target.as_str())) else {
        return result;
    };
    unsafe {
        let settings = Owned(CFNetworkCopySystemProxySettings());
        if settings.0.is_null() {
            if let Some(snapshot) = super::command("/usr/sbin/scutil", &["--proxy"], deadline) {
                let mut fallback = super::proxy::scutil(&snapshot, host);
                fallback
                    .warnings
                    .push(super::NetworkWarning::MacStaticFallback);
                return fallback;
            }
            result
                .warnings
                .push(super::NetworkWarning::MacSettingsUnavailable);
            return result;
        }
        let proxies = Owned(CFNetworkCopyProxiesForURL(target.0, settings.0));
        collect(
            proxies.0,
            target.0,
            ProxySource::MacSystem,
            deadline,
            &mut result,
            0,
        );
        // CFNetwork resolves explicit PAC but does not perform WPAD. Read
        // DHCP option 252 from the primary service, then try DNS WPAD.
        if number(settings.0, "ProxyAutoDiscoveryEnable") != 0
            && number(settings.0, "ProxyAutoConfigEnable") == 0
            && !bypassed(settings.0, host)
        {
            let fallback = std::mem::take(&mut result.candidates);
            let dhcp = Owned(SCDynamicStoreCopyDHCPInfo(ptr::null(), ptr::null()));
            let dhcp_url = if dhcp.0.is_null() {
                None
            } else {
                let data = DHCPInfoGetOptionData(dhcp.0, 252);
                if data.is_null() {
                    None
                } else {
                    let length = CFDataGetLength(data);
                    if !(1..=16384).contains(&length) {
                        None
                    } else {
                        let bytes =
                            std::slice::from_raw_parts(CFDataGetBytePtr(data), length as usize);
                        std::str::from_utf8(bytes)
                            .ok()
                            .map(|s| s.trim_matches(['\0', ' ', '\r', '\n']).to_owned())
                    }
                }
            };
            let mut evaluated = None;
            for location in dhcp_url
                .as_deref()
                .into_iter()
                .chain(Some("http://wpad/wpad.dat"))
            {
                if Instant::now() >= deadline {
                    break;
                }
                if !url::Url::parse(location).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                }) {
                    continue;
                }
                evaluated =
                    url(location).and_then(|pac| evaluate(pac.0, false, target.0, deadline));
                if evaluated.is_some() {
                    break;
                }
            }
            finish_wpad(evaluated, fallback, target.0, deadline, &mut result);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_context_retains_state_until_its_release() {
        let result = Arc::new(Mutex::new(PacResult::default()));
        let weak = Arc::downgrade(&result);
        let info = Arc::as_ptr(&result).cast_mut().cast();
        unsafe {
            retain_pac(info);
            drop(result);
            assert!(weak.upgrade().is_some());
            pac_callback(info, ptr::null(), ptr::null());
            assert!(weak.upgrade().unwrap().lock().unwrap().completed);
            release_pac(info);
        }
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn keychain_username_never_becomes_empty_password_auth() {
        let mut warnings = Vec::new();
        assert!(system_credentials(Some("alice".into()), None, &mut warnings).is_none());
        assert_eq!(
            warnings,
            [super::super::NetworkWarning::KeychainCredentialsUnread]
        );
        let auth =
            system_credentials(Some("alice".into()), Some("secret".into()), &mut warnings).unwrap();
        assert_eq!((auth.username(), auth.password()), ("alice", "secret"));
        // An explicitly supplied empty password remains usable.
        assert_eq!(
            system_credentials(Some("alice".into()), Some(String::new()), &mut warnings)
                .unwrap()
                .password(),
            ""
        );
    }

    #[test]
    fn successful_wpad_owns_the_direct_decision() {
        let target = url("https://relay.test/").unwrap();
        for direct in [false, true] {
            let script = string(&format!(
                "function FindProxyForURL(url, host) {{ return 'PROXY pac.test:8080{}'; }}",
                if direct { "; DIRECT" } else { "" }
            ))
            .unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            let fallback = ProxyDiscovery::one(Proxy::Direct, ProxySource::MacSystem).candidates;
            let mut result = ProxyDiscovery::default();
            unsafe {
                let evaluated =
                    evaluate(script.0, true, target.0, deadline).expect("PAC evaluation");
                finish_wpad(Some(evaluated), fallback, target.0, deadline, &mut result);
            }
            assert_eq!(result.candidates.len(), if direct { 2 } else { 1 });
            assert!(result
                .candidates
                .iter()
                .all(|c| c.source == ProxySource::MacWpad));
            assert_eq!(
                result.candidates.iter().any(|c| c.proxy == Proxy::Direct),
                direct
            );
        }
        let mut result = ProxyDiscovery::default();
        unsafe {
            finish_wpad(
                None,
                ProxyDiscovery::one(Proxy::Direct, ProxySource::MacSystem).candidates,
                target.0,
                Instant::now(),
                &mut result,
            );
        }
        assert_eq!(result.candidates[0].proxy, Proxy::Direct);
    }

    #[test]
    fn looping_pac_returns_at_the_deadline() {
        let script = string("function FindProxyForURL(url, host) { while (true) {} }").unwrap();
        let target = url("https://relay.test/").unwrap();
        let start = Instant::now();
        unsafe {
            assert!(evaluate(
                script.0,
                true,
                target.0,
                start + std::time::Duration::from_millis(100)
            )
            .is_none());
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn framework_evaluates_pac_in_order() {
        let script = string("function FindProxyForURL(url, host) { return 'PROXY first.test:8080; PROXY second.test:3128; DIRECT'; }").unwrap();
        let target = url("https://relay.test/").unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let mut result = ProxyDiscovery::default();
        unsafe {
            let evaluated =
                evaluate(script.0, true, target.0, deadline).expect("CFNetwork PAC evaluation");
            collect(
                evaluated.0,
                target.0,
                ProxySource::MacPac,
                deadline,
                &mut result,
                1,
            );
        }
        let candidates: Vec<_> = result
            .candidates
            .iter()
            .map(|c| c.proxy.to_string())
            .collect();
        assert_eq!(
            candidates,
            [
                "http://first.test:8080",
                "http://second.test:3128",
                "direct"
            ]
        );
    }
    #[test]
    fn framework_downloads_and_evaluates_pac() {
        use std::io::{Read, Write};
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let pac = url(&format!(
            "http://{}/proxy.pac",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = [0u8; 4096];
                        let size = stream.read(&mut request).unwrap();
                        // New CFNetwork versions may attempt HTTPS first
                        // for an HTTP PAC URL, then retry in cleartext.
                        if request[..size].starts_with(b"GET ") {
                            break stream;
                        }
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("PAC listener failed: {error}"),
                }
                assert!(Instant::now() < deadline, "PAC request deadline");
            };
            let script = "function FindProxyForURL(url, host) { return 'PROXY downloaded.test:8080; DIRECT'; }";
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}",script.len()).unwrap();
        });
        let target = url("https://relay.test/").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut result = ProxyDiscovery::default();
        unsafe {
            let evaluated = evaluate(pac.0, false, target.0, deadline);
            server.join().unwrap();
            let evaluated = evaluated.expect("CFNetwork downloaded PAC evaluation");
            collect(
                evaluated.0,
                target.0,
                ProxySource::MacPac,
                deadline,
                &mut result,
                1,
            );
        }
        let candidates: Vec<_> = result
            .candidates
            .iter()
            .map(|c| c.proxy.to_string())
            .collect();
        assert_eq!(candidates, ["http://downloaded.test:8080", "direct"]);
    }
}
