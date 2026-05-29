//! Zip archive codec.

use crate::archive::{Archive, ArchiveWriter};
use crate::backends::memory::MemoryBackend;
use crate::{Container, ContainerError, Result, validate_path};
use std::io::{Cursor, Read, Seek, Write};

/// Cap the pre-allocation hint taken from archive metadata so a malicious
/// archive cannot trigger a huge allocation before any bytes are read.
const PREALLOC_CAP: usize = 64 * 1024 * 1024;

use zip::ZipArchive;
use zip::write::{SimpleFileOptions, ZipWriter as InnerZipWriter};

pub struct Zip;

pub struct ZipWriter<W: Write + Seek> {
	inner: InnerZipWriter<W>,
	options: SimpleFileOptions,
}

impl Archive for Zip {
	type Writer<W: Write + Seek> = ZipWriter<W>;

	fn writer<W: Write + Seek>(output: W) -> Result<Self::Writer<W>> {
		Ok(ZipWriter {
			inner: InnerZipWriter::new(output),
			options: SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
		})
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

impl<W: Write + Seek> ArchiveWriter for ZipWriter<W> {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
		validate_path(path)?;
		self.inner.start_file(path, self.options).map_err(zip_err)?;
		self.inner.write_all(bytes)?;
		Ok(())
	}

	fn finish(self) -> Result<()> {
		self.inner.finish().map_err(zip_err)?;
		Ok(())
	}
}

fn zip_err(error: zip::result::ZipError) -> ContainerError {
	ContainerError::Backend(format!("zip: {error}"))
}
