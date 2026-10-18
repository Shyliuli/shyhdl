//! Optional NVBoard backend: run a design on the NJU virtual FPGA board.
//!
//! The design is compiled to Verilog as usual, then built into a second
//! Verilator model whose `main()` is the board loop instead of a testbench:
//! `nvboard_update()` applies the switch and button states, `eval()` settles
//! the design, and the next frame draws the LEDs. Switches drive the design's
//! inputs, LEDs show its outputs, so a combinational design can be exercised by
//! hand and a clocked one by flipping the clock input.
//!
//! NVBoard itself is vendored in `vendor/nvboard` and built into
//! `libnvboard.a` on first use; nothing outside this crate has to be installed
//! except SDL2, SDL2_image and SDL2_ttf, which NVBoard requires anyway.
//!
//! This module only exists when the `nvboard` feature is enabled.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Switch pins, least significant first. NVBoard's board file defines SW0-15.
const SWITCHES: [&str; 16] = [
    "SW0", "SW1", "SW2", "SW3", "SW4", "SW5", "SW6", "SW7", "SW8", "SW9", "SW10", "SW11", "SW12",
    "SW13", "SW14", "SW15",
];
/// LED pins, least significant first. NVBoard's board file defines LD0-15.
const LEDS: [&str; 16] = [
    "LD0", "LD1", "LD2", "LD3", "LD4", "LD5", "LD6", "LD7", "LD8", "LD9", "LD10", "LD11", "LD12",
    "LD13", "LD14", "LD15",
];

/// One port of the design top, as it appears in the generated Verilog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardPort {
    /// Port name in the generated Verilog.
    pub name: String,
    /// Bit width.
    pub width: u32,
    /// Input ports are driven by switches, outputs by LEDs.
    pub input: bool,
}

impl BoardPort {
    /// A port description.
    pub fn new(name: impl Into<String>, width: u32, input: bool) -> Self {
        Self {
            name: name.into(),
            width,
            input,
        }
    }
}

/// Everything the board needs to build and run one design.
#[derive(Debug, Clone)]
pub struct BoardOptions {
    /// Directory for the board model; created if needed.
    pub build_dir: PathBuf,
    /// Verilog module name of the design top.
    pub top: String,
    /// Generated Verilog of the design.
    pub verilog: String,
    /// Ports of that module, in declaration order.
    pub ports: Vec<BoardPort>,
    /// Input port to toggle as a clock, if the design has one.
    pub clock: Option<String>,
    /// Print the pin assignment and the "close the window" hint before the
    /// window opens.
    pub announce: bool,
}

/// A built board run.
#[derive(Debug, Clone)]
pub struct BoardRun {
    /// The board binary that was run.
    pub binary: PathBuf,
    /// Pin assignments, one line per port, in the order they were made.
    pub bindings: Vec<String>,
    /// Commands executed to get there, for diagnosis.
    pub commands: Vec<String>,
}

impl BoardRun {
    /// One line summary of the pin plan, printed by the CLI.
    pub fn summary(&self) -> String {
        format!(
            "board: {} ({} pin assignment{})",
            self.binary.display(),
            self.bindings.len(),
            if self.bindings.len() == 1 { "" } else { "s" }
        )
    }
}

/// One port-to-pin assignment, kept so the CLI can print the plan.
#[derive(Debug, Clone)]
struct Bind {
    signal: String,
    width: u32,
    pins: Vec<&'static str>,
    input: bool,
}

/// Everything resolved from the environment before a build starts.
#[derive(Debug, Clone)]
struct Tools {
    cxx: String,
    ar: String,
    verilator: PathBuf,
    sdl_cflags: Vec<String>,
    sdl_libs: Vec<String>,
}

/// Find the source of this crate's vendored NVBoard.
fn vendor_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/nvboard")
}

/// Assign switches to inputs and LEDs to outputs, in declaration order.
///
/// A port wider than the board is an error rather than a truncation: a decoder
/// with 32 outputs must not silently show its low half.
fn plan(ports: &[BoardPort], clock: Option<&str>) -> Result<Vec<Bind>> {
    let mut switches = 0usize;
    let mut leds = 0usize;
    let mut binds = Vec::new();

    for port in ports {
        if port.width == 0 {
            return Err(Error::Board {
                message: format!("port `{}` has zero width", port.name),
            });
        }
        if port.input && Some(port.name.as_str()) == clock {
            continue;
        }
        let width = port.width as usize;
        let (pool, used, pool_name) = if port.input {
            (&SWITCHES, &mut switches, "switches")
        } else {
            (&LEDS, &mut leds, "LEDs")
        };
        if width > 16 {
            return Err(Error::Board {
                message: format!(
                    "port `{}` is {width} bits wide and the board has only 16 {pool_name}",
                    port.name
                ),
            });
        }
        if *used + width > pool.len() {
            return Err(Error::Board {
                message: format!(
                    "port `{}` would use {}..{} of the 16 {pool_name}; the design needs more",
                    port.name, *used, *used + width - 1
                ),
            });
        }
        // NVBoard takes the pins most-significant first, like the .nxdc file.
        let pins = (0..width).rev().map(|bit| pool[*used + bit]).collect();
        *used += width;
        binds.push(Bind {
            signal: port.name.clone(),
            width: port.width,
            pins,
            input: port.input,
        });
    }

    if binds.is_empty() {
        return Err(Error::Board {
            message: "the design has no ports to put on the board".to_string(),
        });
    }
    Ok(binds)
}

/// Flags SDL2 development files report, from `sdl2-config` or `pkg-config`.
fn sdl_flags() -> Result<(Vec<String>, Vec<String>)> {
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let output = Command::new(program).args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let cflags = run("sdl2-config", &["--cflags"])
        .or_else(|| run("pkg-config", &["--cflags", "sdl2"]))
        .ok_or_else(|| Error::Board {
            message: "SDL2 development files not found: install SDL2, or put `sdl2-config` on \
                      PATH"
                .to_string(),
        })?;
    let libs = run("sdl2-config", &["--libs"])
        .or_else(|| run("pkg-config", &["--libs", "sdl2"]))
        .ok_or_else(|| Error::Board {
            message: "SDL2 development files not found: `sdl2-config --libs` failed".to_string(),
        })?;
    Ok((words(&cflags), words(&libs)))
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Link flags for NVBoard's two optional SDL helpers, which it always uses.
/// Turn `-L<dir>` into an rpath, so the board binary finds the same SDL
/// libraries it was linked against even when they live outside the default
/// search path.
fn rpath_flags(libs: &[String]) -> Vec<String> {
    libs.iter()
        .filter_map(|flag| flag.strip_prefix("-L"))
        .filter(|dir| !dir.is_empty())
        .map(|dir| format!("-Wl,-rpath,{dir}"))
        .collect()
}

/// Guess which input is the design clock, so a clocked design can be driven
/// from the board without a flag.
pub fn guess_clock(ports: &[BoardPort]) -> Option<String> {
    ports
        .iter()
        .find(|port| {
            port.input
                && port.width == 1
                && matches!(
                    port.name.to_ascii_lowercase().as_str(),
                    "clk" | "clock" | "clk_i" | "clock_i"
                )
        })
        .map(|port| port.name.clone())
}

fn sdl_helper_libs() -> Vec<String> {
    let pkg = Command::new("pkg-config")
        .args(["--libs", "SDL2_image", "SDL2_ttf"])
        .output();
    if let Ok(output) = pkg {
        if output.status.success() {
            return words(&String::from_utf8_lossy(&output.stdout));
        }
    }
    vec!["-lSDL2_image".to_string(), "-lSDL2_ttf".to_string()]
}

/// Include flags for SDL2_image / SDL2_ttf, when pkg-config knows them.
fn sdl_helper_cflags() -> Vec<String> {
    let pkg = Command::new("pkg-config")
        .args(["--cflags", "SDL2_image", "SDL2_ttf"])
        .output();
    if let Ok(output) = pkg {
        if output.status.success() {
            return words(&String::from_utf8_lossy(&output.stdout));
        }
    }
    Vec::new()
}

fn tools() -> Result<Tools> {
    let verilator = crate::simulation::Simulator::discover()?.verilator_path().to_path_buf();
    let (sdl_cflags, sdl_libs) = sdl_flags()?;
    Ok(Tools {
        cxx: std::env::var("CXX").unwrap_or_else(|_| "c++".to_string()),
        ar: std::env::var("AR").unwrap_or_else(|_| "ar".to_string()),
        verilator,
        sdl_cflags,
        sdl_libs,
    })
}

/// Build (or reuse) `libnvboard.a` from the vendored sources.
///
/// The archive lives in the system temporary directory, keyed by a digest of
/// the sources and the compiler flags, so every run but the first is instant
/// and the crate never writes into its own source tree.
fn build_nvboard(tools: &Tools) -> Result<PathBuf> {
    let vendor = vendor_dir();
    if !vendor.join("src").is_dir() {
        return Err(Error::Board {
            message: format!(
                "vendored NVBoard is missing at {}; the `nvboard` feature needs `vendor/nvboard`",
                vendor.display()
            ),
        });
    }

    let mut sources: Vec<PathBuf> = std::fs::read_dir(vendor.join("src"))
        .map_err(|source| Error::Io {
            path: vendor.join("src"),
            source,
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "cpp"))
        .collect();
    sources.sort();

    // NVBoard also includes SDL_image.h and SDL_ttf.h; those live in their own
    // include directories, which sdl2-config does not know about.
    let helper_cflags = sdl_helper_cflags();

    let mut fingerprint = Fnv::new();
    fingerprint.update(tools.cxx.as_bytes());
    fingerprint.update(tools.ar.as_bytes());
    for flag in tools
        .sdl_cflags
        .iter()
        .chain(tools.sdl_libs.iter())
        .chain(helper_cflags.iter())
    {
        fingerprint.update(flag.as_bytes());
    }
    for source in &sources {
        let bytes = std::fs::read(source).map_err(|error| Error::Io {
            path: source.clone(),
            source: error,
        })?;
        fingerprint.update(source.file_name().unwrap_or_default().as_encoded_bytes());
        fingerprint.update(&bytes);
    }
    let cache = std::env::temp_dir()
        .join("shyhdl-nvboard")
        .join(format!("{:016x}", fingerprint.finish()));
    let archive = cache.join("libnvboard.a");
    if archive.is_file() {
        return Ok(archive);
    }

    let objects = cache.join("obj");
    std::fs::create_dir_all(&objects).map_err(|source| Error::Io {
        path: objects.clone(),
        source,
    })?;
    let mut compiled = Vec::new();
    for source in &sources {
        let stem = source
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let object = objects.join(format!("{stem}.o"));
        let mut command = Command::new(&tools.cxx);
        command
            .arg("-O2")
            .arg("-fPIC")
            .arg("-I")
            .arg(vendor.join("include"))
            .arg("-I")
            .arg(vendor.join("usr/include"))
            .args(&tools.sdl_cflags)
            .args(&helper_cflags)
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(&object);
        run_quiet(&mut command, &format!("compiling NVBoard {}", source.display()))?;
        compiled.push(object);
    }

    let mut command = Command::new(&tools.ar);
    command.arg("rcs").arg(&archive).args(&compiled);
    run_quiet(&mut command, "archiving libnvboard.a")?;
    Ok(archive)
}

/// The generated `auto_bind.cpp`: the only place a pin name appears.
fn auto_bind(top: &str, binds: &[Bind]) -> String {
    let mut out = String::new();
    out.push_str("// Generated by ShyHDL: NVBoard pin assignment.\n");
    out.push_str("#include <nvboard.h>\n");
    out.push_str(&format!("#include \"V{top}.h\"\n\n"));
    out.push_str(&format!("void nvboard_bind_all_pins(V{top}* top) {{\n"));
    for bind in binds {
        let pins = bind
            .pins
            .iter()
            .map(|pin| format!(", {pin}"))
            .collect::<String>();
        out.push_str(&format!(
            "\tnvboard_bind_pin(&top->{}, {}, {});\n",
            bind.signal,
            bind.width,
            pins.trim_start_matches(", ")
        ));
    }
    out.push_str("}\n");
    out
}

/// The board model's `main()`: read the board, settle the design, redraw.
fn runner(top: &str, clock: Option<&str>) -> String {
    let clock_block = match clock {
        Some(clock) => format!(
            "    // Drive the design clock from the frame loop: one full period per two frames.\n\
             \x20   static unsigned tick = 0;\n\
             \x20   dut->{clock} = (tick++ >> 1) & 1u;\n\
             \x20   dut->eval();\n"
        ),
        None => String::new(),
    };
    format!(
        "// Generated by ShyHDL: NVBoard model of `{top}`. Do not edit.\n\
         #include <nvboard.h>\n\
         #include <SDL.h>\n\
         #include <verilated.h>\n\
         #include \"V{top}.h\"\n\n\
         void nvboard_bind_all_pins(V{top}* top);\n\n\
         int main(int argc, char** argv) {{\n\
         \x20   Verilated::commandArgs(argc, argv);\n\
         \x20   V{top}* dut = new V{top};\n\
         \x20   nvboard_bind_all_pins(dut);\n\
         \x20   nvboard_init();\n\n\
         \x20   for (;;) {{\n\
         \x20       nvboard_update();\n\
         \x20       dut->eval();\n\
         {clock_block}\
         \x20       // nvboard_update() only redraws 60 times a second; give the frame loop a\n\
         \x20       // little air so a combinational design does not spin a core flat.\n\
         \x20       SDL_Delay(2);\n\
         \x20   }}\n\
         }}\n"
    )
}

/// Build the board model and run it until the window is closed.
pub fn run(options: &BoardOptions) -> Result<BoardRun> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(Error::Board {
            message: "no DISPLAY: the board needs a graphical session to open its window"
                .to_string(),
        });
    }

    let top = options.top.clone();
    let clock = options.clock.clone().or_else(|| guess_clock(&options.ports));
    let binds = plan(&options.ports, clock.as_deref())?;
    if options.announce {
        for bind in &binds {
            eprintln!("{}", describe(bind));
        }
        if let Some(clock) = &clock {
            eprintln!("{clock} -> toggled by the frame loop");
        }
        eprintln!("building the board model ...");
    }

    let tools = tools()?;
    let archive = build_nvboard(&tools)?;

    let build_dir = &options.build_dir;
    std::fs::create_dir_all(build_dir).map_err(|source| Error::Io {
        path: build_dir.clone(),
        source,
    })?;
    let build_dir = std::fs::canonicalize(build_dir).map_err(|source| Error::Io {
        path: build_dir.clone(),
        source,
    })?;

    let verilog_path = build_dir.join(format!("{top}.v"));
    std::fs::write(&verilog_path, &options.verilog).map_err(|source| Error::Io {
        path: verilog_path.clone(),
        source,
    })?;
    let bind_path = build_dir.join("auto_bind.cpp");
    std::fs::write(&bind_path, auto_bind(&top, &binds)).map_err(|source| Error::Io {
        path: bind_path.clone(),
        source,
    })?;
    let runner_path = build_dir.join("board_runner.cpp");
    std::fs::write(&runner_path, runner(&top, clock.as_deref())).map_err(|source| {
        Error::Io {
            path: runner_path.clone(),
            source,
        }
    })?;

    let binary = build_dir.join("board");
    let vendor = vendor_dir();
    let cflags = format!(
        "-I{} -I{} -I{} {}",
        vendor.join("usr/include").display(),
        vendor.join("include").display(),
        build_dir.display(),
        tools.sdl_cflags.join(" ")
    );
    let mut link_libs = tools.sdl_libs.clone();
    link_libs.extend(sdl_helper_cflags().iter().filter(|flag| flag.starts_with("-L")).cloned());
    link_libs.extend(sdl_helper_libs());
    let mut ldflags = format!("{} {}", archive.display(), link_libs.join(" "));
    for rpath in rpath_flags(&link_libs) {
        ldflags.push(' ');
        ldflags.push_str(&rpath);
    }

    let obj_dir = build_dir.join("obj_dir");
    let mut command = Command::new(&tools.verilator);
    command
        .current_dir(&build_dir)
        .arg("--cc")
        .arg("--exe")
        .arg("--build")
        .arg("-O2")
        .arg("--x-assign")
        .arg("fast")
        .arg("--x-initial")
        .arg("fast")
        .arg("--noassert")
        .arg("-Wno-fatal")
        .arg("--top-module")
        .arg(&top)
        .arg("--Mdir")
        .arg(&obj_dir)
        .arg("-o")
        .arg(&binary)
        .arg("-CFLAGS")
        .arg(&cflags)
        .arg("-LDFLAGS")
        .arg(&ldflags)
        .arg(&verilog_path)
        .arg(&bind_path)
        .arg(&runner_path);
    let command_line = format!("{command:?}");
    if let Err(error) = run_quiet(&mut command, "building the board model with Verilator") {
        return Err(match error {
            Error::Board { message } => Error::Board {
                message: format!(
                    "{message}\n\nIf SDL2_image or SDL2_ttf are missing, NVBoard cannot link: \
                     install them, or point pkg-config at them."
                ),
            },
            other => other,
        });
    }

    if options.announce {
        eprintln!("close the board window to stop");
    }

    // NVBoard draws through SDL2's X11 path. Under a Wayland session XWayland is
    // what actually hosts the window, and letting SDL pick its video driver can
    // end in a window the compositor never composites (it looks transparent), so
    // pin the driver unless the caller overrides it. The render driver is left
    // alone: the software fallback already works when GL is unavailable.
    let mut child = Command::new(&binary);
    child
        .current_dir(&build_dir)
        .env("NVBOARD_HOME", &vendor)
        .env("SDL_VIDEO_X11_NET_WM_BYPASS_COMPOSITOR", "0")
        .env(
            "SDL_VIDEODRIVER",
            std::env::var_os("SDL_VIDEODRIVER").unwrap_or_else(|| "x11".into()),
        );
    let status = child.status().map_err(|source| Error::Io {
        path: binary.clone(),
        source,
    })?;
    if !status.success() {
        return Err(Error::Board {
            message: format!("the board exited with {status}"),
        });
    }

    Ok(BoardRun {
        binary,
        bindings: binds.iter().map(describe).collect(),
        commands: vec![command_line],
    })
}

fn describe(bind: &Bind) -> String {
    let pins = bind.pins.join(", ");
    format!(
        "{}[{}:0] -> {} ({})",
        bind.signal,
        bind.width - 1,
        pins,
        if bind.input { "switch" } else { "LED" }
    )
}

/// Run a tool, quoting its output in the error when it fails.
fn run_quiet(command: &mut Command, what: &str) -> Result<()> {
    let output = command.output().map_err(|source| Error::Board {
        message: format!("{what}: {source}"),
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: String = stderr
        .lines()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Err(Error::Board {
        message: format!("{what} failed:\n{tail}"),
    })
}

/// FNV-1a, used only to key the build cache.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
