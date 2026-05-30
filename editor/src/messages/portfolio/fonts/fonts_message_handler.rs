use crate::messages::portfolio::fonts::FALLBACK_FONT_BLOB;
use crate::messages::portfolio::fonts::utility_types::FontCatalog;
use crate::messages::prelude::*;
use graph_craft::application_io::resource::{DataSource, Resource, ResourceHash, ResourceId, ResourceRegistry};
use graphene_std::text::{Blob, Font};
use std::sync::Arc;

#[derive(ExtractField)]
pub struct FontsMessageContext<'a> {
	pub resource_storage: &'a ResourceStorageMessageHandler,
}

/// Central font index: catalog + content-addressed cache of font bytes used for editor-side measurement
/// and the editable-textbox. Document rendering still goes through the per-document resource registry; the
/// fonts handler only learns about a font's hash when [`FontsMessage::ResourceResolved`] is dispatched by
/// the per-document [`ResourceMessageHandler`].
#[derive(Debug, Default, ExtractField)]
pub struct FontsMessageHandler {
	pub font_catalog: FontCatalog,
	/// `Font → ResourceHash` learned each time a font-sourced resource resolves in any document.
	font_hashes: HashMap<Font, ResourceHash>,
	/// Lazy `ResourceHash → bytes` cache; filled on demand by the `Await`-backed `Load` path.
	font_data: HashMap<ResourceHash, Resource>,
}

#[message_handler_data]
impl MessageHandler<FontsMessage, FontsMessageContext<'_>> for FontsMessageHandler {
	fn process_message(&mut self, message: FontsMessage, responses: &mut VecDeque<Message>, context: FontsMessageContext) {
		let FontsMessageContext { resource_storage } = context;

		match message {
			FontsMessage::CatalogLoaded { catalog } => {
				self.font_catalog = catalog;

				// Any `ResolveStep` that gave up because the catalog wasn't loaded yet can now run; rebroadcast
				// `Resolve` so every document re-walks its unresolved ids with URLs available.
				responses.add(PortfolioMessage::ResolveAllResources);
			}
			FontsMessage::ResourceResolved { family, style, hash } => {
				let font = font_from_pair(&family, style.as_deref());
				self.font_hashes.insert(font, hash);
			}
			FontsMessage::Load { family, style, response } => {
				let font = self.normalize(font_from_pair(&family, style.as_deref()));
				let Some(hash) = self.font_hashes.get(&font).copied() else {
					log::warn!("FontsMessage::Load for {font:?} with no known hash; ignoring");
					return;
				};
				if self.font_data.contains_key(&hash) {
					if let Some(response) = response {
						responses.add(*response);
					}
					return;
				}
				let loader = resource_storage.resources();
				responses.add(FrontendMessage::Await {
					future: async move {
						let data = loader.load(hash).await.map(|resource| {
							let bytes: Arc<[u8]> = Arc::from(resource.as_ref());
							bytes
						});
						match data {
							Some(data) => FontsMessage::Cached { hash, data, response }.into(),
							None => {
								log::warn!("Storage missing data for font hash {hash}");
								// If `response` was requested, still fire it so callers don't deadlock; otherwise no-op.
								response.map(|r| *r).unwrap_or(Message::NoOp)
							}
						}
					}
					.into(),
				});
			}
			FontsMessage::Cached { hash, data, response } => {
				self.font_data.insert(hash, Resource::new(data.to_vec()));
				if let Some(response) = response {
					responses.add(*response);
				}
			}
		}
	}

	advertise_actions!(FontsMessageDiscriminant;);
}

impl FontsMessageHandler {
	/// The content hash recorded for a font's family/style, if any document has loaded it.
	pub fn cached_hash(&self, family: &str, style: Option<&str>) -> Option<ResourceHash> {
		self.font_hashes.get(&font_from_pair(family, style)).copied()
	}

	/// The download URL for a font's family/style according to the catalog.
	pub fn cached_url(&self, family: &str, style: Option<&str>) -> Option<String> {
		self.font_catalog.cached_url(family, style)
	}

	/// Returns the cached font blob if loaded; otherwise queues a [`FontsMessage::Load`] (when the hash is
	/// known) and returns the embedded fallback so measurement degrades gracefully instead of failing.
	pub fn get_blob_or_queue_load(&self, font: &Font, responses: &mut VecDeque<Message>) -> Blob<u8> {
		let style = Some(font.font_style.as_str());
		if let Some(hash) = self.font_hashes.get(font) {
			if let Some(resource) = self.font_data.get(hash) {
				return Blob::new(resource.into());
			}
			responses.add(FontsMessage::Load {
				family: font.font_family.clone(),
				style: style.map(str::to_string),
				response: None,
			});
		}
		FALLBACK_FONT_BLOB.clone()
	}

	/// Read the [`Font`] recorded for a resource id in the given document registry (its first
	/// `DataSource::Font` source). Used by the font picker to display the current selection.
	pub fn id_font(&self, registry: &ResourceRegistry, resource_id: ResourceId) -> Option<Font> {
		let info = registry.info(&resource_id)?;
		info.sources.iter().find_map(|source| match source {
			DataSource::Font { family, style } => Some(font_from_pair(family, style.as_deref())),
			_ => None,
		})
	}

	/// Every content hash this handler has learned about (from both the lazy byte cache and the `Font → hash`
	/// index), so they survive `PortfolioMessage::GarbageCollectResources`. Without this, a font that was
	/// resolved earlier in the session would be GC'd from storage as soon as no live document referenced it,
	/// even though we still need its hash for the next picker action.
	pub fn used_resources(&self) -> impl Iterator<Item = ResourceHash> + '_ {
		self.font_hashes.values().copied().chain(self.font_data.keys().copied())
	}

	/// Snap a requested font to the closest style present in the catalog.
	fn normalize(&self, font: Font) -> Font {
		match self.font_catalog.find_font_style_in_catalog(&font) {
			Some(style) => Font::new(font.font_family, style.to_named_style()),
			None => font,
		}
	}
}

fn font_from_pair(family: &str, style: Option<&str>) -> Font {
	Font::new(family.to_string(), style.unwrap_or("Regular (400)").to_string())
}
