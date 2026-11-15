use latch_proto::input::{Event, InputReceiver, InputSender};
use latch_proto::keyboard::{self, CASES};
use std::collections::BTreeSet;
use std::time::Instant;

#[test]
fn all_pairs_roundtrip_sided_keys_and_negotiated_positions_reliably() {
    let mut receiver = InputReceiver::new();
    let mut held = BTreeSet::new();
    let mut seq = 0;
    for case in CASES {
        let events = [true, false].map(|down| Event::Key {
            vk: case.vk,
            down,
            modifiers: if down { case.mask } else { 0 },
        });
        let mut bytes = Vec::new();
        for event in &events {
            bytes.clear();
            event.write(&mut bytes);
            assert_eq!(
                Event::read(&bytes),
                Some((event.clone(), 5)),
                "wire roundtrip"
            );
            assert_eq!(
                bytes[0],
                if keyboard::position(case.vk).is_some() {
                    8
                } else {
                    4
                }
            );
        }
        // Release arrives before the press. Retransmitted duplicates
        // are consumed once, so lock keys cannot toggle twice.
        assert_eq!(
            receiver.receive(seq + 1, [events[1].clone()], |_| panic!("gap")),
            seq
        );
        let mut count = 0;
        assert_eq!(
            receiver.receive(seq, events.clone(), |e| {
                count += 1;
                if let Event::Key { vk, down, .. } = e {
                    if down {
                        held.insert(vk);
                    } else {
                        held.remove(&vk);
                    }
                }
            }),
            seq + 2
        );
        receiver.receive(seq, events, |_| panic!("duplicate"));
        assert_eq!(count, 2);
        assert!(held.is_empty());
        seq += 2;
    }
}

#[test]
fn legacy_sender_rejects_new_keys_and_retains_original_keys() {
    for case in CASES {
        let mut sender = InputSender::new(1);
        sender.push(Event::Key {
            vk: case.vk,
            down: true,
            modifiers: 0,
        });
        let mut bytes = Vec::new();
        let sent = sender.poll(Instant::now(), &mut bytes);
        assert_eq!(sent, keyboard::legacy(case.vk).is_some());
        if sent {
            assert_eq!(bytes[13], 4);
        }
    }
    let mut modern = InputSender::new(1).with_key_positions(true);
    modern.push(Event::Key {
        vk: keyboard::KEYPAD_ENTER,
        down: true,
        modifiers: 0,
    });
    let mut bytes = Vec::new();
    assert!(modern.poll(Instant::now(), &mut bytes));
    assert_eq!(bytes[13], 8);
}

#[test]
fn unknown_positions_are_bounded_acknowledged_and_do_not_block_following_events() {
    use latch_proto::wire::{self, Datagram};
    let events = [
        Event::Key {
            vk: 0xa1,
            down: true,
            modifiers: 1,
        },
        Event::UnknownPosition {
            usage: 0xffff,
            down: true,
            modifiers: 0,
        },
        Event::UnknownPosition {
            usage: 0x92,
            down: false,
            modifiers: 0,
        },
        Event::Key {
            vk: 0xa1,
            down: false,
            modifiers: 0,
        },
        Event::ReleaseAll,
    ];
    let mut bytes = Vec::new();
    wire::write_input(&mut bytes, 1, 0, &events);
    let Datagram::Input {
        first,
        events: parsed,
        ..
    } = wire::parse(&bytes).unwrap()
    else {
        panic!()
    };
    let mut receiver = InputReceiver::new();
    let mut applied = Vec::new();
    assert_eq!(
        receiver.receive(first, parsed.iter(), |e| applied.push(e)),
        5
    );
    assert_eq!(
        applied,
        [events[0].clone(), events[3].clone(), Event::ReleaseAll]
    );
    assert_eq!(receiver.receive(0, events, |_| panic!("duplicate")), 5);
}

#[test]
fn offers_default_off_for_old_viewers_and_hosts_and_are_additive() {
    use latch_proto::control::{Accept, StreamRequest};
    let request = serde_json::json!({"protocol":1,"width":1920,"height":1080,"fps":60,"bitrate_kbps":20000,"codecs":["h264-high"],"udp_port":47880,"audio":false});
    let old: StreamRequest = serde_json::from_value(request.clone()).unwrap();
    assert!(!old.key_positions);
    assert!(serde_json::to_value(&old)
        .unwrap()
        .get("key_positions")
        .is_none());
    let offered = StreamRequest {
        key_positions: true,
        ..old
    };
    let json = serde_json::to_value(offered).unwrap();
    #[derive(serde::Deserialize)]
    struct OldRequest {
        protocol: u32,
    }
    assert_eq!(
        serde_json::from_value::<OldRequest>(json).unwrap().protocol,
        1
    );
    let json = serde_json::json!({"session":1,"token":"00","codec":"h264-high","width":1920,"height":1080,"fps":60,"bitrate_kbps":20000,"encoder":"test","audio":null});
    let old: Accept = serde_json::from_value(json).unwrap();
    assert!(!old.key_positions);
    assert!(serde_json::to_value(&old)
        .unwrap()
        .get("key_positions")
        .is_none());
    let accepted = Accept {
        key_positions: true,
        ..old
    };
    #[derive(serde::Deserialize)]
    struct OldAccept {
        session: u32,
    }
    assert_eq!(
        serde_json::from_value::<OldAccept>(serde_json::to_value(accepted).unwrap())
            .unwrap()
            .session,
        1
    );
}

#[test]
fn positions_are_validated_and_old_key_bytes_keep_their_meaning() {
    assert!(matches!(
        Event::read(&[8, 0xff, 0xff, 1, 0]),
        Some((Event::UnknownPosition { usage: 0xffff, .. }, 5))
    ));
    assert!(Event::read(&[8, 0x58, 0, 2, 0]).is_none());
    assert!(Event::read(&[8, 0x58, 0, 1]).is_none());
    let old = [4, 0xa5, 0, 1, 4];
    let (event, _) = Event::read(&old).unwrap();
    let mut bytes = Vec::new();
    event.write(&mut bytes);
    assert_eq!(bytes, old);
}
