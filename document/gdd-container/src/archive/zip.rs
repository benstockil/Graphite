//! Zip archive codec.

use crate::archive::{Archive, for_each_file};
use crate::backends::memory::MemoryBackend;
use crate::{AsyncContainer, Container, ContainerError, Result, validate_path};
use std::io::{Cursor, Read, Write};

/// Cap the pre-allocation hint taken from archive metadata so a malicious
/// archive cannot trigger a huge allocation before any bytes are read.
const PREALLOC_CAP: usize = 64 * 1024 * 1024;
use zip::ZipArchive;
use zip::write::{SimpleFileOptions, ZipWriter};

pub struct Zip;

impl Archive for Zip {
	async fn serialize_from<S>(src: &S) -> Result<Vec<u8>>
	where
		S: AsyncContainer + ?Sized,
	{
		let mut buffer = Cursor::new(Vec::new());
		{
			let mut writer = ZipWriter::new(&mut buffer);
			let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

			for_each_file(src, |path, bytes| {
				writer.start_file(path, options).map_err(zip_err)?;
				writer.write_all(bytes)?;
				Ok(())
			})
			.await?;

			writer.finish().map_err(zip_err)?;
		}
		Ok(buffer.into_inner())
	}

	fn deserialize(bytes: &[u8]) -> Result<MemoryBackend> {
		let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(zip_err)?;
		let mut backend = MemoryBackend::new();

		for index in 0..archive.len() {
			let mut entry = archive.by_index(index).map_err(zip_err)?;
			if !entry.is_file() {
				continue;
			}
			let name = entry.name().to_string();
			validate_path(&name)?;
			let cap = (entry.size() as usize).min(PREALLOC_CAP);
			let mut contents = Vec::with_capacity(cap);
			entry.read_to_end(&mut contents)?;
			Container::write(&mut backend, &name, &contents)?;
		}

		Ok(backend)
	}
}

fn zip_err(error: zip::result::ZipError) -> ContainerError {
	ContainerError::Backend(format!("zip: {error}"))
}
