use std::ops::Deref;
use std::sync::Arc;

// Blake3 content hash of a resource
pub type ResourceHash = [u8; 32];

#[derive(Clone)]
pub struct Resource {
	inner: Arc<dyn AsRef<[u8]> + Send + Sync>,
}

impl Resource {
	pub fn new<T: AsRef<[u8]> + Send + Sync + 'static>(data: T) -> Self {
		Self { inner: Arc::new(data) }
	}

	pub fn from_arc(inner: Arc<dyn AsRef<[u8]> + Send + Sync>) -> Self {
		Self { inner }
	}
}

impl Deref for Resource {
	type Target = [u8];

	fn deref(&self) -> &[u8] {
		(*self.inner).as_ref()
	}
}

impl AsRef<[u8]> for Resource {
	fn as_ref(&self) -> &[u8] {
		(*self.inner).as_ref()
	}
}

impl std::fmt::Debug for Resource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Resource").field("len", &self.len()).finish()
	}
}

pub trait ResourceStorage {
	fn read(&mut self, hash: &ResourceHash) -> Option<Resource>;
	fn write(&mut self, data: &[u8]) -> ResourceHash;
	fn contains(&mut self, hash: &ResourceHash) -> bool;
}
