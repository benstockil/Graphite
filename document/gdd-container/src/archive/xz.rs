//! Xz-compressed tarball archive codec.

use crate::archive::{Archive, for_each_file};
use crate::backends::memory::MemoryBackend;
use crate::{AsyncContainer, Container, ContainerError, Result, validate_path};
use lzma_rust2::{XzOptions, XzReader, XzWriter};
use std::io::{Cursor, Read};

/// Cap the pre-allocation hint taken from tar metadata.
const ENTRY_PREALLOC_CAP: usize = 64 * 1024 * 1024;

/// Hard cap on the total decompressed size from an xz stream.
/// Defends against decompression bombs at the cost of refusing legitimately large archives.
const MAX_DECOMPRESSED_SIZE: u64 = 4 * 1024 * 1024 * 1024;

pub struct Xz;

impl Archive for Xz {
	async fn serialize_from<S>(src: &S) -> Result<Vec<u8>>
	where
		S: AsyncContainer + ?Sized,
	{
		let mut xz_writer = XzWriter::new(Cursor::new(Vec::new()), XzOptions::default()).map_err(lzma_err)?;
		{
			let mut tar_builder = tar::Builder::new(&mut xz_writer);
			for_each_file(src, |path, bytes| {
				let mut header = tar::Header::new_gnu();
				header.set_path(path).map_err(|error| ContainerError::Backend(format!("tar: invalid path {path}: {error}")))?;
				header.set_size(bytes.len() as u64);
				header.set_mode(0o644);
				header.set_cksum();
				tar_builder.append(&header, bytes)?;
				Ok(())
			})
			.await?;
			tar_builder.finish()?;
		}
		let buffer = xz_writer.finish().map_err(lzma_err)?;
		Ok(buffer.into_inner())
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

fn lzma_err(error: std::io::Error) -> ContainerError {
	ContainerError::Backend(format!("lzma: {error}"))
}
