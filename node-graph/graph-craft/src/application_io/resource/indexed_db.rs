use graphene_application_io::{Resource, ResourceFuture, ResourceHash, ResourceStorage, Resources};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::JsValue;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{IdbDatabase, IdbFactory, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

const STORE_NAME: &str = "resources";
const DATABASE_VERSION: u32 = 1;

pub struct IndexedDbResourceStorage {
	db: IdbDatabase,
	store_name: Arc<String>,
	cache: Arc<Mutex<HashMap<ResourceHash, Resource>>>,
	known_keys: Arc<Mutex<HashSet<ResourceHash>>>,
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
			// Drop any pre-existing object store so an upgrade from an older format (binary keys) starts clean.
			if db.object_store_names().contains(&store_name_for_upgrade) {
				if let Err(error) = db.delete_object_store(&store_name_for_upgrade) {
					log::error!("Failed to delete stale IndexedDB object store {store_name_for_upgrade:?}: {error:?}");
				}
			}
			if let Err(error) = db.create_object_store(&store_name_for_upgrade) {
				log::error!("Failed to create IndexedDB object store {store_name_for_upgrade:?}: {error:?}");
			}
		});
		open_request.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));

		let result = await_request(open_request.unchecked_ref::<IdbRequest>()).await?;
		let db = result.dyn_into::<IdbDatabase>()?;

		let known_keys = fetch_known_keys(&db, &store_name).await?;

		Ok(Self {
			db,
			store_name: Arc::new(store_name),
			cache: Arc::new(Mutex::new(HashMap::new())),
			known_keys: Arc::new(Mutex::new(known_keys)),
		})
	}

	fn enqueue_delete(&self, hash: ResourceHash) {
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

			let key = JsValue::from_str(&hash.to_hex());

			let request = match store.delete(&key) {
				Ok(request) => request,
				Err(error) => {
					log::error!("Failed to enqueue IndexedDB delete: {error:?}");
					return;
				}
			};

			if let Err(error) = await_request(&request).await {
				log::error!("IndexedDB delete failed: {error:?}");
			}
		});
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

			let key = JsValue::from_str(&hash.to_hex());
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

impl Resources for IndexedDbResourceStorage {
	fn load(&self, hash: ResourceHash) -> ResourceFuture {
		// Clone the internal `Arc`s out so the returned future owns its state and doesn't borrow
		// `self`. This lets the read-side handle drop its `RwLock` read guard before awaiting, which
		// is what avoids the otherwise-fatal write-vs-read deadlock on single-threaded wasm.
		let db = self.db.clone();
		let store_name = self.store_name.clone();
		let cache = self.cache.clone();
		let known_keys = self.known_keys.clone();

		// The IDB future captures non-`Send` JS handles; wasm has a single thread so re-asserting
		// `Send` via `UnsafeSendFuture` is sound here.
		Box::pin(UnsafeSendFuture(async move {
			if let Some(resource) = cache.lock().unwrap().get(&hash) {
				return Some(resource.clone());
			}
			if !known_keys.lock().unwrap().contains(&hash) {
				return None;
			}

			// Cache miss for a known key: fetch from IndexedDB and wait for it to land.
			match fetch_one(&db, &store_name, hash).await {
				Ok(Some(payload)) => {
					let recomputed = ResourceHash::from(payload.as_slice());
					if recomputed != hash {
						log::warn!("IndexedDB entry's hash does not match its payload: {hash} vs {recomputed}");
						known_keys.lock().unwrap().remove(&hash);
						None
					} else {
						let resource = Resource::new(Arc::<[u8]>::from(payload));
						cache.lock().unwrap().insert(hash, resource.clone());
						Some(resource)
					}
				}
				Ok(None) => {
					known_keys.lock().unwrap().remove(&hash);
					None
				}
				Err(error) => {
					log::error!("IndexedDB fetch for {hash} failed: {error:?}");
					None
				}
			}
		}))
	}
}

impl ResourceStorage for IndexedDbResourceStorage {
	fn read(&mut self, hash: &ResourceHash) -> Option<Resource> {
		// Cache-only lookup; sync callers (e.g. document export) take what's already hydrated.
		self.cache.lock().unwrap().get(hash).cloned()
	}

	fn write(&mut self, data: &[u8]) -> ResourceHash {
		let hash = ResourceHash::from(data);
		self.cache.lock().unwrap().insert(hash, Resource::new(Arc::<[u8]>::from(data)));
		self.known_keys.lock().unwrap().insert(hash);
		self.enqueue_put(hash, data.to_vec());
		hash
	}

	fn contains(&mut self, hash: &ResourceHash) -> bool {
		self.cache.lock().unwrap().contains_key(hash) || self.known_keys.lock().unwrap().contains(hash)
	}

	fn garbage_collect(&mut self, used: &[ResourceHash]) {
		let used_set: std::collections::HashSet<ResourceHash> = used.iter().cloned().collect();

		let to_delete: Vec<ResourceHash> = {
			let mut known = self.known_keys.lock().unwrap();
			let to_delete: Vec<ResourceHash> = known.iter().filter(|h| !used_set.contains(h)).cloned().collect();
			for hash in &to_delete {
				known.remove(hash);
			}
			to_delete
		};
		self.cache.lock().unwrap().retain(|hash, _| used_set.contains(hash));

		for hash in to_delete {
			self.enqueue_delete(hash);
		}
	}
}

async fn fetch_known_keys(db: &IdbDatabase, store_name: &str) -> Result<HashSet<ResourceHash>, JsValue> {
	let transaction = db.transaction_with_str(store_name)?;
	let store = transaction.object_store(store_name)?;
	let keys_request = store.get_all_keys()?;
	let keys = await_request(&keys_request).await?;
	let keys_array = keys.dyn_into::<js_sys::Array>()?;

	let mut set = HashSet::with_capacity(keys_array.length() as usize);
	for key in keys_array.iter() {
		let Some(key_string) = key.as_string() else {
			log::warn!("Skipping IndexedDB entry whose key is not a string");
			continue;
		};
		match ResourceHash::try_from(key_string.as_str()) {
			Ok(hash) => {
				set.insert(hash);
			}
			Err(error) => log::warn!("Skipping IndexedDB entry whose key {key_string:?} is not a valid resource hash: {error}"),
		}
	}
	Ok(set)
}

async fn fetch_one(db: &IdbDatabase, store_name: &str, hash: ResourceHash) -> Result<Option<Vec<u8>>, JsValue> {
	let transaction = db.transaction_with_str(store_name)?;
	let store = transaction.object_store(store_name)?;
	let key = JsValue::from_str(&hash.to_hex());
	let request = store.get(&key)?;
	let result = await_request(&request).await?;
	if result.is_undefined() || result.is_null() {
		return Ok(None);
	}
	let bytes = result.dyn_into::<js_sys::Uint8Array>()?;
	Ok(Some(bytes.to_vec()))
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
// SAFETY: wasm is single-threaded; the non-`Sync` JS handles are never accessed from another thread.
unsafe impl Sync for IndexedDbResourceStorage {}

/// Wraps a non-`Send` future and asserts `Send` for it. Used only for the IndexedDB `load` future,
/// which holds JS handles (`&IdbDatabase`, etc.) that are structurally not `Send` but are sound to
/// expose as `Send` on wasm because wasm has a single thread.
struct UnsafeSendFuture<F>(F);

// SAFETY: Only constructed in wasm contexts where there is a single thread; the inner future's
// non-`Send` state is therefore never observed from another thread.
unsafe impl<F> Send for UnsafeSendFuture<F> {}

impl<F: Future> Future for UnsafeSendFuture<F> {
	type Output = F::Output;

	fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> core::task::Poll<Self::Output> {
		// SAFETY: `UnsafeSendFuture` only contains `F`, so projecting `Pin<&mut Self>` to
		// `Pin<&mut F>` preserves the pinning guarantee.
		unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
	}
}
