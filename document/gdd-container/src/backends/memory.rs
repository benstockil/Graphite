//! In-memory backend. Useful for tests and as the deserialize target for archive codecs.

use crate::{ByteHolder, Container, ContainerError, Result};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct MemoryBackend {
	files: HashMap<String, Vec<u8>>,
}

impl MemoryBackend {
	pub fn new() -> Self {
		Self::default()
	}
}

impl Container for MemoryBackend {
	fn read(&self, path: &str) -> Result<ByteHolder> {
		self.files
			.get(path)
			.map(|bytes| ByteHolder::Owned(bytes.clone()))
			.ok_or_else(|| ContainerError::NotFound(path.to_string()))
	}

	fn write(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
		self.files.insert(path.to_string(), bytes.to_vec());
		Ok(())
	}

	fn list(&self, prefix: &str) -> Result<Vec<String>> {
		let normalized = normalize_prefix(prefix);
		let results = self
			.files
			.keys()
			.filter(|path| path.starts_with(&normalized) && !path[normalized.len()..].contains('/'))
			.cloned()
			.collect();
		Ok(results)
	}

	fn list_dirs(&self, prefix: &str) -> Result<Vec<String>> {
		let normalized = normalize_prefix(prefix);
		let mut seen = HashSet::new();
		let mut results = Vec::new();
		for path in self.files.keys() {
			if !path.starts_with(&normalized) {
				continue;
			}
			let remainder = &path[normalized.len()..];
			if let Some((segment, _)) = remainder.split_once('/') {
				let dir = format!("{normalized}{segment}");
				if seen.insert(dir.clone()) {
					results.push(dir);
				}
			}
		}
		Ok(results)
	}

	fn exists(&self, path: &str) -> bool {
		self.files.contains_key(path)
	}

	fn remove(&mut self, path: &str) -> Result<()> {
		self.files.remove(path).map(|_| ()).ok_or_else(|| ContainerError::NotFound(path.to_string()))
	}
}

fn normalize_prefix(prefix: &str) -> String {
	if prefix.is_empty() || prefix.ends_with('/') { prefix.to_string() } else { format!("{prefix}/") }
}
