#![cfg(any(feature = "zip", feature = "xz"))]

use gdd_container::Container;
use gdd_container::archive::Archive;
use gdd_container::backends::memory::MemoryBackend;

fn make_source() -> MemoryBackend {
	let mut backend = MemoryBackend::new();
	backend.write("manifest.json", br#"{"format":"gdd"}"#).unwrap();
	backend.write("document.json", b"{\"registry\":\"...\"}").unwrap();
	backend.write("history.jsonl", b"{\"rev\":1}\n{\"rev\":2}\n").unwrap();
	backend.write("resources/abc123", &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
	backend.write("resources/xyz789", b"another resource").unwrap();
	backend
}

fn assert_round_trip(restored: &MemoryBackend) {
	assert_eq!(restored.read("manifest.json").unwrap().as_slice(), br#"{"format":"gdd"}"#);
	assert_eq!(restored.read("document.json").unwrap().as_slice(), b"{\"registry\":\"...\"}");
	assert_eq!(restored.read("history.jsonl").unwrap().as_slice(), b"{\"rev\":1}\n{\"rev\":2}\n");
	assert_eq!(restored.read("resources/abc123").unwrap().as_slice(), &[0xDE, 0xAD, 0xBE, 0xEF]);
	assert_eq!(restored.read("resources/xyz789").unwrap().as_slice(), b"another resource");
}

#[cfg(feature = "zip")]
#[test]
fn zip_round_trip() {
	use gdd_container::archive::Zip;
	let src = make_source();
	let bytes = futures::executor::block_on(<Zip as Archive>::serialize_from(&src)).unwrap();
	let restored = <Zip as Archive>::deserialize(&bytes).unwrap();
	assert_round_trip(&restored);
}

#[cfg(feature = "xz")]
#[test]
fn xz_round_trip() {
	use gdd_container::archive::Xz;
	let src = make_source();
	let bytes = futures::executor::block_on(<Xz as Archive>::serialize_from(&src)).unwrap();
	let restored = <Xz as Archive>::deserialize(&bytes).unwrap();
	assert_round_trip(&restored);
}
