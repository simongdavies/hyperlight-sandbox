//! Boot a Unikraft guest image and run a Python snippet through the
//! `hyperlight-unikraft` backend of `hyperlight-sandbox`.
//!
//! This is the end-to-end smoke test. It needs a real Unikraft kernel + initrd whose entry
//! interpreter accepts `-c <code>` (the upstream `examples/python` image is the reference)
//! and a working hypervisor (`/dev/kvm` on Linux).
//!
//! The backend runs each call as `python3 -c <code>` (argv) — exactly like the
//! `hyperlight-unikraft --exec` CLI — and captures the guest console as `stdout`.
//!
//! Usage:
//! ```text
//! cargo run --example run -- <kernel> <initrd.cpio> ["<code>"]
//! ```
//!
//! ## Using the upstream python image (console-enabled)
//! ```text
//! cd hyperlight-unikraft/examples/python
//! just build && just rootfs           # kernel + initrd (or pull the prebuilt kernel)
//! cd ../../sandbox
//! cargo run --example run -- \
//!     ../examples/python/.unikraft/build/python-hyperlight_hyperlight-x86_64 \
//!     ../examples/python/initrd.cpio \
//!     "print('hello from a Unikraft micro-VM'); print(6 * 7)"
//! ```
//! The snippet's stdout is printed to your terminal. The example runs the same code twice
//! so you can see the cold evolve vs the warm restore in the two `[timing]` lines.
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

    // Run the same code twice to show the model: the first call evolves a fresh VM (kernel
    // boot + interpreter start-up); the second call of the *same* code is a warm restore.
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
