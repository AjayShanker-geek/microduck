//! What `duckctl udp-pad` shows while it runs: the pad, what it holds, the sends, the duck.
//!
//! On a terminal it is a block of lines redrawn in place under the notices, at most [`REDRAW`]
//! often, so drawing never competes with sending. Piped, it is one summary line every
//! [`SUMMARY`] — a log of a session should not be a screen's worth of redraws.

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::{cursor, queue, terminal};
use pad_map::Buttons;
use pad_map::wire::Packet;

use super::link::{Link, State};

pub const REDRAW: Duration = Duration::from_millis(100);
pub const SUMMARY: Duration = Duration::from_secs(10);

const NAMES: [(Buttons, &str); 13] = [
    (Buttons::A, "A"),
    (Buttons::B, "B"),
    (Buttons::X, "X"),
    (Buttons::Y, "Y"),
    (Buttons::LB, "LB"),
    (Buttons::RB, "RB"),
    (Buttons::START, "START"),
    (Buttons::SELECT, "SELECT"),
    (Buttons::UP, "UP"),
    (Buttons::DOWN, "DOWN"),
    (Buttons::LEFT, "LEFT"),
    (Buttons::RIGHT, "RIGHT"),
    (Buttons::HOME, "HOME"),
];

/// The pad on this machine, as last read.
pub struct Pad<'a> {
    pub name: &'a str,
    /// gilrs' answer, already in words: "battery 70%", "wired", or nothing when it cannot tell.
    pub power: Option<String>,
    pub last_input: Option<Duration>,
    pub packet: Packet,
}

/// Everything one frame of the panel shows.
pub struct View<'a> {
    pub to: &'a str,
    pub pad: Option<Pad<'a>>,
    pub hz: u32,
    /// Sends in the last second.
    pub rate: Option<u32>,
    pub failed: u64,
    pub link: &'a Link,
    pub now: Instant,
}

pub fn held(b: Buttons) -> String {
    let names: Vec<_> = NAMES
        .iter()
        .filter(|(bit, _)| b.contains(*bit))
        .map(|(_, n)| format!("[{n}]"))
        .collect();
    if names.is_empty() {
        "—".to_owned()
    } else {
        names.join(" ")
    }
}

fn ms(d: Duration) -> String {
    if d >= Duration::from_secs(10) {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

fn robot(v: &View) -> String {
    let l = v.link;
    let figures = || {
        let mut f = Vec::new();
        if let Some(rtt) = l.rtt() {
            f.push(format!("rtt {:.1} ms", rtt.as_secs_f64() * 1e3));
        }
        if let Some(loss) = l.loss() {
            f.push(format!("loss {:.1}%", loss * 100.0));
        }
        if let Some(stale) = l.stale().filter(|n| *n > 0) {
            f.push(format!("{stale} stale"));
        }
        if let Some(bad) = l.malformed().filter(|n| *n > 0) {
            f.push(format!("{bad} malformed on its port"));
        }
        f.iter().map(|s| format!(" · {s}")).collect::<String>()
    };
    match l.state(v.now) {
        State::Waiting if v.pad.is_none() => "idle — nothing sent without a pad".to_owned(),
        State::Waiting => "waiting for an answer".to_owned(),
        State::Driving => format!("driving{}", figures()),
        State::Busy => {
            "another client is driving it — ignored until that one goes quiet".to_owned()
        }
        State::Silent(d) => format!("no answer for {}", ms(d)),
        State::NeverAnswered => {
            "never answered — netpadd too old for answers, or unreachable".to_owned()
        }
    }
}

/// The block, one string per line.
pub fn lines(v: &View) -> [String; 4] {
    let (pad, input) = match &v.pad {
        None => ("none — plug one in".to_owned(), "—".to_owned()),
        Some(p) => {
            let mut pad = p.name.to_owned();
            if let Some(power) = &p.power {
                pad += &format!(" · {power}");
            }
            if let Some(d) = p.last_input {
                pad += &format!(" · last input {} ago", ms(d));
            }
            let f = p.packet.frame(Buttons::NONE, Buttons::NONE);
            let input = format!(
                "L ({:+.2}, {:+.2})  R ({:+.2}, {:+.2})  LT {:.2}  RT {:.2}  {}",
                f.left_x,
                f.left_y,
                f.right_x,
                f.right_y,
                f.lt,
                f.rt,
                held(f.held)
            );
            (pad, input)
        }
    };
    let rate = v.rate.map_or("—".to_owned(), |r| r.to_string());
    [
        format!("pad    {pad}"),
        format!("input  {input}"),
        format!(
            "send   {rate} Hz of {} → {} · {} failed",
            v.hz, v.to, v.failed
        ),
        format!("duck   {}", robot(v)),
    ]
}

/// One line for a log: the same facts, flattened.
pub fn summary(v: &View) -> String {
    let [pad, _, send, duck] = lines(v);
    let squash = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{} | {} | {}", squash(pad), squash(send), squash(duck))
}

/// The block on a terminal, under whatever notices have been printed.
pub struct Screen {
    drawn: u16,
}

impl Screen {
    pub fn new() -> Self {
        Self { drawn: 0 }
    }

    fn clear(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.drawn > 0 {
            queue!(
                out,
                cursor::MoveToPreviousLine(self.drawn),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )?;
            self.drawn = 0;
        }
        Ok(())
    }

    /// A notice goes above the block, which is redrawn on the next [`Screen::draw`].
    pub fn notice(&mut self, line: &str) {
        let mut err = std::io::stderr().lock();
        let _ = self.clear(&mut err);
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    }

    pub fn draw(&mut self, lines: &[String]) {
        let mut err = std::io::stderr().lock();
        let _ = self.clear(&mut err);
        let width = terminal::size().map_or(usize::MAX, |(w, _)| usize::from(w.max(1)));
        for line in lines {
            // Cut, not wrapped: a wrapped line would throw the count off, and the redraw with it.
            let cut: String = line.chars().take(width.saturating_sub(1)).collect();
            let _ = writeln!(err, "{cut}");
        }
        self.drawn = lines.len() as u16;
        let _ = err.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_buttons_are_named_in_wire_order() {
        assert_eq!(held(Buttons::NONE), "—");
        assert_eq!(held(Buttons::START | Buttons::A), "[A] [START]");
    }

    #[test]
    fn the_block_says_what_the_pad_holds_and_how_the_duck_answers() {
        let link = Link::default();
        let packet = Packet::from_axes(1, 0, [0.0, 0.5, 0.0, 0.0], [0.0, 1.0], Buttons::B);
        let v = View {
            to: "duck.local:4210",
            pad: Some(Pad {
                name: "Xbox Wireless Controller",
                power: Some("battery 70%".to_owned()),
                last_input: Some(Duration::from_millis(12)),
                packet,
            }),
            hz: 30,
            rate: Some(30),
            failed: 0,
            link: &link,
            now: Instant::now(),
        };
        let [pad, input, send, duck] = lines(&v);
        assert!(
            pad.contains("Xbox") && pad.contains("70%") && pad.contains("12 ms"),
            "{pad}"
        );
        assert!(
            input.contains("+0.50") && input.contains("RT 1.00") && input.contains("[B]"),
            "{input}"
        );
        assert!(send.contains("30 Hz of 30"), "{send}");
        assert!(duck.contains("waiting"), "{duck}");
        assert!(!summary(&v).contains('\n'));
    }
}
