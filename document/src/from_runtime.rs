use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use core_types::Context;
use core_types::context::ContextDependencies;
use core_types::uuid::NodeId as RuntimeNodeId;
use graph_craft::concrete;
use graph_craft::document::value::TaggedValue;
use graph_craft::document::{DocumentNode, DocumentNodeImplementation, NodeInput as GraphCraftNodeInput, NodeNetwork};
use xxhash_rust::xxh3::Xxh3;

use crate::attr::*;
use crate::metadata_source::{NoMetadata, NodeMetadataSource};
use crate::{AttributesExt, DeclarationId, ExportSlot, Implementation, InputSlot, Network, NetworkId, Node, NodeId, NodeInput, Position, ProtoNode, ROOT_NETWORK, Registry, TimeStamp};

fn map_serialization_error(key: &str) -> impl FnOnce(serde_json::Error) -> ConversionError + '_ {
	move |e| ConversionError::SerializationError(format!("{key}: {e:?}"))
}

/// Path to a node, used to mint stable global IDs by hashing.
///
/// Root-network entries are empty (`path == []`) and keep their original local ID. Hashing uses
/// xxh3 for cross-run determinism. Used by the initial `from_runtime` conversion; once peer-scoped
/// ID issuance lands, `AddNode` ops mint IDs directly without hashing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodePath {
	path: Vec<(NodeId, NetworkId)>,
	local_id: NodeId,
}

impl NodePath {
	fn root(node_id: NodeId) -> Self {
		Self { path: vec![], local_id: node_id }
	}

	fn nested(parent_path: &NodePath, parent_node_id: NodeId, network_id: NetworkId, local_id: NodeId) -> Self {
		let mut path = parent_path.path.clone();
		path.push((parent_node_id, network_id));
		Self { path, local_id }
	}

	fn to_global_id(&self) -> NodeId {
		if self.path.is_empty() {
			return self.local_id;
		}
		let mut hasher = Xxh3::new();
		self.hash(&mut hasher);
		hasher.finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
	#[error("Failed to serialize value: {0}")]
	SerializationError(String),
	#[error("Unsupported node implementation type")]
	UnsupportedImplementation,
	#[error("Invalid network structure: {0}")]
	InvalidNetwork(String),
}

/// Graph-only conversion (no editor metadata). Use [`Registry::from_runtime_with_metadata`] for
/// editor round-trips.
impl TryFrom<&NodeNetwork> for Registry {
	type Error = ConversionError;

	fn try_from(node_network: &NodeNetwork) -> Result<Self, Self::Error> {
		Registry::from_runtime_with_metadata(node_network, &NoMetadata)
	}
}

impl Registry {
	pub fn from_runtime_with_metadata<M: NodeMetadataSource>(node_network: &NodeNetwork, metadata: &M) -> Result<Self, ConversionError> {
		let mut registry = Registry::default();
		let mut ctx = ConversionContext {
			next_network_id: ROOT_NETWORK + 1,
			next_decl_id: 0,
			proto_node_map: HashMap::new(),
			metadata,
		};

		convert_network(node_network, ROOT_NETWORK, None, &[], &mut registry, &mut ctx)?;

		Ok(registry)
	}
}

struct ConversionContext<'m, M: NodeMetadataSource + ?Sized> {
	next_network_id: NetworkId,
	next_decl_id: DeclarationId,
	proto_node_map: HashMap<String, DeclarationId>,
	metadata: &'m M,
}

fn convert_network<M: NodeMetadataSource + ?Sized>(
	node_network: &NodeNetwork,
	network_id: NetworkId,
	parent_path: Option<&NodePath>,
	metadata_path: &[RuntimeNodeId],
	registry: &mut Registry,
	ctx: &mut ConversionContext<'_, M>,
) -> Result<(), ConversionError> {
	for (runtime_node_id, doc_node) in &node_network.nodes {
		let local_id = runtime_node_id.0;
		let node_path = child_path(parent_path, network_id, local_id);
		let global_id = node_path.to_global_id();

		let mut node = convert_node(doc_node, local_id, network_id, parent_path, metadata_path, *runtime_node_id, registry, ctx)?;
		node.attributes.set(ORIGINAL_NODE_ID, serde_json::json!(local_id), TimeStamp::ORIGIN);
		registry.node_instances.insert(global_id, node);
	}

	let exports = node_network
		.exports
		.iter()
		.map(|export| {
			Ok(ExportSlot {
				target: Some(convert_input(export, parent_path, network_id)?),
				timestamp: TimeStamp::ORIGIN,
			})
		})
		.collect::<Result<Vec<_>, ConversionError>>()?;

	let mut attributes = HashMap::new();
	write_ui_network_attributes(&mut attributes, ctx.metadata, metadata_path, TimeStamp::ORIGIN)?;

	registry.networks.insert(network_id, Network { exports, attributes });

	Ok(())
}

fn child_path(parent_path: Option<&NodePath>, network_id: NetworkId, local_id: NodeId) -> NodePath {
	match parent_path {
		None => NodePath::root(local_id),
		Some(parent) => NodePath::nested(parent, parent.local_id, network_id, local_id),
	}
}

/// `metadata_path` is the chain of runtime IDs from the root down to (but not including) this node.
fn convert_node<M: NodeMetadataSource + ?Sized>(
	doc_node: &DocumentNode,
	local_id: NodeId,
	network_id: NetworkId,
	parent_path: Option<&NodePath>,
	metadata_path: &[RuntimeNodeId],
	runtime_node_id: RuntimeNodeId,
	registry: &mut Registry,
	ctx: &mut ConversionContext<'_, M>,
) -> Result<Node, ConversionError> {
	let node_path = child_path(parent_path, network_id, local_id);
	let timestamp = TimeStamp::ORIGIN;

	let mut inputs = Vec::with_capacity(doc_node.inputs.len());
	let mut inputs_attributes = Vec::with_capacity(doc_node.inputs.len());
	for (input_index, input) in doc_node.inputs.iter().enumerate() {
		inputs.push(InputSlot {
			input: convert_input(input, parent_path, network_id)?,
			timestamp,
		});

		let mut input_attrs = convert_input_attributes(input)?;
		write_ui_input_attributes(&mut input_attrs, ctx.metadata, metadata_path, runtime_node_id, input_index, timestamp)?;
		inputs_attributes.push(input_attrs);
	}

	// For nested networks, append this node onto the metadata path.
	let mut extended_path = Vec::new();
	let child_metadata_path = if matches!(doc_node.implementation, DocumentNodeImplementation::Network(_)) {
		extended_path.extend_from_slice(metadata_path);
		extended_path.push(runtime_node_id);
		extended_path.as_slice()
	} else {
		metadata_path
	};
	let implementation = convert_implementation(&doc_node.implementation, &node_path, child_metadata_path, registry, ctx)?;

	// Defaults match `DocumentNode::default()`; `to_runtime` rehydrates absent keys from the same defaults.
	let mut attributes = HashMap::new();
	attributes
		.set_if_not_default(CALL_ARGUMENT, &doc_node.call_argument, &concrete!(Context), timestamp)
		.map_err(map_serialization_error("call_argument"))?;
	attributes
		.set_if_not_default(CONTEXT_FEATURES, &doc_node.context_features, &ContextDependencies::default(), timestamp)
		.map_err(map_serialization_error("context_features"))?;
	attributes
		.set_if_not_default(VISIBLE, &doc_node.visible, &true, timestamp)
		.map_err(map_serialization_error("visible"))?;
	attributes
		.set_if_not_default(SKIP_DEDUPLICATION, &doc_node.skip_deduplication, &false, timestamp)
		.map_err(map_serialization_error("skip_deduplication"))?;

	write_ui_attributes(&mut attributes, ctx.metadata, metadata_path, runtime_node_id, timestamp)?;

	Ok(Node {
		implementation,
		inputs,
		inputs_attributes,
		attributes,
		network: network_id,
	})
}

fn write_ui_attributes<M: NodeMetadataSource + ?Sized>(
	attributes: &mut crate::Attributes,
	metadata: &M,
	metadata_path: &[RuntimeNodeId],
	runtime_node_id: RuntimeNodeId,
	timestamp: TimeStamp,
) -> Result<(), ConversionError> {
	if let Some(position) = metadata.position(metadata_path, runtime_node_id) {
		attributes.set_serialized(UI_POSITION, &position, timestamp).map_err(map_serialization_error("ui::position"))?;
	}

	// Bool flags are only emitted when true; absence reads as false.
	for (key, value) in [
		(UI_IS_LAYER, metadata.is_layer(metadata_path, runtime_node_id)),
		(UI_LOCKED, metadata.locked(metadata_path, runtime_node_id)),
		(UI_PINNED, metadata.pinned(metadata_path, runtime_node_id)),
	] {
		if value {
			attributes.set(key, serde_json::Value::Bool(true), timestamp);
		}
	}

	if let Some(name) = metadata.display_name(metadata_path, runtime_node_id)
		&& !name.is_empty()
	{
		attributes.set(UI_DISPLAY_NAME, serde_json::Value::String(name.to_string()), timestamp);
	}

	// One whole-vec attribute; per-slot LWW would be overkill for rename-on-output.
	let output_names = metadata.output_names(metadata_path, runtime_node_id);
	if !output_names.is_empty() {
		attributes
			.set_serialized(UI_OUTPUT_NAMES, &output_names, timestamp)
			.map_err(map_serialization_error("ui::output_names"))?;
	}

	Ok(())
}

fn write_ui_network_attributes<M: NodeMetadataSource + ?Sized>(attributes: &mut crate::Attributes, metadata: &M, network_path: &[RuntimeNodeId], timestamp: TimeStamp) -> Result<(), ConversionError> {
	if let Some(value) = metadata.navigation_ptz(network_path) {
		attributes.set(UI_NAV_PTZ, value, timestamp);
	}
	if let Some(value) = metadata.navigation_transform(network_path) {
		attributes.set(UI_NAV_TRANSFORM, value, timestamp);
	}
	if let Some(width) = metadata.navigation_width(network_path) {
		attributes.set_serialized(UI_NAV_WIDTH, &width, timestamp).map_err(map_serialization_error("ui::nav::width"))?;
	}
	if let Some(value) = metadata.previewing(network_path) {
		attributes.set(UI_PREVIEWING, value, timestamp);
	}
	if let Some(reference) = metadata.reference(network_path) {
		attributes.set(UI_REFERENCE, serde_json::Value::String(reference.to_string()), timestamp);
	}

	Ok(())
}

/// Empty strings (the runtime's "unset" sentinel) and absent values are both skipped.
/// `input_data` entries each get their own `ui::input_data::<sub_key>` attribute for per-key LWW.
fn write_ui_input_attributes<M: NodeMetadataSource + ?Sized>(
	attributes: &mut crate::Attributes,
	metadata: &M,
	metadata_path: &[RuntimeNodeId],
	runtime_node_id: RuntimeNodeId,
	input_index: usize,
	timestamp: TimeStamp,
) -> Result<(), ConversionError> {
	let non_empty_string = |key: &'static str, value: Option<&str>, attributes: &mut crate::Attributes| {
		if let Some(value) = value.filter(|s| !s.is_empty()) {
			attributes.set(key, serde_json::Value::String(value.to_string()), timestamp);
		}
	};

	non_empty_string(UI_INPUT_NAME, metadata.input_name(metadata_path, runtime_node_id, input_index), attributes);
	non_empty_string(UI_INPUT_DESCRIPTION, metadata.input_description(metadata_path, runtime_node_id, input_index), attributes);
	if let Some(widget) = metadata.widget_override(metadata_path, runtime_node_id, input_index) {
		attributes.set(UI_WIDGET_OVERRIDE, serde_json::Value::String(widget.to_string()), timestamp);
	}

	for (sub_key, value) in metadata.input_data(metadata_path, runtime_node_id, input_index) {
		attributes.set(&format!("{UI_INPUT_DATA_PREFIX}{sub_key}"), value, timestamp);
	}

	Ok(())
}

fn convert_input(input: &GraphCraftNodeInput, parent_path: Option<&NodePath>, network_id: NetworkId) -> Result<NodeInput, ConversionError> {
	Ok(match input {
		GraphCraftNodeInput::Node { node_id, output_index } => NodeInput::Node {
			node_id: child_path(parent_path, network_id, node_id.0).to_global_id(),
			output_index: *output_index,
		},
		GraphCraftNodeInput::Value { tagged_value, exposed } => {
			let serialized = postcard::to_stdvec(&**tagged_value).map_err(|e| ConversionError::SerializationError(format!("{e:?}")))?;
			NodeInput::Value {
				raw_value: Arc::from(serialized.into_boxed_slice()),
				exposed: *exposed,
			}
		}
		GraphCraftNodeInput::Scope(s) => NodeInput::Scope(s.clone()),
		GraphCraftNodeInput::Import { import_index, .. } => NodeInput::Import { import_idx: *import_index },
		GraphCraftNodeInput::Reflection(_) => NodeInput::Reflection,
		// GPU-specific; not modeled in the Registry format.
		GraphCraftNodeInput::Inline(_) => return Err(ConversionError::UnsupportedImplementation),
	})
}

fn convert_input_attributes(input: &GraphCraftNodeInput) -> Result<crate::Attributes, ConversionError> {
	let mut attributes = HashMap::new();
	let timestamp = TimeStamp::ORIGIN;

	match input {
		GraphCraftNodeInput::Import { import_type, .. } => {
			attributes.set_serialized(IMPORT_TYPE, import_type, timestamp).map_err(map_serialization_error("import_type"))?;
		}
		GraphCraftNodeInput::Reflection(metadata) => {
			attributes
				.set_serialized(REFLECTION_METADATA, metadata, timestamp)
				.map_err(map_serialization_error("reflection_metadata"))?;
		}
		_ => {}
	}

	Ok(attributes)
}

fn convert_implementation<M: NodeMetadataSource + ?Sized>(
	implementation: &DocumentNodeImplementation,
	current_node_path: &NodePath,
	child_metadata_path: &[RuntimeNodeId],
	registry: &mut Registry,
	ctx: &mut ConversionContext<'_, M>,
) -> Result<Implementation, ConversionError> {
	Ok(match implementation {
		DocumentNodeImplementation::ProtoNode(identifier) => {
			let identifier_str = identifier.as_str().to_string();
			let decl_id = ctx.proto_node_map.entry(identifier_str.clone()).or_insert_with(|| {
				let decl_id = ctx.next_decl_id;
				ctx.next_decl_id += 1;
				registry.node_declarations.insert(
					decl_id,
					ProtoNode {
						identifier: identifier_str,
						code: None,
						wasm: None,
						attributes: Default::default(),
					},
				);
				decl_id
			});
			Implementation::ProtoNode(*decl_id)
		}
		DocumentNodeImplementation::Network(nested_network) => {
			let nested_network_id = ctx.next_network_id;
			ctx.next_network_id += 1;
			convert_network(nested_network, nested_network_id, Some(current_node_path), child_metadata_path, registry, ctx)?;
			Implementation::Network(nested_network_id)
		}
		// TODO: Support Extract in the Registry format.
		DocumentNodeImplementation::Extract => return Err(ConversionError::UnsupportedImplementation),
	})
}
