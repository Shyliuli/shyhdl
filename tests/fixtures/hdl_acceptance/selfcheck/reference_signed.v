// Reference DUT for the testbench self-check, written by hand with Verilog's own
// signed arithmetic to express the intended semantics of `signed.shy.rs`.
//
// The acceptance test itself always runs the *generated* Verilog; this file only
// proves that `tb_signed.v` passes a design whose semantics are right, and that
// its witnesses fire for the right reason.
//
// A signed port is declared `signed`, a shift of a signed value uses `>>>`, a
// comparison of two signed values is signed by itself, and the unsigned
// comparison goes through `$unsigned`. Struct fields are cut out of the raw bits
// by hand, with the signedness the source gives them.

module Signed4 (
    input  signed [3:0] a,
    input  signed [3:0] b,
    output signed [3:0] sum,
    output signed [3:0] diff,
    output signed [3:0] prod,
    output signed [3:0] band,
    output signed [3:0] bor,
    output signed [3:0] bxor,
    output signed [3:0] shl,
    output signed [3:0] shr,
    output signed [3:0] neg,
    output lt,
    output le,
    output gt,
    output ge,
    output eq,
    output ne,
    output ult
);
    assign sum = a + b;
    assign diff = a - b;
    assign prod = a * b;
    assign band = a & b;
    assign bor = a | b;
    assign bxor = a ^ b;
    assign shl = a <<< 1;
    assign shr = a >>> 1;
    assign neg = -a;
    assign lt = a < b;
    assign le = a <= b;
    assign gt = a > b;
    assign ge = a >= b;
    assign eq = a == b;
    assign ne = a != b;
    assign ult = $unsigned(a) < $unsigned(b);
endmodule

module Signed8 (
    input  signed [7:0] a,
    input  signed [7:0] b,
    output signed [7:0] sum,
    output signed [7:0] shr,
    output signed [7:0] neg,
    output lt,
    output ult,
    output signed [15:0] ext
);
    assign sum = a + b;
    assign shr = a >>> 1;
    assign neg = -a;
    assign lt = a < b;
    assign ult = $unsigned(a) < $unsigned(b);
    assign ext = a;
endmodule

module PairExt (
    input  [7:0] p,
    input  signed [3:0] t,
    output signed [3:0] s,
    output [3:0] u,
    output signed [7:0] ext,
    output signed [3:0] back,
    output signed [3:0] shr,
    output lt,
    output [1:0] low,
    output low_lt
);
    // `Pair` packs `s` first, most significant, so the field is the high nibble.
    wire signed [3:0] field = p[7:4];

    assign s = field;
    assign u = p[3:0];
    assign ext = { { 4{p[7]} }, p[7:4] };
    assign back = p[7:4];
    assign shr = field >>> 1;
    assign lt = field < t;
    // `slice(0, 2)` of the field is raw bits, and so is the same slice of the
    // unsigned field, so this comparison is unsigned.
    assign low = p[5:4];
    assign low_lt = p[5:4] < p[1:0];
endmodule

module SignedConst (
    output signed [3:0] v,
    output signed [3:0] v_shr,
    output [3:0] u
);
    assign v = -4'sd3;
    assign v_shr = v >>> 1;
    assign u = 4'hF;
endmodule

module SignedTop (
    input  signed [3:0] a4,
    input  signed [3:0] b4,
    input  signed [7:0] a8,
    input  signed [7:0] b8,
    input  [7:0] p,
    input  signed [3:0] t,
    output signed [3:0] sum4,
    output signed [3:0] diff4,
    output signed [3:0] prod4,
    output signed [3:0] band4,
    output signed [3:0] bor4,
    output signed [3:0] bxor4,
    output signed [3:0] shl4,
    output signed [3:0] shr4,
    output signed [3:0] neg4,
    output lt4,
    output le4,
    output gt4,
    output ge4,
    output eq4,
    output ne4,
    output ult4,
    output signed [7:0] sum8,
    output signed [7:0] shr8,
    output signed [7:0] neg8,
    output lt8,
    output ult8,
    output signed [15:0] ext8,
    output signed [3:0] ps,
    output [3:0] pu,
    output signed [7:0] pext,
    output signed [3:0] pback,
    output signed [3:0] pshr,
    output plt,
    output [1:0] plow,
    output plowlt,
    output signed [3:0] cv,
    output signed [3:0] cvshr,
    output [3:0] cu
);
    Signed4 four (
        .a(a4),
        .b(b4),
        .sum(sum4),
        .diff(diff4),
        .prod(prod4),
        .band(band4),
        .bor(bor4),
        .bxor(bxor4),
        .shl(shl4),
        .shr(shr4),
        .neg(neg4),
        .lt(lt4),
        .le(le4),
        .gt(gt4),
        .ge(ge4),
        .eq(eq4),
        .ne(ne4),
        .ult(ult4)
    );

    Signed8 eight (.a(a8), .b(b8), .sum(sum8), .shr(shr8), .neg(neg8),
                   .lt(lt8), .ult(ult8), .ext(ext8));

    PairExt pair (.p(p), .t(t), .s(ps), .u(pu), .ext(pext), .back(pback),
                  .shr(pshr), .lt(plt), .low(plow), .low_lt(plowlt));

    SignedConst konst (.v(cv), .v_shr(cvshr), .u(cu));
endmodule
