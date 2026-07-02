//! Python bindings for the **Unikraft micro-VM** backend of `hyperlight-sandbox`.
//!
//! This is the structural sibling of the `wasm` and `hyperlight-js` PyO3 backends. It wraps
//! [`hyperlight_sandbox::Sandbox`] parameterised over
//! [`hyperlight_unikraft_sandbox::Unikraft`] and exposes a [`UnikraftSandbox`] Python class
//! that the stable `hyperlight_sandbox.Sandbox` API dispatches to when
//! `backend="unikraft"`.
//!
//! Unlike the Wasm backend — which resolves a packaged guest module via `module_path` — the
//! Unikraft backend boots a **kernel + initrd** built as a resident driver (the
//! `python-agent-driver` image is the reference). The first `run()` pays the kernel boot +
//! interpreter start-up; every subsequent `run()` is a warm rewind + run against the
//! post-init snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use hyperlight_sandbox::{
    CredentialEntry, DirPerms, FilePerms, Guest, GuestSandbox, HttpMethod, ResolverFn, Sandbox,
    SandboxBuilder, Snapshot,
};
use hyperlight_sandbox_pyo3_common::{
    PyExecutionResult, build_tool_registry, parse_size, parse_tool_registration,
};
use hyperlight_unikraft_sandbox::Unikraft;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

type UnikraftSandboxInner = Sandbox<Unikraft>;
type UnikraftSnapshotInner = Snapshot<<<Unikraft as Guest>::Sandbox as GuestSandbox>::SnapshotData>;

/// Buffered credential registration for lazy sandbox init.
///
/// NOTE: the Unikraft `fetch` tool does not yet consume scoped credentials (the backend
/// crate flags this as a TODO), so credentials registered here are stored on the sandbox for
/// API parity with the Wasm backend but are not injected into outgoing requests yet.
struct PendingCredential {
    id: String,
    target: String,
    header: String,
    prefix: String,
    resolver: ResolverFn,
}

/// Wrap a Python callable as a [`ResolverFn`] suitable for storage in the credential
/// registry.
///
/// On each invocation the wrapper re-acquires the Python GIL, calls the supplied callable
/// with no arguments, and extracts the result as a Python `str`. Exceptions are mapped to a
/// redacted Rust error — only the exception **type name** is surfaced, never the message
/// (which may contain secret material assembled by user code).
fn python_callable_to_resolver(callable: Py<PyAny>) -> ResolverFn {
    Arc::new(move || -> Result<String, String> {
        Python::attach(|py| {
            let bound = callable.bind(py);
            match bound.call0() {
                Ok(result) => result
                    .extract::<String>()
                    .map_err(|_| "credential resolver did not return a str".to_string()),
                Err(err) => {
                    let type_name = err
                        .get_type(py)
                        .qualname()
                        .ok()
                        .and_then(|n| n.extract::<String>().ok())
                        .unwrap_or_else(|| "Exception".to_string());
                    Err(format!("python resolver raised {type_name}"))
                }
            }
        })
    })
}

#[pyclass]
pub struct PySnapshot {
    inner: UnikraftSnapshotInner,
}

/// A Unikraft micro-VM sandbox exposed to Python.
///
/// The underlying [`hyperlight_sandbox::Sandbox`] is built lazily on the first
/// [`run`](UnikraftSandbox::run) so that tools, network permissions and credentials can be
/// registered beforehand — mirroring the Wasm backend's lifecycle.
#[pyclass(unsendable)]
pub struct UnikraftSandbox {
    inner: Option<UnikraftSandboxInner>,
    tools: HashMap<String, Py<PyAny>>,
    pending_networks: Vec<(String, Option<Vec<String>>)>,
    pending_credentials: Vec<PendingCredential>,
    kernel: String,
    initrd: Option<String>,
    initrd_base: Option<u64>,
    heap_size: Option<u64>,
    stack_size: Option<u64>,
    input_dir: Option<String>,
    output_dir: Option<String>,
    temp_output: bool,
}

#[pymethods]
impl UnikraftSandbox {
    #[new]
    #[pyo3(signature = (kernel, initrd=None, initrd_base=None, input_dir=None, output_dir=None, temp_output=false, heap_size=None, stack_size=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        kernel: &str,
        initrd: Option<&str>,
        initrd_base: Option<u64>,
        input_dir: Option<&str>,
        output_dir: Option<&str>,
        temp_output: bool,
        heap_size: Option<&str>,
        stack_size: Option<&str>,
    ) -> PyResult<Self> {
        Ok(UnikraftSandbox {
            inner: None,
            tools: HashMap::new(),
            pending_networks: Vec::new(),
            pending_credentials: Vec::new(),
            kernel: kernel.to_string(),
            initrd: initrd.map(|s| s.to_string()),
            initrd_base,
            // VM sizing is expressed in bytes on the `Unikraft` builder; parse the
            // human-readable strings here so `None` falls through to the crate defaults.
            heap_size: match heap_size {
                Some(s) => Some(parse_size(s)?),
                None => None,
            },
            stack_size: match stack_size {
                Some(s) => Some(parse_size(s)?),
                None => None,
            },
            input_dir: input_dir.map(|s| s.to_string()),
            output_dir: output_dir.map(|s| s.to_string()),
            temp_output,
        })
    }

    #[pyo3(signature = (name_or_tool, callback=None))]
    fn register_tool(
        &mut self,
        py: Python<'_>,
        name_or_tool: Py<PyAny>,
        callback: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        if self.inner.is_some() {
            return Err(PyRuntimeError::new_err(
                "Cannot register tools after sandbox has been initialized. \
                 Register all tools before the first run() call.",
            ));
        }
        let (name, cb) = parse_tool_registration(py, name_or_tool, callback)?;
        self.tools.insert(name, cb);
        Ok(())
    }

    #[pyo3(signature = (code))]
    fn run(&mut self, py: Python<'_>, code: &str) -> PyResult<PyExecutionResult> {
        if self.inner.is_none() {
            let registry = build_tool_registry(py, &mut self.tools)?;

            // VM sizing lives on the `Unikraft` guest (the backend deliberately ignores the
            // generic SandboxConfig heap/stack, which are tuned for in-process Wasm/JS).
            let mut guest = Unikraft::new(self.kernel.clone());
            if let Some(ref initrd) = self.initrd {
                guest = guest.initrd(initrd.clone());
            }
            if let Some(heap) = self.heap_size {
                guest = guest.heap_size(heap);
            }
            if let Some(stack) = self.stack_size {
                guest = guest.stack_size(stack);
            }
            if let Some(base) = self.initrd_base {
                guest = guest.initrd_base(base);
            }

            let mut builder = SandboxBuilder::new().with_tools(registry).guest(guest);
            if let Some(ref dir) = self.input_dir {
                builder = builder.input_dir(dir);
            }
            if let Some(ref dir) = self.output_dir {
                builder = builder.output_dir(
                    dir,
                    DirPerms::READ | DirPerms::MUTATE,
                    FilePerms::READ | FilePerms::WRITE,
                );
            } else if self.temp_output {
                builder = builder.temp_output();
            }

            let mut sandbox = builder
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("Failed to create sandbox: {e:#}")))?;
            for (target, methods) in std::mem::take(&mut self.pending_networks) {
                let methods = HttpMethod::parse_list(methods)
                    .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
                sandbox
                    .allow_domain(&target, methods)
                    .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
            }
            for cred in std::mem::take(&mut self.pending_credentials) {
                sandbox
                    .register_credential(
                        cred.id,
                        CredentialEntry {
                            target: cred.target,
                            header: cred.header,
                            prefix: cred.prefix,
                            resolver: cred.resolver,
                        },
                    )
                    .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
            }
            self.inner = Some(sandbox);
        }
        let sandbox = self.inner.as_mut().unwrap();
        let result = sandbox
            .run(code)
            .map_err(|e| PyRuntimeError::new_err(format!("Execution failed: {e}")))?;
        Ok(PyExecutionResult {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
        })
    }

    #[pyo3(signature = (target, methods=None))]
    fn allow_domain(&mut self, target: &str, methods: Option<Vec<String>>) -> PyResult<()> {
        if let Some(sandbox) = self.inner.as_mut() {
            let methods = HttpMethod::parse_list(methods)
                .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
            sandbox
                .allow_domain(target, methods)
                .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
        } else {
            self.pending_networks.push((target.to_string(), methods));
        }
        Ok(())
    }

    fn snapshot(&mut self) -> PyResult<PySnapshot> {
        let sandbox = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("Sandbox not initialized"))?;
        let snap = sandbox
            .snapshot()
            .map_err(|e| PyRuntimeError::new_err(format!("Snapshot failed: {e}")))?;
        Ok(PySnapshot { inner: snap })
    }

    fn restore(&mut self, snapshot: &PySnapshot) -> PyResult<()> {
        let sandbox = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("Sandbox not initialized"))?;
        sandbox
            .restore(&snapshot.inner)
            .map_err(|e| PyRuntimeError::new_err(format!("Restore failed: {e}")))?;
        Ok(())
    }

    /// Register a scoped credential for outgoing HTTP requests.
    ///
    /// Accepted for API parity with the Wasm backend. NOTE: the Unikraft `fetch` tool does
    /// not yet inject scoped credentials, so a registered credential is stored on the
    /// sandbox but is not applied to outgoing requests until credential-aware `fetch` lands.
    #[pyo3(signature = (id, target, header, prefix, resolver))]
    fn register_credential(
        &mut self,
        id: &str,
        target: &str,
        header: &str,
        prefix: &str,
        resolver: Py<PyAny>,
    ) -> PyResult<()> {
        let resolver_fn = python_callable_to_resolver(resolver);
        if let Some(sandbox) = self.inner.as_ref() {
            sandbox
                .register_credential(
                    id,
                    CredentialEntry {
                        target: target.to_string(),
                        header: header.to_string(),
                        prefix: prefix.to_string(),
                        resolver: resolver_fn,
                    },
                )
                .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
        } else {
            self.pending_credentials.push(PendingCredential {
                id: id.to_string(),
                target: target.to_string(),
                header: header.to_string(),
                prefix: prefix.to_string(),
                resolver: resolver_fn,
            });
        }
        Ok(())
    }

    fn get_output_files(&self) -> PyResult<Vec<String>> {
        let sandbox = self
            .inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Sandbox not initialized"))?;
        sandbox
            .get_output_files()
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to get output files: {e}")))
    }

    fn output_path(&self) -> PyResult<Option<String>> {
        let sandbox = self
            .inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Sandbox not initialized"))?;
        let path = sandbox
            .output_path()
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to get output path: {e}")))?;
        Ok(path.map(|p| p.display().to_string()))
    }
}

#[pymodule]
fn _native_unikraft(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<UnikraftSandbox>()?;
    m.add_class::<PyExecutionResult>()?;
    m.add_class::<PySnapshot>()?;
    m.add("__version__", "0.1.0")?;
    Ok(())
}
