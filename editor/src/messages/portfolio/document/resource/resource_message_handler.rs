use crate::messages::portfolio::document::resource::utility_types::EmbeddedResources;
use crate::messages::prelude::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use graph_craft::application_io::resource::{DataSource, LoadResource, Resource, ResourceHash, ResourceId, ResourceRegistry};

#[derive(ExtractField)]
pub struct ResourceMessageContext<'a> {
	pub document_id: DocumentId,
	pub fonts: &'a FontsMessageHandler,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, ExtractField)]
pub struct ResourceMessageHandler {
	pub registry: ResourceRegistry,
	pub embedded: EmbeddedResources,
	/// Per-id state for [`ResourceMessage::ResolveStep`]: `(next_source_index, last_attempted_source_index)`.
	/// The "last attempted" index is used by [`ResourceMessage::Resolved`] to learn which `DataSource` produced
	/// the bytes so a font hash can be reported back to [`FontsMessage::ResourceResolved`].
	///
	/// In-flight state only; never persisted. The custom `Deserialize` impl below treats unknown JSON keys as
	/// `ResourceHash` entries (legacy embedded format), so if this field were serialized, opening a freshly-saved
	/// document would fail trying to parse `"pending_resolves"` as a hash.
	#[serde(skip)]
	pending_resolves: HashMap<ResourceId, ResolveProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ResolveProgress {
	next: usize,
	last_attempted: Option<usize>,
}

#[message_handler_data]
impl MessageHandler<ResourceMessage, ResourceMessageContext<'_>> for ResourceMessageHandler {
	fn process_message(&mut self, message: ResourceMessage, responses: &mut VecDeque<Message>, context: ResourceMessageContext) {
		let ResourceMessageContext { document_id, fonts } = context;

		match message {
			ResourceMessage::StoreEmbedded { resource_id, data } => {
				let hash = ResourceHash::from(data.as_ref());
				self.registry.push_source_back(&resource_id, DataSource::Embedded);
				self.registry.resolve(&resource_id, hash);
				responses.add(ResourceStorageMessage::Store { data });
				// Auto-Resolve hook: other ids may now be resolvable indirectly (sibling ids sharing the same hash via font equality).
				responses.add(ResourceMessage::Resolve);
			}
			ResourceMessage::AddFont { resource_id, font } => {
				// Normalize through the catalog when possible (the catalog might be empty until the frontend reports it).
				let style = fonts.font_catalog.find_font_style_in_catalog(&font);
				let style_name = style.map(|style| style.to_named_style()).unwrap_or_else(|| font.font_style.clone());
				self.registry.push_source_back(
					&resource_id,
					DataSource::Font {
						family: font.font_family,
						style: Some(style_name),
					},
				);
				// Auto-Resolve hook: the new unresolved id needs to be picked up.
				responses.add(ResourceMessage::Resolve);
			}
			ResourceMessage::Resolve => {
				let unresolved_ids: Vec<ResourceId> = self.registry.unresolved().map(|info| info.id).collect();
				for id in unresolved_ids {
					// Only kick off a step if this id isn't already mid-walk; otherwise a second `Resolve` (often
					// queued right after `AddFont`) would queue another `ResolveStep` whose counter is already
					// past the available sources, producing "no more sources to try" errors.
					if !self.pending_resolves.contains_key(&id) {
						self.pending_resolves.insert(id, ResolveProgress::default());
						responses.add(ResourceMessage::ResolveStep { resource_id: id });
					}
				}
			}
			ResourceMessage::ResolveStep { resource_id } => {
				let Some(progress) = self.pending_resolves.get_mut(&resource_id) else { return };
				let Some(info) = self.registry.info(&resource_id) else {
					log::error!("ResolveStep for {resource_id}: no registry entry");
					self.pending_resolves.remove(&resource_id);
					return;
				};
				let source_index = progress.next;
				let Some(source) = info.sources.get(source_index).cloned() else {
					log::error!("ResolveStep for {resource_id}: no more sources to try");
					self.pending_resolves.remove(&resource_id);
					return;
				};
				progress.last_attempted = Some(source_index);

				match source {
					DataSource::Embedded => {
						log::error!("Resource {resource_id} is embedded but failed to resolve before reaching ResolveStep");
						progress.next += 1;
					}
					DataSource::Url(url) => {
						responses.add(FrontendMessage::TriggerResolveResource {
							document_id,
							resource_id,
							url: url.to_string(),
						});
						progress.next += 1;
					}
					DataSource::Font { family, style } => {
						if let Some(hash) = fonts.cached_hash(&family, style.as_deref()) {
							self.registry.resolve(&resource_id, hash);
							self.pending_resolves.remove(&resource_id);
							// Re-render now that this id is resolved (in case the caller already queued a render
							// against an unresolved id that would have hit `ResourceNotFound`).
							responses.add(NodeGraphMessage::RunDocumentGraph);
							return;
						}
						if let Some(url) = fonts.cached_url(&family, style.as_deref()) {
							responses.add(FrontendMessage::TriggerResolveResource { document_id, resource_id, url });
							progress.next += 1;
							return;
						}
						// Catalog hasn't loaded yet. Ask the frontend to load it and leave this id pending; `CatalogLoaded`
						// will broadcast `PortfolioMessage::ResolveAllResources`, which re-walks this id with the URL available.
						responses.add(FrontendMessage::TriggerFontCatalogLoad);
					}
				}
			}
			ResourceMessage::Resolved { resource_id, data } => {
				let hash = ResourceHash::from(data.as_ref());
				let attempted = self.pending_resolves.remove(&resource_id).and_then(|progress| progress.last_attempted);
				let font_source = attempted
					.and_then(|index| self.registry.info(&resource_id).and_then(|info| info.sources.get(index).cloned()))
					.and_then(|source| match source {
						DataSource::Font { family, style } => Some((family, style)),
						_ => None,
					});

				self.registry.resolve(&resource_id, hash);
				responses.add(ResourceStorageMessage::Store { data });

				if let Some((family, style)) = font_source {
					responses.add(FontsMessage::ResourceResolved { family, style, hash });
				}

				// Auto-Resolve hook: other ids might now resolve via the freshly learnt font hash.
				responses.add(ResourceMessage::Resolve);
				// Re-render the document graph now that the formerly-unresolved id has bytes.
				responses.add(NodeGraphMessage::RunDocumentGraph);
			}
		}
	}

	fn actions(&self) -> ActionList {
		actions!(ResourceMessageDiscriminant;)
	}
}

impl ResourceMessageHandler {
	pub fn is_empty(&self) -> bool {
		self.registry.is_empty() && self.embedded.is_empty()
	}

	pub async fn embed_resources(&mut self, resources_load_handle: Box<dyn LoadResource>) {
		let embedded = self
			.registry
			.resolved()
			.filter(|info| info.sources.contains(&DataSource::Embedded))
			.filter_map(|info| {
				if let Some(hash) = info.hash {
					let resource = resources_load_handle.load(*hash);
					Some(async move { resource.await.map(|resource| (*hash, resource)) })
				} else {
					None
				}
			})
			.collect::<Vec<_>>();

		self.embedded = EmbeddedResources::from_iter(futures::future::join_all(embedded).await.into_iter().flatten());
	}

	pub fn garbage_collect(&mut self, used: &[ResourceId]) {
		let used = HashSet::<ResourceId>::from_iter(used.iter().cloned());
		let unused = self.registry.ids().filter(|id| !used.contains(id)).collect::<Vec<_>>();
		unused.into_iter().for_each(|id| {
			self.registry.delete(&id);
		});
	}
}

// TODO: Eventually remove this document upgrade code
impl<'de> serde::Deserialize<'de> for ResourceMessageHandler {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		enum Key {
			Registry,
			Embedded,
			Hash(ResourceHash),
		}

		impl<'de> serde::Deserialize<'de> for Key {
			fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
				let raw = String::deserialize(deserializer)?;
				Ok(match raw.as_str() {
					"registry" => Key::Registry,
					"embedded" => Key::Embedded,
					_ => Key::Hash(raw.parse().map_err(serde::de::Error::custom)?),
				})
			}
		}

		struct EmbeddedResourcesVisitor {
			human_readable: bool,
		}

		impl<'de> serde::de::Visitor<'de> for EmbeddedResourcesVisitor {
			type Value = ResourceMessageHandler;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("an EmbeddedResources struct or a legacy EmbeddedResourceData map")
			}

			fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
				let mut output = ResourceMessageHandler::default();

				while let Some(key) = map.next_key::<Key>()? {
					match key {
						Key::Registry => output.registry = map.next_value()?,
						Key::Embedded => output.embedded = map.next_value()?,
						Key::Hash(hash) => {
							let bytes = if self.human_readable {
								let encoded: String = map.next_value()?;
								BASE64.decode(&encoded).map_err(serde::de::Error::custom)?
							} else {
								let raw: serde_bytes::ByteBuf = map.next_value()?;
								raw.into_vec()
							};
							let data_hash = output.embedded.store(Resource::new(bytes));
							if data_hash != hash {
								return Err(serde::de::Error::custom(format!("EmbeddedResource hash mismatch: expected {hash}, got {data_hash}")));
							}
						}
					}
				}

				Ok(output)
			}
		}

		let human_readable = deserializer.is_human_readable();
		deserializer.deserialize_map(EmbeddedResourcesVisitor { human_readable })
	}
}
