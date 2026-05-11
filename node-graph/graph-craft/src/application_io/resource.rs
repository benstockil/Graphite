#[cfg(target_family = "wasm")]
pub mod indexed_db;
#[cfg(not(target_family = "wasm"))]
pub mod mmap;

use graphene_application_io::{Resource, ResourceHash, ResourceStorage};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default, Clone)]
pub struct HashMapResourceStorage {
	resources: HashMap<ResourceHash, Resource>,
}

impl HashMapResourceStorage {
	pub fn new() -> Self {
		Self::default()
	}
}

impl ResourceStorage for HashMapResourceStorage {
	fn read(&mut self, hash: &ResourceHash) -> Option<Resource> {
		self.resources.get(hash).cloned()
	}

	fn write(&mut self, data: &[u8]) -> ResourceHash {
		let hash = ResourceHash::from(data);
		self.resources.insert(hash, Resource::new(Arc::<[u8]>::from(data)));
		hash
	}

	fn contains(&mut self, hash: &ResourceHash) -> bool {
		self.resources.contains_key(hash)
	}
}
