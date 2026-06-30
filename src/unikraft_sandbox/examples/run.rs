//! Boot a Unikraft resident-driver image and run a Python snippet through the
//! `hyperlight-unikraft` backend of `hyperlight-sandbox`.
//!
//! This is the end-to-end smoke test. It needs a real Unikraft kernel + initrd built as a
//! resident driver that exposes a `run` function taking the code string (the
//! `python-agent-driver` image is the reference) and a working hypervisor (WHP on Windows,
//! `/dev/kvm` on Linux).
//!
//! The backend boots the driver once, then delivers each call's code via
//! `Sandbox::run_code` and returns the guest exit code. In-band `stdout` capture is not
//! wired yet (see the crate docs' C2 TODO), so the snippet's console output is not echoed —
//! the `[timing]` lines' `exit=` is the observable result.
//!
//! Usage:
//! ```text
//! cargo run --example run -- <kernel> <initrd.cpio> ["<code>"]
//! ```
//!
//! ## Using the python-agent-driver image
//! ```text
//! # A CPython resident driver wants a large heap; match the image's bake.
//! HL_UNIKRAFT_HEAP_MIB=1280 cargo run --example run -- \
//!     <hl-pub>/kernel <hl-pub>/initrd.cpio "import sys; sys.exit(7)"
//! ```
//! The example runs the snippet twice so you can see the cold boot (lazy driver start-up)
//! vs the warm `run_code` in the two `[timing]` lines.
//!
//! ## Optional: a host filesystem mount (hostfs images only, e.g. `hostfs-posix-py`)
//! ```text
//! HL_UNIKRAFT_MOUNT=/tmp/work:/host cargo run --example run -- <kernel> <initrd> \
//!     "open('/host/out.txt','w').write('hi')"
//! ```

use anyhow::{anyhow, Result};
use hyperlight_sandbox::{SandboxBuilder, ToolRegistry};
use hyperlight_unikraft_sandbox::Unikraft;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let kernel = args
        .next()
        .ok_or_else(|| anyhow!("usage: run <kernel> <initrd> [code]"))?;
    let initrd = args
        .next()
        .ok_or_else(|| anyhow!("usage: run <kernel> <initrd> [code]"))?;
    let code = args
        .next()
        .unwrap_or_else(|| "print('hello from a Unikraft micro-VM')".to_string());

    // The default 512 MiB guest heap suits the upstream `examples/python` image; override
    // via `HL_UNIKRAFT_HEAP_MIB` for heavier runtimes (numpy/pandas stacks want more).
    let mut guest = Unikraft::new(kernel).initrd(initrd);
    if let Ok(mib) = std::env::var("HL_UNIKRAFT_HEAP_MIB") {
        let mib: u64 = mib
            .parse()
            .map_err(|_| anyhow!("HL_UNIKRAFT_HEAP_MIB must be an integer number of MiB"))?;
        guest = guest.heap_size(mib * 1024 * 1024);
    }
    // `HL_UNIKRAFT_MOUNT=host_dir:guest_path` exposes a host directory over hostfs
    // (requires a hostfs-capable guest image, e.g. `hostfs-posix-py`).
    if let Ok(spec) = std::env::var("HL_UNIKRAFT_MOUNT") {
        let (host, guest_path) = spec
            .split_once(':')
            .ok_or_else(|| anyhow!("HL_UNIKRAFT_MOUNT must be 'host_dir:guest_path'"))?;
        guest = guest.mount(host, guest_path);
    }

    let mut sandbox = SandboxBuilder::new()
        .with_tools(ToolRegistry::new())
        .guest(guest)
        .build()?;

    // Run the snippet twice to show the model: the first call boots the resident driver
    // (kernel boot + interpreter start-up); the second call is a warm `run_code` against the
    // post-init snapshot — no re-boot, regardless of whether the code changed.
    let t = std::time::Instant::now();
    let cold = sandbox.run(&code)?;
    eprintln!(
        "[timing] cold run={}ms exit={}",
        t.elapsed().as_millis(),
        cold.exit_code
    );
    print!("{}", cold.stdout);
    eprint!("{}", cold.stderr);

    let t = std::time::Instant::now();
    let warm = sandbox.run(&code)?;
    eprintln!(
        "[timing] warm run={}ms exit={}",
        t.elapsed().as_millis(),
        warm.exit_code
    );
    print!("{}", warm.stdout);
    eprint!("{}", warm.stderr);

    std::process::exit(warm.exit_code);
}
