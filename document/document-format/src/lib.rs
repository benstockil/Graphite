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
use graphene_resource::{LoadResource, Resource, ResourceFuture, ResourceHash, ResourceStorage};

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
pub use manifest::{Manifest, PayloadCodecs};
pub use session_state::SessionState;

/// The manifest is always JSON: it is the bootstrap file, read before any other payload's codec is
/// known, so its own codec cannot itself be configurable.
pub const MANIFEST_CODEC: Codec = Codec::Json;

/// Working-copy codecs. The working copy lives in appdata, not under VCS — these defaults
/// optimize for size and write cost. JSON/JSONL is opt-in via `ExportFormat::Folder` for users
/// who want a diffable on-disk representation. Recorded in the manifest at create time and read
/// back on open (see [`manifest::PayloadCodecs`]), so the persist path never probes the filesystem.
pub const DEFAULT_SESSION_CODEC: Codec = Codec::Json;
pub const DEFAULT_REGISTRY_CODEC: Codec = Codec::Postcard;
pub const DEFAULT_HISTORY_CODEC: Codec = Codec::PostcardFrames;
pub const DEFAULT_HOT_LOG_CODEC: Codec = Codec::PostcardFrames;

/// Editor-facing handle. Owns the `Session` and the working-copy container; mutations are mirrored
/// to disk continuously (every retirement appends to the history file and re-snapshots the registry).
///
/// The per-edit persist path (`commit_from_runtime`, `apply_hot_op`, `retire`) is synchronous and
/// read-free: the manifest is cached in memory (so payload codecs and `last_retired_at` need no
/// disk read), and writes go through the container's sync write surface. Only `open` / `create` /
/// `export` are async, since they read.
pub struct Gdd<L: Layout = GddV1> {
	session: Session,
	working: AnyContainer,
	layout: L,
	/// In-memory copy of the manifest, kept authoritative since `Gdd` is its sole writer. Holds the
	/// per-payload codecs (so the persist path never probes the filesystem) and `last_retired_at`
	/// (so retirement writes the manifest without first reading it). Lets the persist path stay
	/// fully read-free and synchronous.
	manifest: Manifest,
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
		let manifest: Manifest = io::read_single(&working, layout.manifest_basename(), MANIFEST_CODEC).await?;
		validate_manifest(&manifest)?;
		let codecs = manifest.codecs;

		let session_state: SessionState = match io::exists(&working, layout.session_basename(), codecs.session).await {
			true => io::read_single(&working, layout.session_basename(), codecs.session).await?,
			false => SessionState::default(),
		};

		let has_registry = io::exists(&working, layout.registry_basename(), codecs.registry).await;
		let has_history = io::exists(&working, layout.history_basename(), codecs.history).await;

		let mut session = match (has_registry, has_history) {
			(true, true) => {
				let registry: Registry = io::read_single(&working, layout.registry_basename(), codecs.registry).await?;
				let history_map: HashMap<Rev, Delta> = load_history(&working, &layout, codecs.history).await?.into_iter().map(|delta| (delta.id, delta)).collect();
				Session::load(manifest.peer_id, registry, history_map, session_state.head_rev, session_state.next_node_counter)
			}
			(true, false) => {
				// Registry-only export: synthesize a history that reproduces this state.
				let registry: Registry = io::read_single(&working, layout.registry_basename(), codecs.registry).await?;
				Session::bootstrap_from_registry(manifest.peer_id, registry)?
			}
			(false, _) => Session::replay_from_history(manifest.peer_id, load_history(&working, &layout, codecs.history).await?, session_state.next_node_counter)?,
		};

		replay_hot_log(&working, &layout, codecs.hot_log, &mut session).await?;

		Ok(Self { session, working, layout, manifest })
	}

	/// Backend-agnostic create. Records the working-copy default codecs (see `DEFAULT_*_CODEC`) in
	/// the manifest and writes each payload with its recorded codec.
	pub async fn create_in(working: AnyContainer, layout: L, peer: PeerId, document_uuid: u64, editor_version: String, stdlib_version: String) -> Result<Self, OpenError> {
		let manifest = Manifest::new(document_uuid, peer, editor_version, stdlib_version);
		let codecs = manifest.codecs;
		io::write_single(&working, layout.manifest_basename(), MANIFEST_CODEC, &manifest)?;
		io::write_single(&working, layout.session_basename(), codecs.session, &SessionState::default())?;

		let session = Session::with_peer(peer);
		io::write_single(&working, layout.registry_basename(), codecs.registry, session.registry())?;

		Ok(Self { session, working, layout, manifest })
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

async fn load_history<L: Layout>(working: &AnyContainer, layout: &L, codec: Codec) -> Result<Vec<Delta>, OpenError> {
	if !io::exists(working, layout.history_basename(), codec).await {
		return Ok(Vec::new());
	}
	Ok(io::iter::<Delta>(working, layout.history_basename(), codec).await?)
}

async fn replay_hot_log<L: Layout>(working: &AnyContainer, layout: &L, codec: Codec, session: &mut Session) -> Result<(), OpenError> {
	if !io::exists(working, layout.hot_log_basename(), codec).await {
		return Ok(());
	}
	for hot_op in io::iter::<HotOp>(working, layout.hot_log_basename(), codec).await? {
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

	/// The in-memory manifest. `Gdd` is its sole writer, so this is authoritative without re-reading
	/// disk.
	pub fn manifest(&self) -> &Manifest {
		&self.manifest
	}

	/// Edit the cached manifest and persist it. Always JSON, synchronous.
	pub fn update_manifest(&mut self, edit: impl FnOnce(&mut Manifest)) -> Result<(), OpenError> {
		edit(&mut self.manifest);
		io::write_single(&self.working, self.layout.manifest_basename(), MANIFEST_CODEC, &self.manifest)?;
		Ok(())
	}

	/// Commit a runtime snapshot through `Session`, appending each emitted delta to the retired
	/// history file and persisting updated session-state. Synchronous: the session mutation is
	/// in-memory and the disk writes go through the container's sync write surface.
	///
	/// Interim API: forwards to `Session::commit_from_runtime`, which itself is a shim that diffs
	/// the runtime against a freshly-converted `Registry`. The long-term replacement will take
	/// runtime deltas directly. This wrapper goes away when that lands.
	pub fn commit_from_runtime<M: NodeMetadataSource>(&mut self, network: &graph_craft::document::NodeNetwork, metadata: &M) -> Result<Vec<Rev>, CommitError> {
		let revs = self.session.commit_from_runtime(network, metadata)?;
		if let Err(error) = self.persist_committed_deltas(&revs) {
			log::error!("Failed to persist committed deltas to working copy: {error}");
		}
		Ok(revs)
	}

	/// Apply a hot op from the broadcast stream, appending one frame to the hot log.
	pub fn apply_hot_op(&mut self, op: HotOp) -> Result<(), CrdtError> {
		self.session.apply_hot_op(op.clone())?;
		if let Err(error) = self.append_hot_frame(&op) {
			log::error!("Failed to append hot op frame: {error}");
		}
		Ok(())
	}

	fn persist_committed_deltas(&mut self, revs: &[Rev]) -> Result<(), OpenError> {
		if revs.is_empty() {
			return Ok(());
		}

		self.append_history_deltas(revs)?;
		self.persist_session_state()
	}

	/// Encode the history deltas identified by `revs` and append them to the history file.
	/// Single pass over the history (O(history length)), filtering by `revs` membership.
	fn append_history_deltas(&mut self, revs: &[Rev]) -> Result<(), OpenError> {
		let wanted: std::collections::HashSet<Rev> = revs.iter().copied().collect();
		let mut buffer = Vec::new();
		for delta in self.session.history().filter(|delta| wanted.contains(&delta.id)) {
			self.manifest.codecs.history.append(&mut buffer, delta)?;
		}
		self.working.append_non_blocking(&io::path_for(self.layout.history_basename(), self.manifest.codecs.history), &buffer)?;
		Ok(())
	}

	fn persist_session_state(&mut self) -> Result<(), OpenError> {
		let state = SessionState {
			head_rev: self.session.head_rev(),
			next_node_counter: self.session.next_node_counter(),
		};
		io::write_single(&self.working, self.layout.session_basename(), self.manifest.codecs.session, &state)?;
		Ok(())
	}

	fn append_hot_frame(&mut self, op: &HotOp) -> Result<(), OpenError> {
		let mut buffer = Vec::new();
		self.manifest.codecs.hot_log.append(&mut buffer, op)?;
		self.working.append_non_blocking(&io::path_for(self.layout.hot_log_basename(), self.manifest.codecs.hot_log), &buffer)?;
		Ok(())
	}

	/// Working-copy checkpoint: promote hot ops with timestamp `≤ up_to` into retired deltas,
	/// append them to the history file, rewrite the hot log with remaining (unretired) ops,
	/// re-snapshot the registry, and bump `last_retired_at` on the manifest. Synchronous.
	pub fn retire(&mut self, up_to: TimeStamp) -> Result<(), RetireError> {
		let new_revs = self.session.retire(up_to)?;

		if !new_revs.is_empty() {
			self.append_history_deltas(&new_revs)?;
		}

		// Rewrite hot log with whatever survived retirement.
		let mut hot_buffer = Vec::new();
		for hot_op in self.session.hot_log() {
			self.manifest.codecs.hot_log.append(&mut hot_buffer, hot_op)?;
		}
		self.working
			.store_non_blocking(&io::path_for(self.layout.hot_log_basename(), self.manifest.codecs.hot_log), &hot_buffer)?;

		// Re-snapshot registry.
		io::write_single(&self.working, self.layout.registry_basename(), self.manifest.codecs.registry, self.session.registry())?;

		self.persist_session_state()?;

		// Bump cached manifest timestamp and persist it.
		self.update_manifest(|m| m.last_retired_at = Some(chrono::Utc::now().to_rfc3339()))?;

		Ok(())
	}

	pub async fn read_resource(&self, hash: &ResourceHash) -> Result<ByteHolder, ContainerError> {
		self.working.read(&self.layout.resource_path(hash)).await
	}

	pub fn add_resource(&self, hash: ResourceHash, bytes: &[u8]) -> Result<(), ContainerError> {
		self.working.store_non_blocking(&self.layout.resource_path(&hash), bytes)
	}

	/// Add a resource by copying from `src` rather than buffering its bytes. Folder backends use
	/// `fs::copy` (CoW on supported filesystems); other backends fall back to read-then-write.
	pub fn add_resource_from_path(&self, hash: ResourceHash, src: &Path) -> Result<(), ContainerError> {
		let dest_path = self.layout.resource_path(&hash);
		if let AnyContainer::Folder(folder) = &self.working {
			let full = folder.root().join(&dest_path);
			if let Some(parent) = full.parent() {
				std::fs::create_dir_all(parent).map_err(ContainerError::Io)?;
			}
			std::fs::copy(src, &full).map_err(ContainerError::Io)?;
			return Ok(());
		}
		let bytes = std::fs::read(src).map_err(ContainerError::Io)?;
		self.working.store_non_blocking(&dest_path, &bytes)
	}

	pub async fn has_resource(&self, hash: &ResourceHash) -> bool {
		self.working.exists(&self.layout.resource_path(hash)).await
	}

	pub fn remove_resource(&self, hash: &ResourceHash) -> Result<(), ContainerError> {
		self.working.remove_non_blocking(&self.layout.resource_path(hash))
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
	/// sink only ever sees one payload's bytes at a time. The exported manifest's codec map is
	/// rewritten to `codec` so it stays authoritative for the re-encoded payloads; the manifest
	/// itself is always JSON.
	async fn stream_entries(&self, codec: Codec, options: ExportOptions, sink: &mut dyn ExportSink) -> Result<(), ExportError> {
		use document_container::AsyncContainer;

		let mut manifest = self.manifest.clone();
		manifest.codecs = PayloadCodecs {
			registry: codec,
			history: codec,
			hot_log: codec,
			session: codec,
		};
		sink.write_entry(&io::path_for(self.layout.manifest_basename(), MANIFEST_CODEC), &MANIFEST_CODEC.write_single(&manifest)?)?;

		if options.include_registry {
			let registry: Registry = io::read_single(&self.working, self.layout.registry_basename(), self.manifest.codecs.registry).await?;
			sink.write_entry(&io::path_for(self.layout.registry_basename(), codec), &codec.write_single(&registry)?)?;
		}

		if options.include_history && io::exists(&self.working, self.layout.history_basename(), self.manifest.codecs.history).await {
			let deltas: Vec<Delta> = io::iter(&self.working, self.layout.history_basename(), self.manifest.codecs.history).await?;
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

impl<L: Layout + Send + Sync> LoadResource for Gdd<L> {
	fn load(&self, hash: ResourceHash) -> ResourceFuture<'_> {
		Box::pin(async move {
			let bytes = self.working.read(&self.layout.resource_path(&hash)).await.ok()?;
			Some(Resource::new(bytes))
		})
	}
}

impl<L: Layout + Send + Sync> ResourceStorage for Gdd<L> {
	fn store(&self, data: &[u8]) -> ResourceHash {
		let hash = ResourceHash::from(data);
		if let Err(error) = self.working.store_non_blocking(&self.layout.resource_path(&hash), data) {
			log::error!("ResourceStorage::store failed for {hash}: {error}");
		}
		hash
	}

	fn contains(&self, hash: &ResourceHash) -> bool {
		self.working.exists_non_blocking(&self.layout.resource_path(hash))
	}

	fn garbage_collect(&self, used: &[ResourceHash]) {
		let kept: std::collections::HashSet<&ResourceHash> = used.iter().collect();
		let hashes = match futures::executor::block_on(self.resource_hashes()) {
			Ok(hashes) => hashes,
			Err(error) => {
				log::error!("Failed to list resources during garbage_collect: {error}");
				return;
			}
		};
		for hash in hashes {
			if kept.contains(&hash) {
				continue;
			}
			if let Err(error) = self.working.remove_non_blocking(&self.layout.resource_path(&hash)) {
				log::error!("ResourceStorage::garbage_collect failed to remove {hash}: {error}");
			}
		}
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
