use crate::messages::portfolio::fonts::utility_types::FontCatalog;
use crate::messages::prelude::*;
use graph_craft::application_io::resource::ResourceHash;
use std::sync::Arc;

#[impl_message(Message, PortfolioMessage, Fonts)]
#[derive(PartialEq, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum FontsMessage {
	/// The font catalog (family/style/url listing) was loaded by the frontend.
	CatalogLoaded {
		catalog: FontCatalog,
	},
	/// A font resource was just resolved by the per-document resource handler — record its content hash.
	ResourceResolved {
		family: String,
		style: Option<String>,
		hash: ResourceHash,
	},
	/// Ensure the editor-side byte cache holds this font (for measurement / textbox). No-op when the
	/// font hash is not known yet. Fires `response` (if any) once the bytes are in the cache.
	Load {
		family: String,
		style: Option<String>,
		response: Option<Box<Message>>,
	},
	/// Internal: the storage `Await` future has yielded bytes; insert them and fire the queued response.
	Cached {
		hash: ResourceHash,
		#[serde(skip)]
		data: Arc<[u8]>,
		response: Option<Box<Message>>,
	},
}
