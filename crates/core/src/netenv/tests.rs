use super::*;
use proxy::*;
use std::collections::BTreeMap;

fn addresses(result: &ProxyDiscovery) -> Vec<String> {
    result
        .candidates
        .iter()
        .map(|c| c.proxy.to_string())
        .collect()
}

#[test]
fn no_proxy_fixtures() {
    let cases: Vec<(String, String, u16, bool)> =
        serde_json::from_str(include_str!("fixtures/no_proxy.json")).unwrap();
    for (list, host, port, expected) in cases {
        assert_eq!(bypass(&list, &host, port, false), expected, "{list} {host}");
    }
    assert!(bypass("<local>", "pc", 443, true));
    assert!(bypass("proxy-*.internal", "proxy-1.internal", 443, true));
    assert!(!bypass("*.internal", "pc.internal.evil", 443, true));
}
#[test]
fn proxy_url_validation_and_redaction() {
    let value = Proxy::parse("http://user%40domain:p%3A%40ss@[2001:db8::1]:8080").unwrap();
    let Proxy::Http {
        host,
        port,
        credentials: Some(ref credentials),
    } = value
    else {
        panic!();
    };
    assert_eq!(host, "2001:db8::1");
    assert_eq!(port, 8080);
    assert_eq!(credentials.username(), "user@domain");
    assert_eq!(credentials.password(), "p:@ss");
    let output = format!("{credentials:?} {credentials}");
    assert!(!output.contains("user@domain"));
    assert!(!output.contains("p:@ss"));
    let options = Options {
        proxy_override: Some(Proxy::parse("http://user:secret@proxy.test:3128").unwrap()),
        ..Options::new("relay.test")
    };
    let result = proxy::discover(&options, Instant::now());
    assert_eq!(addresses(&result), ["http://[redacted]@proxy.test:3128"]);
    assert!(!format!("{options:?} {result:?}").contains("secret"));
    for value in [
        "",
        "http://",
        "https://proxy.test",
        "socks5://proxy.test",
        "proxy.test:0",
        "proxy.test:65536",
        "proxy.test:bad",
        "http://proxy.test/path",
        "http://proxy.test?token=secret",
        "http://proxy.test#secret",
    ] {
        assert!(Proxy::parse(value).is_err(), "{value}");
    }
    assert_eq!(Proxy::parse("DIRECT").unwrap(), Proxy::Direct);
}
#[test]
fn environment_precedence() {
    let mut env: BTreeMap<_, _> = [
        ("HTTPS_PROXY", "upper.test:8080"),
        ("https_proxy", "lower.test:8080"),
        ("ALL_PROXY", "all.test:3128"),
        ("NO_PROXY", ".internal.test"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    assert_eq!(
        addresses(&environment(&env, "relay.test").unwrap()),
        ["http://upper.test:8080"]
    );
    assert_eq!(
        addresses(&environment(&env, "a.internal.test").unwrap()),
        ["direct"]
    );
    env.remove("HTTPS_PROXY");
    assert_eq!(
        addresses(&environment(&env, "relay.test").unwrap()),
        ["http://lower.test:8080"]
    );
    env.remove("https_proxy");
    assert_eq!(
        addresses(&environment(&env, "relay.test").unwrap()),
        ["http://all.test:3128"]
    );
    env.insert("HTTPS_PROXY".into(), "socks://unsupported.test".into());
    let result = environment(&env, "relay.test").unwrap();
    assert!(result.candidates.is_empty());
    assert_eq!(result.warnings.len(), 1);
    assert!(environment(&BTreeMap::new(), "relay.test").is_none());
}
#[test]
fn pac_fixtures() {
    let cases: Vec<(String, Vec<String>, usize)> =
        serde_json::from_str(include_str!("fixtures/pac.json")).unwrap();
    for (value, expected, warnings) in cases {
        let result = pac_result(&value, ProxySource::MacPac);
        assert_eq!(addresses(&result), expected);
        assert_eq!(result.warnings.len(), warnings);
    }
}
#[test]
fn winhttp_fixtures() {
    #[derive(serde::Deserialize)]
    struct Case {
        proxy: String,
        bypass: String,
        host: String,
        expected: Vec<String>,
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/winhttp.json")).unwrap();
    for case in cases {
        assert_eq!(
            addresses(&winhttp(
                &case.proxy,
                &case.bypass,
                &case.host,
                ProxySource::WindowsUser
            )),
            case.expected
        );
    }
}
#[test]
fn scutil_fixtures() {
    let text = include_str!("fixtures/scutil.txt");
    assert_eq!(
        addresses(&scutil(text, "relay.test")),
        ["http://secure-proxy.test:3128"]
    );
    assert_eq!(addresses(&scutil(text, "pc.example.test")), ["direct"]);
    assert_eq!(addresses(&scutil(text, "printer")), ["direct"]);
    assert_eq!(
        addresses(&scutil("<dictionary> {\n}", "relay.test")),
        ["direct"]
    );
    assert!(!scutil(
        "ProxyAutoConfigEnable : 1\nProxyAutoConfigURLString : http://pac.test/proxy.pac",
        "relay.test"
    )
    .warnings
    .is_empty());
    assert!(scutil("HTTPSEnable : 1", "relay.test")
        .candidates
        .is_empty());
}
#[test]
fn linux_desktop_fixtures() {
    let text = include_str!("fixtures/gsettings.txt");
    let result = linux::gsettings(text, "relay.test").unwrap();
    assert_eq!(addresses(&result), ["http://secure-proxy.test:3128"]);
    assert!(matches!(
        &result.candidates[0].proxy,
        Proxy::Http {
            credentials: None,
            ..
        }
    ));
    let same_host = text.replace("secure-proxy.test", "plain-proxy.test");
    let result = linux::gsettings(&same_host, "relay.test").unwrap();
    assert!(matches!(
        &result.candidates[0].proxy,
        Proxy::Http {
            credentials: None,
            ..
        }
    ));
    let same_endpoint = same_host.replace("https port 3128", "https port 8080");
    let result = linux::gsettings(&same_endpoint, "relay.test").unwrap();
    let Proxy::Http {
        credentials: Some(credentials),
        ..
    } = &result.candidates[0].proxy
    else {
        panic!()
    };
    assert_eq!(credentials.username(), "alice");
    assert_eq!(credentials.password(), "p'ass");
    assert_eq!(
        addresses(&linux::gsettings(text, "a.internal.test").unwrap()),
        ["direct"]
    );
    assert_eq!(
        addresses(
            &linux::gsettings(
                &text.replace("use-same-proxy false", "use-same-proxy true"),
                "relay.test"
            )
            .unwrap()
        ),
        ["http://[redacted]@plain-proxy.test:8080"]
    );
    let auto =
        linux::gsettings(&text.replace("mode 'manual'", "mode 'auto'"), "relay.test").unwrap();
    assert!(auto.candidates.is_empty());
    assert!(auto.warnings[0] == NetworkWarning::LinuxPacUnavailable);
    assert!(linux::gsettings("garbage", "relay.test").is_none());
    let kde = include_str!("fixtures/kioslaverc");
    assert_eq!(
        addresses(&linux::kioslaverc(kde, "relay.test", &BTreeMap::new()).unwrap()),
        ["http://kde-proxy.test:8080"]
    );
    assert_eq!(
        addresses(&linux::kioslaverc(kde, "pc.internal.test", &BTreeMap::new()).unwrap()),
        ["direct"]
    );
    assert_eq!(
        addresses(
            &linux::kioslaverc(
                &kde.replace("ReversedException=false", "ReversedException=true"),
                "relay.test",
                &BTreeMap::new()
            )
            .unwrap()
        ),
        ["direct"]
    );
    for mode in ["2", "3"] {
        assert!(linux::kioslaverc(
            &kde.replace("ProxyType=1", &format!("ProxyType={mode}")),
            "relay.test",
            &BTreeMap::new()
        )
        .unwrap()
        .candidates
        .is_empty());
    }
}
#[test]
fn status_and_netcheck_fixtures() {
    use TailscaleReason as R;
    let status = include_str!("fixtures/status.json");
    let netcheck = include_str!("fixtures/netcheck.json");
    for (peer, reason) in [
        (None, R::Connected),
        (Some("direct"), R::Direct),
        (Some("derp"), R::DerpOnly),
        (Some("100.64.0.10"), R::DerpOnly),
        (Some("relay"), R::PeerRelay),
        (Some("offline"), R::PeerOffline),
        (Some("idle"), R::PathUnknown),
        (Some("missing"), R::PeerUnknown),
    ] {
        let result = tailscale::diagnose(status, Some(netcheck), peer);
        assert_eq!(result.reason, reason);
        if peer.is_some() && reason != R::DerpOnly {
            assert_eq!(result.derp_region, None, "{peer:?}");
        }
        assert!(!result.netcheck.unwrap().udp);
        assert!(!result.message.is_empty());
        if reason == R::PeerRelay {
            assert!(result
                .message
                .contains("one of your own machines acting as a relay"));
            assert!(!result.message.contains("on your network"));
        }
        assert!(!reason.code().is_empty());
    }
    assert_eq!(
        tailscale::diagnose(status, None, Some("derp"))
            .derp_region
            .as_deref(),
        Some("tok")
    );
    assert!(tailscale::diagnose(status, None, Some("relay"))
        .peer_relay
        .is_some());
    for (state, reason) in [
        ("NeedsLogin", R::LoggedOut),
        ("NeedsMachineAuth", R::NeedsApproval),
        ("InUseOtherUser", R::OtherAccount),
        ("Stopped", R::Stopped),
        ("NoState", R::DaemonNotRunning),
        ("Starting", R::Unknown),
    ] {
        assert_eq!(
            tailscale::diagnose(
                &status.replace("\"Running\"", &format!("\"{state}\"")),
                None,
                None
            )
            .reason,
            reason
        );
    }
    let control = status.replace(
        "\"Health\":null",
        "\"Health\":[\"Unable to connect to the Tailscale coordination server\"]",
    );
    assert_eq!(
        tailscale::diagnose(&control, None, None).reason,
        R::ControlUnreachable
    );
    let derp = status.replace(
        "\"Health\":null",
        "\"Health\":[\"Tailscale could not connect to any relay server\"]",
    );
    assert_eq!(
        tailscale::diagnose(&derp, None, None).reason,
        R::DerpUnreachable
    );
    assert_eq!(
        tailscale::diagnose("garbage", None, None).reason,
        R::Unknown
    );
    assert_eq!(
        tailscale::diagnose(
            "{\"BackendState\":\"NeedsLogin\",\"Self\":null,\"Peer\":null}",
            None,
            None
        )
        .reason,
        R::LoggedOut
    );
    // A home region without traffic never proves an active DERP path.
    assert_eq!(
        tailscale::diagnose(status, None, Some("idle")).reason,
        R::PathUnknown
    );
}
#[test]
fn nat64_rfc7050_all_prefix_lengths() {
    use std::net::Ipv6Addr;
    for length in [32u8, 40, 48, 56, 64, 96] {
        let mut prefix = [0u8; 16];
        prefix[0..4].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8]);
        let synth = |last| {
            let mut bytes = prefix;
            for (index, value) in (usize::from(length) / 8..16)
                .filter(|i| length == 96 || *i != 8)
                .take(4)
                .zip([192, 0, 0, last])
            {
                bytes[index] = value;
            }
            Ipv6Addr::from(bytes)
        };
        for last in [170, 171] {
            assert_eq!(
                ip::nat64(&[synth(last)]),
                [Nat64Prefix {
                    address: prefix.into(),
                    length
                }]
            );
        }
        assert_eq!(
            ip::nat64(&[synth(170), synth(171), synth(171)]),
            [Nat64Prefix {
                address: prefix.into(),
                length
            }]
        );
    }
    let addresses = [
        "64:ff9b::c000:aa".parse().unwrap(),
        "64:ff9b::c000:ab".parse().unwrap(),
    ];
    assert_eq!(ip::nat64(&addresses)[0].length, 96);
    assert!(ip::nat64(&["2001:db8::1".parse().unwrap()]).is_empty());
}
#[test]
fn portal_http_fixtures() {
    use portal::classify;
    let target = ProbeTarget::new("http://probe.test/check", 204, &[]).unwrap();
    assert_eq!(
        classify(&target, b"HTTP/1.1 204 No Content\r\n\r\n"),
        CaptivePortal::OpenInternet
    );
    let portal = classify(&target,b"HTTP/1.1 302 Found\r\nLocation: https://user:secret@portal.test/login?token=private#id\r\n\r\n");
    let CaptivePortal::Captive {
        portal_url: Some(ref url),
    } = portal
    else {
        panic!();
    };
    assert_eq!(url.as_str(), "https://portal.test/login?token=private#id");
    assert!(!format!("{portal:?} {url}").contains("private"));
    assert!(!format!("{portal:?}").contains("secret"));
    assert!(!format!("{portal:?} {url}").contains("/login"));
    assert_eq!(format!("{url}"), "https://portal.test/");
    let portal = classify(&target, b"HTTP/1.1 302 Found\r\nLocation: /signin\r\n\r\n");
    let CaptivePortal::Captive {
        portal_url: Some(url),
    } = portal
    else {
        panic!();
    };
    assert_eq!(url.as_str(), "http://probe.test/signin");
    assert_eq!(
        classify(&target, b"HTTP/1.1 511 Sign in\r\n\r\n"),
        CaptivePortal::Captive { portal_url: None }
    );
    assert_eq!(
        classify(
            &target,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>sign in</html>"
        ),
        CaptivePortal::Captive { portal_url: None }
    );
    for response in [
        b"garbage".as_slice(),
        b"HTTP/1.1 407 Proxy auth\r\n\r\n",
        b"HTTP/1.1 403 Forbidden\r\n\r\n",
    ] {
        assert_eq!(classify(&target, response), CaptivePortal::Restricted);
    }
    let target = ProbeTarget::new("http://probe.test/check", 200, b"latch-ok").unwrap();
    assert_eq!(
        classify(&target, b"HTTP/1.1 200 OK\r\n\r\nlatch-ok"),
        CaptivePortal::OpenInternet
    );
    for url in [
        "https://probe.test",
        "http://user:secret@probe.test",
        "http://probe.test?token=secret",
        "invalid",
    ] {
        assert!(ProbeTarget::new(url, 204, &[]).is_err());
    }
}
#[test]
fn cache_never_returns_another_network_or_options() {
    let options = Options::new("relay.test");
    let (tx, rx) = mpsc::channel();
    let mut cache = NetworkEnvironment {
        cached: None,
        pending: Some((1, options.clone(), rx)),
        queued: None,
    };
    assert!(cache.get(1, &options).is_none());
    tx.send(NetworkDiagnosis::default()).unwrap();
    drop(tx);
    cache.poll();
    assert!(cache.get(1, &options).is_some());
    assert!(cache.get(2, &options).is_none());
    assert!(cache.get(1, &Options::new("another.test")).is_none());
    assert_eq!(cache.request(1, options), RequestStatus::Cached);
    assert!(cache.pending.is_none());
}
#[test]
fn malformed_inputs_never_panic() {
    for text in [
        "\0",
        "☃",
        "'",
        "[",
        "http://user:pass@",
        "PROXY",
        "\n\n\n",
        "[Proxy Settings]\nProxyType=x",
    ] {
        let _ = Proxy::parse(text);
        let _ = scutil(text, "relay.test");
        let _ = linux::gsettings(text, "relay.test");
        let _ = linux::kioslaverc(text, "relay.test", &BTreeMap::new());
        let _ = winhttp(text, text, "relay.test", ProxySource::WindowsUser);
        let _ = pac_result(text, ProxySource::MacPac);
        let _ = tailscale::diagnose(text, Some(text), Some("peer"));
    }
}

#[test]
fn interface_family_detection_keeps_ula_and_carrier_nat() {
    for (addresses, expected) in [
        (
            vec!["127.0.0.1", "::1", "fe80::1", "169.254.1.1"],
            (false, false),
        ),
        (vec!["100.100.100.1"], (true, false)),
        (vec!["fd00::1"], (false, true)),
        (vec!["192.168.1.2", "2001:db8::1"], (true, true)),
    ] {
        let ips: Vec<_> = addresses
            .iter()
            .map(|value| value.parse().unwrap())
            .collect();
        assert_eq!(ip::families(&ips), expected);
    }
}

#[test]
fn portal_probe_accepts_chunked_and_bounds_a_stalled_server() {
    use std::io::Write;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = crate::http::read_request(&mut stream).unwrap().unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/probe");
        for forbidden in ["cookie", "authorization", "user-agent", "referer"] {
            assert!(request.header(forbidden).is_none());
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n8\r\nlatch-ok\r\n0\r\n\r\n",
            )
            .unwrap();
    });
    let target = ProbeTarget::new(&format!("http://{addr}/probe"), 200, b"latch-ok").unwrap();
    assert_eq!(
        portal::probe(&target, Instant::now() + Duration::from_secs(2)),
        Some(CaptivePortal::OpenInternet)
    );
    server.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = ProbeTarget::new(
        &format!("http://{}/probe", listener.local_addr().unwrap()),
        204,
        &[],
    )
    .unwrap();
    let start = Instant::now();
    // TCP completes through the listening backlog, with no HTTP response.
    assert!(portal::probe(&target, start + Duration::from_millis(80)).is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn aggregate_returns_partial_results_at_the_deadline() {
    let (tx, rx) = mpsc::channel();
    tx.send(Observation::Proxy(ProxyDiscovery::one(
        Proxy::Direct,
        ProxySource::Override,
    )))
    .unwrap();
    // A live sender models a resolver or WPAD call that has not returned.
    let start = Instant::now();
    let result = collect(rx, 4, start + Duration::from_millis(30));
    assert_eq!(addresses(&result.proxies), ["direct"]);
    assert_eq!(result.captive_portal, CaptivePortal::Unknown);
    assert_eq!(result.tailscale.reason, TailscaleReason::Unknown);
    assert_eq!(result.ip.ipv4, None);
    assert_eq!(result.warnings.len(), 1);
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(tx);
}

#[test]
fn stalled_checks_coalesce_without_starving_other_kinds() {
    let slots: [Arc<JobSlot>; 4] = std::array::from_fn(|_| Arc::new(JobSlot::default()));
    let options = Options::new("relay.test");
    let (release, stalled) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + Duration::from_secs(2);
    assert!(slots[0].subscribe(
        CheckKind::Proxy,
        &options,
        tx.clone(),
        deadline,
        move || {
            stalled.recv().unwrap();
            Observation::Proxy(ProxyDiscovery::one(Proxy::Direct, ProxySource::MacSystem))
        }
    ));
    for _ in 0..30 {
        assert!(
            slots[0].subscribe(CheckKind::Proxy, &options, tx.clone(), deadline, || panic!(
                "coalesced requests must never start another OS job"
            ))
        );
    }
    assert!(!slots[0].subscribe(
        CheckKind::Proxy,
        &Options::new("different.test"),
        tx.clone(),
        deadline,
        || panic!("different inputs must remain pending")
    ));
    assert!(
        slots[2].subscribe(CheckKind::Tailscale, &options, tx, deadline, || {
            Observation::Tailscale(TailscaleDiagnosis::unknown())
        })
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Observation::Tailscale(_)
    ));
    assert!(rx.try_recv().is_err());
    release.send(()).unwrap();
    for _ in 0..31 {
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Observation::Proxy(_)
        ));
    }
    assert!(slots[0].0.lock().unwrap().is_none());
}

#[test]
fn nat64_rejects_ambiguous_constants_and_preserves_dns_order() {
    #[derive(serde::Deserialize)]
    struct Case {
        addresses: Vec<std::net::Ipv6Addr>,
        expected: Vec<(std::net::Ipv6Addr, u8)>,
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/nat64.json")).unwrap();
    for case in cases {
        let expected: Vec<_> = case
            .expected
            .into_iter()
            .map(|(address, length)| Nat64Prefix { address, length })
            .collect();
        assert_eq!(ip::nat64(&case.addresses), expected);
    }
}

#[test]
fn https_destination_accepts_ip_literals_and_rejects_url_components() {
    for host in ["relay.test", "192.0.2.1", "2001:db8::1", "[2001:db8::1]"] {
        let url = destination_url(host).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.port_or_known_default(), Some(443));
    }
    for host in [
        "",
        "user@relay.test",
        "relay.test/path",
        "relay.test?token=x",
        "relay.test#id",
    ] {
        assert!(destination_url(host).is_none());
    }
}

#[test]
fn kde_environment_mode_resolves_the_bypass_variable() {
    let env = BTreeMap::from([
        ("PROXY".into(), "http://alice:secret@kde.test:8080".into()),
        ("relay.test".into(), "bypass.test".into()),
    ]);
    let config = "[Proxy Settings]\nProxyType=4\nhttpsProxy=PROXY\nNoProxyFor=relay.test";
    let result = linux::kioslaverc(config, "relay.test", &env).unwrap();
    assert_eq!(addresses(&result), ["http://[redacted]@kde.test:8080"]);
    assert_eq!(
        addresses(&linux::kioslaverc(config, "bypass.test", &env).unwrap()),
        ["direct"]
    );
    // Credentials remain tied to their own KDE/environment URL.
    let Proxy::Http {
        host,
        port,
        credentials: Some(auth),
    } = &result.candidates[0].proxy
    else {
        panic!()
    };
    assert_eq!(
        (host.as_str(), *port, auth.password()),
        ("kde.test", 8080, "secret")
    );
}

#[test]
fn socks_fallback_is_diagnosed_and_https_credentials_stay_local() {
    let result = winhttp(
        "http=plain.test:80;socks=socks.test:1080",
        "",
        "relay.test",
        ProxySource::WindowsUser,
    );
    assert!(result.candidates.is_empty());
    assert_eq!(result.warnings, [NetworkWarning::UnsupportedPacProtocol]);
    let result = winhttp(
        "http=http://alice:secret@plain.test:80;https=secure.test:443",
        "",
        "relay.test",
        ProxySource::WindowsUser,
    );
    assert_eq!(addresses(&result), ["http://secure.test:443"]);
    let env = BTreeMap::from([
        ("HTTPS_PROXY".into(), "secure.test:443".into()),
        (
            "ALL_PROXY".into(),
            "http://alice:secret@other.test:80".into(),
        ),
    ]);
    assert_eq!(
        addresses(&environment(&env, "relay.test").unwrap()),
        ["http://secure.test:443"]
    );
}

#[test]
fn scutil_ignores_scoped_dictionaries() {
    let text = "<dictionary> {\nHTTPSEnable : 1\nHTTPSProxy : global.test\nHTTPSPort : 8080\n__SCOPED__ : <dictionary> {\nen0 : <dictionary> {\nHTTPSEnable : 1\nHTTPSProxy : scoped.test\nHTTPSPort : 3128\nExceptionsList : <array> {\n0 : relay.test\n}\n}\n}\n}";
    assert_eq!(
        addresses(&scutil(text, "relay.test")),
        ["http://global.test:8080"]
    );
}

#[test]
fn clat_with_dns64_is_ipv6_only() {
    let ips = ["192.0.0.2".parse().unwrap(), "2001:db8::1".parse().unwrap()];
    let prefixes = [Nat64Prefix {
        address: "64:ff9b::".parse().unwrap(),
        length: 96,
    }];
    assert!(!ip::ipv6_only(&ips, &[]));
    assert!(ip::ipv6_only(&ips, &prefixes));
    assert!(!ip::ipv6_only(
        &[ips[0], ips[1], "192.168.1.2".parse().unwrap()],
        &prefixes
    ));
}

#[test]
fn configured_probe_overrides_a_lagging_native_portal_hint() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let target = ProbeTarget::new(
        &format!("http://{}/probe", listener.local_addr().unwrap()),
        204,
        &[],
    )
    .unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        crate::http::read_request(&mut stream).unwrap().unwrap();
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .unwrap();
    });
    assert_eq!(
        portal::detect_with_native(&[target], Instant::now() + Duration::from_secs(2), || {
            CaptivePortal::Captive { portal_url: None }
        }),
        CaptivePortal::OpenInternet
    );
    server.join().unwrap();
}

#[test]
fn cache_restarts_the_latest_generation_from_poll() {
    let mut options = Options::new("relay.test");
    options.timeout = Duration::from_millis(100);
    let (tx, rx) = mpsc::channel();
    let mut cache = NetworkEnvironment {
        pending: Some((1, options.clone(), rx)),
        ..Default::default()
    };
    assert_eq!(cache.request(2, options.clone()), RequestStatus::Pending);
    assert_eq!(cache.request(3, options.clone()), RequestStatus::Pending);
    tx.send(NetworkDiagnosis::default()).unwrap();
    drop(tx);
    cache.poll();
    assert_eq!(cache.pending.as_ref().unwrap().0, 3);
    let deadline = Instant::now() + Duration::from_secs(2);
    while cache.get(3, &options).is_none() && Instant::now() < deadline {
        cache.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(cache.get(3, &options).is_some());
    assert!(cache.get(2, &options).is_none());
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn real_system_proxy_smoke() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        tx.send(proxy::discover(&Options::new("relay.test"), deadline))
            .unwrap();
    });
    let result = rx
        .recv_timeout(Duration::from_millis(3500))
        .expect("OS proxy API deadline");
    for candidate in result.candidates {
        assert_ne!(candidate.source, ProxySource::Override);
        if let Proxy::Http { host, port, .. } = candidate.proxy {
            assert!(!host.is_empty());
            assert_ne!(port, 0);
            assert!(destination_url(&host).is_some());
        }
    }
    for warning in result.warnings {
        assert!(!warning.message().contains('@'));
        assert!(!warning.code().is_empty());
    }
}

#[cfg(target_os = "windows")]
#[test]
fn real_windows_portal_smoke() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(windows::portal()).unwrap();
    });
    let _ = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("WinRT connectivity API deadline");
}

#[test]
fn warning_codes_are_stable_and_messages_are_credential_free() {
    use NetworkWarning as W;
    let warnings = [
        (W::ChecksPending, "checks_pending"),
        (W::ChecksTimedOut, "checks_timed_out"),
        (W::InvalidProxy, "invalid_proxy"),
        (W::InvalidDestination, "invalid_destination"),
        (W::UnsupportedPacProtocol, "unsupported_pac_protocol"),
        (W::MalformedPacCandidate, "malformed_pac_candidate"),
        (W::PacEvaluatorUnavailable, "pac_evaluator_unavailable"),
        (W::IncompleteSystemProxy, "incomplete_system_proxy"),
        (W::LinuxPacUnavailable, "linux_pac_unavailable"),
        (W::PacFailed, "pac_failed"),
        (W::UnsupportedSystemProtocol, "unsupported_system_protocol"),
        (W::MacStaticFallback, "mac_static_fallback"),
        (W::MacSettingsUnavailable, "mac_settings_unavailable"),
        (W::WpadFailed, "wpad_failed"),
        (W::KeychainCredentialsUnread, "keychain_credentials_unread"),
        (W::WindowsServiceSettings, "windows_service_settings"),
    ];
    for (warning, code) in warnings {
        assert_eq!(warning.code(), code);
        assert!(!warning.message().is_empty());
        assert!(!warning.message().contains('@'));
    }
    assert_eq!(
        W::KeychainCredentialsUnread.severity(),
        WarningSeverity::Informational
    );
    assert_eq!(W::ChecksTimedOut.severity(), WarningSeverity::Actionable);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn desktop_commands_prefer_the_system_executable() {
    // Sandboxed builders such as Nix have no /usr/bin; the bare name is then
    // the documented fallback.
    let system = std::path::Path::new("/usr/bin/true");
    if system.is_file() {
        assert_eq!(command_program("true"), system);
    } else {
        assert_eq!(command_program("true"), std::path::Path::new("true"));
    }
    let missing = "latch-netenv-missing-executable";
    assert_eq!(command_program(missing), std::path::Path::new(missing));
    assert_eq!(
        command_program("/usr/sbin/scutil"),
        std::path::Path::new("/usr/sbin/scutil")
    );
}

#[test]
fn partial_proxy_result_is_usable_while_portal_check_continues() {
    let options = Options::new("relay.test");
    let (tx, rx) = mpsc::channel();
    let mut cache = NetworkEnvironment {
        pending: Some((1, options.clone(), rx)),
        ..Default::default()
    };
    let partial = NetworkDiagnosis {
        proxies: proxy::pac_result("PROXY proxy.test:3128", ProxySource::MacPac),
        ..Default::default()
    };
    tx.send(partial).unwrap();
    cache.poll();
    assert_eq!(cache.get(1, &options).unwrap().proxies.candidates.len(), 1);
    assert!(cache.pending.is_some());
    drop(tx);
    cache.poll();
    assert!(cache.pending.is_none());
}

#[test]
fn empty_proxy_observation_is_distinct_from_pending_discovery() {
    let (tx, rx) = mpsc::channel();
    tx.send(Observation::Proxy(ProxyDiscovery::default()))
        .unwrap();
    drop(tx);
    let result = collect(rx, 1, Instant::now() + Duration::from_secs(1));
    assert!(result.proxy_observed);
    assert!(result.proxies.candidates.is_empty());
    assert!(!NetworkDiagnosis::default().proxy_observed);
}
