#![expect(unused, reason = "WIP: the Document API surface is still being wired in")]
use std::collections::BTreeMap;
use std::{borrow::Cow, collections::HashMap, sync::Arc};

pub use graphene_resource::{ResourceHash, ResourceId};
use serde::{Deserialize, Serialize};

pub mod delta;
pub mod from_runtime;
pub mod metadata_source;
pub mod to_runtime;

pub use metadata_source::{InputMetadataEntry, NetworkMetadataEntry, NoMetadata, NodeMetadataEntry, NodeMetadataSource};

#[cfg(test)]
mod crdt_tests;
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
	node_declarations: HashMap<DeclarationId, ProtoNode>,
	pub node_instances: HashMap<NodeId, Node>,
	pub networks: HashMap<NetworkId, Network>,
	/// Public library API: nodes an importing document can reference.
	/// `library::*` attributes on each referenced node carry its display name, category, docs.
	pub exported_nodes: Vec<NodeId>,
	/// Append-only mapping from per-device `PeerId` to per-human `UserId`.
	/// Registered by each device's first contribution via `RegistryDelta::RegisterPeer`.
	pub peer_users: HashMap<PeerId, UserId>,
	/// Content-addressable resources (images, fonts, eventually proto-node declarations) referenced
	/// by `ResourceId`. See [`ResourceStore`].
	pub resources: ResourceStore,
	pub attributes: Attributes,
}

impl Registry {
	pub fn declaration_ids(&self) -> impl Iterator<Item = DeclarationId> + '_ {
		self.node_declarations.keys().copied()
	}

	/// True if both registries agree on every value-bearing field, ignoring per-slot and
	/// per-attribute timestamps. Mirrors `compute_deltas`'s value-only semantics, so unchanged
	/// state at a stamped slot doesn't count as drift.
	pub fn value_equal(&self, other: &Self) -> bool {
		if self.node_declarations != other.node_declarations {
			return false;
		}
		if self.exported_nodes != other.exported_nodes {
			return false;
		}
		if self.peer_users != other.peer_users {
			return false;
		}
		if !resources_value_equal(&self.resources, &other.resources) {
			return false;
		}
		if !attributes_value_equal(&self.attributes, &other.attributes) {
			return false;
		}

		if self.node_instances.len() != other.node_instances.len() {
			return false;
		}
		for (id, node) in &self.node_instances {
			let Some(other_node) = other.node_instances.get(id) else { return false };
			if !node.value_equal(other_node) {
				return false;
			}
		}

		if self.networks.len() != other.networks.len() {
			return false;
		}
		for (id, network) in &self.networks {
			let Some(other_network) = other.networks.get(id) else { return false };
			if !network.value_equal(other_network) {
				return false;
			}
		}

		true
	}

	/// True if the relative timestamp order on every shared timestamped slot agrees across
	/// the two registries. Catches LWW-bookkeeping bugs that `value_equal` deliberately ignores.
	///
	/// For every pair of shared keys (a, b), checks that `self[a].cmp(self[b])` and
	/// `other[a].cmp(other[b])` are compatible: `Equal` on either side is always compatible;
	/// otherwise both sides must agree on direction. Equality on one side imposes no order, so
	/// a registry with all-equal timestamps trivially passes against any other.
	///
	/// Slots present in only one registry are skipped. O(N²) in the number of shared timestamped
	/// slots; intended for debug-only use.
	pub fn order_consistent(&self, other: &Self) -> bool {
		let self_stamps = collect_timestamps(self);
		let other_stamps = collect_timestamps(other);

		let shared: Vec<(TimestampKey, TimeStamp, TimeStamp)> = self_stamps.into_iter().filter_map(|(key, ts)| other_stamps.get(&key).map(|other_ts| (key, ts, *other_ts))).collect();

		for i in 0..shared.len() {
			for j in (i + 1)..shared.len() {
				let self_order = shared[i].1.cmp(&shared[j].1);
				let other_order = shared[i].2.cmp(&shared[j].2);
				use std::cmp::Ordering::*;
				let compatible = matches!((self_order, other_order), (Equal, _) | (_, Equal) | (Less, Less) | (Greater, Greater));
				if !compatible {
					return false;
				}
			}
		}
		true
	}
}

fn attributes_value_equal(a: &Attributes, b: &Attributes) -> bool {
	if a.len() != b.len() {
		return false;
	}
	a.iter().all(|(key, value)| b.get(key).is_some_and(|other| value.value == other.value))
}

/// Value-level resource comparison: same resolved hashes and same source chains (keyed by
/// `SourceKey`, comparing source bodies), ignoring LWW timestamps. Mirrors `attributes_value_equal`.
fn resources_value_equal(a: &ResourceStore, b: &ResourceStore) -> bool {
	if a.len() != b.len() {
		return false;
	}
	a.iter().all(|(id, entry)| {
		b.get(id).is_some_and(|other| {
			entry.hash == other.hash
				&& entry.sources.len() == other.sources.len()
				&& entry
					.sources
					.iter()
					.all(|(key, value)| other.sources.get(key).is_some_and(|other_value| value.source == other_value.source))
		})
	})
}

/// Stable identity for any timestamped slot in a `Registry`. Used by `order_consistent`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum TimestampKey {
	NodeInput(NodeId, usize),
	NodeInputAttribute(NodeId, usize, String),
	NodeAttribute(NodeId, String),
	NetworkExport(NetworkId, usize),
	NetworkAttribute(NetworkId, String),
	DocumentAttribute(String),
	ResourceHash(ResourceId),
	ResourceSource(ResourceId, SourceKey),
}

fn collect_timestamps(registry: &Registry) -> HashMap<TimestampKey, TimeStamp> {
	let mut out = HashMap::new();
	for (node_id, node) in &registry.node_instances {
		for (i, slot) in node.inputs.iter().enumerate() {
			out.insert(TimestampKey::NodeInput(*node_id, i), slot.timestamp);
		}
		for (i, attrs) in node.inputs_attributes.iter().enumerate() {
			for (key, value) in attrs {
				out.insert(TimestampKey::NodeInputAttribute(*node_id, i, key.clone()), value.timestamp);
			}
		}
		for (key, value) in &node.attributes {
			out.insert(TimestampKey::NodeAttribute(*node_id, key.clone()), value.timestamp);
		}
	}
	for (network_id, network) in &registry.networks {
		for (i, slot) in network.exports.iter().enumerate() {
			out.insert(TimestampKey::NetworkExport(*network_id, i), slot.timestamp);
		}
		for (key, value) in &network.attributes {
			out.insert(TimestampKey::NetworkAttribute(*network_id, key.clone()), value.timestamp);
		}
	}
	for (key, value) in &registry.attributes {
		out.insert(TimestampKey::DocumentAttribute(key.clone()), value.timestamp);
	}
	for (id, entry) in &registry.resources {
		out.insert(TimestampKey::ResourceHash(*id), entry.hash_timestamp);
		for (source_key, source_value) in &entry.sources {
			out.insert(TimestampKey::ResourceSource(*id, *source_key), source_value.timestamp);
		}
	}
	out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Document {
	registry: Registry,
	history: HashMap<Rev, Delta>,
	/// Live broadcast stream — applied to the registry on receive, GC'd at retirement.
	/// Persisted for crash recovery so in-flight unretired work survives editor restarts.
	hot_log: Vec<HotOp>,
	/// User's cursor in their local chain.
	head: Rev,
	clock: LamportClock,
	peer: PeerId,
	/// Latest retired commit on the local chain that has been broadcast to at least one peer.
	/// Commits after this can be rewritten silently; commits at or before this are published
	/// and require forward reverse-delta ops to undo. `None` means nothing broadcast yet.
	last_broadcast_rev: Option<Rev>,
	/// Shared-monotonic counter feeding `next_node_id`. Bumped on every mint regardless of which
	/// peer is calling; collision avoidance comes from hashing `(self.peer, counter)`, so two peers
	/// reading the same counter still produce distinct IDs.
	next_node_counter: u64,
}

/// A live editing session over a `Document`. Owns the document plus runtime collaboration
/// state that isn't persisted (currently just peer heartbeat tracking).
#[derive(Clone, Debug)]
pub struct Session {
	document: Document,
	/// Each peer's `retirement_tip` as reported by their most recent heartbeat. Drives
	/// leader-eligibility computation (lowest PeerId among peers whose tip matches the session max).
	remote_tips: HashMap<PeerId, Rev>,
}

impl Session {
	/// Mints a fresh `PeerId` from the process-wide UUID generator and wraps an empty `Document`.
	/// Two peers in the same process will collide (the generator is seeded once); use `with_peer`
	/// in tests where determinism matters.
	pub fn new() -> Self {
		Self::with_peer(PeerId(core_types::uuid::generate_uuid()))
	}

	/// Construct a session bound to a specific `PeerId`. Used by tests; production code wants
	/// `Session::new`.
	pub fn with_peer(peer: PeerId) -> Self {
		Self {
			document: Document {
				registry: Registry::default(),
				history: HashMap::new(),
				hot_log: Vec::new(),
				head: 0,
				clock: LamportClock::new(peer),
				peer,
				last_broadcast_rev: None,
				next_node_counter: 0,
			},
			remote_tips: HashMap::new(),
		}
	}

	pub fn peer(&self) -> PeerId {
		self.document.peer
	}

	pub fn registry(&self) -> &Registry {
		&self.document.registry
	}

	/// Diff the current registry against a fresh conversion of `network`, then commit each emitted
	/// op as its own `Delta` on the local chain. One `clock.tick()` per op (strictly causal within
	/// a commit). Returns the new `Rev`s in commit order, empty if nothing changed.
	pub fn commit_from_runtime<M: NodeMetadataSource>(
		&mut self,
		network: &graph_craft::document::NodeNetwork,
		metadata: &M,
		resources: &graphene_resource::ResourceRegistry,
	) -> Result<Vec<Rev>, CommitError> {
		let target = Registry::from_runtime_with_metadata(network, metadata, resources, self.document.peer)?;
		let ops = crate::delta::compute_deltas(&self.document.registry, &target);
		Ok(self.commit_ops(ops, false)?)
	}

	/// Wrap each op as a `Delta`, apply it, and chain it onto the local history. One tick per op.
	/// `idempotent` is forwarded to `apply_op`: pass `true` when the registry already reflects the
	/// op (e.g. retirement of a hot op that was applied at broadcast time) so duplicate
	/// structural inserts no-op rather than error.
	fn commit_ops(&mut self, ops: impl IntoIterator<Item = RegistryDelta>, idempotent: bool) -> Result<Vec<Rev>, CrdtError> {
		let ops = ops.into_iter();
		let mut produced = Vec::with_capacity(ops.size_hint().0);
		for op in ops {
			let reverse = self.document.compute_reverse_delta(&op)?;
			let timestamp = self.document.clock.tick();
			let parents = if self.document.head == 0 { Vec::new() } else { vec![self.document.head] };
			let author = self.document.peer;

			let delta = Delta::new(parents, author, timestamp, op, reverse);
			let rev = delta.id;

			for parent in &delta.parents {
				assert!(self.document.history.contains_key(parent), "commit parent must be in history");
			}
			self.document.apply_op(delta.delta_type.clone(), delta.timestamp, idempotent)?;
			self.document.history.insert(rev, delta);
			self.document.head = rev;
			produced.push(rev);
		}

		Ok(produced)
	}

	/// Wrap an already-materialized snapshot. Trusts `registry` to match `history`; advances the
	/// clock past every observed timestamp but does not re-apply ops.
	pub fn load(peer: PeerId, registry: Registry, history: HashMap<Rev, Delta>, head: Rev, next_node_counter: u64) -> Self {
		let mut clock = LamportClock::new(peer);
		for delta in history.values() {
			clock.observe(delta.timestamp);
		}

		Self {
			document: Document {
				registry,
				history,
				hot_log: Vec::new(),
				head,
				clock,
				peer,
				last_broadcast_rev: None,
				next_node_counter,
			},
			remote_tips: HashMap::new(),
		}
	}

	/// Rebuild the registry from scratch by applying every delta in causal order.
	/// `deltas` must be in causal order (every parent before its children).
	pub fn replay_from_history(peer: PeerId, deltas: impl IntoIterator<Item = Delta>, next_node_counter: u64) -> Result<Self, CrdtError> {
		let mut session = Self::with_peer(peer);
		session.document.next_node_counter = next_node_counter;

		for delta in deltas {
			let rev = delta.id;
			session.document.apply_op(delta.delta_type.clone(), delta.timestamp, true)?;
			session.document.history.insert(rev, delta);
			session.document.head = rev;
		}

		Ok(session)
	}

	/// Apply a hot op without going through the broadcast stream.
	pub fn apply_hot_op(&mut self, hot_op: HotOp) -> Result<(), CrdtError> {
		self.document.apply_hot_op(hot_op)
	}

	/// Replay a persisted hot op. Idempotent on structural ops, suitable for crash recovery
	/// where the registry may already reflect the op's effect from a prior retired snapshot.
	pub fn replay_hot_op(&mut self, hot_op: HotOp) -> Result<(), CrdtError> {
		self.document.replay_hot_op(hot_op)
	}

	/// Promote hot ops with timestamp `≤ up_to` into retired deltas, re-applied with fresh
	/// retirement timestamps so LWW arms bump field timestamps to `T_retire`.
	///
	/// Today: one retired delta per hot op. Coarsening is a future step.
	pub fn retire(&mut self, up_to: TimeStamp) -> Result<Vec<Rev>, CrdtError> {
		let mut drained = Vec::new();
		let mut remaining = Vec::with_capacity(self.document.hot_log.len());
		for hot_op in self.document.hot_log.drain(..) {
			if hot_op.timestamp <= up_to {
				drained.push(hot_op);
			} else {
				remaining.push(hot_op);
			}
		}
		self.document.hot_log = remaining;

		self.commit_ops(drained.into_iter().map(|hot_op| hot_op.op), true)
	}

	/// Build a synthetic linear history whose replay reproduces `registry`. Each op gets a
	/// freshly-ticked clock timestamp and chains to the previous op's `Rev`.
	pub fn bootstrap_from_registry(peer: PeerId, registry: Registry) -> Result<Self, CrdtError> {
		let ops = crate::delta::compute_deltas(&Registry::default(), &registry);
		let mut session = Self::with_peer(peer);
		session.commit_ops(ops, false)?;
		Ok(session)
	}

	pub fn history(&self) -> impl Iterator<Item = &Delta> + '_ {
		self.document.history.values()
	}

	pub fn hot_log(&self) -> &[HotOp] {
		&self.document.hot_log
	}

	pub fn head_rev(&self) -> Rev {
		self.document.head
	}

	pub fn next_node_counter(&self) -> u64 {
		self.document.next_node_counter
	}
}

/// Errors from `Session::commit_from_runtime`.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
	#[error("Failed to convert runtime network: {0}")]
	Conversion(#[from] from_runtime::ConversionError),
	#[error("Failed to apply commit: {0}")]
	Crdt(#[from] CrdtError),
}

impl Default for Session {
	fn default() -> Self {
		Self::new()
	}
}

/// One live op in the hot zone. Carries only enough to drive live LWW; no parents (transient),
/// no Rev (not content-addressed in the durable DAG). GC'd at retirement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotOp {
	pub op: RegistryDelta,
	pub timestamp: TimeStamp,
	pub author: PeerId,
}

pub type DeclarationId = u64; // Content-based hash
pub type NodeId = u64;
pub type NetworkId = u64;
type ProtoNodeId = String;
/// Content-addressed identity for a `Delta`.
/// 128-bit blake3 truncation: comfortable collision headroom for any plausible document lifetime
/// without being adversarial-grade. Same delta content always produces the same `Rev`.
pub type Rev = u128;

/// Per-device identity. Stable per `(device, document)`. Used for CRDT tiebreaking and `NodeId`
/// scoping. Globally unique across all peers ever in a document.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct PeerId(pub u64);

/// Per-human identity. Stable across devices (one user, many devices). Used for identity display
/// and undo-chain walking. Derived from `PeerId` via `Registry.peer_users`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct UserId(pub u64);

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

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize)]
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

/// Fractional priority for ordering a resource's source chain. New sources are inserted by picking
/// a value strictly between two neighbors, so concurrent insertions elsewhere never collide; an
/// exact tie between two peers inserting at the same gap is broken by `PeerId` in [`SourceKey`].
/// `f64` precision is ample for the short fallback chains resources carry in practice.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Priority(pub f64);

impl Eq for Priority {}

impl Ord for Priority {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		// Source priorities are always finite values we mint ourselves; `total_cmp` gives a total
		// order regardless, so a stray NaN sorts deterministically rather than panicking.
		self.0.total_cmp(&other.0)
	}
}

impl PartialOrd for Priority {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl std::hash::Hash for Priority {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		// Hash the bit pattern, consistent with the `total_cmp`-based `Eq`.
		self.0.to_bits().hash(state);
	}
}

/// Ordering key for an entry in a resource's source chain: fractional `priority`, with `peer` as
/// the tiebreak so concurrent insertions at the same priority converge deterministically.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
	pub priority: Priority,
	pub peer: PeerId,
}

/// One entry in a resource's source chain. The `source` body is type-erased (`serde_json::Value`)
/// so the on-disk `DataSource` shape can evolve through migrations without the storage layer
/// committing to a Rust enum; `timestamp` drives LWW on re-setting this same entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceValue {
	pub source: serde_json::Value,
	pub timestamp: TimeStamp,
}

/// A single content-addressable resource: an ordered, conflict-mergeable chain of fallback sources
/// plus the resolved content hash. The source chain is an add-wins ordered set (concurrent
/// additions all survive); the hash is last-writer-wins (concurrent resolves of the same logical
/// resource agree by construction, since the hash is content-derived).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceEntry {
	pub sources: BTreeMap<SourceKey, SourceValue>,
	pub hash: Option<ResourceHash>,
	pub hash_timestamp: TimeStamp,
}

/// All resources referenced by the document, keyed by stable per-document [`ResourceId`]. Replicates
/// through the normal CmRDT path; bytes live in content-addressed storage keyed by [`ResourceHash`].
pub type ResourceStore = HashMap<ResourceId, ResourceEntry>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
	implementation: Implementation,
	inputs: Vec<InputSlot>,
	inputs_attributes: Vec<Attributes>,
	attributes: Attributes,
	network: NetworkId,
}

impl Node {
	pub fn implementation(&self) -> &Implementation {
		&self.implementation
	}
	pub fn inputs(&self) -> &[InputSlot] {
		&self.inputs
	}
	pub fn inputs_attributes(&self) -> &[Attributes] {
		&self.inputs_attributes
	}
	pub fn attributes(&self) -> &Attributes {
		&self.attributes
	}
	pub fn network(&self) -> NetworkId {
		self.network
	}

	/// True if both nodes agree on every value-bearing field, ignoring slot/attribute timestamps.
	pub fn value_equal(&self, other: &Self) -> bool {
		if self.implementation != other.implementation || self.network != other.network {
			return false;
		}
		if self.inputs.len() != other.inputs.len() {
			return false;
		}
		if !self.inputs.iter().zip(&other.inputs).all(|(a, b)| a.input == b.input) {
			return false;
		}
		if self.inputs_attributes.len() != other.inputs_attributes.len() {
			return false;
		}
		if !self.inputs_attributes.iter().zip(&other.inputs_attributes).all(|(a, b)| attributes_value_equal(a, b)) {
			return false;
		}
		attributes_value_equal(&self.attributes, &other.attributes)
	}
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Network {
	pub exports: Vec<ExportSlot>,
	/// Per-network `ui::*` state (navigation, previewing). Separate from `Node.attributes` so
	/// view-state edits LWW independently.
	pub attributes: Attributes,
}

impl Network {
	/// True if both networks agree on every value-bearing field, ignoring slot/attribute timestamps.
	pub fn value_equal(&self, other: &Self) -> bool {
		if self.exports.len() != other.exports.len() {
			return false;
		}
		if !self.exports.iter().zip(&other.exports).all(|(a, b)| a.target == b.target) {
			return false;
		}
		attributes_value_equal(&self.attributes, &other.attributes)
	}
}

/// One positional export slot. `target == None` marks an empty/removed slot. Timestamp drives LWW
/// on concurrent `SetExport` ops.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportSlot {
	pub target: Option<NodeInput>,
	pub timestamp: TimeStamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
pub struct Delta {
	pub id: Rev,
	pub parents: Vec<Rev>,
	pub author: PeerId,
	pub timestamp: TimeStamp,
	pub delta_type: RegistryDelta,
	pub reverse: RegistryDelta,
}

impl Delta {
	pub fn new(parents: Vec<Rev>, author: PeerId, timestamp: TimeStamp, delta_type: RegistryDelta, reverse: RegistryDelta) -> Self {
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

/// Hash `(peer, counter)` with blake3 and truncate to 64 bits to mint a peer-scoped `NodeId`.
/// Two peers reading the same counter still produce distinct IDs because the peer is in the hash.
fn mint_node_id(peer: PeerId, counter: u64) -> NodeId {
	let bytes = postcard::to_stdvec(&(peer, counter)).expect("(PeerId, counter) must serialize");
	let digest = blake3::hash(&bytes);
	let mut truncated = [0u8; 8];
	truncated.copy_from_slice(&digest.as_bytes()[..8]);
	NodeId::from_le_bytes(truncated)
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
	/// Append-only registration of a device's `PeerId` against its owning `UserId`.
	/// First write wins; conflicting re-registration errors. Duplicate identical registration
	/// is a no-op. Not LWW — the mapping is forever.
	RegisterPeer {
		peer: PeerId,
		user: UserId,
	},
	/// LWW on a resource's resolved content hash. Creates the resource entry if absent.
	/// Concurrent resolves agree by construction (the hash is content-derived), so LWW is safe.
	RegisterResource {
		id: ResourceId,
		hash: Option<ResourceHash>,
	},
	/// Add (or LWW-overwrite) one entry in a resource's source fallback chain. The source body is
	/// type-erased; `key` carries the fractional priority + peer that order it. Add-wins: concurrent
	/// adds at distinct keys all survive. Creates the resource entry if absent.
	AddSource {
		id: ResourceId,
		key: SourceKey,
		source: serde_json::Value,
	},
	/// Remove one entry from a resource's source chain. LWW against the entry's timestamp.
	RemoveSource {
		id: ResourceId,
		key: SourceKey,
	},
}

/// `value: None` means remove. The timestamp comes from the wrapping `Delta`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributeDelta {
	pub key: String,
	pub value: Option<serde_json::Value>,
}

impl Document {
	/// Mint a fresh `NodeId` scoped to this document's peer. The 64-bit ID is `blake3(peer, counter)`
	/// truncated; the counter is shared across peers and persisted with the document.
	pub fn next_node_id(&mut self) -> NodeId {
		self.next_node_counter += 1;
		mint_node_id(self.peer, self.next_node_counter)
	}

	pub fn restore_node_from_history(&mut self, old_node_id: NodeId) -> Result<(), CrdtError> {
		let delta = self
			.history_iter()
			.find(|d| matches!(d.reverse, RegistryDelta::AddNode { node_id, .. } if node_id == old_node_id))
			.ok_or(CrdtError::NotFoundInHistory)?
			.clone();
		self.revert_delta(delta)
	}

	pub fn restore_network_from_history(&mut self, network_id: NetworkId) -> Result<(), CrdtError> {
		// Find the Delta whose forward op removed this network. Its `reverse` is `AddNetwork`,
		// which is what we want to re-apply.
		let delta = self
			.history_iter()
			.find(|d| matches!(&d.reverse, RegistryDelta::AddNetwork { network, .. } if *network == network_id))
			.ok_or(CrdtError::NotFoundInHistory)?
			.clone();
		self.revert_delta(delta)
	}

	/// Apply a delta's `reverse` as the new forward op. Idempotent on structural ops: a revert
	/// whose target state already holds is a no-op.
	pub fn revert_delta(&mut self, mut delta: Delta) -> Result<(), CrdtError> {
		std::mem::swap(&mut delta.delta_type, &mut delta.reverse);
		for parent in &delta.parents {
			assert!(self.history.contains_key(parent));
		}
		self.apply_op(delta.delta_type, delta.timestamp, true)
	}

	pub fn apply_delta(&mut self, delta: Delta) -> Result<(), CrdtError> {
		for parent in &delta.parents {
			assert!(self.history.contains_key(parent));
		}
		self.apply_op(delta.delta_type, delta.timestamp, false)
	}

	/// Apply a live broadcast op. Updates the registry via LWW and appends to the hot log.
	/// Doesn't touch history or `head` — hot ops are transient.
	pub fn apply_hot_op(&mut self, hot_op: HotOp) -> Result<(), CrdtError> {
		self.apply_hot_op_with(hot_op, false)
	}

	/// Replay a hot op recovered from persisted state. Idempotent on structural ops so that
	/// re-applying an op whose effect is already reflected in the registry is a no-op rather
	/// than an error.
	pub fn replay_hot_op(&mut self, hot_op: HotOp) -> Result<(), CrdtError> {
		self.apply_hot_op_with(hot_op, true)
	}

	fn apply_hot_op_with(&mut self, hot_op: HotOp, idempotent: bool) -> Result<(), CrdtError> {
		self.apply_op(hot_op.op.clone(), hot_op.timestamp, idempotent)?;
		self.hot_log.push(hot_op);
		Ok(())
	}

	/// Apply a retired commit. Idempotent on structural ops (AddNode/AddNetwork on existing
	/// targets, Remove on missing ones) since hot ops already produced the structural state.
	/// The point is to bump field timestamps to T_retire via the LWW arms.
	pub fn apply_retired_delta(&mut self, delta: Delta) -> Result<(), CrdtError> {
		for parent in &delta.parents {
			assert!(self.history.contains_key(parent));
		}
		self.apply_op(delta.delta_type.clone(), delta.timestamp, true)?;
		self.history.insert(delta.id, delta);
		Ok(())
	}

	fn apply_op(&mut self, op: RegistryDelta, timestamp: TimeStamp, idempotent: bool) -> Result<(), CrdtError> {
		// Advance the local clock past every observed op, including ones that subsequently no-op or
		// error. Observation is about causality knowledge, not about whether the op took effect.
		self.clock.observe(timestamp);

		match op {
			RegistryDelta::AddNode { node_id, node } => {
				if self.registry.node_instances.contains_key(&node_id) {
					if idempotent {
						// Hot ops already created this node; skip rather than error.
						return Ok(());
					}
					return Err(CrdtError::NodeAlreadyExists);
				}
				// Cascading resurrection: if the node's owning network is gone, restore it first.
				self.ensure_network_exists(node.network)?;
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
				if let Some(NodeInput::Node { node_id: referenced, .. }) = &target {
					self.ensure_node_exists(*referenced)?;
				}
				self.ensure_network_exists(network)?;

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
					if idempotent {
						return Ok(());
					}
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
			RegistryDelta::RegisterPeer { peer, user } => match self.registry.peer_users.get(&peer) {
				Some(existing) if *existing != user => return Err(CrdtError::PeerRegistrationConflict),
				Some(_) => {}
				None => {
					self.registry.peer_users.insert(peer, user);
				}
			},
			RegistryDelta::RegisterResource { id, hash } => {
				let entry = self.registry.resources.entry(id).or_default();
				if timestamp > entry.hash_timestamp {
					entry.hash = hash;
					entry.hash_timestamp = timestamp;
				}
			}
			RegistryDelta::AddSource { id, key, source } => {
				let entry = self.registry.resources.entry(id).or_default();
				match entry.sources.entry(key) {
					std::collections::btree_map::Entry::Occupied(mut occupied) => {
						if timestamp > occupied.get().timestamp {
							occupied.insert(SourceValue { source, timestamp });
						}
					}
					std::collections::btree_map::Entry::Vacant(vacant) => {
						vacant.insert(SourceValue { source, timestamp });
					}
				}
			}
			RegistryDelta::RemoveSource { id, key } => {
				if let Some(entry) = self.registry.resources.get_mut(&id) {
					let should_remove = entry.sources.get(&key).is_some_and(|existing| timestamp > existing.timestamp);
					if should_remove {
						entry.sources.remove(&key);
					}
				}
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

	fn ensure_network_exists(&mut self, network_id: NetworkId) -> Result<(), CrdtError> {
		if !self.registry.networks.contains_key(&network_id) {
			self.restore_network_from_history(network_id)?;
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
				// If the network is absent the forward op will resurrect it; the reverse is "set the export to None"
				// since pre-forward there was no export to point at.
				let target = self.registry.networks.get(&network).and_then(|net| net.exports.get(slot as usize)).and_then(|s| s.target.clone());
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
			// Registrations are append-only and not user-undoable; reverse is the same op,
			// which applies as a no-op on the already-registered PeerId.
			&RegistryDelta::RegisterPeer { peer, user } => RegistryDelta::RegisterPeer { peer, user },
			&RegistryDelta::RegisterResource { id, .. } => RegistryDelta::RegisterResource {
				id,
				hash: self.registry.resources.get(&id).and_then(|entry| entry.hash),
			},
			&RegistryDelta::AddSource { id, key, .. } => match self.registry.resources.get(&id).and_then(|entry| entry.sources.get(&key)) {
				// The slot already held a source: undo restores it.
				Some(existing) => RegistryDelta::AddSource {
					id,
					key,
					source: existing.source.clone(),
				},
				// The slot was empty: undo removes what this op added.
				None => RegistryDelta::RemoveSource { id, key },
			},
			&RegistryDelta::RemoveSource { id, key } => match self.registry.resources.get(&id).and_then(|entry| entry.sources.get(&key)) {
				Some(existing) => RegistryDelta::AddSource {
					id,
					key,
					source: existing.source.clone(),
				},
				// Nothing to restore; reverse is a no-op removal.
				None => RegistryDelta::RemoveSource { id, key },
			},
		})
	}

	/// Retired-only walk from `head` along first parents. Hot ops are excluded by design.
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

#[derive(Debug, thiserror::Error)]
pub enum CrdtError {
	#[error("Target node does not exist")]
	TargetNodeDoesNotExist,
	#[error("Network does not exist")]
	NetworkDoesNotExist,
	#[error("Input index out of bounds")]
	InputIndexOutOfBounds,
	#[error("Delta not found in history")]
	NotFoundInHistory,
	#[error("Node already exists")]
	NodeAlreadyExists,
	#[error("Network already exists")]
	NetworkAlreadyExists,
	/// PeerId is already registered to a different UserId.
	#[error("Peer is already registered to a different user")]
	PeerRegistrationConflict,
}
