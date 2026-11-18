//! Architecture.md section 11.2's poll schedule as data: the one home of every slot, the window
//! period each slot belongs to, and S7-D3's stop deadline computed from them. Times are minutes
//! after 00:00 UTC; no field can carry an offset. EDGAR is not on this schedule.

use chrono::{DateTime, Days, NaiveTime, TimeDelta, Utc};

/// One slot: its start as a UTC time of day and the period of the window it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub hour: u32,
    pub minute: u32,
    pub period_minutes: i64,
}

const fn at(hour: u32, minute: u32, period_minutes: i64) -> Slot {
    Slot {
        hour,
        minute,
        period_minutes,
    }
}

const BUSY: i64 = 30;
const OUTSIDE: i64 = 90;
const MINUTES_A_DAY: i64 = 24 * 60;

/// S7-D1 (owner, 2026-11-17): 11:00-22:00 every 30 min, 23 slots; outside, 8 slots at 90 min,
/// 23:30 -> 00:30 being 60 min apart.
pub const SLOTS: [Slot; 31] = [
    at(0, 30, OUTSIDE),
    at(2, 0, OUTSIDE),
    at(3, 30, OUTSIDE),
    at(5, 0, OUTSIDE),
    at(6, 30, OUTSIDE),
    at(8, 0, OUTSIDE),
    at(9, 30, OUTSIDE),
    at(11, 0, BUSY),
    at(11, 30, BUSY),
    at(12, 0, BUSY),
    at(12, 30, BUSY),
    at(13, 0, BUSY),
    at(13, 30, BUSY),
    at(14, 0, BUSY),
    at(14, 30, BUSY),
    at(15, 0, BUSY),
    at(15, 30, BUSY),
    at(16, 0, BUSY),
    at(16, 30, BUSY),
    at(17, 0, BUSY),
    at(17, 30, BUSY),
    at(18, 0, BUSY),
    at(18, 30, BUSY),
    at(19, 0, BUSY),
    at(19, 30, BUSY),
    at(20, 0, BUSY),
    at(20, 30, BUSY),
    at(21, 0, BUSY),
    at(21, 30, BUSY),
    at(22, 0, BUSY),
    at(23, 30, OUTSIDE),
];

/// Why a slot table is not a schedule; each refusal names the first slot that breaks it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleRefusal {
    #[error("the schedule names no slot")]
    NoSlot,
    #[error("{hour:02}:{minute:02} is not a UTC time of day")]
    NotATimeOfDay { hour: u32, minute: u32 },
    #[error("slot {slot} is named twice")]
    DuplicateSlot { slot: String },
    #[error("slot {slot} comes before {previous}, which is listed first")]
    Unsorted { slot: String, previous: String },
    /// Half the period must leave more than the patience, or the deadline is at or before the
    /// slot's own start.
    #[error("slot {slot}'s period of {period_minutes} min leaves no time to ask before its stop")]
    PeriodTooShort { slot: String, period_minutes: i64 },
    /// A period is the spacing of the slot's window, so it equals the gap to one neighbour.
    #[error(
        "slot {slot}'s period of {period_minutes} min is neither the {before} min since the slot \
         before nor the {after} min to the slot after"
    )]
    PeriodNotASpacing {
        slot: String,
        period_minutes: i64,
        before: i64,
        after: i64,
    },
}

/// A validated slot table; [`committed`] is `SLOTS`.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    slots: &'static [Slot],
}

/// The slot a run belongs to, as instants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotAt {
    pub start: DateTime<Utc>,
    pub period: TimeDelta,
    pub next: DateTime<Utc>,
}

impl SlotAt {
    /// S7-D3: `min(start + period / 2, next) - patience`.
    #[must_use]
    pub fn deadline(&self, patience: TimeDelta) -> DateTime<Utc> {
        (self.start + self.period / 2).min(self.next) - patience
    }
}

fn label(slot: &Slot) -> String {
    format!("{:02}:{:02}", slot.hour, slot.minute)
}

const fn minute_of_day(slot: &Slot) -> i64 {
    (slot.hour as i64) * 60 + slot.minute as i64
}

impl Schedule {
    /// Refuses a table that is empty, unsorted, names a slot twice or outside a day, or whose
    /// period is not the spacing on either side of its slot or leaves `patience` no time.
    pub fn new(slots: &'static [Slot], patience: TimeDelta) -> Result<Self, ScheduleRefusal> {
        if slots.is_empty() {
            return Err(ScheduleRefusal::NoSlot);
        }
        if let Some(slot) = slots
            .iter()
            .find(|slot| slot.hour >= 24 || slot.minute >= 60)
        {
            return Err(ScheduleRefusal::NotATimeOfDay {
                hour: slot.hour,
                minute: slot.minute,
            });
        }
        for pair in slots.windows(2) {
            let [previous, slot] = pair else { continue };
            match minute_of_day(slot).cmp(&minute_of_day(previous)) {
                std::cmp::Ordering::Equal => {
                    return Err(ScheduleRefusal::DuplicateSlot { slot: label(slot) })
                }
                std::cmp::Ordering::Less => {
                    return Err(ScheduleRefusal::Unsorted {
                        slot: label(slot),
                        previous: label(previous),
                    })
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        let count = slots.len();
        for (index, slot) in slots.iter().enumerate() {
            let half = TimeDelta::minutes(slot.period_minutes) / 2;
            if half <= patience {
                return Err(ScheduleRefusal::PeriodTooShort {
                    slot: label(slot),
                    period_minutes: slot.period_minutes,
                });
            }
            let before = slots
                .get((index + count - 1) % count)
                .map_or(0, |prior| gap(minute_of_day(prior), minute_of_day(slot)));
            let after = slots
                .get((index + 1) % count)
                .map_or(0, |next| gap(minute_of_day(slot), minute_of_day(next)));
            if slot.period_minutes != before && slot.period_minutes != after {
                return Err(ScheduleRefusal::PeriodNotASpacing {
                    slot: label(slot),
                    period_minutes: slot.period_minutes,
                    before,
                    after,
                });
            }
        }
        Ok(Self { slots })
    }

    #[must_use]
    pub fn longest_period(&self) -> TimeDelta {
        let longest = self.slots.iter().map(|slot| slot.period_minutes).max();
        TimeDelta::minutes(longest.unwrap_or_default())
    }

    #[must_use]
    pub fn shortest_period(&self) -> TimeDelta {
        let shortest = self.slots.iter().map(|slot| slot.period_minutes).min();
        TimeDelta::minutes(shortest.unwrap_or_default())
    }

    /// The slot `now` belongs to: the latest slot start at or before it, the previous UTC day's
    /// last slot before the day's first one.
    #[must_use]
    pub fn slot_at(&self, now: DateTime<Utc>) -> Option<SlotAt> {
        let day = now.date_naive();
        let starts = |on: chrono::NaiveDate| {
            self.slots.iter().filter_map(move |slot| {
                let time = NaiveTime::from_hms_opt(slot.hour, slot.minute, 0)?;
                Some((on.and_time(time).and_utc(), slot))
            })
        };
        let yesterday = day.checked_sub_days(Days::new(1))?;
        let tomorrow = day.checked_add_days(Days::new(1))?;
        let all: Vec<(DateTime<Utc>, &Slot)> = starts(yesterday)
            .chain(starts(day))
            .chain(starts(tomorrow))
            .collect();
        let index = all.iter().rposition(|(start, _)| *start <= now)?;
        let (start, slot) = all.get(index)?;
        let (next, _) = all.get(index + 1)?;
        Some(SlotAt {
            start: *start,
            period: TimeDelta::minutes(slot.period_minutes),
            next: *next,
        })
    }
}

/// `SLOTS`, validated against the provider's patience.
pub fn committed(patience: TimeDelta) -> Result<Schedule, ScheduleRefusal> {
    Schedule::new(&SLOTS, patience)
}

/// Minutes from `from` to `to` going forward, across midnight when `to` is not later.
const fn gap(from: i64, to: i64) -> i64 {
    (to - from + MINUTES_A_DAY - 1).rem_euclid(MINUTES_A_DAY) + 1
}

#[cfg(test)]
mod tests {
    use super::{at, committed, Schedule, ScheduleRefusal, Slot, SlotAt, SLOTS};
    use chrono::{DateTime, TimeDelta, Utc};

    const PATIENCE: TimeDelta = TimeDelta::seconds(30);

    fn instant(text: &str) -> DateTime<Utc> {
        text.parse().unwrap_or(DateTime::UNIX_EPOCH)
    }

    fn refusal(slots: &'static [Slot]) -> Option<ScheduleRefusal> {
        Schedule::new(slots, PATIENCE).err()
    }

    #[test]
    fn the_committed_schedule_is_a_schedule() {
        assert_eq!(refusal(&SLOTS), None);
    }

    /// One table per refusal, each breaking only that rule.
    #[test]
    fn each_broken_table_is_refused_by_name() {
        const EMPTY: [Slot; 0] = [];
        const OFF_DAY: [Slot; 2] = [at(11, 0, 30), at(24, 0, 30)];
        const OFF_HOUR: [Slot; 2] = [at(11, 0, 30), at(11, 60, 30)];
        const TWICE: [Slot; 3] = [at(11, 0, 30), at(11, 0, 30), at(11, 30, 30)];
        const BACKWARDS: [Slot; 2] = [at(11, 30, 30), at(11, 0, 30)];
        const SHORT: [Slot; 2] = [at(11, 0, 1), at(11, 1, 1)];
        const NO_SPACING: [Slot; 3] = [at(11, 0, 30), at(11, 30, 45), at(12, 0, 30)];
        assert_eq!(
            [
                refusal(&EMPTY),
                refusal(&OFF_DAY),
                refusal(&OFF_HOUR),
                refusal(&TWICE),
                refusal(&BACKWARDS),
                refusal(&SHORT),
                refusal(&NO_SPACING),
            ],
            [
                Some(ScheduleRefusal::NoSlot),
                Some(ScheduleRefusal::NotATimeOfDay {
                    hour: 24,
                    minute: 0
                }),
                Some(ScheduleRefusal::NotATimeOfDay {
                    hour: 11,
                    minute: 60
                }),
                Some(ScheduleRefusal::DuplicateSlot {
                    slot: "11:00".to_owned()
                }),
                Some(ScheduleRefusal::Unsorted {
                    slot: "11:00".to_owned(),
                    previous: "11:30".to_owned()
                }),
                Some(ScheduleRefusal::PeriodTooShort {
                    slot: "11:00".to_owned(),
                    period_minutes: 1
                }),
                Some(ScheduleRefusal::PeriodNotASpacing {
                    slot: "11:30".to_owned(),
                    period_minutes: 45,
                    before: 30,
                    after: 30
                }),
            ]
        );
    }

    /// `(start, next, deadline)` of the committed slot `now` belongs to.
    fn slot(now: &str) -> Option<(String, String, String)> {
        let at = committed(PATIENCE).ok()?.slot_at(instant(now))?;
        let shown = |instant: DateTime<Utc>| instant.format("%d %H:%M:%S%.6f").to_string();
        Some((
            shown(at.start),
            shown(at.next),
            shown(at.deadline(PATIENCE)),
        ))
    }

    fn owned(start: &str, next: &str, deadline: &str) -> Option<(String, String, String)> {
        Some((start.to_owned(), next.to_owned(), deadline.to_owned()))
    }

    /// A slot start belongs to its own slot and 1 µs before it to the slot before; a 30-min slot
    /// stops at +14:30 and a 90-min one at +44:30.
    #[test]
    fn a_run_belongs_to_the_latest_slot_start_at_or_before_it() {
        assert_eq!(
            [
                slot("2026-11-17T12:00:00Z"),
                slot("2026-11-17T11:59:59.999999Z"),
                slot("2026-11-17T02:00:00Z"),
            ],
            [
                owned(
                    "17 12:00:00.000000",
                    "17 12:30:00.000000",
                    "17 12:14:30.000000"
                ),
                owned(
                    "17 11:30:00.000000",
                    "17 12:00:00.000000",
                    "17 11:44:30.000000"
                ),
                owned(
                    "17 02:00:00.000000",
                    "17 03:30:00.000000",
                    "17 02:44:30.000000"
                ),
            ]
        );
    }

    /// 23:30 is 60 min before 00:30 and still stops at +44:30, the day before's slot for a run
    /// at 00:10 or at midnight; 09:30 runs to 11:00 across the outside window's end.
    #[test]
    fn the_23_30_slot_and_the_09_30_slot_cross_their_gaps() {
        assert_eq!(
            [
                slot("2026-11-18T00:10:00Z"),
                slot("2026-11-18T00:00:00Z"),
                slot("2026-11-17T10:59:00Z"),
            ],
            [
                owned(
                    "17 23:30:00.000000",
                    "18 00:30:00.000000",
                    "18 00:14:30.000000"
                ),
                owned(
                    "17 23:30:00.000000",
                    "18 00:30:00.000000",
                    "18 00:14:30.000000"
                ),
                owned(
                    "17 09:30:00.000000",
                    "17 11:00:00.000000",
                    "17 10:14:30.000000"
                ),
            ]
        );
    }

    /// A run started more than half a period late still belongs to its slot, whose deadline has
    /// passed: `collect` then asks nothing and defers every request.
    #[test]
    fn a_run_more_than_half_a_period_late_gets_a_deadline_already_passed() {
        let late = instant("2026-11-17T11:20:00Z");
        let deadline = committed(PATIENCE)
            .ok()
            .and_then(|schedule| schedule.slot_at(late))
            .map(|at| at.deadline(PATIENCE));
        assert_eq!(deadline, Some(instant("2026-11-17T11:14:30Z")));
    }

    /// The next slot binds when half a period would pass it: 11:00's 90 min stop at 11:29:30.
    #[test]
    fn the_next_slot_caps_a_half_period_that_would_pass_it() {
        const NEAR: [Slot; 3] = [at(9, 30, 90), at(11, 0, 90), at(11, 30, 30)];
        let at_eleven = Schedule::new(&NEAR, PATIENCE)
            .ok()
            .and_then(|schedule| schedule.slot_at(instant("2026-11-17T11:00:00Z")));
        assert_eq!(
            at_eleven.map(|at: SlotAt| at.deadline(PATIENCE)),
            Some(instant("2026-11-17T11:29:30Z"))
        );
    }

    const STOPS: [&str; 31] = [
        "18 00:30:00 18 02:00:00 18 01:14:30",
        "18 02:00:00 18 03:30:00 18 02:44:30",
        "18 03:30:00 18 05:00:00 18 04:14:30",
        "18 05:00:00 18 06:30:00 18 05:44:30",
        "18 06:30:00 18 08:00:00 18 07:14:30",
        "18 08:00:00 18 09:30:00 18 08:44:30",
        "18 09:30:00 18 11:00:00 18 10:14:30",
        "18 11:00:00 18 11:30:00 18 11:14:30",
        "18 11:30:00 18 12:00:00 18 11:44:30",
        "18 12:00:00 18 12:30:00 18 12:14:30",
        "18 12:30:00 18 13:00:00 18 12:44:30",
        "18 13:00:00 18 13:30:00 18 13:14:30",
        "18 13:30:00 18 14:00:00 18 13:44:30",
        "18 14:00:00 18 14:30:00 18 14:14:30",
        "18 14:30:00 18 15:00:00 18 14:44:30",
        "18 15:00:00 18 15:30:00 18 15:14:30",
        "18 15:30:00 18 16:00:00 18 15:44:30",
        "18 16:00:00 18 16:30:00 18 16:14:30",
        "18 16:30:00 18 17:00:00 18 16:44:30",
        "18 17:00:00 18 17:30:00 18 17:14:30",
        "18 17:30:00 18 18:00:00 18 17:44:30",
        "18 18:00:00 18 18:30:00 18 18:14:30",
        "18 18:30:00 18 19:00:00 18 18:44:30",
        "18 19:00:00 18 19:30:00 18 19:14:30",
        "18 19:30:00 18 20:00:00 18 19:44:30",
        "18 20:00:00 18 20:30:00 18 20:14:30",
        "18 20:30:00 18 21:00:00 18 20:44:30",
        "18 21:00:00 18 21:30:00 18 21:14:30",
        "18 21:30:00 18 22:00:00 18 21:44:30",
        "18 22:00:00 18 23:30:00 18 22:14:30",
        "18 23:30:00 19 00:30:00 19 00:14:30",
    ];

    /// Architecture.md section 11.2's stop column: each committed slot run at its own start on
    /// 2026-11-18, as `start next stop`. A dropped, moved or re-periodised slot is red here.
    #[test]
    fn every_committed_slot_stops_where_section_11_2_says() {
        let shown = |instant: DateTime<Utc>| instant.format("%d %H:%M:%S").to_string();
        let replayed: Vec<String> = committed(PATIENCE)
            .ok()
            .map(|schedule| {
                SLOTS
                    .iter()
                    .filter_map(|slot| {
                        let start = format!("2026-11-18T{:02}:{:02}:00Z", slot.hour, slot.minute);
                        schedule.slot_at(instant(&start))
                    })
                    .map(|at| {
                        let stop = at.deadline(PATIENCE);
                        format!("{} {} {}", shown(at.start), shown(at.next), shown(stop))
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(replayed, STOPS);
    }

    /// The cross-check lead and the GNW frontier age are the committed schedule's periods, so a
    /// change to `SLOTS` that moves either is red here (architecture.md section 11.2).
    #[test]
    fn the_cross_check_lead_is_the_longest_period_and_the_frontier_age_the_shortest() {
        let periods = committed(PATIENCE)
            .map(|schedule| (schedule.longest_period(), schedule.shortest_period()));
        assert_eq!(
            periods,
            Ok((open_web_feed::LONGEST_POLL_PERIOD, TimeDelta::minutes(30)))
        );
    }
}
