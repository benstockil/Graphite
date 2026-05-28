//! Archive codecs (zip, xz).
//!
//! Each codec is a stateless [`Archive`] impl:
//! `serialize_from` streams a source container's contents into archive bytes,
//! `deserialize` parses a byte buffer into a [`MemoryBackend`](crate::backends::memory::MemoryBackend).

use crate::backends::memory::MemoryBackend;
use crate::{AsyncContainer, Result};
use std::future::Future;

#[cfg(feature = "zip")]
mod zip;
#[cfg(feature = "zip")]
pub use zip::Zip;

#[cfg(feature = "xz")]
mod xz;
#[cfg(feature = "xz")]
pub use xz::Xz;

/// Bidirectional codec between a byte stream and a container's worth of named payloads.
pub trait Archive {
	fn serialize_from<S>(src: &S) -> impl Future<Output = Result<Vec<u8>>>
	where
		S: AsyncContainer + ?Sized;

	fn deserialize(bytes: &[u8]) -> Result<MemoryBackend>;
}

/// Walk every file under `src` in path-sorted order, recursing into subdirectories.
/// Invokes `visit` with each `(path, &bytes)` pair so codec implementations can stream
/// entries into their writer without buffering the whole container.
#[cfg(any(feature = "zip", feature = "xz"))]
pub(crate) async fn for_each_file<S, F>(src: &S, mut visit: F) -> Result<()>
where
	S: AsyncContainer + ?Sized,
	F: FnMut(&str, &[u8]) -> Result<()>,
{
	walk(src, "", &mut visit).await
}

#[cfg(any(feature = "zip", feature = "xz"))]
async fn walk<S, F>(src: &S, prefix: &str, visit: &mut F) -> Result<()>
where
	S: AsyncContainer + ?Sized,
	F: FnMut(&str, &[u8]) -> Result<()>,
{
	let mut files = src.list(prefix).await?;
	files.sort();
	for path in files {
		let holder = src.read(&path).await?;
		visit(&path, holder.as_slice())?;
	}

	let mut dirs = src.list_dirs(prefix).await?;
	dirs.sort();
	for dir in dirs {
		Box::pin(walk(src, &dir, visit)).await?;
	}
	Ok(())
}
