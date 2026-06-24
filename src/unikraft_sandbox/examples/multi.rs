//! Multi-snippet demo: run several *different* Python workloads through one sandbox, each
//! immediately repeated to show the execution model.
//!
//! Because the code is baked into the boot argv, a *new* code string evolves a fresh VM
//! (cold), while an immediate *repeat* of the same code is a warm `restore` + `call_run`.
//! Each line prints `cold=<ms> warm=<ms>` followed by the snippet's captured stdout.
//!
//! Run (after `just build && just rootfs` in `examples/python`):
//! ```text
//! cargo run --example multi
//! ```

use anyhow::Result;
use hyperlight_sandbox::{SandboxBuilder, ToolRegistry};
use hyperlight_unikraft_sandbox::Unikraft;
use std::path::PathBuf;

fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../hyperlight-unikraft/examples")
        .join("python");
    let kernel = root.join(".unikraft/build/python-hyperlight_hyperlight-x86_64");
    let initrd = root.join("initrd.cpio");

    if !kernel.exists() || !initrd.exists() {
        eprintln!(
            "SKIP — build the python image first: (cd examples/python && just build && just rootfs)"
        );
        return Ok(());
    }

    let mut sandbox = SandboxBuilder::new()
        .with_tools(ToolRegistry::new())
        .guest(Unikraft::new(kernel).initrd(initrd))
        .build()?;

    // Three distinct real workloads. Each is run twice: the first call evolves a fresh VM,
    // the second of the same code is a warm restore.
    let snippets = [
        (
            "primes",
            "print('primes<30:', [n for n in range(2, 30) if all(n % d for d in range(2, n))])",
        ),
        (
            "json",
            "import json; print('json:', json.dumps({'a': 1, 'b': [2, 3]}, separators=(',', ':')))",
        ),
        (
            "math",
            "import math; print('pi:', round(math.pi, 6), '10! =', math.factorial(10))",
        ),
    ];

    for (label, code) in snippets {
        let t = std::time::Instant::now();
        let out = sandbox.run(code)?; // cold: distinct code -> fresh evolve
        let cold_ms = t.elapsed().as_millis();

        let t = std::time::Instant::now();
        let _ = sandbox.run(code)?; // warm: same code -> restore + call_run
        let warm_ms = t.elapsed().as_millis();

        eprintln!(
            "[{label}] cold={cold_ms}ms warm={warm_ms}ms exit={}",
            out.exit_code
        );
        print!("  {}", out.stdout);
    }

    Ok(())
}
