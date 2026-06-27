//! Dashboard rasterisation behind a trait (#143 D).
//!
//! Chat adapters rasterise a dashboard surface to a PNG one of two ways:
//!   * [`Client`] — the default in-tree `triton-rasterizer` sidecar over
//!     HTTP (`POST /v1/dashboard.png`);
//!   * [`UpstreamRasterizer`] — an opt-in delegation to a registered
//!     upstream tool `render_a2ui_to_png`, dispatched through the same
//!     [`Dispatcher`] (and therefore the same bearer minting, SSRF guard,
//!     circuit breaker, and audit line) as any other tool call.
//!
//! Both implement [`DashboardRasterizer`]; the binary wires whichever the
//! deployment opted into and the adapters call it the same way. The
//! sidecar stays the default, so the change is non-breaking.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use triton_core::{Dispatcher, Principal, TritonError};

use crate::{Client, DashboardRequest, RasterizerError};

/// Abstracts "turn this dashboard spec into PNG bytes" so the chat
/// adapters don't care whether the bytes come from the sidecar or a
/// delegated upstream tool. The `principal` is the resolved chat sender —
/// unused by the sidecar, forwarded into the minted token by the upstream
/// path so the renderer sees the same identity as a normal dispatch.
#[async_trait]
pub trait DashboardRasterizer: Send + Sync {
    async fn render(
        &self,
        req: &DashboardRequest,
        principal: &Principal,
    ) -> Result<Vec<u8>, RasterizerError>;
}

#[async_trait]
impl DashboardRasterizer for Client {
    async fn render(
        &self,
        req: &DashboardRequest,
        _principal: &Principal,
    ) -> Result<Vec<u8>, RasterizerError> {
        // The sidecar is identity-agnostic (the substrate ACL fronts it);
        // the inherent `Client::render` carries the whole contract.
        Client::render(self, req).await
    }
}

/// Delegates rasterisation to a registered upstream tool
/// `render_a2ui_to_png` (#143 D). The tool is invoked through the
/// dispatcher exactly like any other tool call — so minted bearer,
/// principal forwarding, SSRF egress guard, per-tool circuit breaker, and
/// the single audit line all apply. The upstream returns the PNG as
/// base64 in a `png_base64` field.
pub struct UpstreamRasterizer {
    dispatcher: Arc<Dispatcher>,
    tool: String,
}

impl UpstreamRasterizer {
    pub fn new(dispatcher: Arc<Dispatcher>, tool: impl Into<String>) -> Self {
        Self {
            dispatcher,
            tool: tool.into(),
        }
    }
}

impl std::fmt::Debug for UpstreamRasterizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamRasterizer")
            .field("tool", &self.tool)
            .finish_non_exhaustive()
    }
}

/// Map a dispatch error onto the adapter-facing rasterizer error
/// categories so the existing fallback decisions (retry vs surface)
/// behave the same whether the sidecar or an upstream produced the fault.
fn dispatch_err(e: TritonError) -> RasterizerError {
    match e {
        TritonError::Validation(m) | TritonError::Forbidden(m) | TritonError::Auth(m) => {
            RasterizerError::BadRequest(m)
        }
        other => RasterizerError::Server(other.to_string()),
    }
}

#[async_trait]
impl DashboardRasterizer for UpstreamRasterizer {
    async fn render(
        &self,
        req: &DashboardRequest,
        principal: &Principal,
    ) -> Result<Vec<u8>, RasterizerError> {
        // The dashboard spec (`{title, tiles}`) is the tool's input — the
        // same JSON the sidecar accepts.
        let args = serde_json::to_value(req)
            .map_err(|e| RasterizerError::BadRequest(format!("encode dashboard spec: {e}")))?;
        let dispatch = self
            .dispatcher
            .invoke(&self.tool, args, principal.clone(), "chat")
            .await
            .map_err(dispatch_err)?;
        let b64 = dispatch
            .result
            .get("png_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RasterizerError::Server(format!("{} returned no png_base64 field", self.tool))
            })?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| RasterizerError::Server(format!("bad base64 png from {}: {e}", self.tool)))
    }
}
