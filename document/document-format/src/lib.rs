//! Typed handle for `.gdd` documents.
//!
//! [`Gdd`] owns a [`graph_storage::Session`] plus a working-copy [`document_container::AnyContainer`].
//! Mutations flow through `Gdd` to keep the session and the on-disk working copy mirrored.
//! Export is a separate, explicit operation — see [`export::ExportFormat`].
//!
//! See `notes/disk-container-format.md` for the design rationale.

use std::collections::HashMap;
use std::path::Path;

use document_container::archive::Archive;
use document_container::backends::folder::FolderBackend;
use document_container::{AnyContainer, AsyncContainer, ByteHolder, ContainerError};
use graph_storage::{CommitError, CrdtError, Delta, HotOp, NodeMetadataSource, PeerId, Registry, Rev, Session, TimeStamp};
use graphene_resource::ResourceHash;

pub mod codec;
pub mod export;
pub mod io;
pub mod layout;
pub mod manifest;
pub mod session_state;

pub use codec::{Codec, CodecError};
pub use export::{ExportFormat, ExportOptions};
pub use io::ReadError;
pub use layout::{GddV1, Layout};
pub use manifest::Manifest;
pub use session_state::SessionState;

/// Working-copy codecs. The working copy lives in appdata, not under VCS — these defaults
/// optimize for size and write cost. JSON/JSONL is opt-in via `ExportFormat::Folder` for users
/// who want a diffable on-disk representation.
pub const DEFAULT_MANIFEST_CODEC: Codec = Codec::Json;
pub const DEFAULT_SESSION_CODEC: Codec = Codec::Json;
pub const DEFAULT_REGISTRY_CODEC: Codec = Codec::Postcard;
pub const DEFAULT_HISTORY_CODEC: Codec = Codec::PostcardFrames;
pub const DEFAULT_HOT_LOG_CODEC: Codec = Codec::PostcardFrames;

/// Editor-facing handle. Owns the `Session` and the working-copy container; mutations are mirrored
/// to disk continuously (every retirement appends to `history.jsonl` and re-snapshots `registry.bin`).
///
/// The manifest is not cached in memory — it's read through the container when callers ask for it
/// via [`Gdd::read_manifest`]. Manifest reads are rare (open, create, "last saved" UI lookups),
/// so caching would just add a sync invariant for no measurable win.
pub struct Gdd<L: Layout = GddV1> {
	session: Session,
	working: AnyContainer,
	layout: L,
}

impl<L: Layout + Default> Gdd<L> {
	/// Open an existing working copy at `path`. Validates the manifest, materializes the session
	/// from `registry.bin` (fast path) or by replaying `history.jsonl` (slow path), then applies
	/// the persisted hot log on top.
	pub async fn open(path: &Path) -> Result<Self, OpenError> {
		let working = AnyContainer::Folder(FolderBackend::open(path)?);
		let layout = L::default();
		Self::open_in(working, layout).await
	}

	/// Create a fresh, empty working copy at `path` bound to `peer`. Writes a default manifest
	/// and session state; the caller fills in editor metadata via [`Gdd::update_manifest`].
	pub async fn create(path: &Path, peer: PeerId, document_uuid: u64, editor_version: String, stdlib_version: String) -> Result<Self, OpenError> {
		let working = AnyContainer::Folder(FolderBackend::create(path)?);
		let layout = L::default();
		Self::create_in(working, layout, peer, document_uuid, editor_version, stdlib_version).await
	}
}

impl<L: Layout> Gdd<L> {
	/// Backend-agnostic open. Splits out so tests can supply a [`document_container::backends::memory::MemoryBackend`].
	pub async fn open_in(working: AnyContainer, layout: L) -> Result<Self, OpenError> {
		let (manifest, _): (Manifest, _) = io::read_single_by_basename(&working, layout.manifest_basename()).await?;
		validate_manifest(&manifest)?;

		let session_state: SessionState = match io::basename_exists(&working, layout.session_basename()).await {
			true => io::read_single_by_basename(&working, layout.session_basename()).await?.0,
			false => SessionState::default(),
		};

		let has_registry = io::basename_exists(&working, layout.registry_basename()).await;
		let has_history = io::basename_exists(&working, layout.history_basename()).await;

		let mut session = match (has_registry, has_history) {
			(true, true) => {
				let (registry, _): (Registry, _) = io::read_single_by_basename(&working, layout.registry_basename()).await?;
				let history_map: HashMap<Rev, Delta> = load_history(&working, &layout).await?.into_iter().map(|delta| (delta.id, delta)).collect();
				Session::load(manifest.peer_id, registry, history_map, session_state.head_rev, session_state.next_node_counter)
			}
			(true, false) => {
				// Registry-only export: synthesize a history that reproduces this state.
				let (registry, _): (Registry, _) = io::read_single_by_basename(&working, layout.registry_basename()).await?;
				Session::bootstrap_from_registry(manifest.peer_id, registry)?
			}
			(false, _) => Session::replay_from_history(manifest.peer_id, load_history(&working, &layout).await?, session_state.next_node_counter)?,
		};

		replay_hot_log(&working, &layout, &mut session).await?;

		Ok(Self { session, working, layout })
	}

	/// Backend-agnostic create. Writes with the working-copy default codecs (see `DEFAULT_*_CODEC`).
	pub async fn create_in(mut working: AnyContainer, layout: L, peer: PeerId, document_uuid: u64, editor_version: String, stdlib_version: String) -> Result<Self, OpenError> {
		let manifest = Manifest::new(document_uuid, peer, editor_version, stdlib_version);
		io::write_single_by_basename(&mut working, layout.manifest_basename(), DEFAULT_MANIFEST_CODEC, &manifest).await?;
		io::write_single_by_basename(&mut working, layout.session_basename(), DEFAULT_SESSION_CODEC, &SessionState::default()).await?;

		let session = Session::with_peer(peer);
		io::write_single_by_basename(&mut working, layout.registry_basename(), DEFAULT_REGISTRY_CODEC, session.registry()).await?;

		Ok(Self { session, working, layout })
	}
}

fn validate_manifest(manifest: &Manifest) -> Result<(), OpenError> {
	if manifest.format != manifest::FORMAT_MAGIC {
		return Err(OpenError::WrongFormat {
			found: manifest.format.clone(),
			expected: manifest::FORMAT_MAGIC,
		});
	}
	if manifest.format_version > manifest::SUPPORTED_FORMAT_VERSION {
		return Err(OpenError::UnsupportedVersion {
			found: manifest.format_version,
			max_supported: manifest::SUPPORTED_FORMAT_VERSION,
		});
	}
	Ok(())
}

/// Returns the codec whose extension matches the file currently on disk at `basename`.
/// `None` if no file exists for any known codec.
async fn current_codec_for(container: &AnyContainer, basename: &str) -> Option<Codec> {
	for &codec in io::KNOWN_CODECS {
		if container.exists(&io::path_for(basename, codec)).await {
			return Some(codec);
		}
	}
	None
}

async fn load_history<L: Layout>(working: &AnyContainer, layout: &L) -> Result<Vec<Delta>, OpenError> {
	if !io::basename_exists(working, layout.history_basename()).await {
		return Ok(Vec::new());
	}
	let (deltas, _): (Vec<Delta>, _) = io::iter_by_basename(working, layout.history_basename()).await?;
	Ok(deltas)
}

async fn replay_hot_log<L: Layout>(working: &AnyContainer, layout: &L, session: &mut Session) -> Result<(), OpenError> {
	if !io::basename_exists(working, layout.hot_log_basename()).await {
		return Ok(());
	}
	let (hot_ops, _): (Vec<HotOp>, _) = io::iter_by_basename(working, layout.hot_log_basename()).await?;
	for hot_op in hot_ops {
		session.replay_hot_op(hot_op)?;
	}
	Ok(())
}

impl<L: Layout> Gdd<L> {
	pub fn session(&self) -> &Session {
		&self.session
	}

	pub fn registry(&self) -> &Registry {
		self.session.registry()
	}

	pub fn layout(&self) -> &L {
		&self.layout
	}

	/// Drop the session and return the working-copy container + layout.
	/// Intended for test code that needs to reopen against the same container.
	pub fn into_storage(self) -> (AnyContainer, L) {
		(self.working, self.layout)
	}

	/// Read and deserialize the on-disk manifest.
	pub async fn read_manifest(&self) -> Result<Manifest, OpenError> {
		let (manifest, _) = io::read_single_by_basename::<Manifest>(&self.working, self.layout.manifest_basename()).await?;
		Ok(manifest)
	}

	/// Write `manifest` to disk, preserving whatever codec is already there.
	pub async fn write_manifest(&mut self, manifest: &Manifest) -> Result<(), OpenError> {
		let codec = current_codec_for(&self.working, self.layout.manifest_basename()).await.unwrap_or(DEFAULT_MANIFEST_CODEC);
		io::write_single_by_basename(&mut self.working, self.layout.manifest_basename(), codec, manifest).await?;
		Ok(())
	}

	/// Read → mutate → write. Sugar for the common case where the caller wants to change a few
	/// manifest fields without juggling the round-trip themselves.
	pub async fn update_manifest(&mut self, edit: impl FnOnce(&mut Manifest)) -> Result<(), OpenError> {
		let mut manifest = self.read_manifest().await?;
		edit(&mut manifest);
		self.write_manifest(&manifest).await
	}

	/// Commit a runtime snapshot through `Session`, appending each emitted delta to the retired
	/// history file and persisting updated session-state.
	///
	/// Interim API: forwards to `Session::commit_from_runtime`, which itself is a shim that diffs
	/// the runtime against a freshly-converted `Registry`. The long-term replacement will take
	/// runtime deltas directly. This wrapper goes away when that lands.
	pub async fn commit_from_runtime<M: NodeMetadataSource>(&mut self, network: &graph_craft::document::NodeNetwork, metadata: &M) -> Result<Vec<Rev>, CommitError> {
		let revs = self.session.commit_from_runtime(network, metadata)?;
		if let Err(error) = self.persist_committed_deltas(&revs).await {
			log::error!("Failed to persist committed deltas to working copy: {error}");
		}
		Ok(revs)
	}

	/// Apply a hot op from the broadcast stream, appending one frame to the hot log.
	pub async fn apply_hot_op(&mut self, op: HotOp) -> Result<(), CrdtError> {
		self.session.apply_hot_op(op.clone())?;
		if let Err(error) = self.append_hot_frame(&op).await {
			log::error!("Failed to append hot op frame: {error}");
		}
		Ok(())
	}

	async fn persist_committed_deltas(&mut self, revs: &[Rev]) -> Result<(), OpenError> {
		if revs.is_empty() {
			return Ok(());
		}

		let history_codec = current_codec_for(&self.working, self.layout.history_basename()).await.unwrap_or(DEFAULT_HISTORY_CODEC);
		let mut buffer = Vec::new();
		for rev in revs {
			let delta = self.session.history().find(|delta| delta.id == *rev).ok_or(CodecError::Empty)?;
			history_codec.append(&mut buffer, delta)?;
		}
		self.working.append(&io::path_for(self.layout.history_basename(), history_codec), &buffer).await?;

		let session_codec = current_codec_for(&self.working, self.layout.session_basename()).await.unwrap_or(DEFAULT_SESSION_CODEC);
		let state = SessionState {
			head_rev: self.session.head_rev(),
			next_node_counter: self.session.next_node_counter(),
		};
		io::write_single_by_basename(&mut self.working, self.layout.session_basename(), session_codec, &state).await?;

		Ok(())
	}

	async fn append_hot_frame(&mut self, op: &HotOp) -> Result<(), OpenError> {
		let codec = current_codec_for(&self.working, self.layout.hot_log_basename()).await.unwrap_or(DEFAULT_HOT_LOG_CODEC);
		let mut buffer = Vec::new();
		codec.append(&mut buffer, op)?;
		self.working.append(&io::path_for(self.layout.hot_log_basename(), codec), &buffer).await?;
		Ok(())
	}

	/// Working-copy checkpoint: promote hot ops with timestamp `≤ up_to` into retired deltas,
	/// append them to the history file, rewrite the hot log with remaining (unretired) ops,
	/// re-snapshot the registry, and bump `last_retired_at` on the manifest.
	pub async fn retire(&mut self, up_to: TimeStamp) -> Result<(), RetireError> {
		let new_revs = self.session.retire(up_to)?;

		if !new_revs.is_empty() {
			let history_codec = current_codec_for(&self.working, self.layout.history_basename()).await.unwrap_or(DEFAULT_HISTORY_CODEC);
			let mut buffer = Vec::new();
			for rev in &new_revs {
				let delta = self.session.history().find(|delta| delta.id == *rev).ok_or(CodecError::Empty)?;
				history_codec.append(&mut buffer, delta)?;
			}
			self.working.append(&io::path_for(self.layout.history_basename(), history_codec), &buffer).await?;
		}

		// Rewrite hot log with whatever survived retirement.
		let hot_codec = current_codec_for(&self.working, self.layout.hot_log_basename()).await.unwrap_or(DEFAULT_HOT_LOG_CODEC);
		let mut hot_buffer = Vec::new();
		for hot_op in self.session.hot_log() {
			hot_codec.append(&mut hot_buffer, hot_op)?;
		}
		self.working.write(&io::path_for(self.layout.hot_log_basename(), hot_codec), &hot_buffer).await?;

		// Re-snapshot registry.
		let registry_codec = current_codec_for(&self.working, self.layout.registry_basename()).await.unwrap_or(DEFAULT_REGISTRY_CODEC);
		io::write_single_by_basename(&mut self.working, self.layout.registry_basename(), registry_codec, self.session.registry()).await?;

		// Persist session cursor.
		let session_codec = current_codec_for(&self.working, self.layout.session_basename()).await.unwrap_or(DEFAULT_SESSION_CODEC);
		let state = SessionState {
			head_rev: self.session.head_rev(),
			next_node_counter: self.session.next_node_counter(),
		};
		io::write_single_by_basename(&mut self.working, self.layout.session_basename(), session_codec, &state).await?;

		// Bump manifest timestamp.
		self.update_manifest(|m| m.last_retired_at = Some(chrono::Utc::now().to_rfc3339())).await?;

		Ok(())
	}

	pub async fn read_resource(&self, hash: &ResourceHash) -> Result<ByteHolder, ContainerError> {
		self.working.read(&self.layout.resource_path(hash)).await
	}

	pub async fn add_resource(&mut self, hash: ResourceHash, bytes: &[u8]) -> Result<(), ContainerError> {
		self.working.write(&self.layout.resource_path(&hash), bytes).await
	}

	/// Add a resource by copying from `src` rather than buffering its bytes. Folder backends use
	/// `fs::copy` (CoW on supported filesystems); other backends fall back to read-then-write.
	pub async fn add_resource_from_path(&mut self, hash: ResourceHash, src: &Path) -> Result<(), ContainerError> {
		let dest_path = self.layout.resource_path(&hash);
		if let AnyContainer::Folder(folder) = &mut self.working {
			let full = folder.root().join(&dest_path);
			if let Some(parent) = full.parent() {
				std::fs::create_dir_all(parent).map_err(ContainerError::Io)?;
			}
			std::fs::copy(src, &full).map_err(ContainerError::Io)?;
			return Ok(());
		}
		let bytes = std::fs::read(src).map_err(ContainerError::Io)?;
		self.working.write(&dest_path, &bytes).await
	}

	pub async fn has_resource(&self, hash: &ResourceHash) -> bool {
		self.working.exists(&self.layout.resource_path(hash)).await
	}

	pub async fn remove_resource(&mut self, hash: &ResourceHash) -> Result<(), ContainerError> {
		self.working.remove(&self.layout.resource_path(hash)).await
	}

	/// Enumerate every resource currently in the working copy. Paths that don't parse as a
	/// `ResourceHash` (foreign files dropped into the resources directory) are silently skipped.
	pub async fn resource_hashes(&self) -> Result<Vec<ResourceHash>, ContainerError> {
		let dir = self.layout.resources_dir();
		if !self.working.list_dirs("").await?.iter().any(|d| d == dir) {
			return Ok(Vec::new());
		}
		let entries = self.working.list(dir).await?;
		let prefix = format!("{dir}/");
		let mut hashes = Vec::with_capacity(entries.len());
		for entry in entries {
			let Some(name) = entry.strip_prefix(&prefix) else { continue };
			if let Ok(hash) = name.parse::<ResourceHash>() {
				hashes.push(hash);
			}
		}
		Ok(hashes)
	}

	/// Build a self-contained export of the working copy: re-encodes typed payloads with the
	/// chosen codec, omits session/hot-log (peer-local + ephemeral), copies resources straight
	/// through, then materializes as a folder, zip, or xz archive at `dest`. Does not mutate
	/// `self` and does not buffer the full export — resources stream end-to-end.
	pub async fn export(&self, dest: &Path, format: ExportFormat, options: ExportOptions) -> Result<(), ExportError> {
		options.validate().map_err(ExportError::InvalidOptions)?;

		let codec = match format {
			ExportFormat::Folder { codec } | ExportFormat::Zip { codec } | ExportFormat::Xz { codec } => codec,
		};

		match format {
			ExportFormat::Folder { .. } => {
				let mut folder = document_container::backends::folder::FolderBackend::create(dest)?;
				let mut sink = FolderSink { folder: &mut folder };
				self.stream_entries(codec, options, &mut sink).await?;
			}
			ExportFormat::Zip { .. } => {
				let file = std::fs::File::create(dest).map_err(document_container::ContainerError::Io)?;
				let mut writer = document_container::archive::Zip::writer(file)?;
				self.stream_entries(codec, options, &mut writer).await?;
				use document_container::archive::ArchiveWriter;
				writer.finish()?;
			}
			ExportFormat::Xz { .. } => {
				let file = std::fs::File::create(dest).map_err(document_container::ContainerError::Io)?;
				let mut writer = document_container::archive::Xz::writer(file)?;
				self.stream_entries(codec, options, &mut writer).await?;
				use document_container::archive::ArchiveWriter;
				writer.finish()?;
			}
		}

		let _ = options.embed_all_resources; // TODO once resource API lands

		Ok(())
	}

	/// Drive a sink through manifest → registry → history → resources, re-encoding typed payloads
	/// with `codec` and passing resources verbatim. Each entry is written one at a time so the
	/// sink only ever sees one payload's bytes at a time.
	async fn stream_entries(&self, codec: Codec, options: ExportOptions, sink: &mut dyn ExportSink) -> Result<(), ExportError> {
		use document_container::AsyncContainer;

		let manifest = self.read_manifest().await?;
		sink.write_entry(&io::path_for(self.layout.manifest_basename(), codec), &codec.write_single(&manifest)?)?;

		if options.include_registry {
			let (registry, _): (Registry, _) = io::read_single_by_basename(&self.working, self.layout.registry_basename()).await?;
			sink.write_entry(&io::path_for(self.layout.registry_basename(), codec), &codec.write_single(&registry)?)?;
		}

		if options.include_history && io::basename_exists(&self.working, self.layout.history_basename()).await {
			let (deltas, _): (Vec<Delta>, _) = io::iter_by_basename(&self.working, self.layout.history_basename()).await?;
			let mut buffer = Vec::new();
			for delta in &deltas {
				codec.append(&mut buffer, delta)?;
			}
			sink.write_entry(&io::path_for(self.layout.history_basename(), codec), &buffer)?;
		}

		let resources_dir = self.layout.resources_dir();
		if self.working.list_dirs("").await?.iter().any(|d| d == resources_dir) {
			for path in self.working.list(resources_dir).await? {
				let holder = self.working.read(&path).await?;
				match holder.source_path() {
					Some(src_path) => sink.write_entry_from_path(&path, src_path)?,
					None => sink.write_entry(&path, holder.as_slice())?,
				}
			}
		}

		Ok(())
	}
}

/// Abstraction over the sink an export streams entries into. Lets a single async loop drive
/// folder writes, zip writes, and xz writes without duplicating the entry sequence.
trait ExportSink {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<(), ExportError>;

	/// Copy a file from disk into the sink. Default impl reads the source into memory and
	/// forwards to `write_entry`; sinks like the folder writer override to use `fs::copy`
	/// (CoW on supported filesystems, kernel-side copy otherwise).
	fn write_entry_from_path(&mut self, path: &str, src: &std::path::Path) -> Result<(), ExportError> {
		let bytes = std::fs::read(src).map_err(document_container::ContainerError::Io)?;
		self.write_entry(path, &bytes)
	}
}

struct FolderSink<'a> {
	folder: &'a mut document_container::backends::folder::FolderBackend,
}

impl ExportSink for FolderSink<'_> {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<(), ExportError> {
		document_container::Container::write(self.folder, path, bytes)?;
		Ok(())
	}

	fn write_entry_from_path(&mut self, path: &str, src: &std::path::Path) -> Result<(), ExportError> {
		document_container::validate_path(path)?;
		let dest = self.folder.root().join(path);
		if let Some(parent) = dest.parent() {
			std::fs::create_dir_all(parent).map_err(document_container::ContainerError::Io)?;
		}
		std::fs::copy(src, &dest).map_err(document_container::ContainerError::Io)?;
		Ok(())
	}
}

impl<W: std::io::Write + std::io::Seek> ExportSink for document_container::archive::ZipWriter<W> {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<(), ExportError> {
		use document_container::archive::ArchiveWriter;
		ArchiveWriter::write_entry(self, path, bytes)?;
		Ok(())
	}
}

impl<W: std::io::Write + std::io::Seek> ExportSink for document_container::archive::XzWriter<W> {
	fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<(), ExportError> {
		use document_container::archive::ArchiveWriter;
		ArchiveWriter::write_entry(self, path, bytes)?;
		Ok(())
	}
}

/// Errors from [`Gdd::open`] / [`Gdd::create`]. Per design, any unexpected condition is a hard error.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
	#[error("container error: {0}")]
	Container(#[from] ContainerError),
	#[error("read error: {0}")]
	Read(#[from] ReadError),
	#[error("not a .gdd document (manifest format = {found:?}, expected {expected:?})")]
	WrongFormat { found: String, expected: &'static str },
	#[error("unsupported format version: found {found}, max supported {max_supported}")]
	UnsupportedVersion { found: u32, max_supported: u32 },
	#[error("codec error: {0}")]
	Codec(#[from] CodecError),
	#[error("CRDT error: {0}")]
	Crdt(#[from] CrdtError),
}

#[derive(Debug, thiserror::Error)]
pub enum RetireError {
	#[error("container error: {0}")]
	Container(#[from] ContainerError),
	#[error("read error: {0}")]
	Read(#[from] ReadError),
	#[error("codec error: {0}")]
	Codec(#[from] CodecError),
	#[error("CRDT error: {0}")]
	Crdt(#[from] CrdtError),
	#[error("manifest update failed: {0}")]
	Manifest(#[from] OpenError),
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
	#[error("container error: {0}")]
	Container(#[from] ContainerError),
	#[error("read error: {0}")]
	Read(#[from] ReadError),
	#[error("open error: {0}")]
	Open(#[from] OpenError),
	#[error("codec error: {0}")]
	Codec(#[from] CodecError),
	#[error("invalid export options: {0}")]
	InvalidOptions(&'static str),
}
