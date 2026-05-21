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
use graph_storage::{InputMetadataEntry, NetworkMetadataEntry, NodeMetadataEntry, NodeMetadataSource, Position};

use super::memo_network::MemoNetwork;
use super::{
	DocumentNodeTransientMetadata, InputMetadata, InputPersistentMetadata, LayerPosition, NodeNetworkInterface, NodeNetworkMetadata, NodePersistentMetadata, NodePosition, NodeTypePersistentMetadata,
	PTZ, Previewing,
};

/// Failure modes for [`build_interface_from_storage`]. Only fires when the storage-side entry list
/// is inconsistent with the converted `NodeNetwork` — e.g. a node's `input_metadata` vec doesn't
/// match its `inputs` vec in length.
#[derive(Debug, thiserror::Error)]
pub enum InterfaceRebuildError {
	#[error("input_metadata length {got} does not match inputs length {expected} for node {node:?} in network {network_path:?}")]
	InputMetadataLengthMismatch { node: NodeId, network_path: Vec<NodeId>, expected: usize, got: usize },
}

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

	fn input_name(&self, network_path: &[NodeId], local_id: NodeId, input_index: usize) -> Option<&str> {
		let input = self.interface.node_metadata(&local_id, network_path)?.persistent_metadata.input_metadata.get(input_index)?;
		Some(input.persistent_metadata.input_name.as_str())
	}

	fn input_description(&self, network_path: &[NodeId], local_id: NodeId, input_index: usize) -> Option<&str> {
		let input = self.interface.node_metadata(&local_id, network_path)?.persistent_metadata.input_metadata.get(input_index)?;
		Some(input.persistent_metadata.input_description.as_str())
	}

	fn widget_override(&self, network_path: &[NodeId], local_id: NodeId, input_index: usize) -> Option<&str> {
		let input = self.interface.node_metadata(&local_id, network_path)?.persistent_metadata.input_metadata.get(input_index)?;
		input.persistent_metadata.widget_override.as_deref()
	}

	fn input_data(&self, network_path: &[NodeId], local_id: NodeId, input_index: usize) -> HashMap<String, serde_json::Value> {
		let Some(metadata) = self.interface.node_metadata(&local_id, network_path) else {
			return HashMap::new();
		};
		let Some(input) = metadata.persistent_metadata.input_metadata.get(input_index) else {
			return HashMap::new();
		};
		input.persistent_metadata.input_data.clone()
	}

	fn output_names(&self, network_path: &[NodeId], local_id: NodeId) -> Vec<String> {
		self.interface
			.node_metadata(&local_id, network_path)
			.map(|metadata| metadata.persistent_metadata.output_names.clone())
			.unwrap_or_default()
	}

	fn navigation_ptz(&self, network_path: &[NodeId]) -> Option<serde_json::Value> {
		let ptz = self.interface.node_graph_ptz(network_path)?;
		serde_json::to_value(ptz).ok()
	}

	fn navigation_transform(&self, network_path: &[NodeId]) -> Option<serde_json::Value> {
		let network_metadata = self.interface.network_metadata.nested_metadata(network_path)?;
		let transform = network_metadata.persistent_metadata.navigation_metadata.node_graph_to_viewport;
		serde_json::to_value(transform).ok()
	}

	fn navigation_width(&self, network_path: &[NodeId]) -> Option<f64> {
		let network_metadata = self.interface.network_metadata.nested_metadata(network_path)?;
		Some(network_metadata.persistent_metadata.navigation_metadata.node_graph_width)
	}

	fn previewing(&self, network_path: &[NodeId]) -> Option<serde_json::Value> {
		let previewing = self.interface.previewing(network_path);
		// Skip the runtime default so legacy documents don't grow a `ui::previewing: No` attribute
		// on every network. Pattern-match rather than equality so we don't need PartialEq on RootNode.
		if matches!(previewing, super::Previewing::No) {
			return None;
		}
		serde_json::to_value(previewing).ok()
	}

	fn reference(&self, network_path: &[NodeId]) -> Option<&str> {
		let network_metadata = self.interface.network_metadata.nested_metadata(network_path)?;
		network_metadata.persistent_metadata.reference.as_deref()
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

/// Build a `NodeNetworkInterface` from a storage-converted `NodeNetwork` plus the per-node and
/// per-network metadata vecs emitted by `Registry::to_runtime_with_full_metadata`. Seeds a default
/// `DocumentNodeMetadata` for every node in every nested network, then patches in the per-node
/// and per-network fields the entries carried.
///
/// Reaches into the private `network` / `network_metadata` fields directly (rather than going
/// through public setters) because the public setters carry transient-cache invalidation logic
/// that's irrelevant when constructing a fresh interface from a self-consistent storage snapshot.
///
/// Errors when a node entry's `input_metadata` length disagrees with the converted node's `inputs`
/// length — a sign of corrupt storage rather than a recoverable condition.
pub fn build_interface_from_storage(network: NodeNetwork, node_entries: Vec<NodeMetadataEntry>, network_entries: Vec<NetworkMetadataEntry>) -> Result<NodeNetworkInterface, InterfaceRebuildError> {
	let mut network_metadata = NodeNetworkMetadata::default();
	seed_metadata_tree(&network, &mut network_metadata);
	apply_entries_into_tree(&network, &mut network_metadata, node_entries)?;
	apply_network_entries_into_tree(&mut network_metadata, network_entries);

	let mut interface = NodeNetworkInterface::default();
	interface.network = MemoNetwork::new(network);
	interface.network_metadata = network_metadata;
	Ok(interface)
}

/// Patches per-network `ui::*` state (navigation, previewing, reference) onto the metadata tree.
/// Entries whose `network_path` doesn't resolve are skipped with a warning — the per-node phase
/// is the strict one; per-network drift is more often a stale path than a corrupt document.
fn apply_network_entries_into_tree(metadata: &mut NodeNetworkMetadata, entries: Vec<NetworkMetadataEntry>) {
	for entry in entries {
		let Some(network_metadata) = metadata.nested_metadata_mut(&entry.network_path) else {
			log::warn!("apply_network_entries_into_tree: nested network at {:?} not found, skipping", entry.network_path);
			continue;
		};

		let persistent = &mut network_metadata.persistent_metadata;

		if let Some(value) = entry.navigation_ptz
			&& let Ok(ptz) = serde_json::from_value::<PTZ>(value)
		{
			persistent.navigation_metadata.node_graph_ptz = ptz;
		}

		if let Some(value) = entry.navigation_transform
			&& let Ok(transform) = serde_json::from_value(value)
		{
			persistent.navigation_metadata.node_graph_to_viewport = transform;
		}

		if let Some(width) = entry.navigation_width {
			persistent.navigation_metadata.node_graph_width = width;
		}

		if let Some(value) = entry.previewing
			&& let Ok(previewing) = serde_json::from_value::<Previewing>(value)
		{
			persistent.previewing = previewing;
		}

		if let Some(reference) = entry.reference {
			persistent.reference = Some(reference);
		}
	}
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
/// caller is responsible for ensuring entries match the network. Length mismatches on
/// `input_metadata` are *not* skipped — they surface as `InterfaceRebuildError`, since they
/// indicate corrupt storage rather than recoverable drift.
fn apply_entries_into_tree(network: &NodeNetwork, metadata: &mut NodeNetworkMetadata, entries: Vec<NodeMetadataEntry>) -> Result<(), InterfaceRebuildError> {
	// Group entries by network_path so we do one nested_metadata_mut lookup per network.
	let mut by_path: HashMap<Vec<NodeId>, Vec<NodeMetadataEntry>> = HashMap::new();
	for entry in entries {
		by_path.entry(entry.network_path.clone()).or_default().push(entry);
	}

	for (path, entries) in by_path {
		let nested_network = nested_network_at(network, &path);

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

			// Strict length check: the converted network is the source of truth for input count;
			// the storage entry must agree slot-by-slot. A mismatch means the storage layer and the
			// converted runtime drifted, which is a bug worth surfacing.
			let expected_inputs = nested_network.and_then(|net| net.nodes.get(&entry.local_id)).map(|doc_node| doc_node.inputs.len());
			if let Some(expected) = expected_inputs
				&& expected != entry.input_metadata.len()
			{
				return Err(InterfaceRebuildError::InputMetadataLengthMismatch {
					node: entry.local_id,
					network_path: path.clone(),
					expected,
					got: entry.input_metadata.len(),
				});
			}

			persistent.input_metadata = entry.input_metadata.into_iter().map(input_metadata_entry_to_runtime).collect();

			if !entry.output_names.is_empty() {
				persistent.output_names = entry.output_names;
			}

			document_node_metadata.transient_metadata = DocumentNodeTransientMetadata::default();
		}
	}

	Ok(())
}

/// Resolves the nested `NodeNetwork` at the given path. Returns `None` when the path is invalid
/// (e.g. references a non-network node) — callers treat that as "skip silently" since the
/// surrounding `nested_metadata_mut` lookup already warns and bails.
fn nested_network_at<'a>(root: &'a NodeNetwork, path: &[NodeId]) -> Option<&'a NodeNetwork> {
	let mut current = root;
	for segment in path {
		let node = current.nodes.get(segment)?;
		let DocumentNodeImplementation::Network(nested) = &node.implementation else { return None };
		current = nested;
	}
	Some(current)
}

/// Maps one storage-side `InputMetadataEntry` back to the runtime's `InputMetadata`. Empty strings
/// are the runtime's "no value set" sentinel for `input_name` / `input_description`, so a `None`
/// on the storage side restores as `""`.
fn input_metadata_entry_to_runtime(entry: InputMetadataEntry) -> InputMetadata {
	InputMetadata {
		persistent_metadata: InputPersistentMetadata {
			input_name: entry.input_name.unwrap_or_default(),
			input_description: entry.input_description.unwrap_or_default(),
			widget_override: entry.widget_override,
			input_data: entry.input_data,
		},
		..Default::default()
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
		let mut checked_any_input_metadata = false;

		for (network_path, local_id) in collect_all_node_paths(interface) {
			let expected_position = source.position(&network_path, local_id);
			let expected_is_layer = source.is_layer(&network_path, local_id);
			let expected_display = source.display_name(&network_path, local_id).map(str::to_owned);
			let expected_locked = source.locked(&network_path, local_id);
			let expected_pinned = source.pinned(&network_path, local_id);
			let expected_output_names = source.output_names(&network_path, local_id);

			// Pull per-input metadata directly off the runtime side. We use the runtime invariant
			// (`input_metadata.len() == inputs.len()`) as the iteration bound rather than calling the
			// trait per index, since the trait would return `None` past the end and we want a strict
			// slot-by-slot comparison.
			let input_count = interface.document_node(&local_id, &network_path).map(|node| node.inputs.len()).unwrap_or(0);
			let any_input_metadata_present = (0..input_count).any(|i| {
				source.input_name(&network_path, local_id, i).is_some_and(|s| !s.is_empty())
					|| source.input_description(&network_path, local_id, i).is_some_and(|s| !s.is_empty())
					|| source.widget_override(&network_path, local_id, i).is_some()
					|| !source.input_data(&network_path, local_id, i).is_empty()
			});

			let any_metadata = expected_position.is_some()
				|| expected_is_layer
				|| expected_display.as_deref().is_some_and(|s| !s.is_empty())
				|| expected_locked
				|| expected_pinned
				|| any_input_metadata_present
				|| !expected_output_names.is_empty();

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
			assert_eq!(entry.output_names, expected_output_names, "output_names mismatch for node {local_id:?} in network {network_path:?}");

			// Per-input metadata: the entry's `input_metadata` vec has the same length as the node's
			// input slot count, and each slot matches the trait's per-field view (with the same
			// "empty string ↔ None" normalization as `display_name`).
			assert_eq!(
				entry.input_metadata.len(),
				input_count,
				"input_metadata length mismatch for node {local_id:?} in network {network_path:?}: entry has {}, node has {input_count}",
				entry.input_metadata.len(),
			);
			for (index, input_entry) in entry.input_metadata.iter().enumerate() {
				let expected_name = source.input_name(&network_path, local_id, index).filter(|s| !s.is_empty()).map(str::to_owned);
				let expected_description = source.input_description(&network_path, local_id, index).filter(|s| !s.is_empty()).map(str::to_owned);
				let expected_widget = source.widget_override(&network_path, local_id, index).map(str::to_owned);
				let expected_data = source.input_data(&network_path, local_id, index);

				assert_eq!(
					input_entry.input_name, expected_name,
					"input_name mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
				assert_eq!(
					input_entry.input_description, expected_description,
					"input_description mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
				assert_eq!(
					input_entry.widget_override, expected_widget,
					"widget_override mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
				assert_eq!(
					input_entry.input_data, expected_data,
					"input_data mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
			}

			if entry.position.is_some() {
				checked_any_position = true;
			}
			if entry.is_layer {
				checked_any_layer = true;
			}
			if any_input_metadata_present {
				checked_any_input_metadata = true;
			}
		}

		// Sanity: a real artwork should exercise at least these two shapes; otherwise the test is
		// just iterating empty metadata and proving nothing.
		assert!(checked_any_position, "demo artwork produced no positioned nodes — fixture is wrong or extraction is broken");
		assert!(checked_any_layer, "demo artwork produced no layer nodes — fixture is wrong or extraction is broken");
		// Demo artworks always have some custom widget overrides / input names; if not, the
		// per-input round-trip isn't actually being exercised here.
		assert!(checked_any_input_metadata, "demo artwork produced no per-input metadata — fixture is wrong or extraction is broken");
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
		let (rebuilt_network, node_entries, network_entries) = registry.to_runtime_with_full_metadata().expect("to_runtime_with_full_metadata failed");

		let rebuilt = build_interface_from_storage(rebuilt_network, node_entries, network_entries).expect("build_interface_from_storage failed");
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

			// Per-input metadata: walk both interfaces' input slots in lockstep.
			let original_inputs = original.document_node(&local_id, &network_path).map(|n| n.inputs.len()).unwrap_or(0);
			let rebuilt_inputs = rebuilt.document_node(&local_id, &network_path).map(|n| n.inputs.len()).unwrap_or(0);
			assert_eq!(
				rebuilt_inputs, original_inputs,
				"input count mismatch for node {local_id:?} in network {network_path:?}: rebuilt={rebuilt_inputs} original={original_inputs}"
			);

			for index in 0..rebuilt_inputs {
				assert_eq!(
					rebuilt_view.input_name(&network_path, local_id, index),
					original_view.input_name(&network_path, local_id, index),
					"input_name mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
				assert_eq!(
					rebuilt_view.input_description(&network_path, local_id, index),
					original_view.input_description(&network_path, local_id, index),
					"input_description mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
				assert_eq!(
					rebuilt_view.widget_override(&network_path, local_id, index),
					original_view.widget_override(&network_path, local_id, index),
					"widget_override mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
				assert_eq!(
					rebuilt_view.input_data(&network_path, local_id, index),
					original_view.input_data(&network_path, local_id, index),
					"input_data mismatch at slot {index} for node {local_id:?} in network {network_path:?}"
				);
			}

			assert_eq!(
				rebuilt_view.output_names(&network_path, local_id),
				original_view.output_names(&network_path, local_id),
				"output_names mismatch for node {local_id:?} in network {network_path:?}"
			);
		}

		// Symmetric: every node in the original must also exist in the rebuilt interface.
		for (network_path, local_id) in collect_all_node_paths(original) {
			assert!(
				rebuilt.nested_network(&network_path).and_then(|n| n.nodes.get(&local_id)).is_some(),
				"original node {local_id:?} in network {network_path:?} missing after rebuild"
			);
		}

		// Per-network metadata: compare every nested network path (including the root). Any path the
		// original carries must resolve to the same navigation / previewing / reference state in the
		// rebuilt interface.
		let mut network_paths_to_check: Vec<Vec<NodeId>> = vec![Vec::new()];
		for (network_path, local_id) in collect_all_node_paths(original) {
			let mut child = network_path.clone();
			child.push(local_id);
			// Only descend into nodes that actually contain a nested network.
			if original.nested_network(&child).is_some() {
				network_paths_to_check.push(child);
			}
		}

		let mut checked_any_navigation = false;
		let mut checked_any_reference = false;
		for path in &network_paths_to_check {
			assert_eq!(rebuilt_view.navigation_ptz(path), original_view.navigation_ptz(path), "navigation_ptz mismatch for network {path:?}");
			assert_eq!(
				rebuilt_view.navigation_transform(path),
				original_view.navigation_transform(path),
				"navigation_transform mismatch for network {path:?}"
			);
			assert_eq!(
				rebuilt_view.navigation_width(path),
				original_view.navigation_width(path),
				"navigation_width mismatch for network {path:?}"
			);
			assert_eq!(rebuilt_view.previewing(path), original_view.previewing(path), "previewing mismatch for network {path:?}");
			assert_eq!(rebuilt_view.reference(path), original_view.reference(path), "reference mismatch for network {path:?}");

			if original_view.navigation_ptz(path).is_some() || original_view.navigation_transform(path).is_some() || original_view.navigation_width(path).is_some() {
				checked_any_navigation = true;
			}
			if original_view.reference(path).is_some() {
				checked_any_reference = true;
			}
		}

		// Sanity: a real artwork should exercise at least navigation state on the root network and
		// some reference on nested-definition nodes; otherwise the per-network round-trip is just
		// iterating empty data and proving nothing.
		assert!(checked_any_navigation, "demo artwork produced no navigation metadata — fixture is wrong or extraction is broken");
		assert!(checked_any_reference, "demo artwork produced no reference metadata — fixture is wrong or extraction is broken");
	}
}
