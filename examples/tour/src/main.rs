//! A small system split across Rust modules: `datapath` and `select` hold the
//! hardware, `top` wires them, and `main` drives export, simulation and debug.
//!
//! Run it with `cargo run -p shyhdl-example-tour`.

mod datapath;
mod select;
mod top;

use shyhdl::*;
use top::TourTop;

fn main() -> shyhdl::Result<()> {
    let text = verilog::<TourTop>()?;
    println!("TourTop: {} lines of Verilog", text.lines().count());

    // One module of the same design, by module name.
    for name in ["Datapath_N8", "Datapath8"] {
        if let Ok(module) = module_verilog::<TourTop>(name) {
            println!("{name}: {} lines", module.lines().count());
            break;
        }
    }

    let mut sim = simulate::<TourTop>("build/tour")?;
    let result = sim.run(top::tour_smoke)?;
    println!("tour_smoke: {:?}", result.outcome);
    if result.outcome != Outcome::Pass {
        return Err(Error::NotImplemented {
            feature: "the tour test did not pass",
        });
    }

    let dbg = Debugger::new(&result, 10)?;
    println!("debug start on port :{}", dbg.port);
    if std::env::var_os("SHYHDL_DEBUG").is_some() {
        // Blocks while one GDB session replays this recording.
        dbg.run(result)?;
    }
    Ok(())
}
