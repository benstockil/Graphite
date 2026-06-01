//! CLI: round-trips a `.graphite` file through `NodeNetwork → Registry → NodeNetwork`.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use graph_craft::document::NodeNetwork;
use graph_storage::Registry;

fn main() -> ExitCode {
	if let Err(error) = run() {
		eprintln!("{error}");
		return ExitCode::FAILURE;
	}
	ExitCode::SUCCESS
}

fn run() -> Result<(), String> {
	let args: Vec<String> = std::env::args().collect();
	let [_, input, output] = args.as_slice() else {
		return Err(format!("Usage: {} <input.graphite> <output.graphite>", args.first().map(String::as_str).unwrap_or("round_trip")));
	};

	let input_path = PathBuf::from(input);
	let output_path = PathBuf::from(output);

	println!("Loading artwork from: {}", input_path.display());

	let json_content = fs::read_to_string(&input_path).map_err(|e| format!("Error reading input: {e}"))?;
	let mut doc: serde_json::Value = serde_json::from_str(&json_content).map_err(|e| format!("Error parsing JSON: {e}"))?;

	let original_network: NodeNetwork = serde_json::from_value(doc["network_interface"]["network"].clone()).map_err(|e| format!("Error deserializing NodeNetwork: {e}"))?;
	println!("Original network: {} nodes", original_network.nodes.len());

	// No byte store here: keep the extracted declaration bytes in hand and rebuild the `Declarations`
	// map from them so the back-conversion can resolve proto-node identifiers.
	let conversion =
		Registry::convert_from_runtime(&original_network, &graph_storage::NoMetadata, &Default::default(), graph_storage::PeerId(0)).map_err(|e| format!("Error converting to Registry: {e}"))?;
	let declarations = conversion.declarations().map_err(|e| format!("Error rebuilding declarations: {e}"))?;
	let registry = conversion.registry;
	println!("Registry: {} node instances, {} networks", registry.node_instances.len(), registry.networks.len());

	let mut node_ids: Vec<_> = registry.node_instances.keys().copied().collect();
	node_ids.sort();
	println!("Registry node IDs: {node_ids:?}");

	let (converted_network, _entries) = registry.to_runtime_with_metadata(&declarations).map_err(|e| format!("Error converting back to NodeNetwork: {e}"))?;
	println!("Converted network: {} nodes", converted_network.nodes.len());

	doc["network_interface"]["network"] = serde_json::to_value(&converted_network).map_err(|e| format!("Error serializing converted network: {e}"))?;
	let output_json = serde_json::to_string_pretty(&doc).map_err(|e| format!("Error serializing output JSON: {e}"))?;
	fs::write(&output_path, output_json).map_err(|e| format!("Error writing output: {e}"))?;

	println!("Wrote round-tripped artwork to: {}", output_path.display());
	Ok(())
}
