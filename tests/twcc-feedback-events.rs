//! Transport-wide feedback (TWCC) as events, and the transport-wide sequence number of each RTP
//! packet handed out, with bandwidth estimation off: for an application that paces its own
//! sending and reads the remote's feedback itself.

use std::collections::{BTreeSet, HashSet};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, TwccPacket};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Receive, Transmit};
use str0m::{Event, Input, Output, Rtc, RtcConfig};

mod common;
use common::{Peer, TestRtc, init_crypto_default, init_log, negotiate};

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
    /// The transport-wide sequence number of every RTP packet the sender handed out, in order.
    sent: Vec<u64>,
    /// Sequence numbers the link loses on the way to the receiver.
    lose: BTreeSet<u64>,
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
            sent: Vec::new(),
            lose: BTreeSet::new(),
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
        loop {
            match self.l.poll_output().unwrap() {
                Output::Timeout(_) => {
                    assert!(self.l.last_sent_packet().is_none());
                    break;
                }
                Output::Transmit(transmit) => {
                    // Only an RTP packet has a sequence number; STUN and RTCP give None.
                    if let Some(packet) = self.l.last_sent_packet() {
                        let seq = packet.twcc_seq.expect("transport-cc is negotiated");
                        self.sent.push(seq);
                        if self.lose.remove(&seq) {
                            continue;
                        }
                    }
                    deliver(&mut self.r, self.now, &transmit);
                }
                Output::Event(event) => {
                    assert!(self.l.last_sent_packet().is_none());
                    self.l.events.push((self.now, event));
                }
            }
        }

        self.r.handle_input(Input::Timeout(self.now)).unwrap();
        loop {
            match self.r.poll_output().unwrap() {
                Output::Timeout(_) => break,
                Output::Transmit(transmit) => {
                    // The receiver sends no RTP, so nothing it hands out is a sent packet.
                    assert!(self.r.last_sent_packet().is_none());
                    deliver(&mut self.l, self.now, &transmit);
                }
                Output::Event(event) => self.r.events.push((self.now, event)),
            }
        }
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

    /// Every packet the sender's feedback events reported, in the order reported.
    fn reported(&self) -> Vec<TwccPacket> {
        self.l
            .events
            .iter()
            .filter_map(|(_, event)| match event {
                Event::TwccFeedback(feedback) => Some(feedback.packets.iter().copied()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn has_bwe_estimates(&self) -> bool {
        self.l
            .events
            .iter()
            .any(|(_, event)| matches!(event, Event::EgressBitrateEstimate(_)))
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
fn feedback_events_name_the_packets_sent_with_bwe_off() {
    let mut link = Link::connect(|config| config.enable_twcc_feedback_events(true));
    link.stream(Duration::from_secs(2));

    let sent = &link.sent;
    assert!(
        sent.len() > 150,
        "about 60 pictures of three packets: {}",
        sent.len()
    );
    assert!(
        sent.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "one sequence number per RTP packet, in order"
    );
    let sent: HashSet<u64> = sent.iter().copied().collect();

    let reported = link.reported();
    assert!(
        reported.len() * 10 >= sent.len() * 8,
        "most of them reported before the end: {} of {}",
        reported.len(),
        sent.len()
    );
    assert!(reported.windows(2).all(|pair| pair[1].seq > pair[0].seq));
    assert!(reported.iter().all(|p| sent.contains(&p.seq)));
    assert!(reported.iter().all(|p| p.size > 0 && p.received.is_some()));

    // The link adds no delay, so the packets arrive spaced as they were sent, to within TWCC's
    // 250 µs resolution. The two clocks have different origins; only the spacing is compared.
    let first = reported[0];
    for p in &reported {
        let sent_after = p.sent - first.sent;
        let arrived_after = p.received.unwrap() - first.received.unwrap();
        assert!(
            sent_after.abs_diff(arrived_after) <= Duration::from_millis(1),
            "packet {}: sent {sent_after:?} after the first, arrived {arrived_after:?} after it",
            p.seq
        );
    }

    assert!(!link.has_bwe_estimates(), "no bandwidth estimation ran");
}

#[test]
fn feedback_events_are_off_unless_enabled() {
    assert!(!Rtc::builder().twcc_feedback_events());

    let mut link = Link::connect(|config| config);
    link.stream(Duration::from_secs(1));

    assert!(
        !link.sent.is_empty(),
        "sequence numbers are given all the same"
    );
    assert!(link.reported().is_empty());
}

#[test]
fn a_lost_packet_is_reported_as_not_received() {
    let mut link = Link::connect(|config| config.enable_twcc_feedback_events(true));
    link.stream(Duration::from_millis(500));

    let lost = link.sent.last().unwrap() + 1;
    link.lose.insert(lost);
    link.stream(Duration::from_millis(500));
    assert!(link.lose.is_empty(), "the packet went out");

    let reports: Vec<_> = link
        .reported()
        .into_iter()
        .filter(|p| p.seq == lost)
        .map(|p| p.received)
        .collect();
    assert!(!reports.is_empty(), "the lost packet is reported");
    assert!(reports.iter().all(Option::is_none), "and never as received");
}

#[test]
fn feedback_events_arrive_with_bwe_on_too() {
    let mut link = Link::connect(|config| {
        config
            .enable_bwe(Some(Bitrate::kbps(1000)))
            .enable_twcc_feedback_events(true)
    });
    link.stream(Duration::from_secs(2));

    assert!(link.has_bwe_estimates());
    let reported = link.reported();
    assert!(!reported.is_empty());
    let sent: HashSet<u64> = link.sent.iter().copied().collect();
    assert!(reported.iter().all(|p| sent.contains(&p.seq)));
}
