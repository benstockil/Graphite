use graphene_application_io::{Resource, ResourceHash, ResourceStorage};
use std::collections::HashMap;
use std::sync::Arc;

/// In-memory `ResourceStorage` backed by a `HashMap`, hashing data with blake3 on write.
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
	fn read(&self, hash: &ResourceHash) -> Option<Resource> {
		self.resources.get(hash).cloned()
	}

	fn write(&mut self, data: &[u8]) -> ResourceHash {
		let hash = blake3::hash(data).into();
		self.resources.insert(hash, Resource::new(Arc::<[u8]>::from(data)));
		hash
	}

	fn contains(&self, hash: &ResourceHash) -> bool {
		self.resources.contains_key(hash)
	}
}
