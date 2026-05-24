use core_types::uuid::NodeId as RuntimeNodeId;
use graph_craft::concrete;
use graph_craft::document::{DocumentNode, DocumentNodeImplementation, NodeInput, NodeNetwork};
use graph_craft::{ProtoNodeIdentifier, Type};

use crate::{Document, HotOp, NoMetadata, NodeId, PeerId, RegistryDelta, Session, TimeStamp};

fn fresh_document(peer: PeerId) -> Document {
	Session::with_peer(peer).document
}

fn remove_node_op(node_id: NodeId) -> RegistryDelta {
	RegistryDelta::RemoveNode { node_id }
}

/// Every applied op must advance the local clock past the op's timestamp, so any subsequent
/// local tick is causally later than what we just observed. Locks in the invariant that
/// `apply_op` calls `clock.observe`, regardless of which apply entry point was used.
#[test]
fn apply_hot_op_advances_clock_past_observed_timestamp() {
	let mut document = fresh_document(PeerId(1));
	assert_eq!(document.clock.counter, 0);

	let observed = TimeStamp { counter: 42, peer: PeerId(2) };
	let hot_op = HotOp {
		op: remove_node_op(99),
		timestamp: observed,
		author: PeerId(2),
	};

	document.apply_hot_op(hot_op).expect("RemoveNode on absent node is a no-op, not an error");

	assert!(
		document.clock.counter >= observed.counter,
		"clock counter {} did not advance past observed counter {}",
		document.clock.counter,
		observed.counter
	);

	let next = document.clock.tick();
	assert!(
		next.counter > observed.counter,
		"next tick {} must be strictly later than the observed timestamp {}",
		next.counter,
		observed.counter
	);
}

/// `next_node_id` must never repeat across successive calls on the same document. The blake3 output
/// space is enormous, so any collision in a small loop is a counter-bumping bug, not a hash
/// collision.
#[test]
fn next_node_id_is_unique_within_a_document() {
	let mut document = fresh_document(PeerId(1));

	let mut seen = std::collections::HashSet::new();
	for _ in 0..1000 {
		let id = document.next_node_id();
		assert!(seen.insert(id), "next_node_id repeated after {} calls", seen.len());
	}
}

/// Two peers reading the same shared counter must produce different `NodeId`s. This is the whole
/// reason the counter can be shared across peers instead of being per-peer.
#[test]
fn next_node_id_differs_across_peers_at_same_counter() {
	let mut document_a = fresh_document(PeerId(1));
	let mut document_b = fresh_document(PeerId(2));

	let id_a = document_a.next_node_id();
	let id_b = document_b.next_node_id();
	assert_ne!(id_a, id_b, "peer-scoping is broken: two peers minted the same NodeId at counter 1");
}

fn tiny_network() -> NodeNetwork {
	NodeNetwork {
		exports: vec![NodeInput::node(RuntimeNodeId(0), 0)],
		nodes: [(
			RuntimeNodeId(0),
			DocumentNode {
				inputs: vec![NodeInput::import(concrete!(u32), 0)],
				implementation: DocumentNodeImplementation::ProtoNode(ProtoNodeIdentifier::new("graphene_core::ops::identity::IdentityNode")),
				..Default::default()
			},
		)]
		.into_iter()
		.collect(),
		..Default::default()
	}
}

/// Committing the same NodeNetwork twice must produce zero history entries on the second commit.
/// Without value-only diffing in compute_deltas, the second commit would emit spurious
/// ChangeNodeInput / ChangeNodeAttribute ops because self.registry has real timestamps while the
/// freshly-built `to` registry has TimeStamp::ORIGIN.
#[test]
fn commit_from_runtime_is_idempotent_for_unchanged_network() {
	let mut session = Session::with_peer(PeerId(1));
	let network = tiny_network();

	let first = session.commit_from_runtime(&network, &NoMetadata).expect("first commit failed");
	assert!(!first.is_empty(), "first commit should produce at least one delta for the initial network");

	let second = session.commit_from_runtime(&network, &NoMetadata).expect("second commit failed");
	assert_eq!(second.len(), 0, "second commit of unchanged network produced {} spurious deltas: {:?}", second.len(), second);
}

/// Erroring ops still bump the clock: we observed the timestamp on the wire, the fact that the
/// op was rejected locally doesn't unobserve it.
#[test]
fn apply_op_advances_clock_even_when_op_errors() {
	let mut document = fresh_document(PeerId(1));

	let observed = TimeStamp { counter: 17, peer: PeerId(2) };
	let failing_op = RegistryDelta::ChangeNodeInput {
		node_id: 7,
		input_idx: 0,
		new_input: crate::NodeInput::Import { import_idx: 0 },
	};

	let _ = document.apply_op(failing_op, observed, false);

	assert!(document.clock.counter >= observed.counter, "clock should advance on observation even when the op errors");
}
