//! Read-only access to a recorded waveform: which signals were recorded and at
//! what width, which observation points exist, and what value a signal had at a
//! given time. It never moves a cursor; that is [`crate::replay`].
//!
//! # Contract
//!
//! * **X and Z are values, not zeros.** A recorded `x` or `z` stays `x` or
//!   `z`; converting it to a number is an error ([`WaveError::NotTwoState`]).
//! * **Widths are unbounded.** Values are bit vectors, not `u64`. Numeric
//!   accessors fail on wide or unknown values instead of truncating.
//! * **Missing data is explicit.** An unrecorded signal is
//!   [`WaveError::SignalNotFound`]; a signal with no recorded value at or
//!   before a time yields `Ok(None)`. Neither is ever reported as `0`.
//! * **One timestamp, one snapshot.** Changes inside one timestamp collapse to
//!   the value it ends with ([`Waveform::changes`]); intra-timestamp deltas
//!   are not claimed.
//! * **Times are integers.** Durations are integer arithmetic in zeptoseconds,
//!   zeptoseconds, with no floating point.
//!
//! VCD/FST parsing is done by `wellen`, which handles aliases, nested scopes,
//! vectors of any width, timescales and both file formats.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use wellen::{SignalRef, SignalValueRef};

/// Result of a waveform operation.
pub type WaveResult<T> = std::result::Result<T, WaveError>;

/// Everything that can go wrong while reading a recording.
///
/// Every variant names the signal, path or duration that caused it, so a
/// failure can be diagnosed without a second run.
#[derive(Debug, thiserror::Error)]
pub enum WaveError {
    /// The recording could not be opened.
    #[error("failed to open waveform {path}: {source}")]
    Io {
        /// Path that was being opened.
        path: PathBuf,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },

    /// The recording exists but could not be parsed.
    #[error("failed to parse waveform {path}: {message}")]
    Parse {
        /// Path of the recording.
        path: PathBuf,
        /// Message from the waveform backend.
        message: String,
    },

    /// The recording contains no observation points, so nothing can be
    /// replayed.
    #[error("waveform {path} contains no observation points")]
    NoObservationPoints {
        /// Path of the recording.
        path: PathBuf,
    },

    /// No variable with this path was recorded.
    #[error("signal '{path}' is not recorded in this waveform")]
    SignalNotFound {
        /// Path that was looked up.
        path: String,
    },

    /// A lookup matched more than one recorded variable.
    #[error("signal path '{path}' is ambiguous, it matches {matches:?}")]
    AmbiguousSignal {
        /// Path that was looked up.
        path: String,
        /// Full paths that matched.
        matches: Vec<String>,
    },

    /// A signal identifier that this waveform does not know.
    #[error("signal id {id} does not belong to this waveform")]
    UnknownSignalId {
        /// The identifier that was used.
        id: u32,
    },

    /// An observation point index outside the recording.
    #[error(
        "observation point {index} is out of range, the recording has {count} observation points"
    )]
    ObservationOutOfRange {
        /// The index that was requested.
        index: usize,
        /// Number of recorded observation points.
        count: usize,
    },

    /// A scalar signal was required and the signal has a different width.
    #[error("signal '{path}' is {width} bits wide, {expected}")]
    WidthMismatch {
        /// Path of the signal.
        path: String,
        /// Actual width.
        width: u32,
        /// What the caller needed, e.g. "a 1-bit clock".
        expected: &'static str,
    },

    /// The value has `x`/`z` bits and cannot be read as a number.
    #[error("value of '{path}' is not two-state: {bits}")]
    NotTwoState {
        /// Path of the signal.
        path: String,
        /// The offending value, most significant bit first.
        bits: String,
    },

    /// A numeric accessor was used on a value that does not fit the target
    /// type.
    #[error("value of '{path}' does not fit in {target}: {bits}")]
    ValueTooWide {
        /// Path of the signal.
        path: String,
        /// The requested type, e.g. "u64".
        target: &'static str,
        /// The offending value, most significant bit first.
        bits: String,
    },

    /// A duration was not written as an integer with a time unit.
    #[error("'{text}' is not an integer duration: {reason}")]
    BadDuration {
        /// The text that was parsed.
        text: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A duration or timescale is too large to be represented exactly.
    #[error("time value '{text}' overflows exact integer arithmetic")]
    TimeOverflow {
        /// The text or value that overflowed.
        text: String,
    },

    /// The waveform has no `$timescale`, so physical durations cannot be
    /// converted to its time unit.
    #[error("waveform {path} has no timescale, durations cannot be converted")]
    MissingTimescale {
        /// Path of the recording.
        path: PathBuf,
    },

    /// A duration is not an exact multiple of the waveform time precision.
    #[error(
        "duration {duration} is not a multiple of the waveform timescale {timescale} \
         (remainder {remainder_zs} zs)"
    )]
    IndivisibleDuration {
        /// The duration that was requested.
        duration: Timescale,
        /// The timescale of the recording.
        timescale: Timescale,
        /// Remainder in zeptoseconds.
        remainder_zs: u128,
    },

    /// A step mode described at request is impossible for this recording.
    #[error("invalid step mode: {message}")]
    InvalidMode {
        /// What is wrong.
        message: String,
    },
}

/// A power-of-ten time unit, kept so that durations round-trip exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimeUnit {
    /// 10^-21 s.
    Zepto,
    /// 10^-18 s.
    Atto,
    /// 10^-15 s.
    Femto,
    /// 10^-12 s.
    Pico,
    /// 10^-9 s.
    Nano,
    /// 10^-6 s.
    Micro,
    /// 10^-3 s.
    Milli,
    /// 1 s.
    Second,
}

impl TimeUnit {
    /// Power of ten relative to one second.
    pub const fn exponent(self) -> i32 {
        match self {
            TimeUnit::Zepto => -21,
            TimeUnit::Atto => -18,
            TimeUnit::Femto => -15,
            TimeUnit::Pico => -12,
            TimeUnit::Nano => -9,
            TimeUnit::Micro => -6,
            TimeUnit::Milli => -3,
            TimeUnit::Second => 0,
        }
    }

    /// Power of ten relative to one zeptosecond, always non-negative so that
    /// conversion stays in integers.
    const fn zepto_exponent(self) -> u32 {
        (self.exponent() + 21) as u32
    }

    /// Canonical spelling used when printing.
    pub const fn symbol(self) -> &'static str {
        match self {
            TimeUnit::Zepto => "zs",
            TimeUnit::Atto => "as",
            TimeUnit::Femto => "fs",
            TimeUnit::Pico => "ps",
            TimeUnit::Nano => "ns",
            TimeUnit::Micro => "us",
            TimeUnit::Milli => "ms",
            TimeUnit::Second => "s",
        }
    }

    /// Parses a unit symbol. `µs` and `μs` are accepted for microseconds.
    fn parse(text: &str) -> Option<TimeUnit> {
        match text.to_ascii_lowercase().as_str() {
            "zs" => Some(TimeUnit::Zepto),
            "as" => Some(TimeUnit::Atto),
            "fs" => Some(TimeUnit::Femto),
            "ps" => Some(TimeUnit::Pico),
            "ns" => Some(TimeUnit::Nano),
            "us" | "µs" | "μs" => Some(TimeUnit::Micro),
            "ms" => Some(TimeUnit::Milli),
            "s" => Some(TimeUnit::Second),
            _ => None,
        }
    }

    /// Maps a `wellen` timescale unit onto this representation.
    fn from_wellen(unit: wellen::TimescaleUnit) -> Option<TimeUnit> {
        match unit {
            wellen::TimescaleUnit::ZeptoSeconds => Some(TimeUnit::Zepto),
            wellen::TimescaleUnit::AttoSeconds => Some(TimeUnit::Atto),
            wellen::TimescaleUnit::FemtoSeconds => Some(TimeUnit::Femto),
            wellen::TimescaleUnit::PicoSeconds => Some(TimeUnit::Pico),
            wellen::TimescaleUnit::NanoSeconds => Some(TimeUnit::Nano),
            wellen::TimescaleUnit::MicroSeconds => Some(TimeUnit::Micro),
            wellen::TimescaleUnit::MilliSeconds => Some(TimeUnit::Milli),
            wellen::TimescaleUnit::Seconds => Some(TimeUnit::Second),
            _ => None,
        }
    }
}

impl std::fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.symbol())
    }
}

/// A time scale, e.g. the `1ns` of `$timescale 1ns $end`.
///
/// Stored as a factor and a unit rather than a bare number so that
/// `10ns` never becomes `10` or `10000` by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timescale {
    factor: u64,
    unit: TimeUnit,
}

impl Timescale {
    /// Builds a timescale from an integer factor and a unit.
    ///
    /// The factor must be positive; any other value is rejected by
    /// [`Timescale::parse`] and by the waveform backends.
    pub const fn new(factor: u64, unit: TimeUnit) -> Self {
        Timescale { factor, unit }
    }

    /// The integer factor, e.g. `10` for `10ns`.
    pub const fn factor(self) -> u64 {
        self.factor
    }

    /// The unit, e.g. [`TimeUnit::Nano`] for `10ns`.
    pub const fn unit(self) -> TimeUnit {
        self.unit
    }

    /// Exact size of this timescale in zeptoseconds (10^-21 s).
    ///
    /// `u128` rather than `u64` so that a one-second timescale, which is
    /// 10^21 zeptoseconds, still has an exact integer representation.
    pub fn zeptoseconds(self) -> WaveResult<u128> {
        let text = self.to_string();
        if self.factor == 0 {
            return Err(WaveError::BadDuration {
                text,
                reason: "the factor must be positive".to_string(),
            });
        }
        let decade = 10u128
            .checked_pow(self.unit.zepto_exponent())
            .ok_or_else(|| WaveError::TimeOverflow { text: text.clone() })?;
        (self.factor as u128)
            .checked_mul(decade)
            .ok_or(WaveError::TimeOverflow { text })
    }

    /// Parses an integer duration such as `10ns`, `1 ps`, `100us` or `2s`.
    ///
    /// Only unsigned integers and the units `zs` through `s` are accepted;
    /// fractional values are rejected instead of being rounded.
    pub fn parse(text: &str) -> WaveResult<Timescale> {
        let trimmed = text.trim();
        let digits_end = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (digits, unit_text) = trimmed.split_at(digits_end);
        let bad = |reason: &str| WaveError::BadDuration {
            text: text.to_string(),
            reason: reason.to_string(),
        };

        if digits.is_empty() {
            return Err(bad("it does not start with a decimal integer"));
        }
        let unit_text = unit_text.trim();
        if unit_text.is_empty() {
            return Err(bad(
                "it has no time unit, expected one of zs/as/fs/ps/ns/us/ms/s",
            ));
        }
        let unit = TimeUnit::parse(unit_text)
            .ok_or_else(|| bad("unknown time unit, expected one of zs/as/fs/ps/ns/us/ms/s"))?;
        let factor: u64 = digits
            .parse()
            .map_err(|_| bad("the factor does not fit in 64 bits"))?;
        if factor == 0 {
            return Err(bad("the factor must be positive"));
        }
        let scale = Timescale { factor, unit };
        scale.zeptoseconds()?;
        Ok(scale)
    }
}

impl std::fmt::Display for Timescale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.factor, self.unit)
    }
}

/// One bit of a recorded value.
///
/// The four-state VCD states are represented directly; the extra nine-state
/// characters that some dumpers emit are preserved in [`BitState::Other`]
/// instead of being coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitState {
    /// Logic zero.
    Zero,
    /// Logic one.
    One,
    /// Unknown.
    X,
    /// High impedance.
    Z,
    /// Another recorded state (`h`, `l`, `u`, `w` or `-`).
    Other(char),
}

impl BitState {
    /// Converts the ASCII form used by VCD and by `wellen`.
    pub fn from_ascii(c: char) -> BitState {
        match c {
            '0' => BitState::Zero,
            '1' => BitState::One,
            'x' | 'X' => BitState::X,
            'z' | 'Z' => BitState::Z,
            other => BitState::Other(other),
        }
    }

    /// The ASCII form as it appears in a VCD file.
    pub const fn as_ascii(self) -> char {
        match self {
            BitState::Zero => '0',
            BitState::One => '1',
            BitState::X => 'x',
            BitState::Z => 'z',
            BitState::Other(c) => c,
        }
    }

    /// True only for `0` and `1`.
    pub const fn is_known(self) -> bool {
        matches!(self, BitState::Zero | BitState::One)
    }

    /// True for `x` and for the nine-state values that carry no binary value.
    pub const fn is_unknown(self) -> bool {
        !self.is_known()
    }
}

/// A bit vector of any width, least significant bit first.
///
/// Deliberately not an integer: a 44-bit `struct` value, or a value holding
/// `x`/`z`, is representable here and is never silently truncated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BitVec {
    bits: Vec<BitState>,
}

impl BitVec {
    /// Builds a bit vector from bits given least significant bit first.
    pub fn from_lsb_first(bits: Vec<BitState>) -> BitVec {
        BitVec { bits }
    }

    /// Builds a bit vector from an ASCII string given most significant bit
    /// first, the way VCD writes it.
    pub fn from_msb_string(text: &str) -> BitVec {
        let bits = text.chars().rev().map(BitState::from_ascii).collect();
        BitVec { bits }
    }

    /// Number of bits.
    pub fn width(&self) -> u32 {
        self.bits.len() as u32
    }

    /// Bits, least significant bit first.
    pub fn bits(&self) -> &[BitState] {
        &self.bits
    }

    /// Bit at `index`, where 0 is the least significant bit.
    pub fn bit(&self, index: u32) -> Option<BitState> {
        self.bits.get(index as usize).copied()
    }

    /// The value as ASCII, most significant bit first.
    pub fn to_msb_string(&self) -> String {
        self.bits.iter().rev().map(|b| b.as_ascii()).collect()
    }

    /// True when every bit is `0` or `1`.
    pub fn is_two_state(&self) -> bool {
        self.bits.iter().all(|b| b.is_known())
    }

    /// Checks that the value is exactly `width` bits wide and two-state.
    fn require_two_state(&self, path: &str, width: Option<u32>) -> WaveResult<()> {
        if let Some(expected) = width {
            if self.width() != expected {
                return Err(WaveError::ValueTooWide {
                    path: path.to_string(),
                    target: match expected {
                        1 => "a single bit",
                        _ => "the target type",
                    },
                    bits: self.to_msb_string(),
                });
            }
        }
        if !self.is_two_state() {
            return Err(WaveError::NotTwoState {
                path: path.to_string(),
                bits: self.to_msb_string(),
            });
        }
        Ok(())
    }

    /// The value as a `u64`.
    ///
    /// A wide vector is fine as long as the value itself fits: converting the
    /// 72-bit value `3` to `3` is exact, not a truncation. The conversion
    /// fails on `x`/`z` bits and on any bit above bit 63 that is set.
    pub fn as_u64(&self, path: &str) -> WaveResult<u64> {
        self.require_two_state(path, None)?;
        if self.bits.len() > 64 && self.bits[64..].contains(&BitState::One) {
            return Err(WaveError::ValueTooWide {
                path: path.to_string(),
                target: "u64",
                bits: self.to_msb_string(),
            });
        }
        let mut out = 0u64;
        for (i, bit) in self.bits.iter().take(64).enumerate() {
            if *bit == BitState::One {
                out |= 1u64 << i;
            }
        }
        Ok(out)
    }
}

impl std::fmt::Display for BitVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_msb_string())
    }
}

/// A recorded value.
///
/// Kept as a bit vector rather than an integer so that wide values and `x`/`z`
/// survive the round trip from the recording to the debugger.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A bit vector of any width.
    Bits(BitVec),
    /// A real value, as recorded.
    Real(f64),
    /// A string value, as recorded.
    Text(String),
    /// An event, which carries no value.
    Event,
}

impl Value {
    /// The value as a bit vector, when it is one.
    pub fn as_bits(&self) -> Option<&BitVec> {
        match self {
            Value::Bits(bits) => Some(bits),
            _ => None,
        }
    }

    /// The width in bits, if this is a bit vector.
    pub fn width(&self) -> Option<u32> {
        self.as_bits().map(BitVec::width)
    }

    /// The single bit of a 1-bit value.
    pub fn as_bit(&self, path: &str) -> WaveResult<BitState> {
        let bits = match self {
            Value::Bits(bits) => bits,
            _ => {
                return Err(WaveError::WidthMismatch {
                    path: path.to_string(),
                    width: 0,
                    expected: "a 1-bit logic value",
                });
            }
        };
        if bits.width() != 1 {
            return Err(WaveError::WidthMismatch {
                path: path.to_string(),
                width: bits.width(),
                expected: "a 1-bit logic value",
            });
        }
        Ok(bits.bits()[0])
    }

    /// The value as a boolean; `x`, `z` and widths other than 1 are errors.
    pub fn as_bool(&self, path: &str) -> WaveResult<bool> {
        match self.as_bit(path)? {
            BitState::Zero => Ok(false),
            BitState::One => Ok(true),
            other => Err(WaveError::NotTwoState {
                path: path.to_string(),
                bits: other.as_ascii().to_string(),
            }),
        }
    }

    /// The value as a `u64`; unknown bits or bits that do not fit are errors.
    pub fn as_u64(&self, path: &str) -> WaveResult<u64> {
        match self {
            Value::Bits(bits) => bits.as_u64(path),
            _ => Err(WaveError::NotTwoState {
                path: path.to_string(),
                bits: self.to_string(),
            }),
        }
    }

    fn from_wellen(value: SignalValueRef<'_>) -> Value {
        match value {
            SignalValueRef::BitVec(bits) => {
                let lsb_first: Vec<BitState> = bits
                    .iter_lsb_to_msb()
                    .map(|b| BitState::from_ascii(b.as_ascii()))
                    .collect();
                Value::Bits(BitVec::from_lsb_first(lsb_first))
            }
            SignalValueRef::Real(real) => Value::Real(real),
            SignalValueRef::String(text) => Value::Text(text.to_string()),
            SignalValueRef::Event => Value::Event,
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Bits(bits) => write!(f, "{bits}"),
            Value::Real(real) => write!(f, "{real}"),
            Value::Text(text) => write!(f, "{text}"),
            Value::Event => f.write_str("<event>"),
        }
    }
}

/// Verifies that `path` is a regular file this process can open.
///
/// The waveform backend panics rather than returning an error for an
/// unreadable path, so an explicit diagnosis has to come from here.
fn check_readable_file(path: &Path) -> WaveResult<()> {
    let metadata = std::fs::metadata(path).map_err(|source| WaveError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(WaveError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }
    std::fs::File::open(path)
        .map(|_| ())
        .map_err(|source| WaveError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Compares two recorded values for the purpose of "did this change".
///
/// `x` and `z` are ordinary states here and take part in the comparison like
/// any other bit. Reals compare by bit pattern, so a re-dumped `NaN` is not
/// mistaken for a change the way `PartialEq` would mistake it.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Real(a), Value::Real(b)) => a.to_bits() == b.to_bits(),
        _ => left == right,
    }
}

/// Identifies a recorded signal.
///
/// Two names for the same signal (a VCD alias) share one identifier, so a
/// watch on either name sees one stream of changes. An identifier is only
/// meaningful for the [`Waveform`] that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignalId(u32);

impl SignalId {
    /// The backend index, exposed for diagnostics and logging.
    pub const fn index(self) -> u32 {
        self.0
    }

    fn to_ref(self) -> Option<SignalRef> {
        SignalRef::from_index(self.0 as usize)
    }
}

impl std::fmt::Display for SignalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "signal#{}", self.0)
    }
}

/// How a signal is encoded in the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalKind {
    /// A 1-bit logic value.
    Bit,
    /// A bit vector of the width reported by [`SignalInfo::width`].
    Vector,
    /// A 64-bit real.
    Real,
    /// A variable length string.
    Text,
    /// An event with no value.
    Event,
}

/// One recorded variable, as it was declared in the waveform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalInfo {
    /// Identifier to use in queries.
    pub id: SignalId,
    /// Full hierarchical path, components separated by `.`.
    pub path: String,
    /// Width in bits; 0 for reals, strings and events.
    pub width: u32,
    /// Encoding of the recorded values.
    pub kind: SignalKind,
}

/// Kind of a scope in the waveform hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScopeKind {
    /// A module instance.
    Module,
    /// A task.
    Task,
    /// A function.
    Function,
    /// A named begin block.
    Begin,
    /// A fork block.
    Fork,
    /// A generate block.
    Generate,
    /// A struct.
    Struct,
    /// A union.
    Union,
    /// A class.
    Class,
    /// An interface.
    Interface,
    /// A package.
    Package,
    /// A program.
    Program,
    /// A VHDL scope of any kind.
    Vhdl,
    /// A scope type this layer does not classify.
    Other,
}

/// One scope of the waveform hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeInfo {
    /// Full hierarchical path, components separated by `.`.
    pub path: String,
    /// Local name.
    pub name: String,
    /// Path of the enclosing scope, absent for a root scope.
    pub parent: Option<String>,
    /// Kind of scope.
    pub kind: ScopeKind,
}

impl ScopeKind {
    fn from_wellen(kind: wellen::ScopeType) -> ScopeKind {
        use wellen::ScopeType as W;
        match kind {
            W::Module => ScopeKind::Module,
            W::Task => ScopeKind::Task,
            W::Function => ScopeKind::Function,
            W::Begin => ScopeKind::Begin,
            W::Fork => ScopeKind::Fork,
            W::Generate => ScopeKind::Generate,
            W::Struct => ScopeKind::Struct,
            W::Union => ScopeKind::Union,
            W::Class => ScopeKind::Class,
            W::Interface => ScopeKind::Interface,
            W::Package => ScopeKind::Package,
            W::Program => ScopeKind::Program,
            W::VhdlArchitecture
            | W::VhdlProcedure
            | W::VhdlFunction
            | W::VhdlRecord
            | W::VhdlProcess
            | W::VhdlBlock
            | W::VhdlForGenerate
            | W::VhdlIfGenerate
            | W::VhdlGenerate
            | W::VhdlPackage => ScopeKind::Vhdl,
            _ => ScopeKind::Other,
        }
    }
}

/// A loaded recording.
///
/// Decoding every declared signal up front is this version's memory policy: it
/// keeps the queries below read-only and lets any signal be read at any cursor
/// position. Loading only the signals the design metadata names is a later
/// optimization, not something replay depends on.
#[derive(Debug)]
pub struct Waveform {
    path: PathBuf,
    inner: wellen::simple::Waveform,
    times: Vec<u64>,
    signals: Vec<SignalInfo>,
    by_path: HashMap<String, Vec<usize>>,
    canonical: HashMap<SignalId, String>,
    scopes: Vec<ScopeInfo>,
    timescale: Option<Timescale>,
}

impl Waveform {
    /// Loads a VCD or FST recording.
    ///
    /// Fails when the file cannot be read, cannot be parsed, or contains no
    /// observation points at all.
    pub fn load(path: impl AsRef<Path>) -> WaveResult<Waveform> {
        let path = path.as_ref().to_path_buf();
        // `wellen` panics instead of reporting an error when the file cannot be
        // opened, so readability is established here first.
        check_readable_file(&path)?;
        let inner = match wellen::simple::read(&path) {
            Ok(inner) => inner,
            Err(wellen::WellenError::Io(source)) => return Err(WaveError::Io { path, source }),
            Err(other) => {
                return Err(WaveError::Parse {
                    path,
                    message: other.to_string(),
                });
            }
        };

        let times = inner.time_table().to_vec();
        if times.is_empty() {
            return Err(WaveError::NoObservationPoints { path });
        }

        let mut waveform = Waveform {
            path,
            inner,
            times,
            signals: Vec::new(),
            by_path: HashMap::new(),
            canonical: HashMap::new(),
            scopes: Vec::new(),
            timescale: None,
        };
        waveform.index_hierarchy();
        waveform.load_all_signals();
        Ok(waveform)
    }

    /// Path of the recording this waveform was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `$timescale` of the recording, when it declares one.
    pub fn timescale(&self) -> Option<Timescale> {
        self.timescale
    }

    /// All recorded observation points, strictly increasing.
    pub fn timestamps(&self) -> &[u64] {
        &self.times
    }

    /// Number of recorded observation points.
    pub fn observation_count(&self) -> usize {
        self.times.len()
    }

    /// Time of the observation point at `index`.
    pub fn observation_time(&self, index: usize) -> Option<u64> {
        self.times.get(index).copied()
    }

    /// Index of the last observation point at or before `time`.
    ///
    /// `None` means the recording has nothing at or before `time`; the caller
    /// must not substitute a sample of its own.
    pub fn observation_index(&self, time: u64) -> Option<usize> {
        match self.times.binary_search(&time) {
            Ok(index) => Some(index),
            Err(0) => None,
            Err(insert) => Some(insert - 1),
        }
    }

    /// First recorded time.
    pub fn first_time(&self) -> u64 {
        self.times[0]
    }

    /// Last recorded time, which is the end of the record.
    pub fn last_time(&self) -> u64 {
        self.times[self.times.len() - 1]
    }

    /// Scopes of the waveform hierarchy, in declaration order.
    pub fn scopes(&self) -> &[ScopeInfo] {
        &self.scopes
    }

    /// Every declared variable, in declaration order.
    ///
    /// A VCD alias produces two entries that share one [`SignalId`].
    pub fn signals(&self) -> &[SignalInfo] {
        &self.signals
    }

    /// Looks up a variable by its full hierarchical path, exactly.
    ///
    /// Nothing is guessed; use [`Waveform::find_signals`] to see partial
    /// matches and decide for yourself.
    pub fn signal(&self, path: &str) -> WaveResult<SignalId> {
        let candidates: Vec<&SignalInfo> = self
            .by_path
            .get(path)
            .map(|indices| indices.iter().map(|index| &self.signals[*index]).collect())
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut ids: Vec<SignalId> = Vec::new();
        for info in &candidates {
            if seen.insert(info.id) {
                ids.push(info.id);
            }
        }
        match ids.len() {
            0 => Err(WaveError::SignalNotFound {
                path: path.to_string(),
            }),
            1 => Ok(ids[0]),
            _ => Err(WaveError::AmbiguousSignal {
                path: path.to_string(),
                matches: candidates.iter().map(|info| info.path.clone()).collect(),
            }),
        }
    }

    /// Every variable whose full path is `path`, or ends with `.` + `path`.
    ///
    /// Empty when nothing matches. Aliases of one signal are reported once.
    pub fn find_signals(&self, path: &str) -> Vec<SignalInfo> {
        let mut out: Vec<SignalInfo> = match self.by_path.get(path) {
            Some(indices) => indices.iter().map(|i| self.signals[*i].clone()).collect(),
            None => {
                let suffix = format!(".{path}");
                self.signals
                    .iter()
                    .filter(|info| info.path.ends_with(&suffix))
                    .cloned()
                    .collect()
            }
        };
        // An alias is one signal with several names; a caller asking for a
        // signal wants the signal, not each spelling of it.
        let mut seen = HashSet::new();
        out.retain(|info| seen.insert(info.id));
        out
    }

    /// Information about one signal.
    pub fn signal_info(&self, id: SignalId) -> WaveResult<&SignalInfo> {
        self.signals
            .iter()
            .find(|info| info.id == id)
            .ok_or(WaveError::UnknownSignalId { id: id.index() })
    }

    /// The path this layer uses to name a signal: the first declared name for
    /// an alias group.
    pub fn path_of(&self, id: SignalId) -> WaveResult<&str> {
        self.canonical
            .get(&id)
            .map(String::as_str)
            .ok_or(WaveError::UnknownSignalId { id: id.index() })
    }

    /// Width of a signal in bits.
    pub fn width(&self, id: SignalId) -> WaveResult<u32> {
        Ok(self.signal_info(id)?.width)
    }

    /// Value in effect at `time`: the last recorded value at or before it.
    ///
    /// `Ok(None)` means the recording starts later, which is not a zero.
    pub fn value_at(&self, id: SignalId, time: u64) -> WaveResult<Option<Value>> {
        let signal = self.loaded_signal(id)?;
        let Some(index) = self.observation_index(time) else {
            return Ok(None);
        };
        Ok(Self::value_at_index(signal, index))
    }

    /// Value recorded exactly at `time`, if the signal changed then.
    ///
    /// This distinguishes "changed here" from "held what it had before".
    pub fn exact_at(&self, id: SignalId, time: u64) -> WaveResult<Option<Value>> {
        let signal = self.loaded_signal(id)?;
        let Ok(index) = self.times.binary_search(&time) else {
            return Ok(None);
        };
        let index = index as u32;
        if signal.time_indices().binary_search(&index).is_err() {
            return Ok(None);
        }
        Ok(Self::value_at_index(signal, index as usize))
    }

    /// Recorded value changes of a signal, oldest first.
    ///
    /// Changes recorded at the same timestamp are collapsed to the value that
    /// timestamp ends with, and a re-dump of the current value is not reported
    /// as a change. Consecutive entries therefore always differ, which is what
    /// a watch needs; the recording itself may contain more raw entries.
    ///
    /// The first recorded value has no predecessor and is still listed: it was
    /// recorded, so reporting it is not an invented sample. It is the only
    /// entry a `previous` of `None` can come from.
    pub fn changes(&self, id: SignalId) -> WaveResult<Vec<(u64, Value)>> {
        self.change_records(id)
    }

    /// Value at an observation point index.
    ///
    /// `None` means the signal has no recorded value at or before that point,
    /// which is not the same as an `Event` value.
    fn value_at_index(signal: &wellen::Signal, index: usize) -> Option<Value> {
        let offset = signal.get_offset(index as u32)?;
        Some(Value::from_wellen(signal.get_value_at(&offset, 0)))
    }

    /// The decoded change list used by both [`Waveform::changes`] and the
    /// replay engine.
    pub(crate) fn change_records(&self, id: SignalId) -> WaveResult<Vec<(u64, Value)>> {
        let signal = self.loaded_signal(id)?;
        // First collapse everything recorded inside one timestamp into the
        // value that timestamp ends with.
        let mut snapshots: Vec<(u64, Value)> = Vec::new();
        for (index, value) in signal.iter_changes() {
            let time = self.times[index as usize];
            let value = Value::from_wellen(value);
            match snapshots.last_mut() {
                Some(last) if last.0 == time => last.1 = value,
                _ => snapshots.push((time, value)),
            }
        }
        // Then drop timestamps that end on the value they started with: a
        // re-dump, or several changes that cancel out, is not a change. `x`
        // and `z` take part in this comparison like any other state.
        let mut changes: Vec<(u64, Value)> = Vec::new();
        for (time, value) in snapshots {
            if let Some(last) = changes.last() {
                if same_value(&last.1, &value) {
                    continue;
                }
            }
            changes.push((time, value));
        }
        Ok(changes)
    }

    /// Resolves a signal identifier against the backend.
    fn loaded_signal(&self, id: SignalId) -> WaveResult<&wellen::Signal> {
        let reference = id
            .to_ref()
            .ok_or(WaveError::UnknownSignalId { id: id.index() })?;
        if !self.canonical.contains_key(&id) {
            return Err(WaveError::UnknownSignalId { id: id.index() });
        }
        self.inner
            .get_signal(reference)
            .ok_or(WaveError::UnknownSignalId { id: id.index() })
    }

    /// Converts a physical duration into this recording's time units.
    ///
    /// The duration must be an exact multiple of the recording precision, so a
    /// step grid can never drift.
    pub fn ticks(&self, duration: Timescale) -> WaveResult<u64> {
        let timescale = self
            .timescale()
            .ok_or_else(|| WaveError::MissingTimescale {
                path: self.path.clone(),
            })?;
        let model_zs = timescale.zeptoseconds()?;
        let duration_zs = duration.zeptoseconds()?;
        let remainder_zs = duration_zs % model_zs;
        if remainder_zs != 0 {
            return Err(WaveError::IndivisibleDuration {
                duration,
                timescale,
                remainder_zs,
            });
        }
        let ticks = duration_zs / model_zs;
        if ticks == 0 {
            return Err(WaveError::BadDuration {
                text: duration.to_string(),
                reason: "a duration of zero ticks is not a step".to_string(),
            });
        }
        u64::try_from(ticks).map_err(|_| WaveError::TimeOverflow {
            text: duration.to_string(),
        })
    }

    /// Builds the scope list and the signal table.
    fn index_hierarchy(&mut self) {
        let hierarchy = self.inner.hierarchy();
        self.scopes = hierarchy
            .all_scopes()
            .map(|scope_ref| {
                let scope = &hierarchy[scope_ref];
                let path = scope.full_name(hierarchy);
                let name = scope.name(hierarchy).to_string();
                let parent = path.rfind('.').map(|split| path[..split].to_string());
                ScopeInfo {
                    path,
                    name,
                    parent,
                    kind: ScopeKind::from_wellen(scope.scope_type()),
                }
            })
            .collect();

        for var_ref in hierarchy.all_vars() {
            let var = &hierarchy[var_ref];
            let id = SignalId(var.signal_ref().index() as u32);
            let encoding = var.signal_encoding(hierarchy);
            let (width, kind) = match encoding {
                wellen::SignalEncoding::BitVector(width) => {
                    let kind = if width == 1 {
                        SignalKind::Bit
                    } else {
                        SignalKind::Vector
                    };
                    (width, kind)
                }
                wellen::SignalEncoding::Real => (0, SignalKind::Real),
                wellen::SignalEncoding::String => (0, SignalKind::Text),
            };
            // `$var event` is recorded as a zero-width bit vector.
            let kind = if width == 0 && kind == SignalKind::Bit {
                SignalKind::Event
            } else {
                kind
            };
            let path = var.full_name(hierarchy);
            let index = self.signals.len();
            self.by_path.entry(path.clone()).or_default().push(index);
            self.canonical.entry(id).or_insert_with(|| path.clone());
            self.signals.push(SignalInfo {
                id,
                path,
                width,
                kind,
            });
        }

        self.timescale = hierarchy.timescale().and_then(|scale| {
            let unit = TimeUnit::from_wellen(scale.unit)?;
            Some(Timescale::new(scale.factor as u64, unit))
        });
    }

    /// Decodes every recorded signal so that later queries cannot fail for
    /// lack of decoded data.
    fn load_all_signals(&mut self) {
        let references: Vec<SignalRef> = self.inner.hierarchy().signals().collect();
        self.inner.load_signals_multi_threaded(&references);
    }
}
