// Reference DUT for the testbench self-check of `wrapped.shy.rs`, written by hand
// to express the intended semantics directly.
//
// Widths, applied by hand here:
//
//   Msg                     = payload 8 + tag 2 = 10 bits
//   Envelope { msg, extra } = msg 10 + extra 2  = 12 bits
//   [Msg; 2]                = msg1 10 + msg0 10 = 20 bits
//
// A struct packs its fields most significant first, an array puts element 0 in
// the least significant bits, and the tag of an enum is the lowest slice of its
// own value.
//
// The acceptance test itself always runs the *generated* Verilog; this file only
// proves that `tb_wrapped.v` passes a design whose semantics are right.

module WrapMake (
    input  [3:0]  v,
    input  [3:0]  s,
    output [11:0] env_idle,
    output [11:0] env_pair,
    output [19:0] arr,
    output [9:0]  arr0,
    output [9:0]  arr1,
    output [9:0]  env_field,
    output [1:0]  env_extra
);
    // `Envelope { msg, extra }`: `msg` first, so it is the high ten bits.
    assign env_idle = { 8'h00, 2'd0, 2'h1 };
    assign env_pair = { s, v, 2'd2, 2'h2 };
    // `[Msg; 2]`: element 0 is the low ten bits.
    assign arr = { 8'h00, 2'd0, 4'h0, v, 2'd1 };
    assign arr0 = arr[9:0];
    assign arr1 = arr[19:10];
    assign env_field = env_pair[11:2];
    assign env_extra = env_pair[1:0];
endmodule

/// Equality over these shapes. Two enums are equal when their tags agree and, for
/// a variant that carries a payload, the payload too; the payload bits a variant
/// does not use never take part, and a tag with no variant compares on its tag
/// alone.
module EnumEq (
    input  [9:0] x,
    input  [9:0] y,
    output       eq
);
    wire [1:0] tx = x[1:0];
    wire [1:0] ty = y[1:0];

    // Tag 0 (`Idle`) and tag 3 (no variant) carry no payload, so only the tag is
    // compared. Tag 1 (`Data`) compares the four-bit value, ignoring the four
    // unused bits above it. Tag 2 (`Pair`) compares all eight payload bits.
    wire payload_eq = (tx == 2'd1) ? (x[5:2] == y[5:2]) : (x[9:2] == y[9:2]);
    wire carries_payload = (tx == 2'd1) || (tx == 2'd2);

    assign eq = (tx == ty) && (carries_payload ? payload_eq : 1'b1);
endmodule

module WrapEq (
    input  [11:0] a,
    input  [11:0] b,
    input  [9:0]  ea,
    input  [9:0]  eb,
    input  [19:0] aa,
    input  [19:0] ab,
    output env_same,
    output env_differ,
    output msg_same,
    output msg_differ,
    output arr_same,
    output arr_differ
);
    // A struct is equal when all of its fields are, the enum field included.
    wire env_eq = (a[11:2] == b[11:2]) && (a[1:0] == b[1:0]);
    assign env_same = env_eq;
    assign env_differ = !env_eq;

    wire msg_eq;
    EnumEq em (.x(ea), .y(eb), .eq(msg_eq));
    assign msg_same = msg_eq;
    assign msg_differ = !msg_eq;

    // An array is equal when every element is, so element 0's payload counts and
    // an inactive element's unused payload bits do not.
    wire e0, e1;
    EnumEq el0 (.x(aa[9:0]), .y(ab[9:0]), .eq(e0));
    EnumEq el1 (.x(aa[19:10]), .y(ab[19:10]), .eq(e1));
    wire arr_eq = e0 && e1;
    assign arr_same = arr_eq;
    assign arr_differ = !arr_eq;
endmodule

module WrapTop (
    input  [3:0]  v,
    input  [3:0]  s,
    input  [11:0] a,
    input  [11:0] b,
    input  [9:0]  ea,
    input  [9:0]  eb,
    input  [19:0] aa,
    input  [19:0] ab,
    output [11:0] env_idle,
    output [11:0] env_pair,
    output [19:0] arr,
    output [9:0]  arr0,
    output [9:0]  arr1,
    output [9:0]  env_field,
    output [1:0]  env_extra,
    output env_same,
    output env_differ,
    output msg_same,
    output msg_differ,
    output arr_same,
    output arr_differ
);
    WrapMake make (.v(v), .s(s), .env_idle(env_idle), .env_pair(env_pair), .arr(arr),
                   .arr0(arr0), .arr1(arr1), .env_field(env_field), .env_extra(env_extra));

    WrapEq eq (.a(a), .b(b), .ea(ea), .eb(eb), .aa(aa), .ab(ab), .env_same(env_same),
               .env_differ(env_differ), .msg_same(msg_same), .msg_differ(msg_differ),
               .arr_same(arr_same), .arr_differ(arr_differ));
endmodule
