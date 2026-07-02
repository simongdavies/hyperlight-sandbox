//! A [`hyperlight-sandbox`] backend that runs guests inside **Unikraft micro-VMs**
//! on Hyperlight.
//!
//! This crate implements the host-agnostic [`Guest`] / [`GuestSandbox`] traits from
//! `hyperlight-sandbox` by wrapping [`hyperlight_unikraft::Sandbox`]. It is the
//! structural sibling of the `wasm` and `javascript` backends, but instead of an
//! in-process Wasm/JS engine it boots a Unikraft unikernel (e.g. CPython or Node.js)
//! inside a hardware-isolated VM and drives it through Hyperlight's function-call
//! interface.
//!
//! # Capability model
//!
//! Like the other backends, the *core* capabilities are bridged to the guest so that
//! behaviour is identical across backends. Each capability is registered as a host
//! function on Unikraft's single `__dispatch` channel via
//! [`SandboxBuilder::tool`](hyperlight_unikraft::SandboxBuilder::tool):
//!
//! | Guest call (`__dispatch` name) | Arguments                              | Backed by                              |
//! | ------------------------------ | -------------------------------------- | -------------------------------------- |
//! | `call_tool`                    | `{ "name": str, "args": any }`         | the core [`ToolRegistry`]              |
//! | `read_file`                    | `{ "path": str }`                      | the core [`CapFs`] (`/input`,`/output`)|
//! | `write_file`                   | `{ "path": str, "text"\|"bytes": .. }` | the core [`CapFs`] (`/output`)         |
//! | `fetch`                        | `{ "url", "method", "headers", "body" }`| host HTTP gated by [`NetworkPermissions`] |
//!
//! Unikraft wraps every tool result as `{ "result": <value> }` or `{ "error": <msg> }`,
//! so the in-guest driver should unwrap that envelope.
//!
//! The native Unikraft `fs_*` / `net_*` tools are deliberately **not** exposed: file and
//! network access flow through the core capability objects so the `/input`,`/output`
//! contract and network allow-list behave the same as on the wasm/js backends.
//!
//! # Execution model
//!
//! The guest is a **resident driver**: the kernel + initrd boot a long-lived interpreter
//! (e.g. CPython) that initialises once and then waits for work on Hyperlight's `run`
//! function. The driver boots lazily on the first [`run`](GuestSandbox::run) and stays
//! resident; each call delivers its code to the driver via
//! [`Sandbox::run_code`](hyperlight_unikraft::Sandbox::run_code), which rewinds to the warm
//! post-init snapshot, runs the code, and returns the guest exit code.
//!
//! So the first call pays the kernel boot + interpreter start-up (`build`, seconds) and
//! every subsequent call — *whatever the code* — is a warm rewind + run. [`snapshot`] /
//! [`restore`] checkpoint that warm VM.
//!
//! The guest still writes stdout / stderr to the VM console (port `0xE9`), which Hyperlight
//! routes to the host's `fd 2`. In-band, per-call capture of that output into the
//! [`ExecutionResult`] `stdout` is **not wired yet** (see the limitations below), so for now
//! `stdout` is returned empty and the `exit_code` is authoritative.
//!
//! Point [`Unikraft`] at a kernel + initrd built as a resident driver that exposes a `run`
//! function taking the code string (the `python-agent-driver` image is the reference).
//!
//! [`snapshot`]: GuestSandbox::snapshot
//! [`restore`]: GuestSandbox::restore
//!
//! # Known limitations / TODO
//!
//! This is an early backend; some shortcuts were taken to get an end-to-end Python run
//! working. Each is flagged at its site with a `TODO` and summarised here:
//!
//! - **`stdout` is not captured yet.** The guest console (port `0xE9`) is routed straight to
//!   the host's `fd 2`; the released crate's console redirect is a no-op on Windows, so the
//!   backend returns an empty `stdout` and relies on the `exit_code`. TODO (C2): have the
//!   resident driver return the captured output in-band per call — per-call and portable.
//! - **Capability globals require a guest preamble.** The host registers `call_tool` /
//!   `read_file` / `write_file` / `fetch` over `__dispatch`, but a plain interpreter image
//!   exposes no matching guest globals, so guest code cannot reach them yet — use a
//!   [`mount`](Unikraft::mount) for host filesystem access instead. TODO: ship a guest
//!   preamble that binds these globals.
//! - **Scoped credentials are ignored** by `fetch`. TODO: credential-aware outgoing HTTP.
//! - **Build wiring pins a fork branch:** the crate path-depends on the sibling
//!   `hyperlight-sandbox` core checkout, and git-depends on
//!   `simongdavies/hyperlight-unikraft` branch `feat/plex-run-capture` (the released 0.11.0
//!   base plus `run_code`). TODO: switch to the upstream/crates.io release once the run
//!   capture work is merged.
//!
//! [`hyperlight-sandbox`]: hyperlight_sandbox

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use hyperlight_sandbox::runtime::BlockOn;
use hyperlight_sandbox::{
    http as sandbox_http, CapFs, CredentialRegistry, ExecutionResult, Guest, GuestSandbox,
    HttpMethod, NetworkPermissions, SandboxConfig, Snapshot, ToolRegistry,
};
use hyperlight_unikraft::{
    Preopen, Sandbox as UnikraftVm, SandboxBuilder, Snapshot as UnikraftSnapshot,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Default guest heap size (512 MiB).
///
/// Matches `hyperlight-unikraft`'s own default and is large enough for CPython / Node.js
/// guests. We deliberately do **not** derive VM memory from [`SandboxConfig`], whose
/// defaults are sized for in-process Wasm/JS heaps (tens of MiB) and would be far too
/// small to boot a unikernel language runtime.
const DEFAULT_HEAP_SIZE: u64 = 512 * 1024 * 1024;

/// Default guest stack / scratch size (8 MiB).
const DEFAULT_STACK_SIZE: u64 = 8 * 1024 * 1024;

/// The Unikraft guest backend.
///
/// Carries the kernel + (optional) initrd image to boot and the VM sizing to use. Pass an
/// instance to [`SandboxBuilder::guest`](hyperlight_sandbox::SandboxBuilder::guest) (or
/// [`Sandbox::new`](hyperlight_sandbox::Sandbox::new)) to construct a sandbox.
///
/// ```no_run
/// use hyperlight_sandbox::{SandboxBuilder, ToolRegistry};
/// use hyperlight_unikraft_sandbox::Unikraft;
///
/// let mut sandbox = SandboxBuilder::new()
///     .with_tools(ToolRegistry::new())
///     .guest(Unikraft::new("python-kernel").initrd("python.cpio"))
///     .build()?;
/// let out = sandbox.run("print('hi from a micro-VM')")?;
/// println!("{}", out.stdout);
/// # Ok::<(), anyhow::Error>(())
/// ```
/// A loaded golden snapshot, shareable across many sandboxes.
///
/// Load it **once** per process with [`Unikraft::load_golden`], then hand a clone to every
/// [`Unikraft::from_golden`]: Hyperlight maps the golden memory copy-on-write, so N sandboxes
/// built from the same handle share its physical pages and only pay for what they dirty.
/// Cloning is cheap (an `Arc` bump).
#[derive(Clone)]
pub struct UnikraftGolden(Arc<UnikraftSnapshot>);

impl std::fmt::Debug for UnikraftGolden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnikraftGolden").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Unikraft {
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    heap_size: u64,
    stack_size: u64,
    initrd_base: Option<u64>,
    mounts: Vec<(PathBuf, String)>,
    /// When set, the sandbox is created from this warm golden via `from_snapshot` instead of
    /// cold-booting a kernel — much faster per exec and concurrency-safe.
    golden: Option<UnikraftGolden>,
}

impl Unikraft {
    /// Create a backend that boots `kernel`. Use [`initrd`](Self::initrd) to attach the
    /// guest filesystem image, which is usually required.
    pub fn new(kernel: impl Into<PathBuf>) -> Self {
        Self {
            kernel: kernel.into(),
            initrd: None,
            heap_size: DEFAULT_HEAP_SIZE,
            stack_size: DEFAULT_STACK_SIZE,
            initrd_base: None,
            mounts: Vec::new(),
            golden: None,
        }
    }

    /// Load a warm golden snapshot from `dir` (produced by `bake` / `pyhl setup`) into a
    /// shareable [`UnikraftGolden`] handle. Load once, then build many sandboxes from it via
    /// [`from_golden`](Self::from_golden) — they share the golden memory copy-on-write.
    pub fn load_golden(dir: impl AsRef<Path>) -> Result<UnikraftGolden> {
        let snapshot =
            UnikraftVm::load_snapshot(dir).context("failed to load golden snapshot directory")?;
        Ok(UnikraftGolden(snapshot))
    }

    /// Build sandboxes from a pre-loaded [`UnikraftGolden`] (see [`load_golden`](Self::load_golden))
    /// instead of cold-booting a kernel. The kernel — and, for an inline-baked golden, the initrd —
    /// are captured in the golden, so neither is re-mapped per sandbox. This is the fast,
    /// concurrency-safe path (a fresh VM per execution from an immutable shared golden).
    pub fn from_golden(golden: UnikraftGolden) -> Self {
        Self {
            // Unused on the golden path: the kernel is baked into the golden. Kept non-optional
            // so the cold-boot builder stays ergonomic; never read when `golden` is `Some`.
            kernel: PathBuf::new(),
            initrd: None,
            heap_size: DEFAULT_HEAP_SIZE,
            stack_size: DEFAULT_STACK_SIZE,
            initrd_base: None,
            mounts: Vec::new(),
            golden: Some(golden),
        }
    }

    /// Attach the initrd/rootfs CPIO image, mapped zero-copy into guest memory.
    pub fn initrd(mut self, path: impl Into<PathBuf>) -> Self {
        self.initrd = Some(path.into());
        self
    }

    /// Override the guest heap size in bytes (default 512 MiB).
    pub fn heap_size(mut self, bytes: u64) -> Self {
        self.heap_size = bytes;
        self
    }

    /// Override the guest stack / scratch size in bytes (default 8 MiB).
    pub fn stack_size(mut self, bytes: u64) -> Self {
        self.stack_size = bytes;
        self
    }

    /// Override the guest virtual address where the mapped initrd is placed. Leave unset to
    /// use the host default (3 GiB). Newer `plat-hyperlight` kernels (e.g. `hostfs-posix-py`)
    /// expect it just below 4 GiB (`0xFEF0_0000`); set this to match the kernel, or the VM
    /// traps on an unmapped MMIO read during init.
    pub fn initrd_base(mut self, base: u64) -> Self {
        self.initrd_base = Some(base);
        self
    }

    /// Expose a host directory to the guest at `guest_path` (a `hostfs` mount). Unmodified
    /// POSIX file I/O under `guest_path` in the guest is forwarded to `host_dir`. Repeatable.
    pub fn mount(mut self, host_dir: impl Into<PathBuf>, guest_path: impl Into<String>) -> Self {
        self.mounts.push((host_dir.into(), guest_path.into()));
        self
    }
}

impl Guest for Unikraft {
    type Sandbox = UnikraftGuestSandbox;

    fn build(
        self,
        // VM sizing lives on `Unikraft` (see `DEFAULT_HEAP_SIZE`), so the generic
        // SandboxConfig heap/stack — tuned for in-process Wasm/JS — is not used here.
        _config: SandboxConfig,
        tools: ToolRegistry,
        network: Arc<Mutex<NetworkPermissions>>,
        fs: Arc<Mutex<CapFs>>,
        // Scoped credentials are injected on the WASI-HTTP outgoing path used by the
        // wasm backend; the Unikraft `fetch` tool does not consume them yet. Accepted
        // for trait-compatibility.
        // TODO: credential-aware `fetch` (inject scoped credential headers, as the wasm backend does).
        _credentials: CredentialRegistry,
    ) -> Result<UnikraftGuestSandbox> {
        UnikraftGuestSandbox::new(self, tools, network, fs)
    }
}

/// A built Unikraft sandbox. Holds the boot configuration and capability bridge. The guest
/// driver boots once (lazily, on first [`run`](GuestSandbox::run)) and stays **resident**;
/// each subsequent run delivers its code via `Sandbox::run_code` against the warm post-init
/// snapshot, so there is no per-code re-evolve.
pub struct UnikraftGuestSandbox {
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    heap_size: u64,
    stack_size: u64,
    initrd_base: Option<u64>,
    preopens: Vec<Preopen>,
    tools: Arc<ToolRegistry>,
    network: Arc<Mutex<NetworkPermissions>>,
    fs: Arc<Mutex<CapFs>>,
    /// When set, the VM is created from this shared warm golden (`from_snapshot`) rather than
    /// cold-booted. The kernel/initrd baked into the golden are not re-mapped per sandbox.
    golden: Option<UnikraftGolden>,
    /// The resident micro-VM, booted lazily on first run and warm-reused thereafter.
    vm: Option<UnikraftVm>,
}

/// Options accepted by the host `fetch` tool. Mirrors the JS backend's `FetchOptions`.
#[derive(Deserialize)]
struct FetchOptions {
    #[serde(default = "default_get_method")]
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

fn default_get_method() -> String {
    "GET".to_string()
}

/// Build the host-side `__dispatch` tool registry that bridges the core sandbox
/// capabilities (tool registry, filesystem, network) into the Unikraft guest.
///
/// Shared by the cold-boot and snapshot-load paths so custom tools behave identically
/// either way. NOTE: the stock `hl_pydriver` does not yet expose matching guest globals
/// (`call_tool`/`read_file`/`write_file`/`fetch`) over `__dispatch`, so guest code cannot
/// reach these until the driver gains a sandbox preamble. The `hostfs` POSIX path (file
/// I/O via [`Unikraft::mount`]) works today regardless.
fn register_host_tools(
    mut builder: SandboxBuilder,
    tools: Arc<ToolRegistry>,
    network: Arc<Mutex<NetworkPermissions>>,
    fs: Arc<Mutex<CapFs>>,
) -> SandboxBuilder {
    // call_tool: forward to the core ToolRegistry.
    {
        let tools = tools.clone();
        builder = builder.tool("call_tool", move |args: Value| -> Result<Value> {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("call_tool: missing 'name'"))?;
            let tool_args = args.get("args").cloned().unwrap_or(Value::Null);
            tools.dispatch(name, tool_args)
        });
    }

    // read_file: core CapFs (read-only `/input`, writable `/output`).
    {
        let fs = fs.clone();
        builder = builder.tool("read_file", move |args: Value| -> Result<Value> {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("read_file: missing 'path'"))?;
            let files = fs
                .lock()
                .map_err(|_| anyhow!("filesystem mutex poisoned"))?;
            let data = files.read_guest_file(path)?;
            Ok(json!({ "data": data }))
        });
    }

    // write_file: text or bytes -> core CapFs `/output`.
    {
        let fs = fs.clone();
        builder = builder.tool("write_file", move |args: Value| -> Result<Value> {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("write_file: missing 'path'"))?
                .to_string();
            let data = if let Some(text) = args.get("text").and_then(Value::as_str) {
                text.as_bytes().to_vec()
            } else if let Some(bytes) = args.get("bytes") {
                serde_json::from_value::<Vec<u8>>(bytes.clone())
                    .context("write_file: 'bytes' must be an array of byte values")?
            } else {
                return Err(anyhow!("write_file: requires 'text' or 'bytes'"));
            };
            let mut files = fs
                .lock()
                .map_err(|_| anyhow!("filesystem mutex poisoned"))?;
            files.write_output_path(&path, data)?;
            Ok(json!({ "ok": true }))
        });
    }

    // fetch: host-side outbound HTTP, gated by the core NetworkPermissions.
    {
        let network = network.clone();
        builder = builder.tool("fetch", move |args: Value| -> Result<Value> {
            let url_str = args
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("fetch: missing 'url'"))?;
            let opts: FetchOptions =
                serde_json::from_value(args.clone()).context("fetch: invalid options")?;

            let parsed = url::Url::parse(url_str)
                .with_context(|| format!("fetch: invalid URL {url_str}"))?;
            let method: HttpMethod = opts.method.parse().map_err(|e| anyhow!("{e}"))?;

            // Enforce the network allow-list before making any connection.
            {
                let net = network
                    .lock()
                    .map_err(|_| anyhow!("network mutex poisoned"))?;
                if !net.is_allowed(&parsed, &method) {
                    return Ok(json!({
                        "status": 403,
                        "headers": {},
                        "body": format!("HTTP request denied for {method} {parsed}"),
                    }));
                }
            }

            if opts.headers.len() > sandbox_http::MAX_RESPONSE_HEADER_COUNT {
                return Err(anyhow!("fetch: too many request headers"));
            }

            let request = sandbox_http::HttpRequest {
                url: parsed,
                method: method.to_string(),
                headers: opts.headers.into_iter().collect(),
                body: sandbox_http::HttpRequest::body_from_bytes(opts.body.map(String::into_bytes)),
            };

            match sandbox_http::send_http_request(request).block_on() {
                Ok(resp) => {
                    let body = String::from_utf8(resp.body)
                        .unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned());
                    Ok(json!({
                        "status": resp.status,
                        "headers": resp.headers,
                        "body": body,
                    }))
                }
                Err(error) => Ok(json!({
                    "status": 502,
                    "headers": {},
                    "body": format!("{error}"),
                })),
            }
        });
    }

    builder
}

impl UnikraftGuestSandbox {
    fn new(
        backend: Unikraft,
        tools: ToolRegistry,
        network: Arc<Mutex<NetworkPermissions>>,
        fs: Arc<Mutex<CapFs>>,
    ) -> Result<Self> {
        // The `fetch` tool drives async HTTP via `send_http_request().block_on()`, which
        // needs the shared tokio runtime. Fail fast (and clearly) if it is unavailable.
        hyperlight_sandbox::runtime::RUNTIME
            .as_ref()
            .map_err(|e| anyhow!("{e}"))?;

        let Unikraft {
            kernel,
            initrd,
            heap_size,
            stack_size,
            initrd_base,
            mounts,
            golden,
        } = backend;

        // Map requested host directories to guest `hostfs` preopens once; applied when the
        // resident VM is built.
        let preopens: Vec<Preopen> = mounts
            .iter()
            .map(|(host, guest)| Preopen::new(host, guest.clone()))
            .collect::<Result<_>>()?;

        Ok(Self {
            kernel,
            initrd,
            heap_size,
            stack_size,
            initrd_base,
            preopens,
            tools: Arc::new(tools),
            network,
            fs,
            golden,
            vm: None,
        })
    }

    /// Boot the resident guest driver once: a kernel + runtime with the host capability
    /// tools and any preopened mounts wired in, but **no code in argv**. `build()` runs the
    /// driver's init and captures the post-init warm snapshot that
    /// [`run_code`](hyperlight_unikraft::Sandbox::run_code) rewinds to before each call.
    fn evolve(&self) -> Result<UnikraftVm> {
        // Golden (from_snapshot) path: create a fresh VM from the shared warm snapshot. The
        // kernel and (for an inline golden) the initrd are baked into the golden, so neither is
        // cold-booted or re-mapped. This is the fast, concurrency-safe construction.
        //
        // NOTE: custom host tools (`call_tool`/`fetch`) are not yet wired on this path — the
        // underlying `from_snapshot` builds only the built-in result-capture + hostfs tools, so
        // stdout/stderr capture and `hostfs` mounts work, but the SDK ToolRegistry bridge does
        // not (same limitation as the cold-boot `__dispatch` bridge TODO). Sufficient for the
        // tool-less execution MVP; full parity needs a tools-aware `from_snapshot` upstream.
        if let Some(golden) = &self.golden {
            return UnikraftVm::from_snapshot(
                golden.0.clone(),
                &self.preopens,
                self.initrd.clone(),
                None,
                None,
            )
            .context("failed to create Unikraft VM from golden snapshot");
        }

        let mut builder = UnikraftVm::builder(&self.kernel)
            .heap_size(self.heap_size)
            .stack_size(self.stack_size);
        // Bridge the core capabilities onto the builder's `__dispatch` host functions. The
        // released hyperlight-unikraft exposes per-tool `.tool()`, not the fork's bulk
        // `with_tools(registry)`.
        builder = register_host_tools(
            builder,
            self.tools.clone(),
            self.network.clone(),
            self.fs.clone(),
        );
        // `initrd_base` is a fork-only builder option absent from the released crate, which
        // places the initrd internally; accept-and-ignore to keep the public setter working.
        let _ = self.initrd_base;
        if let Some(initrd) = &self.initrd {
            builder = builder.initrd_file(initrd.clone());
        }
        for preopen in &self.preopens {
            builder = builder.preopen(preopen.clone());
        }
        builder.build().context("failed to boot Unikraft VM")
    }

    fn run_impl(&mut self, code: &str) -> Result<ExecutionResult> {
        // Boot the resident driver on first use; warm-reuse it thereafter.
        if self.vm.is_none() {
            self.vm = Some(self.evolve()?);
        }

        // Clear the core `/output` staging area (a no-op when only hostfs mounts are used).
        if let Ok(mut files) = self.fs.lock() {
            files.clear_output_files();
        }

        let vm = self.vm.as_mut().expect("resident VM booted above");

        // `run_code` rewinds to the post-init warm snapshot, delivers `code` to the resident
        // driver via the `run` guest function, and returns the guest's captured stdout/stderr
        // (posted by the driver's `__hl_result` tool) plus its exit code. Against an older
        // driver that does not post `__hl_result`, stdout/stderr come back empty.
        match vm.run_code(code) {
            Ok(output) => Ok(ExecutionResult {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.exit_code,
            }),
            // A guest trap surfaces as a failed execution rather than a hard error, matching
            // the JS backend.
            Err(error) => Ok(ExecutionResult {
                stdout: String::new(),
                stderr: error.to_string(),
                exit_code: -1,
            }),
        }
    }
}

impl GuestSandbox for UnikraftGuestSandbox {
    // Unikraft rewinds to a single internal checkpoint, so there is no per-snapshot
    // payload to hand back to the caller.
    type SnapshotData = ();

    fn run(&mut self, code: &str) -> Result<ExecutionResult> {
        self.run_impl(code)
    }

    fn snapshot(&mut self) -> Result<Snapshot<()>> {
        // Re-capture the current guest state of the live VM as the checkpoint that future
        // `restore` calls rewind to. NOTE: only one checkpoint is retained, so a later
        // `snapshot` supersedes an earlier one — multi-snapshot stacking is not supported.
        let vm = self
            .vm
            .as_mut()
            .ok_or_else(|| anyhow!("snapshot() called before any run()"))?;
        vm.snapshot_now().context("failed to capture snapshot")?;
        Ok(Snapshot::new("hyperlight-unikraft", Arc::new(())))
    }

    fn restore(&mut self, _snapshot: &Snapshot<()>) -> Result<()> {
        let vm = self
            .vm
            .as_mut()
            .ok_or_else(|| anyhow!("restore() called before any run()"))?;
        vm.restore().context("failed to restore snapshot")
    }
}

#[cfg(test)]
mod tests {
    //! These tests cover the host-side wire contracts and builder wiring; they do not
    //! boot a VM, so they run without a hypervisor.
    use super::*;

    #[test]
    fn fetch_options_default_to_get() {
        let o: FetchOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o.method, "GET");
        assert!(o.headers.is_empty());
        assert!(o.body.is_none());

        let o: FetchOptions =
            serde_json::from_str(r#"{"method":"POST","headers":{"x":"y"},"body":"hello"}"#)
                .unwrap();
        assert_eq!(o.method, "POST");
        assert_eq!(o.headers.get("x").map(String::as_str), Some("y"));
        assert_eq!(o.body.as_deref(), Some("hello"));
    }

    #[test]
    fn unikraft_defaults() {
        let u = Unikraft::new("k");
        assert_eq!(u.heap_size, DEFAULT_HEAP_SIZE);
        assert_eq!(u.stack_size, DEFAULT_STACK_SIZE);
        assert!(u.initrd.is_none());
    }

    #[test]
    fn unikraft_builder_overrides() {
        let u = Unikraft::new("kernel")
            .initrd("rootfs.cpio")
            .heap_size(64 << 20)
            .stack_size(4 << 20);
        assert_eq!(u.kernel, PathBuf::from("kernel"));
        assert_eq!(u.initrd, Some(PathBuf::from("rootfs.cpio")));
        assert_eq!(u.heap_size, 64 << 20);
        assert_eq!(u.stack_size, 4 << 20);
    }
}
