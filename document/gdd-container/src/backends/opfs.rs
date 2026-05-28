//! OPFS (Origin Private File System) backend for browser wasm.

use crate::{AsyncContainer, ByteHolder, ContainerError, Result};
use js_sys::Uint8Array;
use std::future::Future;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, DomException, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions, FileSystemGetFileOptions, FileSystemWritableFileStream, WritableStream};

pub struct OpfsBackend {
	root: FileSystemDirectoryHandle,
}

// Safety: only built for browser wasm where JS handles never leave the main thread.
unsafe impl Send for OpfsBackend {}
unsafe impl Sync for OpfsBackend {}

impl OpfsBackend {
	/// Open (or create) `directory_name` under the OPFS root.
	pub async fn open(directory_name: &str) -> Result<Self> {
		let root = open_directory(directory_name).await.map_err(js_err)?;
		Ok(Self { root })
	}
}

impl AsyncContainer for OpfsBackend {
	fn read(&self, path: &str) -> impl Future<Output = Result<ByteHolder>> + '_ {
		let path = path.to_string();
		async move {
			let bytes = read_file(&self.root, &path).await.map_err(js_err)?;
			Ok(ByteHolder::Owned(bytes))
		}
	}

	fn write(&mut self, path: &str, bytes: &[u8]) -> impl Future<Output = Result<()>> + '_ {
		let path = path.to_string();
		let bytes = bytes.to_vec();
		async move { write_file(&self.root, &path, &bytes).await.map_err(js_err) }
	}

	fn write_sized(&mut self, path: &str, size: usize, fill: &mut dyn FnMut(&mut [u8])) -> impl Future<Output = Result<()>> + '_ {
		let mut buffer = vec![0; size];
		fill(&mut buffer);
		let path = path.to_string();
		async move { write_file(&self.root, &path, &buffer).await.map_err(js_err) }
	}

	fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + '_ {
		let prefix = prefix.to_string();
		async move { list_entries(&self.root, &prefix, EntryKind::File).await.map_err(js_err) }
	}

	fn list_dirs(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + '_ {
		let prefix = prefix.to_string();
		async move { list_entries(&self.root, &prefix, EntryKind::Directory).await.map_err(js_err) }
	}

	fn exists(&self, path: &str) -> impl Future<Output = bool> + '_ {
		let path = path.to_string();
		async move { file_exists(&self.root, &path).await }
	}

	fn remove(&mut self, path: &str) -> impl Future<Output = Result<()>> + '_ {
		let path = path.to_string();
		async move { remove_file(&self.root, &path).await.map_err(js_err) }
	}
}

fn js_err(error: JsValue) -> ContainerError {
	ContainerError::Backend(format!("{error:?}"))
}

async fn open_directory(directory_name: &str) -> std::result::Result<FileSystemDirectoryHandle, JsValue> {
	let storage = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?.navigator().storage();
	let root: FileSystemDirectoryHandle = JsFuture::from(storage.get_directory()).await?.dyn_into()?;

	let options = FileSystemGetDirectoryOptions::new();
	options.set_create(true);
	JsFuture::from(root.get_directory_handle_with_options(directory_name, &options)).await?.dyn_into()
}

/// Descend the `/`-separated path against `root` and return the directory handle plus the final segment.
async fn descend<'a>(root: &FileSystemDirectoryHandle, relative: &'a str, create_dirs: bool) -> std::result::Result<(FileSystemDirectoryHandle, &'a str), JsValue> {
	let mut current = root.clone();
	let mut segments = relative.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>();
	let file = segments.pop().ok_or_else(|| JsValue::from_str("empty path"))?;

	for segment in segments {
		let options = FileSystemGetDirectoryOptions::new();
		options.set_create(create_dirs);
		current = JsFuture::from(current.get_directory_handle_with_options(segment, &options)).await?.dyn_into()?;
	}
	Ok((current, file))
}

async fn read_file(root: &FileSystemDirectoryHandle, path: &str) -> std::result::Result<Vec<u8>, JsValue> {
	let (directory, name) = descend(root, path, false).await?;

	let handle: FileSystemFileHandle = JsFuture::from(directory.get_file_handle(name)).await?.dyn_into()?;
	let file_value = JsFuture::from(handle.get_file()).await?;
	let blob: Blob = file_value.dyn_into()?;
	let buffer = JsFuture::from(blob.array_buffer()).await?;
	Ok(Uint8Array::new(&buffer).to_vec())
}

async fn write_file(root: &FileSystemDirectoryHandle, path: &str, bytes: &[u8]) -> std::result::Result<(), JsValue> {
	let (directory, name) = descend(root, path, true).await?;

	let options = FileSystemGetFileOptions::new();
	options.set_create(true);

	let handle: FileSystemFileHandle = JsFuture::from(directory.get_file_handle_with_options(name, &options)).await?.dyn_into()?;
	let writable: FileSystemWritableFileStream = JsFuture::from(handle.create_writable()).await?.dyn_into()?;
	let stream: WritableStream = writable.clone().unchecked_into();
	let array = Uint8Array::from(bytes);

	if let Err(error) = JsFuture::from(writable.write_with_js_u8_array(&array)?).await {
		let _ = JsFuture::from(stream.abort()).await;
		return Err(error);
	}

	JsFuture::from(stream.close()).await?;
	Ok(())
}

async fn remove_file(root: &FileSystemDirectoryHandle, path: &str) -> std::result::Result<(), JsValue> {
	let (directory, name) = descend(root, path, false).await?;
	JsFuture::from(directory.remove_entry(name)).await?;
	Ok(())
}

async fn file_exists(root: &FileSystemDirectoryHandle, path: &str) -> bool {
	let Ok((directory, name)) = descend(root, path, false).await else {
		return false;
	};
	JsFuture::from(directory.get_file_handle(name)).await.is_ok()
}

#[derive(Clone, Copy)]
enum EntryKind {
	File,
	Directory,
}

async fn list_entries(root: &FileSystemDirectoryHandle, prefix: &str, want: EntryKind) -> std::result::Result<Vec<String>, JsValue> {
	let directory = if prefix.is_empty() {
		root.clone()
	} else {
		let mut current = root.clone();
		for segment in prefix.split('/').filter(|s| !s.is_empty()) {
			let options = FileSystemGetDirectoryOptions::new();
			options.set_create(false);
			current = match JsFuture::from(current.get_directory_handle_with_options(segment, &options)).await {
				Ok(value) => value.dyn_into()?,
				Err(error) if is_not_found(&error) => return Ok(Vec::new()),
				Err(error) => return Err(error),
			};
		}
		current
	};

	let entries = directory.entries();
	let iterator: js_sys::AsyncIterator = entries.unchecked_into();
	let mut results = Vec::new();
	let prefix_with_slash = if prefix.is_empty() || prefix.ends_with('/') { prefix.to_string() } else { format!("{prefix}/") };

	let want_kind = match want {
		EntryKind::File => web_sys::FileSystemHandleKind::File,
		EntryKind::Directory => web_sys::FileSystemHandleKind::Directory,
	};

	loop {
		let next: js_sys::IteratorNext = JsFuture::from(iterator.next()?).await?.unchecked_into();
		if next.done() {
			break;
		}
		let pair: js_sys::Array = next.value().unchecked_into();
		let Some(name) = pair.get(0).as_string() else { continue };
		let handle: web_sys::FileSystemHandle = pair.get(1).unchecked_into();
		if handle.kind() != want_kind {
			continue;
		}
		results.push(format!("{prefix_with_slash}{name}"));
	}
	Ok(results)
}

fn is_not_found(error: &JsValue) -> bool {
	error.dyn_ref::<DomException>().is_some_and(|error| error.name() == "NotFoundError")
}
