//! Bridge between the editor's `NodeNetworkInterface` and `graph-storage`'s `NodeMetadataSource`
//! trait, plus an integration round-trip test against a real demo `.graphite` document.
//!
//! The `NodeMetadataSource` impl lives on a thin wrapper [`StorageMetadataView`] rather than on
//! `NodeNetworkInterface` itself. Several trait method names (`position`, `is_layer`,
//! `display_name`) collide with existing inherent methods on `NodeNetworkInterface`, and Rust
//! silently resolves bare calls to the inherent ones. The wrapper isolates the trait surface so
//! callers can't accidentally invoke the wrong method.

use std::collections::HashMap;

use glam::IVec2;
use graph_craft::document::{DocumentNodeImplementation, NodeId, NodeNetwork};
use graph_storage::{NodeMetadataEntry, NodeMetadataSource, Position};

use super::memo_network::MemoNetwork;
use super::{DocumentNodeTransientMetadata, LayerPosition, NodeNetworkInterface, NodeNetworkMetadata, NodePersistentMetadata, NodePosition, NodeTypePersistentMetadata};

/// Adapts a `&NodeNetworkInterface` to `graph-storage`'s `NodeMetadataSource`. Construct with
/// [`StorageMetadataView::new`] and pass to `Registry::from_runtime_with_metadata`.
pub struct StorageMetadataView<'a> {
	interface: &'a NodeNetworkInterface,
}

impl<'a> StorageMetadataView<'a> {
	pub fn new(interface: &'a NodeNetworkInterface) -> Self {
		Self { interface }
	}
}

impl NodeMetadataSource for StorageMetadataView<'_> {
	fn position(&self, network_path: &[NodeId], local_id: NodeId) -> Option<Position> {
		let metadata = self.interface.node_metadata(&local_id, network_path)?;
		match &metadata.persistent_metadata.node_type_metadata {
			NodeTypePersistentMetadata::Layer(layer) => match layer.position {
				LayerPosition::Absolute(v) => Some(Position::Absolute([v.x, v.y])),
				LayerPosition::Stack(offset) => Some(Position::Stack(offset)),
			},
			NodeTypePersistentMetadata::Node(node) => match *node.position() {
				NodePosition::Absolute(v) => Some(Position::Absolute([v.x, v.y])),
				NodePosition::Chain => Some(Position::Chain),
			},
		}
	}

	fn is_layer(&self, network_path: &[NodeId], local_id: NodeId) -> bool {
		self.interface
			.node_metadata(&local_id, network_path)
			.map(|m| matches!(m.persistent_metadata.node_type_metadata, NodeTypePersistentMetadata::Layer(_)))
			.unwrap_or(false)
	}

	fn display_name(&self, network_path: &[NodeId], local_id: NodeId) -> Option<&str> {
		let metadata = self.interface.node_metadata(&local_id, network_path)?;
		Some(metadata.persistent_metadata.display_name.as_str())
	}

	fn locked(&self, network_path: &[NodeId], local_id: NodeId) -> bool {
		self.interface.node_metadata(&local_id, network_path).map(|m| m.persistent_metadata.locked).unwrap_or(false)
	}

	fn pinned(&self, network_path: &[NodeId], local_id: NodeId) -> bool {
		self.interface.node_metadata(&local_id, network_path).map(|m| m.persistent_metadata.pinned).unwrap_or(false)
	}
}

/// Maps a storage-side `Position` back to the runtime's `(NodeTypePersistentMetadata, IVec2)` shape
/// the editor expects. `is_layer` decides which variant to construct; `Chain` collapses to
/// `IVec2::ZERO` for layers and `Stack(n)` collapses to a node default for non-layers, since those
/// combinations shouldn't arise from a faithful round-trip.
pub fn position_to_runtime(position: Position, is_layer: bool) -> NodeTypePersistentMetadata {
	match (position, is_layer) {
		(Position::Absolute([x, y]), true) => NodeTypePersistentMetadata::layer(IVec2::new(x, y)),
		(Position::Absolute([x, y]), false) => NodeTypePersistentMetadata::node(IVec2::new(x, y)),
		(Position::Stack(offset), _) => {
			// Stack only makes sense for layers.
			let mut metadata = NodeTypePersistentMetadata::layer(IVec2::ZERO);
			if let NodeTypePersistentMetadata::Layer(layer) = &mut metadata {
				layer.position = LayerPosition::Stack(offset);
			}
			metadata
		}
		(Position::Chain, _) => {
			// Chain only makes sense for non-layer nodes.
			NodeTypePersistentMetadata::Node(NodePersistentMetadata::new(NodePosition::Chain))
		}
	}
}

/// Build a `NodeNetworkInterface` from a storage-converted `NodeNetwork` and a flat list of
/// `NodeMetadataEntry` values. Seeds a default `DocumentNodeMetadata` for every node in every
/// nested network, then patches in the per-node fields the entries carried.
///
/// Reaches into the private `network` / `network_metadata` fields directly (rather than going
/// through public setters) because the public setters carry transient-cache invalidation logic
/// that's irrelevant when constructing a fresh interface from a self-consistent storage snapshot.
pub fn build_interface_from_storage(network: NodeNetwork, entries: Vec<NodeMetadataEntry>) -> NodeNetworkInterface {
	let mut network_metadata = NodeNetworkMetadata::default();
	seed_metadata_tree(&network, &mut network_metadata);
	apply_entries_into_tree(&mut network_metadata, entries);

	let mut interface = NodeNetworkInterface::default();
	interface.network = MemoNetwork::new(network);
	interface.network_metadata = network_metadata;
	interface
}

/// Walk `network` recursively and ensure `metadata` has a default `DocumentNodeMetadata` slot for
/// every node at every nesting level. Mirrors the editor's invariant that
/// `NodeNetworkPersistentMetadata::node_metadata` contains every document-node key.
fn seed_metadata_tree(network: &NodeNetwork, metadata: &mut NodeNetworkMetadata) {
	for (&local_id, node) in &network.nodes {
		let node_metadata = metadata.persistent_metadata.node_metadata.entry(local_id).or_default();

		if let DocumentNodeImplementation::Network(nested) = &node.implementation {
			let child = node_metadata.persistent_metadata.network_metadata.get_or_insert_with(NodeNetworkMetadata::default);
			seed_metadata_tree(nested, child);
		}
	}
}

/// Patches each entry onto the metadata tree previously seeded by [`seed_metadata_tree`]. Entries
/// whose `(network_path, local_id)` doesn't resolve (e.g., stale data) are silently skipped — the
/// caller is responsible for ensuring entries match the network.
fn apply_entries_into_tree(metadata: &mut NodeNetworkMetadata, entries: Vec<NodeMetadataEntry>) {
	// Group entries by network_path so we do one nested_metadata_mut lookup per network.
	let mut by_path: HashMap<Vec<NodeId>, Vec<NodeMetadataEntry>> = HashMap::new();
	for entry in entries {
		by_path.entry(entry.network_path.clone()).or_default().push(entry);
	}

	for (path, entries) in by_path {
		let Some(network_metadata) = metadata.nested_metadata_mut(&path) else {
			log::warn!("apply_entries_into_tree: nested network at {path:?} not found, skipping {} entries", entries.len());
			continue;
		};

		for entry in entries {
			let Some(document_node_metadata) = network_metadata.persistent_metadata.node_metadata.get_mut(&entry.local_id) else {
				log::warn!("apply_entries_into_tree: node {:?} not seeded under network {path:?}, skipping", entry.local_id);
				continue;
			};

			let persistent = &mut document_node_metadata.persistent_metadata;

			if let Some(position) = entry.position {
				persistent.node_type_metadata = position_to_runtime(position, entry.is_layer);
			} else if entry.is_layer {
				// Layer flag without a position: keep the default IVec2::ZERO that `Default` set up.
				persistent.node_type_metadata = NodeTypePersistentMetadata::layer(IVec2::ZERO);
			}

			if let Some(name) = entry.display_name {
				persistent.display_name = name;
			}
			persistent.locked = entry.locked;
			persistent.pinned = entry.pinned;

			document_node_metadata.transient_metadata = DocumentNodeTransientMetadata::default();
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use graph_storage::{NodeMetadataSource, Registry};

	use super::*;
	use crate::messages::portfolio::document::document_message_handler::DocumentMessageHandler;

	/// Load a demo `.graphite` straight into a `DocumentMessageHandler` for inspection.
	fn load_demo(file_name: &str) -> DocumentMessageHandler {
		let path = format!("../demo-artwork/{file_name}");
		let content = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read {path}: {e}"));
		DocumentMessageHandler::deserialize_document(&content).unwrap_or_else(|e| panic!("Failed to deserialize {path}: {e:?}"))
	}

	/// Walk every node in every nested network and collect `(network_path, local_id)` pairs so the
	/// test can iterate every node addressable from the metadata side.
	fn collect_all_node_paths(interface: &NodeNetworkInterface) -> Vec<(Vec<NodeId>, NodeId)> {
		fn walk(interface: &NodeNetworkInterface, path: Vec<NodeId>, out: &mut Vec<(Vec<NodeId>, NodeId)>) {
			let Some(network) = interface.nested_network(&path) else { return };
			for (&local_id, node) in &network.nodes {
				out.push((path.clone(), local_id));

				if matches!(&node.implementation, graph_craft::document::DocumentNodeImplementation::Network(_)) {
					let mut child = path.clone();
					child.push(local_id);
					walk(interface, child, out);
				}
			}
		}

		let mut out = Vec::new();
		walk(interface, Vec::new(), &mut out);
		out
	}

	/// Loads a demo artwork, round-trips its `NodeNetwork + NodeNetworkInterface metadata` through
	/// `Registry`, and asserts every node's `ui::*` attributes survive unchanged.
	///
	/// Uses one demo (rather than the full set) because this is an exhaustive per-node check.
	#[test]
	fn editor_metadata_round_trip_against_demo() {
		let document = load_demo("changing-seasons.graphite");
		let interface = &document.network_interface;
		let source = StorageMetadataView::new(interface);

		let network = interface.document_network().clone();

		let registry = Registry::from_runtime_with_metadata(&network, &source).expect("from_runtime_with_metadata failed");

		let (_converted_network, entries) = registry.to_runtime_with_metadata().expect("to_runtime_with_metadata failed");

		// Index emitted entries by their (network_path, local_id) address.
		let entries_by_address: HashMap<(Vec<NodeId>, NodeId), &graph_storage::NodeMetadataEntry> = entries.iter().map(|e| ((e.network_path.clone(), e.local_id), e)).collect();

		let mut checked_any_position = false;
		let mut checked_any_layer = false;

		for (network_path, local_id) in collect_all_node_paths(interface) {
			let expected_position = source.position(&network_path, local_id);
			let expected_is_layer = source.is_layer(&network_path, local_id);
			let expected_display = source.display_name(&network_path, local_id).map(str::to_owned);
			let expected_locked = source.locked(&network_path, local_id);
			let expected_pinned = source.pinned(&network_path, local_id);

			let any_metadata = expected_position.is_some() || expected_is_layer || expected_display.as_deref().is_some_and(|s| !s.is_empty()) || expected_locked || expected_pinned;

			if !any_metadata {
				assert!(
					entries_by_address.get(&(network_path.clone(), local_id)).is_none(),
					"node {local_id:?} in network {network_path:?} has no editor metadata but produced an entry"
				);
				continue;
			}

			let entry = entries_by_address
				.get(&(network_path.clone(), local_id))
				.unwrap_or_else(|| panic!("missing entry for node {local_id:?} in network {network_path:?}"));

			assert_eq!(entry.position, expected_position, "position mismatch for node {local_id:?} in network {network_path:?}");
			assert_eq!(entry.is_layer, expected_is_layer, "is_layer mismatch for node {local_id:?} in network {network_path:?}");
			// The trait round-trip drops empty display names (treats them as "unset") so the entry's
			// `display_name` is `None` when the runtime carries `""`. Normalize before comparing.
			let normalized_expected_display = expected_display.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
			assert_eq!(
				entry.display_name, normalized_expected_display,
				"display_name mismatch for node {local_id:?} in network {network_path:?}"
			);
			assert_eq!(entry.locked, expected_locked, "locked mismatch for node {local_id:?} in network {network_path:?}");
			assert_eq!(entry.pinned, expected_pinned, "pinned mismatch for node {local_id:?} in network {network_path:?}");

			if entry.position.is_some() {
				checked_any_position = true;
			}
			if entry.is_layer {
				checked_any_layer = true;
			}
		}

		// Sanity: a real artwork should exercise at least these two shapes; otherwise the test is
		// just iterating empty metadata and proving nothing.
		assert!(checked_any_position, "demo artwork produced no positioned nodes — fixture is wrong or extraction is broken");
		assert!(checked_any_layer, "demo artwork produced no layer nodes — fixture is wrong or extraction is broken");
	}

	/// Full editor-side round-trip: original interface → Registry → (NodeNetwork, Vec<entry>) →
	/// freshly-built interface. Asserts the rebuilt interface presents the same `ui::*` state as
	/// the original when read through `StorageMetadataView`.
	#[test]
	fn editor_interface_rebuild_round_trip() {
		let document = load_demo("changing-seasons.graphite");
		let original = &document.network_interface;
		let original_view = StorageMetadataView::new(original);

		let network = original.document_network().clone();
		let registry = Registry::from_runtime_with_metadata(&network, &original_view).expect("from_runtime_with_metadata failed");
		let (rebuilt_network, entries) = registry.to_runtime_with_metadata().expect("to_runtime_with_metadata failed");

		let rebuilt = build_interface_from_storage(rebuilt_network, entries);
		let rebuilt_view = StorageMetadataView::new(&rebuilt);

		// Every node the original carried must also resolve identically through the rebuilt view.
		// Iterating over the *rebuilt* interface verifies that the rebuild covered every node, not
		// just the ones the entries vec mentioned.
		for (network_path, local_id) in collect_all_node_paths(&rebuilt) {
			assert_eq!(
				rebuilt_view.position(&network_path, local_id),
				original_view.position(&network_path, local_id),
				"position mismatch for node {local_id:?} in network {network_path:?}"
			);
			assert_eq!(
				rebuilt_view.is_layer(&network_path, local_id),
				original_view.is_layer(&network_path, local_id),
				"is_layer mismatch for node {local_id:?} in network {network_path:?}"
			);
			// Original display names are returned by the source as-is (including `""`). After
			// round-trip the rebuilt interface also stores `""` for nodes that had no name set,
			// so this comparison is exact.
			assert_eq!(
				rebuilt_view.display_name(&network_path, local_id),
				original_view.display_name(&network_path, local_id),
				"display_name mismatch for node {local_id:?} in network {network_path:?}"
			);
			assert_eq!(
				rebuilt_view.locked(&network_path, local_id),
				original_view.locked(&network_path, local_id),
				"locked mismatch for node {local_id:?} in network {network_path:?}"
			);
			assert_eq!(
				rebuilt_view.pinned(&network_path, local_id),
				original_view.pinned(&network_path, local_id),
				"pinned mismatch for node {local_id:?} in network {network_path:?}"
			);
		}

		// Symmetric: every node in the original must also exist in the rebuilt interface.
		for (network_path, local_id) in collect_all_node_paths(original) {
			assert!(
				rebuilt.nested_network(&network_path).and_then(|n| n.nodes.get(&local_id)).is_some(),
				"original node {local_id:?} in network {network_path:?} missing after rebuild"
			);
		}
	}
}
