// ShyHDL simulation runner: the thin C++ main() that Verilator's --build links
// against the generated model. ShyHDL writes this file into the build directory;
// it is a build artifact and is not a hand-edited source file.
//
// Timing is entirely Verilator's: the loop jumps from one scheduled time slot to
// the next with VerilatedContext::time(contextp->nextTimeSlot()), evaluates the
// model, and dumps the waveform. Nothing on the Rust side models time.
//
//   argv[1]  VCD path, or "" to run without a waveform
//   argv[2]  maximum number of event steps (0 = unlimited)
//
// Exit codes: 0 normal (including a testbench that ended with $finish),
//             3 the step limit was hit (reported as a timeout).

#include <cinttypes>
#include <cstdio>
#include <cstdlib>
#include <memory>

#include <verilated.h>
#if VM_TRACE
#include <verilated_vcd_c.h>
#endif

#include "@@TOP_TYPE@@.h"

// Only needed so linking works with the legacy Verilator time API; the timing
// loop below uses VerilatedContext::time/nextTimeSlot.
double sc_time_stamp() { return 0; }

int main(int argc, char** argv) {
    const char* wave_path = (argc > 1) ? argv[1] : "";
    const uint64_t max_steps = (argc > 2) ? strtoull(argv[2], nullptr, 10) : 0;

    const std::unique_ptr<VerilatedContext> contextp{new VerilatedContext};
    contextp->commandArgs(argc, argv);
    contextp->traceEverOn(true);

    // "TOP" is the hierarchical scope name in the waveform: every DUT instance
    // is recorded below TOP.<testbench module>.<instance name>.
    const std::unique_ptr<@@TOP_TYPE@@> top{new @@TOP_TYPE@@{contextp.get(), "TOP"}};

#if VM_TRACE
    VerilatedVcdC* tfp = nullptr;
    if (wave_path[0] != '\0') {
        tfp = new VerilatedVcdC;
        top->trace(tfp, 99);
        // In Verilator 5 `open()` returns void; `isOpen()` reports whether the
        // file really could be created, and a run without a usable waveform is
        // reported instead of being recorded as an empty trace.
        tfp->open(wave_path);
        if (!tfp->isOpen()) {
            fprintf(stderr, "SHYHDL_RUNNER_ERROR cannot open waveform file %s\n", wave_path);
            fflush(stderr);
            top->final();
            return 2;
        }
    }
#endif

    uint64_t steps = 0;
    int status = 0;
    while (!contextp->gotFinish()) {
        top->eval();
#if VM_TRACE
        if (tfp) tfp->dump(contextp->time());
#endif
        if (!top->eventsPending()) break;
        const uint64_t next = top->nextTimeSlot();
        if (next <= contextp->time()) break;  // no progress possible
        contextp->time(next);
        if (max_steps != 0 && ++steps >= max_steps) {
            printf("SHYHDL_RUNNER_TIMEOUT steps=%" PRIu64 " time=%" PRIu64 "\n", steps,
                   contextp->time());
            fflush(stdout);
            status = 3;
            break;
        }
    }

    const uint64_t sim_time = contextp->time();
    const bool finished = contextp->gotFinish();
    top->final();
#if VM_TRACE
    if (tfp) {
        tfp->close();
        delete tfp;
    }
#endif
    printf("SHYHDL_RUNNER_DONE sim_time=%" PRIu64 " steps=%" PRIu64 " finish=%d\n", sim_time,
           steps, finished ? 1 : 0);
    fflush(stdout);
    return status;
}
