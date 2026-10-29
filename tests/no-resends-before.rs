//! A frame after which the packets before it are not resent: for the keyframe a sender
//! switches a stream down to when the path has no room, where resending what that keyframe
//! replaces would spend the room the switch was made to find.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Receive, Transmit};
use str0m::{Input, Output};

mod common;
use common::{Peer, TestRtc, init_crypto_default, init_log, negotiate};

const TICK: Duration = Duration::from_millis(1);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// A 3 kB picture: three packets.
const FRAME_SIZE: usize = 3000;

/// A video sender and its receiver on a direct link that loses packets to order, driven a
/// millisecond at a time.
struct Link {
    l: TestRtc,
    r: TestRtc,
    mid: Mid,
    now: Instant,
    frames: u64,
    /// The video payload type and the one its resends carry.
    pts: (Pt, Pt),
    /// How many of the next video packets from the sender are lost on the way.
    lose: usize,
    /// Resends the sender has sent.
    resent: usize,
}

impl Link {
    fn connect() -> Self {
        init_log();
        init_crypto_default();

        let mut l = TestRtc::new(Peer::Left);
        let mut r = TestRtc::new(Peer::Right);
        l.add_host_candidate((Ipv4Addr::new(1, 1, 1, 1), 1000).into());
        r.add_host_candidate((Ipv4Addr::new(2, 2, 2, 2), 2000).into());

        let mid = negotiate(&mut l, &mut r, |change| {
            change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
        });

        let params = l.params_vp8();
        let pts = (params.pt(), params.resend().expect("RTX negotiated"));
        let now = l.last.max(r.last);
        let mut link = Link {
            l,
            r,
            mid,
            now,
            frames: 0,
            pts,
            lose: 0,
            resent: 0,
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

    /// Advances both peers one millisecond, handing every datagram straight to the other side
    /// except the video packets ordered lost.
    fn tick(&mut self) {
        self.now += TICK;
        self.l.last = self.now;
        self.r.last = self.now;

        self.l.handle_input(Input::Timeout(self.now)).unwrap();
        loop {
            match self.l.poll_output().unwrap() {
                Output::Timeout(_) => break,
                Output::Transmit(transmit) => {
                    match rtp_payload_type(&transmit.contents) {
                        Some(pt) if pt == self.pts.1 => self.resent += 1,
                        Some(pt) if pt == self.pts.0 && self.lose > 0 => {
                            self.lose -= 1;
                            continue;
                        }
                        _ => {}
                    }
                    deliver(&mut self.r, self.now, &transmit);
                }
                Output::Event(event) => self.l.events.push((self.now, event)),
            }
        }

        self.r.handle_input(Input::Timeout(self.now)).unwrap();
        loop {
            match self.r.poll_output().unwrap() {
                Output::Timeout(_) => break,
                Output::Transmit(transmit) => deliver(&mut self.l, self.now, &transmit),
                Output::Event(event) => self.r.events.push((self.now, event)),
            }
        }
    }

    /// A picture; with `no_resends_before`, the packets written before it are not resent.
    fn write_frame(&mut self, no_resends_before: bool) {
        let time = MediaTime::new(self.frames * 3000, Frequency::NINETY_KHZ);
        let data = vec![0u8; FRAME_SIZE];
        self.l
            .writer(self.mid)
            .unwrap()
            .no_resends_before(no_resends_before)
            .write(self.pts.0, self.now, time, data)
            .unwrap();
        self.frames += 1;
    }

    /// Sends a picture every 33 ms for `span`.
    fn stream(&mut self, span: Duration) {
        let end = self.now + span;
        let mut next = self.now;
        while self.now < end {
            if self.now >= next {
                self.write_frame(false);
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

    /// Resends sent while the link streams pictures for `span` from now.
    fn resent_over(&mut self, span: Duration) -> usize {
        let before = self.resent;
        self.stream(span);
        self.resent - before
    }
}

/// The payload type of an RTP datagram, whose header SRTP leaves in the clear. None for RTCP,
/// STUN and DTLS.
fn rtp_payload_type(datagram: &[u8]) -> Option<Pt> {
    let rtp = datagram.len() >= 12 && datagram[0] >> 6 == 2 && !(192..=223).contains(&datagram[1]);
    rtp.then(|| (datagram[1] & 0x7f).into())
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
fn the_packets_before_the_frame_are_not_resent() {
    // A picture loses a packet on the way, and the next picture is written before the
    // receiver's NACK can be back.
    let run = |no_resends_before: bool| {
        let mut link = Link::connect();
        link.stream(Duration::from_millis(500));
        link.lose = 1;
        link.write_frame(false);
        link.idle(Duration::from_millis(5));
        link.write_frame(no_resends_before);
        (link.resent_over(Duration::from_millis(300)), link)
    };

    assert!(
        run(false).0 >= 1,
        "the receiver asks for the lost packet and gets it"
    );

    let (resent, mut link) = run(true);
    assert_eq!(resent, 0, "the packet lost before the frame is not resent");
    link.lose = 1;
    assert!(
        link.resent_over(Duration::from_millis(300)) >= 1,
        "a packet lost after it is"
    );
}

#[test]
fn the_resends_before_the_frame_stop_counting_against_the_rtx_ratio_cap() {
    // Thirty packets lost in a row come back all at once, far over the 15 % of the bytes sent in
    // a second that the default cap allows for resends, which then holds every resend back for a
    // while.
    let run = |no_resends_before: bool| {
        let mut link = Link::connect();
        link.stream(Duration::from_millis(500));
        link.lose = 30;
        for _ in 0..10 {
            link.write_frame(false);
        }
        assert!(
            link.resent_over(Duration::from_millis(200)) >= 30,
            "the burst was resent"
        );
        link.write_frame(no_resends_before);
        link.idle(Duration::from_millis(5));
        link.lose = 1;
        link.resent_over(Duration::from_millis(300))
    };

    assert_eq!(run(false), 0, "a packet lost after the burst is held back");
    assert!(
        run(true) >= 1,
        "after the frame, what the burst cost no longer counts: the packet lost after it is resent"
    );
}
