#![allow(unused)]
use std::{borrow::Cow, collections::HashMap, sync::Arc};

// Public modules for conversions
pub mod delta;
pub mod from_runtime;
pub mod to_runtime;

#[cfg(test)]
mod round_trip_tests;

// Attribute keys for storing DocumentNode metadata in the Registry format
const ATTR_CALL_ARGUMENT: &str = "call_argument";
const ATTR_CONTEXT_FEATURES: &str = "context_features";
const ATTR_IMPORT_TYPE: &str = "import_type";
const ATTR_VISIBLE: &str = "visible";
const ATTR_SKIP_DEDUPLICATION: &str = "skip_deduplication";
const ATTR_REFLECTION_METADATA: &str = "reflection_metadata";
const ATTR_ORIGINAL_NODE_ID: &str = "original_node_id";
const ATTR_EXPORTED_NODES_TS: &str = "library::exported_nodes_ts";

/// The root network ID by convention. The document's renderable graph lives in `networks[&ROOT_NETWORK]`.
pub const ROOT_NETWORK: NetworkId = 0;

#[derive(Clone, Debug, Default)]
pub struct Registry {
	node_declarations: HashMap<DeclarationId, ProtoNode>,
	pub node_instances: HashMap<NodeId, Node>,
	pub networks: HashMap<NetworkId, Network>,
	/// Public library API surface: nodes an importing document can reference.
	/// A node exposed here may itself be a proto node or a network (via `Implementation::Network`).
	/// Display name, category, docs etc. live as `library::*` attributes on the referenced node.
	pub exported_nodes: Vec<NodeId>,
	/// Document-level attributes (format version, title, library exported-nodes timestamp, ...).
	pub attributes: Attributes,
}
#[derive(Clone, Debug)]
struct Document {
	registry: Registry,
	history: HashMap<Rev, Delta>,
	head: Rev,
}

pub type DeclarationId = u64; // content based hash
pub type NodeId = u64;
pub type NetworkId = u64;
type ProtoNodeId = String;
type TimeStamp = u64;
type Rev = u64; // Use merkle tree hash?
pub type Value = (serde_json::Value, TimeStamp);

pub type Attributes = HashMap<String, Value>;

#[derive(Clone, Debug)]
pub struct Node {
	implementation: Implementation,
	inputs: Vec<NodeInput>,
	inputs_attributes: Vec<Attributes>,
	attributes: Attributes,
	network: NetworkId,
}

struct NodeAttributes {
	name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeInput {
	Node {
		node_id: NodeId,
		output_index: usize,
	},
	Value {
		raw_value: Arc<[u8]>,
		exposed: bool,
	},
	Scope(Cow<'static, str>),
	Import {
		import_idx: usize,
	},
	/// Marker for Reflection input. The actual DocumentNodeMetadata is stored in input_attributes.
	Reflection,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Implementation {
	ProtoNode(DeclarationId),
	Network(NetworkId),
}

#[derive(Clone, Debug)]
pub struct Network {
	pub exports: Vec<ExportSlot>,
}

/// A positional export slot. `target == None` means the slot has been removed (or never existed past this length).
/// The timestamp drives LWW on concurrent `SetExport` ops targeting the same slot.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportSlot {
	pub target: Option<NodeInput>,
	pub timestamp: TimeStamp,
}

#[derive(Clone, Debug)]
struct ProtoNode {
	identifier: ProtoNodeId,
	code: Option<String>,
	wasm: Option<Vec<u8>>,
	attributes: Attributes,
}

#[derive(Clone, Debug)]
struct Delta {
	timestamp: TimeStamp,
	predecessor: Option<Rev>,
	id: Rev,
	delta_type: RegistryDelta,
	reverse: RegistryDelta,
}

#[derive(Clone, Debug)]
pub enum RegistryDelta {
	AddNode {
		node_id: NodeId,
		node: Node,
	},
	RemoveNode {
		node_id: NodeId,
	},
	ChangeNodeInput {
		node_id: NodeId,
		input_idx: usize,
		new_input: NodeInput,
	},
	ChangeNodeAttribute {
		node_id: NodeId,
		delta: AttributeDelta,
	},
	ChangeNodeInputAttribute {
		node_id: NodeId,
		input_idx: usize,
		delta: AttributeDelta,
	},
	/// Set or clear a single export slot on a network. LWW per slot by `timestamp`.
	/// `target == None` removes the slot.
	SetExport {
		network: NetworkId,
		slot: u32,
		target: Option<NodeInput>,
		timestamp: TimeStamp,
	},
	RemoveNetwork {
		network: NetworkId,
	},
	/// Whole-list LWW on the document's public library API surface.
	/// The timestamp is tracked in the document attributes under `library::exported_nodes_ts`.
	SetExportedNodes {
		nodes: Vec<NodeId>,
		timestamp: TimeStamp,
	},
	/// Edit a document-level attribute (format version, title, ...).
	ChangeDocumentAttribute {
		delta: AttributeDelta,
	},
}

#[derive(Clone, Debug)]
pub enum AttributeDelta {
	Set { key: String, value: Value },
	Remove { key: String },
}

impl Document {
	pub fn restore_node_from_history(&mut self, old_node_id: NodeId) -> Result<(), CrdtError> {
		for delta in self.history_iter() {
			if let RegistryDelta::AddNode { node_id, .. } = delta.reverse
				&& old_node_id == node_id
			{
				return self.revert_delta(delta.clone());
			}
		}
		Err(CrdtError::NotFoundInHistory)
	}
	pub fn restore_network_from_history(&mut self, network_id: NetworkId) -> Result<(), CrdtError> {
		for delta in self.history_iter() {
			let reverse_targets_network = match &delta.reverse {
				RegistryDelta::SetExport { network, .. } => *network == network_id,
				RegistryDelta::RemoveNetwork { network } => *network == network_id,
				_ => false,
			};
			if reverse_targets_network {
				return self.revert_delta(delta.clone());
			}
		}
		Err(CrdtError::NotFoundInHistory)
	}
	pub fn revert_delta(&mut self, mut delta: Delta) -> Result<(), CrdtError> {
		std::mem::swap(&mut delta.delta_type, &mut delta.reverse);
		self.apply_delta(delta)
	}

	pub fn apply_delta(&mut self, delta: Delta) -> Result<(), CrdtError> {
		if let Some(pred) = delta.predecessor {
			assert!(self.history.contains_key(&pred));
		}

		match delta.delta_type {
			RegistryDelta::AddNode { node_id, node } => {
				if self.registry.node_instances.contains_key(&node_id) {
					return Err(CrdtError::NodeAlreadyExists);
				}
				self.registry.node_instances.insert(node_id, node);
			}
			RegistryDelta::RemoveNode { node_id } => {
				self.registry.node_instances.remove(&node_id);
			}
			RegistryDelta::ChangeNodeInput { node_id, input_idx, new_input } => {
				if let NodeInput::Node { node_id, output_index } = new_input {
					self.ensure_node_exists(node_id)?;
				}
				// These operations have to be modeled via node add / remove operation to avoid potential conflicts
				assert!(!matches!(new_input, NodeInput::Node { .. }));
				assert!(!matches!(new_input, NodeInput::Scope { .. }));

				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let input = node.inputs.get_mut(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;
				*input = new_input;
			}
			RegistryDelta::ChangeNodeAttribute { node_id, delta } => {
				self.ensure_node_exists(node_id)?;

				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				apply_attribute_delta(delta, &mut node.attributes);
			}
			RegistryDelta::ChangeNodeInputAttribute { node_id, input_idx, delta } => {
				self.ensure_node_exists(node_id)?;
				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let input_attributes = node.inputs_attributes.get_mut(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;
				apply_attribute_delta(delta, input_attributes);
			}
			RegistryDelta::SetExport { network, slot, target, timestamp } => {
				let net = self.registry.networks.get_mut(&network).ok_or(CrdtError::NetworkDoesNotExist)?;
				let slot_idx = slot as usize;

				// Extend with empty slots if needed.
				if slot_idx >= net.exports.len() {
					net.exports.resize(slot_idx + 1, ExportSlot { target: None, timestamp: 0 });
				}

				let existing = &mut net.exports[slot_idx];
				// LWW: only apply if this op is newer than what's already there.
				if timestamp > existing.timestamp {
					existing.target = target;
					existing.timestamp = timestamp;
				}
			}
			RegistryDelta::RemoveNetwork { network } => {
				self.registry.networks.remove(&network);
			}
			RegistryDelta::SetExportedNodes { nodes, timestamp } => {
				// LWW via a sidecar timestamp stored in the document attributes.
				let current_ts = self.registry.attributes.get(ATTR_EXPORTED_NODES_TS).and_then(|(v, _)| v.as_u64()).unwrap_or(0);
				if timestamp > current_ts {
					self.registry.exported_nodes = nodes;
					self.registry.attributes.insert(ATTR_EXPORTED_NODES_TS.to_string(), (serde_json::json!(timestamp), timestamp));
				}
			}
			RegistryDelta::ChangeDocumentAttribute { delta } => {
				apply_attribute_delta(delta, &mut self.registry.attributes);
			}
		}
		Ok(())
	}

	fn ensure_node_exists(&mut self, node_id: u64) -> Result<(), CrdtError> {
		if !self.registry.node_instances.contains_key(&node_id) {
			self.restore_node_from_history(node_id)?;
		}
		Ok(())
	}

	fn compute_reverse_delta(&self, delta: &RegistryDelta) -> Result<RegistryDelta, CrdtError> {
		let reverse_delta = match delta {
			&RegistryDelta::AddNode { node_id, .. } => RegistryDelta::RemoveNode { node_id },
			&RegistryDelta::RemoveNode { node_id } => {
				let node = self.registry.node_instances.get(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?.clone();
				RegistryDelta::AddNode { node_id, node }
			}
			&RegistryDelta::ChangeNodeInput { node_id, input_idx, .. } => {
				let node = self.registry.node_instances.get(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let input = node.inputs.get(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;

				RegistryDelta::ChangeNodeInput {
					node_id,
					input_idx,
					new_input: input.clone(),
				}
			}
			&RegistryDelta::ChangeNodeAttribute { node_id, ref delta } => {
				let node = self.registry.node_instances.get(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;

				RegistryDelta::ChangeNodeAttribute {
					node_id,
					delta: reverse_attribute_delta(delta, &node.attributes),
				}
			}
			&RegistryDelta::ChangeNodeInputAttribute { node_id, input_idx, ref delta } => {
				let node = self.registry.node_instances.get(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let input_attributes = node.inputs_attributes.get(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;
				RegistryDelta::ChangeNodeInputAttribute {
					node_id,
					delta: reverse_attribute_delta(delta, input_attributes),
					input_idx,
				}
			}
			&RegistryDelta::SetExport { network, slot, .. } => {
				let net = self.registry.networks.get(&network).ok_or(CrdtError::NetworkDoesNotExist)?;
				let (target, timestamp) = net.exports.get(slot as usize).map(|s| (s.target.clone(), s.timestamp)).unwrap_or((None, 0));

				RegistryDelta::SetExport { network, slot, target, timestamp }
			}
			&RegistryDelta::RemoveNetwork { network } => match self.registry.networks.get(&network) {
				// We don't have a full "restore network" inverse yet — the best we can do
				// is record that the network existed. A future op can reconstruct exports
				// via per-slot `SetExport`s pulled from history.
				Some(_) => RegistryDelta::RemoveNetwork { network },
				None => RegistryDelta::RemoveNetwork { network },
			},
			&RegistryDelta::SetExportedNodes { .. } => {
				let current_ts = self.registry.attributes.get(ATTR_EXPORTED_NODES_TS).and_then(|(v, _)| v.as_u64()).unwrap_or(0);
				RegistryDelta::SetExportedNodes {
					nodes: self.registry.exported_nodes.clone(),
					timestamp: current_ts,
				}
			}
			RegistryDelta::ChangeDocumentAttribute { delta } => RegistryDelta::ChangeDocumentAttribute {
				delta: reverse_attribute_delta(delta, &self.registry.attributes),
			},
		};
		Ok(reverse_delta)
	}

	fn history_iter(&self) -> HistoryIter<'_> {
		HistoryIter {
			document: self,
			parent_rev: self.head,
		}
	}

	fn find_delta(&mut self, check_fn: impl Fn(&Delta) -> bool) -> Result<&Delta, CrdtError> {
		for delta in self.history_iter() {
			if check_fn(delta) {
				return Ok(delta);
			}
		}
		Err(CrdtError::NotFoundInHistory)
	}
}

fn reverse_attribute_delta(delta: &AttributeDelta, attributes: &Attributes) -> AttributeDelta {
	let current_value = attributes.get(delta.key());
	let key = delta.key().to_string();
	match current_value {
		None => AttributeDelta::Remove { key },
		Some(previous) => AttributeDelta::Set { key, value: previous.clone() },
	}
}

impl AttributeDelta {
	fn key(&self) -> &str {
		match self {
			AttributeDelta::Set { key, .. } => key,
			AttributeDelta::Remove { key } => key,
		}
	}
}

fn apply_attribute_delta(delta: AttributeDelta, attributes: &mut Attributes) {
	match delta {
		AttributeDelta::Set { key, value } => {
			attributes.entry(key).and_modify(|x| {
				if value.1 > x.1 {
					*x = value
				}
			});
		}
		AttributeDelta::Remove { key } => {
			attributes.remove(&key);
		}
	}
}

struct HistoryIter<'a> {
	document: &'a Document,
	parent_rev: Rev,
}

impl<'a> Iterator for HistoryIter<'a> {
	type Item = &'a Delta;

	fn next(&mut self) -> Option<Self::Item> {
		let delta = self.document.history.get(&self.parent_rev)?;
		self.parent_rev = delta.predecessor?;
		Some(delta)
	}
}

enum CrdtError {
	TargetNodeDoesNotExist,
	NetworkDoesNotExist,
	InputIndexOutOfBounds,
	NotFoundInHistory,
	NodeAlreadyExists,
}
