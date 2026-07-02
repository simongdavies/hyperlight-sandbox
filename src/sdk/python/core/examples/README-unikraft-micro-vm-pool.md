# Unikraft micro-VM pool demo

[`unikraft_micro_vm_pool.py`](./unikraft_micro_vm_pool.py) runs (untrusted) Python inside a
**per-execution Hyperlight micro-VM**, using the `unikraft` backend's `from_snapshot` path:

- a warm **golden snapshot** is loaded **once**;
- every execution builds a **fresh** micro-VM from that shared golden, runs the code, and drops
  the VM — one VM per execution (**hermetic** and **concurrency-safe**);
- the golden is captured *warm* (post-interpreter-init), so each run pays only VM create +
  snapshot restore, not a fresh CPython start-up.

Measured on WHP (Windows), pool size 2, inline golden: **~0.6 s warm per-exec**, hermetic,
`pandas` works, concurrent across the pool.

## Prerequisites

- A hypervisor available to Hyperlight: **WHP** (Windows) or **KVM** (Linux).
- Python 3.10+ and the built SDK packages installed into your environment:
  - `hyperlight_sandbox` (core) — `pip install -e src/sdk/python/core`
  - `hyperlight_sandbox_backend_unikraft` — build with `maturin develop --release -m
    src/sdk/python/unikraft_backend/Cargo.toml`

## Produce the golden (once)

The golden is **produced, not shipped** (it is large and machine-specific). You need the
python-agent-driver **kernel + initrd**, then **bake** a warm *inline* golden from them.

1. **Get the kernel + initrd** using the `pyhl` tool from
   [`hyperlight-unikraft`](https://github.com/simongdavies/hyperlight-unikraft):

   ```sh
   # Pull the published python-agent-driver image (needs docker/podman on PATH):
   pyhl setup                      # installs kernel + *-initrd.cpio into ./.pyhl/
   # …or install from a local python-agent-driver build:
   pyhl setup --from <driver-build-dir>
   ```

2. **Bake a warm, inline golden** (initrd baked into the snapshot — no per-exec re-map, no
   fixed-address collision), using the `bake` example in that same repo:

   ```sh
   cargo run --release --example bake -- \
       ./.pyhl/kernel ./.pyhl/initrd.cpio ./golden 1280 inline
   ```

   This boots the driver, warms the interpreter, exercises the run path once, then saves the
   warm post-init snapshot to `./golden`.

> **Reproducibility note (WIP).** The `bake` example and its `inline` mode currently live on the
> `feat/plex-run-capture` branch of `hyperlight-unikraft`; they need to be committed/published on
> that branch for a clean `git clone → bake` on another machine. A `file`-mode golden (omit
> `inline`) also works but re-maps the initrd per exec.

## Run

```sh
# PowerShell
$env:HL_GOLDEN = "C:\path\to\golden"
python src/sdk/python/core/examples/unikraft_micro_vm_pool.py

# bash
HL_GOLDEN=/path/to/golden python src/sdk/python/core/examples/unikraft_micro_vm_pool.py
```

Optional: `HL_POOL_SIZE` (default `2`) sets the number of concurrent micro-VMs. The golden
directory may also be passed as the first positional argument instead of `HL_GOLDEN`.

Expected output (numbers vary by host):

```
pool of 2 micro-VM worker(s) ready in 7.4s
warm per-exec (n=10): mean=634ms  min=597ms  max=671ms
concurrency: 8 jobs on 2 micro-VM(s) -> wall=3503ms (438ms/job effective)
pandas: exit=0 stdout="{'x': 6}\n"
hermeticity: CLEAN
done
```

## Notes / current limitations

- **Custom host tools** (`call_tool` / `fetch`) are not yet wired on the `from_snapshot` path —
  it currently provides only the built-in stdout/stderr capture and `hostfs` mounts. That is
  sufficient for the tool-less execution MVP; full parity needs a tools-aware `from_snapshot`.
- Concurrency speed-up is partial: the guest **run** releases the GIL (real parallelism), but
  the VM **create** still holds it briefly, so N-way scaling is sub-linear today.
