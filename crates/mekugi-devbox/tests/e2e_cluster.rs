use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mekugi_transport::cluster::ClusterManager;
use mekugi_transport::config::{BoxConfig, ClusterConfig};
use tempfile::TempDir;
use tokio::process::{Child, Command};

fn find_host_bin() -> Option<PathBuf> {
    for candidate in [
        std::env::var("CODE_MODE_HOST_BIN").ok(),
        Some("/tmp/mekugi-release/codex-code-mode-host".into()),
        Some("/usr/local/bin/codex-code-mode-host".into()),
    ]
    .into_iter()
    .flatten()
    {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start_devbox(
    host_bin: &Path,
    workdir: &Path,
    token: &str,
    docs_path: &str,
) -> (Child, u16, u16) {
    let cm_port = free_port();
    let grpc_port = free_port();
    let http_port = free_port();

    let bin_path = std::env::var("CARGO_BIN_EXE_mekugi-devbox")
        .expect("CARGO_BIN_EXE_mekugi-devbox should be set by cargo test");
    let child = Command::new(bin_path)
        .env("CODE_MODE_HOST_BIN", host_bin)
        .env("CODE_MODE_HOST_TARGET", format!("127.0.0.1:{cm_port}"))
        .env("DEVBOX_WORKDIR", workdir)
        .env("DEVBOX_HOST", "127.0.0.1")
        .env("DEVBOX_PORT", http_port.to_string())
        .env("DEVBOX_GRPC_ADDR", format!("127.0.0.1:{grpc_port}"))
        .env("DEVBOX_INTERNAL_TOKEN", token)
        .env("CODE_MODE_DOCS_PATH", docs_path)
        .env("LOG_LEVEL", "warn")
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn mekugi-devbox");

    // Wait until gRPC GetInfo succeeds
    let probe_cfg = ClusterConfig::single_box(
        Some("probe"),
        format!("grpc://127.0.0.1:{grpc_port}"),
        Some(token.to_owned()),
    )
    .validate(None)
    .unwrap();
    let probe = ClusterManager::new(probe_cfg, None).unwrap();

    for _ in 0..80 {
        let boxes = probe.list_boxes(500).await;
        if boxes.first().is_some_and(|b| b.status == "ok") {
            return (child, grpc_port, http_port);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("mekugi-devbox did not become healthy in time");
}

#[tokio::test]
async fn test_rust_devbox_multi_box_cluster_e2e() {
    let Some(host_bin) = find_host_bin() else {
        eprintln!("skipping e2e test: codex-code-mode-host not installed");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let dir_a = tmp.path().join("alpha");
    let dir_b = tmp.path().join("beta");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    let docs_file = tmp.path().join("docs.json");
    std::fs::write(
        &docs_file,
        serde_json::json!({
            "source_repo": "mekugi-harness/mekugi",
            "source_ref": "rust-e2e-rev",
            "exec_description_template": "Rust e2e exec {{ default_exec_yield_time_ms }} {{ image_helper }}",
            "wait_description_template": "Rust e2e wait",
            "image_helper_description": "img-helper"
        })
        .to_string(),
    )
    .unwrap();

    let (mut child_a, grpc_a, _http_a) =
        start_devbox(&host_bin, &dir_a, "tok-a", docs_file.to_str().unwrap()).await;
    let (mut child_b, grpc_b, _http_b) = start_devbox(&host_bin, &dir_b, "tok-b", "").await;

    // 1. Auth rejection on wrong token
    let bad_cfg = ClusterConfig::single_box(
        Some("alpha"),
        format!("grpc://127.0.0.1:{grpc_a}"),
        Some("wrong-tok".into()),
    )
    .validate(None)
    .unwrap();
    let bad_cluster = ClusterManager::new(bad_cfg, None).unwrap();
    let err = bad_cluster
        .exec(None, "text('nope');".into(), 5000, 1000)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Unauthenticated") || err.to_string().contains("Unauthorized"));

    // 2. Multi-box cluster (alpha default, beta worker, offline unreachable)
    let cfg = ClusterConfig {
        default_box: Some("alpha".into()),
        boxes: vec![
            BoxConfig {
                name: "alpha".into(),
                target: format!("grpc://127.0.0.1:{grpc_a}"),
                token: Some("tok-a".into()),
                token_env: None,
                description: Some("Primary Rust devbox".into()),
                labels: BTreeMap::from([("role".into(), "primary".into())]),
                pool_size: Some(1),
                max_yield_time_ms: None,
                ca_cert_path: None,
            },
            BoxConfig {
                name: "beta".into(),
                target: format!("grpc://127.0.0.1:{grpc_b}"),
                token: Some("tok-b".into()),
                token_env: None,
                description: Some("Worker Rust devbox".into()),
                labels: BTreeMap::from([("role".into(), "worker".into())]),
                pool_size: Some(2),
                max_yield_time_ms: None,
                ca_cert_path: None,
            },
            BoxConfig {
                name: "offline".into(),
                target: "grpc://127.0.0.1:9".into(),
                token: None,
                token_env: None,
                description: Some("Offline machine".into()),
                labels: BTreeMap::new(),
                pool_size: None,
                max_yield_time_ms: None,
                ca_cert_path: None,
            },
        ],
    }
    .validate(None)
    .unwrap();

    let cluster = Arc::new(ClusterManager::new(cfg, None).unwrap());

    // 3. list_boxes + GetCodeModeDocs over gRPC
    let boxes = cluster.list_boxes(1500).await;
    assert_eq!(boxes.len(), 3);
    assert_eq!(boxes[0].status, "ok");
    assert!(boxes[0].is_default);
    assert_eq!(boxes[0].workdir.as_deref(), Some(dir_a.to_str().unwrap()));
    assert_eq!(boxes[1].status, "ok");
    assert_eq!(boxes[1].workdir.as_deref(), Some(dir_b.to_str().unwrap()));
    assert_eq!(boxes[2].status, "unreachable");

    let docs = cluster
        .fetch_code_mode_docs(None, 2000)
        .await
        .unwrap()
        .expect("docs should be returned from alpha");
    assert_eq!(docs.source_ref, "rust-e2e-rev");

    // 4. Default box exec + apply_patch + exec_command
    let default_out = cluster
        .exec(
            None,
            r#"const p = await tools.apply_patch("*** Begin Patch\n*** Add File: demo.txt\n+rust-native\n*** End Patch");
const r = await tools.exec_command({ cmd: "pwd && cat demo.txt" });
text(p + "\n" + r);"#
                .into(),
            10_000,
            10_000,
        )
        .await
        .unwrap();
    assert!(default_out.text.contains("A demo.txt"));
    assert!(default_out.text.contains("rust-native"));
    assert!(default_out.text.contains(dir_a.to_str().unwrap()));

    // 5. Explicit box="beta" yield + stateless wait without passing box
    let yielded = cluster
        .exec(
            Some("beta"),
            r#"await new Promise(r => setTimeout(r, 350)); text("beta-ok:" + await tools.exec_command({ cmd: "pwd" }));"#
                .into(),
            50,
            10_000,
        )
        .await
        .unwrap();
    assert_eq!(yielded.status, "yielded");
    assert!(yielded.cell_id.starts_with("beta:"));
    assert!(yielded.text.contains(&yielded.cell_id));

    let waited = cluster
        .wait(&yielded.cell_id, None, 5000, 10_000, false)
        .await
        .unwrap();
    assert!(waited.text.contains("beta-ok:"));
    assert!(waited.text.contains(dir_b.to_str().unwrap()));

    // 6. Concurrent slow + fast command on SAME single HTTP/2 channel (alpha) do not block
    let c1 = cluster.clone();
    let slow_handle = tokio::spawn(async move {
        c1.exec(
            Some("alpha"),
            r#"await new Promise(r => setTimeout(r, 900)); text("slow-ok");"#.into(),
            5000,
            10_000,
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let fast = cluster
        .exec(Some("alpha"), r#"text("fast-ok");"#.into(), 5000, 10_000)
        .await
        .unwrap();
    assert!(fast.text.contains("fast-ok"));
    assert!(
        !slow_handle.is_finished(),
        "fast command did not finish before slow command on same HTTP/2 channel"
    );
    let slow = slow_handle.await.unwrap();
    assert!(slow.text.contains("slow-ok"));

    let _ = child_a.start_kill();
    let _ = child_b.start_kill();
}

#[cfg(unix)]
#[tokio::test]
async fn test_systemd_sd_notify_lifecycle() {
    use std::os::unix::net::UnixDatagram;

    let Some(host_bin) = find_host_bin() else {
        eprintln!("skipping e2e test: codex-code-mode-host not installed");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let notify_sock = tmp.path().join("notify.sock");
    let sock = UnixDatagram::bind(&notify_sock).unwrap();
    sock.set_nonblocking(true).unwrap();

    let cm_port = free_port();
    let grpc_port = free_port();
    let bin_path = std::env::var("CARGO_BIN_EXE_mekugi-devbox")
        .expect("CARGO_BIN_EXE_mekugi-devbox should be set by cargo test");

    let mut child = Command::new(bin_path)
        .env("CODE_MODE_HOST_BIN", &host_bin)
        .env("CODE_MODE_HOST_TARGET", format!("127.0.0.1:{cm_port}"))
        .env("DEVBOX_WORKDIR", tmp.path())
        .env("DEVBOX_GRPC_ADDR", format!("127.0.0.1:{grpc_port}"))
        .env("DEVBOX_PORT", "")
        .env("DEVBOX_INTERNAL_TOKEN", "systemd-test-token")
        .env("CODE_MODE_DOCS_PATH", "")
        .env("NOTIFY_SOCKET", &notify_sock)
        .env("WATCHDOG_USEC", "400000")
        .env("LOG_LEVEL", "warn")
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn mekugi-devbox");

    async fn recv_notify(sock: &UnixDatagram, timeout: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = [0u8; 512];
        loop {
            match sock.recv(&mut buf) {
                Ok(n) => return Some(String::from_utf8_lossy(&buf[..n]).into_owned()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if tokio::time::Instant::now() >= deadline {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("notify socket recv failed: {e}"),
            }
        }
    }

    let ready = recv_notify(&sock, Duration::from_secs(10))
        .await
        .expect("timed out waiting for READY=1");
    assert!(ready.contains("READY=1"), "unexpected ready msg: {ready}");
    assert!(
        ready.contains(&format!("gRPC 127.0.0.1:{grpc_port}")),
        "expected gRPC addr in STATUS: {ready}"
    );
    assert!(
        !ready.contains("HTTP"),
        "HTTP should be disabled when DEVBOX_PORT is empty: {ready}"
    );

    let watchdog = recv_notify(&sock, Duration::from_secs(2))
        .await
        .expect("timed out waiting for WATCHDOG=1");
    assert_eq!(watchdog, "WATCHDOG=1");

    let pid = child.id().expect("child pid");
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("failed to send SIGTERM");
    assert!(status.success());

    let mut saw_stopping = false;
    for _ in 0..10 {
        if let Some(msg) = recv_notify(&sock, Duration::from_secs(2)).await {
            if msg.contains("STOPPING=1") {
                saw_stopping = true;
                break;
            }
        } else {
            break;
        }
    }
    assert!(saw_stopping, "did not receive STOPPING=1 on SIGTERM");

    let exit = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("timed out waiting for graceful exit")
        .expect("wait failed");
    assert!(exit.success(), "expected clean exit on SIGTERM, got {exit}");
}
