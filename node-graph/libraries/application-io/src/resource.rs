use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// Blake3 content hash of a resource
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ResourceHash([u8; 32]);

impl ResourceHash {
	pub const fn new(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}

	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	pub fn to_hex(&self) -> String {
		const HEX: &[u8; 16] = b"0123456789abcdef";
		let mut out = String::with_capacity(self.0.len() * 2);
		for byte in &self.0 {
			out.push(HEX[(byte >> 4) as usize] as char);
			out.push(HEX[(byte & 0x0f) as usize] as char);
		}
		out
	}
}

impl From<[u8; 32]> for ResourceHash {
	fn from(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}
}

impl From<blake3::Hash> for ResourceHash {
	fn from(hash: blake3::Hash) -> Self {
		Self(hash.into())
	}
}

impl From<ResourceHash> for [u8; 32] {
	fn from(hash: ResourceHash) -> Self {
		hash.0
	}
}

impl From<ResourceHash> for String {
	fn from(hash: ResourceHash) -> Self {
		hash.to_hex()
	}
}

impl fmt::Display for ResourceHash {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.to_hex())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceHashParseError {
	InvalidLength { found: usize },
	InvalidCharacter { byte: u8, position: usize },
}

impl fmt::Display for ResourceHashParseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidLength { found } => write!(f, "resource hash must be 64 hex characters, got {found}"),
			Self::InvalidCharacter { byte, position } => write!(f, "resource hash contains non-hex byte {byte:#04x} at position {position}"),
		}
	}
}

impl std::error::Error for ResourceHashParseError {}

impl TryFrom<&str> for ResourceHash {
	type Error = ResourceHashParseError;

	fn try_from(value: &str) -> Result<Self, Self::Error> {
		let bytes = value.as_bytes();
		if bytes.len() != 64 {
			return Err(ResourceHashParseError::InvalidLength { found: bytes.len() });
		}

		let mut out = [0u8; 32];
		for (index, chunk) in bytes.chunks_exact(2).enumerate() {
			let high = decode_hex_nibble(chunk[0], index * 2)?;
			let low = decode_hex_nibble(chunk[1], index * 2 + 1)?;
			out[index] = (high << 4) | low;
		}

		Ok(Self(out))
	}
}

fn decode_hex_nibble(byte: u8, position: usize) -> Result<u8, ResourceHashParseError> {
	match byte {
		b'0'..=b'9' => Ok(byte - b'0'),
		b'a'..=b'f' => Ok(byte - b'a' + 10),
		b'A'..=b'F' => Ok(byte - b'A' + 10),
		_ => Err(ResourceHashParseError::InvalidCharacter { byte, position }),
	}
}

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

impl fmt::Debug for Resource {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Resource").field("len", &self.len()).finish()
	}
}

pub trait ResourceStorage: Send {
	fn read(&mut self, hash: &ResourceHash) -> Option<Resource>;
	fn write(&mut self, data: &[u8]) -> ResourceHash;
	fn contains(&mut self, hash: &ResourceHash) -> bool;
}
