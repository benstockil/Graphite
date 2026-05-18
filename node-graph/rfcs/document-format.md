# Summary

A document format (`.gdd`) for Graphite that decouples on-disk layout from the editor's in-memory runtime types. The format is a flat node registry plus a tree of operation-based CRDT deltas. The same delta type drives history, undo/redo, concurrent multi-user editing, migrations, and incremental compilation.

# Motivation

A delta-based, runtime-independent storage format addresses four problems with the legacy `.graphite` format (bincode/JSON of the editor's runtime structs):

- **Scattered migrations.** Three coexisting legacy mechanisms — global string replacement on serialized JSON (`document_migration_string_preprocessing`), per-field `#[serde(alias = ...)]` / `deserialize_with` on runtime structs, and post-deserialize fixups (`migrate_path_modify_node`, `migrate_node`) — each requires keeping old runtime shapes alive in the codebase.
- **Snapshot undo/redo.** `document_undo_history: VecDeque<NodeNetworkInterface>` clones the whole interface on every gesture.
- **No concurrent editing path.** Online multi-user editing and offline merge are blocked by the snapshot model.
- **Recompiled-from-scratch graphs.** No diff signal to drive incremental compilation.

A single delta representation unifies the data needed to fix all four: history step, CRDT op, migration unit, and compilation invalidation signal.

# Guide-level explanation

A document is a `Registry` plus a tree of operations applied to it.

## Registry

The `Registry` is a **flat** node graph. All nodes from all nested networks live in a single map; each node carries a back-pointer to its network. Networks themselves only store their list of exports. Proto-node identifiers are interned into a separate declaration table.

```rs
pub struct Registry {
    pub node_instances: HashMap<NodeId, Node>,                 // all nodes, flat
    pub networks: HashMap<NetworkId, Network>,                  // exports per network
    pub node_declarations: HashMap<DeclarationId, ProtoNode>,   // interned identifiers
    pub exported_nodes: Vec<NodeId>,                            // library API surface
    pub attributes: Attributes,                                 // document-level metadata
}

pub struct Node {
    pub implementation: Implementation,     // ProtoNode(decl) or Network(net)
    pub inputs: Vec<InputSlot>,
    pub inputs_attributes: Vec<Attributes>,
    pub attributes: Attributes,
    pub network: NetworkId,
}

pub struct InputSlot {
    pub input: NodeInput,
    pub timestamp: TimeStamp,
}

pub struct Network {
    pub exports: Vec<ExportSlot>,
}

pub struct ExportSlot {
    pub target: Option<NodeInput>,           // None = removed/empty
    pub timestamp: TimeStamp,
}

pub const ROOT_NETWORK: NetworkId = 0;
```

The renderable graph lives in `networks[&ROOT_NETWORK]`. By convention the renderer consumes slot 0 of its exports; the editor can pick a different slot via type-based heuristics or user choice.

## Two exports concepts

- **`Network.exports`** — the outputs of a callable network. Used by parent networks and (on `ROOT_NETWORK`) by the renderer. High-frequency edits.
- **`Registry.exported_nodes`** — the document's library API: nodes an importing document can reference. A node exposed here may itself be backed by a network via `Implementation::Network`. Library metadata (display name, category, ...) lives as `library::*` attributes on the referenced node. Low-frequency edits.

Library import (how `.gdd` files reference each other and surface library nodes) is the subject of a follow-up RFC.

## Attributes — the type-erased metadata bucket

All metadata that isn't structural — node positions, display names, `call_argument` overrides, visibility, `context_features`, locked/pinned flags, input type hints, reflection metadata — lives in a single `Attributes` bucket per node, per input, and at the document level:

```rs
pub struct Value {
    pub value: serde_json::Value,
    pub timestamp: TimeStamp,
}

pub type Attributes = HashMap<String, Value>;
```

Keys are namespaced (`ui::position`, `compute::call_argument`, `library::display_name`, ...). Values are JSON; the per-value `TimeStamp` drives LWW on concurrent edits.

Type-erasure exists for migrations: storage data can be transformed without keeping old Rust struct shapes alive just to deserialize them.

## Deltas

A `RegistryDelta` is one atomic change to the registry, simultaneously a history step, a CRDT op to broadcast to peers, and a recompilation signal:

```rs
pub enum RegistryDelta {
    AddNode      { node_id: NodeId, node: Node },
    RemoveNode   { node_id: NodeId },
    ChangeNodeInput          { node_id: NodeId, input_idx: usize, new_input: NodeInput, timestamp: TimeStamp },
    ChangeNodeAttribute      { node_id: NodeId, delta: AttributeDelta },
    ChangeNodeInputAttribute { node_id: NodeId, input_idx: usize, delta: AttributeDelta },
    SetExport     { network: NetworkId, slot: u32, target: Option<NodeInput>, timestamp: TimeStamp },
    AddNetwork    { network: NetworkId, contents: Network },
    RemoveNetwork { network: NetworkId, snapshot: Network },
    SetExportedNodes        { nodes: Vec<NodeId>, timestamp: TimeStamp },
    ChangeDocumentAttribute { delta: AttributeDelta },
}

pub enum AttributeDelta {
    Set    { key: String, value: serde_json::Value, timestamp: TimeStamp },
    Remove { key: String, timestamp: TimeStamp },
}
```

Each delta is wrapped with metadata for history and causality:

```rs
pub struct Delta {
    pub id: Rev,
    pub timestamp: TimeStamp,
    pub predecessor: Option<Rev>,
    pub delta_type: RegistryDelta,
    pub reverse: RegistryDelta,      // precomputed for undo
}
```

## History as a tree

Branching is implicit. Every concurrent or out-of-sync edit creates a branch by virtue of sharing a `predecessor` with another delta. Linear undo is the common case; branching falls out naturally when two peers (or two windows on one machine) edit from the same predecessor.

```
              D1 ── D2 ── D3        (one user's session)
             /
   ── root ──
             \
              D4 ── D5              (another peer, branched at root)
```

A history UI lets users navigate this tree to recover from convoluted undo/redo sessions or revisit past exploration. History compression collapses similar consecutive deltas (e.g., three sequential "move shape" ops) into a single coarser delta.

## Concurrency model — CmRDT

The format uses an operation-based CRDT. The transport layer delivers ops in causal order exactly once (TCP plus the `predecessor` chain); the storage layer assumes this and requires only that concurrent op pairs commute. It does not need idempotency, state-merge, or out-of-order replay.

Graph-shape invariants (the graph remaining a DAG, the result compiling) are best-effort: conflicts that produce a non-compiling graph surface as wiring or type errors rather than being masked by the CRDT.

## Editor pipeline

The editor operates on its existing runtime types. Storage is a serialization layer for persistence, sync, and history:

```
                ┌─────────────────────────────────────────┐
                │ Editor (runtime)                        │
                │  NodeNetworkInterface                   │
                │   ├── NodeNetwork  (compute graph)      │
                │   └── NodeNetworkMetadata  (editor UI)  │
                └─────────────────────────────────────────┘
                          ▲                  │
                          │ to_runtime       │ from_runtime
                          │                  ▼
                ┌─────────────────────────────────────────┐
                │ Storage layer  (graph-storage crate)    │
                │  Registry, RegistryDelta, Document      │
                └─────────────────────────────────────────┘
                                   │
                                   ▼
                ┌─────────────────────────────────────────┐
                │ On-disk  (.gdd = zip archive)           │
                │  ├── manifest                           │
                │  ├── document  (Registry)               │
                │  ├── history   (delta DAG)              │
                │  └── resources/<content-hash>           │
                └─────────────────────────────────────────┘
```

The runtime is the source of truth during editing. Conversion runs on save, on load, and across the sync boundary when broadcasting or receiving ops.

## On-disk container

A `.gdd` file is a zip archive of a small directory:

- `manifest` — global format version, document-level attributes.
- `document` — the serialized `Registry`.
- `history` — the serialized delta DAG.
- `resources/<content-hash>` — image and other large-blob bytes.

## Resources

Images and other large blobs live in a content-addressable store inside the zip, aligned with the resource registry in PR #4148. The `Registry` stores hashes; the zip contains the bytes under `resources/<hash>`. Legacy documents with inline image `TaggedValue`s have those values extracted into the resource registry at load time. New saves never embed inline blobs.

## Migrations

Migrations run on the type-erased `Registry`, after deserialization and before `to_runtime`. The pipeline reads the format version from the manifest, deserializes the registry with attributes as raw `serde_json::Value`, applies registered migrations scoped to the version range, and hands the result to `to_runtime`.

Migrations live in a dedicated crate so they are usable both from the editor and from a CLI for batch upgrades. A single global format version is used initially; per-library versioning is a future extension.

# Reference-level explanation

## Conversion: runtime ↔ storage

`from_runtime` flattens the recursive `NodeNetwork` into the flat `Registry`:

- Each node's path through the runtime nesting is hashed to produce a stable global `NodeId`. The original local ID is stashed in an attribute (`compute::original_node_id`) so the round-trip can rebuild the runtime's per-network local IDs.
- Each nested `NodeNetwork` is assigned a fresh `NetworkId`. Aliasing (multiple nodes referencing the same network) is structurally supported by the storage model — `Implementation::Network(NetworkId)` is a reference — but the converter does not exploit it yet. Aliasing is fixed at the runtime layer first; the converter then preserves sharing without an explicit dedup pass.
- Non-structural `DocumentNode` fields (`call_argument`, `context_features`, `visible`, `skip_deduplication`, ...) become entries in the node's `attributes`. UI metadata from `DocumentNodeMetadata` (positions, display names, locked, pinned, ...) flows through the same bucket under `ui::*` keys.

`to_runtime` is the inverse: rebuild local IDs from the stashed attribute, restore typed fields from attribute values, follow `Implementation::Network` references to recursively materialize nested networks.

## Slots — inputs and exports

`Vec<InputSlot>` and `Vec<ExportSlot>` are positionally indexed at the storage layer. Each slot carries its own `TimeStamp`, giving LWW per slot on concurrent edits.

`ExportSlot` is sparse: `target == None` means the slot has been removed. `InputSlot` is dense. The runtime conversion compacts exports into a dense `Vec<NodeInput>` (preserving the runtime's "remove an export shifts later positions" semantics) and strips input timestamps.

Because inputs are stamped, `NodeInput::Node` references are set directly via `ChangeNodeInput` — there is no add/remove rewire workaround.

## CmRDT semantics

For the full design and per-op derivation, see the [CmRDT design doc](../../notes/document-format-cmrdt.md). Summary:

- **Timestamps.** `TimeStamp = (u64, PeerId)` — a Lamport counter with a peer-ID tiebreak. Comparison is lexicographic. Wall-clock time is not used.
- **NodeId identity.** Every new `AddNode` issues a peer-scoped ID, so concurrent creates cannot collide.
- **Causal delivery.** `apply_delta` requires the predecessor is already in local history. The storage layer does not buffer; out-of-order delivery is a transport concern. New peers initialize via snapshot transfer (`Registry` + history) before streaming deltas.
- **Removal.** Physical, no tombstones. If a later op targets an absent node or network, the receiver replays the most recent `AddNode` / network creation from history before applying. `RemoveNetwork` carries `snapshot: Network` so its reverse and resurrection don't require re-walking history. Removal is therefore non-durable under concurrent edits: any concurrent reference to a removed node revives it.
- **LWW primitives.** Per-input (`InputSlot.timestamp`), per-export-slot (`ExportSlot.timestamp`), per-attribute-value (the `TimeStamp` in `Attributes`), and whole-list for `SetExportedNodes` via a sidecar timestamp in `Registry.attributes` under `library::exported_nodes_ts`. `AttributeDelta::Remove` carries a timestamp so `Set` vs. `Remove` has a defined winner.

The CRDT does not mask graph-shape conflicts. Concurrent same-slot `SetExport`s with different targets resolve by LWW, but the resulting wiring may be wrong; downstream consumers see it as a compile or wiring error.

## History storage

`HashMap<Rev, Delta>` plus a `head: Rev`. Walking history follows `predecessor` chains. Branches are siblings under a shared predecessor; merges are not modeled as nodes — they are implicit in applying a delta from a different branch onto the local head.

## Editor metadata

`DocumentNodePersistentMetadata` and `NodeNetworkPersistentMetadata` from the runtime — display names, locked/pinned, navigation/PTZ state, selection undo/redo stacks, layer/node type metadata — flow through the storage `Attributes` bucket under `ui::*` keys. Transient runtime caches (`DocumentNodeTransientMetadata`, click targets, resolved types, `OriginalLocation`) stay runtime-only and are not stored.

# Drawbacks

- **Diffing two full `Registry`s on every edit is O(N) in document size per gesture.** The interim cost of treating storage as a serialization layer derived from the runtime; addressed later by computing deltas directly on runtime mutations.
- **Attributes as `serde_json::Value` carry per-value overhead.** Mitigable with postcard encoding or a typed fast path for hot keys without changing the design.
- **Single global format version is a sharp edge** when libraries diverge: a breaking change in one library bumps the version for documents that don't use it.
- **`RemoveNode` is non-durable under concurrency.** Any concurrent reference to a removed node revives it from history.

# Rationale and alternatives

**Delta-based vs. cleaner snapshot format.** A delta is the right unit for history, CRDT sync, and incremental compilation. Picking one representation for all three eliminates conversion seams between subsystems that need to interoperate.

**CmRDT vs. state-based CRDT or OT.** State-based CRDTs require a merge function and large state vectors. OT requires a central server to mediate transforms. CmRDT only requires per-op commutativity plus a causal-order delivery layer, which the transport provides.

**Ad-hoc resurrection vs. tombstones.** Tombstones add a permanent footprint to the data model and a GC policy question. Resurrection reuses the history log already needed for undo as the recovery mechanism, keeping the live `Registry` lean. The cost is that `RemoveNode` is not durable under concurrent edits.

**Type-erased attributes vs. typed metadata fields.** Migrations operate on attribute values without keeping old Rust struct shapes alive. The cost is per-value overhead, mitigable without changing the model.

**Flat node storage vs. nested networks.** All CRDT ops target nodes via a single uniform `NodeId` address space regardless of nesting depth. A nested representation would require ops to carry a path, complicating commutativity.

**`.gdd` vs. reusing `.graphite`.** A distinct extension makes migration unambiguous and prevents older Graphite versions from trying to open a new-format file.

# Future possibilities

- **Per-library format versioning** so a breaking change in one library doesn't bump the version for documents that don't use it.
- **History linearization** — prune unused branches from a convoluted tree to produce a clean undo/redo history.
- **Runtime-native deltas.** Move delta computation out of the storage layer into the runtime, eliminating per-edit `Registry` re-conversion.
- **Incremental compilation driven by deltas.** The compiler consumes runtime deltas and recompiles only changed regions.
- **Runtime-level aliasing for shared node-network definitions.** Storage already supports `Implementation::Network` as a reference; once the runtime supports sharing natively, the converter preserves it.
- **Online migration service** — active editors drop migrations older than some threshold; old documents go through a remote upgrade pipeline first.
- **Distributed / signed history.** Content-addressed `Rev` plus signing enables multi-author provenance and verifiable history.
- **Libraries as files** — a follow-up RFC will specify how `.gdd` files act as importable libraries via `Registry.exported_nodes`.
