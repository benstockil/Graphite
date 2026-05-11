use graphene_application_io::{Resource, ResourceHash, ResourceStorage};
use std::collections::HashMap;
use std::sync::Arc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::JsValue;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{IdbDatabase, IdbFactory, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

const STORE_NAME: &str = "resources";
const DATABASE_VERSION: u32 = 1;

pub struct IndexedDbResourceStorage {
	cache: HashMap<ResourceHash, Resource>,
	db: IdbDatabase,
	store_name: String,
}

impl IndexedDbResourceStorage {
	pub async fn load(database_name: &str) -> Result<Self, JsValue> {
		let factory = indexed_db_factory()?;
		let open_request = factory.open_with_u32(database_name, DATABASE_VERSION)?;
		let store_name = STORE_NAME.to_string();

		let store_name_for_upgrade = store_name.clone();
		let on_upgrade = Closure::once_into_js(move |event: web_sys::Event| {
			let Some(target) = event.target() else { return };
			let Ok(request) = target.dyn_into::<IdbOpenDbRequest>() else { return };
			let Ok(db_value) = request.result() else { return };
			let Ok(db) = db_value.dyn_into::<IdbDatabase>() else { return };
			if let Err(error) = db.create_object_store(&store_name_for_upgrade) {
				log::error!("Failed to create IndexedDB object store {store_name_for_upgrade:?}: {error:?}");
			}
		});
		open_request.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));

		let result = await_request(open_request.unchecked_ref::<IdbRequest>()).await?;
		let db = result.dyn_into::<IdbDatabase>()?;

		let mut storage = Self {
			cache: HashMap::new(),
			db,
			store_name,
		};
		storage.hydrate().await?;

		Ok(storage)
	}

	pub fn len(&self) -> usize {
		self.cache.len()
	}

	pub fn is_empty(&self) -> bool {
		self.cache.is_empty()
	}

	async fn hydrate(&mut self) -> Result<(), JsValue> {
		let transaction = self.db.transaction_with_str(&self.store_name)?;
		let store = transaction.object_store(&self.store_name)?;

		let keys_request = store.get_all_keys()?;
		let values_request = store.get_all()?;

		let keys = await_request(&keys_request).await?;
		let values = await_request(&values_request).await?;

		let keys_array = keys.dyn_into::<js_sys::Array>()?;
		let values_array = values.dyn_into::<js_sys::Array>()?;

		for (key, value) in keys_array.iter().zip(values_array.iter()) {
			let Ok(key_bytes) = key.dyn_into::<js_sys::Uint8Array>() else {
				log::warn!("Skipping IndexedDB entry whose key is not a Uint8Array");
				continue;
			};
			let Ok(value_bytes) = value.dyn_into::<js_sys::Uint8Array>() else {
				log::warn!("Skipping IndexedDB entry whose value is not a Uint8Array");
				continue;
			};

			if key_bytes.length() != 32 {
				log::warn!("Skipping IndexedDB entry whose key is {} bytes (expected 32)", key_bytes.length());
				continue;
			}
			let mut hash_buf = [0u8; 32];
			key_bytes.copy_to(&mut hash_buf);

			let payload = value_bytes.to_vec();
			let recomputed = ResourceHash::from(blake3::hash(&payload));
			let stored = ResourceHash::from(hash_buf);
			if recomputed != stored {
				log::warn!("Skipping IndexedDB entry whose hash does not match its payload: {stored} vs {recomputed}");
				continue;
			}

			self.cache.insert(stored, Resource::new(Arc::<[u8]>::from(payload)));
		}

		Ok(())
	}

	fn enqueue_put(&self, hash: ResourceHash, data: Vec<u8>) {
		let db = self.db.clone();
		let store_name = self.store_name.clone();

		spawn_local(async move {
			let transaction = match db.transaction_with_str_and_mode(&store_name, IdbTransactionMode::Readwrite) {
				Ok(transaction) => transaction,
				Err(error) => {
					log::error!("Failed to open IndexedDB transaction: {error:?}");
					return;
				}
			};

			let store = match transaction.object_store(&store_name) {
				Ok(store) => store,
				Err(error) => {
					log::error!("Failed to access IndexedDB object store {store_name:?}: {error:?}");
					return;
				}
			};

			let key = js_sys::Uint8Array::from(hash.as_bytes().as_slice());
			let value = js_sys::Uint8Array::from(data.as_slice());

			let request = match store.put_with_key(&value, &key) {
				Ok(request) => request,
				Err(error) => {
					log::error!("Failed to enqueue IndexedDB put: {error:?}");
					return;
				}
			};

			if let Err(error) = await_request(&request).await {
				log::error!("IndexedDB put failed: {error:?}");
			}
		});
	}
}

impl ResourceStorage for IndexedDbResourceStorage {
	fn read(&mut self, hash: &ResourceHash) -> Option<Resource> {
		self.cache.get(hash).cloned()
	}

	fn write(&mut self, data: &[u8]) -> ResourceHash {
		let hash = ResourceHash::from(blake3::hash(data));
		self.cache.insert(hash, Resource::new(Arc::<[u8]>::from(data)));
		self.enqueue_put(hash, data.to_vec());
		hash
	}

	fn contains(&mut self, hash: &ResourceHash) -> bool {
		self.cache.contains_key(hash)
	}
}

fn indexed_db_factory() -> Result<IdbFactory, JsValue> {
	let window = web_sys::window().ok_or_else(|| JsValue::from_str("no global window"))?;
	window.indexed_db()?.ok_or_else(|| JsValue::from_str("indexedDB is unavailable"))
}

async fn await_request(request: &IdbRequest) -> Result<JsValue, JsValue> {
	let promise = js_sys::Promise::new(&mut |resolve, reject| {
		let reject_for_error = reject.clone();
		let request_for_success = request.clone();
		let on_success = Closure::once_into_js(move |_event: web_sys::Event| match request_for_success.result() {
			Ok(value) => {
				let _ = resolve.call1(&JsValue::NULL, &value);
			}
			Err(error) => {
				let _ = reject.call1(&JsValue::NULL, &error);
			}
		});
		request.set_onsuccess(Some(on_success.unchecked_ref()));

		let request_for_error = request.clone();
		let on_error = Closure::once_into_js(move |_event: web_sys::Event| {
			let error = request_for_error
				.error()
				.ok()
				.flatten()
				.map(JsValue::from)
				.unwrap_or_else(|| JsValue::from_str("IndexedDB request error"));
			let _ = reject_for_error.call1(&JsValue::NULL, &error);
		});
		request.set_onerror(Some(on_error.unchecked_ref()));
	});

	JsFuture::from(promise).await
}

// SAFETY: wasm is single-threaded, so the non-`Send` JS handles inside the IndexedDB-backed `ResourceStorage` are never observed across threads.
unsafe impl Send for IndexedDbResourceStorage {}
