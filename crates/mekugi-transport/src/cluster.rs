use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::box_client::{BoxNode, BoxStatus, CodeModeDocs, ExecOutcome, TransportError};
use crate::config::ValidatedClusterConfig;
use crate::routing::{format_cell_id, parse_cell_route, rewrite_cell_id_in_text};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterHealth {
    pub status: String,
    pub default_box: String,
    pub boxes: Vec<BoxStatus>,
}

/// Lock-free multi-box cluster manager.
///
/// Every box owns an independent persistent keep-alive transport (multiplexed
/// HTTP/2 gRPC channel pool or persistent HTTP/2+HTTP/1.1 client). The map is
/// immutable after construction so hot-path exec/wait calls acquire zero locks.
#[derive(Debug)]
pub struct ClusterManager {
    default_box: String,
    order: Vec<String>,
    boxes: HashMap<String, Arc<BoxNode>>,
}

impl ClusterManager {
    pub fn new(
        validated: ValidatedClusterConfig,
        fallback_token: Option<&str>,
    ) -> Result<Self, TransportError> {
        let mut order = Vec::with_capacity(validated.boxes.len());
        let mut boxes = HashMap::with_capacity(validated.boxes.len());

        for box_cfg in validated.boxes {
            let name = box_cfg.name.clone();
            let node = Arc::new(BoxNode::new(box_cfg, fallback_token)?);
            order.push(name.clone());
            boxes.insert(name, node);
        }

        Ok(Self {
            default_box: validated.default_box,
            order,
            boxes,
        })
    }

    pub fn default_box(&self) -> &str {
        &self.default_box
    }

    pub fn box_names(&self) -> &[String] {
        &self.order
    }

    pub fn resolve_box(&self, box_name: Option<&str>) -> Result<Arc<BoxNode>, TransportError> {
        let target = box_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_box);

        self.boxes
            .get(target)
            .cloned()
            .ok_or_else(|| TransportError::UnknownBox {
                requested: target.to_owned(),
                available: self.order.join(", "),
            })
    }

    pub async fn exec(
        &self,
        box_name: Option<&str>,
        code: String,
        yield_time_ms: u32,
        max_output_tokens: u32,
    ) -> Result<ExecOutcome, TransportError> {
        let node = self.resolve_box(box_name)?;
        let mut outcome = node.exec(code, yield_time_ms, max_output_tokens).await?;
        if !outcome.cell_id.is_empty() {
            let namespaced = format_cell_id(&node.config.name, &outcome.cell_id);
            outcome.text = rewrite_cell_id_in_text(&outcome.text, &outcome.cell_id, &namespaced);
            outcome.cell_id = namespaced;
        }
        Ok(outcome)
    }

    pub async fn wait(
        &self,
        cell_id: &str,
        box_name: Option<&str>,
        yield_time_ms: u32,
        max_output_tokens: u32,
        terminate: bool,
    ) -> Result<ExecOutcome, TransportError> {
        let (target_box, raw_cell_id) = parse_cell_route(cell_id, box_name, &self.default_box)?;
        let node = self.resolve_box(Some(&target_box))?;
        let mut outcome = node
            .wait(raw_cell_id.clone(), yield_time_ms, max_output_tokens, terminate)
            .await?;
        let raw_returned = if outcome.cell_id.is_empty() {
            raw_cell_id
        } else {
            outcome.cell_id.clone()
        };
        let namespaced = format_cell_id(&node.config.name, &raw_returned);
        outcome.text = rewrite_cell_id_in_text(&outcome.text, &raw_returned, &namespaced);
        outcome.cell_id = namespaced;
        Ok(outcome)
    }

    /// Probe all configured devboxes in parallel with a bounded per-box timeout
    /// so an offline or slow machine never stalls list_boxes.
    pub async fn list_boxes(&self, probe_timeout_ms: u64) -> Vec<BoxStatus> {
        let timeout = Duration::from_millis(probe_timeout_ms.clamp(200, 15_000));
        let futs = self.order.iter().filter_map(|name| {
            let node = self.boxes.get(name)?.clone();
            let is_default = name == &self.default_box;
            Some(async move { node.probe_status(is_default, timeout).await })
        });
        join_all(futs).await
    }

    pub async fn health_all(&self, probe_timeout_ms: u64) -> ClusterHealth {
        let boxes = self.list_boxes(probe_timeout_ms).await;
        let default_ok = boxes
            .iter()
            .find(|b| b.is_default)
            .is_some_and(|b| b.status == "ok");
        let status = if default_ok { "ok" } else { "degraded" }.to_owned();
        ClusterHealth {
            status,
            default_box: self.default_box.clone(),
            boxes,
        }
    }

    pub async fn fetch_code_mode_docs(
        &self,
        box_name: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Option<CodeModeDocs>, TransportError> {
        let node = self.resolve_box(box_name)?;
        let timeout = Duration::from_millis(timeout_ms.clamp(200, 15_000));
        node.fetch_code_mode_docs(timeout).await
    }
}
