// CLI fixture: two `#[hardware_test]` functions, one with an explicit time
// setting and one with the default. `list-tests` only reads the attributes, so
// the bodies are empty; generating a testbench for them belongs to the
// testbench milestone and is not exercised by this file.

#[hardware_test]
fn counts_up() {}

#[hardware_test(timeunit = "10ns", precision = "100ps")]
fn waits_for_reset() {}
