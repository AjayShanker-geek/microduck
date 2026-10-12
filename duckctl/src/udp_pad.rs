//! `duckctl udp-pad` — the pad plugged into this machine, sent to a duck's `netpadd`.
//!
//! The reference sender for the format `docs/robot/udp-pad.md` owns. It sends the pad's whole
//! state the moment anything changes, at most `hz` times a second, and a keepalive every 1/hz
//! when nothing does, and shows what it sends and what `netpadd` answers (`panel.rs`). No pad,
//! nothing sent — the robot times the client out and its deadman holds it, exactly as when a
//! Bluetooth pad goes away.

use std::hash::{BuildHasher, Hasher};
use std::io::IsTerminal;
use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gilrs::{Axis, Button, GamepadId, Gilrs, PowerInfo};
use pad_map::Buttons;
use pad_map::wire::{DEFAULT_PORT, Packet, Status};

mod link;
mod panel;

use link::{Link, State};
use panel::{REDRAW, SUMMARY, Screen, View};

/// When to send: a change and a keepalive are both capped at the interval. What a change buys
/// is that the loop wakes for it at once rather than at the next keepalive (see `run`).
pub struct Pacer {
    interval: Duration,
    last: Option<Instant>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    /// Whether to send now; records it when yes. A change and a keepalive are both capped at
    /// the interval — the difference is that the loop wakes for a change at once (see `run`).
    pub fn due(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|at| now.duration_since(at) >= self.interval);
        if due {
            self.last = Some(now);
        }
        due
    }

    /// When the next send may happen.
    pub fn next(&self) -> Option<Instant> {
        self.last.map(|at| at + self.interval)
    }
}

/// A failing send that has been clean this long is fixed, and the next failure is news again.
/// Longer than the gap between failures when nothing listens: a connected UDP socket reports the
/// ICMP refusal on the send after the one that caused it, so failures alternate with successes.
const RECOVERED: Duration = Duration::from_secs(2);

/// What is worth a line on stderr, said once per change rather than once per packet: at 30 Hz a
/// line per failed send scrolls the reason away before anyone reads it.
#[derive(Default)]
pub struct Notices {
    /// Whether a pad was there when last looked, `None` before the first look.
    pad: Option<bool>,
    /// The failure last reported, until sends have been clean for [`RECOVERED`].
    failing: Option<std::io::ErrorKind>,
    last_failure: Option<Instant>,
    /// The duck's answers as last said, `None` before the first look.
    link: Option<State>,
}

impl Notices {
    /// The pad on this machine, by name when there is one.
    pub fn pad(&mut self, name: Option<&str>) -> Option<String> {
        let (was, now) = (self.pad, name.is_some());
        self.pad = Some(now);
        match (was, name) {
            (Some(w), _) if w == now => None,
            (_, Some(name)) => Some(format!("gamepad: {name} — sending its state")),
            (None, None) => Some(
                "no gamepad on this machine — sending nothing until one is plugged in".to_owned(),
            ),
            (Some(_), None) => Some(
                "gamepad gone — sending nothing; the duck stops once its deadman runs out"
                    .to_owned(),
            ),
        }
    }

    /// One send's result.
    pub fn sent(
        &mut self,
        to: &str,
        result: &std::io::Result<usize>,
        now: Instant,
    ) -> Option<String> {
        match result {
            Err(e) => {
                self.last_failure = Some(now);
                if self.failing == Some(e.kind()) {
                    return None;
                }
                self.failing = Some(e.kind());
                Some(match e.kind() {
                    std::io::ErrorKind::ConnectionRefused => format!(
                        "nothing listening on {to} — is [netpad] enabled on the duck? \
                         (docs/robot/udp-pad.md); still trying"
                    ),
                    _ => format!("cannot send to {to}: {e}; still trying"),
                })
            }
            Ok(_) => {
                let fixed = self.failing.is_some()
                    && self
                        .last_failure
                        .is_some_and(|at| now.duration_since(at) >= RECOVERED);
                if !fixed {
                    return None;
                }
                self.failing = None;
                Some(format!("reaching {to} again"))
            }
        }
    }
}

impl Notices {
    /// The duck's answers, once per change of kind.
    pub fn link(&mut self, state: State) -> Option<String> {
        if self.link.is_some_and(|was| was.same_kind(state)) {
            return None;
        }
        self.link = Some(state);
        match state {
            State::Waiting => None,
            State::Driving => Some("the duck answers — driving".to_owned()),
            State::Busy => Some(
                "another client is driving the duck — this one is ignored until that one has \
                 been quiet for [netpad] timeout_ms"
                    .to_owned(),
            ),
            State::Silent(_) => Some("no answer from the duck — still sending".to_owned()),
            State::NeverAnswered => Some(
                "no answer from netpadd: it may predate answers (driving still works, the \
                 duck line stays blank), or nothing reaches it"
                    .to_owned(),
            ),
        }
    }
}

fn target(host: &str) -> String {
    if host
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        host.to_owned()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    }
}

const GILRS: [(Button, Buttons); 13] = [
    (Button::South, Buttons::A),
    (Button::East, Buttons::B),
    (Button::West, Buttons::X),
    (Button::North, Buttons::Y),
    (Button::LeftTrigger, Buttons::LB),
    (Button::RightTrigger, Buttons::RB),
    (Button::Start, Buttons::START),
    (Button::Select, Buttons::SELECT),
    (Button::DPadUp, Buttons::UP),
    (Button::DPadDown, Buttons::DOWN),
    (Button::DPadLeft, Buttons::LEFT),
    (Button::DPadRight, Buttons::RIGHT),
    // Home — Xbox, PS. gilrs calls the middle button `Mode`.
    (Button::Mode, Buttons::HOME),
];

/// How often the pad is read. Faster than any `hz` worth sending, so a change is seen within a
/// millisecond and the cap, not the polling, is what decides when it goes.
const POLL: Duration = Duration::from_millis(1);

/// One second's sends, counted.
#[derive(Default)]
struct Rate {
    since: Option<Instant>,
    count: u32,
    last: Option<u32>,
}

impl Rate {
    fn tick(&mut self, now: Instant, sent: bool) {
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_secs(1) {
            self.last = Some(self.count);
            self.since = Some(now);
            self.count = 0;
        }
        self.count += u32::from(sent);
    }
}

/// What the receiving thread hands over: an answer and when it landed, or a receive error — the
/// ICMP refusal a connected socket reports, which may land here rather than on a send.
enum Heard {
    Status(Status, Instant),
    Failed(std::io::ErrorKind),
}

/// Answers are read on their own thread, so a duck that never answers, or answers late, cannot
/// delay a send. Bounded, and dropped when full: the figures go stale, the pad keeps going.
fn listen(socket: &UdpSocket) -> std::io::Result<mpsc::Receiver<Heard>> {
    let socket = socket.try_clone()?;
    let (tx, rx) = mpsc::sync_channel(64);
    std::thread::spawn(move || {
        let mut b = [0u8; 64];
        loop {
            let heard = match socket.recv(&mut b) {
                Ok(n) => match Status::decode(&b[..n]) {
                    Ok(s) => Heard::Status(s, Instant::now()),
                    Err(_) => continue,
                },
                Err(e) => {
                    // A persistent error must not spin a core.
                    std::thread::sleep(Duration::from_millis(50));
                    Heard::Failed(e.kind())
                }
            };
            if let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(heard) {
                return;
            }
        }
    });
    Ok(rx)
}

fn power(info: PowerInfo) -> Option<String> {
    match info {
        PowerInfo::Unknown => None,
        PowerInfo::Wired => Some("wired".to_owned()),
        PowerInfo::Discharging(n) => Some(format!("battery {n}%")),
        PowerInfo::Charging(n) => Some(format!("charging {n}%")),
        PowerInfo::Charged => Some("charged".to_owned()),
    }
}

pub fn run(host: &str, hz: u32, invert_y: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut gilrs = Gilrs::new().map_err(|e| format!("no gamepad subsystem: {e}"))?;
    let to = target(host);
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(&to)?;
    let answers = listen(&socket)?;
    let mut pacer = Pacer::new(Duration::from_secs_f64(1.0 / f64::from(hz)));
    // A random start, so a restarted sender is never mistaken for a stale one for long.
    let mut seq = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish() as u32;
    let started = Instant::now();
    let mut last = None::<Packet>;
    let mut notices = Notices::default();
    let (mut link, mut rate, mut failed) = (Link::default(), Rate::default(), 0u64);
    let mut last_input = None::<(GamepadId, Instant)>;
    let mut screen = std::io::stderr().is_terminal().then(Screen::new);
    let mut shown = None::<Instant>;
    let say = |screen: &mut Option<Screen>, line: String| match screen {
        Some(s) => s.notice(&line),
        None => eprintln!("{line}"),
    };
    say(
        &mut screen,
        format!("sending the first pad on this machine to {to}, at most {hz} Hz — Ctrl-C to stop"),
    );

    loop {
        while let Some(ev) = gilrs.next_event() {
            last_input = Some((ev.id, Instant::now()));
        }
        while let Ok(heard) = answers.try_recv() {
            match heard {
                Heard::Status(s, at) => link.on_status(s, at),
                Heard::Failed(kind) => {
                    if let Some(line) = notices.sent(&to, &Err(kind.into()), Instant::now()) {
                        say(&mut screen, line);
                    }
                }
            }
        }
        let pad = gilrs.gamepads().next();
        if let Some(line) = notices.pad(pad.as_ref().map(|(_, p)| p.name())) {
            say(&mut screen, line);
        }
        let now = Instant::now();
        if let Some(line) = notices.link(link.state(now)) {
            say(&mut screen, line);
        }

        let (mut sent_now, mut changed) = (false, false);
        // The pad as read now, for the panel: what is held, which may be a slot ahead of `last`.
        let mut reading = None::<Packet>;
        if let Some((_, pad)) = &pad {
            let held = GILRS
                .iter()
                .filter(|(b, _)| pad.is_pressed(*b))
                .fold(Buttons::NONE, |a, (_, bit)| a | *bit);
            let trigger = |b: Button| f64::from(pad.button_data(b).map_or(0.0, |d| d.value()));
            let y = if invert_y { -1.0 } else { 1.0 };
            let mut packet = Packet::from_axes(
                0,
                now.duration_since(started).as_millis() as u32,
                [
                    f64::from(pad.value(Axis::LeftStickX)),
                    y * f64::from(pad.value(Axis::LeftStickY)),
                    f64::from(pad.value(Axis::RightStickX)),
                    y * f64::from(pad.value(Axis::RightStickY)),
                ],
                [
                    trigger(Button::LeftTrigger2),
                    trigger(Button::RightTrigger2),
                ],
                held,
            );
            changed = last.is_none_or(|l| l.axes != packet.axes || l.buttons != packet.buttons);
            reading = Some(packet);
            if pacer.due(now) {
                seq = seq.wrapping_add(1);
                packet.seq = seq;
                // A refused send (no route yet, the duck rebooting, netpadd not running) is not
                // fatal: the next one tries again. Said once, not per packet — see `Notices`.
                let result = socket.send(&packet.encode());
                match &result {
                    Ok(_) => {
                        link.on_sent(seq, now);
                        sent_now = true;
                    }
                    Err(_) => failed += 1,
                }
                if let Some(line) = notices.sent(&to, &result, now) {
                    say(&mut screen, line);
                }
                last = Some(packet);
            }
        } else {
            last = None;
            link.paused();
        }
        rate.tick(now, sent_now);

        let every = if screen.is_some() { REDRAW } else { SUMMARY };
        if shown.is_none_or(|at| now.duration_since(at) >= every) {
            shown = Some(now);
            let view = View {
                to: &to,
                pad: pad.as_ref().map(|(id, p)| panel::Pad {
                    name: p.name(),
                    power: power(p.power_info()),
                    last_input: last_input
                        .filter(|(at_id, _)| at_id == id)
                        .map(|(_, at)| now.duration_since(at)),
                    packet: reading.unwrap_or_default(),
                }),
                hz,
                rate: rate.last,
                failed,
                link: &link,
                now,
            };
            match &mut screen {
                Some(s) => s.draw(&panel::lines(&view)),
                None => eprintln!("{}", panel::summary(&view)),
            }
        }

        let wake = if pad.is_none() {
            now + Duration::from_millis(100)
        } else if changed {
            now + POLL
        } else {
            pacer.next().unwrap_or(now + POLL).min(now + POLL * 5)
        };
        if let Some(d) = wake.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn sends_are_capped_at_the_interval_whether_or_not_anything_changed() {
        let t = Instant::now();
        let mut p = Pacer::new(Duration::from_millis(33));
        assert!(p.due(t), "first");
        assert!(!p.due(t + Duration::from_millis(10)), "capped");
        assert!(p.due(t + Duration::from_millis(33)), "change at the slot");
        assert!(!p.due(t + Duration::from_millis(50)), "steady, not yet");
        assert!(p.due(t + Duration::from_millis(66)), "keepalive");
    }

    /// A sender with no duck listening fails every other packet at 30 Hz; it says so once, and
    /// once more only when the failure changes or it has been fine for a while and fails again.
    #[test]
    fn a_send_failure_is_said_once_per_kind_and_recovery_once() {
        use std::io::{Error, ErrorKind};
        let (t, to) = (Instant::now(), "duck.local:4210");
        let at = |ms: u64| t + Duration::from_millis(ms);
        let refused = || Err(Error::from(ErrorKind::ConnectionRefused));
        let mut n = Notices::default();

        assert_eq!(n.sent(to, &Ok(28), at(0)), None);
        let first = n.sent(to, &refused(), at(33)).expect("said");
        assert!(
            first.contains("nothing listening on duck.local:4210"),
            "{first}"
        );
        assert!(first.contains("[netpad] enabled"), "{first}");
        // The ICMP answer lands on every other send: alternating, and silent.
        for i in 2..60 {
            let r = if i % 2 == 0 { Ok(28) } else { refused() };
            assert_eq!(n.sent(to, &r, at(33 * i)), None, "send {i}");
        }
        let other = n.sent(
            to,
            &Err(Error::from(ErrorKind::NetworkUnreachable)),
            at(2_000),
        );
        assert!(other.is_some_and(|m| m.contains("duck.local:4210")));

        // Clean for long enough: said once, and the next failure is news again.
        assert_eq!(n.sent(to, &Ok(28), at(3_000)), None, "not yet");
        assert!(
            n.sent(to, &Ok(28), at(4_000))
                .is_some_and(|m| m.contains("again"))
        );
        assert_eq!(n.sent(to, &Ok(28), at(4_033)), None);
        assert!(n.sent(to, &refused(), at(5_000)).is_some());
    }

    /// The pad on this machine: said when it is first looked for, and on every change after.
    #[test]
    fn the_pad_is_said_once_per_change() {
        let mut n = Notices::default();
        assert!(n.pad(None).is_some_and(|m| m.contains("no gamepad")));
        assert_eq!(n.pad(None), None);
        assert!(
            n.pad(Some("Xbox Controller"))
                .is_some_and(|m| m.contains("Xbox Controller"))
        );
        assert_eq!(n.pad(Some("Xbox Controller")), None);
        assert!(n.pad(None).is_some_and(|m| m.contains("gone")));
        assert_eq!(n.pad(None), None);
    }

    /// The duck's answers: said when they start, change kind, or stop — not as a silence grows.
    #[test]
    fn the_link_is_said_once_per_change_of_kind() {
        let mut n = Notices::default();
        assert_eq!(n.link(State::Waiting), None);
        assert!(
            n.link(State::Driving)
                .is_some_and(|m| m.contains("driving"))
        );
        assert_eq!(n.link(State::Driving), None);
        assert!(
            n.link(State::Silent(link::SILENT))
                .is_some_and(|m| m.contains("no answer"))
        );
        assert_eq!(n.link(State::Silent(link::NEVER)), None, "a longer silence");
        assert!(
            n.link(State::Busy)
                .is_some_and(|m| m.contains("another client"))
        );
    }

    #[test]
    fn host_without_port_gets_the_default() {
        assert_eq!(target("10.0.0.5"), "10.0.0.5:4210");
        assert_eq!(target("10.0.0.5:9000"), "10.0.0.5:9000");
        assert_eq!(target("duck.local"), "duck.local:4210");
    }
}
