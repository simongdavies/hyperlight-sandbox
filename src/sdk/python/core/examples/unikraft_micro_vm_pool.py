"""
Micro-VM pool demo — Unikraft guest, ``from_snapshot`` execution model.

Runs (untrusted) Python inside a per-execution Hyperlight micro-VM. A warm golden snapshot is
loaded **once**; every execution is then served by building a **fresh** sandbox from that shared
golden, running the code, and dropping the VM. This is "Model A" — one micro-VM per execution:

  * **Hermetic by construction** — each run starts from the immutable golden, so no state,
    filesystem, or resource leaks between executions (even a tenant's own successive runs).
  * **Concurrency-safe** — building from a shared golden does not collide (unlike reusing one
    warm VM via ``restore()``), so a pool of worker threads runs guests in parallel.
  * **Fast** — the golden is captured *warm* (post-interpreter-init), so a run pays only the VM
    create + snapshot restore, not a fresh CPython start-up, on every call.

The pool uses one dedicated worker thread per concurrency slot: a ``Sandbox`` is ``!Send`` (it
may only be touched by the thread that created it), so each worker builds, runs, and drops its
own sandboxes. The shared golden handle *is* safe to share across threads (copy-on-write).

Usage (produce the golden once — see ``examples/README.md``), then::

    # PowerShell
    $env:HL_GOLDEN = "C:\\path\\to\\golden"      # an inline-baked golden directory
    python unikraft_micro_vm_pool.py

    # bash
    HL_GOLDEN=/path/to/golden python unikraft_micro_vm_pool.py

Optional: ``HL_POOL_SIZE`` (default 2) sets the number of concurrent micro-VMs.
"""

from __future__ import annotations

import os
import queue
import statistics
import sys
import threading
import time
from concurrent.futures import Future
from dataclasses import dataclass

from hyperlight_sandbox import ExecutionResult, Sandbox, load_golden

# Number of concurrent micro-VMs (worker threads) unless HL_POOL_SIZE overrides it.
DEFAULT_POOL_SIZE = 2
# Sequential warm-latency samples.
WARM_RUNS = 10
# Concurrency test fires POOL_SIZE * CONCURRENCY_FACTOR jobs at the pool at once.
CONCURRENCY_FACTOR = 4
# Generous per-execution timeout (seconds); a warm exec is well under a second, but the very
# first execution in a process also pays a one-off VM-platform warm-up.
RUN_TIMEOUT_S = 120.0
# Upper bound on pool start-up (worker warm-up) before we give up.
READY_TIMEOUT_S = 600.0


@dataclass
class _Job:
    code: str
    future: "Future[ExecutionResult]"


class MicroVMPool:
    """A pool of worker threads, each serving executions from a shared warm golden snapshot.

    Every execution builds a fresh micro-VM from the golden, runs the code, and drops the VM —
    one VM per execution. The golden's memory is shared copy-on-write across all of them.
    """

    def __init__(self, golden: object, size: int = DEFAULT_POOL_SIZE, warmup: bool = True) -> None:
        if size < 1:
            raise ValueError("pool size must be >= 1")
        self._golden = golden
        self._size = size
        self._jobs: "queue.Queue[_Job | None]" = queue.Queue()
        # Workers report readiness (or a start-up failure) so the pool never hangs on a crash.
        self._ready: "queue.Queue[tuple[int, BaseException | None]]" = queue.Queue()
        self._workers: list[threading.Thread] = []
        for i in range(size):
            t = threading.Thread(
                target=self._worker_loop, args=(i, warmup), name=f"microvm-{i}", daemon=True
            )
            t.start()
            self._workers.append(t)

    def _worker_loop(self, idx: int, warmup: bool) -> None:
        # The first `from_snapshot` in a process warms the host VM platform (on Windows/WHP the
        # partition + surrogate machinery). Pay that once, up front, so it never lands on a
        # caller's first real execution.
        try:
            if warmup:
                warm = Sandbox(backend="unikraft", golden=self._golden)
                warm.run("pass")
                del warm
            self._ready.put((idx, None))
        except BaseException as exc:  # noqa: BLE001 - surface start-up failures to the caller
            self._ready.put((idx, exc))
            return

        while True:
            job = self._jobs.get()
            if job is None:  # shutdown sentinel
                self._jobs.task_done()
                break
            try:
                # Fresh micro-VM per execution, from the shared golden, dropped afterwards.
                sandbox = Sandbox(backend="unikraft", golden=self._golden)
                job.future.set_result(sandbox.run(job.code))
                del sandbox
            except BaseException as exc:  # noqa: BLE001 - propagate to the submitter
                job.future.set_exception(exc)
            finally:
                self._jobs.task_done()

    def wait_until_ready(self, timeout: float | None = None) -> None:
        """Block until every worker has warmed up. Raises if any worker failed to start."""
        for _ in range(self._size):
            _idx, exc = self._ready.get(timeout=timeout)
            if exc is not None:
                raise RuntimeError("a micro-VM pool worker failed to initialise") from exc

    def submit(self, code: str) -> "Future[ExecutionResult]":
        """Enqueue ``code`` for the next free worker; returns a Future of the result."""
        fut: "Future[ExecutionResult]" = Future()
        self._jobs.put(_Job(code, fut))
        return fut

    def run(self, code: str, timeout: float | None = None) -> ExecutionResult:
        """Submit ``code`` and wait for its result."""
        return self.submit(code).result(timeout)

    def shutdown(self, join_timeout: float = 30.0) -> None:
        """Signal all workers to stop and join them."""
        for _ in self._workers:
            self._jobs.put(None)
        for t in self._workers:
            t.join(timeout=join_timeout)


def _resolve_golden_dir() -> str:
    golden = os.environ.get("HL_GOLDEN") or (sys.argv[1] if len(sys.argv) > 1 else None)
    if not golden:
        sys.exit(
            "No golden snapshot configured.\n"
            "  Set HL_GOLDEN=<golden dir> (or pass the directory as the first argument).\n"
            "  See examples/README.md to bake an inline golden once."
        )
    if not os.path.isdir(golden):
        sys.exit(f"Golden snapshot directory not found: {golden}")
    return golden


def main() -> None:
    golden_dir = _resolve_golden_dir()
    size = int(os.environ.get("HL_POOL_SIZE", DEFAULT_POOL_SIZE))

    print(f"loading golden snapshot from {golden_dir} ...", flush=True)
    golden = load_golden(golden_dir)

    pool = MicroVMPool(golden, size=size)
    t0 = time.perf_counter()
    pool.wait_until_ready(timeout=READY_TIMEOUT_S)
    print(f"pool of {size} micro-VM worker(s) ready in {time.perf_counter() - t0:.1f}s", flush=True)

    # --- Warm per-exec latency (one execution at a time) ---
    latencies = []
    for _ in range(WARM_RUNS):
        start = time.perf_counter()
        result = pool.run("print('x')", timeout=RUN_TIMEOUT_S)
        latencies.append((time.perf_counter() - start) * 1000.0)
        assert result.exit_code == 0, result
    print(
        f"warm per-exec (n={WARM_RUNS}): mean={statistics.mean(latencies):.0f}ms "
        f"min={min(latencies):.0f}ms max={max(latencies):.0f}ms",
        flush=True,
    )

    # --- Concurrency: fire size*CONCURRENCY_FACTOR jobs at once across the pool ---
    jobs = size * CONCURRENCY_FACTOR
    start = time.perf_counter()
    futures = [pool.submit("print('x')") for _ in range(jobs)]
    results = [f.result(timeout=RUN_TIMEOUT_S) for f in futures]
    wall_ms = (time.perf_counter() - start) * 1000.0
    assert all(r.exit_code == 0 for r in results)
    print(
        f"concurrency: {jobs} jobs on {size} micro-VM(s) -> wall={wall_ms:.0f}ms "
        f"({wall_ms / jobs:.0f}ms/job effective)",
        flush=True,
    )

    # --- pandas: a representative AI-tool workload (native deps in the guest) ---
    result = pool.run(
        "import pandas as pd; print(pd.DataFrame({'x': [1, 2, 3]}).sum().to_dict())",
        timeout=RUN_TIMEOUT_S,
    )
    print(f"pandas: exit={result.exit_code} stdout={result.stdout!r}", flush=True)

    # --- Hermeticity: state from one execution must not leak into the next ---
    pool.run("GLOBAL_LEAK = 42", timeout=RUN_TIMEOUT_S)
    leak = pool.run(
        "print('LEAK' if 'GLOBAL_LEAK' in dir() else 'CLEAN')", timeout=RUN_TIMEOUT_S
    )
    print(f"hermeticity: {leak.stdout.strip()} (expect CLEAN)", flush=True)

    pool.shutdown()
    print("done", flush=True)


if __name__ == "__main__":
    main()
