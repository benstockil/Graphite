use std::borrow::Cow;
use std::collections::HashMap;

use core_types::memo::MemoHash;
use core_types::uuid::NodeId as RuntimeNodeId;
use graph_craft::document::value::TaggedValue;
use graph_craft::document::{DocumentNode, DocumentNodeImplementation, NodeInput as GraphCraftNodeInput, NodeNetwork};
use graph_craft::{ProtoNodeIdentifier, Type, concrete};
use rustc_hash::FxHashMap;

use crate::attr::*;
use crate::metadata_source::{InputMetadataEntry, NetworkMetadataEntry, NodeMetadataEntry};
use crate::{AttributesRead, DeclarationId, Implementation, NetworkId, NodeId, NodeInput, Position, ROOT_NETWORK, Registry};

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

/// Graph-only conversion (no editor metadata). Use [`Registry::to_runtime_with_metadata`] or
/// [`Registry::to_runtime_with_full_metadata`] when round-tripping editor state.
impl TryFrom<&Registry> for NodeNetwork {
	type Error = ConversionError;

	fn try_from(registry: &Registry) -> Result<Self, Self::Error> {
		convert_network(registry, ROOT_NETWORK, &[], &mut None, &mut None)
	}
}

impl Registry {
	/// Convert a `Registry` to a `NodeNetwork` plus two flat metadata vecs (per-node and per-network).
	/// Per-node entries cover every node carrying any `ui::*` attribute; per-network entries cover
	/// every network whose attributes carry navigation/previewing state. The editor reassembles its
	/// `NodeNetworkMetadata` from both.
	pub fn to_runtime_with_metadata(&self) -> Result<(NodeNetwork, Vec<NodeMetadataEntry>), ConversionError> {
		let (network, node_entries, _) = self.to_runtime_with_full_metadata()?;
		Ok((network, node_entries))
	}

	/// Same as `to_runtime_with_metadata` but also returns the per-network metadata vec. Used by the
	/// editor's rebuild path; tests and callers that only care about node metadata can use the
	/// short form above.
	pub fn to_runtime_with_full_metadata(&self) -> Result<(NodeNetwork, Vec<NodeMetadataEntry>, Vec<NetworkMetadataEntry>), ConversionError> {
		let mut node_metadata = Some(Vec::new());
		let mut network_metadata = Some(Vec::new());
		let network = convert_network(self, ROOT_NETWORK, &[], &mut node_metadata, &mut network_metadata)?;
		Ok((network, node_metadata.expect("node collector seeded above"), network_metadata.expect("network collector seeded above")))
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
/// `metadata_path` for `convert_network` is the chain of runtime local IDs naming *this* network
/// (the owning-node chain from root, with this network's owning node as the last entry — empty for
/// the root network). `node_collector` and `network_collector`, when present, are populated with
/// `NodeMetadataEntry` / `NetworkMetadataEntry` values for every node / network carrying any
/// `ui::*` attribute.
fn convert_network(
	registry: &Registry,
	network_id: NetworkId,
	metadata_path: &[RuntimeNodeId],
	node_collector: &mut Option<Vec<NodeMetadataEntry>>,
	network_collector: &mut Option<Vec<NetworkMetadataEntry>>,
) -> Result<NodeNetwork, ConversionError> {
	let network = registry.networks.get(&network_id).ok_or(ConversionError::NetworkNotFound(network_id))?;

	// Per-network metadata. Emitted only when the network actually carries any `ui::*` attribute,
	// so legacy documents (no per-network state set) don't get a swarm of empty entries.
	if let Some(collector) = network_collector.as_mut() {
		let entry = extract_network_metadata(&network.attributes, metadata_path);
		if !entry.is_empty() {
			collector.push(entry);
		}
	}

	// Nodes belonging to this network level only. Nested networks are recursively converted
	// when their owning node is materialized via `Implementation::Network`.
	let nodes: FxHashMap<_, DocumentNode> = registry
		.node_instances
		.iter()
		.filter(|(_, node)| node.network == network_id)
		.map(|(&global_id, node)| {
			let local_id = node.attributes.get(ORIGINAL_NODE_ID).and_then(|v| v.value.as_u64()).unwrap_or(global_id);
			let runtime_id = RuntimeNodeId(local_id);

			if let Some(collector) = node_collector.as_mut()
				&& let Some(entry) = extract_ui_metadata(node, metadata_path, runtime_id)
			{
				collector.push(entry);
			}

			convert_node(registry, node, metadata_path, runtime_id, node_collector, network_collector).map(|doc_node| (runtime_id, doc_node))
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
///
/// `input_metadata` is always sized to match `node.inputs.len()` so the editor side can do a
/// strict slot-by-slot rebuild without sparse-index bookkeeping. Empty slots round-trip as
/// `InputMetadataEntry::default()`.
fn extract_ui_metadata(node: &crate::Node, network_path: &[RuntimeNodeId], local_id: RuntimeNodeId) -> Option<NodeMetadataEntry> {
	let position: Option<Position> = node.attributes.get_typed(UI_POSITION);
	let is_layer = node.attributes.get_or(UI_IS_LAYER, false);
	let display_name: Option<String> = node.attributes.get_typed(UI_DISPLAY_NAME);
	let locked = node.attributes.get_or(UI_LOCKED, false);
	let pinned = node.attributes.get_or(UI_PINNED, false);
	let output_names: Vec<String> = node.attributes.get_or_default(UI_OUTPUT_NAMES);

	let input_metadata: Vec<InputMetadataEntry> = node.inputs_attributes.iter().map(extract_input_metadata).collect();

	let entry = NodeMetadataEntry {
		network_path: network_path.to_vec(),
		local_id,
		position,
		is_layer,
		display_name,
		locked,
		pinned,
		input_metadata,
		output_names,
	};
	(!entry.is_empty()).then_some(entry)
}

/// Reads a network's `ui::*` attributes back into a `NetworkMetadataEntry`. The four sub-fields are
/// kept as raw `serde_json::Value` (or `f64` for width) so storage stays free of the editor's
/// `PTZ` / `DAffine2` / `Previewing` types.
fn extract_network_metadata(attributes: &crate::Attributes, network_path: &[RuntimeNodeId]) -> NetworkMetadataEntry {
	NetworkMetadataEntry {
		network_path: network_path.to_vec(),
		navigation_ptz: attributes.get(UI_NAV_PTZ).map(|v| v.value.clone()),
		navigation_transform: attributes.get(UI_NAV_TRANSFORM).map(|v| v.value.clone()),
		navigation_width: attributes.get_typed(UI_NAV_WIDTH),
		previewing: attributes.get(UI_PREVIEWING).map(|v| v.value.clone()),
		reference: attributes.get_typed(UI_REFERENCE),
	}
}

/// Reads one input slot's `ui::*` attributes back into the editor-facing `InputMetadataEntry`.
/// The `input_data` map is reassembled by scanning every attribute key under the
/// `ui::input_data::` prefix; the stripped remainder becomes the runtime sub-key.
fn extract_input_metadata(attributes: &crate::Attributes) -> InputMetadataEntry {
	let input_data: HashMap<String, serde_json::Value> = attributes
		.iter()
		.filter_map(|(key, value)| key.strip_prefix(UI_INPUT_DATA_PREFIX).map(|sub_key| (sub_key.to_owned(), value.value.clone())))
		.collect();

	InputMetadataEntry {
		input_name: attributes.get_typed(UI_INPUT_NAME),
		input_description: attributes.get_typed(UI_INPUT_DESCRIPTION),
		widget_override: attributes.get_typed(UI_WIDGET_OVERRIDE),
		input_data,
	}
}

/// Converts a Registry Node to a DocumentNode, remapping global IDs to local IDs
fn convert_node(
	registry: &Registry,
	node: &crate::Node,
	metadata_path: &[RuntimeNodeId],
	runtime_node_id: RuntimeNodeId,
	node_collector: &mut Option<Vec<NodeMetadataEntry>>,
	network_collector: &mut Option<Vec<NetworkMetadataEntry>>,
) -> Result<DocumentNode, ConversionError> {
	// Convert inputs with their associated attributes, remapping node references
	let inputs = node
		.inputs
		.iter()
		.zip(node.inputs_attributes.iter())
		.map(|(slot, input_attrs)| convert_input(registry, &slot.input, input_attrs))
		.collect::<Result<Vec<_>, _>>()?;

	// Defaults match `DocumentNode::default()` so the write side can omit attributes for the common
	// case. Keep these in sync with the `set_if_not_default` calls in `from_runtime`.
	let call_argument = node.attributes.get_or(CALL_ARGUMENT, concrete!(core_types::Context));
	let context_features = node.attributes.get_or_default(CONTEXT_FEATURES);
	let visible = node.attributes.get_or(VISIBLE, true);
	let skip_deduplication = node.attributes.get_or(SKIP_DEDUPLICATION, false);

	Ok(DocumentNode {
		inputs,
		call_argument,
		implementation: convert_implementation(registry, &node.implementation, metadata_path, runtime_node_id, node_collector, network_collector)?,
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
			let import_type = input_attributes.get_or(IMPORT_TYPE, Type::Generic(Cow::Borrowed("T")));
			GraphCraftNodeInput::Import {
				import_type,
				import_index: *import_idx,
			}
		}
		NodeInput::Reflection => {
			let metadata = input_attributes
				.get_typed(REFLECTION_METADATA)
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
	node_collector: &mut Option<Vec<NodeMetadataEntry>>,
	network_collector: &mut Option<Vec<NetworkMetadataEntry>>,
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

			DocumentNodeImplementation::Network(convert_network(registry, *net_id, &child_path, node_collector, network_collector)?)
		}
	})
}
