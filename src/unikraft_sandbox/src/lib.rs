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
//! Each [`run`](GuestSandbox::run) launches the guest interpreter with the code passed as
//! argv — `<code_flag> <code>`, e.g. `python3 -c <code>` (the default; use `-e` for Node).
//! This mirrors the `hyperlight-unikraft` CLI's `--exec` flag. The guest writes stdout /
//! stderr to the VM console (port `0xE9`), which Hyperlight routes to the host; the backend
//! captures it for the duration of the call and returns it as the [`ExecutionResult`]
//! `stdout`.
//!
//! Because the code is baked into the boot argv, the VM is (re-)evolved when the code
//! changes. A VM is cached and **warm-reused** while the code is unchanged: the first call
//! pays the kernel boot + interpreter start-up (`build`, seconds), and subsequent calls of
//! the same code are a fast `restore` + `call_run` (hundreds of ms). [`snapshot`] /
//! [`restore`] checkpoint that warm VM.
//!
//! Point [`Unikraft`] at any kernel + initrd whose entry interpreter accepts the
//! `<code_flag> <code>` convention (the upstream `examples/python` image is the reference).
//!
//! [`snapshot`]: GuestSandbox::snapshot
//! [`restore`]: GuestSandbox::restore
//!
//! # Known limitations / TODO
//!
//! This is an early backend; some shortcuts were taken to get an end-to-end Python run
//! working. Each is flagged at its site with a `TODO` and summarised here:
//!
//! - **Distinct code re-evolves the VM (~seconds).** The code is baked into the boot argv,
//!   so each new code string boots a fresh VM. Repeats of the same code are warm (~hundreds
//!   of ms). TODO: a resident-driver image that accepts code per call (over `__dispatch`)
//!   would make every call warm.
//! - **Console capture is process-global.** stdout is captured by redirecting the host
//!   process's `fd 2` (see [`stderr_capture`]) around each call. A process-wide lock
//!   serialises concurrent sandboxes, and on Windows the redirect is a no-op (no capture).
//!   TODO: an in-guest capture returned from the call would be per-call and portable.
//! - **Capability globals require a guest preamble.** The host registers `call_tool` /
//!   `read_file` / `write_file` / `fetch` over `__dispatch`, but a plain interpreter image
//!   (e.g. `examples/python`) exposes no matching guest globals, so guest code cannot reach
//!   them yet — use a [`mount`](Unikraft::mount) for host filesystem access instead. TODO:
//!   ship a guest preamble that binds these globals.
//! - **Scoped credentials are ignored** by `fetch`. TODO: credential-aware outgoing HTTP.
//! - **Build wiring is local-checkout-specific:** the crate path-depends on sibling
//!   checkouts of `hyperlight-sandbox` (currently its `feat/scoped-credentials` branch)
//!   and `hyperlight-unikraft/host`, and pins toolchain 1.94 (the core needs ≥1.92 while
//!   the host repo pins 1.89). TODO: switch to versioned/git deps for release.
//!
//! [`hyperlight-sandbox`]: hyperlight_sandbox

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use hyperlight_sandbox::runtime::BlockOn;
use hyperlight_sandbox::{
    http as sandbox_http, CapFs, CredentialRegistry, ExecutionResult, Guest, GuestSandbox,
    HttpMethod, NetworkPermissions, SandboxConfig, Snapshot, ToolRegistry,
};
use hyperlight_unikraft::{
    stderr_capture, Preopen, Sandbox as UnikraftVm, ToolRegistry as UnikraftTools,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Default interpreter flag used to pass the code string as argv (e.g. `python3 -c <code>`,
/// `node -e <code>`). Override with [`Unikraft::code_flag`].
const DEFAULT_CODE_FLAG: &str = "-c";

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
#[derive(Debug, Clone)]
pub struct Unikraft {
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    code_flag: String,
    heap_size: u64,
    stack_size: u64,
    initrd_base: Option<u64>,
    mounts: Vec<(PathBuf, String)>,
}

impl Unikraft {
    /// Create a backend that boots `kernel`. Use [`initrd`](Self::initrd) to attach the
    /// guest filesystem image, which is usually required.
    pub fn new(kernel: impl Into<PathBuf>) -> Self {
        Self {
            kernel: kernel.into(),
            initrd: None,
            code_flag: DEFAULT_CODE_FLAG.to_string(),
            heap_size: DEFAULT_HEAP_SIZE,
            stack_size: DEFAULT_STACK_SIZE,
            initrd_base: None,
            mounts: Vec::new(),
        }
    }

    /// Attach the initrd/rootfs CPIO image, mapped zero-copy into guest memory.
    pub fn initrd(mut self, path: impl Into<PathBuf>) -> Self {
        self.initrd = Some(path.into());
        self
    }

    /// Override the interpreter flag used to pass code as argv. Defaults to `"-c"`
    /// (CPython / `sh`); use `"-e"` for a Node.js guest (`node -e <code>`).
    pub fn code_flag(mut self, flag: impl Into<String>) -> Self {
        self.code_flag = flag.into();
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

/// A built Unikraft sandbox. Holds the boot configuration and capability bridge; each
/// [`run`](GuestSandbox::run) (re-)evolves a micro-VM with the code baked into argv and
/// captures its console output. The VM is cached and warm-reused while the code is unchanged.
pub struct UnikraftGuestSandbox {
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    code_flag: String,
    heap_size: u64,
    stack_size: u64,
    initrd_base: Option<u64>,
    preopens: Vec<Preopen>,
    tools: Arc<ToolRegistry>,
    network: Arc<Mutex<NetworkPermissions>>,
    fs: Arc<Mutex<CapFs>>,
    current: Option<CurrentVm>,
}

/// A live VM together with the code it was evolved for, enabling warm restore + re-run.
struct CurrentVm {
    code: String,
    vm: UnikraftVm,
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

/// Escape `code` so the guest's argparse tokeniser preserves it as a single argv entry,
/// regardless of embedded whitespace or quotes. Wraps in `"..."` and backslash-escapes
/// internal `\` and `"` — mirrors the `hyperlight-unikraft` CLI's `--exec` handling.
fn argparse_escape(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + 4);
    out.push('"');
    for ch in code.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Build the host-side `__dispatch` tool registry that bridges the core sandbox
/// capabilities (tool registry, filesystem, network) into the Unikraft guest.
///
/// Shared by the cold-boot and snapshot-load paths so custom tools behave identically
/// either way. NOTE: the stock `hl_pydriver` does not yet expose matching guest globals
/// (`call_tool`/`read_file`/`write_file`/`fetch`) over `__dispatch`, so guest code cannot
/// reach these until the driver gains a sandbox preamble. The `hostfs` POSIX path (file
/// I/O via [`Unikraft::mount`]) works today regardless.
fn build_host_tools(
    tools: Arc<ToolRegistry>,
    network: Arc<Mutex<NetworkPermissions>>,
    fs: Arc<Mutex<CapFs>>,
) -> UnikraftTools {
    let mut reg = UnikraftTools::new();

    // call_tool: forward to the core ToolRegistry.
    {
        let tools = tools.clone();
        reg.register("call_tool", move |args: Value| -> Result<Value> {
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
        reg.register("read_file", move |args: Value| -> Result<Value> {
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
        reg.register("write_file", move |args: Value| -> Result<Value> {
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
        reg.register("fetch", move |args: Value| -> Result<Value> {
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

    reg
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
            code_flag,
            heap_size,
            stack_size,
            initrd_base,
            mounts,
        } = backend;

        // Map requested host directories to guest `hostfs` preopens once; re-applied on
        // every evolve.
        let preopens: Vec<Preopen> = mounts
            .iter()
            .map(|(host, guest)| Preopen::new(host, guest.clone()))
            .collect::<Result<_>>()?;

        Ok(Self {
            kernel,
            initrd,
            code_flag,
            heap_size,
            stack_size,
            initrd_base,
            preopens,
            tools: Arc::new(tools),
            network,
            fs,
            current: None,
        })
    }

    /// Evolve a fresh micro-VM with `code` baked into the interpreter argv
    /// (`<code_flag> <code>`), wiring the host capability tools and any preopened mounts.
    /// `build()` boots the kernel + runtime and captures the post-init warm snapshot that
    /// [`run_impl`](Self::run_impl) rewinds to before each `call_run`.
    fn evolve_for(&self, code: &str) -> Result<UnikraftVm> {
        let registry = build_host_tools(self.tools.clone(), self.network.clone(), self.fs.clone());

        // `--exec`-style invocation: the guest interpreter is launched as
        // `<code_flag> <code>` (e.g. `python3 -c <code>`). The code is argparse-escaped so
        // the guest tokeniser keeps it as a single argv entry regardless of spaces/quotes.
        let args = vec![self.code_flag.clone(), argparse_escape(code)];

        let mut builder = UnikraftVm::builder(&self.kernel)
            .args(args)
            .heap_size(self.heap_size)
            .stack_size(self.stack_size)
            .with_tools(registry);
        if let Some(base) = self.initrd_base {
            builder = builder.initrd_base(base);
        }
        if let Some(initrd) = &self.initrd {
            builder = builder.initrd_file(initrd.clone());
        }
        for preopen in &self.preopens {
            builder = builder.preopen(preopen.clone());
        }
        builder.build().context("failed to evolve Unikraft VM")
    }

    fn run_impl(&mut self, code: &str) -> Result<ExecutionResult> {
        // (Re-)evolve only when the code changes: the baked-in argv means a distinct code
        // needs a fresh boot, while a repeat of the same code is a fast warm restore.
        if self
            .current
            .as_ref()
            .map(|c| c.code != code)
            .unwrap_or(true)
        {
            let vm = self.evolve_for(code)?;
            self.current = Some(CurrentVm {
                code: code.to_string(),
                vm,
            });
        }

        // Clear the core `/output` staging area (a no-op when only hostfs mounts are used).
        if let Ok(mut files) = self.fs.lock() {
            files.clear_output_files();
        }

        let current = self.current.as_mut().expect("current VM set above");

        // Rewind to the post-init warm snapshot, then run the app via `call_run`. Unikraft
        // routes the guest console (stdout/stderr, port 0xE9) to the host's fd 2, so we
        // redirect fd 2 to a temp file for the duration of the call and read it back as
        // `stdout`. `stderr_capture` serialises this with a process-wide lock.
        //
        // TODO: process-global capture is not concurrency-friendly and is a no-op on
        // Windows; an in-guest capture returned from the call would be per-call and portable.
        current.vm.restore().context("failed to rewind warm VM")?;
        current.vm.reset_exit_code();

        let capture_path = std::env::temp_dir().join(format!(
            "hl-unikraft-sandbox-{}-{:?}.out",
            std::process::id(),
            std::thread::current().id(),
        ));
        let capture = stderr_capture::Capture::redirect_to_file(&capture_path)?;
        let call_result = current.vm.call_run();
        // Restore stderr before reading the captured file or doing anything else.
        capture.restore()?;

        let stdout = std::fs::read_to_string(&capture_path).unwrap_or_default();
        let _ = std::fs::remove_file(&capture_path);
        let exit_code = current.vm.last_exit_code();

        match call_result {
            Ok(()) => Ok(ExecutionResult {
                stdout,
                stderr: String::new(),
                exit_code,
            }),
            // A guest trap surfaces as a failed execution (with whatever console output
            // preceded it) rather than a hard error, matching the JS backend.
            Err(error) => Ok(ExecutionResult {
                stdout,
                stderr: error.to_string(),
                exit_code: if exit_code != 0 { exit_code } else { -1 },
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
        let current = self
            .current
            .as_mut()
            .ok_or_else(|| anyhow!("snapshot() called before any run()"))?;
        current
            .vm
            .snapshot_now()
            .context("failed to capture snapshot")?;
        Ok(Snapshot::new("hyperlight-unikraft", Arc::new(())))
    }

    fn restore(&mut self, _snapshot: &Snapshot<()>) -> Result<()> {
        let current = self
            .current
            .as_mut()
            .ok_or_else(|| anyhow!("restore() called before any run()"))?;
        current.vm.restore().context("failed to restore snapshot")
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
        assert_eq!(u.code_flag, DEFAULT_CODE_FLAG);
        assert_eq!(u.heap_size, DEFAULT_HEAP_SIZE);
        assert_eq!(u.stack_size, DEFAULT_STACK_SIZE);
        assert!(u.initrd.is_none());
    }

    #[test]
    fn unikraft_builder_overrides() {
        let u = Unikraft::new("kernel")
            .initrd("rootfs.cpio")
            .code_flag("-e")
            .heap_size(64 << 20)
            .stack_size(4 << 20);
        assert_eq!(u.kernel, PathBuf::from("kernel"));
        assert_eq!(u.initrd, Some(PathBuf::from("rootfs.cpio")));
        assert_eq!(u.code_flag, "-e");
        assert_eq!(u.heap_size, 64 << 20);
        assert_eq!(u.stack_size, 4 << 20);
    }

    #[test]
    fn argparse_escape_wraps_and_escapes() {
        assert_eq!(argparse_escape("print(1)"), "\"print(1)\"");
        assert_eq!(argparse_escape(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
