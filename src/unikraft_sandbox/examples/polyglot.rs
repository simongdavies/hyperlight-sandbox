//! Polyglot demo: run **Python** and **shell** snippets through the *same*
//! `hyperlight-unikraft` backend.
//!
//! The backend is guest-agnostic: only the kernel + initrd image changes, the host code is
//! identical. Each call is `interpreter -c <code>` (Python: `python3 -c`, shell: `sh -c`)
//! and the guest console is captured as `stdout`.
//!
//! Run (after `just build && just rootfs` in the two example dirs):
//! ```text
//! cargo run --example polyglot
//! ```
//! Images that are not built are skipped with a note.

use anyhow::Result;
use hyperlight_sandbox::{SandboxBuilder, ToolRegistry};
use hyperlight_unikraft_sandbox::Unikraft;
use std::path::PathBuf;

/// Resolve a built example image (kernel + initrd) relative to this crate, so the demo runs
/// regardless of the current working directory.
fn image(example: &str, kernel_name: &str) -> (PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../hyperlight-unikraft/examples")
        .join(example);
    (
        root.join(".unikraft/build").join(kernel_name),
        root.join("initrd.cpio"),
    )
}

/// Boot `code` on the given image and print the captured stdout. Skips cleanly if the image
/// has not been built yet.
fn demo(label: &str, kernel: PathBuf, initrd: PathBuf, heap_mib: u64, code: &str) -> Result<()> {
    if !kernel.exists() || !initrd.exists() {
        eprintln!("[{label}] SKIP — image not built ({})", kernel.display());
        return Ok(());
    }

    let mut sandbox = SandboxBuilder::new()
        .with_tools(ToolRegistry::new())
        .guest(
            Unikraft::new(kernel)
                .initrd(initrd)
                .heap_size(heap_mib * 1024 * 1024),
        )
        .build()?;

    let t = std::time::Instant::now();
    let out = sandbox.run(code)?;
    eprintln!(
        "[{label}] {}ms exit={}",
        t.elapsed().as_millis(),
        out.exit_code
    );
    print!("{}", out.stdout);
    Ok(())
}

fn main() -> Result<()> {
    println!("=== Python (python3 -c) ===");
    let (k, i) = image("python", "python-hyperlight_hyperlight-x86_64");
    demo(
        "python",
        k,
        i,
        512,
        "import sys; print('python', sys.version.split()[0]); print('sum(0..100) =', sum(range(101)))",
    )?;

    // NOTE: this unikernel shell has no `fork`, so command substitution `$(...)` and pipes
    // are unavailable — use builtins (arithmetic) and standalone external commands.
    println!("\n=== Shell (sh -c) ===");
    let (k, i) = image("shell", "shell-hyperlight_hyperlight-x86_64");
    demo(
        "shell",
        k,
        i,
        16,
        "echo shell-ok; echo math=$((6*7)); uname -srm; echo done",
    )?;

    Ok(())
}
