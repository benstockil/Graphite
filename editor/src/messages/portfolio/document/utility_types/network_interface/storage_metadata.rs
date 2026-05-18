//! Bridge between the editor's `NodeNetworkInterface` and `graph-storage`'s `NodeMetadataSource`
//! trait, plus an integration round-trip test against a real demo `.graphite` document.

use glam::IVec2;
use graph_craft::document::NodeId;
use graph_storage::{NodeMetadataSource, Position};

use super::{LayerPosition, NodeNetworkInterface, NodePersistentMetadata, NodePosition, NodeTypePersistentMetadata};

impl NodeMetadataSource for NodeNetworkInterface {
	fn position(&self, network_path: &[NodeId], local_id: NodeId) -> Option<Position> {
		let metadata = self.node_metadata(&local_id, network_path)?;
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
		self.node_metadata(&local_id, network_path)
			.map(|m| matches!(m.persistent_metadata.node_type_metadata, NodeTypePersistentMetadata::Layer(_)))
			.unwrap_or(false)
	}

	fn display_name(&self, network_path: &[NodeId], local_id: NodeId) -> Option<&str> {
		let metadata = self.node_metadata(&local_id, network_path)?;
		Some(metadata.persistent_metadata.display_name.as_str())
	}

	fn locked(&self, network_path: &[NodeId], local_id: NodeId) -> bool {
		self.node_metadata(&local_id, network_path).map(|m| m.persistent_metadata.locked).unwrap_or(false)
	}

	fn pinned(&self, network_path: &[NodeId], local_id: NodeId) -> bool {
		self.node_metadata(&local_id, network_path).map(|m| m.persistent_metadata.pinned).unwrap_or(false)
	}
}

/// Maps a storage-side `Position` back to the runtime's `(NodeTypePersistentMetadata, IVec2)` shape
/// the editor expects. `is_layer` decides which variant to construct; `Chain` collapses to
/// `IVec2::ZERO` for layers and `Stack(n)` collapses to a node default for non-layers, since those
/// combinations shouldn't arise from a faithful round-trip.
#[allow(dead_code)] // Wired in by the round-trip test below; production reverse-mapping lands when the editor consumes Vec<NodeMetadataEntry> directly.
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

		let network = interface.document_network().clone();

		let registry = Registry::from_runtime_with_metadata(&network, interface).expect("from_runtime_with_metadata failed");

		let (_converted_network, entries) = registry.to_runtime_with_metadata().expect("to_runtime_with_metadata failed");

		// Index emitted entries by their (network_path, local_id) address.
		let entries_by_address: HashMap<(Vec<NodeId>, NodeId), &graph_storage::NodeMetadataEntry> = entries.iter().map(|e| ((e.network_path.clone(), e.local_id), e)).collect();

		let mut checked_any_position = false;
		let mut checked_any_layer = false;

		// `NodeNetworkInterface` already exposes several methods (`is_layer`, `display_name`, ...)
		// whose names collide with the trait. Rust resolves bare calls to the inherent ones, so we
		// go through `&dyn NodeMetadataSource` to invoke the trait impl unambiguously.
		let source: &dyn NodeMetadataSource = interface;

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
			assert_eq!(entry.display_name, normalized_expected_display, "display_name mismatch for node {local_id:?} in network {network_path:?}");
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
}
