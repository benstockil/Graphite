#![expect(unused, reason = "WIP: the Document API surface is still being wired in")]
use std::{borrow::Cow, collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};

pub mod delta;
pub mod from_runtime;
pub mod metadata_source;
pub mod to_runtime;

pub use metadata_source::{InputMetadataEntry, NetworkMetadataEntry, NoMetadata, NodeMetadataEntry, NodeMetadataSource};

#[cfg(test)]
mod round_trip_tests;

/// Attribute keys. Glob-import (`use crate::attr::*`) at conversion sites.
///
/// `ui::*` keys are namespaced per CRDT design so each value gets its own LWW timestamp. Per-input
/// keys live on `Node.inputs_attributes[i]`; per-network keys live on `Network.attributes`.
pub mod attr {
	pub const CALL_ARGUMENT: &str = "call_argument";
	pub const CONTEXT_FEATURES: &str = "context_features";
	pub const IMPORT_TYPE: &str = "import_type";
	pub const VISIBLE: &str = "visible";
	pub const SKIP_DEDUPLICATION: &str = "skip_deduplication";
	pub const REFLECTION_METADATA: &str = "reflection_metadata";
	pub const ORIGINAL_NODE_ID: &str = "original_node_id";
	pub const EXPORTED_NODES_TS: &str = "library::exported_nodes_ts";

	pub const UI_POSITION: &str = "ui::position";
	pub const UI_IS_LAYER: &str = "ui::is_layer";
	pub const UI_DISPLAY_NAME: &str = "ui::display_name";
	pub const UI_LOCKED: &str = "ui::locked";
	pub const UI_PINNED: &str = "ui::pinned";

	pub const UI_INPUT_NAME: &str = "ui::input_name";
	pub const UI_INPUT_DESCRIPTION: &str = "ui::input_description";
	pub const UI_WIDGET_OVERRIDE: &str = "ui::widget_override";
	/// Prefix for `InputPersistentMetadata::input_data` entries. Full key: `ui::input_data::<sub_key>`.
	pub const UI_INPUT_DATA_PREFIX: &str = "ui::input_data::";

	pub const UI_OUTPUT_NAMES: &str = "ui::output_names";
	/// Lives on the *owning* node (the one with `Implementation::Network`), not on the nested network.
	pub const UI_REFERENCE: &str = "ui::reference";

	pub const UI_NAV_PTZ: &str = "ui::nav::ptz";
	pub const UI_NAV_TRANSFORM: &str = "ui::nav::transform";
	pub const UI_NAV_WIDTH: &str = "ui::nav::width";
	pub const UI_PREVIEWING: &str = "ui::previewing";
}

/// Unified storage-side position. The valid variants depend on `attr::UI_IS_LAYER`:
/// layers use `Absolute` or `Stack`; non-layer nodes use `Absolute` or `Chain`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Position {
	Absolute([i32; 2]),
	Chain,
	Stack(u32),
}

/// Root network ID. The renderable graph lives in `networks[&ROOT_NETWORK]`.
pub const ROOT_NETWORK: NetworkId = 0;

#[derive(Clone, Debug, Default)]
pub struct Registry {
	node_declarations: HashMap<DeclarationId, ProtoNode>,
	pub node_instances: HashMap<NodeId, Node>,
	pub networks: HashMap<NetworkId, Network>,
	/// Public library API: nodes an importing document can reference.
	/// `library::*` attributes on each referenced node carry its display name, category, docs.
	pub exported_nodes: Vec<NodeId>,
	pub attributes: Attributes,
}
#[derive(Clone, Debug)]
struct Document {
	registry: Registry,
	history: HashMap<Rev, Delta>,
	head: Rev,
	clock: LamportClock,
}

pub type DeclarationId = u64; // Content-based hash
pub type NodeId = u64;
pub type NetworkId = u64;
type ProtoNodeId = String;
/// Content-addressed identity for a `Delta`.
/// 128-bit blake3 truncation: comfortable collision headroom for any plausible document lifetime
/// without being adversarial-grade. Same delta content always produces the same `Rev`.
pub type Rev = u128;

/// One editor session, used as a CRDT tiebreaker when two peers mint colliding Lamport counters.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct PeerId(pub u64);

/// Lamport timestamp with a peer-ID tiebreak. Higher counter wins; ties broken by peer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct TimeStamp {
	pub counter: u64,
	pub peer: PeerId,
}

impl TimeStamp {
	/// Pre-edit origin. Used by initial `from_runtime` conversion before any edits have happened.
	pub const ORIGIN: Self = TimeStamp { counter: 0, peer: PeerId(0) };
}

#[derive(Copy, Clone, Debug, Default)]
pub struct LamportClock {
	counter: u64,
	peer: PeerId,
}

impl LamportClock {
	pub fn new(peer: PeerId) -> Self {
		Self { counter: 0, peer }
	}

	/// Mints a fresh local timestamp.
	pub fn tick(&mut self) -> TimeStamp {
		self.counter += 1;
		TimeStamp {
			counter: self.counter,
			peer: self.peer,
		}
	}

	/// Advances past an incoming op so future local ticks are causally later.
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

/// Write helpers for `Attributes`.
pub trait AttributesExt {
	/// Inserts a JSON value under `key`.
	fn set(&mut self, key: &str, value: serde_json::Value, timestamp: TimeStamp);

	/// Serializes `value` and inserts it under `key`.
	fn set_serialized<T: serde::Serialize>(&mut self, key: &str, value: &T, timestamp: TimeStamp) -> Result<(), serde_json::Error>;

	/// Inserts only when `value != default`, so the read side falls back to the same default.
	fn set_if_not_default<T: serde::Serialize + PartialEq>(&mut self, key: &str, value: &T, default: &T, timestamp: TimeStamp) -> Result<(), serde_json::Error>;
}

impl AttributesExt for Attributes {
	fn set(&mut self, key: &str, value: serde_json::Value, timestamp: TimeStamp) {
		self.insert(key.to_string(), Value { value, timestamp });
	}

	fn set_serialized<T: serde::Serialize>(&mut self, key: &str, value: &T, timestamp: TimeStamp) -> Result<(), serde_json::Error> {
		self.set(key, serde_json::to_value(value)?, timestamp);
		Ok(())
	}

	fn set_if_not_default<T: serde::Serialize + PartialEq>(&mut self, key: &str, value: &T, default: &T, timestamp: TimeStamp) -> Result<(), serde_json::Error> {
		if value != default {
			self.set_serialized(key, value, timestamp)?;
		}
		Ok(())
	}
}

/// Typed read helpers for `Attributes`.
pub trait AttributesRead {
	/// Deserializes the value under `key`, or `None` if missing or undecodable.
	fn get_typed<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T>;

	/// Same as `get_typed`, falling back to `default`.
	fn get_or<T: serde::de::DeserializeOwned>(&self, key: &str, default: T) -> T {
		self.get_typed(key).unwrap_or(default)
	}

	/// Same as `get_typed`, falling back to `T::default()`.
	fn get_or_default<T: serde::de::DeserializeOwned + Default>(&self, key: &str) -> T {
		self.get_typed(key).unwrap_or_default()
	}
}

impl AttributesRead for Attributes {
	fn get_typed<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
		self.get(key).and_then(|v| serde_json::from_value(v.value.clone()).ok())
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
	implementation: Implementation,
	inputs: Vec<InputSlot>,
	inputs_attributes: Vec<Attributes>,
	attributes: Attributes,
	network: NetworkId,
}

/// One positional input. The timestamp drives LWW on concurrent `ChangeNodeInput` ops targeting
/// the same `(node_id, input_idx)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputSlot {
	pub input: NodeInput,
	pub timestamp: TimeStamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
	/// Marker; the `DocumentNodeMetadata` lives in `inputs_attributes`.
	Reflection,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Implementation {
	ProtoNode(DeclarationId),
	Network(NetworkId),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Network {
	pub exports: Vec<ExportSlot>,
	/// Per-network `ui::*` state (navigation, previewing). Separate from `Node.attributes` so
	/// view-state edits LWW independently.
	pub attributes: Attributes,
}

/// One positional export slot. `target == None` marks an empty/removed slot. Timestamp drives LWW
/// on concurrent `SetExport` ops.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportSlot {
	pub target: Option<NodeInput>,
	pub timestamp: TimeStamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProtoNode {
	identifier: ProtoNodeId,
	code: Option<String>,
	wasm: Option<Vec<u8>>,
	attributes: Attributes,
}

/// Content-addressed delta: `id` is `blake3_128(parents, author, timestamp, delta_type)`.
///
/// `reverse` is state-dependent undo bookkeeping (it captures pre-state at the moment the forward
/// op was applied), so it's serialized for storage but excluded from the identity hash — two peers
/// observing the same forward delta against different local states would otherwise compute
/// different Revs for the same logical op.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Delta {
	id: Rev,
	parents: Vec<Rev>,
	author: PeerId,
	timestamp: TimeStamp,
	delta_type: RegistryDelta,
	reverse: RegistryDelta,
}

impl Delta {
	fn new(parents: Vec<Rev>, author: PeerId, timestamp: TimeStamp, delta_type: RegistryDelta, reverse: RegistryDelta) -> Self {
		let id = compute_rev(&parents, author, timestamp, &delta_type);
		Self {
			id,
			parents,
			author,
			timestamp,
			delta_type,
			reverse,
		}
	}
}

/// Hash the identity-bearing fields of a `Delta` with blake3 and truncate to 128 bits.
fn compute_rev(parents: &[Rev], author: PeerId, timestamp: TimeStamp, delta_type: &RegistryDelta) -> Rev {
	let mut hasher = blake3::Hasher::new();
	let bytes = postcard::to_stdvec(&(parents, author, timestamp, delta_type)).expect("Delta identity fields must serialize");
	hasher.update(&bytes);
	let digest = hasher.finalize();
	let mut truncated = [0u8; 16];
	truncated.copy_from_slice(&digest.as_bytes()[..16]);
	Rev::from_le_bytes(truncated)
}

/// Op payload. Timestamps live on the wrapping `Delta` — one per delta, applied to all LWW-eligible
/// writes within. See `notes/document-format-collaboration.md`.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
	/// LWW per slot. `target == None` removes the slot.
	SetExport {
		network: NetworkId,
		slot: u32,
		target: Option<NodeInput>,
	},
	AddNetwork {
		network: NetworkId,
		contents: Network,
	},
	/// `snapshot` lets the reverse delta rebuild without re-walking history.
	RemoveNetwork {
		network: NetworkId,
		snapshot: Network,
	},
	/// Whole-list LWW; timestamp lives under `attr::EXPORTED_NODES_TS` on the document.
	SetExportedNodes {
		nodes: Vec<NodeId>,
	},
	ChangeDocumentAttribute {
		delta: AttributeDelta,
	},
}

/// `value: None` means remove. The timestamp comes from the wrapping `Delta`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributeDelta {
	pub key: String,
	pub value: Option<serde_json::Value>,
}

impl Document {
	pub fn restore_node_from_history(&mut self, old_node_id: NodeId) -> Result<(), CrdtError> {
		let delta = self
			.history_iter()
			.find(|d| matches!(d.reverse, RegistryDelta::AddNode { node_id, .. } if node_id == old_node_id))
			.ok_or(CrdtError::NotFoundInHistory)?
			.clone();
		self.revert_delta(delta)
	}

	pub fn restore_network_from_history(&mut self, network_id: NetworkId) -> Result<(), CrdtError> {
		let delta = self
			.history_iter()
			.find(|d| {
				matches!(&d.reverse,
					RegistryDelta::SetExport { network, .. }
					| RegistryDelta::AddNetwork { network, .. }
					| RegistryDelta::RemoveNetwork { network, .. } if *network == network_id)
			})
			.ok_or(CrdtError::NotFoundInHistory)?
			.clone();
		self.revert_delta(delta)
	}

	pub fn revert_delta(&mut self, mut delta: Delta) -> Result<(), CrdtError> {
		std::mem::swap(&mut delta.delta_type, &mut delta.reverse);
		self.apply_delta(delta)
	}

	pub fn apply_delta(&mut self, delta: Delta) -> Result<(), CrdtError> {
		for parent in &delta.parents {
			assert!(self.history.contains_key(parent));
		}

		let timestamp = delta.timestamp;
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
				// Ad-hoc resurrect a referenced node if it was removed concurrently.
				if let NodeInput::Node { node_id: referenced, .. } = &new_input {
					self.ensure_node_exists(*referenced)?;
				}
				self.ensure_node_exists(node_id)?;

				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let slot = node.inputs.get_mut(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;
				if timestamp > slot.timestamp {
					slot.input = new_input;
					slot.timestamp = timestamp;
				}
			}
			RegistryDelta::ChangeNodeAttribute { node_id, delta } => {
				self.ensure_node_exists(node_id)?;
				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				apply_attribute_delta(delta, timestamp, &mut node.attributes);
			}
			RegistryDelta::ChangeNodeInputAttribute { node_id, input_idx, delta } => {
				self.ensure_node_exists(node_id)?;
				let node = self.registry.node_instances.get_mut(&node_id).ok_or(CrdtError::TargetNodeDoesNotExist)?;
				let input_attributes = node.inputs_attributes.get_mut(input_idx).ok_or(CrdtError::InputIndexOutOfBounds)?;
				apply_attribute_delta(delta, timestamp, input_attributes);
			}
			RegistryDelta::SetExport { network, slot, target } => {
				let net = self.registry.networks.get_mut(&network).ok_or(CrdtError::NetworkDoesNotExist)?;
				let slot_idx = slot as usize;

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
				self.registry.networks.remove(&network);
			}
			RegistryDelta::SetExportedNodes { nodes } => {
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
				apply_attribute_delta(delta, timestamp, &mut self.registry.attributes);
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
		Ok(match delta {
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
					input_idx,
					delta: reverse_attribute_delta(delta, input_attributes),
				}
			}
			&RegistryDelta::SetExport { network, slot, .. } => {
				let net = self.registry.networks.get(&network).ok_or(CrdtError::NetworkDoesNotExist)?;
				let target = net.exports.get(slot as usize).and_then(|s| s.target.clone());
				RegistryDelta::SetExport { network, slot, target }
			}
			RegistryDelta::AddNetwork { network, contents } => RegistryDelta::RemoveNetwork {
				network: *network,
				snapshot: contents.clone(),
			},
			&RegistryDelta::RemoveNetwork { network, ref snapshot } => RegistryDelta::AddNetwork { network, contents: snapshot.clone() },
			RegistryDelta::SetExportedNodes { .. } => RegistryDelta::SetExportedNodes {
				nodes: self.registry.exported_nodes.clone(),
			},
			RegistryDelta::ChangeDocumentAttribute { delta } => RegistryDelta::ChangeDocumentAttribute {
				delta: reverse_attribute_delta(delta, &self.registry.attributes),
			},
		})
	}

	fn history_iter(&self) -> HistoryIter<'_> {
		HistoryIter {
			document: self,
			parent_rev: self.head,
		}
	}

	fn find_delta(&self, check_fn: impl Fn(&Delta) -> bool) -> Result<&Delta, CrdtError> {
		self.history_iter().find(|d| check_fn(d)).ok_or(CrdtError::NotFoundInHistory)
	}
}

fn reverse_attribute_delta(delta: &AttributeDelta, attributes: &Attributes) -> AttributeDelta {
	AttributeDelta {
		key: delta.key.clone(),
		value: attributes.get(&delta.key).map(|previous| previous.value.clone()),
	}
}

fn apply_attribute_delta(delta: AttributeDelta, timestamp: TimeStamp, attributes: &mut Attributes) {
	let AttributeDelta { key, value } = delta;
	match value {
		Some(value) => match attributes.entry(key) {
			std::collections::hash_map::Entry::Occupied(mut entry) => {
				if timestamp > entry.get().timestamp {
					entry.insert(Value { value, timestamp });
				}
			}
			std::collections::hash_map::Entry::Vacant(entry) => {
				entry.insert(Value { value, timestamp });
			}
		},
		None => {
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
		// First parent only for now. Local-chain walking (filter by author) is a follow-up.
		self.parent_rev = *delta.parents.first()?;
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
