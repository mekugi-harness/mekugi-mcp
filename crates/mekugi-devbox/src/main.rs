mod apply_patch;
mod code_mode_client;
mod docs;
mod pragma;
mod pty_manager;
mod systemd;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use mekugi_transport::box_client::CodeModeDocs;
use mekugi_transport::grpc_server::{DevboxBackend, RunningGrpcServer};
use mekugi_transport::pb;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tonic::Status;
use tracing::{error, info, warn};

use crate::code_mode_client::{CodeModeHostClient, HostSupervisor, MAX_CELL_YIELD_TIME_MS};
use crate::docs::{load_extracted_docs_file, parse_description_rs};

#[derive(Clone)]
struct AppState {
    client: Arc<CodeModeHostClient>,
    expected_token: Option<String>,
    extracted_docs: Option<CodeModeDocs>,
    hostname: String,
    os: String,
    arch: String,
}

impl AppState {
    fn authorize_http(&self, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
        let Some(expected) = &self.expected_token else {
            return Ok(());
        };
        let header = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let Some(provided) = header.strip_prefix("Bearer ") else {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Unauthorized"})),
            ));
        };
        if bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
            Ok(())
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Unauthorized"})),
            ))
        }
    }

    async fn status_payload(&self) -> (bool, Value) {
        let running = self.client.is_host_running().await;
        let session_id = self.client.session_id().await;
        let active_cells = self.client.active_cell_count().await;
        let active_shells = self.client.active_shell_count().await;
        let payload = json!({
            "status": if running { "ok" } else { "degraded" },
            "hostname": self.hostname,
            "os": self.os,
            "arch": self.arch,
            "workdir": self.client.workdir().display().to_string(),
            "session_id": session_id,
            "active_cells": active_cells,
            "active_shells": active_shells,
            "error": if running { Value::Null } else { json!("codex-code-mode-host is not running") },
        });
        (running, payload)
    }
}

#[async_trait]
impl DevboxBackend for AppState {
    async fn exec(&self, req: pb::ExecRequest) -> Result<pb::ExecResponse, Status> {
        if req.code.trim().is_empty() {
            return Err(Status::invalid_argument("'code' must be a non-empty string"));
        }
        let outcome = self
            .client
            .execute(
                &req.code,
                Some(req.yield_time_ms),
                (req.max_output_tokens > 0).then_some(req.max_output_tokens),
            )
            .await
            .map_err(Status::internal)?;
        Ok(pb::ExecResponse {
            cell_id: outcome.cell_id,
            status: outcome.status,
            text: outcome.text,
            more_output: outcome.more_output,
            error_text: outcome.error_text,
        })
    }

    async fn wait(&self, req: pb::WaitRequest) -> Result<pb::WaitResponse, Status> {
        if req.cell_id.trim().is_empty() {
            return Err(Status::invalid_argument(
                "'cell_id' must be a non-empty string",
            ));
        }
        let outcome = self
            .client
            .wait(
                &req.cell_id,
                Some(req.yield_time_ms),
                (req.max_output_tokens > 0).then_some(req.max_output_tokens),
                req.terminate,
            )
            .await
            .map_err(Status::internal)?;
        Ok(pb::WaitResponse {
            cell_id: outcome.cell_id,
            status: outcome.status,
            text: outcome.text,
            more_output: outcome.more_output,
            error_text: outcome.error_text,
        })
    }

    async fn get_info(&self) -> Result<pb::GetInfoResponse, Status> {
        let running = self.client.is_host_running().await;
        Ok(pb::GetInfoResponse {
            status: if running { "ok" } else { "degraded" }.into(),
            hostname: self.hostname.clone(),
            os: self.os.clone(),
            arch: self.arch.clone(),
            workdir: self.client.workdir().display().to_string(),
            session_id: self.client.session_id().await,
            active_cells: self.client.active_cell_count().await,
            active_shells: self.client.active_shell_count().await,
            error: (!running).then(|| "codex-code-mode-host is not running".into()),
        })
    }

    async fn get_code_mode_docs(&self) -> Result<pb::GetCodeModeDocsResponse, Status> {
        let Some(d) = &self.extracted_docs else {
            return Err(Status::not_found("Extracted code-mode docs not available"));
        };
        Ok(pb::GetCodeModeDocsResponse {
            source_repo: d.source_repo.clone(),
            source_ref: d.source_ref.clone(),
            exec_description_template: d.exec_description_template.clone(),
            wait_description_template: d.wait_description_template.clone(),
            image_helper_description: d.image_helper_description.clone(),
        })
    }
}

#[derive(Deserialize)]
struct HttpExecReq {
    code: String,
    yield_time_ms: Option<u32>,
    max_output_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct HttpWaitReq {
    cell_id: String,
    yield_time_ms: Option<u32>,
    max_output_tokens: Option<u32>,
    #[serde(default)]
    terminate: bool,
}

async fn http_health(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let (running, payload) = state.status_payload().await;
    let status = if running {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(payload))
}

async fn http_exec(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<HttpExecReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state.authorize_http(&headers)?;
    if body.code.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "'code' must be a non-empty string"})),
        ));
    }
    let outcome = state
        .client
        .execute(&body.code, body.yield_time_ms, body.max_output_tokens)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(serde_json::to_value(outcome).unwrap_or_default()))
}

async fn http_wait(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<HttpWaitReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state.authorize_http(&headers)?;
    if body.cell_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "'cell_id' must be a non-empty string"})),
        ));
    }
    let outcome = state
        .client
        .wait(
            &body.cell_id,
            body.yield_time_ms,
            body.max_output_tokens,
            body.terminate,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(serde_json::to_value(outcome).unwrap_or_default()))
}

async fn http_docs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state.authorize_http(&headers)?;
    let Some(docs) = &state.extracted_docs else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Extracted code-mode docs not available"})),
        ));
    };
    Ok(Json(serde_json::to_value(docs).unwrap_or_default()))
}

fn read_hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn maybe_run_extract_docs(args: &[String]) -> Option<Result<(), String>> {
    if !args.iter().any(|a| a == "--extract-docs") {
        return None;
    }
    let mut repo = "mekugi-harness/mekugi".to_owned();
    let mut ref_val: Option<String> = None;
    let mut source_file: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--extract-docs" => i += 1,
            "--repo" if i + 1 < args.len() => {
                repo = args[i + 1].clone();
                i += 2;
            }
            "--ref" if i + 1 < args.len() => {
                ref_val = Some(args[i + 1].trim().to_owned());
                i += 2;
            }
            "--ref-file" if i + 1 < args.len() => {
                match fs::read_to_string(&args[i + 1]) {
                    Ok(s) => ref_val = Some(s.trim().to_owned()),
                    Err(e) => return Some(Err(format!("Failed to read ref-file: {e}"))),
                }
                i += 2;
            }
            "--source-file" if i + 1 < args.len() => {
                source_file = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--output" if i + 1 < args.len() => {
                output = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => return Some(Err(format!("Unknown argument: {other}"))),
        }
    }

    let Some(out_path) = output else {
        return Some(Err("--output is required for --extract-docs".into()));
    };
    let Some(src_path) = source_file else {
        return Some(Err("--source-file is required for --extract-docs".into()));
    };
    let source = match fs::read_to_string(&src_path) {
        Ok(s) => s,
        Err(e) => return Some(Err(format!("Failed to read source file: {e}"))),
    };
    let source_ref = ref_val.unwrap_or_else(|| "local".into());
    let parsed = match parse_description_rs(&source, &repo, &source_ref) {
        Ok(d) => d,
        Err(e) => return Some(Err(e)),
    };
    if let Some(parent) = out_path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Some(Err(format!("Failed to create output dir: {e}")));
    }
    let json_text = match serde_json::to_string_pretty(&parsed) {
        Ok(s) => format!("{s}
"),
        Err(e) => return Some(Err(e.to_string())),
    };
    if let Err(e) = fs::write(&out_path, json_text) {
        return Some(Err(format!("Failed to write output: {e}")));
    }
    println!(
        "Extracted Mekugi code-mode docs from {repo}@{source_ref} -> {}",
        out_path.display()
    );
    Some(Ok(()))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if let Some(res) = maybe_run_extract_docs(&args) {
        if let Err(e) = res {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let filter = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();

    let grpc_target =
        env::var("CODE_MODE_HOST_TARGET").unwrap_or_else(|_| "127.0.0.1:50051".into());
    let host_bin = env::var("CODE_MODE_HOST_BIN")
        .unwrap_or_else(|_| "/usr/local/bin/codex-code-mode-host".into());
    let binary_path = (!host_bin.trim().is_empty()).then(|| PathBuf::from(host_bin));
    let workdir = PathBuf::from(env::var("DEVBOX_WORKDIR").unwrap_or_else(|_| "/workspace".into()));
    let shell = env::var("DEVBOX_SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let max_yield_ms = env::var("MAX_CELL_YIELD_TIME_MS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(MAX_CELL_YIELD_TIME_MS);
    let expected_token = env::var("DEVBOX_INTERNAL_TOKEN")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if expected_token.is_none() {
        warn!(
            "DEVBOX_INTERNAL_TOKEN is unset; HTTP and gRPC endpoints will accept unauthenticated requests"
        );
    }
    let docs_path = env::var("CODE_MODE_DOCS_PATH")
        .unwrap_or_else(|_| "/usr/local/share/mekugi/code-mode-docs.json".into());
    let extracted_docs = if docs_path.trim().is_empty() {
        None
    } else {
        load_extracted_docs_file(&PathBuf::from(docs_path))
    };

    let grpc_listen = env::var("DEVBOX_GRPC_ADDR").unwrap_or_else(|_| "0.0.0.0:50052".into());
    let http_host = env::var("DEVBOX_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let http_port = env::var("DEVBOX_PORT").unwrap_or_else(|_| "8080".into());
    let http_listen = if http_host.trim().is_empty() || http_port.trim().is_empty() {
        None
    } else {
        Some(format!("{}:{}", http_host.trim(), http_port.trim()))
    };

    if let Err(e) = fs::create_dir_all(&workdir) {
        error!("Failed to create workdir {}: {e}", workdir.display());
        std::process::exit(1);
    }

    let supervisor = Arc::new(HostSupervisor::new(binary_path, grpc_target.clone()));
    let client = Arc::new(CodeModeHostClient::new(
        grpc_target,
        workdir,
        shell,
        max_yield_ms,
        supervisor.clone(),
    ));

    if let Err(e) = supervisor.ensure_running().await {
        error!("Failed to start codex-code-mode-host: {e}");
        supervisor.stop().await;
        std::process::exit(1);
    }
    if let Err(e) = client.start().await {
        error!("Failed to open code-mode session: {e}");
        client.close().await;
        supervisor.stop().await;
        std::process::exit(1);
    }

    let state = Arc::new(AppState {
        client: client.clone(),
        expected_token: expected_token.clone(),
        extracted_docs,
        hostname: read_hostname(),
        os: env::consts::OS.to_owned(),
        arch: env::consts::ARCH.to_owned(),
    });

    let grpc_server = if !grpc_listen.trim().is_empty() {
        match RunningGrpcServer::start(&grpc_listen, expected_token.clone(), state.clone()).await {
            Ok(srv) => {
                info!("Rust DevboxAgent gRPC server listening on {}", srv.local_addr);
                Some(srv)
            }
            Err(e) => {
                error!("Failed to start gRPC server on {grpc_listen}: {e}");
                client.close().await;
                supervisor.stop().await;
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let http_server = if let Some(addr) = http_listen {
        let router = Router::new()
            .route("/health", get(http_health))
            .route("/exec", post(http_exec))
            .route("/wait", post(http_wait))
            .route("/code-mode-docs", get(http_docs))
            .with_state(state);

        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                let local_addr = listener.local_addr().unwrap();
                info!("Rust devbox HTTP server listening on {local_addr}");
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let handle = tokio::spawn(async move {
                    let _ = axum::serve(listener, router)
                        .with_graceful_shutdown(async move {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                });
                Some((local_addr, shutdown_tx, handle))
            }
            Err(e) => {
                error!("Failed to bind HTTP server on {addr}: {e}");
                if let Some(srv) = grpc_server {
                    srv.stop().await;
                }
                client.close().await;
                supervisor.stop().await;
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    if grpc_server.is_none() && http_server.is_none() {
        error!("At least one of DEVBOX_GRPC_ADDR or DEVBOX_HOST/DEVBOX_PORT must be enabled");
        client.close().await;
        supervisor.stop().await;
        std::process::exit(1);
    }

    let mut endpoints = Vec::new();
    if let Some(srv) = &grpc_server {
        endpoints.push(format!("gRPC {}", srv.local_addr));
    }
    if let Some((addr, _, _)) = &http_server {
        endpoints.push(format!("HTTP {addr}"));
    }
    systemd::notify(&format!(
        "READY=1\nSTATUS=Listening on {}",
        endpoints.join(", ")
    ));

    let watchdog_handle = systemd::watchdog_interval().map(|interval| {
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if client.is_host_running().await {
                    systemd::notify("WATCHDOG=1");
                } else {
                    warn!("codex-code-mode-host is not running; skipping systemd watchdog ping");
                }
            }
        })
    });

    shutdown_signal().await;
    if let Some(handle) = watchdog_handle {
        handle.abort();
    }
    systemd::notify("STOPPING=1\nSTATUS=Shutting down");

    let http_drain = async {
        if let Some((_, shutdown_tx, handle)) = http_server {
            let _ = shutdown_tx.send(());
            if tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                warn!("HTTP server drain timed out after 5s");
            }
        }
    };
    let grpc_drain = async {
        if let Some(srv) = grpc_server {
            srv.stop().await;
        }
    };
    tokio::join!(http_drain, grpc_drain);

    client.close().await;
    supervisor.stop().await;
}
