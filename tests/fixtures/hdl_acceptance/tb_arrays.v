// Hand-written Verilog testbench for fixed-length arrays.
//
// Every expected value is built here by slicing and concatenating the raw bits
// the test drives, so the layout under test is never used to compute its own
// expectation: element 0 is written as the least significant slice by hand, and
// an out-of-range dynamic read is checked to be the standard unknown with a
// reduction XOR rather than with a SystemVerilog system function.
//
// The checks are a small set of discriminating vectors plus every index
// boundary, not an exhaustive combination of the whole design:
//
// * the constant group sweeps all 16 values of the element input,
// * the dynamic read visits indices 0, 1, 2 and the out-of-range 3,
// * the combinational write visits each index, including an out-of-range one
//   that must leave the defaulted array alone,
// * the sequential write walks a short sequence so that an element written
//   earlier is still visible next to one written later, and an out-of-range
//   index keeps every element on its old value,
// * the nesting group sweeps all 16 values of a signed tag through an array of
//   structs, with a struct holding an array checked beside it.
//
// Verilog-2005 only. Protocol: exactly one `SHYHDL-AC-PASS arrays ...` line on
// success, otherwise `SHYHDL-AC-FAIL arrays ...` lines.
//
// Expects the frontend-generated modules: ArrConst, ArrDynRead, ArrDynWrite,
// ArrNest, ArrTop.

`timescale 1ns/1ps

module tb_arrays;
    localparam integer EXPECTED_CHECKS = 79;

    reg  [3:0]  v4;
    reg  [1:0]  i2;
    reg  [11:0] src12;
    reg  [15:0] cells16;
    reg  [8:0]  b9;
    reg         clk, rst_n;
    reg  [11:0] expected_comb;
    reg         k1, j1, one_in;
    reg  [2:0]  j3;
    reg  [1:0]  m2;
    reg  [1:0]  oi2, ii2, fi2;
    reg  [15:0] n16, expect_nest, expect_field;
    reg  [7:0]  outer8;
    reg  [3:0]  expect4;
    reg  [3:0]  element0;

    wire [11:0] repeated, listed, copied, comb_out, seq_out;
    wire [15:0] nested;
    wire [15:0] nest_out, field_out;
    wire [7:0]  sub;
    wire [3:0]  elem0, elem2, y, b0, b1, nested_elem, comb_read, snap, nested_dyn;
    wire [7:0]  tags, vals, tag0_ext, cell0;
    wire        bflag, one_bit, bit_from_element;
    wire [3:0]  cast_y;

    wire [11:0] t_listed, t_comb_out, t_seq_out;
    wire [7:0]  t_tags, t_vals;

    integer a, b, i, checks, failures;

    ArrConst dutc (
        .v(v4),
        .k(k1),
        .m(m2),
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

    ArrDynRead dutr (.src(src12), .i(i2), .one(one_in), .j(j1), .j3(j3), .y(y),
                     .one_bit(one_bit), .bit_from_element(bit_from_element),
                     .cast_y(cast_y));

    ArrDynWrite dutw (
        .v(v4),
        .i(i2),
        .oi(oi2),
        .ii(ii2),
        .fi(fi2),
        .clk(clk),
        .rst_n(rst_n),
        .comb_out(comb_out),
        .comb_read(comb_read),
        .seq_out(seq_out),
        .snap(snap),
        .nest_out(nest_out),
        .field_out(field_out)
    );

    ArrNest dutn (
        .cells(cells16),
        .b(b9),
        .tags(tags),
        .vals(vals),
        .tag0_ext(tag0_ext),
        .b0(b0),
        .b1(b1),
        .bflag(bflag),
        .cell0(cell0)
    );

    ArrTop dut_top (
        .v(v4),
        .i(i2),
        .k(k1),
        .m(m2),
        .j(j1),
        .one(one_in),
        .oi(oi2),
        .ii(ii2),
        .fi(fi2),
        .src(src12),
        .cells(cells16),
        .b(b9),
        .clk(clk),
        .rst_n(rst_n),
        .listed(t_listed),
        .comb_out(t_comb_out),
        .seq_out(t_seq_out),
        .tags(t_tags),
        .vals(t_vals)
    );

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL arrays %0s v=%h i=%0d src=%h cells=%h b=%h got=%h want=%h",
                             what, v4, i2, src12, cells16, b9, got, want);
            end
        end
    endtask

    // The standard unknown, detected in plain Verilog-2005: a reduction XOR is
    // `x` as soon as one bit is `x`.
    task check_unknown;
        input [255:0] what;
        input [7:0]   got;
        begin
            if ((^got) !== 1'bx) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL arrays %0s should be unknown, got=%h", what, got);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        v4 = 4'h0;
        i2 = 2'd0;
        src12 = 12'hC84;
        cells16 = 16'h0000;
        b9 = 9'h000;
        clk = 1'b0;
        rst_n = 1'b0;
        k1 = 1'b0;
        m2 = 2'd0;
        j1 = 1'b0;
        one_in = 1'b0;

        // Constant construction: repeat, list, constant element write, a
        // whole-array copy of the same shape, a half-open sub-array and reads.
        for (i = 0; i < 16; i = i + 1) begin
            v4 = i[3:0];
            #1;
            checks = checks + 1;
            check_value("const.repeat", repeated, { v4, v4, v4 });
            check_value("const.list-with-constant-write", listed, { 4'h2, 4'h7, v4 });
            check_value("const.copy-same-shape", copied, { 4'h2, 4'h7, v4 });
            check_value("const.sub-half-open", sub, { 4'h7, v4 });
            check_value("const.elem0", elem0, v4);
            check_value("const.elem2", elem2, 4'h2);
            check_value("top.const-list", t_listed, { 4'h2, 4'h7, v4 });
            // The nested literal is `[[v; 2], [2, 3]]`, so the outer element 0
            // holds the repeat in its low eight bits.
            check_value("const.nested-literal", nested, { 4'h3, 4'h2, v4, v4 });
            check_value("const.nested-index", nested_elem, 4'h2);
        end

        // A dynamic index read: each element in turn, then one past the end.
        src12 = { 4'hC, 4'h8, 4'h4 };
        for (i = 0; i < 4; i = i + 1) begin
            i2 = i[1:0];
            #1;
            checks = checks + 1;
            case (i2)
                2'd0: check_value("dynread.index0", y, src12[3:0]);
                2'd1: check_value("dynread.index1", y, src12[7:4]);
                2'd2: check_value("dynread.index2", y, src12[11:8]);
                default: check_unknown("dynread.out-of-range-is-unknown", y);
            endcase
        end

        // The combinational dynamic write: defaulted whole array, then the
        // written element, including an index that has no element.
        for (i = 0; i < 4; i = i + 1) begin
            i2 = i[1:0];
            v4 = 4'h5;
            if (i == 3)
                v4 = 4'hF;
            expected_comb = { 4'h3, 4'h2, 4'h1 };
            if (i2 < 2'd3)
                expected_comb[i2 * 4 +: 4] = v4;
            #1;
            checks = checks + 1;
            check_value("combwrite.default-plus-dynamic", comb_out, expected_comb);
            check_value("combwrite.out-of-range-keeps-default", t_comb_out, expected_comb);
            // Reading the array back inside the same process, after the write,
            // is what shows the source order; an index with no element reads the
            // unknown instead.
            if (i2 < 2'd3)
                check_value("combwrite.read-back-after-write", comb_read, v4);
            else
                check_unknown("combwrite.read-back-out-of-range", comb_read);
        end

        // The nested dynamic write: `[[1,2],[3,4]]` is defaulted first, then the
        // element both indices select. An out-of-range inner index must not spill
        // into the element beside it, and an out-of-range outer index must leave
        // the whole array alone.
        v4 = 4'h7;
        for (a = 0; a < 3; a = a + 1) begin
            oi2 = a[1:0];
            for (b = 0; b < 3; b = b + 1) begin
                ii2 = b[1:0];
                #1;
                checks = checks + 1;
                expect_nest = { 4'h4, 4'h3, 4'h2, 4'h1 };
                if (a < 2 && b < 2)
                    expect_nest[{ a[0], b[0] } * 4 +: 4] = v4;
                check_value("nestwrite.default-plus-dynamic", nest_out, expect_nest);
            end
        end

        // The dynamic field write: only `val` of the selected struct changes, so
        // the tag of that struct and the whole struct beside it keep the default,
        // and an index with no element changes nothing at all.
        for (a = 0; a < 3; a = a + 1) begin
            fi2 = a[1:0];
            #1;
            checks = checks + 1;
            // `[Cell{tag:1,val:2}, Cell{tag:3,val:4}]`: element 0 is the low
            // byte, and inside a `Cell` the tag is the high nibble, so the array
            // is `{4'h3, 4'h4, 4'h1, 4'h2}` and the write replaces the low nibble
            // of the selected element's own low nibble.
            expect_field = { 4'h3, 4'h4, 4'h1, 4'h2 };
            if (a == 0)
                expect_field[3:0] = v4;
            else if (a == 1)
                expect_field[11:8] = v4;
            check_value("fieldwrite.only-the-selected-field", field_out, expect_field);
        end

        // The sequential dynamic write: one element per edge, the others on
        // their old value, and an out-of-range index changing nothing.
        rst_n = 1'b0;
        #1;
        checks = checks + 1;
        check_value("seqwrite.reset", seq_out, 12'h000);
        check_value("top.seqwrite.reset", t_seq_out, 12'h000);
        check_value("seqwrite.snapshot-registered", snap, 4'h0);
        rst_n = 1'b1;

        i2 = 2'd1;
        v4 = 4'h5;
        clk = 1'b1;
        #1;
        clk = 1'b0;
        #1;
        checks = checks + 1;
        check_value("seqwrite.first-element-old-state", seq_out, { 4'h0, 4'h5, 4'h0 });
        check_value("top.seqwrite.first-element", t_seq_out, { 4'h0, 4'h5, 4'h0 });
        // The element that was just written reads as its old value at this edge.
        check_value("seqwrite.snapshot-is-old-value", snap, 4'h0);

        i2 = 2'd2;
        v4 = 4'h9;
        clk = 1'b1;
        #1;
        clk = 1'b0;
        #1;
        checks = checks + 1;
        check_value("seqwrite.second-element-keeps-first", seq_out, { 4'h9, 4'h5, 4'h0 });
        check_value("top.seqwrite.second-element", t_seq_out, { 4'h9, 4'h5, 4'h0 });
        check_value("seqwrite.snapshot-second-is-old", snap, 4'h0);

        i2 = 2'd1;
        v4 = 4'hA;
        clk = 1'b1;
        #1;
        clk = 1'b0;
        #1;
        checks = checks + 1;
        check_value("seqwrite.rewrite-keeps-second", seq_out, { 4'h9, 4'hA, 4'h0 });
        check_value("top.seqwrite.rewrite", t_seq_out, { 4'h9, 4'hA, 4'h0 });
        // Element 1 held 5 before this edge, so the snapshot reads 5 rather than
        // the 0xA the same edge is writing.
        check_value("seqwrite.snapshot-rewrite-is-old", snap, 4'h5);

        i2 = 2'd3;
        v4 = 4'hF;
        clk = 1'b1;
        #1;
        clk = 1'b0;
        #1;
        checks = checks + 1;
        check_value("seqwrite.out-of-range-no-effect", seq_out, { 4'h9, 4'hA, 4'h0 });
        check_value("top.seqwrite.out-of-range", t_seq_out, { 4'h9, 4'hA, 4'h0 });
        check_unknown("seqwrite.snapshot-out-of-range-is-unknown", snap);

        // An array of structs with a signed field, next to a struct that holds an
        // array. The tags are swept over every nibble so the sign of the field
        // matters.
        b9 = { 4'h4, 4'h3, 1'b1 };
        for (i = 0; i < 16; i = i + 1) begin
            cells16 = { 4'h9, 4'hD, i[3:0], 4'h6 };
            #1;
            checks = checks + 1;
            check_value("nest.tags-of-cells", tags, { 4'h9, i[3:0] });
            check_value("nest.vals-of-cells", vals, { 4'hD, 4'h6 });
            check_value("nest.signed-field-sext", tag0_ext, { { 4{i[3]} }, i[3:0] });
            check_value("nest.struct-array-item0", b0, 4'h3);
            check_value("nest.struct-array-item1", b1, 4'h4);
            check_value("nest.struct-array-flag", bflag, 1'b1);
            check_value("nest.struct-element-round-trip", cell0, { i[3:0], 4'h6 });
            check_value("top.nest-tags", t_tags, { 4'h9, i[3:0] });
            check_value("top.nest-vals", t_vals, { 4'hD, 4'h6 });
        end

        // A one-element array: index 0 is the element, index 1 has none and has
        // to read the unknown rather than the element at index 0.
        for (i = 0; i < 4; i = i + 1) begin
            one_in = i[1];
            j1 = i[0];
            #1;
            checks = checks + 1;
            if (i[0] == 1'b0)
                check_value("one-element.index0-is-the-element", one_bit, one_in);
            else
                check_unknown("one-element.index1-is-unknown", one_bit);
        end

        // `src[0][j3]`: the bit select applies to the low four-bit element, not
        // to the whole array, and index 4 and beyond read the unknown rather
        // than the neighbouring element. The low nibble is 4'b1010 and the next
        // one is non-zero, so a spill would be visible.
        src12 = { 4'hF, 4'hD, 4'b1010 };
        // The element is materialised first, because a bit select cannot follow
        // a part select in one Verilog index chain.
        element0 = src12[3:0];
        for (i = 0; i < 6; i = i + 1) begin
            j3 = i[2:0];
            #1;
            checks = checks + 1;
            if (j3 < 3'd4)
                check_value("bitselect.bit-of-element0", bit_from_element,
                            element0[j3]);
            else
                check_unknown("bitselect.past-element-is-unknown", bit_from_element);
        end

        // The same element select through a signed index cast back to unsigned:
        // every in-range index must agree with the plain read of `src[i]`, and
        // the out-of-range one is the unknown. The signed pattern of index 2 and
        // 3 is negative, so a signed offset would pick the wrong element.
        for (i = 0; i < 4; i = i + 1) begin
            i2 = i[1:0];
            #1;
            checks = checks + 1;
            if (i2 < 2'd3)
                check_value("cast-index.matches-plain-read", cast_y, src12[i2 * 4 +: 4]);
            else
                check_unknown("cast-index.out-of-range-is-unknown", cast_y);
        end

        // A nested array with a dynamic inner index: the outer index picks a
        // half, the inner one a nibble of it, and an inner index with no element
        // reads the unknown instead of the nibble beside it.
        v4 = 4'h8;
        for (a = 0; a < 2; a = a + 1) begin
            k1 = a[0];
            for (b = 0; b < 4; b = b + 1) begin
                m2 = b[1:0];
                #1;
                checks = checks + 1;
                // The nested literal is `[[v; 2], [2, 3]]`, sliced by hand.
                n16 = { 4'h3, 4'h2, v4, v4 };
                outer8 = k1 ? n16[15:8] : n16[7:0];
                expect4 = (b == 0) ? outer8[3:0] : outer8[7:4];
                if (b < 2)
                    check_value("nested-dyn.inner-element", nested_dyn, expect4);
                else
                    check_unknown("nested-dyn.inner-out-of-range-is-unknown", nested_dyn);
            end
        end

        if (failures == 0 && checks == EXPECTED_CHECKS)
            $display("SHYHDL-AC-PASS arrays checks=%0d", checks);
        else
            $display("SHYHDL-AC-FAIL arrays failures=%0d checks=%0d expected_checks=%0d",
                     failures, checks, EXPECTED_CHECKS);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL arrays timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
