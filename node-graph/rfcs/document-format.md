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
    pub networks: HashMap<NetworkId, Network>,                  // exports + per-network attrs
    pub node_declarations: HashMap<DeclarationId, ProtoNode>,   // interned identifiers
    pub exported_nodes: Vec<NodeId>,                            // library API surface
    pub peer_users: HashMap<PeerId, UserId>,                    // per-device → per-human identity
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
    pub attributes: Attributes,              // per-network ui::* (navigation, previewing)
}

pub struct ExportSlot {
    pub target: Option<NodeInput>,           // None = removed/empty
    pub timestamp: TimeStamp,
}

pub const ROOT_NETWORK: NetworkId = 0;
```

`peer_users` records the append-only `PeerId → UserId` mapping written by each device's first contribution (see [Concurrency model](#concurrency-model--cmrdt)).

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
    ChangeNodeInput          { node_id: NodeId, input_idx: usize, new_input: NodeInput },
    ChangeNodeAttribute      { node_id: NodeId, delta: AttributeDelta },
    ChangeNodeInputAttribute { node_id: NodeId, input_idx: usize, delta: AttributeDelta },
    SetExport     { network: NetworkId, slot: u32, target: Option<NodeInput> },
    AddNetwork    { network: NetworkId, contents: Network },
    RemoveNetwork { network: NetworkId, snapshot: Network },
    SetExportedNodes        { nodes: Vec<NodeId> },
    ChangeDocumentAttribute { delta: AttributeDelta },
    RegisterPeer            { peer: PeerId, user: UserId },
}

/// `value: None` is the removal case. Timestamp lives on the wrapping `Delta`.
pub struct AttributeDelta {
    pub key: String,
    pub value: Option<serde_json::Value>,
}
```

Each delta is wrapped with metadata for history, identity, and causality. `Rev` is content-addressed: `blake3` truncated to 128 bits of `(parents, author, timestamp, delta_type)`, so identical content always produces the same `Rev` and concurrent retirements that converge collapse by construction.

```rs
pub type Rev = u128;

pub struct Delta {
    pub id: Rev,
    pub parents: Vec<Rev>,           // multi-parent for JJ-style merges
    pub author: PeerId,
    pub timestamp: TimeStamp,
    pub delta_type: RegistryDelta,
    pub reverse: RegistryDelta,      // precomputed for undo; excluded from id
}
```

One timestamp per `Delta` applies to every LWW-eligible write inside its `delta_type` — slot writes, attribute writes, and whole-list writes all read the same `Delta.timestamp`.

## History as a tree

History is a multi-parent DAG. Branching is implicit: every concurrent or out-of-sync edit creates a branch by virtue of sharing a parent with another delta. A user's first commit after observing remote work adds the remote tip as an additional parent, so merges ride on the user's own edit rather than introducing phantom merge commits.

```
              D1 ── D2 ── D3        (one user's session)
             /
   ── root ──
             \
              D4 ── D5              (another peer, branched at root)
```

Linear undo is the common case; branching falls out naturally when two peers (or two windows on one machine) edit from the same parent. A history UI lets users navigate this tree to recover from convoluted undo/redo sessions or revisit past exploration. History compression collapses similar consecutive deltas (e.g., three sequential "move shape" ops) into a single coarser delta.

## Two-tier history: hot ops and retired commits

History has two tiers:

- **Hot ops** — speculative, broadcast per-keystroke for live collaboration. Carry only a Lamport timestamp; no parents, no content-addressed `Rev`. Live in `Document.hot_log`, GC'd at retirement, persisted as a sidecar for crash recovery. May pass through non-compiling intermediate states.
- **Retired commits** — coarser `Delta`s produced by retirement. Every retired commit compiles in the leader's local view. Content-addressed, multi-parent, durable, browseable, replayable.

A leader-elected peer periodically retires a window of hot ops into one or more semantically-equivalent retired commits (one per logical `(node, field)` group, not one giant commit per window). Retired commits use a single retirement timestamp for every field they write; the original hot-op timestamps are discarded. Leader election is gossip-based — lowest `PeerId` among peers whose `retirement_tip` matches the session max — and best-effort: there is no quorum, since content-addressed `Rev`s make concurrent retirements that converge dedupe by construction.

Undo is always a forward reverse-delta commit with a fresh timestamp; hot or retired target, same mechanism. Do/undo pair collapse only happens when both land in the same retirement window, subject to dependency closure (collapse must not orphan a reference to `X`).

Solo retirement is the same mechanism with a session of one — history compaction during solo editing falls out for free.

See [the collaboration model note](../../notes/document-format-collaboration.md) for the full retirement, leader-election, persistence, and reconnect semantics.

## Concurrency model — CmRDT

The format uses an operation-based CRDT. The transport layer delivers ops in causal order exactly once (TCP plus the multi-parent chain in each `Delta`); the storage layer assumes this and requires only that concurrent op pairs commute. It does not need idempotency, state-merge, or out-of-order replay.

Graph-shape invariants (the graph remaining a DAG, the result compiling) are best-effort: conflicts that produce a non-compiling graph surface as wiring or type errors rather than being masked by the CRDT.

Identity is two-tier: `PeerId` is per-device (stable per `(device, document)`, used for CRDT tiebreaking and `NodeId` scoping); `UserId` is per-human (stable across devices, used for identity display and undo-chain walking). Each device's first contribution emits `RegisterPeer { peer, user }`, which writes an append-only entry to `Registry.peer_users`. Causal delivery guarantees the registration arrives before any of that peer's other ops.

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
                │ On-disk  (.gdd container)               │
                │  named payloads: manifest, document,    │
                │  history, resources/<hash>              │
                │  served by an interchangeable backend   │
                │  (folder, zip, xz, ...)                 │
                └─────────────────────────────────────────┘
```

The runtime is the source of truth during editing. Conversion runs on save, on load, and across the sync boundary when broadcasting or receiving ops. The editor-facing handle is `Session` (`graph_storage::Session`); `Document` is internal. `Session::commit_from_runtime(&NodeNetwork, &dyn NodeMetadataSource)` is the single entry point: it diffs the stored registry against a fresh conversion, ticks the clock once per emitted op, wraps each in a `Delta`, and applies + records to history.

## On-disk container

A `.gdd` document is a collection of named byte payloads held together by an interchangeable container backend. The same logical document can be saved as a loose folder (VCS-friendly, diffable), a zip archive (compact, portable), or an xz-compressed archive (archival), without any change above the container layer.

The container abstraction lives in two downstream crates: `gdd-container` owns the backends and byte-ownership (`memmap2` regions, decompressed buffers, mmap'd external files), and `gdd-format` defines the typed `Gdd` handle, the layout (logical-payload-name → in-container path), the codec, and the save/load orchestration. `graph-storage` itself stays disk-unaware.

```
            ┌─────────────────────────────────┐
            │ editor                          │
            └─────────────────────────────────┘
                  │              │
                  ▼              ▼
            ┌───────────────┐  ┌──────────────────────────────┐
            │ graph-storage │  │ gdd-format                   │
            │ (disk-unaware)│◀─│  Gdd handle, Layout, codec,  │
            └───────────────┘  │  SaveOptions                 │
                               └──────────────────────────────┘
                                              │
                                              ▼
                               ┌──────────────────────────────┐
                               │ gdd-container                │
                               │  backends (folder, zip, xz)  │
                               └──────────────────────────────┘
```

Arrows are "depends on": the editor uses `Session` from `graph-storage` at runtime and `Gdd` from `gdd-format` on save/load; `gdd-format` serializes `graph-storage`'s types and delegates byte I/O to `gdd-container`; `graph-storage` and `gdd-container` are independent leaves.

A document contains:

- `manifest.json` — always JSON, the bootstrap file. Carries the magic identifier `"gdd"`, a single `u32` `format_version`, a stable `document_uuid`, the saving session's `PeerId`, editor and stdlib versions, an optional save timestamp, and a record of which payloads this save included (registry / history / embedded resources).
- `document.{json,bin}` — the serialized `Registry`. Codec chosen per-save (JSON for inspectable, binary for compact).
- `history.{jsonl,bin}` — the serialized delta DAG. JSON history is line-oriented (one delta per line) so appending is a one-line diff.
- `resources/<hash>` — embedded resource bytes, keyed by `ResourceHash`.

In the folder backend the same layout shows up as plain files on disk; archive backends pack the same named entries into a single file.

```
            my-doc.gdd/
            ├── manifest.json
            ├── document.json
            ├── history.jsonl
            └── resources/
                ├── 7f3a...
                └── 2c91...
```

The `Gdd` handle owns the loaded bytes and exposes them as zero-copy slices. On the folder backend, reads are direct mmap references; on archive backends, contents are decompressed once on open. Writes mutate the handle in place; `save()` / `save_as()` persists to a chosen backend.

A `SaveOptions` struct controls scope per-save: `include_registry` (skip = rebuild from history on load), `include_history` (skip = state-only snapshot), `embed_all_resources` (materialize every linked-file resource into the container for a portable file, without mutating the in-memory state), and `codec`. These compose freely except that `include_registry: false && include_history: false` is rejected.

## Resources

Resource bytes are content-addressed by `ResourceHash` and resolved through the `ResourceRegistry` (`node-graph/libraries/resources`). Each resource carries a chain of `DataSource` variants tried in order: `Embedded` (bytes live in `resources/<hash>` inside the container), `FilePath(PathBuf)` (bytes mmap'd from an external file at the recorded path), `Url`, `Font` (runtime-resolved, not container-resolved). Multiple sources can coexist on one resource — e.g., `FilePath` for the local working copy and `Embedded` as a portability fallback.

On disk, each `DataSource` is encoded as `serde_json::Value` rather than a typed enum, with the same motivation as the `Attributes` bucket: type-erasure lets migrations rename or restructure variants without keeping old enum shapes alive in Rust, and lets older binaries preserve unknown variants instead of failing to deserialize. `DataSource` stays a typed enum at the runtime layer; the conversion happens at the serialization boundary.

Legacy documents with inline image `TaggedValue`s have those values extracted into the resource registry at load time; new saves never embed inline blobs in `NodeInput::Value`. Embed-vs-link policy on save is dictated by the registry's `DataSource` chain, not by the container.

## Migrations

Migrations run on the type-erased `Registry`, after deserialization and before `to_runtime`. The pipeline reads the format version from the manifest, deserializes the registry with attributes as raw `serde_json::Value`, applies registered migrations scoped to the version range, and hands the result to `to_runtime`.

Migrations live in a dedicated crate so they are usable both from the editor and from a CLI for batch upgrades. A single global format version is used initially; per-library versioning is a future extension.

# Reference-level explanation

## Conversion: runtime ↔ storage

`from_runtime` flattens the recursive `NodeNetwork` into the flat `Registry`:

- Each node's path through the runtime nesting is hashed (blake3 truncated to 64 bits, with the document's `PeerId` mixed in) to produce a stable global `NodeId`. The original local ID is stashed in an attribute (`compute::original_node_id`) so the round-trip can rebuild the runtime's per-network local IDs. Subsequent live edits mint fresh peer-scoped IDs via `Document::next_node_id` (`blake3(peer, counter)`) instead of going through the path-hash bootstrap.
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
- **Causal delivery.** `apply_delta` requires every entry in `delta.parents` is already in local history. The storage layer does not buffer; out-of-order delivery is a transport concern. New peers initialize via snapshot transfer (`Registry` + history) before streaming deltas.
- **Removal.** Physical, no tombstones. If a later op targets an absent node or network, the receiver replays the most recent `AddNode` / network creation from history before applying. `RemoveNetwork` carries `snapshot: Network` so its reverse and resurrection don't require re-walking history. Removal is therefore non-durable under concurrent edits: any concurrent reference to a removed node revives it.
- **LWW primitives.** Per-input (`InputSlot.timestamp`), per-export-slot (`ExportSlot.timestamp`), per-attribute-value (the `TimeStamp` in `Attributes`), and whole-list for `SetExportedNodes` via a sidecar timestamp in `Registry.attributes` under `library::exported_nodes_ts`. The timestamp driving every LWW arm comes from the wrapping `Delta`; `AttributeDelta` carries `value: Option<_>` so a single shape covers both `Set` (`Some`) and `Remove` (`None`) and `Set` vs. `Remove` has a defined winner.

The CRDT does not mask graph-shape conflicts. Concurrent same-slot `SetExport`s with different targets resolve by LWW, but the resulting wiring may be wrong; downstream consumers see it as a compile or wiring error.

## History storage

`HashMap<Rev, Delta>` plus a `head: Rev` (the local cursor; advances only on local commits) and a `hot_log: Vec<HotOp>` (in-flight unretired ops). Walking history follows `delta.parents`; the default walk follows the first parent to reconstruct a single peer's local chain. Branches are siblings under a shared parent; merges aren't modeled as nodes — they're implicit in a delta listing multiple parents.

## Editor metadata

`DocumentNodePersistentMetadata` and `NodeNetworkPersistentMetadata` from the runtime — display names, locked/pinned, navigation/PTZ state, selection undo/redo stacks, layer/node type metadata — flow through the storage `Attributes` bucket under `ui::*` keys. Transient runtime caches (`DocumentNodeTransientMetadata`, click targets, resolved types, `OriginalLocation`) stay runtime-only and are not stored.

# Drawbacks

- **Diffing two full `Registry`s on every autosave is O(N) in document size.** The interim cost of treating storage as a serialization layer derived from the runtime; currently triggered at autosave boundaries (`commit_storage_snapshot`) rather than per gesture, and addressed long-term by computing deltas directly on runtime mutations.
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
