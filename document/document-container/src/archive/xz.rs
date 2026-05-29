//! Xz-compressed tarball archive codec.

use crate::archive::{Archive, ArchiveWriter};
use crate::backends::memory::MemoryBackend;
use crate::{Container, ContainerError, Result, validate_path};
use lzma_rust2::{XzOptions, XzReader, XzWriter as InnerXzWriter};
use std::io::{Cursor, Read, Seek, Write};

/// Cap the pre-allocation hint taken from tar metadata.
const ENTRY_PREALLOC_CAP: usize = 64 * 1024 * 1024;

/// Hard cap on the total decompressed size from an xz stream.
/// Defends against decompression bombs at the cost of refusing legitimately large archives.
const MAX_DECOMPRESSED_SIZE: u64 = 4 * 1024 * 1024 * 1024;

pub struct Xz;

/// xz-tar writer. Held as an `Option` so `finish` can take ownership and unwind the layered
/// writers in the right order: drop the tar builder first to flush its trailer, then finish xz.
pub struct XzWriter<W: Write + Seek> {
	tar: Option<tar::Builder<InnerXzWriter<W>>>,
}

impl Archive for Xz {
	type Writer<W: Write + Seek> = XzWriter<W>;

	fn writer<W: Write + Seek>(output: W) -> Result<Self::Writer<W>> {
		let xz_writer = InnerXzWriter::new(output, XzOptions::default()).map_err(lzma_err)?;
		Ok(XzWriter {
			tar: Some(tar::Builder::new(xz_writer)),
		})
	}

	fn deserialize(bytes: &[u8]) -> Result<MemoryBackend> {
		let xz_reader = XzReader::new(Cursor::new(bytes), false);
		let bounded = xz_reader.take(MAX_DECOMPRESSED_SIZE);

		let mut tar_reader = tar::Archive::new(bounded);
		let mut backend = MemoryBackend::new();

		for entry in tar_reader.entries()? {
			let mut entry = entry?;
			if entry.header().entry_type() != tar::EntryType::Regular {
				continue;
			}
			let path = entry.path()?.to_string_lossy().into_owned();
			validate_path(&path)?;
			let cap = (entry.size() as usize).min(ENTRY_PREALLOC_CAP);
			let mut contents = Vec::with_capacity(cap);
			entry.read_to_end(&mut contents)?;
			Container::write(&mut backend, &path, &contents)?;
		}

		Ok(backend)
	}
}

impl<W: Write + Seek> ArchiveWriter for XzWriter<W> {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
		validate_path(path)?;
		let tar = self.tar.as_mut().ok_or_else(|| ContainerError::Backend("XzWriter already finished".into()))?;
		let mut header = tar::Header::new_gnu();
		header.set_path(path).map_err(|error| ContainerError::Backend(format!("tar: invalid path {path}: {error}")))?;
		header.set_size(bytes.len() as u64);
		header.set_mode(0o644);
		header.set_cksum();
		tar.append(&header, bytes)?;
		Ok(())
	}

	fn finish(mut self) -> Result<()> {
		let mut tar = self.tar.take().ok_or_else(|| ContainerError::Backend("XzWriter already finished".into()))?;
		tar.finish()?;
		let xz_writer = tar.into_inner()?;
		xz_writer.finish().map_err(lzma_err)?;
		Ok(())
	}
}

fn lzma_err(error: std::io::Error) -> ContainerError {
	ContainerError::Backend(format!("lzma: {error}"))
}
