// Reference DUT for the testbench self-check, written by hand to express the
// intended semantics of `arrays.shy.rs` directly.
//
// Layout, applied by hand here: `[T; N]` puts element 0 in the least significant
// bits, a struct packs its fields most significant first, and the two compose.
// The acceptance test itself always runs the *generated* Verilog; this file only
// proves that `tb_arrays.v` passes a design whose semantics are right.

module ArrConst (
    input  [3:0] v,
    input        k,
    input  [1:0] m,
    output [11:0] repeated,
    output [11:0] listed,
    output [11:0] copied,
    output [7:0]  sub,
    output [3:0]  elem0,
    output [3:0]  elem2,
    output [15:0] nested,
    output [3:0]  nested_elem,
    output [3:0]  nested_dyn
);
    // Element 2 is written first because element 0 is the least significant.
    assign repeated = { v, v, v };
    // `[v, 1, 2]` with a constant write of 7 over element 1.
    assign listed = { 4'h2, 4'h7, v };
    assign copied = listed;
    assign sub = { 4'h7, v };
    assign elem0 = copied[3:0];
    assign elem2 = copied[11:8];

    // `[[v; 2], [2, 3]]`: the outer element 0 holds the repeat in its low bits.
    assign nested = { 4'h3, 4'h2, v, v };
    assign nested_elem = nested[11:8];

    // `nested[k][m]`: the outer index picks an eight-bit half and the inner one
    // a nibble of it. An inner index with no element is the unknown rather than
    // the element beside it.
    wire [7:0] outer = k ? nested[15:8] : nested[7:0];
    assign nested_dyn = (m == 2'd0) ? outer[3:0] : (m == 2'd1) ? outer[7:4] : 4'bx;
endmodule

module ArrDynRead (
    input  [11:0] src,
    input  [1:0]  i,
    input         one,
    input         j,
    input  [2:0]  j3,
    output reg [3:0] y,
    output        one_bit,
    output        bit_from_element,
    output [3:0]  cast_y
);
    always @(*) begin
        case (i)
            2'd0: y = src[3:0];
            2'd1: y = src[7:4];
            2'd2: y = src[11:8];
            // No element at this index: the standard unknown.
            default: y = 4'bx;
        endcase
    end

    // A one-element array: index 0 is the element and index 1 has none.
    assign one_bit = (j == 1'b0) ? one : 1'bx;

    // `src[0]` is the low four-bit element; the bit select then applies to that
    // element, and an index past its width is the unknown. The element is
    // materialised first, so the bit select can never reach the whole array.
    wire [3:0] element0 = src[3:0];
    assign bit_from_element = (j3 < 3'd4) ? element0[j3] : 1'bx;

    // The same element select through a signed index value that was cast back to
    // unsigned: `$unsigned` of the two-bit pattern, so index 2 and 3 select the
    // elements they name rather than being read as a negative offset.
    wire [1:0] unsigned_index = $unsigned(i);
    assign cast_y = (unsigned_index < 2'd3) ? src[unsigned_index * 4 +: 4] : 4'bx;
endmodule

module ArrDynWrite (
    input  [3:0] v,
    input  [1:0] i,
    input  [1:0] oi,
    input  [1:0] ii,
    input  [1:0] fi,
    input        clk,
    input        rst_n,
    output reg [11:0] comb_out,
    output reg [3:0]  comb_read,
    output reg [11:0] seq_out,
    output reg [3:0]  snap,
    output reg [15:0] nest_out,
    output reg [15:0] field_out
);
    always @(*) begin
        // The whole array is defaulted, then one element may change.
        comb_out = { 4'h3, 4'h2, 4'h1 };
        case (i)
            2'd0: comb_out[3:0] = v;
            2'd1: comb_out[7:4] = v;
            2'd2: comb_out[11:8] = v;
            default: ;   // out of range changes nothing
        endcase

        // Read back in the same process, after the write: source order decides,
        // so an in-range index sees the value just written.
        case (i)
            2'd0: comb_read = comb_out[3:0];
            2'd1: comb_read = comb_out[7:4];
            2'd2: comb_read = comb_out[11:8];
            default: comb_read = 4'bx;
        endcase

        // `[[1, 2], [3, 4]]` with element 0 lowest, then one inner element
        // replaced. Both indices are range checked, so neither an inner nor an
        // outer out-of-range index can land on a neighbouring element.
        nest_out = { 4'h4, 4'h3, 4'h2, 4'h1 };
        if (oi < 2'd2 && ii < 2'd2) begin
            case ({ oi, ii })
                4'b0000: nest_out[3:0] = v;
                4'b0001: nest_out[7:4] = v;
                4'b0100: nest_out[11:8] = v;
                4'b0101: nest_out[15:12] = v;
                default: ;
            endcase
        end

        // `[Cell{1,2}, Cell{3,4}]`: the tag is the high nibble of each byte, so
        // element 0 is the low byte and the array is `0x3412`. Only `val` of the
        // selected struct changes, so the write lands on the low nibble of that
        // element's low nibble.
        field_out = { 4'h3, 4'h4, 4'h1, 4'h2 };
        if (fi == 2'd0) field_out[3:0] = v;
        else if (fi == 2'd1) field_out[11:8] = v;
    end

    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            seq_out <= 12'h000;
            snap <= 4'h0;
        end else begin
            // Both reads and the write are non-blocking, so the snapshot sees
            // the state from before the edge, including the element this edge
            // writes.
            case (i)
                2'd0: snap <= seq_out[3:0];
                2'd1: snap <= seq_out[7:4];
                2'd2: snap <= seq_out[11:8];
                default: snap <= 4'bx;
            endcase
            case (i)
                2'd0: seq_out[3:0] <= v;
                2'd1: seq_out[7:4] <= v;
                2'd2: seq_out[11:8] <= v;
                default: ;   // out of range keeps every element
            endcase
        end
    end
endmodule

module ArrNest (
    input  [15:0] cells,
    input  [8:0]  b,
    output [7:0]  tags,
    output [7:0]  vals,
    output [7:0]  tag0_ext,
    output [3:0]  b0,
    output [3:0]  b1,
    output        bflag,
    output [7:0]  cell0
);
    // `Cell { tag, val }` packs the tag most significant, and `[Cell; 2]` puts
    // cell 0 in the low byte.
    wire signed [3:0] tag0 = cells[7:4];
    wire signed [3:0] tag1 = cells[15:12];
    wire [3:0] val0 = cells[3:0];
    wire [3:0] val1 = cells[11:8];

    assign tags = { tag1, tag0 };
    assign vals = { val1, val0 };
    assign tag0_ext = { { 4{cells[7]} }, cells[7:4] };
    assign cell0 = cells[7:0];

    // `Bundle { items: [uint<4>; 2], flag: bool }` puts `items` above `flag`,
    // and item 0 is the low nibble of `items`.
    assign b0 = b[4:1];
    assign b1 = b[8:5];
    assign bflag = b[0];
endmodule

module ArrTop (
    input  [3:0]  v,
    input  [1:0]  i,
    input         k,
    input  [1:0]  m,
    input         j,
    input         one,
    input  [2:0]  j3,
    input  [1:0]  oi,
    input  [1:0]  ii,
    input  [1:0]  fi,
    input  [11:0] src,
    input  [15:0] cells,
    input  [8:0]  b,
    input         clk,
    input         rst_n,
    output [11:0] repeated,
    output [11:0] listed,
    output [11:0] copied,
    output [7:0]  sub,
    output [3:0]  elem0,
    output [3:0]  elem2,
    output [15:0] nested,
    output [3:0]  nested_elem,
    output [3:0]  nested_dyn,
    output [3:0]  y,
    output        one_bit,
    output        bit_from_element,
    output [3:0]  cast_y,
    output [11:0] comb_out,
    output [3:0]  comb_read,
    output [11:0] seq_out,
    output [3:0]  snap,
    output [15:0] nest_out,
    output [15:0] field_out,
    output [7:0]  tags,
    output [7:0]  vals,
    output [7:0]  tag0_ext,
    output [3:0]  b0,
    output [3:0]  b1,
    output        bflag,
    output [7:0]  cell0
);
    ArrConst konst (
        .v(v),
        .k(k),
        .m(m),
        .repeated(repeated),
        .listed(listed),
        .copied(copied),
        .sub(sub),
        .elem0(elem0),
        .elem2(elem2),
        .nested(nested),
        .nested_elem(nested_elem),
        .nested_dyn(nested_dyn)
    );

    ArrDynRead reader (.src(src), .i(i), .one(one), .j(j), .j3(j3), .y(y),
                       .one_bit(one_bit), .bit_from_element(bit_from_element),
                       .cast_y(cast_y));

    ArrDynWrite writer (
        .v(v),
        .i(i),
        .oi(oi),
        .ii(ii),
        .fi(fi),
        .clk(clk),
        .rst_n(rst_n),
        .comb_out(comb_out),
        .comb_read(comb_read),
        .seq_out(seq_out),
        .snap(snap),
        .nest_out(nest_out),
        .field_out(field_out)
    );

    ArrNest nest (
        .cells(cells),
        .b(b),
        .tags(tags),
        .vals(vals),
        .tag0_ext(tag0_ext),
        .b0(b0),
        .b1(b1),
        .bflag(bflag),
        .cell0(cell0)
    );
endmodule
