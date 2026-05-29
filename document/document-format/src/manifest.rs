//! Bootstrap file for a `.gdd` document. Always JSON regardless of payload codec choice.

use graph_storage::PeerId;
use serde::{Deserialize, Serialize};

/// Magic string carried in [`Manifest::format`] to identify a `.gdd` document.
pub const FORMAT_MAGIC: &str = "gdd";

/// Maximum manifest version this build can open. Bumped when manifest layout changes
/// in a way that older builds can't safely read.
pub const SUPPORTED_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
	pub format: String,
	pub format_version: u32,
	pub document_uuid: u64,
	pub peer_id: PeerId,
	pub editor_version: String,
	pub stdlib_version: String,
	/// RFC 3339 timestamp of the most recent retirement, set by [`crate::Gdd::retire`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub last_retired_at: Option<String>,
}

impl Manifest {
	pub fn new(document_uuid: u64, peer_id: PeerId, editor_version: String, stdlib_version: String) -> Self {
		Self {
			format: FORMAT_MAGIC.to_string(),
			format_version: SUPPORTED_FORMAT_VERSION,
			document_uuid,
			peer_id,
			editor_version,
			stdlib_version,
			last_retired_at: None,
		}
	}
}
