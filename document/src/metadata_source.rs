//! Trait by which `from_runtime` reads editor-side per-node metadata without depending on the
//! editor crate. The editor implements this on its `NodeNetworkInterface` (or equivalent); tests
//! and CLI tools pass [`NoMetadata`] when there's nothing to thread through.
//!
//! `network_path` is the chain of runtime local `NodeId`s from the root network down to (but not
//! including) the node being queried, matching the addressing scheme used by
//! `NodeNetworkInterface::node_metadata(node_id, network_path)`.

use core_types::uuid::NodeId as RuntimeNodeId;

use crate::Position;

/// One node's worth of editor-side metadata produced by `Registry::to_runtime_with_metadata`.
/// The editor consumes a `Vec<NodeMetadataEntry>` and reassembles its own nested
/// `NodeNetworkMetadata` from it.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeMetadataEntry {
	/// Path of runtime local IDs from the root network down to (but not including) this node.
	pub network_path: Vec<RuntimeNodeId>,
	pub local_id: RuntimeNodeId,
	pub position: Option<Position>,
	pub is_layer: bool,
	pub display_name: Option<String>,
	pub locked: bool,
	pub pinned: bool,
}

pub trait NodeMetadataSource {
	fn position(&self, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> Option<Position>;
	fn is_layer(&self, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> bool;
	fn display_name(&self, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> Option<&str>;
	fn locked(&self, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> bool;
	fn pinned(&self, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> bool;
}

/// A no-op metadata source. Use when running a conversion that has no editor metadata to attach
/// (synthetic test networks, legacy-document migration before editor wiring exists, etc.).
pub struct NoMetadata;

impl NodeMetadataSource for NoMetadata {
	fn position(&self, _: &[RuntimeNodeId], _: RuntimeNodeId) -> Option<Position> {
		None
	}
	fn is_layer(&self, _: &[RuntimeNodeId], _: RuntimeNodeId) -> bool {
		false
	}
	fn display_name(&self, _: &[RuntimeNodeId], _: RuntimeNodeId) -> Option<&str> {
		None
	}
	fn locked(&self, _: &[RuntimeNodeId], _: RuntimeNodeId) -> bool {
		false
	}
	fn pinned(&self, _: &[RuntimeNodeId], _: RuntimeNodeId) -> bool {
		false
	}
}
