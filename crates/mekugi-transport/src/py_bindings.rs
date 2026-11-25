use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use pyo3::exceptions::{PyFileNotFoundError, PyLookupError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use tokio::sync::Mutex;
use tonic::Status;

use crate::box_client::{BoxStatus, CodeModeDocs, ExecOutcome, TransportError};
use crate::cluster::{ClusterHealth, ClusterManager};
use crate::config::ClusterConfig;
use crate::grpc_server::{DevboxBackend, RunningGrpcServer};
use crate::pb;
use crate::routing;

fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("mekugi-transport")
            .build()
            .expect("failed to build Tokio runtime for mekugi-transport")
    })
}

fn map_transport_err(err: TransportError) -> PyErr {
    match err {
        TransportError::Config(e) => PyValueError::new_err(e.to_string()),
        TransportError::UnknownBox { .. } | TransportError::Route(_) => {
            PyValueError::new_err(err.to_string())
        }
        other => PyRuntimeError::new_err(other.to_string()),
    }
}

fn outcome_to_py(py: Python<'_>, out: ExecOutcome) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("box", out.box_name)?;
    d.set_item("cell_id", out.cell_id)?;
    d.set_item("status", out.status)?;
    d.set_item("text", out.text)?;
    d.set_item("more_output", out.more_output)?;
    d.set_item("error_text", out.error_text)?;
    Ok(d.into_any().unbind())
}

fn box_status_to_py(py: Python<'_>, st: BoxStatus) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("name", st.name)?;
    d.set_item("target", st.target)?;
    d.set_item("transport", st.transport)?;
    d.set_item("is_default", st.is_default)?;
    d.set_item("status", st.status)?;
    d.set_item("latency_ms", st.latency_ms)?;
    d.set_item("description", st.description)?;
    let labels = PyDict::new(py);
    for (k, v) in st.labels {
        labels.set_item(k, v)?;
    }
    d.set_item("labels", labels)?;
    d.set_item("hostname", st.hostname)?;
    d.set_item("os", st.os)?;
    d.set_item("arch", st.arch)?;
    d.set_item("workdir", st.workdir)?;
    d.set_item("session_id", st.session_id)?;
    d.set_item("active_cells", st.active_cells)?;
    d.set_item("active_shells", st.active_shells)?;
    d.set_item("pool_size", st.pool_size)?;
    d.set_item("error", st.error)?;
    Ok(d.into_any().unbind())
}

fn health_to_py(py: Python<'_>, h: ClusterHealth) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("status", h.status)?;
    d.set_item("default_box", h.default_box)?;
    let mut boxes = Vec::with_capacity(h.boxes.len());
    for b in h.boxes {
        boxes.push(box_status_to_py(py, b)?);
    }
    d.set_item("boxes", boxes)?;
    Ok(d.into_any().unbind())
}

fn docs_to_py(py: Python<'_>, docs: CodeModeDocs) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("source_repo", docs.source_repo)?;
    d.set_item("source_ref", docs.source_ref)?;
    d.set_item("exec_description_template", docs.exec_description_template)?;
    d.set_item("wait_description_template", docs.wait_description_template)?;
    d.set_item("image_helper_description", docs.image_helper_description)?;
    Ok(d.into_any().unbind())
}

#[pyclass(name = "ClusterTransport")]
pub struct PyClusterTransport {
    manager: Arc<ClusterManager>,
}

#[pymethods]
impl PyClusterTransport {
    #[classmethod]
    #[pyo3(signature = (json_str, default_box=None, fallback_token=None))]
    fn from_json(
        _cls: &Bound<'_, pyo3::types::PyType>,
        json_str: &str,
        default_box: Option<&str>,
        fallback_token: Option<&str>,
    ) -> PyResult<Self> {
        let cfg = ClusterConfig::from_json_str(json_str)
            .and_then(|c| c.validate(default_box))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let _guard = shared_runtime().enter();
        let manager = ClusterManager::new(cfg, fallback_token).map_err(map_transport_err)?;
        Ok(Self {
            manager: Arc::new(manager),
        })
    }

    #[classmethod]
    #[pyo3(signature = (path, default_box=None, fallback_token=None))]
    fn from_file(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: &str,
        default_box: Option<&str>,
        fallback_token: Option<&str>,
    ) -> PyResult<Self> {
        let cfg = ClusterConfig::from_file(path)
            .and_then(|c| c.validate(default_box))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let _guard = shared_runtime().enter();
        let manager = ClusterManager::new(cfg, fallback_token).map_err(map_transport_err)?;
        Ok(Self {
            manager: Arc::new(manager),
        })
    }

    #[classmethod]
    #[pyo3(signature = (target, box_name=None, token=None))]
    fn single_box(
        _cls: &Bound<'_, pyo3::types::PyType>,
        target: &str,
        box_name: Option<&str>,
        token: Option<&str>,
    ) -> PyResult<Self> {
        let cfg = ClusterConfig::single_box(box_name, target.to_owned(), token.map(ToOwned::to_owned))
            .validate(box_name)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let _guard = shared_runtime().enter();
        let manager = ClusterManager::new(cfg, token).map_err(map_transport_err)?;
        Ok(Self {
            manager: Arc::new(manager),
        })
    }

    #[getter]
    fn default_box(&self) -> String {
        self.manager.default_box().to_owned()
    }

    #[getter]
    fn box_names(&self) -> Vec<String> {
        self.manager.box_names().to_vec()
    }

    #[pyo3(signature = (code, box_name=None, yield_time_ms=10_000, max_output_tokens=10_000))]
    fn exec<'py>(
        &self,
        py: Python<'py>,
        code: String,
        box_name: Option<String>,
        yield_time_ms: u32,
        max_output_tokens: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mgr = self.manager.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let outcome = mgr
                .exec(box_name.as_deref(), code, yield_time_ms, max_output_tokens)
                .await
                .map_err(map_transport_err)?;
            Python::with_gil(|py| outcome_to_py(py, outcome))
        })
    }

    #[pyo3(signature = (cell_id, box_name=None, yield_time_ms=10_000, max_output_tokens=10_000, terminate=false))]
    fn wait<'py>(
        &self,
        py: Python<'py>,
        cell_id: String,
        box_name: Option<String>,
        yield_time_ms: u32,
        max_output_tokens: u32,
        terminate: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mgr = self.manager.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let outcome = mgr
                .wait(
                    &cell_id,
                    box_name.as_deref(),
                    yield_time_ms,
                    max_output_tokens,
                    terminate,
                )
                .await
                .map_err(map_transport_err)?;
            Python::with_gil(|py| outcome_to_py(py, outcome))
        })
    }

    #[pyo3(signature = (probe_timeout_ms=2_000))]
    fn list_boxes<'py>(
        &self,
        py: Python<'py>,
        probe_timeout_ms: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mgr = self.manager.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let statuses = mgr.list_boxes(probe_timeout_ms).await;
            Python::with_gil(|py| {
                let mut out = Vec::with_capacity(statuses.len());
                for st in statuses {
                    out.push(box_status_to_py(py, st)?);
                }
                Ok(out)
            })
        })
    }

    #[pyo3(signature = (probe_timeout_ms=2_000))]
    fn health_all<'py>(
        &self,
        py: Python<'py>,
        probe_timeout_ms: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mgr = self.manager.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let health = mgr.health_all(probe_timeout_ms).await;
            Python::with_gil(|py| health_to_py(py, health))
        })
    }

    #[pyo3(signature = (box_name=None, timeout_ms=2_000))]
    fn fetch_code_mode_docs<'py>(
        &self,
        py: Python<'py>,
        box_name: Option<String>,
        timeout_ms: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mgr = self.manager.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let maybe_docs = mgr
                .fetch_code_mode_docs(box_name.as_deref(), timeout_ms)
                .await
                .map_err(map_transport_err)?;
            Python::with_gil(|py| match maybe_docs {
                Some(d) => Ok(Some(docs_to_py(py, d)?)),
                None => Ok(None),
            })
        })
    }

}

struct PyDevboxBackend {
    handler: Py<PyAny>,
    locals: pyo3_async_runtimes::TaskLocals,
}

impl PyDevboxBackend {
    #[allow(clippy::result_large_err)]
    async fn call_py_coro<F>(&self, build_coro: F) -> Result<Py<PyAny>, Status>
    where
        F: FnOnce(Python<'_>, &Bound<'_, PyAny>) -> PyResult<Py<PyAny>>,
    {
        let fut = Python::with_gil(|py| -> PyResult<_> {
            let handler = self.handler.bind(py);
            let coro = build_coro(py, handler)?;
            pyo3_async_runtimes::into_future_with_locals(&self.locals, coro.into_bound(py))
        })
        .map_err(|e| Status::internal(e.to_string()))?;

        fut.await.map_err(|e| {
            Python::with_gil(|py| {
                let msg = e.to_string();
                if e.is_instance_of::<PyValueError>(py) {
                    Status::invalid_argument(msg)
                } else if e.is_instance_of::<PyFileNotFoundError>(py)
                    || e.is_instance_of::<PyLookupError>(py)
                {
                    Status::not_found(msg)
                } else {
                    Status::internal(msg)
                }
            })
        })
    }
}

#[async_trait]
impl DevboxBackend for PyDevboxBackend {
    async fn exec(&self, req: pb::ExecRequest) -> Result<pb::ExecResponse, Status> {
        let obj = self
            .call_py_coro(|_py, handler| {
                let coro = handler.call_method1(
                    "on_exec",
                    (req.code, req.yield_time_ms, req.max_output_tokens),
                )?;
                Ok(coro.unbind())
            })
            .await?;

        Python::with_gil(|py| -> PyResult<pb::ExecResponse> {
            let d = obj.bind(py).downcast::<PyDict>()?;
            Ok(pb::ExecResponse {
                cell_id: d
                    .get_item("cell_id")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                status: d
                    .get_item("status")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_else(|| "completed".to_owned()),
                text: d
                    .get_item("text")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                more_output: d
                    .get_item("more_output")?
                    .and_then(|v| v.extract::<bool>().ok())
                    .unwrap_or(false),
                error_text: d
                    .get_item("error_text")?
                    .and_then(|v| v.extract::<Option<String>>().ok())
                    .flatten(),
            })
        })
        .map_err(|e| Status::internal(e.to_string()))
    }

    async fn wait(&self, req: pb::WaitRequest) -> Result<pb::WaitResponse, Status> {
        let obj = self
            .call_py_coro(|_py, handler| {
                let coro = handler.call_method1(
                    "on_wait",
                    (
                        req.cell_id,
                        req.yield_time_ms,
                        req.max_output_tokens,
                        req.terminate,
                    ),
                )?;
                Ok(coro.unbind())
            })
            .await?;

        Python::with_gil(|py| -> PyResult<pb::WaitResponse> {
            let d = obj.bind(py).downcast::<PyDict>()?;
            Ok(pb::WaitResponse {
                cell_id: d
                    .get_item("cell_id")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                status: d
                    .get_item("status")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_else(|| "completed".to_owned()),
                text: d
                    .get_item("text")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                more_output: d
                    .get_item("more_output")?
                    .and_then(|v| v.extract::<bool>().ok())
                    .unwrap_or(false),
                error_text: d
                    .get_item("error_text")?
                    .and_then(|v| v.extract::<Option<String>>().ok())
                    .flatten(),
            })
        })
        .map_err(|e| Status::internal(e.to_string()))
    }

    async fn get_info(&self) -> Result<pb::GetInfoResponse, Status> {
        let obj = self
            .call_py_coro(|_py, handler| {
                let coro = handler.call_method0("on_info")?;
                Ok(coro.unbind())
            })
            .await?;

        Python::with_gil(|py| -> PyResult<pb::GetInfoResponse> {
            let d = obj.bind(py).downcast::<PyDict>()?;
            Ok(pb::GetInfoResponse {
                status: d
                    .get_item("status")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_else(|| "ok".to_owned()),
                hostname: d
                    .get_item("hostname")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                os: d
                    .get_item("os")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                arch: d
                    .get_item("arch")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                workdir: d
                    .get_item("workdir")?
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default(),
                session_id: d
                    .get_item("session_id")?
                    .and_then(|v| v.extract::<Option<String>>().ok())
                    .flatten(),
                active_cells: d
                    .get_item("active_cells")?
                    .and_then(|v| v.extract::<u32>().ok())
                    .unwrap_or(0),
                active_shells: d
                    .get_item("active_shells")?
                    .and_then(|v| v.extract::<u32>().ok())
                    .unwrap_or(0),
                error: d
                    .get_item("error")?
                    .and_then(|v| v.extract::<Option<String>>().ok())
                    .flatten(),
            })
        })
        .map_err(|e| Status::internal(e.to_string()))
    }

    #[allow(clippy::result_large_err)]
    async fn get_code_mode_docs(&self) -> Result<pb::GetCodeModeDocsResponse, Status> {
        let obj = self
            .call_py_coro(|_py, handler| {
                let coro = handler.call_method0("on_docs")?;
                Ok(coro.unbind())
            })
            .await?;

        Python::with_gil(|py| -> Result<pb::GetCodeModeDocsResponse, Status> {
            let bound = obj.bind(py);
            if bound.is_none() {
                return Err(Status::not_found("Extracted code-mode docs not available"));
            }
            let d = bound
                .downcast::<PyDict>()
                .map_err(|e| Status::internal(e.to_string()))?;
            let get_str = |key: &str| -> Result<String, Status> {
                d.get_item(key)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .and_then(|v| v.extract::<String>().ok())
                    .ok_or_else(|| Status::internal(format!("missing docs field '{key}'")))
            };
            Ok(pb::GetCodeModeDocsResponse {
                source_repo: get_str("source_repo")?,
                source_ref: get_str("source_ref")?,
                exec_description_template: get_str("exec_description_template")?,
                wait_description_template: get_str("wait_description_template")?,
                image_helper_description: get_str("image_helper_description")?,
            })
        })
    }
}

#[pyclass(name = "DevboxGrpcServer")]
pub struct PyDevboxGrpcServer {
    listen_addr: String,
    internal_token: Option<String>,
    handler: Py<PyAny>,
    running: Arc<Mutex<Option<RunningGrpcServer>>>,
}

#[pymethods]
impl PyDevboxGrpcServer {
    #[new]
    #[pyo3(signature = (listen_addr, handler, internal_token=None))]
    fn new(listen_addr: String, handler: Py<PyAny>, internal_token: Option<String>) -> Self {
        Self {
            listen_addr,
            internal_token,
            handler,
            running: Arc::new(Mutex::new(None)),
        }
    }

    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = pyo3_async_runtimes::tokio::get_current_locals(py)?;
        let handler = self.handler.clone_ref(py);
        let listen_addr = self.listen_addr.clone();
        let token = self.internal_token.clone();
        let running = self.running.clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = running.lock().await;
            if let Some(existing) = guard.as_ref() {
                return Ok(existing.local_addr.to_string());
            }
            let backend = Arc::new(PyDevboxBackend { handler, locals });
            let server = RunningGrpcServer::start(&listen_addr, token, backend)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("failed to start gRPC server: {e}")))?;
            let addr_str = server.local_addr.to_string();
            *guard = Some(server);
            Ok(addr_str)
        })
    }

    fn stop<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let running = self.running.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let server = running.lock().await.take();
            if let Some(srv) = server {
                srv.stop().await;
            }
            Ok(())
        })
    }
}

#[pyfunction]
fn format_cell_id(box_name: &str, raw_cell_id: &str) -> String {
    routing::format_cell_id(box_name, raw_cell_id)
}

#[pyfunction]
#[pyo3(signature = (cell_id, explicit_box=None, default_box="default"))]
fn parse_cell_route(
    cell_id: &str,
    explicit_box: Option<&str>,
    default_box: &str,
) -> PyResult<(String, String)> {
    routing::parse_cell_route(cell_id, explicit_box, default_box)
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pymodule]
pub fn _transport(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Ensure pyo3-async-runtimes uses our multi-threaded Tokio runtime.
    let _ = pyo3_async_runtimes::tokio::init_with_runtime(shared_runtime());
    m.add_class::<PyClusterTransport>()?;
    m.add_class::<PyDevboxGrpcServer>()?;
    m.add_function(wrap_pyfunction!(format_cell_id, m)?)?;
    m.add_function(wrap_pyfunction!(parse_cell_route, m)?)?;
    Ok(())
}
