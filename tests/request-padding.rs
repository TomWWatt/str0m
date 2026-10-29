//! Padding on request, with bandwidth estimation off: for an application that probes a path
//! itself.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Receive, Transmit};
use str0m::rtp::rtcp::Rtcp;
use str0m::rtp::{RawPacket, RtpWrite, Ssrc};
use str0m::{Input, Output, RtcConfig};

mod common;
use common::{Peer, TestRtc, connect_l_r, init_crypto_default, init_log, negotiate, progress};

const TICK: Duration = Duration::from_millis(1);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// A 3 kB picture: three packets.
const FRAME_SIZE: usize = 3000;

/// A video sender and its receiver on a direct link, driven a millisecond at a time.
struct Link {
    l: TestRtc,
    r: TestRtc,
    mid: Mid,
    now: Instant,
    frames: u64,
}

impl Link {
    fn connect(configure: impl FnOnce(RtcConfig) -> RtcConfig) -> Self {
        init_log();
        init_crypto_default();

        let mut l = TestRtc::new_with_config(Peer::Left, configure);
        let mut r = TestRtc::new_with_config(Peer::Right, |config| config);
        l.add_host_candidate((Ipv4Addr::new(1, 1, 1, 1), 1000).into());
        r.add_host_candidate((Ipv4Addr::new(2, 2, 2, 2), 2000).into());

        let mid = negotiate(&mut l, &mut r, |change| {
            change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
        });

        let now = l.last.max(r.last);
        let mut link = Link {
            l,
            r,
            mid,
            now,
            frames: 0,
        };
        while !(link.l.is_connected() && link.r.is_connected()) {
            assert!(
                link.now - now < Duration::from_secs(5),
                "the peers did not connect"
            );
            link.tick();
        }
        link
    }

    /// Advances both peers one millisecond, handing every datagram straight to the other side.
    fn tick(&mut self) {
        self.now += TICK;
        self.l.last = self.now;
        self.r.last = self.now;
        self.l.handle_input(Input::Timeout(self.now)).unwrap();
        self.poll_sender();
        self.r.handle_input(Input::Timeout(self.now)).unwrap();
        poll(&mut self.r, &mut self.l, self.now);
    }

    /// Polls the sender until it asks for a timeout, delivering what it sends, and returns
    /// the timeout it asked for.
    fn poll_sender(&mut self) -> Instant {
        poll(&mut self.l, &mut self.r, self.now)
    }

    fn write_frame(&mut self) {
        let pt = self.l.params_vp8().pt();
        let time = MediaTime::new(self.frames * 3000, Frequency::NINETY_KHZ);
        let data = vec![0u8; FRAME_SIZE];
        self.l
            .writer(self.mid)
            .unwrap()
            .write(pt, self.now, time, data)
            .unwrap();
        self.frames += 1;
    }

    /// Sends a picture every 33 ms for `span`.
    fn stream(&mut self, span: Duration) {
        let end = self.now + span;
        let mut next = self.now;
        while self.now < end {
            if self.now >= next {
                self.write_frame();
                next += FRAME_INTERVAL;
            }
            self.tick();
        }
    }

    fn idle(&mut self, span: Duration) {
        let end = self.now + span;
        while self.now < end {
            self.tick();
        }
    }

    /// The transport-wide sequence numbers of the packets sent on the RTX stream after `since`.
    fn rtx_sent_after(&self, since: Instant) -> Vec<u16> {
        let rtx = self.l.params_vp8().resend().expect("RTX negotiated");
        self.l
            .events
            .iter()
            .filter(|(at, _)| *at > since)
            .filter_map(|(_, event)| match event.as_raw_packet() {
                Some(RawPacket::RtpTx(header, _)) if header.payload_type == rtx => {
                    header.ext_vals.transport_cc
                }
                _ => None,
            })
            .collect()
    }

    /// The packets the remote's feedback reported as received, by transport-wide sequence
    /// number.
    fn reported_received(&self) -> HashSet<u16> {
        self.l
            .events
            .iter()
            .filter_map(|(_, event)| match event.as_raw_packet() {
                Some(RawPacket::RtcpRx(Rtcp::Twcc(twcc))) => {
                    Some(twcc.clone().into_iter(self.l.start, 0.into()))
                }
                _ => None,
            })
            .flatten()
            .filter(|(_, _, received)| received.is_some())
            .map(|(seq, _, _)| *seq as u16)
            .collect()
    }
}

/// Polls `from` until it asks for a timeout, handing each datagram to `to` and keeping the
/// events; returns the timeout asked for.
fn poll(from: &mut TestRtc, to: &mut TestRtc, now: Instant) -> Instant {
    loop {
        match from.poll_output().unwrap() {
            Output::Timeout(at) => return at,
            Output::Transmit(transmit) => deliver(to, now, &transmit),
            Output::Event(event) => from.events.push((now, event)),
        }
    }
}

fn deliver(to: &mut TestRtc, now: Instant, transmit: &Transmit) {
    let data = transmit.contents.to_vec();
    let receive = Receive {
        proto: transmit.proto,
        source: transmit.source,
        destination: transmit.destination,
        contents: (&data[..]).try_into().unwrap(),
    };
    to.handle_input(Input::Receive(now, receive)).unwrap();
}

#[test]
fn padding_goes_out_when_asked_for_and_is_reported_like_media() {
    let mut link = Link::connect(|config| config.enable_raw_packets(true));
    link.stream(Duration::from_millis(500));
    let quiet = link.now;
    link.idle(Duration::from_millis(300));
    assert!(
        link.rtx_sent_after(quiet).is_empty(),
        "nothing goes on the RTX stream unasked"
    );

    // Nothing to send: the next timeout is some way off.
    let at = link.poll_sender();
    assert!(at > link.now);

    let mid = link.mid;
    let requested = link
        .l
        .direct_api()
        .stream_tx_by_mid(mid, None)
        .expect("the video stream")
        .request_padding(20_000);
    assert!(requested);
    let at = link.poll_sender();
    assert!(at <= link.now, "the request asks for a timeout at once");

    let since = link.now;
    link.tick();
    let padding = link.rtx_sent_after(since);
    assert!(
        padding.len() >= 10,
        "the padding went out in one go, unpaced: {} packets",
        padding.len()
    );

    link.idle(Duration::from_millis(300));
    let reported = link.reported_received();
    assert!(
        padding.iter().all(|seq| reported.contains(seq)),
        "the padding is reported like media"
    );
}

#[test]
fn a_stream_cannot_pad_without_rtx_or_before_it_knows_a_payload_type() {
    init_log();
    init_crypto_default();

    let (mut l, mut r) = connect_l_r();
    let plain: Ssrc = 42.into();
    let repaired: Ssrc = 43.into();
    l.direct_api().declare_media("a".into(), MediaKind::Video);
    l.direct_api()
        .declare_stream_tx(plain, None, "a".into(), None);
    l.direct_api().declare_media("b".into(), MediaKind::Video);
    l.direct_api()
        .declare_stream_tx(repaired, Some(44.into()), "b".into(), None);
    let pt = l.params_vp8().pt();

    let request = |l: &mut TestRtc, ssrc: &Ssrc| {
        l.direct_api()
            .stream_tx(ssrc)
            .unwrap()
            .request_padding(1000)
    };
    assert!(!request(&mut l, &plain), "no RTX stream to pad on");
    assert!(
        !request(&mut l, &repaired),
        "no payload type to pad with yet"
    );

    // Nothing negotiated the payload types here: the first regular packet names one.
    for ssrc in [plain, repaired] {
        let now = l.last;
        l.direct_api()
            .stream_tx(&ssrc)
            .unwrap()
            .write_rtp(RtpWrite::new(pt, 47_000.into(), 3000, now, [1, 2, 3, 4]).nackable(true));
    }
    for _ in 0..10 {
        progress(&mut l, &mut r).unwrap();
    }
    assert!(request(&mut l, &repaired));
    assert!(!request(&mut l, &plain), "still no RTX stream to pad on");
}
