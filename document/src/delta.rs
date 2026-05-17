use std::collections::{HashMap, HashSet};

use crate::{AttributeDelta, ExportSlot, NetworkId, Node, NodeId, NodeInput, Registry, RegistryDelta, TimeStamp};

/// Computes the minimal set of deltas to transform `from` into `to`
pub fn compute_deltas(from: &Registry, to: &Registry) -> Vec<RegistryDelta> {
	let mut deltas = Vec::new();

	// Find all node IDs in both registries
	let from_node_ids: HashSet<NodeId> = from.node_instances.keys().copied().collect();
	let to_node_ids: HashSet<NodeId> = to.node_instances.keys().copied().collect();

	// 1. Find removed nodes
	for &node_id in from_node_ids.difference(&to_node_ids) {
		deltas.push(RegistryDelta::RemoveNode { node_id });
	}

	// 2. Find added nodes
	for &node_id in to_node_ids.difference(&from_node_ids) {
		let node = to.node_instances[&node_id].clone();
		deltas.push(RegistryDelta::AddNode { node_id, node });
	}

	// 3. Find modified nodes (nodes that exist in both)
	for &node_id in from_node_ids.intersection(&to_node_ids) {
		let from_node = &from.node_instances[&node_id];
		let to_node = &to.node_instances[&node_id];

		// If implementation changed, we need to remove and re-add the node
		// (since we don't have a ChangeImplementation delta variant)
		if !nodes_have_same_implementation(from_node, to_node) {
			deltas.push(RegistryDelta::RemoveNode { node_id });
			deltas.push(RegistryDelta::AddNode { node_id, node: to_node.clone() });
			continue;
		}

		// Check for input changes. The diff path carries forward the slot's existing timestamp;
		// callers that need a fresh clock-tick must mint timestamps themselves.
		for (input_idx, (from_slot, to_slot)) in from_node.inputs.iter().zip(to_node.inputs.iter()).enumerate() {
			if from_slot != to_slot {
				deltas.push(RegistryDelta::ChangeNodeInput {
					node_id,
					input_idx,
					new_input: to_slot.input.clone(),
					timestamp: to_slot.timestamp,
				});
			}
		}

		// Handle input count changes
		if from_node.inputs.len() != to_node.inputs.len() {
			// If input count changed, we need to remove and re-add the node
			deltas.push(RegistryDelta::RemoveNode { node_id });
			deltas.push(RegistryDelta::AddNode { node_id, node: to_node.clone() });
			continue;
		}

		// Check for attribute changes
		let attribute_deltas = compute_attribute_deltas(&from_node.attributes, &to_node.attributes);
		for delta in attribute_deltas {
			deltas.push(RegistryDelta::ChangeNodeAttribute { node_id, delta });
		}

		// Check for input attribute changes
		for (input_idx, (from_attrs, to_attrs)) in from_node.inputs_attributes.iter().zip(to_node.inputs_attributes.iter()).enumerate() {
			let input_attr_deltas = compute_attribute_deltas(from_attrs, to_attrs);
			for delta in input_attr_deltas {
				deltas.push(RegistryDelta::ChangeNodeInputAttribute { node_id, input_idx, delta });
			}
		}
	}

	// 4. Handle network changes.
	let from_network_ids: HashSet<NetworkId> = from.networks.keys().copied().collect();
	let to_network_ids: HashSet<NetworkId> = to.networks.keys().copied().collect();

	// Disappeared networks: snapshot the `from` state so the reverse can rebuild.
	for &network_id in from_network_ids.difference(&to_network_ids) {
		let snapshot = from.networks[&network_id].clone();
		deltas.push(RegistryDelta::RemoveNetwork { network: network_id, snapshot });
	}

	// New networks: emit `AddNetwork` carrying the full contents. Per-slot diffs only run for
	// networks present in both sides; new ones are added wholesale.
	for &network_id in to_network_ids.difference(&from_network_ids) {
		let contents = to.networks[&network_id].clone();
		deltas.push(RegistryDelta::AddNetwork { network: network_id, contents });
	}

	// Per-slot diff for networks present in both.
	for &network_id in from_network_ids.intersection(&to_network_ids) {
		let from_network = &from.networks[&network_id];
		let to_network = &to.networks[&network_id];

		let max_len = from_network.exports.len().max(to_network.exports.len());
		for slot_idx in 0..max_len {
			let from_slot = from_network.exports.get(slot_idx);
			let to_slot = to_network.exports.get(slot_idx);

			if from_slot != to_slot {
				let (target, timestamp) = to_slot.map(|s| (s.target.clone(), s.timestamp)).unwrap_or((None, TimeStamp::ORIGIN));
				deltas.push(RegistryDelta::SetExport {
					network: network_id,
					slot: slot_idx as u32,
					target,
					timestamp,
				});
			}
		}
	}

	deltas
}

/// Check if two nodes have the same implementation
fn nodes_have_same_implementation(a: &Node, b: &Node) -> bool {
	match (&a.implementation, &b.implementation) {
		(crate::Implementation::ProtoNode(a_id), crate::Implementation::ProtoNode(b_id)) => a_id == b_id,
		(crate::Implementation::Network(a_id), crate::Implementation::Network(b_id)) => a_id == b_id,
		_ => false,
	}
}

/// Compute attribute deltas between two attribute maps.
///
/// The diff carries forward the timestamps already present on the `to` side. Removals are
/// stamped with `TimeStamp::ORIGIN` since the diff path has no clock; callers that need
/// causally-ordered removes must mint the timestamp themselves.
fn compute_attribute_deltas(from: &crate::Attributes, to: &crate::Attributes) -> Vec<AttributeDelta> {
	let mut deltas = Vec::new();

	let from_keys: HashSet<&String> = from.keys().collect();
	let to_keys: HashSet<&String> = to.keys().collect();

	for key in from_keys.difference(&to_keys) {
		deltas.push(AttributeDelta::Remove {
			key: (*key).clone(),
			timestamp: TimeStamp::ORIGIN,
		});
	}

	for key in &to_keys {
		let to_value = &to[*key];
		let changed = from.get(*key).is_none_or(|from_value| from_value != to_value);
		if changed {
			deltas.push(AttributeDelta::Set {
				key: (*key).clone(),
				value: to_value.value.clone(),
				timestamp: to_value.timestamp,
			});
		}
	}

	deltas
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Implementation, Network, Node, ProtoNode};

	#[test]
	fn test_compute_deltas_empty() {
		let registry = Registry::default();

		let deltas = compute_deltas(&registry, &registry);
		assert_eq!(deltas.len(), 0, "No deltas should be generated for identical registries");
	}

	#[test]
	fn test_compute_deltas_add_node() {
		let from = Registry::default();

		let mut to = from.clone();
		let node = Node {
			implementation: Implementation::ProtoNode(1),
			inputs: vec![],
			inputs_attributes: vec![],
			attributes: HashMap::new(),
			network: 0,
		};
		to.node_instances.insert(42, node);

		let deltas = compute_deltas(&from, &to);
		assert_eq!(deltas.len(), 1);
		assert!(matches!(deltas[0], RegistryDelta::AddNode { node_id: 42, .. }));
	}

	#[test]
	fn test_compute_deltas_remove_node() {
		let mut from = Registry::default();

		let node = Node {
			implementation: Implementation::ProtoNode(1),
			inputs: vec![],
			inputs_attributes: vec![],
			attributes: HashMap::new(),
			network: 0,
		};
		from.node_instances.insert(42, node);

		let to = Registry::default();

		let deltas = compute_deltas(&from, &to);
		assert_eq!(deltas.len(), 1);
		assert!(matches!(deltas[0], RegistryDelta::RemoveNode { node_id: 42 }));
	}

	#[test]
	fn test_compute_deltas_modify_attribute() {
		let mut from = Registry::default();

		let mut node = Node {
			implementation: Implementation::ProtoNode(1),
			inputs: vec![],
			inputs_attributes: vec![],
			attributes: HashMap::new(),
			network: 0,
		};
		let stamp = |counter: u64| TimeStamp { counter, peer: crate::PeerId(0) };
		node.attributes.insert(
			"test".to_string(),
			crate::Value {
				value: serde_json::json!("old"),
				timestamp: stamp(0),
			},
		);
		from.node_instances.insert(42, node);

		let mut to = from.clone();
		to.node_instances.get_mut(&42).unwrap().attributes.insert(
			"test".to_string(),
			crate::Value {
				value: serde_json::json!("new"),
				timestamp: stamp(1),
			},
		);

		let deltas = compute_deltas(&from, &to);
		assert_eq!(deltas.len(), 1);
		assert!(matches!(
			&deltas[0],
			RegistryDelta::ChangeNodeAttribute { node_id: 42, delta: AttributeDelta::Set { key, .. } } if key == "test"
		));
	}

	#[test]
	fn test_compute_deltas_network_changes() {
		let make_slot = |id: u64| ExportSlot {
			target: Some(NodeInput::Node { node_id: id, output_index: 0 }),
			timestamp: TimeStamp::ORIGIN,
		};

		let mut from = Registry::default();
		from.networks.insert(
			0,
			Network {
				exports: vec![make_slot(1), make_slot(2)],
			},
		);

		let mut to = from.clone();
		to.networks.get_mut(&0).unwrap().exports.push(make_slot(3));

		let deltas = compute_deltas(&from, &to);
		// Only slot 2 changed (added). Slots 0 and 1 are unchanged so they don't emit ops.
		assert_eq!(deltas.len(), 1);
		assert!(matches!(
			&deltas[0],
			RegistryDelta::SetExport {
				network: 0,
				slot: 2,
				target: Some(NodeInput::Node { node_id: 3, .. }),
				..
			}
		));
	}
}
