use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

use crate::config::{BoxConfig, ConfigError, TransportScheme};
use crate::pb;
use crate::pb::devbox_agent_client::DevboxAgentClient;

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),
    #[error("unknown box '{requested}'; available boxes: {available}")]
    UnknownBox {
        requested: String,
        available: String,
    },
    #[error("route error: {0}")]
    Route(#[from] crate::routing::RouteError),
    #[error("gRPC transport error on box '{box_name}': {message}")]
    Grpc { box_name: String, message: String },
    #[error("HTTP transport error on box '{box_name}': {message}")]
    Http { box_name: String, message: String },
    #[error("box '{box_name}' returned error ({status}): {message}")]
    Remote {
        box_name: String,
        status: String,
        message: String,
    },
    #[error("request to box '{box_name}' timed out after {timeout_ms}ms")]
    Timeout { box_name: String, timeout_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutcome {
    pub box_name: String,
    pub cell_id: String,
    pub status: String,
    pub text: String,
    pub more_output: bool,
    pub error_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxInfo {
    pub status: String,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub workdir: Option<String>,
    pub session_id: Option<String>,
    pub active_cells: u32,
    pub active_shells: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxStatus {
    pub name: String,
    pub target: String,
    pub transport: String,
    pub is_default: bool,
    pub status: String,
    pub latency_ms: u64,
    pub description: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub workdir: Option<String>,
    pub session_id: Option<String>,
    pub active_cells: u32,
    pub active_shells: u32,
    pub pool_size: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeModeDocs {
    pub source_repo: String,
    pub source_ref: String,
    pub exec_description_template: String,
    pub wait_description_template: String,
    pub image_helper_description: String,
}

#[derive(Debug)]
pub struct GrpcPool {
    channels: Vec<Channel>,
    next_idx: AtomicUsize,
    bearer_header: Option<MetadataValue<tonic::metadata::Ascii>>,
}

impl GrpcPool {
    pub fn connect_lazy(
        cfg: &BoxConfig,
        scheme: TransportScheme,
        token: Option<&str>,
    ) -> Result<Self, TransportError> {
        let endpoint_uri = if scheme == TransportScheme::Grpcs {
            cfg.target.replacen("grpcs://", "https://", 1)
        } else {
            cfg.target.replacen("grpc://", "http://", 1)
        };

        let pool_size = cfg.effective_pool_size(scheme);
        let mut channels = Vec::with_capacity(pool_size);

        for _ in 0..pool_size {
            let mut endpoint = Endpoint::from_shared(endpoint_uri.clone())
                .map_err(|e| ConfigError::Invalid(format!("invalid gRPC target '{}': {e}", cfg.target)))?
                .connect_timeout(CONNECT_TIMEOUT)
                .tcp_nodelay(true)
                .tcp_keepalive(Some(TCP_KEEPALIVE))
                .http2_keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
                .keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT)
                .keep_alive_while_idle(true)
                .http2_adaptive_window(true);

            if scheme == TransportScheme::Grpcs {
                let mut tls = ClientTlsConfig::new().with_native_roots();
                if let Some(ca_path) = &cfg.ca_cert_path {
                    let pem = std::fs::read(ca_path).map_err(|source| ConfigError::Io {
                        path: ca_path.clone(),
                        source,
                    })?;
                    tls = tls.ca_certificate(Certificate::from_pem(pem));
                }
                endpoint = endpoint.tls_config(tls).map_err(|e| {
                    ConfigError::Invalid(format!("invalid TLS config for '{}': {e}", cfg.name))
                })?;
            }

            channels.push(endpoint.connect_lazy());
        }

        let bearer_header = match token {
            Some(tok) if !tok.is_empty() => {
                let val = MetadataValue::try_from(format!("Bearer {tok}")).map_err(|e| {
                    ConfigError::Invalid(format!("invalid bearer token for '{}': {e}", cfg.name))
                })?;
                Some(val)
            }
            _ => None,
        };

        Ok(Self {
            channels,
            next_idx: AtomicUsize::new(0),
            bearer_header,
        })
    }

    fn next_client(&self) -> DevboxAgentClient<Channel> {
        let idx = if self.channels.len() == 1 {
            0
        } else {
            self.next_idx.fetch_add(1, Ordering::Relaxed) % self.channels.len()
        };
        // Channel::clone is a cheap Arc clone sharing the multiplexed HTTP/2 connection.
        DevboxAgentClient::new(self.channels[idx].clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn build_request<T>(&self, msg: T, timeout: Duration) -> Request<T> {
        let mut req = Request::new(msg);
        req.set_timeout(timeout);
        if let Some(auth) = &self.bearer_header {
            req.metadata_mut().insert("authorization", auth.clone());
        }
        req
    }
}

#[derive(Debug)]
pub struct HttpBoxClient {
    base_url: String,
    client: reqwest::Client,
}

impl HttpBoxClient {
    pub fn new(
        cfg: &BoxConfig,
        scheme: TransportScheme,
        token: Option<&str>,
    ) -> Result<Self, TransportError> {
        let mut headers = HeaderMap::new();
        if let Some(tok) = token
            && !tok.is_empty()
        {
            let hv = HeaderValue::from_str(&format!("Bearer {tok}")).map_err(|e| {
                ConfigError::Invalid(format!("invalid bearer token for '{}': {e}", cfg.name))
            })?;
            headers.insert(AUTHORIZATION, hv);
        }

        let pool_size = cfg.effective_pool_size(scheme);
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_nodelay(true)
            .tcp_keepalive(Some(TCP_KEEPALIVE))
            .pool_max_idle_per_host(pool_size)
            .pool_idle_timeout(Some(HTTP_POOL_IDLE_TIMEOUT));

        if let Some(ca_path) = &cfg.ca_cert_path {
            let pem = std::fs::read(ca_path).map_err(|source| ConfigError::Io {
                path: ca_path.clone(),
                source,
            })?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| {
                ConfigError::Invalid(format!("invalid CA cert '{ca_path}': {e}"))
            })?;
            builder = builder.add_root_certificate(cert);
        }

        let client = builder.build().map_err(|e| {
            ConfigError::Invalid(format!("failed to build HTTP client for '{}': {e}", cfg.name))
        })?;

        Ok(Self {
            base_url: cfg.target.trim_end_matches('/').to_owned(),
            client,
        })
    }
}

#[derive(Debug)]
pub enum BoxTransport {
    Grpc(GrpcPool),
    Http(HttpBoxClient),
}

#[derive(Debug)]
pub struct BoxNode {
    pub config: BoxConfig,
    pub scheme: TransportScheme,
    pub pool_size: usize,
    pub max_yield_time_ms: u32,
    pub transport: BoxTransport,
}

#[derive(Debug, Deserialize)]
struct HttpExecResponse {
    #[serde(default)]
    cell_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    more_output: bool,
    #[serde(default)]
    error_text: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpHealthResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    active_cells: u32,
    #[serde(default)]
    active_shells: u32,
    #[serde(default)]
    error: Option<String>,
}

impl BoxNode {
    pub fn new(config: BoxConfig, fallback_token: Option<&str>) -> Result<Self, TransportError> {
        let scheme = config.scheme()?;
        let pool_size = config.effective_pool_size(scheme);
        let max_yield_time_ms = config.effective_max_yield_time_ms();
        let token = config.resolve_token(fallback_token);

        let transport = if scheme.is_grpc() {
            BoxTransport::Grpc(GrpcPool::connect_lazy(&config, scheme, token.as_deref())?)
        } else {
            BoxTransport::Http(HttpBoxClient::new(&config, scheme, token.as_deref())?)
        };

        Ok(Self {
            config,
            scheme,
            pool_size,
            max_yield_time_ms,
            transport,
        })
    }

    pub fn transport_label(&self) -> &'static str {
        match self.scheme {
            TransportScheme::Grpc => "grpc",
            TransportScheme::Grpcs => "grpcs",
            TransportScheme::Http => "http",
            TransportScheme::Https => "https",
        }
    }

    pub async fn exec(
        &self,
        code: String,
        yield_time_ms: u32,
        max_output_tokens: u32,
    ) -> Result<ExecOutcome, TransportError> {
        let clamped_yield = yield_time_ms.min(self.max_yield_time_ms);
        let timeout = Duration::from_millis((u64::from(clamped_yield) + 12_000).max(15_000));

        match &self.transport {
            BoxTransport::Grpc(pool) => {
                let mut client = pool.next_client();
                let req = pool.build_request(
                    pb::ExecRequest {
                        code,
                        yield_time_ms: clamped_yield,
                        max_output_tokens,
                    },
                    timeout,
                );
                let resp = client.exec(req).await.map_err(|status| TransportError::Grpc {
                    box_name: self.config.name.clone(),
                    message: format!("{}: {}", status.code(), status.message()),
                })?;
                let body = resp.into_inner();
                Ok(ExecOutcome {
                    box_name: self.config.name.clone(),
                    cell_id: body.cell_id,
                    status: body.status,
                    text: body.text,
                    more_output: body.more_output,
                    error_text: body.error_text,
                })
            }
            BoxTransport::Http(http) => {
                let resp = http
                    .client
                    .post(format!("{}/exec", http.base_url))
                    .timeout(timeout)
                    .json(&serde_json::json!({
                        "code": code,
                        "yield_time_ms": clamped_yield,
                        "max_output_tokens": max_output_tokens,
                    }))
                    .send()
                    .await
                    .map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: e.to_string(),
                    })?;

                let status = resp.status();
                let parsed: HttpExecResponse =
                    resp.json().await.map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: format!("invalid JSON response ({status}): {e}"),
                    })?;
                if !status.is_success() {
                    return Err(TransportError::Remote {
                        box_name: self.config.name.clone(),
                        status: status.as_u16().to_string(),
                        message: parsed
                            .error
                            .unwrap_or_else(|| format!("Devbox error ({status})")),
                    });
                }
                Ok(ExecOutcome {
                    box_name: self.config.name.clone(),
                    cell_id: parsed.cell_id,
                    status: parsed.status,
                    text: parsed.text,
                    more_output: parsed.more_output,
                    error_text: parsed.error_text,
                })
            }
        }
    }

    pub async fn wait(
        &self,
        raw_cell_id: String,
        yield_time_ms: u32,
        max_output_tokens: u32,
        terminate: bool,
    ) -> Result<ExecOutcome, TransportError> {
        let clamped_yield = yield_time_ms.min(self.max_yield_time_ms);
        let timeout = Duration::from_millis((u64::from(clamped_yield) + 12_000).max(15_000));

        match &self.transport {
            BoxTransport::Grpc(pool) => {
                let mut client = pool.next_client();
                let req = pool.build_request(
                    pb::WaitRequest {
                        cell_id: raw_cell_id,
                        yield_time_ms: clamped_yield,
                        max_output_tokens,
                        terminate,
                    },
                    timeout,
                );
                let resp = client.wait(req).await.map_err(|status| TransportError::Grpc {
                    box_name: self.config.name.clone(),
                    message: format!("{}: {}", status.code(), status.message()),
                })?;
                let body = resp.into_inner();
                Ok(ExecOutcome {
                    box_name: self.config.name.clone(),
                    cell_id: body.cell_id,
                    status: body.status,
                    text: body.text,
                    more_output: body.more_output,
                    error_text: body.error_text,
                })
            }
            BoxTransport::Http(http) => {
                let resp = http
                    .client
                    .post(format!("{}/wait", http.base_url))
                    .timeout(timeout)
                    .json(&serde_json::json!({
                        "cell_id": raw_cell_id,
                        "yield_time_ms": clamped_yield,
                        "max_output_tokens": max_output_tokens,
                        "terminate": terminate,
                    }))
                    .send()
                    .await
                    .map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: e.to_string(),
                    })?;

                let status = resp.status();
                let parsed: HttpExecResponse =
                    resp.json().await.map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: format!("invalid JSON response ({status}): {e}"),
                    })?;
                if !status.is_success() {
                    return Err(TransportError::Remote {
                        box_name: self.config.name.clone(),
                        status: status.as_u16().to_string(),
                        message: parsed
                            .error
                            .unwrap_or_else(|| format!("Devbox error ({status})")),
                    });
                }
                Ok(ExecOutcome {
                    box_name: self.config.name.clone(),
                    cell_id: parsed.cell_id,
                    status: parsed.status,
                    text: parsed.text,
                    more_output: parsed.more_output,
                    error_text: parsed.error_text,
                })
            }
        }
    }

    pub async fn probe_status(&self, is_default: bool, timeout: Duration) -> BoxStatus {
        let start = Instant::now();
        let probed = tokio::time::timeout(timeout, self.fetch_info(timeout)).await;
        let latency_ms = start.elapsed().as_millis() as u64;

        match probed {
            Ok(Ok(info)) => BoxStatus {
                name: self.config.name.clone(),
                target: self.config.target.clone(),
                transport: self.transport_label().to_owned(),
                is_default,
                status: if info.status.is_empty() {
                    "ok".to_owned()
                } else {
                    info.status
                },
                latency_ms,
                description: self.config.description.clone(),
                labels: self.config.labels.clone(),
                hostname: info.hostname,
                os: info.os,
                arch: info.arch,
                workdir: info.workdir,
                session_id: info.session_id,
                active_cells: info.active_cells,
                active_shells: info.active_shells,
                pool_size: self.pool_size,
                error: info.error,
            },
            Ok(Err(err)) => BoxStatus {
                name: self.config.name.clone(),
                target: self.config.target.clone(),
                transport: self.transport_label().to_owned(),
                is_default,
                status: "unreachable".to_owned(),
                latency_ms,
                description: self.config.description.clone(),
                labels: self.config.labels.clone(),
                hostname: None,
                os: None,
                arch: None,
                workdir: None,
                session_id: None,
                active_cells: 0,
                active_shells: 0,
                pool_size: self.pool_size,
                error: Some(err.to_string()),
            },
            Err(_) => BoxStatus {
                name: self.config.name.clone(),
                target: self.config.target.clone(),
                transport: self.transport_label().to_owned(),
                is_default,
                status: "unreachable".to_owned(),
                latency_ms,
                description: self.config.description.clone(),
                labels: self.config.labels.clone(),
                hostname: None,
                os: None,
                arch: None,
                workdir: None,
                session_id: None,
                active_cells: 0,
                active_shells: 0,
                pool_size: self.pool_size,
                error: Some(format!("probe timed out after {}ms", timeout.as_millis())),
            },
        }
    }

    async fn fetch_info(&self, timeout: Duration) -> Result<BoxInfo, TransportError> {
        match &self.transport {
            BoxTransport::Grpc(pool) => {
                let mut client = pool.next_client();
                let req = pool.build_request(pb::GetInfoRequest {}, timeout);
                let resp = client.get_info(req).await.map_err(|status| TransportError::Grpc {
                    box_name: self.config.name.clone(),
                    message: format!("{}: {}", status.code(), status.message()),
                })?;
                let info = resp.into_inner();
                Ok(BoxInfo {
                    status: info.status,
                    hostname: (!info.hostname.is_empty()).then_some(info.hostname),
                    os: (!info.os.is_empty()).then_some(info.os),
                    arch: (!info.arch.is_empty()).then_some(info.arch),
                    workdir: (!info.workdir.is_empty()).then_some(info.workdir),
                    session_id: info.session_id,
                    active_cells: info.active_cells,
                    active_shells: info.active_shells,
                    error: None,
                })
            }
            BoxTransport::Http(http) => {
                let resp = http
                    .client
                    .get(format!("{}/health", http.base_url))
                    .timeout(timeout)
                    .send()
                    .await
                    .map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: e.to_string(),
                    })?;
                let status_code = resp.status();
                let body: HttpHealthResponse =
                    resp.json().await.map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: format!("invalid health JSON ({status_code}): {e}"),
                    })?;
                let status_str = if !body.status.is_empty() {
                    body.status
                } else if status_code.is_success() {
                    "ok".to_owned()
                } else {
                    "degraded".to_owned()
                };
                Ok(BoxInfo {
                    status: status_str,
                    hostname: body.hostname,
                    os: body.os,
                    arch: body.arch,
                    workdir: body.workdir,
                    session_id: body.session_id,
                    active_cells: body.active_cells,
                    active_shells: body.active_shells,
                    error: body.error,
                })
            }
        }
    }

    pub async fn fetch_code_mode_docs(
        &self,
        timeout: Duration,
    ) -> Result<Option<CodeModeDocs>, TransportError> {
        match &self.transport {
            BoxTransport::Grpc(pool) => {
                let mut client = pool.next_client();
                let req = pool.build_request(pb::GetCodeModeDocsRequest {}, timeout);
                match client.get_code_mode_docs(req).await {
                    Ok(resp) => {
                        let d = resp.into_inner();
                        Ok(Some(CodeModeDocs {
                            source_repo: d.source_repo,
                            source_ref: d.source_ref,
                            exec_description_template: d.exec_description_template,
                            wait_description_template: d.wait_description_template,
                            image_helper_description: d.image_helper_description,
                        }))
                    }
                    Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
                    Err(status) => Err(TransportError::Grpc {
                        box_name: self.config.name.clone(),
                        message: format!("{}: {}", status.code(), status.message()),
                    }),
                }
            }
            BoxTransport::Http(http) => {
                let resp = http
                    .client
                    .get(format!("{}/code-mode-docs", http.base_url))
                    .timeout(timeout)
                    .send()
                    .await
                    .map_err(|e| TransportError::Http {
                        box_name: self.config.name.clone(),
                        message: e.to_string(),
                    })?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if !resp.status().is_success() {
                    return Err(TransportError::Remote {
                        box_name: self.config.name.clone(),
                        status: resp.status().as_u16().to_string(),
                        message: "failed to fetch code-mode docs".into(),
                    });
                }
                let docs: CodeModeDocs = resp.json().await.map_err(|e| TransportError::Http {
                    box_name: self.config.name.clone(),
                    message: format!("invalid code-mode docs JSON: {e}"),
                })?;
                Ok(Some(docs))
            }
        }
    }
}
