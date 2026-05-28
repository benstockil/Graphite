use gdd_container::backends::folder::FolderBackend;
use gdd_container::backends::memory::MemoryBackend;
use gdd_container::{Container, ContainerError};

fn run_round_trip<C: Container>(mut container: C) {
	container.write("manifest.json", br#"{"format":"gdd"}"#).unwrap();
	container.write("resources/abc123", &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
	container.write("resources/xyz789", b"another resource").unwrap();

	assert!(container.exists("manifest.json"));
	assert!(container.exists("resources/abc123"));
	assert!(!container.exists("does-not-exist"));

	let manifest = container.read("manifest.json").unwrap();
	assert_eq!(manifest.as_slice(), br#"{"format":"gdd"}"#);

	let blob = container.read("resources/abc123").unwrap();
	assert_eq!(blob.as_slice(), &[0xDE, 0xAD, 0xBE, 0xEF]);

	let top_level = container.list("").unwrap();
	assert!(top_level.iter().any(|p| p == "manifest.json"));
	assert!(
		top_level.iter().all(|p| !p.starts_with("resources/")),
		"list(\"\") must not descend into subdirectories, got {top_level:?}"
	);

	let mut resources = container.list("resources").unwrap();
	resources.sort();
	assert_eq!(resources, vec!["resources/abc123".to_string(), "resources/xyz789".to_string()]);

	container.remove("resources/abc123").unwrap();
	assert!(!container.exists("resources/abc123"));
	assert!(matches!(container.read("resources/abc123"), Err(ContainerError::NotFound(_))));
}

#[test]
fn memory_backend_round_trip() {
	run_round_trip(MemoryBackend::new());
}

#[test]
fn folder_backend_round_trip() {
	let dir = tempfile::tempdir().unwrap();
	let backend = FolderBackend::create(dir.path()).unwrap();
	run_round_trip(backend);
}

#[test]
fn folder_backend_rejects_path_traversal() {
	let dir = tempfile::tempdir().unwrap();
	let mut backend = FolderBackend::create(dir.path()).unwrap();
	for bad in ["../escape", "subdir/../escape", "/abs", "back\\slash"] {
		let result = backend.write(bad, b"nope");
		assert!(matches!(result, Err(ContainerError::InvalidPath(_))), "expected InvalidPath for {bad:?}, got {result:?}");
	}
}

#[test]
fn folder_backend_write_sized_fills_via_mmap() {
	let dir = tempfile::tempdir().unwrap();
	let mut backend = FolderBackend::create(dir.path()).unwrap();

	let payload = b"hello world";
	backend
		.write_sized("resources/sized", payload.len(), &mut |buffer| {
			buffer.copy_from_slice(payload);
		})
		.unwrap();

	let read_back = backend.read("resources/sized").unwrap();
	assert_eq!(read_back.as_slice(), payload);
}
