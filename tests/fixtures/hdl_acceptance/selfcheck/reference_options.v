// Reference DUT for the testbench self-check of `options.shy.rs`, written by hand
// to express the intended semantics directly.
//
// Widths, applied by hand here:
//
//   Option<uint<4>>                 = payload 4 + tag 1 = 5 bits
//   Result<uint<4>, int<8>>         = payload 8 + tag 1 = 9 bits
//   Option<Result<uint<4>, int<8>>> = payload 9 + tag 1 = 10 bits
//
// The tag is the lowest slice, the payload is as wide as the widest case, and the
// payload bits a case does not use are zero. `None` is tag 0 and `Some` tag 1;
// `Ok` is tag 0 and `Err` tag 1.
//
// The acceptance test itself always runs the *generated* Verilog; this file only
// proves that `tb_options.v` passes a design whose semantics are right.

module OptMake (
    input  [3:0] v,
    input  [7:0] s,
    output [4:0] none,
    output [4:0] some,
    output [8:0] ok,
    output [8:0] err,
    output [9:0] deep_ok,
    output [9:0] deep_err
);
    assign none = { 4'h0, 1'b0 };
    assign some = { v, 1'b1 };
    // `Ok`'s four-bit payload sits in the low half of the eight-bit payload and
    // its unused high half is zero.
    assign ok = { 4'h0, v, 1'b0 };
    assign err = { s, 1'b1 };
    // `Some(Ok(v))` is the outer tag set over an inner `Ok`.
    assign deep_ok = { 4'h0, v, 1'b0, 1'b1 };
    assign deep_err = { s, 1'b1, 1'b1 };
endmodule

module NestedMatch (
    input  [1:0] ob,
    output reg [3:0] y
);
    always @(*) begin
        // The tag is the lowest bit, so `Some(false)` is `{1'b0, 1'b1}` and
        // `Some(true)` is `{1'b1, 1'b1}`.
        if (ob[0] == 1'b0) begin
            // `None`, including a value whose unused payload bit is junk.
            y = 4'h0;
        end else if (ob[1] == 1'b0) begin
            y = 4'h1;   // Some(false)
        end else begin
            y = 4'h2;   // Some(true)
        end
    end
endmodule

module DeepNest (
    input  [9:0] deep_ok,
    input  [9:0] deep_err,
    output reg [3:0] ok_y,
    output reg [7:0] ok_e,
    output reg [3:0] err_y,
    output reg [7:0] err_e
);
    always @(*) begin
        if (deep_ok[0] == 1'b0) begin
            ok_y = 4'hF;
            ok_e = 8'h00;
        end else if (deep_ok[1] == 1'b0) begin
            // `Some(Ok(x))`: the four-bit value lives in the low half of the
            // eight-bit payload.
            ok_y = deep_ok[5:2];
            ok_e = 8'h00;
        end else begin
            ok_y = 4'h0;
            ok_e = deep_ok[9:2];
        end

        if (deep_err[0] == 1'b0) begin
            err_y = 4'hF;
            err_e = 8'h00;
        end else if (deep_err[1] == 1'b0) begin
            err_y = deep_err[5:2];
            err_e = 8'h00;
        end else begin
            err_y = 4'h0;
            err_e = deep_err[9:2];
        end
    end
endmodule

/// `==` and `!=` over these types compare the active variant and its payload
/// only: an inactive variant's payload bits never take part, and the tag always
/// does.
module EnumEq (
    input  [4:0] a,
    input  [4:0] b,
    input  [8:0] ra,
    input  [8:0] rb,
    input  [9:0] na,
    input  [9:0] nb,
    output same,
    output differ,
    output rsame,
    output rdiffer,
    output nsame,
    output ndiffer
);
    // `Option<uint<4>>`: `None` carries no payload, so only its tag matters.
    wire a_some = a[0];
    wire b_some = b[0];
    wire opt_eq = (a_some == b_some) && (a_some ? (a[4:1] == b[4:1]) : 1'b1);

    // `Result<uint<4>, int<8>>`: the payload is the full eight bits, but only the
    // half the active case uses takes part.
    wire ra_err = ra[0];
    wire rb_err = rb[0];
    wire res_eq = (ra_err == rb_err) && (ra_err ? (ra[8:1] == rb[8:1])
                                               : (ra[4:1] == rb[4:1]));

    // `Option<Result<..>>`: at the outer level only the tag and, when `Some`, the
    // whole inner value matters; the inner comparison never looks past the inner
    // tag when it is absent.
    wire na_some = na[0];
    wire nb_some = nb[0];
    wire na_err = na[2];
    wire nb_err = nb[2];
    wire nest_eq = (na_some == nb_some)
                   && (na_some ? ((na_err == nb_err)
                                  && (na_err ? (na[9:3] == nb[9:3]) : (na[5:3] == nb[5:3])))
                               : 1'b1);

    assign same = opt_eq;
    assign differ = !opt_eq;
    assign rsame = res_eq;
    assign rdiffer = !res_eq;
    assign nsame = nest_eq;
    assign ndiffer = !nest_eq;
endmodule

module OptTop (
    input  [3:0] v,
    input  [7:0] s,
    input  [1:0] ob,
    input  [4:0] a,
    input  [4:0] b,
    input  [8:0] ra,
    input  [8:0] rb,
    input  [9:0] na,
    input  [9:0] nb,
    output [4:0] none,
    output [4:0] some,
    output [8:0] ok,
    output [8:0] err,
    output [9:0] deep_ok,
    output [9:0] deep_err,
    output [3:0] n_y,
    output same,
    output differ,
    output rsame,
    output rdiffer,
    output nsame,
    output ndiffer,
    output [3:0] ok_y,
    output [7:0] ok_e,
    output [3:0] err_y,
    output [7:0] err_e
);
    OptMake make (.v(v), .s(s), .none(none), .some(some), .ok(ok), .err(err),
                  .deep_ok(deep_ok), .deep_err(deep_err));

    NestedMatch nested (.ob(ob), .y(n_y));

    EnumEq eq (.a(a), .b(b), .ra(ra), .rb(rb), .na(na), .nb(nb), .same(same),
               .differ(differ), .rsame(rsame), .rdiffer(rdiffer), .nsame(nsame),
               .ndiffer(ndiffer));

    DeepNest deep (.deep_ok(deep_ok), .deep_err(deep_err), .ok_y(ok_y), .ok_e(ok_e),
                   .err_y(err_y), .err_e(err_e));
endmodule
