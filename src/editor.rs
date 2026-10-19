//! Editor-only support for the DSL surface.
//!
//! Nothing here is compiled by rustc in a normal build: the module is gated on
//! `cfg(rust_analyzer)`, which only rust-analyzer enables. It exists so the
//! analyzer can type-check the hardware body the macros expose to it, while the
//! real build keeps compiling the placeholder items.
//!
//! The DSL's hardware types are real Rust types (`U<N>`, `I<N>`), so the editor
//! keeps the declared widths in hovers and completions. What Rust itself does
//! not provide is the DSL's implicit conversions and operators; this module
//! supplies them as ordinary trait implementations and free functions. Every
//! function body here panics: it is never executed, it only exists to give the
//! analyzer a type.

#![allow(dead_code)]

use std::ops::{Add, BitAnd, BitOr, BitXor, Mul, Not, Shl, Shr, Sub};

use crate::{I, U};

/// Build a hardware value from an integer literal, with the width inferred
/// from the surrounding context (`self.y = U::from(1)`).
macro_rules! from_integer {
    ($ty:ident) => {
        impl<const W: usize> From<i32> for $ty<W> {
            fn from(_: i32) -> Self {
                unreachable!("hardware values are compiled by ShyHDL, not executed by Rust")
            }
        }
    };
}

from_integer!(U);
from_integer!(I);

/// Reinterpret a signed value as unsigned and the other way round: the DSL
/// spells this `as U<N>` / `as I<N>`, which the macro rewrites to `from`.
impl<const W: usize> From<I<W>> for U<W> {
    fn from(_: I<W>) -> Self {
        unreachable!()
    }
}
impl<const W: usize> From<U<W>> for I<W> {
    fn from(_: U<W>) -> Self {
        unreachable!()
    }
}

/// Build a hardware value from an integer literal whose width is inferred from
/// the context: `self.y = __bits(0x15);`.
///
/// The macro rewrites a bare literal standing alone in a value position into a
/// call to this function. Nothing calls it at run time.
pub fn __bits<T: From<i32>>(_value: i32) -> T {
    unreachable!("hardware values are compiled by ShyHDL, not executed by Rust")
}

/// A single bit or element index: `x[i]`.
///
/// A scalar yields a bit, an array yields an element. The DSL decides which by
/// the value's type; the trait keeps that distinction in the editor.
pub trait BitIndex {
    /// A bit for a scalar, the element type for an array.
    type Output;
    /// Element `index`.
    fn __bit(self, index: usize) -> Self::Output;
}

impl<const W: usize> BitIndex for U<W> {
    type Output = bool;
    fn __bit(self, _index: usize) -> bool {
        unreachable!()
    }
}
impl<const W: usize> BitIndex for I<W> {
    type Output = bool;
    fn __bit(self, _index: usize) -> bool {
        unreachable!()
    }
}
impl<T: Copy, const N: usize> BitIndex for [T; N] {
    type Output = T;
    fn __bit(self, _index: usize) -> T {
        unreachable!()
    }
}

/// A bit range: `x[lo..hi]`.
///
/// The result is a hardware value whose width comes from the range; the
/// surrounding context names the type, so the return type is a type parameter.
pub trait BitSlice {
    /// The selected bits, as the type the context asks for.
    fn __bits<O>(self, range: std::ops::Range<usize>) -> O;
}

impl<const W: usize> BitSlice for U<W> {
    fn __bits<O>(self, _range: std::ops::Range<usize>) -> O {
        unreachable!()
    }
}
impl<const W: usize> BitSlice for I<W> {
    fn __bits<O>(self, _range: std::ops::Range<usize>) -> O {
        unreachable!()
    }
}
impl<T: Copy, const N: usize> BitSlice for [T; N] {
    fn __bits<O>(self, _range: std::ops::Range<usize>) -> O {
        unreachable!()
    }
}

/// A write to a bit or a bit range: `x[i] = v` and `x[lo..hi] = v`.
pub trait BitAssign {
    /// Write one bit.
    fn __set_bit<V>(&mut self, index: usize, value: V);
    /// Write a bit range.
    fn __set_bits<V>(&mut self, range: std::ops::Range<usize>, value: V);
}

impl<const W: usize> BitAssign for U<W> {
    fn __set_bit<V>(&mut self, _index: usize, _value: V) {
        unreachable!()
    }
    fn __set_bits<V>(&mut self, _range: std::ops::Range<usize>, _value: V) {
        unreachable!()
    }
}
impl<const W: usize> BitAssign for I<W> {
    fn __set_bit<V>(&mut self, _index: usize, _value: V) {
        unreachable!()
    }
    fn __set_bits<V>(&mut self, _range: std::ops::Range<usize>, _value: V) {
        unreachable!()
    }
}
impl<T: Copy, const N: usize> BitAssign for [T; N] {
    fn __set_bit<V>(&mut self, _index: usize, _value: V) {
        unreachable!()
    }
    fn __set_bits<V>(&mut self, _range: std::ops::Range<usize>, _value: V) {
        unreachable!()
    }
}

/// The arithmetic and bit operators the DSL writes on hardware values.
///
/// An integer literal next to a hardware value is rewritten to `__bits(..)` by
/// the macro, so both operands have the same type and one implementation per
/// operator is enough.
macro_rules! hardware_ops {
    ($ty:ident) => {
        impl<const W: usize> Add for $ty<W> {
            type Output = $ty<W>;
            fn add(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> Sub for $ty<W> {
            type Output = $ty<W>;
            fn sub(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> Mul for $ty<W> {
            type Output = $ty<W>;
            fn mul(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> BitAnd for $ty<W> {
            type Output = $ty<W>;
            fn bitand(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> BitOr for $ty<W> {
            type Output = $ty<W>;
            fn bitor(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> BitXor for $ty<W> {
            type Output = $ty<W>;
            fn bitxor(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> Shl for $ty<W> {
            type Output = $ty<W>;
            fn shl(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> Shr for $ty<W> {
            type Output = $ty<W>;
            fn shr(self, _: $ty<W>) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> Not for $ty<W> {
            type Output = $ty<W>;
            fn not(self) -> $ty<W> {
                unreachable!()
            }
        }
        impl<const W: usize> PartialOrd for $ty<W> {
            fn partial_cmp(&self, _: &$ty<W>) -> Option<std::cmp::Ordering> {
                unreachable!()
            }
        }
    };
}

hardware_ops!(U);
hardware_ops!(I);

/// The width-changing methods the DSL calls on a hardware value.
macro_rules! hardware_methods {
    ($ty:ident) => {
        impl<const W: usize> $ty<W> {
            /// Zero-extend to `M` bits.
            pub fn zext<const M: usize>(self) -> $ty<M> {
                unreachable!()
            }
            /// Sign-extend to `M` bits.
            pub fn sext<const M: usize>(self) -> $ty<M> {
                unreachable!()
            }
            /// Truncate to `M` bits.
            pub fn trunc<const M: usize>(self) -> $ty<M> {
                unreachable!()
            }
            /// Bit `bit` of the value.
            pub fn get(self, _bit: usize) -> bool {
                unreachable!()
            }
        }
    };
}

hardware_methods!(U);
hardware_methods!(I);

/// The testbench calls the DSL writes as plain functions.
///
/// A testbench body is ordinary Rust syntax with these calls in it; giving each
/// one a signature is enough for the analyzer to accept the body.
pub fn delay<T>(_: T) {}
pub fn ns(_value: u64) -> u64 {
    unreachable!()
}
pub fn ps(_value: u64) -> u64 {
    unreachable!()
}
pub fn us(_value: u64) -> u64 {
    unreachable!()
}
pub fn ms(_value: u64) -> u64 {
    unreachable!()
}
pub fn s(_value: u64) -> u64 {
    unreachable!()
}
pub fn fs(_value: u64) -> u64 {
    unreachable!()
}
pub fn hz(_value: u64) -> u64 {
    unreachable!()
}
pub fn khz(_value: u64) -> u64 {
    unreachable!()
}
pub fn mhz(_value: u64) -> u64 {
    unreachable!()
}
pub fn posedge(_value: bool) -> bool {
    unreachable!()
}
pub fn negedge(_value: bool) -> bool {
    unreachable!()
}
pub fn gen_clk(_clock: bool, _frequency: u64) {}
pub fn wait_until(_condition: bool) {}
pub fn sample_after(_edge: bool, _delay: u64) {}

/// The DSL's statement macros.
///
/// `initial!` expands to its own block so the analyzer completes inside it.
/// `always!` carries event arms that are not Rust statements, so it expands to
/// the unit value: its body is not analyzed, which is a documented limitation.
#[macro_export]
macro_rules! initial {
    ($($body:tt)*) => {{ $($body)* }};
}
#[macro_export]
macro_rules! always {
    ($($body:tt)*) => {
        ()
    };
}
#[macro_export]
macro_rules! clock {
    ($($body:tt)*) => {
        ()
    };
}
#[macro_export]
macro_rules! reset {
    ($($body:tt)*) => {
        ()
    };
}
#[macro_export]
macro_rules! pass {
    () => {
        ()
    };
}
#[macro_export]
macro_rules! finish {
    () => {
        ()
    };
}
#[macro_export]
macro_rules! timeout {
    ($($body:tt)*) => {
        ()
    };
}
#[macro_export]
macro_rules! nb_assign {
    ($($body:tt)*) => {
        ()
    };
}
/// A bit-mask pattern. It expands to a literal so it is valid both as a match
/// pattern and as a value.
#[macro_export]
macro_rules! mask {
    ($($body:tt)*) => {
        0
    };
}
