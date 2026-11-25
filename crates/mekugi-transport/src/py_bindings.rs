use std::sync::{Arc, OnceLock};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::box_client::{BoxStatus, CodeModeDocs, ExecOutcome, TransportError};
use crate::cluster::{ClusterHealth, ClusterManager};
use crate::config::ClusterConfig;
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
    m.add_function(wrap_pyfunction!(format_cell_id, m)?)?;
    m.add_function(wrap_pyfunction!(parse_cell_route, m)?)?;
    Ok(())
}
