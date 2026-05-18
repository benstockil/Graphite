#![allow(unused)]
use std::{borrow::Cow, collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};

// Public modules for conversions
pub mod delta;
pub mod from_runtime;
pub mod metadata_source;
pub mod to_runtime;

pub use metadata_source::{NoMetadata, NodeMetadataEntry, NodeMetadataSource};

#[cfg(test)]
mod round_trip_tests;

/// Attribute key constants for `Node.attributes`, `Node.inputs_attributes`, and
/// `Registry.attributes`. Glob-import (`use crate::attr::*`) at conversion sites to avoid
/// maintaining an explicit name list that grows with every new attribute.
pub mod attr {
	// Compute-relevant keys round-tripped from runtime `DocumentNode` fields.
	pub const CALL_ARGUMENT: &str = "call_argument";
	pub const CONTEXT_FEATURES: &str = "context_features";
	pub const IMPORT_TYPE: &str = "import_type";
	pub const VISIBLE: &str = "visible";
	pub const SKIP_DEDUPLICATION: &str = "skip_deduplication";
	pub const REFLECTION_METADATA: &str = "reflection_metadata";
	pub const ORIGINAL_NODE_ID: &str = "original_node_id";
	pub const EXPORTED_NODES_TS: &str = "library::exported_nodes_ts";

	// Editor-side metadata keys. Per the CmRDT design, all UI state is key-namespaced under
	// `ui::*` so each value can be edited under its own LWW timestamp.
	pub const UI_POSITION: &str = "ui::position";
	pub const UI_IS_LAYER: &str = "ui::is_layer";
	pub const UI_DISPLAY_NAME: &str = "ui::display_name";
	pub const UI_LOCKED: &str = "ui::locked";
	pub const UI_PINNED: &str = "ui::pinned";
}

/// Storage-side position for a node. The shape unifies what the runtime splits across
/// `NodePosition` (Absolute | Chain) and `LayerPosition` (Absolute | Stack); which variants
/// are valid for a given node is decided by `attr::UI_IS_LAYER`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Position {
	Absolute([i32; 2]),
	Chain,
	Stack(u32),
}

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
	clock: LamportClock,
}

pub type DeclarationId = u64; // content based hash
pub type NodeId = u64;
pub type NetworkId = u64;
type ProtoNodeId = String;
type Rev = u64; // Use merkle tree hash?

/// Identifies one editor session for the purposes of CRDT timestamp tiebreaking.
/// Two peers can mint colliding Lamport counters; the peer ID disambiguates.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct PeerId(pub u64);

/// Lamport-style logical timestamp with a peer-ID tiebreak. Comparison is lexicographic:
/// higher counter wins; equal counters are decided by peer ID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct TimeStamp {
	pub counter: u64,
	pub peer: PeerId,
}

impl TimeStamp {
	/// Pre-edit origin timestamp. Used for the initial `from_runtime` conversion of a legacy
	/// document where no edits have happened yet.
	pub const ORIGIN: Self = TimeStamp { counter: 0, peer: PeerId(0) };
}

/// A Lamport clock owned by a `Document`. `tick()` mints a fresh local timestamp; `observe()`
/// advances the counter past an incoming op's timestamp so future local ticks are causally later.
#[derive(Copy, Clone, Debug, Default)]
pub struct LamportClock {
	counter: u64,
	peer: PeerId,
}

impl LamportClock {
	pub fn new(peer: PeerId) -> Self {
		Self { counter: 0, peer }
	}

	pub fn tick(&mut self) -> TimeStamp {
		self.counter += 1;
		TimeStamp {
			counter: self.counter,
			peer: self.peer,
		}
	}

	pub fn observe(&mut self, incoming: TimeStamp) {
		self.counter = self.counter.max(incoming.counter);
	}
}

/// A type-erased attribute value paired with the timestamp at which it was last set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Value {
	pub value: serde_json::Value,
	pub timestamp: TimeStamp,
}

impl Value {
	pub fn new(value: serde_json::Value, timestamp: TimeStamp) -> Self {
		Self { value, timestamp }
	}
}

pub type Attributes = HashMap<String, Value>;

#[derive(Clone, Debug)]
pub struct Node {
	implementation: Implementation,
	inputs: Vec<InputSlot>,
	inputs_attributes: Vec<Attributes>,
	attributes: Attributes,
	network: NetworkId,
}

/// A positional input on a `Node`. The timestamp drives LWW on concurrent `ChangeNodeInput` ops
/// targeting the same `(node_id, input_idx)`. Mirrors `ExportSlot`.
#[derive(Clone, Debug, PartialEq)]
pub struct InputSlot {
	pub input: NodeInput,
	pub timestamp: TimeStamp,
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
		timestamp: TimeStamp,
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
	/// Insert a network. Parallels `AddNode`; used as the reverse of `RemoveNetwork` and emitted
	/// by the diff path when a new network appears.
	AddNetwork {
		network: NetworkId,
		contents: Network,
	},
	/// Tombstone-free network removal. `snapshot` captures the network's exports at the moment of
	/// deletion so the reverse delta can reconstruct without re-walking history.
	RemoveNetwork {
		network: NetworkId,
		snapshot: Network,
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
	Set { key: String, value: serde_json::Value, timestamp: TimeStamp },
	Remove { key: String, timestamp: TimeStamp },
}

impl AttributeDelta {
	fn key(&self) -> &str {
		match self {
			AttributeDelta::Set { key, .. } => key,
			AttributeDelta::Remove { key, .. } => key,
		}
	}

	fn timestamp(&self) -> TimeStamp {
		match self {
			AttributeDelta::Set { timestamp, .. } => *timestamp,
			AttributeDelta::Remove { timestamp, .. } => *timestamp,
		}
	}
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
				RegistryDelta::AddNetwork { network, .. } => *network == network_id,
				RegistryDelta::RemoveNetwork { network, .. } => *network == network_id,
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
			RegistryDelta::ChangeNodeInput {
				node_id,
				input_idx,
				new_input,
				timestamp,
			} => {
				// If the new input references another node, ad-hoc resurrect it if absent.
				if let NodeInput::Node { node_id: referenced, .. } = &new_input {
					self.ensure_node_exists(*referenced)?;
				}
				self.ensure_node_exists(node_id)?;

				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let slot = node.inputs.get_mut(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;
				// LWW: only apply if this op is newer than the slot's current timestamp.
				if timestamp > slot.timestamp {
					slot.input = new_input;
					slot.timestamp = timestamp;
				}
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
					net.exports.resize(
						slot_idx + 1,
						ExportSlot {
							target: None,
							timestamp: TimeStamp::ORIGIN,
						},
					);
				}

				let existing = &mut net.exports[slot_idx];
				// LWW: only apply if this op is newer than what's already there.
				if timestamp > existing.timestamp {
					existing.target = target;
					existing.timestamp = timestamp;
				}
			}
			RegistryDelta::AddNetwork { network, contents } => {
				if self.registry.networks.contains_key(&network) {
					return Err(CrdtError::NetworkAlreadyExists);
				}
				self.registry.networks.insert(network, contents);
			}
			RegistryDelta::RemoveNetwork { network, .. } => {
				// Physical removal. The snapshot lives on the delta itself so the reverse can rebuild
				// without re-walking history; we don't need it on the forward path.
				self.registry.networks.remove(&network);
			}
			RegistryDelta::SetExportedNodes { nodes, timestamp } => {
				// LWW via a sidecar timestamp stored in the document attributes.
				let current_ts = self.registry.attributes.get(attr::EXPORTED_NODES_TS).map(|v| v.timestamp).unwrap_or(TimeStamp::ORIGIN);
				if timestamp > current_ts {
					self.registry.exported_nodes = nodes;
					self.registry.attributes.insert(
						attr::EXPORTED_NODES_TS.to_string(),
						Value {
							value: serde_json::Value::Null,
							timestamp,
						},
					);
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
				let slot = node.inputs.get(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;

				RegistryDelta::ChangeNodeInput {
					node_id,
					input_idx,
					new_input: slot.input.clone(),
					timestamp: slot.timestamp,
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
				let (target, timestamp) = net.exports.get(slot as usize).map(|s| (s.target.clone(), s.timestamp)).unwrap_or((None, TimeStamp::ORIGIN));

				RegistryDelta::SetExport { network, slot, target, timestamp }
			}
			RegistryDelta::AddNetwork { network, contents } => RegistryDelta::RemoveNetwork {
				network: *network,
				snapshot: contents.clone(),
			},
			&RegistryDelta::RemoveNetwork { network, ref snapshot } => RegistryDelta::AddNetwork { network, contents: snapshot.clone() },
			&RegistryDelta::SetExportedNodes { .. } => {
				let current_ts = self.registry.attributes.get(attr::EXPORTED_NODES_TS).map(|v| v.timestamp).unwrap_or(TimeStamp::ORIGIN);
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
	let op_timestamp = delta.timestamp();
	match current_value {
		None => AttributeDelta::Remove { key, timestamp: op_timestamp },
		Some(previous) => AttributeDelta::Set {
			key,
			value: previous.value.clone(),
			timestamp: previous.timestamp,
		},
	}
}

fn apply_attribute_delta(delta: AttributeDelta, attributes: &mut Attributes) {
	match delta {
		AttributeDelta::Set { key, value, timestamp } => match attributes.entry(key) {
			std::collections::hash_map::Entry::Occupied(mut entry) => {
				if timestamp > entry.get().timestamp {
					entry.insert(Value { value, timestamp });
				}
			}
			std::collections::hash_map::Entry::Vacant(entry) => {
				entry.insert(Value { value, timestamp });
			}
		},
		AttributeDelta::Remove { key, timestamp } => {
			let should_remove = attributes.get(&key).is_none_or(|existing| timestamp > existing.timestamp);
			if should_remove {
				attributes.remove(&key);
			}
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
	NetworkAlreadyExists,
}
