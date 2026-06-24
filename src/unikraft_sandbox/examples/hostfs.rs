//! hostfs demo: a guest writes files into a host directory and we read them back on the
//! **host** — transparent POSIX I/O across the VM boundary via Unikraft's `lib/hostfs`.
//!
//! Requires the `hostfs-posix-py` image. That is a newer `plat-hyperlight` kernel which
//! expects the initrd mapped just below 4 GiB, hence [`Unikraft::initrd_base`]:
//! ```text
//! cd hyperlight-unikraft/examples/hostfs-posix-py && just build && just rootfs
//! cd ../../sandbox && cargo run --example hostfs
//! ```

use anyhow::Result;
use hyperlight_sandbox::{SandboxBuilder, ToolRegistry};
use hyperlight_unikraft_sandbox::Unikraft;
use std::path::PathBuf;

/// Newer `plat-hyperlight` kernels (e.g. hostfs-posix-py) expect the initrd just below 4 GiB.
const HOSTFS_INITRD_BASE: u64 = 0xFEF0_0000;

/// Guest Python (single line for argv) that writes a report + a log line under /host using
/// only the stdlib — no SDK, no JSON, no hcall. Files close explicitly so writes flush.
const GUEST_CODE: &str = "import os; os.makedirs('/host/logs', exist_ok=True); \
f=open('/host/report.txt','w'); \
f.write('generated inside a Unikraft micro-VM\\nsum(0..100)=%d\\n' % sum(range(101))); \
f.close(); \
g=open('/host/logs/run.log','a'); g.write('ran once\\n'); g.close(); \
print('guest: wrote /host/report.txt + /host/logs/run.log')";

fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../hyperlight-unikraft/examples")
        .join("hostfs-posix-py");
    let kernel = root.join(".unikraft/build/hostfs-posix-py-hyperlight_hyperlight-x86_64");
    let initrd = root.join("hostfs-posix-py-initrd.cpio");
    if !kernel.exists() || !initrd.exists() {
        eprintln!(
            "SKIP — build the hostfs image first: \
             (cd examples/hostfs-posix-py && just build && just rootfs)"
        );
        return Ok(());
    }

    // A fresh host directory the guest will see (read/write) as /host.
    let work = std::env::temp_dir().join(format!("hl-hostfs-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    println!("host dir {} -> /host (guest)\n", work.display());

    let mut sandbox = SandboxBuilder::new()
        .with_tools(ToolRegistry::new())
        .guest(
            Unikraft::new(kernel)
                .initrd(initrd)
                .initrd_base(HOSTFS_INITRD_BASE)
                .mount(&work, "/host"),
        )
        .build()?;

    let out = sandbox.run(GUEST_CODE)?;
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);

    // Prove it on the HOST side: the files the guest wrote are really here.
    println!("\n--- host sees ---");
    println!(
        "report.txt:\n{}",
        std::fs::read_to_string(work.join("report.txt"))?
    );
    print!(
        "logs/run.log:\n{}",
        std::fs::read_to_string(work.join("logs/run.log"))?
    );

    Ok(())
}
