use std::borrow::Cow;
use std::collections::HashMap;

use core_types::memo::MemoHash;
use core_types::uuid::NodeId as RuntimeNodeId;
use graph_craft::document::value::TaggedValue;
use graph_craft::document::{DocumentNode, DocumentNodeImplementation, NodeInput as GraphCraftNodeInput, NodeNetwork};
use graph_craft::{ProtoNodeIdentifier, Type, concrete};
use rustc_hash::FxHashMap;

use crate::attr::*;
use crate::metadata_source::NodeMetadataEntry;
use crate::{DeclarationId, Implementation, NetworkId, NodeId, NodeInput, Position, ROOT_NETWORK, Registry};

/// Errors that can occur during conversion from Registry to NodeNetwork
#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
	#[error("Network {0} not found")]
	NetworkNotFound(NetworkId),
	#[error("Node {0} not found")]
	NodeNotFound(NodeId),
	#[error("ProtoNode declaration {0} not found")]
	DeclarationNotFound(DeclarationId),
	#[error("Deserialization error: {0}")]
	DeserializationError(String),
}

/// Converts a Registry to a NodeNetwork.
///
/// ## Identity Node Pattern
///
/// The Registry uses dummy identity nodes for network exports to reuse CRDT logic.
/// `Network.exports: Vec<NodeId>` contains IDs of identity nodes (not the actual nodes to export).
/// These identity nodes have a single input pointing to the actual node to export.
///
/// During conversion:
/// 1. Identity nodes are resolved by following their first input
/// 2. Identity nodes are excluded from the converted network's nodes map
/// 3. The resolved inputs become the network's exports
///
/// ## Nested Structure Preservation
///
/// The conversion maintains full nesting:
/// - Each `NodeNetwork.nodes` contains only the nodes at that specific network level
/// - Nested networks are recursively converted and embedded in `DocumentNodeImplementation::Network`
/// - Registry uses indirection (NetworkId references), NodeNetwork uses direct embedding
impl TryFrom<&Registry> for NodeNetwork {
	type Error = ConversionError;

	fn try_from(registry: &Registry) -> Result<Self, Self::Error> {
		convert_network(registry, ROOT_NETWORK, &[], &mut None)
	}
}

impl Registry {
	/// Convert a `Registry` to a `NodeNetwork` and a flat list of editor-side `ui::*` metadata
	/// entries (one per node that carried any `ui::*` attribute). The editor reassembles its
	/// `NodeNetworkMetadata` from the returned vec.
	pub fn to_runtime_with_metadata(&self) -> Result<(NodeNetwork, Vec<NodeMetadataEntry>), ConversionError> {
		let mut metadata = Some(Vec::new());
		let network = convert_network(self, ROOT_NETWORK, &[], &mut metadata)?;
		Ok((network, metadata.expect("collector seeded above")))
	}
}

/// Converts a specific network by ID, recursively converting any nested networks.
///
/// ## ID Remapping
///
/// The Registry uses globally unique hashed IDs, but each NodeNetwork needs local IDs (0, 1, 2...).
/// We extract the original local IDs from attr::ORIGINAL_NODE_ID on-demand when converting nodes
/// and their references. Since references only point to nodes in the same network, we can
/// deterministically look up the original ID without building an upfront mapping.
///
/// ## Exports
///
/// `Network.exports` is a sparse `Vec<ExportSlot>` where each slot may be `None` (removed) or
/// `Some(NodeInput)`. The runtime expects a dense `Vec<NodeInput>`, so we compact `None` slots
/// during conversion. The slot index stability that matters for CRDT convergence is a storage
/// concern only.
///
/// `metadata_path` is the chain of runtime local IDs from the root network down to (but not
/// including) this network's owning node. `metadata_collector`, when present, is populated with
/// `NodeMetadataEntry` values for every node carrying `ui::*` attributes.
fn convert_network(registry: &Registry, network_id: NetworkId, metadata_path: &[RuntimeNodeId], metadata_collector: &mut Option<Vec<NodeMetadataEntry>>) -> Result<NodeNetwork, ConversionError> {
	let network = registry.networks.get(&network_id).ok_or(ConversionError::NetworkNotFound(network_id))?;

	// Nodes belonging to this network level only. Nested networks are recursively converted
	// when their owning node is materialized via `Implementation::Network`.
	let nodes: FxHashMap<_, DocumentNode> = registry
		.node_instances
		.iter()
		.filter(|(_, node)| node.network == network_id)
		.map(|(&global_id, node)| {
			let local_id = node.attributes.get(ORIGINAL_NODE_ID).and_then(|v| v.value.as_u64()).unwrap_or(global_id);
			let runtime_id = RuntimeNodeId(local_id);

			if let Some(collector) = metadata_collector.as_mut()
				&& let Some(entry) = extract_ui_metadata(node, metadata_path, runtime_id)
			{
				collector.push(entry);
			}

			convert_node(registry, node, metadata_path, runtime_id, metadata_collector).map(|doc_node| (runtime_id, doc_node))
		})
		.collect::<Result<FxHashMap<_, _>, _>>()?;

	// Compact-on-conversion: drop `None` slots, materialize `Some` targets into the runtime's dense vec.
	// Input attributes are not currently round-tripped for exports — they're only meaningful for `Reflection`
	// and `Import` inputs which don't appear as export targets in practice.
	let empty_attrs = HashMap::new();
	let exports: Vec<GraphCraftNodeInput> = network
		.exports
		.iter()
		.filter_map(|slot| slot.target.as_ref())
		.map(|input| convert_input(registry, input, &empty_attrs))
		.collect::<Result<Vec<_>, _>>()?;

	Ok(NodeNetwork {
		exports,
		nodes,
		// TODO: Support scope injections
		scope_injections: FxHashMap::default(),
		generated: false,
	})
}

/// Pulls a node's `ui::*` attribute values into a `NodeMetadataEntry`. Returns `None` when the
/// node carries no editor metadata at all (so the editor doesn't end up with empty entries for
/// every node in legacy documents).
fn extract_ui_metadata(node: &crate::Node, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> Option<NodeMetadataEntry> {
	let position = node.attributes.get(UI_POSITION).and_then(|v| serde_json::from_value::<Position>(v.value.clone()).ok());
	let is_layer = node.attributes.get(UI_IS_LAYER).and_then(|v| v.value.as_bool()).unwrap_or(false);
	let display_name = node.attributes.get(UI_DISPLAY_NAME).and_then(|v| v.value.as_str()).map(str::to_owned);
	let locked = node.attributes.get(UI_LOCKED).and_then(|v| v.value.as_bool()).unwrap_or(false);
	let pinned = node.attributes.get(UI_PINNED).and_then(|v| v.value.as_bool()).unwrap_or(false);

	let any_set = position.is_some() || is_layer || display_name.is_some() || locked || pinned;
	any_set.then(|| NodeMetadataEntry {
		network_path: network_path.to_vec(),
		local_id,
		position,
		is_layer,
		display_name,
		locked,
		pinned,
	})
}

/// Converts a Registry Node to a DocumentNode, remapping global IDs to local IDs
fn convert_node(
	registry: &Registry,
	node: &crate::Node,
	metadata_path: &[RuntimeNodeId],
	runtime_node_id: RuntimeNodeId,
	metadata_collector: &mut Option<Vec<NodeMetadataEntry>>,
) -> Result<DocumentNode, ConversionError> {
	// Convert inputs with their associated attributes, remapping node references
	let inputs = node
		.inputs
		.iter()
		.zip(node.inputs_attributes.iter())
		.map(|(slot, input_attrs)| convert_input(registry, &slot.input, input_attrs))
		.collect::<Result<Vec<_>, _>>()?;

	// Extract call_argument from attributes
	let call_argument = node
		.attributes
		.get(CALL_ARGUMENT)
		.and_then(|v| serde_json::from_value(v.value.clone()).ok())
		.unwrap_or_else(|| concrete!(())); // Default to unit type if not found

	// Extract context_features from attributes
	let context_features = node.attributes.get(CONTEXT_FEATURES).and_then(|v| serde_json::from_value(v.value.clone()).ok()).unwrap_or_default(); // Default to empty context features if not found

	// Extract visible from attributes
	let visible = node.attributes.get(VISIBLE).and_then(|v| serde_json::from_value(v.value.clone()).ok()).unwrap_or(true); // Default to true if not found

	// Extract skip_deduplication from attributes
	let skip_deduplication = node.attributes.get(SKIP_DEDUPLICATION).and_then(|v| serde_json::from_value(v.value.clone()).ok()).unwrap_or(false); // Default to false if not found

	Ok(DocumentNode {
		inputs,
		call_argument,
		implementation: convert_implementation(registry, &node.implementation, metadata_path, runtime_node_id, metadata_collector)?,
		visible,
		skip_deduplication,
		context_features,
		// OriginalLocation is generated during compilation, not stored
		original_location: Default::default(),
	})
}

/// Converts a Registry NodeInput to a graph-craft NodeInput, remapping global IDs to local IDs
fn convert_input(registry: &Registry, input: &NodeInput, input_attributes: &crate::Attributes) -> Result<GraphCraftNodeInput, ConversionError> {
	Ok(match input {
		NodeInput::Node { node_id, output_index } => {
			// Look up the referenced node and extract its original local ID
			let referenced_node = registry.node_instances.get(node_id).ok_or(ConversionError::NodeNotFound(*node_id))?;
			let local_id = referenced_node.attributes.get(ORIGINAL_NODE_ID).and_then(|v| v.value.as_u64()).unwrap_or(*node_id); // Fallback to global ID if not found

			GraphCraftNodeInput::Node {
				node_id: RuntimeNodeId(local_id),
				output_index: *output_index,
			}
		}
		NodeInput::Value { raw_value, exposed } => {
			// Deserialize using postcard - Arc<[u8]> derefs to &[u8]
			let tagged_value: TaggedValue = postcard::from_bytes(raw_value).map_err(|e| ConversionError::DeserializationError(format!("TaggedValue: {:?}", e)))?;
			GraphCraftNodeInput::Value {
				tagged_value: MemoHash::new(tagged_value),
				exposed: *exposed,
			}
		}
		NodeInput::Scope(s) => GraphCraftNodeInput::Scope(s.clone()),
		NodeInput::Import { import_idx } => {
			// Extract import_type from input_attributes if available
			let import_type = input_attributes
				.get(IMPORT_TYPE)
				.and_then(|v| serde_json::from_value(v.value.clone()).ok())
				.unwrap_or_else(|| Type::Generic(Cow::Borrowed("T"))); // Default to generic if not found

			GraphCraftNodeInput::Import {
				import_type,
				import_index: *import_idx,
			}
		}
		NodeInput::Reflection => {
			// Extract reflection_metadata from input_attributes
			let metadata = input_attributes
				.get(REFLECTION_METADATA)
				.and_then(|v| serde_json::from_value(v.value.clone()).ok())
				.ok_or_else(|| ConversionError::DeserializationError("Missing reflection_metadata in input_attributes".to_string()))?;

			GraphCraftNodeInput::Reflection(metadata)
		}
	})
}

/// Converts a Registry Implementation to a DocumentNodeImplementation. `parent_metadata_path` is
/// the chain leading to the *owning* node (i.e., this node); when we descend into a sub-network
/// we push the owning node's runtime ID onto that chain.
fn convert_implementation(
	registry: &Registry,
	implementation: &Implementation,
	parent_metadata_path: &[RuntimeNodeId],
	owning_runtime_id: RuntimeNodeId,
	metadata_collector: &mut Option<Vec<NodeMetadataEntry>>,
) -> Result<DocumentNodeImplementation, ConversionError> {
	Ok(match implementation {
		Implementation::ProtoNode(decl_id) => {
			// Simple case: just convert the identifier
			let proto = registry.node_declarations.get(decl_id).ok_or(ConversionError::DeclarationNotFound(*decl_id))?;
			DocumentNodeImplementation::ProtoNode(ProtoNodeIdentifier::with_owned_string(proto.identifier.clone()))
		}
		Implementation::Network(net_id) => {
			// Recursive case: convert the referenced network to a full NodeNetwork.
			// This will create a nested NodeNetwork with its own nodes map
			// containing only the nodes where node.network == net_id.
			let mut child_path = Vec::with_capacity(parent_metadata_path.len() + 1);
			child_path.extend_from_slice(parent_metadata_path);
			child_path.push(owning_runtime_id);

			DocumentNodeImplementation::Network(convert_network(registry, *net_id, &child_path, metadata_collector)?)
		}
	})
}
