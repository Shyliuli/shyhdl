// Hand-written Verilog testbench for disjoint partial-width writes.
//
// `y[0..4]` is written from `a` and `y[4..8]` from `y[0..4]`. The ranges do not
// overlap, so this is legal hardware with no combinational loop even though a
// signal-level dependency graph contains a `y -> y` edge. The result is
// `y = { a, a }`, checked exhaustively over the 4-bit input space.
//
// This testbench is separate from `tb_overwrite.v` on purpose: if partial-width
// writes are not supported yet, that verdict must not hide the AC-MVP-012
// verdict, and vice versa.
//
// Protocol: exactly one `SHYHDL-AC-PASS partselect ...` line on success,
// otherwise `SHYHDL-AC-FAIL partselect ...` lines. A run with no verdict is a
// failure.
//
// Expects the frontend-generated module: PartSelect.

`timescale 1ns/1ps

module tb_partselect;
    localparam integer EXPECTED_CHECKS = 16;

    reg  [3:0] a;
    wire [7:0] y;

    integer i, checks, failures;

    PartSelect dut_partselect (.a(a), .y(y));

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL partselect %0s a=%h got=%h want=%h",
                             what, a, got, want);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        a = 4'h0;

        for (i = 0; i < 16; i = i + 1) begin
            a = i[3:0];
            #1;
            checks = checks + 1;
            check_value("partselect.y", y, {i[3:0], i[3:0]});
        end

        if (failures == 0 && checks == EXPECTED_CHECKS)
            $display("SHYHDL-AC-PASS partselect checks=%0d", checks);
        else
            $display("SHYHDL-AC-FAIL partselect failures=%0d checks=%0d expected_checks=%0d",
                     failures, checks, EXPECTED_CHECKS);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL partselect timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
