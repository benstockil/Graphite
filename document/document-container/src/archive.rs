//! Archive codecs (zip, xz).
//!
//! Each codec exposes a writer type that streams entries into an `io::Write` sink, so callers
//! can drive the entry sequence and the output destination (file, buffer, anything) themselves.
//! Decoding produces a [`MemoryBackend`].

use crate::Result;
use crate::backends::memory::MemoryBackend;
use std::io::{Seek, Write};

#[cfg(feature = "zip")]
mod zip;
#[cfg(feature = "zip")]
pub use zip::{Zip, ZipWriter};

#[cfg(feature = "xz")]
mod xz;
#[cfg(feature = "xz")]
pub use xz::{Xz, XzWriter};

/// Streaming archive codec. The associated `Writer` type wraps a `Write + Seek` sink (zip needs
/// `Seek` for the central directory; xz doesn't but `Seek` is free on file-like sinks) and
/// accepts entries one at a time. `finish` flushes the codec's trailer and consumes the wrapper.
pub trait Archive {
	type Writer<W: Write + Seek>: ArchiveWriter
	where
		W: Write + Seek;

	fn writer<W: Write + Seek>(output: W) -> Result<Self::Writer<W>>;

	fn deserialize(bytes: &[u8]) -> Result<MemoryBackend>;
}

pub trait ArchiveWriter {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<()>;
	fn finish(self) -> Result<()>;
}
