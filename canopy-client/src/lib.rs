//! Canopy Client - Shared runtime for CLI and MCP
//!
//! Provides the `ClientRuntime` that owns both standalone and service modes,
//! so CLI and MCP stay in sync without leaking mode branching to callers.

pub mod dirty;
pub mod merge;
pub mod predict;
pub mod provenance;
pub mod runtime;
pub mod service_client;

pub use canopy_core::ExpandOutcome;
pub use provenance::HandleProvenance;
pub use runtime::{ClientRuntime, IndexResult};
pub use service_client::{ReindexResponse, ServiceClient, ServiceStatus};
