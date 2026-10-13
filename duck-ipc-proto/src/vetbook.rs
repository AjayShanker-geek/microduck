//! The vet book: one duck's whole life, in numbers — hours on, metres walked, falls, pickups, and
//! every servo that has been swapped out and how long it lasted.
//!
//! **The same struct is the wire answer and the file on disk.** `robotd` keeps it at
//! `/var/lib/robot/vetbook.json`, outside every release directory, so it survives an update and a
//! rollback (`architecture.md`, "what survives an update"); a reflash starts a new book. Every
//! field is `#[serde(default)]`, so a book written by an older `robotd` loads into a newer one and
//! a field added later starts at zero rather than refusing the file — a book that will not load is
//! a robot's history thrown away, which is the one thing this exists not to do.
//!
//! Counters and durations only, never a time series: the file stays a few kilobytes for the life
//! of the robot. What happened *when* is the journal's job, and the short [`Vetbook::events`]
//! list is for the handful of things worth a date — a servo swap, an update, a note.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bumped when a field changes meaning. Adding one does not bump it — see the module header.
pub const SCHEMA: u32 = 1;

/// How many [`VetEvent`]s a book keeps. The oldest go first; servo swaps are also kept for good
/// on their joint's [`MotorRecord::retired`], so trimming never loses one.
pub const MAX_EVENTS: usize = 300;

/// A servo case temperature at or above which its time counts towards [`Wear::hot_s`]. The
/// XL330's own overheat shutdown is 70 °C; ten degrees short of it is where a joint is working
/// hard rather than merely warm.
pub const HOT_C: f64 = 60.0;

/// The whole book. Answer to `robot.vetbook`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vetbook {
    pub schema: u32,
    /// When this book was started, Unix seconds. The board has no RTC, so this is as good as the
    /// clock was at the time — [`Life::powered_s`] is the age that does not depend on it.
    pub opened: u64,
    pub life: Life,
    /// One per joint, in [`crate::JOINT_NAMES`] order.
    pub motors: Vec<MotorRecord>,
    /// Dated moments, oldest first, capped at [`MAX_EVENTS`].
    pub events: Vec<VetEvent>,
    /// Bookkeeping `robotd` uses to tell a boot from a daemon restart and an update from a
    /// restart on the same release. Not for display.
    pub last_boot_id: String,
    pub last_version: String,
    /// Smoothed pack voltage at the last save — what a new boot's first reading is compared with
    /// to guess that the pack was charged in between.
    pub last_volts: f64,
}

/// Lifetime totals. Seconds are `f64` so a 20 ms tick adds without rounding away.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Life {
    /// `robotd` running — as near to "switched on" as software can see.
    pub powered_s: f64,
    /// Torque on: the joints holding the robot up.
    pub torqued_s: f64,
    pub walking_s: f64,
    pub standing_s: f64,
    pub sitting_s: f64,
    /// Paused by the pickup detector: somebody is holding the duck.
    pub in_hands_s: f64,
    /// The fall verdict up: on its back, front or side.
    pub fallen_s: f64,
    pub theremin_s: f64,
    pub singing_s: f64,

    /// Ground distance while walking, from contact odometry, metres.
    pub distance_m: f64,
    /// Yaw turned while walking, either way, radians.
    pub turned_rad: f64,

    /// Board boots (a new kernel boot id), and `robotd` starts within them.
    pub boots: u64,
    pub starts: u64,
    /// Starts on a different release from the previous one.
    pub updates: u64,
    /// Falls: the fall verdict going up.
    pub falls: u64,
    /// Falls the limp-fall sequence rode down.
    pub soft_landings: u64,
    pub pickups: u64,
    pub ground_picks: u64,
    pub sit_downs: u64,
    pub stand_ups: u64,
    /// Bring-ups to the home pose: Start or `robot.init` from limp.
    pub wake_ups: u64,
    pub pets: u64,
    /// Chorale pieces joined.
    pub songs: u64,
    pub motor_reboots: u64,
    /// Sit-then-power-off sequences that ran to the end: `robot.shutdown`, the pad, an empty pack.
    pub power_offs: u64,
    /// Boots whose first battery reading was well above the last one saved — a charged pack,
    /// guessed. There is no fuel gauge.
    pub charges: u64,
    /// One-shot skills, by name: kicks, roulades, bows…
    pub skills: BTreeMap<String, u64>,

    /// Dropped motor-bus reads, and control ticks that overran.
    pub bus_drops: u64,
    pub missed_ticks: u64,

    pub hottest_motor_c: f64,
    pub hottest_cpu_c: f64,
    /// Lowest and highest smoothed pack voltage seen. Zero lowest means never read.
    pub lowest_volts: f64,
    pub highest_volts: f64,
}

/// One joint's servo — the one in it now, and the ones it had before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotorRecord {
    pub id: u8,
    pub joint: String,
    /// When the servo now in this joint went in, Unix seconds. The book's own opening for the
    /// servo the robot was built with.
    pub installed: u64,
    /// What the servo now in the joint has done.
    pub wear: Wear,
    /// Model and firmware, as last read. `None` until a `robotd` that asks has run.
    pub identity: Option<Identity>,
    /// The servos this joint has had before, oldest first.
    pub retired: Vec<Retired>,
}

/// What one servo has been through.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wear {
    /// Torque on.
    pub torqued_s: f64,
    /// Total angle the output shaft has turned through, either way, radians — the joint's
    /// odometer, and the closest thing to gear wear the bus can measure.
    pub travel_rad: f64,
    /// Present current integrated over time, ampere-seconds — how hard it has worked, not only
    /// how far it has moved: a knee holding a squat travels nowhere and carries the robot.
    pub charge_as: f64,
    pub hottest_c: f64,
    /// Time at or above [`HOT_C`].
    pub hot_s: f64,
    /// Hardware-error alerts the servo latched, by bit — each one held its torque off until a
    /// reboot. Counted as the bit comes up, not for as long as it stays up.
    pub trips: Trips,
}

/// Latched alerts, one counter per `Hardware Error Status` bit that means something here.
///
/// **Bit 0, input voltage, is not counted.** The servos are fed the 2S pack straight, above their
/// own 6 V rating (`robotd-design.md` §3.2), so every servo latches it on every power-up of a
/// charged pack — counted, it reported every joint tripped on a healthy robot. A book written
/// before it was dropped may carry a `voltage` count; it is ignored on load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Trips {
    /// Bit 2: overheating.
    pub overheat: u64,
    /// Bit 3: motor encoder.
    pub encoder: u64,
    /// Bit 4: electrical shock.
    pub shock: u64,
    /// Bit 5: overload — the usual one, a joint asked to hold more than it can.
    pub overload: u64,
}

impl Trips {
    /// Count the bits that are set in `now` and were not in `was`.
    pub fn count(&mut self, was: u8, now: u8) {
        let up = now & !was;
        for (bit, counter) in [
            (2, &mut self.overheat),
            (3, &mut self.encoder),
            (4, &mut self.shock),
            (5, &mut self.overload),
        ] {
            if up & (1 << bit) != 0 {
                *counter += 1;
            }
        }
    }

    pub fn total(&self) -> u64 {
        self.overheat + self.encoder + self.shock + self.overload
    }

    pub fn add(&mut self, other: &Trips) {
        self.overheat += other.overheat;
        self.encoder += other.encoder;
        self.shock += other.shock;
        self.overload += other.overload;
    }
}

/// A servo taken out of a joint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Retired {
    pub installed: u64,
    pub removed: u64,
    pub wear: Wear,
    /// What the servo said about itself while it was in, when it was ever asked.
    pub identity: Option<Identity>,
}

/// Something worth a date.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VetEvent {
    /// Unix seconds.
    pub at: u64,
    /// `opened`, `update`, `swap`, `note`, `charge`.
    pub kind: String,
    pub text: String,
}

/// A servo's model number and firmware version — all the identity a Dynamixel has, since it
/// carries no serial number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    pub model: u16,
    pub firmware: u8,
}

/// Parameters of `robot.vetbookNote`: a line of free text, dated — "reglued the beak", "new
/// feet". Servo swaps are not written here: `robotd` sees those itself, when it adopts a servo
/// fresh from the factory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VetbookNoteParams {
    pub text: String,
}

impl Vetbook {
    /// A new book for a robot nobody has written about yet.
    pub fn open(now: u64) -> Self {
        let motors = crate::JOINT_IDS
            .iter()
            .zip(crate::JOINT_NAMES.iter())
            .map(|(&id, &joint)| MotorRecord {
                id,
                joint: joint.to_owned(),
                installed: now,
                ..MotorRecord::default()
            })
            .collect();
        let mut book = Self {
            schema: SCHEMA,
            opened: now,
            motors,
            ..Self::default()
        };
        book.event(now, "opened", "the vet book was opened");
        book
    }

    /// Make a book loaded from disk whole: a joint this build has and the file does not gets a
    /// fresh record, so a book from before a joint existed still has one row per joint.
    pub fn repair(&mut self) {
        for (&id, &joint) in crate::JOINT_IDS.iter().zip(crate::JOINT_NAMES.iter()) {
            if !self.motors.iter().any(|m| m.id == id) {
                self.motors.push(MotorRecord {
                    id,
                    joint: joint.to_owned(),
                    installed: self.opened,
                    ..MotorRecord::default()
                });
            }
        }
        self.schema = SCHEMA;
    }

    pub fn event(&mut self, at: u64, kind: &str, text: impl Into<String>) {
        self.events.push(VetEvent {
            at,
            kind: kind.to_owned(),
            text: text.into(),
        });
        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(..excess);
        }
    }

    /// A servo came out of joint `id` and a new one went in. `Err` names the IDs there are.
    pub fn swap(&mut self, id: u8, at: u64) -> Result<(), String> {
        let Some(motor) = self.motors.iter_mut().find(|m| m.id == id) else {
            return Err(format!(
                "no joint has id {id}; the ids are {:?}",
                crate::JOINT_IDS
            ));
        };
        let wear = std::mem::take(&mut motor.wear);
        let hours = wear.torqued_s / 3600.0;
        motor.retired.push(Retired {
            installed: motor.installed,
            removed: at,
            wear,
            identity: motor.identity.take(),
        });
        motor.installed = at;
        let text = format!(
            "{} (id {id}): new servo, the old one lasted {hours:.1} h of torque",
            motor.joint
        );
        self.event(at, "swap", text);
        Ok(())
    }
}

impl Wear {
    pub fn add(&mut self, other: &Wear) {
        self.torqued_s += other.torqued_s;
        self.travel_rad += other.travel_rad;
        self.charge_as += other.charge_as;
        self.hottest_c = self.hottest_c.max(other.hottest_c);
        self.hot_s += other.hot_s;
        self.trips.add(&other.trips);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swap_retires_the_wear_and_starts_the_joint_fresh() {
        let mut book = Vetbook::open(100);
        let knee = crate::JOINT_IDS[3];
        book.motors[3].wear.torqued_s = 7200.0;
        book.swap(knee, 500).unwrap();
        let m = &book.motors[3];
        assert_eq!(m.installed, 500);
        assert_eq!(m.wear, Wear::default());
        assert_eq!(m.retired.len(), 1);
        assert_eq!(m.retired[0].installed, 100);
        assert_eq!(m.retired[0].wear.torqued_s, 7200.0);
        assert_eq!(book.events.last().unwrap().kind, "swap");
        assert!(book.events.last().unwrap().text.contains("2.0 h"));
    }

    #[test]
    fn an_unknown_id_is_refused_with_the_ones_there_are() {
        let mut book = Vetbook::open(0);
        let err = book.swap(200, 1).unwrap_err();
        assert!(err.contains("200"));
    }

    #[test]
    fn an_old_book_loads_and_is_repaired() {
        // A book from a robotd that knew nothing but powered time.
        let old = r#"{"opened":5,"life":{"powered_s":12.5}}"#;
        let mut book: Vetbook = serde_json::from_str(old).unwrap();
        book.repair();
        assert_eq!(book.life.powered_s, 12.5);
        assert_eq!(book.motors.len(), crate::JOINT_IDS.len());
        assert_eq!(book.schema, SCHEMA);
    }

    #[test]
    fn a_trip_is_counted_when_its_bit_comes_up_not_while_it_stays() {
        let mut trips = Trips::default();
        // Input voltage, latched by every servo on a charged pack: never a trip.
        trips.count(0, 0b1);
        assert_eq!(trips.total(), 0);
        trips.count(0, 0b10_0000);
        trips.count(0b10_0000, 0b10_0000);
        trips.count(0b10_0000, 0b11_0100);
        assert_eq!(trips.overload, 1);
        assert_eq!(trips.shock, 1);
        assert_eq!(trips.overheat, 1);
        assert_eq!(trips.total(), 3);
    }

    #[test]
    fn a_book_with_voltage_trips_still_loads() {
        let old = r#"{"motors":[{"id":20,"wear":{"trips":{"voltage":1,"overload":2}}}]}"#;
        let book: Vetbook = serde_json::from_str(old).unwrap();
        assert_eq!(book.motors[0].wear.trips.total(), 2);
    }

    #[test]
    fn events_are_capped_oldest_first() {
        let mut book = Vetbook::open(0);
        for i in 0..MAX_EVENTS + 10 {
            book.event(i as u64, "note", format!("{i}"));
        }
        assert_eq!(book.events.len(), MAX_EVENTS);
        assert_eq!(
            book.events.last().unwrap().text,
            format!("{}", MAX_EVENTS + 9)
        );
    }
}
