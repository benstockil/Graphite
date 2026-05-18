use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use core_types::uuid::NodeId as RuntimeNodeId;
use graph_craft::document::value::TaggedValue;
use graph_craft::document::{DocumentNode, DocumentNodeImplementation, NodeInput as GraphCraftNodeInput, NodeNetwork};
use xxhash_rust::xxh3::Xxh3;

use crate::attr::*;
use crate::metadata_source::{NoMetadata, NodeMetadataSource};
use crate::{DeclarationId, ExportSlot, Implementation, InputSlot, Network, NetworkId, Node, NodeId, NodeInput, Position, ProtoNode, ROOT_NETWORK, Registry, TimeStamp, Value};

/// Represents a path to a node in the document structure for generating stable IDs
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodePath {
	/// Sequence of (node_id, network_id) pairs from root to this node
	/// For root network nodes, this is empty and we use the original ID
	path: Vec<(NodeId, NetworkId)>,
	/// The local ID of this node within its network
	local_id: NodeId,
}

impl NodePath {
	/// Create a path for a root network node
	fn root(node_id: NodeId) -> Self {
		Self { path: vec![], local_id: node_id }
	}

	/// Create a path for a nested network node
	fn nested(parent_path: &NodePath, parent_node_id: NodeId, network_id: NetworkId, local_id: NodeId) -> Self {
		let mut path = parent_path.path.clone();
		path.push((parent_node_id, network_id));
		Self { path, local_id }
	}

	/// Generate a stable, globally unique ID by hashing this path.
	///
	/// Uses xxh3 (deterministic across runs) rather than `DefaultHasher` (process-randomized), so
	/// IDs are stable on disk. Path-hashed IDs are a one-shot bootstrap for legacy `from_runtime`
	/// conversion; once peer-scoped ID issuance lands, `AddNode` ops mint IDs directly without hashing.
	fn to_global_id(&self) -> NodeId {
		// For root network nodes (empty path), use the original ID.
		if self.path.is_empty() {
			return self.local_id;
		}

		let mut hasher = Xxh3::new();
		self.hash(&mut hasher);
		hasher.finish()
	}
}

/// Errors that can occur during conversion from NodeNetwork to Registry
#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
	#[error("Failed to serialize value: {0}")]
	SerializationError(String),
	#[error("Unsupported node implementation type")]
	UnsupportedImplementation,
	#[error("Invalid network structure: {0}")]
	InvalidNetwork(String),
}

/// Converts a NodeNetwork back to a Registry.
///
/// ## Identity Node Creation
///
/// Since Registry uses identity nodes for network exports, this conversion:
/// 1. Creates identity nodes for each export in the network
/// 2. Adds these identity nodes to the network's nodes
/// 3. Updates the network exports to reference these identity nodes
///
/// ## Nested Structure Flattening
///
/// The conversion flattens nested networks:
/// - Each nested NodeNetwork is assigned a unique NetworkId
/// - All nodes (including from nested networks) are added to the flat registry.node_instances map
/// - Each node records which network it belongs to via the `network` field
///
/// This entry point provides no editor-side metadata. Use [`Registry::from_runtime_with_metadata`]
/// to thread positions, display names, etc.
impl TryFrom<&NodeNetwork> for Registry {
	type Error = ConversionError;

	fn try_from(node_network: &NodeNetwork) -> Result<Self, Self::Error> {
		Registry::from_runtime_with_metadata(node_network, &NoMetadata)
	}
}

impl Registry {
	/// Convert a `NodeNetwork` to a `Registry`, attaching `ui::*` attributes pulled from `metadata`.
	pub fn from_runtime_with_metadata<M: NodeMetadataSource>(node_network: &NodeNetwork, metadata: &M) -> Result<Self, ConversionError> {
		let mut registry = Registry {
			node_declarations: HashMap::new(),
			node_instances: HashMap::new(),
			networks: HashMap::new(),
			exported_nodes: vec![],
			attributes: HashMap::new(),
		};

		let mut ctx = ConversionContext {
			// Track the next available IDs. The root network uses the reserved `ROOT_NETWORK` constant (0).
			next_network_id: ROOT_NETWORK + 1,
			next_decl_id: 0,
			proto_node_map: HashMap::new(),
			metadata,
		};

		// Root network has no parent path on either side.
		convert_network(node_network, ROOT_NETWORK, None, &[], &mut registry, &mut ctx)?;

		Ok(registry)
	}
}

/// Recursion-wide state shared across `convert_*` helpers. Holds the next-ID counters, the
/// proto-node interning map, and the editor metadata source used to populate `ui::*` attributes.
struct ConversionContext<'m, M: NodeMetadataSource + ?Sized> {
	next_network_id: NetworkId,
	next_decl_id: DeclarationId,
	proto_node_map: HashMap<String, DeclarationId>,
	metadata: &'m M,
}

/// Converts a single network and registers it in the registry. Exports become first-class `ExportSlot`s on `Network`.
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

		let node_path = match parent_path {
			None => NodePath::root(local_id),
			Some(parent) => NodePath::nested(parent, parent.local_id, network_id, local_id),
		};
		let global_id = node_path.to_global_id();

		let mut node = convert_node(doc_node, local_id, network_id, parent_path, metadata_path, *runtime_node_id, registry, ctx)?;

		let timestamp = TimeStamp::ORIGIN;
		node.attributes.insert(
			ORIGINAL_NODE_ID.to_string(),
			Value {
				value: serde_json::json!(local_id),
				timestamp,
			},
		);

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

	registry.networks.insert(network_id, Network { exports });

	Ok(())
}

/// Converts a DocumentNode to a Registry Node.
///
/// `runtime_node_id` is the runtime-side local ID used to query `ctx.metadata`; `metadata_path`
/// is the chain of runtime IDs from the root network down to (but not including) this node.
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
	// Construct this node's full path
	let node_path = match parent_path {
		None => NodePath::root(local_id),
		Some(parent) => NodePath::nested(parent, parent.local_id, network_id, local_id),
	};

	// Convert inputs, tracking their attributes. Initial conversion uses the origin timestamp;
	// the CRDT system manages timestamps for subsequent ChangeNodeInput ops.
	let mut inputs = Vec::new();
	let mut inputs_attributes = Vec::new();

	for input in &doc_node.inputs {
		inputs.push(InputSlot {
			input: convert_input(input, parent_path, network_id)?,
			timestamp: TimeStamp::ORIGIN,
		});
		inputs_attributes.push(convert_input_attributes(input)?);
	}

	// For nested networks, extend the metadata path with this node before recursing.
	let mut child_metadata_path = Vec::new();
	let child_metadata_path = if matches!(doc_node.implementation, DocumentNodeImplementation::Network(_)) {
		child_metadata_path.extend_from_slice(metadata_path);
		child_metadata_path.push(runtime_node_id);
		child_metadata_path.as_slice()
	} else {
		metadata_path
	};

	// Convert implementation (pass this node's path for nested networks)
	let implementation = convert_implementation(&doc_node.implementation, &node_path, child_metadata_path, registry, ctx)?;

	// Store DocumentNode metadata in attributes for lossless conversion.
	// Initial conversion uses the origin timestamp; the CRDT system manages timestamps for subsequent deltas.
	let mut attributes = HashMap::new();
	let timestamp = TimeStamp::ORIGIN;

	let serialized_call_arg = serde_json::to_value(&doc_node.call_argument).map_err(|e| ConversionError::SerializationError(format!("call_argument: {:?}", e)))?;
	attributes.insert(
		CALL_ARGUMENT.to_string(),
		Value {
			value: serialized_call_arg,
			timestamp,
		},
	);

	let serialized_context = serde_json::to_value(&doc_node.context_features).map_err(|e| ConversionError::SerializationError(format!("context_features: {:?}", e)))?;
	attributes.insert(CONTEXT_FEATURES.to_string(), Value { value: serialized_context, timestamp });

	let serialized_visible = serde_json::to_value(&doc_node.visible).map_err(|e| ConversionError::SerializationError(format!("visible: {:?}", e)))?;
	attributes.insert(VISIBLE.to_string(), Value { value: serialized_visible, timestamp });

	let serialized_skip_dedup = serde_json::to_value(&doc_node.skip_deduplication).map_err(|e| ConversionError::SerializationError(format!("skip_deduplication: {:?}", e)))?;
	attributes.insert(
		SKIP_DEDUPLICATION.to_string(),
		Value {
			value: serialized_skip_dedup,
			timestamp,
		},
	);

	// Editor metadata. Only emitted when the source supplies a value, so converting a network
	// without editor metadata (e.g., synthetic tests via `NoMetadata`) leaves the `ui::*` keys
	// absent rather than writing defaults that would round-trip as if explicitly set.
	write_ui_attributes(&mut attributes, ctx.metadata, metadata_path, runtime_node_id, timestamp)?;

	Ok(Node {
		implementation,
		inputs,
		inputs_attributes,
		attributes,
		network: network_id,
	})
}

/// Pulls `ui::*` values from the metadata source and inserts them into `attributes`.
fn write_ui_attributes<M: NodeMetadataSource + ?Sized>(
	attributes: &mut crate::Attributes,
	metadata: &M,
	metadata_path: &[RuntimeNodeId],
	runtime_node_id: RuntimeNodeId,
	timestamp: TimeStamp,
) -> Result<(), ConversionError> {
	if let Some(position) = metadata.position(metadata_path, runtime_node_id) {
		let value = serde_json::to_value(position).map_err(|e| ConversionError::SerializationError(format!("ui::position: {:?}", e)))?;
		attributes.insert(UI_POSITION.to_string(), Value { value, timestamp });
	}

	// `is_layer` is the layer-vs-node toggle. Default is `false`, so only emit when true to keep
	// the attribute set lean — the absence of the key is read as `false` on round-trip.
	if metadata.is_layer(metadata_path, runtime_node_id) {
		attributes.insert(
			UI_IS_LAYER.to_string(),
			Value {
				value: serde_json::Value::Bool(true),
				timestamp,
			},
		);
	}

	if let Some(name) = metadata.display_name(metadata_path, runtime_node_id) {
		if !name.is_empty() {
			attributes.insert(
				UI_DISPLAY_NAME.to_string(),
				Value {
					value: serde_json::Value::String(name.to_string()),
					timestamp,
				},
			);
		}
	}

	if metadata.locked(metadata_path, runtime_node_id) {
		attributes.insert(
			UI_LOCKED.to_string(),
			Value {
				value: serde_json::Value::Bool(true),
				timestamp,
			},
		);
	}

	if metadata.pinned(metadata_path, runtime_node_id) {
		attributes.insert(
			UI_PINNED.to_string(),
			Value {
				value: serde_json::Value::Bool(true),
				timestamp,
			},
		);
	}

	Ok(())
}

/// Converts a graph-craft NodeInput to a Registry NodeInput, remapping node IDs to global IDs
fn convert_input(input: &GraphCraftNodeInput, parent_path: Option<&NodePath>, network_id: NetworkId) -> Result<NodeInput, ConversionError> {
	Ok(match input {
		GraphCraftNodeInput::Node { node_id, output_index } => {
			// Remap the local node ID to its global hashed ID
			let local_id = node_id.0;
			let node_path = match parent_path {
				None => NodePath::root(local_id),
				Some(parent) => NodePath::nested(parent, parent.local_id, network_id, local_id),
			};
			let global_id = node_path.to_global_id();

			NodeInput::Node {
				node_id: global_id,
				output_index: *output_index,
			}
		}
		GraphCraftNodeInput::Value { tagged_value, exposed } => {
			// Serialize the TaggedValue using postcard
			let serialized = postcard::to_stdvec(&**tagged_value).map_err(|e| ConversionError::SerializationError(format!("{:?}", e)))?;
			NodeInput::Value {
				raw_value: Arc::from(serialized.into_boxed_slice()),
				exposed: *exposed,
			}
		}
		GraphCraftNodeInput::Scope(s) => NodeInput::Scope(s.clone()),
		GraphCraftNodeInput::Import { import_index, .. } => NodeInput::Import { import_idx: *import_index },
		GraphCraftNodeInput::Reflection(_) => {
			// The DocumentNodeMetadata is stored in input_attributes, this is just a marker
			NodeInput::Reflection
		}
		GraphCraftNodeInput::Inline(_) => {
			// Inline is not supported in the Registry format (GPU-specific)
			return Err(ConversionError::UnsupportedImplementation);
		}
	})
}

/// Extracts input metadata and stores it in attributes for lossless conversion.
/// Initial conversion uses the origin timestamp.
fn convert_input_attributes(input: &GraphCraftNodeInput) -> Result<crate::Attributes, ConversionError> {
	let mut attributes = HashMap::new();
	let timestamp = TimeStamp::ORIGIN;

	if let GraphCraftNodeInput::Import { import_type, .. } = input {
		let serialized_type = serde_json::to_value(import_type).map_err(|e| ConversionError::SerializationError(format!("import_type: {:?}", e)))?;
		attributes.insert(IMPORT_TYPE.to_string(), Value { value: serialized_type, timestamp });
	}

	if let GraphCraftNodeInput::Reflection(metadata) = input {
		let serialized_metadata = serde_json::to_value(metadata).map_err(|e| ConversionError::SerializationError(format!("reflection_metadata: {:?}", e)))?;
		attributes.insert(
			REFLECTION_METADATA.to_string(),
			Value {
				value: serialized_metadata,
				timestamp,
			},
		);
	}

	Ok(attributes)
}

/// Converts a DocumentNodeImplementation to a Registry Implementation
fn convert_implementation<M: NodeMetadataSource + ?Sized>(
	implementation: &DocumentNodeImplementation,
	current_node_path: &NodePath,
	child_metadata_path: &[RuntimeNodeId],
	registry: &mut Registry,
	ctx: &mut ConversionContext<'_, M>,
) -> Result<Implementation, ConversionError> {
	Ok(match implementation {
		DocumentNodeImplementation::ProtoNode(identifier) => {
			// Get or create a declaration for this proto node
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
			// Recursively convert the nested network
			// The current node becomes the parent for nodes in the nested network
			let nested_network_id = ctx.next_network_id;
			ctx.next_network_id += 1;

			convert_network(nested_network, nested_network_id, Some(current_node_path), child_metadata_path, registry, ctx)?;

			Implementation::Network(nested_network_id)
		}
		DocumentNodeImplementation::Extract => {
			// Extract nodes are not supported in the Registry format yet
			return Err(ConversionError::UnsupportedImplementation);
		}
	})
}
