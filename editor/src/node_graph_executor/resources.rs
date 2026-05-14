use graph_craft::application_io::{Resource, ResourceHash, ResourceStorage};
use std::sync::Arc;

use crate::messages::prelude::*;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum ResourceRequest {
	Write(Arc<[u8]>),
	Export { resources: Box<[ResourceHash]>, document_id: DocumentId },
	GarbageCollect { used: Box<[ResourceHash]> },
}
pub enum ResourceResponse {
	Export { document_id: DocumentId, resources: Box<[(ResourceHash, Resource)]> },
}

pub trait ResourceStorageExt {
	fn process_request(&mut self, request: ResourceRequest) -> Option<ResourceResponse>;
}

impl ResourceStorageExt for Box<dyn ResourceStorage> {
	fn process_request(&mut self, request: ResourceRequest) -> Option<ResourceResponse> {
		match request {
			ResourceRequest::Write(data) => {
				let _hash = self.write(data.as_ref());
				None
			}
			ResourceRequest::Export { resources, document_id } => {
				let mut exported_resources = Vec::new();
				for hash in resources.iter() {
					if let Some(resource) = self.read(hash) {
						exported_resources.push((*hash, resource));
					}
				}
				Some(ResourceResponse::Export {
					document_id,
					resources: exported_resources.into_boxed_slice(),
				})
			}
			ResourceRequest::GarbageCollect { used } => {
				self.garbage_collect(&used);
				None
			}
		}
	}
}
