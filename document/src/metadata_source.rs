//! Trait by which `from_runtime` reads editor-side per-node metadata without depending on the
//! editor crate. The editor implements this on its `NodeNetworkInterface` (or equivalent); tests
//! and CLI tools pass [`NoMetadata`] when there's nothing to thread through.
//!
//! `network_path` is the chain of runtime local `NodeId`s from the root network down to (but not
//! including) the node being queried, matching the addressing scheme used by
//! `NodeNetworkInterface::node_metadata(node_id, network_path)`.

use std::collections::HashMap;

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
	/// Per-input metadata. Length always equals the runtime node's `inputs.len()` after conversion;
	/// slots with no editor metadata carry a default `InputMetadataEntry`. The editor errors on
	/// rebuild if the length disagrees with the converted `NodeNetwork`.
	pub input_metadata: Vec<InputMetadataEntry>,
	/// User-chosen names for the node's output slots. Empty when no overrides were set.
	pub output_names: Vec<String>,
}

impl NodeMetadataEntry {
	pub fn is_empty(&self) -> bool {
		self.position.is_none()
			&& !self.is_layer
			&& self.display_name.is_none()
			&& !self.locked
			&& !self.pinned
			&& self.output_names.is_empty()
			&& self.input_metadata.iter().all(InputMetadataEntry::is_empty)
	}
}

/// One network's worth of editor metadata. Distinct from `NodeMetadataEntry` because navigation
/// state and the previewing flag are properties of a *network* (one per nested graph), not of any
/// node. `to_runtime_with_metadata` returns these alongside the node entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkMetadataEntry {
	/// Path of runtime local IDs identifying *which* network. Empty path = root network; otherwise
	/// the chain of owning node IDs from the root down to and including the node that contains
	/// this network.
	pub network_path: Vec<RuntimeNodeId>,
	/// JSON of the runtime `PTZ` type (pan / tilt / zoom).
	pub navigation_ptz: Option<serde_json::Value>,
	/// JSON of `DAffine2` (node-graph-space to viewport-space transform).
	pub navigation_transform: Option<serde_json::Value>,
	/// Width of the node graph in viewport space.
	pub navigation_width: Option<f64>,
	/// JSON of the runtime `Previewing` enum. `None` means the runtime default (`Previewing::No`).
	pub previewing: Option<serde_json::Value>,
	/// Definition-lineage tag. Tied to the *network* (i.e., the sub-graph's identity), not the
	/// containing node — matches the runtime's `NodeNetworkPersistentMetadata::reference`.
	pub reference: Option<String>,
}

impl NetworkMetadataEntry {
	pub fn is_empty(&self) -> bool {
		self.navigation_ptz.is_none() && self.navigation_transform.is_none() && self.navigation_width.is_none() && self.previewing.is_none() && self.reference.is_none()
	}
}

/// Per-input editor metadata extracted from `Node.inputs_attributes`. Mirrors the runtime's
/// `InputPersistentMetadata` shape, but with `Option` around the string fields so an unset name
/// (the runtime default `""`) is distinguishable from an explicit empty string.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputMetadataEntry {
	pub input_name: Option<String>,
	pub input_description: Option<String>,
	pub widget_override: Option<String>,
	/// Reassembled from all `ui::input_data::<sub_key>` attributes on this input slot.
	pub input_data: HashMap<String, serde_json::Value>,
}

impl InputMetadataEntry {
	pub fn is_empty(&self) -> bool {
		self.input_name.is_none() && self.input_description.is_none() && self.widget_override.is_none() && self.input_data.is_empty()
	}
}

/// Editor-side metadata source. Every method has a no-op default so implementors only override
/// what they actually carry; `NoMetadata` is `impl NodeMetadataSource for NoMetadata {}`.
pub trait NodeMetadataSource {
	fn position(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId) -> Option<Position> {
		None
	}
	fn is_layer(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId) -> bool {
		false
	}
	fn display_name(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId) -> Option<&str> {
		None
	}
	fn locked(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId) -> bool {
		false
	}
	fn pinned(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId) -> bool {
		false
	}
	/// User-chosen names for each output slot. Empty vec = no overrides. The storage layer writes
	/// the whole vec as a single `ui::output_names` attribute (per-slot LWW is overkill for
	/// rename-on-output, which is a rare concurrent edit).
	fn output_names(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId) -> Vec<String> {
		Vec::new()
	}

	// Per-input methods. `input_index` is positional into the runtime node's `inputs` vec.
	// `from_runtime` calls these for each input slot it materializes.
	fn input_name(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId, _input_index: usize) -> Option<&str> {
		None
	}
	fn input_description(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId, _input_index: usize) -> Option<&str> {
		None
	}
	fn widget_override(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId, _input_index: usize) -> Option<&str> {
		None
	}
	/// Returns the full `input_data` map for an input. Each entry becomes a separate
	/// `ui::input_data::<key>` attribute in storage so concurrent edits to different sub-keys
	/// converge under per-key LWW. Returning an owned `HashMap` keeps the trait object-safe.
	fn input_data(&self, _network_path: &[RuntimeNodeId], _local_id: RuntimeNodeId, _input_index: usize) -> HashMap<String, serde_json::Value> {
		HashMap::new()
	}

	// Per-network methods. `network_path` is the chain of owning runtime node IDs from the root
	// network down to and including the node containing this network. Empty = root network.
	// Returns are JSON-shaped because the underlying runtime types live editor-side and aren't
	// available in `graph-storage`; storage just passes the blobs through.
	fn navigation_ptz(&self, _network_path: &[RuntimeNodeId]) -> Option<serde_json::Value> {
		None
	}
	fn navigation_transform(&self, _network_path: &[RuntimeNodeId]) -> Option<serde_json::Value> {
		None
	}
	fn navigation_width(&self, _network_path: &[RuntimeNodeId]) -> Option<f64> {
		None
	}
	/// `Previewing` value as JSON. The runtime default (`Previewing::No`) maps to `None`, since
	/// emitting the default would clutter every legacy document's storage with an inert flag.
	fn previewing(&self, _network_path: &[RuntimeNodeId]) -> Option<serde_json::Value> {
		None
	}
	/// Definition-lineage tag for this network (the editor calls this `reference`). Per-network,
	/// not per-node: identifies the `DocumentNodeDefinition` the sub-graph was instantiated from.
	fn reference(&self, _network_path: &[RuntimeNodeId]) -> Option<&str> {
		None
	}
}

/// A no-op metadata source. Use when running a conversion that has no editor metadata to attach
/// (synthetic test networks, legacy-document migration before editor wiring exists, etc.).
pub struct NoMetadata;

impl NodeMetadataSource for NoMetadata {}
