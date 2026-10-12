//! What arrives on the socket, turned into "send this frame now" — with no I/O and no clock of
//! its own, so every rule below is a unit test rather than a timing experiment.
//!
//! Nothing is queued. The newest accepted state replaces the last one; the edges between them are
//! accumulated per datagram so none is lost to merging; a send happens on arrival when the last
//! one is at least `min_interval` old, else at that slot. The spec's §3 is the argument.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use pad_map::wire::{Packet, WireError};
use pad_map::{Buttons, PadFrame};

/// How far ahead of the last accepted `seq` a datagram may be and still count as newer. A client
/// sends at most 100 a second and is gone after `timeout_ms` of silence, so an honest one never
/// gets near it; without it, one forged datagram far ahead would read every real one as stale and
/// hand the robot to whoever forged it.
pub const MAX_SEQ_AHEAD: u32 = 1000;

/// Late without a break for this long is not a backlog — one drains in a burst, in milliseconds —
/// but the sender's clock having moved (a laptop that slept, a suspended process), and the
/// datagrams are taken as on time from then on.
pub const LATE_FOR: Duration = Duration::from_secs(1);

/// Assumes `min_interval <= fallback`, which `[netpad] max_hz >= 10` guarantees in robotd-params.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// `1 / [netpad] max_hz`.
    pub min_interval: Duration,
    /// `[netpad] timeout_ms`.
    pub timeout: Duration,
    /// How often the mapping runs with no new datagram — `pad_map::HEARTBEAT`.
    pub fallback: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    /// Accepted, and it is the first since the client was last gone.
    Connected,
    Taken,
    /// Not newer than what is held, or further ahead than [`MAX_SEQ_AHEAD`].
    Stale,
    /// Newer, but delivered more than `timeout` later than the stream's fastest — see [`Delivery`].
    Late,
    /// Another sender holds the lock.
    OtherPeer,
    Malformed(WireError),
}

#[derive(Debug, PartialEq)]
pub enum Step {
    /// Run the mapping on this.
    Tick(PadFrame),
    /// The client just went quiet: send nothing from now, and reset the holds.
    Gone,
    Idle,
}

#[derive(Debug)]
pub struct Receiver {
    timing: Timing,
    /// The client, while it is alive. `None` means nobody is driving.
    peer: Option<SocketAddr>,
    /// Who sent `latest`, kept across a silence so the same stream coming back is recognised.
    stream: Option<SocketAddr>,
    latest: Packet,
    delivery: Delivery,
    /// The zero of the robot's side of [`Delivery`]'s offsets.
    epoch: Instant,
    last_rx: Instant,
    last_tick: Option<Instant>,
    /// Something was accepted since the last tick.
    fresh: bool,
    pressed: Buttons,
    released: Buttons,
    reserved_seen: u16,
}

impl Receiver {
    pub fn new(timing: Timing) -> Self {
        debug_assert!(timing.min_interval <= timing.fallback);
        Self {
            timing,
            peer: None,
            stream: None,
            latest: Packet::default(),
            delivery: Delivery::default(),
            epoch: Instant::now(),
            last_rx: Instant::now(),
            last_tick: None,
            fresh: false,
            pressed: Buttons::NONE,
            released: Buttons::NONE,
            reserved_seen: 0,
        }
    }

    pub fn on_datagram(&mut self, now: Instant, from: SocketAddr, bytes: &[u8]) -> Accept {
        let packet = match Packet::decode(bytes) {
            Ok(p) => p,
            Err(e) => return Accept::Malformed(e),
        };
        self.reserved_seen |= packet.reserved();
        // Newer, in wrapping order, and not implausibly so, from whoever sent the last one.
        let ahead = packet.seq.wrapping_sub(self.latest.seq);
        let continues = self.stream == Some(from) && (1..=MAX_SEQ_AHEAD).contains(&ahead);
        match self.peer {
            Some(peer) if peer != from => return Accept::OtherPeer,
            Some(_) if !continues => return Accept::Stale,
            _ => {}
        }
        // A new stream brings its own clock; the same one back after a silence keeps its own, so
        // what it queued during that silence is still known for late.
        if !continues {
            self.delivery = Delivery::default();
        }
        if packet.t_ms != 0 {
            let ours = now.duration_since(self.epoch).as_millis() as u32;
            if !self
                .delivery
                .on_time(now, ours.wrapping_sub(packet.t_ms), self.timing.timeout)
            {
                return Accept::Late;
            }
        }
        let connected = if self.peer.is_some() {
            let (now_held, before) = (packet.held(), self.latest.held());
            self.pressed |= now_held.newly_set(before);
            self.released |= before.newly_set(now_held);
            false
        } else {
            // A new client, or the old one back: what it holds was pressed before we saw it,
            // and firing it now would run a skill nobody just asked for.
            self.peer = Some(from);
            self.stream = Some(from);
            self.pressed = Buttons::NONE;
            self.released = Buttons::NONE;
            self.last_tick = None;
            true
        };
        self.latest = packet;
        self.last_rx = now;
        self.fresh = true;
        if connected {
            Accept::Connected
        } else {
            Accept::Taken
        }
    }

    pub fn poll(&mut self, now: Instant) -> Step {
        if self.peer.is_none() {
            return Step::Idle;
        }
        if now.duration_since(self.last_rx) >= self.timing.timeout {
            self.peer = None;
            self.fresh = false;
            return Step::Gone;
        }
        let due = match self.last_tick {
            None => true,
            Some(at) => {
                let since = now.duration_since(at);
                (self.fresh && since >= self.timing.min_interval) || since >= self.timing.fallback
            }
        };
        if !due {
            return Step::Idle;
        }
        let frame = self.latest.frame(self.pressed, self.released);
        self.pressed = Buttons::NONE;
        self.released = Buttons::NONE;
        self.fresh = false;
        self.last_tick = Some(now);
        Step::Tick(frame)
    }

    /// When `poll` next has something to say, or `None` when nobody is driving.
    pub fn deadline(&self) -> Option<Instant> {
        self.peer?;
        let timeout = self.last_rx + self.timing.timeout;
        let next = match self.last_tick {
            None => return Some(self.last_rx),
            Some(at) if self.fresh => at + self.timing.min_interval,
            Some(at) => at + self.timing.fallback,
        };
        Some(next.min(timeout))
    }

    /// The client driving the robot, while there is one.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// Button bits this build does not know, OR-ed over everything accepted — for one log line.
    pub fn reserved_bits_seen(&self) -> u16 {
        self.reserved_seen
    }
}

/// How late a stream's datagrams arrive, without syncing clocks: `ours − t_ms` is the delivery
/// time plus a fixed offset between the two clocks, so its smallest value is the fastest delivery
/// seen, and how far a datagram sits above it is how late that one is. Only the difference is ever
/// used, so the offset — and which clock is ahead — cancels out.
///
/// Drift between the clocks moves the minimum by tens of µs a second; at the default 250 ms limit
/// that takes hours to matter, and then costs one [`LATE_FOR`] before the stream is taken again.
#[derive(Debug, Default)]
struct Delivery {
    fastest: Option<u32>,
    late_since: Option<Instant>,
}

impl Delivery {
    /// Whether a datagram with this offset is on time; learns from it when it is.
    fn on_time(&mut self, now: Instant, offset: u32, limit: Duration) -> bool {
        let Some(fastest) = self.fastest else {
            self.fastest = Some(offset);
            return true;
        };
        let late = offset.wrapping_sub(fastest) as i32;
        if late < 0 {
            self.fastest = Some(offset);
        } else if late as u128 > limit.as_millis() {
            let since = *self.late_since.get_or_insert(now);
            if now.duration_since(since) < LATE_FOR {
                return false;
            }
            self.fastest = Some(offset);
        }
        self.late_since = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pad_map::wire::Packet;

    const A: &str = "10.0.0.2:5000";
    const B: &str = "10.0.0.3:5000";

    fn rx() -> Receiver {
        Receiver::new(Timing {
            min_interval: Duration::from_millis(33),
            timeout: Duration::from_millis(250),
            fallback: Duration::from_millis(100),
        })
    }
    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }
    fn pkt(seq: u32, buttons: Buttons, lx: f64) -> [u8; 28] {
        Packet::from_axes(seq, 0, [lx, 0.0, 0.0, 0.0], [0.0, 0.0], buttons).encode()
    }
    /// A datagram stamped with the sender's clock, `t_ms` (never 0, which is unchecked).
    fn sent(seq: u32, t_ms: u32, buttons: Buttons) -> [u8; 28] {
        Packet::from_axes(seq, t_ms, [0.0; 4], [0.0, 0.0], buttons).encode()
    }
    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }
    fn tick(step: Step) -> PadFrame {
        match step {
            Step::Tick(f) => f,
            other => panic!("expected a tick, got {other:?}"),
        }
    }

    #[test]
    fn a_first_datagram_is_sent_at_once() {
        let (mut r, t) = (rx(), Instant::now());
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.5)),
            Accept::Connected
        );
        assert!((tick(r.poll(t)).left_x - 0.5).abs() < 1e-4);
    }

    /// The cap: a second change inside one interval waits for the slot, and the slot sends the
    /// newest state, not the one in between.
    #[test]
    fn changes_inside_an_interval_merge_into_the_newest_at_the_slot() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.1));
        tick(r.poll(t));
        r.on_datagram(ms(t, 5), addr(A), &pkt(2, Buttons::NONE, 0.2));
        r.on_datagram(ms(t, 10), addr(A), &pkt(3, Buttons::NONE, 0.3));
        assert_eq!(r.poll(ms(t, 10)), Step::Idle, "inside the interval");
        assert_eq!(r.deadline(), Some(ms(t, 33)));
        assert!(
            (tick(r.poll(ms(t, 33))).left_x - 0.3).abs() < 1e-4,
            "the newest"
        );
    }

    /// Once the last send is an interval old, a change goes out on arrival — no waiting for a
    /// periodic slot. This is the latency the design exists for.
    #[test]
    fn a_change_after_a_quiet_interval_is_sent_on_arrival() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.0));
        tick(r.poll(t));
        r.on_datagram(ms(t, 60), addr(A), &pkt(2, Buttons::NONE, 0.9));
        assert!((tick(r.poll(ms(t, 60))).left_x - 0.9).abs() < 1e-4);
    }

    #[test]
    fn a_late_datagram_is_dropped() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(5, Buttons::NONE, 0.5));
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(4, Buttons::NONE, -0.5)),
            Accept::Stale
        );
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(5, Buttons::NONE, -0.5)),
            Accept::Stale
        );
        assert!((tick(r.poll(t)).left_x - 0.5).abs() < 1e-4);
    }

    #[test]
    fn seq_wraps() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(u32::MAX, Buttons::NONE, 0.0));
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(0, Buttons::NONE, 0.0)),
            Accept::Taken
        );
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(u32::MAX, Buttons::NONE, 0.0)),
            Accept::Stale
        );
    }

    /// A datagram from the driving client's address, forged far ahead, would otherwise make every
    /// real one stale and take the robot over.
    #[test]
    fn a_datagram_too_far_ahead_is_dropped() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(5, Buttons::NONE, 0.5));
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(5 + MAX_SEQ_AHEAD + 1, Buttons::NONE, -1.0)),
            Accept::Stale
        );
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(6, Buttons::NONE, 0.5)),
            Accept::Taken,
            "the real client keeps driving"
        );
        assert_eq!(
            r.on_datagram(t, addr(A), &pkt(6 + MAX_SEQ_AHEAD, Buttons::NONE, 0.5)),
            Accept::Taken,
            "the bound itself is accepted"
        );
    }

    /// A press and a release that both land between two sends still make a tap.
    #[test]
    fn a_press_and_release_inside_one_interval_are_both_seen() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.0));
        tick(r.poll(t));
        r.on_datagram(ms(t, 5), addr(A), &pkt(2, Buttons::START, 0.0));
        r.on_datagram(ms(t, 10), addr(A), &pkt(3, Buttons::NONE, 0.0));
        let f = tick(r.poll(ms(t, 33)));
        assert!(f.pressed.contains(Buttons::START) && f.released.contains(Buttons::START));
        assert!(!f.held.contains(Buttons::START));
        let f = tick(r.poll(ms(t, 140)));
        assert_eq!(
            (f.pressed, f.released),
            (Buttons::NONE, Buttons::NONE),
            "edges are consumed"
        );
    }

    #[test]
    fn the_first_datagram_makes_no_edges() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::A | Buttons::UP, 0.0));
        let f = tick(r.poll(t));
        assert_eq!(f.pressed, Buttons::NONE);
        assert!(f.held.contains(Buttons::A));
    }

    #[test]
    fn another_peer_is_ignored_while_the_first_is_alive() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.5));
        assert_eq!(
            r.on_datagram(ms(t, 10), addr(B), &pkt(99, Buttons::NONE, -1.0)),
            Accept::OtherPeer
        );
        assert!((tick(r.poll(ms(t, 10))).left_x - 0.5).abs() < 1e-4);
    }

    #[test]
    fn going_away_reports_gone_once_and_frees_the_lock() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::START, 0.5));
        tick(r.poll(t));
        tick(r.poll(ms(t, 100)));
        tick(r.poll(ms(t, 200)));
        assert_eq!(r.poll(ms(t, 250)), Step::Gone);
        assert_eq!(r.poll(ms(t, 300)), Step::Idle);
        assert_eq!(r.deadline(), None, "nothing to wake for");
        assert_eq!(
            r.on_datagram(ms(t, 400), addr(B), &pkt(1, Buttons::NONE, 0.0)),
            Accept::Connected
        );
    }

    #[test]
    fn a_restarted_client_is_taken_after_the_timeout() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1000, Buttons::NONE, 0.0));
        tick(r.poll(t));
        assert_eq!(
            r.on_datagram(ms(t, 50), addr(A), &pkt(3, Buttons::NONE, 0.0)),
            Accept::Stale
        );
        assert_eq!(r.poll(ms(t, 250)), Step::Gone);
        assert_eq!(
            r.on_datagram(ms(t, 260), addr(A), &pkt(4, Buttons::NONE, 0.0)),
            Accept::Connected
        );
    }

    /// Lost datagrams do not stop the mapping: hold timing and the heartbeat keep their clock.
    #[test]
    fn the_fallback_ticks_with_no_new_datagram() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.0));
        tick(r.poll(t));
        assert_eq!(r.poll(ms(t, 99)), Step::Idle);
        assert_eq!(r.deadline(), Some(ms(t, 100)));
        tick(r.poll(ms(t, 100)));
    }

    /// Start held when the client drops, still held when it comes back, then let go: the first
    /// datagram back makes no edges, so without the hold being spent the mapping would time a
    /// fresh hold from the reconnection and call the release a tap — `robot.init` from a thumb
    /// that was only ever resting on the button. The receiver and the mapping together, as
    /// `main.rs` wires them, ask for nothing at all.
    #[test]
    fn start_held_across_a_dropout_asks_for_nothing() {
        use pad_map::{Config, Mapper, Out};

        let (mut r, t) = (rx(), Instant::now());
        let cfg = Config::defaults(false, &Default::default());
        let mut mapper = Mapper::new();
        let (mut out, mut frame) = (Vec::new(), Vec::new());
        let mut requests = Vec::new();
        let mut run = |r: &mut Receiver, now: Instant| match r.poll(now) {
            Step::Tick(pad) => {
                mapper.tick(&pad, &cfg, now, &mut out, &mut frame);
                requests.extend(out.drain(..).filter(|o| matches!(o, Out::Request(_))));
            }
            Step::Gone => mapper.pad_gone(),
            Step::Idle => {}
        };

        r.on_datagram(t, addr(A), &pkt(1, Buttons::NONE, 0.0));
        run(&mut r, t);
        r.on_datagram(ms(t, 50), addr(A), &pkt(2, Buttons::START, 0.0));
        run(&mut r, ms(t, 50));
        run(&mut r, ms(t, 150));
        run(&mut r, ms(t, 300)); // the timeout: Gone, and the holds reset
        assert_eq!(r.deadline(), None, "the client is gone");

        // Back with Start still down, held for longer than the way home, then let go.
        r.on_datagram(ms(t, 1_000), addr(A), &pkt(3, Buttons::START, 0.0));
        let mut seq = 3;
        for at in (1_000..=3_000).step_by(50) {
            seq += 1;
            r.on_datagram(ms(t, at), addr(A), &pkt(seq, Buttons::START, 0.0));
            run(&mut r, ms(t, at));
        }
        r.on_datagram(ms(t, 3_050), addr(A), &pkt(seq + 1, Buttons::NONE, 0.0));
        run(&mut r, ms(t, 3_050));
        run(&mut r, ms(t, 3_200));

        assert!(requests.is_empty(), "asked for {requests:?}");
    }

    #[test]
    fn malformed_is_reported_and_changes_nothing() {
        let (mut r, t) = (rx(), Instant::now());
        assert!(matches!(
            r.on_datagram(t, addr(A), b"hello"),
            Accept::Malformed(_)
        ));
        assert_eq!(r.poll(t), Step::Idle);
        assert_eq!(
            r.on_datagram(t, addr(B), &pkt(1, Buttons::NONE, 0.0)),
            Accept::Connected,
            "no lock taken"
        );
    }

    /// The link holds three seconds of datagrams and lets them go at once: none drives, the Start
    /// pressed in them fires nothing, and the first one sent after drives at once.
    #[test]
    fn a_backlog_is_dropped_and_what_follows_drives() {
        let (mut r, t) = (rx(), Instant::now());
        // The sender's clock reads 5000 at our `t`, and delivery takes 2 ms.
        for i in 0..=3u32 {
            let at = 33 * u64::from(i);
            r.on_datagram(
                ms(t, at + 2),
                addr(A),
                &sent(1 + i, 5000 + at as u32, Buttons::NONE),
            );
            tick(r.poll(ms(t, at + 2)));
        }
        // Sent from 133 ms to 2800 ms, every one delivered at 3102 ms: more than 250 ms late.
        assert_eq!(r.poll(ms(t, 3102)), Step::Gone, "silent for three seconds");
        let mut seq = 5;
        for at in (133..2800u32).step_by(33) {
            let held = if at < 1000 {
                Buttons::START
            } else {
                Buttons::NONE
            };
            assert_eq!(
                r.on_datagram(ms(t, 3102), addr(A), &sent(seq, 5000 + at, held)),
                Accept::Late,
                "sent at {at} ms"
            );
            seq += 1;
        }
        assert_eq!(r.poll(ms(t, 3102)), Step::Idle, "nothing drives");
        assert_eq!(
            r.on_datagram(ms(t, 3105), addr(A), &sent(seq, 5000 + 3103, Buttons::NONE)),
            Accept::Connected
        );
        assert_eq!(tick(r.poll(ms(t, 3105))).pressed, Buttons::NONE);
    }

    /// Late by up to `timeout` is jitter, and taken.
    #[test]
    fn late_inside_the_timeout_is_taken() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(ms(t, 2), addr(A), &sent(1, 100, Buttons::NONE));
        assert_eq!(
            r.on_datagram(ms(t, 285), addr(A), &sent(2, 133, Buttons::NONE)),
            Accept::Taken,
            "250 ms late"
        );
        assert_eq!(
            r.on_datagram(ms(t, 320), addr(A), &sent(3, 166, Buttons::NONE)),
            Accept::Late,
            "252 ms late"
        );
    }

    /// The sender's clock jumps back — its process was suspended, say. Every datagram after reads
    /// late; after [`LATE_FOR`] of that, the stream is taken again.
    #[test]
    fn a_sender_clock_that_moved_is_taken_after_late_for() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(ms(t, 2), addr(A), &sent(1, 10_000, Buttons::NONE));
        let mut first_taken = None;
        for (seq, at) in (2..).zip((500..3000u64).step_by(33)) {
            // Its clock now reads 5 s behind where it would have.
            let t_ms = 10_000 + at as u32 - 5_000;
            if r.on_datagram(ms(t, at), addr(A), &sent(seq, t_ms, Buttons::NONE)) != Accept::Late {
                first_taken.get_or_insert(at);
            }
        }
        let first = first_taken.expect("taken in the end");
        assert!((1500..1600).contains(&first), "taken at {first} ms");
    }

    /// A restarted sender is a new stream with its own clock, and is not judged by the old one's.
    #[test]
    fn a_new_stream_brings_its_own_clock() {
        let (mut r, t) = (rx(), Instant::now());
        r.on_datagram(ms(t, 2), addr(A), &sent(1, 90_000, Buttons::NONE));
        tick(r.poll(ms(t, 2)));
        assert_eq!(r.poll(ms(t, 300)), Step::Gone);
        assert_eq!(
            r.on_datagram(ms(t, 400), addr(A), &sent(777_777, 1, Buttons::NONE)),
            Accept::Connected
        );
    }
}
