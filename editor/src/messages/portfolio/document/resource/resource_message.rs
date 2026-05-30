use crate::messages::prelude::*;
use graph_craft::application_io::resource::ResourceId;
use graph_craft::document::NodeId;
use graphene_std::text::Font;
use std::sync::Arc;

#[impl_message(Message, DocumentMessage, Resource)]
#[derive(PartialEq, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ResourceMessage {
	/// Insert content-addressed bytes into storage and resolve the registry id to them. Used for embedded payloads.
	StoreEmbedded {
		resource_id: ResourceId,
		data: Arc<[u8]>,
	},
	/// Replace the font input on a text node with a freshly minted `Resource(id)` whose registry source is
	/// `DataSource::Font { family, style }`. Auto-enqueues `Resolve` so the fetch (or cache hit) follows.
	SetFont {
		node_id: NodeId,
		font: Font,
	},
	/// Walk every unresolved id in the registry and dispatch a `ResolveStep` per id.
	Resolve,
	/// Try the next `DataSource` for the given id (Embedded/Url/Font). May resolve in-place via the fonts cache,
	/// trigger a frontend URL fetch, or request the catalog and retry.
	ResolveStep {
		resource_id: ResourceId,
	},
	/// The frontend (via `PortfolioMessage::ResourceResolved`) returned bytes for an id. Store them, mark the id
	/// resolved in the registry, and (if the source was a `DataSource::Font`) notify the fonts handler.
	Resolved {
		resource_id: ResourceId,
		data: Arc<[u8]>,
	},
}
