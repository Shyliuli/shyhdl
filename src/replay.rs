//! Cursor movement over a recorded waveform: the replay half of the debugger.
//!
//! The engine owns a [`Waveform`] and a *reference point* sequence, and moves a
//! cursor along it:
//!
//! * [`StepMode::Clock`] - reference points are the edges of one named clock
//!   signal, detected from the recording itself (REQ-DBG-023, REQ-DBG-024).
//! * [`StepMode::Interval`] - reference points are the fixed grid
//!   `origin + n * period` (REQ-DBG-025).
//!
//! Two operations move the cursor, and they mean different things:
//!
//! * [`ReplayEngine::step`] moves exactly one reference point. Forward, it
//!   stops earlier if a watched signal changed in between, because stepping
//!   must not jump over an earlier watch hit (REQ-DBG-034). Backward it moves
//!   to the previous reference point and to nothing else (REQ-DBG-031).
//! * [`ReplayEngine::continue_run`] ignores reference points and stops at the
//!   earliest watched change, or at the end of the record. It only considers
//!   changes strictly ahead of the cursor, so signing up for a watch while
//!   sitting on a timestamp can never report that timestamp again, and
//!   repeating it can never stall in place (REQ-DBG-032).
//!
//! `watch` reads the recording, not a periodic sample: a signal that changed
//! at 13 and again at 14 is seen at both times even when the values sampled at
//! the surrounding reference points are identical (REQ-DBG-033, AC-MVP-029).
//!
//! # Sampling policy
//!
//! * Every value is read at one observation point: the last recorded
//!   timestamp at or before `Cursor::time`. That mapping is stored in
//!   `Cursor::index` when the cursor moves, so a caller that has not moved the
//!   cursor is reading the same sample it read before (REQ-DBG-057).
//! * A time with no recorded sample never gets an invented one; `index` is
//!   `None` when the cursor precedes the recording (REQ-DBG-036).
//! * Several changes inside one timestamp are merged into the value that
//!   timestamp ends with. The engine reports a timestamp once, with the final
//!   snapshot of every watched signal that changed in it, and never claims to
//!   replay the order of changes inside that timestamp (REQ-DBG-037,
//!   REQ-DBG-038).
//! * The value a signal was first recorded with counts as a watch hit with no
//!   `previous` value. Moving forward from the initial cursor, which sits at
//!   the first recorded timestamp, never reports it; moving backwards past it
//!   does.
//!
//! Nothing here re-runs a simulation, and nothing here writes (REQ-DBG-001,
//! REQ-DBG-011).
//!
//! # Where the design metadata fits
//!
//! These two layers only know waveform paths and [`crate::waveform::SignalId`]s.
//! Mapping a source variable to the paths it occupies, laying out a `struct`,
//! evaluating a conditional breakpoint at a stop, and checking that the
//! recording and the symbols come from the same build all belong to the
//! debugger that sits on top. A watch on a variable that spans several signals
//! is therefore a watch on all of those signals, registered here in one call.

use crate::waveform::{BitState, SignalId, Value, WaveError, WaveResult, Waveform};

/// Which recorded edge of a clock counts as a reference point.
///
/// The variants follow Verilog's edge definitions, not just the two-state
/// ones; the complete transition table is on the edge detector below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    /// Verilog `posedge`: `0` -> `1`, `0` -> `x`/`z`, or `x`/`z` -> `1`.
    Pos,
    /// Verilog `negedge`: `1` -> `0`, `1` -> `x`/`z`, or `x`/`z` -> `0`.
    Neg,
    /// Either edge, which also covers `0` -> `x` and `x` -> `0`.
    Both,
}

/// Where the reference points of a replay come from.
///
/// The mode is always explicit. Multiple clocks or a missing clock are the
/// caller's problem to resolve; this layer never picks a default clock for it
/// (REQ-DBG-020, REQ-DBG-021, REQ-DBG-026).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepMode {
    /// Reference points are the edges of one named clock signal.
    Clock {
        /// Full path of the clock signal in the waveform.
        signal: String,
        /// Edge that counts as a step.
        edge: Edge,
    },
    /// Reference points are `origin + n * period`, in the waveform's own time
    /// units, for `n >= 0` and only inside the recorded range.
    Interval {
        /// First grid point; also the grid origin.
        origin: u64,
        /// Distance between grid points, at least one tick.
        period: u64,
    },
}

/// Direction of a cursor movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Towards later times.
    Forward,
    /// Towards earlier times.
    Backward,
}

/// Position in the recording.
///
/// `index` is the recorded observation point the cursor reads from: the last
/// one at or before `time`. It is `None` when `time` precedes the first
/// recorded observation, in which case there is no sample to read and the
/// caller must say so instead of inventing one (REQ-DBG-036).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cursor {
    /// Time of the cursor, in the waveform's own time units.
    pub time: u64,
    /// Recorded observation point at or before `time`.
    pub index: Option<usize>,
}

impl Cursor {
    /// The cursor as a pair of time and observation point index.
    pub fn as_pair(&self) -> (u64, Option<usize>) {
        (self.time, self.index)
    }
}

/// One watched signal that changed at the reported timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchHit {
    /// Signal that changed.
    pub signal: SignalId,
    /// Path of the signal, for reporting.
    pub path: String,
    /// Timestamp of the change.
    pub time: u64,
    /// Value before the change; `None` for the first recorded value.
    pub previous: Option<Value>,
    /// Value the timestamp ends with.
    pub value: Value,
}

/// Why the cursor stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// A watched signal changed. All watched signals that changed at this
    /// timestamp are reported together, as one snapshot.
    Watch(Vec<WatchHit>),
    /// The cursor reached the next (or previous) reference point, and no
    /// earlier watched change was in the way.
    ReferencePoint,
    /// There is no further observation value: the cursor is at the end of the
    /// recording. This is not a failure (REQ-DBG-012).
    End,
    /// There is no earlier observation value: the cursor is at the start of
    /// the recording.
    Start,
}

/// Result of a movement.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// Cursor after the movement.
    pub cursor: Cursor,
    /// Why it stopped there.
    pub reason: StopReason,
}

impl StepOutcome {
    /// True when the stop was caused by a watched signal changing.
    pub fn is_watch_hit(&self) -> bool {
        matches!(self.reason, StopReason::Watch(_))
    }
}

/// A watched signal and its decoded change list.
#[derive(Debug)]
struct WatchedSignal {
    id: SignalId,
    path: String,
    changes: Vec<(u64, Value)>,
}

/// How the reference points of this engine are produced.
#[derive(Debug)]
enum ReferencePlan {
    Clock {
        path: String,
        edge: Edge,
        /// Times at which the selected edge occurs, ascending.
        edges: Vec<u64>,
    },
    Interval {
        origin: u64,
        period: u64,
    },
}

/// Moves a cursor over a recording.
///
/// Owns the [`Waveform`]; nothing is written back to it.
#[derive(Debug)]
pub struct ReplayEngine {
    waveform: Waveform,
    mode: StepMode,
    plan: ReferencePlan,
    cursor: Cursor,
    watch: Vec<WatchedSignal>,
}

impl ReplayEngine {
    /// Builds an engine for a recording and a step mode.
    ///
    /// Fails when the mode cannot be honoured: an unknown or non-scalar clock
    /// signal, or a zero interval.
    pub fn new(waveform: Waveform, mode: StepMode) -> WaveResult<ReplayEngine> {
        let plan = match &mode {
            StepMode::Clock { signal, edge } => {
                let id = waveform.signal(signal)?;
                let width = waveform.width(id)?;
                if width != 1 {
                    return Err(WaveError::WidthMismatch {
                        path: signal.clone(),
                        width,
                        expected: "a 1-bit clock signal",
                    });
                }
                let path = waveform.path_of(id)?.to_string();
                let changes = waveform.change_records(id)?;
                let edges = clock_edges(&changes, &path, *edge)?;
                ReferencePlan::Clock {
                    path,
                    edge: *edge,
                    edges,
                }
            }
            StepMode::Interval { origin, period } => {
                if *period == 0 {
                    return Err(WaveError::InvalidMode {
                        message: "the interval period must be at least one tick".to_string(),
                    });
                }
                ReferencePlan::Interval {
                    origin: *origin,
                    period: *period,
                }
            }
        };

        let cursor = Cursor {
            time: waveform.first_time(),
            index: Some(0),
        };
        Ok(ReplayEngine {
            waveform,
            mode,
            plan,
            cursor,
            watch: Vec::new(),
        })
    }

    /// The recording being replayed.
    pub fn waveform(&self) -> &Waveform {
        &self.waveform
    }

    /// The configured step mode.
    pub fn mode(&self) -> &StepMode {
        &self.mode
    }

    /// Current cursor.
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Moves the cursor to `time` without moving it to a reference point.
    ///
    /// The cursor reads the last observation point at or before `time`; no
    /// sample is created for a time the recording does not contain
    /// (REQ-DBG-036).
    pub fn seek(&mut self, time: u64) -> Cursor {
        self.cursor = self.cursor_at(time);
        self.cursor
    }

    /// Moves the cursor to a recorded observation point by index.
    ///
    /// An index outside the recording is an explicit error; the cursor is left
    /// where it was.
    pub fn seek_index(&mut self, index: usize) -> WaveResult<Cursor> {
        let time =
            self.waveform
                .observation_time(index)
                .ok_or(WaveError::ObservationOutOfRange {
                    index,
                    count: self.waveform.observation_count(),
                })?;
        self.cursor = Cursor {
            time,
            index: Some(index),
        };
        Ok(self.cursor)
    }

    /// Returns the cursor to the start of the record.
    pub fn reset(&mut self) -> Cursor {
        self.cursor = Cursor {
            time: self.waveform.first_time(),
            index: Some(0),
        };
        self.cursor
    }

    /// Adds signals to the watch set.
    ///
    /// Watching is cumulative and idempotent. A signal that is not recorded is
    /// an error (REQ-DBG-005).
    pub fn watch(&mut self, signals: &[SignalId]) -> WaveResult<()> {
        for id in signals {
            if self.watch.iter().any(|watched| watched.id == *id) {
                continue;
            }
            let path = self.waveform.path_of(*id)?.to_string();
            let changes = self.waveform.change_records(*id)?;
            self.watch.push(WatchedSignal {
                id: *id,
                path,
                changes,
            });
        }
        Ok(())
    }

    /// Stops watching one signal.
    pub fn unwatch(&mut self, id: SignalId) {
        self.watch.retain(|watched| watched.id != id);
    }

    /// Drops every watched signal.
    pub fn clear_watch(&mut self) {
        self.watch.clear();
    }

    /// The watched signals, in the order they were added.
    pub fn watched(&self) -> Vec<SignalId> {
        self.watch.iter().map(|watched| watched.id).collect()
    }

    /// True when `id` is watched.
    pub fn is_watched(&self, id: SignalId) -> bool {
        self.watch.iter().any(|watched| watched.id == id)
    }

    /// The clock this replay steps on, when the mode is [`StepMode::Clock`].
    pub fn clock(&self) -> Option<(&str, Edge)> {
        match &self.plan {
            ReferencePlan::Clock { path, edge, .. } => Some((path.as_str(), *edge)),
            ReferencePlan::Interval { .. } => None,
        }
    }

    /// First and last reference point of this replay, if it has any.
    ///
    /// `None` means the mode produced no reference point inside the record,
    /// for example a clock that is gated off from the first recorded time.
    pub fn reference_bounds(&self) -> Option<(u64, u64)> {
        match &self.plan {
            ReferencePlan::Clock { edges, .. } => Some((*edges.first()?, *edges.last()?)),
            ReferencePlan::Interval { origin, period } => {
                let first = self.waveform.first_time();
                let last = self.waveform.last_time();
                let lo = (*origin).max(first);
                if lo > last {
                    return None;
                }
                let start = grid_ceil(lo, *origin, *period)?;
                if start > last {
                    return None;
                }
                let end = grid_floor(last, *origin, *period)?;
                if end < first || end < start {
                    return None;
                }
                Some((start, end))
            }
        }
    }

    /// The reference point adjacent to `from`, in `direction`.
    ///
    /// This is the primitive behind `step`; it is public so a caller can build
    /// its own navigation (for example a named-cycle breakpoint) without
    /// duplicating edge or grid arithmetic here.
    pub fn next_reference_time(&self, from: u64, direction: Direction) -> Option<u64> {
        match &self.plan {
            ReferencePlan::Clock { edges, .. } => match direction {
                Direction::Forward => {
                    let position = edges.partition_point(|time| *time <= from);
                    edges.get(position).copied()
                }
                Direction::Backward => {
                    let position = edges.partition_point(|time| *time < from);
                    position.checked_sub(1).and_then(|i| edges.get(i).copied())
                }
            },
            ReferencePlan::Interval { origin, period } => match direction {
                Direction::Forward => {
                    let first = self.waveform.first_time();
                    let last = self.waveform.last_time();
                    let lo = from.checked_add(1)?.max(first);
                    if lo > last {
                        return None;
                    }
                    let candidate = grid_ceil(lo, *origin, *period)?;
                    (candidate <= last).then_some(candidate)
                }
                Direction::Backward => {
                    let first = self.waveform.first_time();
                    let hi = from.checked_sub(1)?.min(self.waveform.last_time());
                    if hi < first {
                        return None;
                    }
                    let candidate = grid_floor(hi, *origin, *period)?;
                    (candidate >= first).then_some(candidate)
                }
            },
        }
    }

    /// Moves the cursor one reference point.
    ///
    /// Forward, a watched change strictly between the cursor and the next
    /// reference point stops the movement first, and a watched change exactly
    /// at the next reference point is reported as a watch hit rather than as a
    /// plain reference point (REQ-DBG-034). Backward, the cursor goes to the
    /// previous reference point and nowhere else (REQ-DBG-031).
    pub fn step(&mut self, direction: Direction) -> StepOutcome {
        match direction {
            Direction::Forward => {
                let next_reference = self.next_reference_time(self.cursor.time, Direction::Forward);
                let next_hit = self.next_watch_time(self.cursor.time);
                match (next_reference, next_hit) {
                    (Some(reference), Some(hit)) if hit <= reference => self.stop_at_watch(hit),
                    (Some(reference), _) => self.stop_at_reference(reference),
                    (None, Some(hit)) => self.stop_at_watch(hit),
                    (None, None) => self.stop_at_end(),
                }
            }
            Direction::Backward => {
                match self.next_reference_time(self.cursor.time, Direction::Backward) {
                    Some(reference) => self.stop_at_reference(reference),
                    None => self.stop_at_start(),
                }
            }
        }
    }

    /// Runs to the next watched change, ignoring reference points.
    ///
    /// Forward, only changes strictly later than the cursor count: a change
    /// that is already at the cursor's timestamp has passed, and registering a
    /// watch while sitting on a timestamp must not report that timestamp
    /// again. Because the search is strictly ahead of the cursor, a repeated
    /// `continue` always advances and can never stop in place
    /// (REQ-DBG-032, REQ-DBG-039). With nothing left to hit, the cursor goes
    /// to the end of the record (`End`) or, backwards, to its start (`Start`).
    pub fn continue_run(&mut self, direction: Direction) -> StepOutcome {
        match direction {
            Direction::Forward => match self.next_watch_time(self.cursor.time) {
                Some(hit) => self.stop_at_watch(hit),
                None => self.stop_at_end(),
            },
            Direction::Backward => match self.prev_watch_time(self.cursor.time) {
                Some(hit) => self.stop_at_watch(hit),
                None => self.stop_at_start(),
            },
        }
    }

    /// Value of one signal at the current observation point.
    pub fn value(&self, id: SignalId) -> WaveResult<Option<Value>> {
        self.waveform.value_at(id, self.cursor.time)
    }

    /// Values of several signals, all read at the same observation point
    /// (REQ-DBG-057).
    pub fn snapshot(&self, signals: &[SignalId]) -> WaveResult<Vec<(SignalId, Option<Value>)>> {
        signals
            .iter()
            .map(|id| Ok((*id, self.value(*id)?)))
            .collect()
    }

    /// Where the cursor's observation point is, as time and index.
    fn cursor_at(&self, time: u64) -> Cursor {
        Cursor {
            time,
            index: self.waveform.observation_index(time),
        }
    }

    fn stop_at_reference(&mut self, time: u64) -> StepOutcome {
        self.cursor = self.cursor_at(time);
        StepOutcome {
            cursor: self.cursor,
            reason: StopReason::ReferencePoint,
        }
    }

    fn stop_at_watch(&mut self, time: u64) -> StepOutcome {
        let hits = self.collect_hits(time);
        self.cursor = self.cursor_at(time);
        StepOutcome {
            cursor: self.cursor,
            reason: StopReason::Watch(hits),
        }
    }

    fn stop_at_end(&mut self) -> StepOutcome {
        let index = self.waveform.observation_count() - 1;
        self.cursor = Cursor {
            time: self.waveform.last_time(),
            index: Some(index),
        };
        StepOutcome {
            cursor: self.cursor,
            reason: StopReason::End,
        }
    }

    fn stop_at_start(&mut self) -> StepOutcome {
        self.cursor = Cursor {
            time: self.waveform.first_time(),
            index: Some(0),
        };
        StepOutcome {
            cursor: self.cursor,
            reason: StopReason::Start,
        }
    }

    /// Every watched signal that changed exactly at `time`, as one snapshot.
    fn collect_hits(&self, time: u64) -> Vec<WatchHit> {
        let mut hits = Vec::new();
        for watched in &self.watch {
            let Ok(position) = watched
                .changes
                .binary_search_by_key(&time, |(change, _)| *change)
            else {
                continue;
            };
            hits.push(WatchHit {
                signal: watched.id,
                path: watched.path.clone(),
                time,
                previous: position
                    .checked_sub(1)
                    .map(|before| watched.changes[before].1.clone()),
                value: watched.changes[position].1.clone(),
            });
        }
        hits
    }

    /// Earliest watched change strictly after `after`.
    fn next_watch_time(&self, after: u64) -> Option<u64> {
        let mut best: Option<u64> = None;
        for watched in &self.watch {
            let position = watched.changes.partition_point(|(time, _)| *time <= after);
            if let Some((time, _)) = watched.changes.get(position) {
                best = Some(best.map_or(*time, |current| current.min(*time)));
            }
        }
        best
    }

    /// Latest watched change strictly before `before`.
    fn prev_watch_time(&self, before: u64) -> Option<u64> {
        let mut best: Option<u64> = None;
        for watched in &self.watch {
            let position = watched.changes.partition_point(|(time, _)| *time < before);
            if let Some((time, _)) = position.checked_sub(1).and_then(|i| watched.changes.get(i)) {
                best = Some(best.map_or(*time, |current| current.max(*time)));
            }
        }
        best
    }
}

/// Times at which the selected edge of a 1-bit clock occurs.
///
/// The first recorded value establishes the initial level and is not an edge:
/// a transition can only be observed between two recorded values (REQ-DBG-024).
///
/// The edge table is the complete Verilog one, so a four-state recording does
/// not silently lose edges:
///
/// | transition | `Pos` | `Neg` |
/// | --- | --- | --- |
/// | `0 -> 1`, `0 -> x`, `0 -> z` | yes | |
/// | `x`/`z` -> `1` | yes | |
/// | `1 -> 0`, `1 -> x`, `1 -> z` | | yes |
/// | `x`/`z` -> `0` | | yes |
///
/// `Both` is the union of the two, so it also covers `0 -> x` and `x -> 0`.
/// The nine-state values `h`, `l`, `u`, `w` and `-` have no Verilog edge
/// meaning and are treated as unknown, never as `0` or `1`.
fn clock_edges(changes: &[(u64, Value)], path: &str, edge: Edge) -> WaveResult<Vec<u64>> {
    let mut edges = Vec::new();
    let mut previous: Option<BitState> = None;
    for (time, value) in changes {
        let current = value.as_bit(path)?;
        if let Some(before) = previous {
            let rising = (before == BitState::Zero && current != BitState::Zero)
                || (before.is_unknown() && current == BitState::One);
            let falling = (before == BitState::One && current != BitState::One)
                || (before.is_unknown() && current == BitState::Zero);
            let selected = match edge {
                Edge::Pos => rising,
                Edge::Neg => falling,
                Edge::Both => rising || falling,
            };
            if selected {
                edges.push(*time);
            }
        }
        previous = Some(current);
    }
    Ok(edges)
}

/// Smallest grid point at or after `at`, or `None` when it would overflow.
fn grid_ceil(at: u64, origin: u64, period: u64) -> Option<u64> {
    if at <= origin {
        return Some(origin);
    }
    let offset = at - origin;
    let steps = offset.checked_add(period - 1)? / period;
    origin.checked_add(steps.checked_mul(period)?)
}

/// Largest grid point at or before `at`, or `None` when `at` precedes the
/// grid origin.
fn grid_floor(at: u64, origin: u64, period: u64) -> Option<u64> {
    if at < origin {
        return None;
    }
    let steps = (at - origin) / period;
    origin.checked_add(steps.checked_mul(period)?)
}
