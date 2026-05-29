//! Bridge between [`crate::Codec`] and [`gdd_container::AnyContainer`]. Reads and writes
//! to/from basenames, picking the codec by extension on read.

use gdd_container::{AnyContainer, AsyncContainer};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Codec, CodecError};

/// Codecs tried by [`read_single_by_basename`] / [`iter_by_basename`] in order.
pub const KNOWN_CODECS: &[Codec] = &[Codec::Json, Codec::JsonLines, Codec::Postcard, Codec::PostcardFrames];

/// Compose a container path from `basename` and `codec.extension()`.
pub fn path_for(basename: &str, codec: Codec) -> String {
	format!("{basename}.{}", codec.extension())
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
	#[error("no file found for basename {basename:?} with any known codec")]
	NotFound { basename: String },
	#[error("container error: {0}")]
	Container(#[from] gdd_container::ContainerError),
	#[error("codec error: {0}")]
	Codec(#[from] CodecError),
}

/// Find the first `{basename}.{ext}` file matching any known codec, read it, and decode the
/// single value contained in it. Returns the chosen codec alongside the value so the caller knows
/// what to use for subsequent writes.
pub async fn read_single_by_basename<T: DeserializeOwned>(container: &AnyContainer, basename: &str) -> Result<(T, Codec), ReadError> {
	let (bytes, codec) = read_bytes_by_basename(container, basename).await?;
	let value = codec.read_single::<T>(bytes.as_slice())?;
	Ok((value, codec))
}

/// Same as [`read_single_by_basename`] but yields every value when the chosen codec is a stream.
pub async fn iter_by_basename<T: DeserializeOwned>(container: &AnyContainer, basename: &str) -> Result<(Vec<T>, Codec), ReadError> {
	let (bytes, codec) = read_bytes_by_basename(container, basename).await?;
	let values = codec.iter::<T>(bytes.as_slice()).collect::<Result<Vec<_>, _>>()?;
	Ok((values, codec))
}

/// Whether any known codec's file exists for `basename`.
pub async fn basename_exists(container: &AnyContainer, basename: &str) -> bool {
	for &codec in KNOWN_CODECS {
		if container.exists(&path_for(basename, codec)).await {
			return true;
		}
	}
	false
}

async fn read_bytes_by_basename(container: &AnyContainer, basename: &str) -> Result<(gdd_container::ByteHolder, Codec), ReadError> {
	for &codec in KNOWN_CODECS {
		let path = path_for(basename, codec);
		if container.exists(&path).await {
			let bytes = container.read(&path).await?;
			return Ok((bytes, codec));
		}
	}
	Err(ReadError::NotFound { basename: basename.to_string() })
}

/// Encode `value` with `codec` and write to `{basename}.{ext}`.
pub async fn write_single_by_basename<T: Serialize>(container: &mut AnyContainer, basename: &str, codec: Codec, value: &T) -> Result<(), ReadError> {
	let bytes = codec.write_single(value)?;
	container.write(&path_for(basename, codec), &bytes).await?;
	Ok(())
}
