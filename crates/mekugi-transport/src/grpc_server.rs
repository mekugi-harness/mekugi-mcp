use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::pb;
use crate::pb::devbox_agent_server::{DevboxAgent, DevboxAgentServer};

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

#[async_trait]
pub trait DevboxBackend: Send + Sync + 'static {
    async fn exec(&self, req: pb::ExecRequest) -> Result<pb::ExecResponse, Status>;
    async fn wait(&self, req: pb::WaitRequest) -> Result<pb::WaitResponse, Status>;
    async fn get_info(&self) -> Result<pb::GetInfoResponse, Status>;
    async fn get_health(&self) -> Result<pb::GetHealthResponse, Status>;
    async fn get_code_mode_docs(&self) -> Result<pb::GetCodeModeDocsResponse, Status>;
}

struct DevboxAgentService<B: DevboxBackend> {
    backend: Arc<B>,
    expected_token: Option<String>,
}

impl<B: DevboxBackend> DevboxAgentService<B> {
    #[allow(clippy::result_large_err)]
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let Some(expected) = &self.expected_token else {
            return Ok(());
        };
        let header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let Some(provided) = header.strip_prefix("Bearer ") else {
            return Err(Status::unauthenticated("Unauthorized"));
        };
        if provided.as_bytes().ct_eq(expected.as_bytes()).into() {
            Ok(())
        } else {
            Err(Status::unauthenticated("Unauthorized"))
        }
    }
}

#[tonic::async_trait]
impl<B: DevboxBackend> DevboxAgent for DevboxAgentService<B> {
    async fn exec(
        &self,
        request: Request<pb::ExecRequest>,
    ) -> Result<Response<pb::ExecResponse>, Status> {
        self.authorize(&request)?;
        let resp = self.backend.exec(request.into_inner()).await?;
        Ok(Response::new(resp))
    }

    async fn wait(
        &self,
        request: Request<pb::WaitRequest>,
    ) -> Result<Response<pb::WaitResponse>, Status> {
        self.authorize(&request)?;
        let resp = self.backend.wait(request.into_inner()).await?;
        Ok(Response::new(resp))
    }

    async fn get_info(
        &self,
        request: Request<pb::GetInfoRequest>,
    ) -> Result<Response<pb::GetInfoResponse>, Status> {
        self.authorize(&request)?;
        let resp = self.backend.get_info().await?;
        Ok(Response::new(resp))
    }

    async fn get_health(
        &self,
        request: Request<pb::GetHealthRequest>,
    ) -> Result<Response<pb::GetHealthResponse>, Status> {
        let resp = self.backend.get_health().await?;
        let _ = request;
        Ok(Response::new(resp))
    }

    async fn get_code_mode_docs(
        &self,
        request: Request<pb::GetCodeModeDocsRequest>,
    ) -> Result<Response<pb::GetCodeModeDocsResponse>, Status> {
        self.authorize(&request)?;
        let resp = self.backend.get_code_mode_docs().await?;
        Ok(Response::new(resp))
    }
}

pub struct RunningGrpcServer {
    pub local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl RunningGrpcServer {
    pub async fn start<B: DevboxBackend>(
        listen_addr: &str,
        expected_token: Option<String>,
        backend: Arc<B>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        let incoming = tonic::transport::server::TcpIncoming::from(listener)
            .with_nodelay(Some(true))
            .with_keepalive(Some(TCP_KEEPALIVE));

        let token = expected_token
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());

        let svc = DevboxAgentServer::new(DevboxAgentService {
            backend,
            expected_token: token,
        })
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let join_handle = tokio::spawn(async move {
            Server::builder()
                .tcp_nodelay(true)
                .http2_keepalive_interval(Some(HTTP2_KEEPALIVE_INTERVAL))
                .http2_keepalive_timeout(Some(HTTP2_KEEPALIVE_TIMEOUT))
                .http2_adaptive_window(Some(true))
                .add_service(svc)
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            join_handle,
        })
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.join_handle.await;
    }
}
