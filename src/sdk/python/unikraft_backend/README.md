# hyperlight-sandbox-backend-unikraft

Unikraft micro-VM backend for the `hyperlight-sandbox` Python SDK.

This package provides the native `UnikraftSandbox` class that the stable
`hyperlight_sandbox.Sandbox` API dispatches to when constructed with
`backend="unikraft"`. It wraps the `hyperlight-unikraft-sandbox` Rust backend, which boots a
Unikraft unikernel (e.g. CPython) inside a hardware-isolated Hyperlight micro-VM and drives
it through Hyperlight's function-call interface.

Unlike the Wasm backend (which loads a packaged guest module), the Unikraft backend boots a
**kernel + initrd** built as a resident driver — the `python-agent-driver` image is the
reference. The first `run()` pays kernel boot + interpreter start-up; each subsequent `run()`
is a warm rewind + run against the post-init snapshot.

## Usage

```python
from hyperlight_sandbox import Sandbox

sandbox = Sandbox(
    backend="unikraft",
    kernel="/path/to/kernel",
    initrd="/path/to/initrd.cpio",
    heap_size="1280Mi",  # CPython resident driver wants a large heap
)
result = sandbox.run("print('hello from a micro-VM')")
print(result.exit_code, result.stdout)
```

## Build (development)

```pwsh
just python python-install-backends   # builds all backends via `maturin develop`
# or, just this backend:
cd src/sdk/python/unikraft_backend; maturin develop
```

## Requirements

A working hypervisor (WHP on Windows, `/dev/kvm` on Linux) and a Unikraft resident-driver
kernel + initrd built to expose a `run` function taking the code string.
