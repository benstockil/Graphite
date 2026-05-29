//! Persistent cursor state for the local peer. Separate from [`crate::Manifest`] because the
//! manifest describes document identity (what this document *is*), while [`SessionState`]
//! describes where the local peer's cursor sits inside it.
//!
//! Lives in `session.json`. Rewritten on retirement.

use graph_storage::Rev;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct SessionState {
	/// Local-chain cursor. Points at the most recently applied retired delta.
	#[serde(default)]
	pub head_rev: Rev,
	/// Shared-monotonic counter feeding `Document::next_node_id`. Persisted so reopens don't
	/// collide on minted IDs.
	#[serde(default)]
	pub next_node_counter: u64,
}
