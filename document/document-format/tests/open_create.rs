use document_container::AnyContainer;
use document_container::backends::memory::MemoryBackend;
use document_format::{Codec, Gdd, GddV1, Layout, Manifest, OpenError, io, manifest};
use graph_storage::{HotOp, Network, NetworkId, PeerId, ROOT_NETWORK, RegistryDelta, TimeStamp};

fn empty_container() -> AnyContainer {
	AnyContainer::Memory(MemoryBackend::new())
}

#[test]
fn create_in_round_trips_empty_document() {
	futures::executor::block_on(async {
		let container = empty_container();

		let created = match Gdd::<GddV1>::create_in(container, GddV1, PeerId(7), 0xFEED, "editor-x".into(), "stdlib-x".into()).await {
			Ok(gdd) => gdd,
			Err(error) => panic!("create_in failed: {error:?}"),
		};

		let (working, layout) = created.into_storage();
		let reopened = match Gdd::<GddV1>::open_in(working, layout).await {
			Ok(gdd) => gdd,
			Err(error) => panic!("open_in failed: {error:?}"),
		};

		assert_eq!(reopened.session().peer(), PeerId(7));
		assert!(reopened.registry().node_instances.is_empty());
		assert!(reopened.registry().networks.is_empty());
	});
}

#[test]
fn open_in_rejects_wrong_format_magic() {
	futures::executor::block_on(async {
		let mut container = empty_container();
		let layout = GddV1;

		let mut bogus = Manifest::new(0xC0DE, PeerId(1), "ed".into(), "std".into());
		bogus.format = "not-gdd".into();
		io::write_single_by_basename(&mut container, layout.manifest_basename(), Codec::Json, &bogus).await.unwrap();

		match Gdd::<GddV1>::open_in(container, layout).await {
			Err(OpenError::WrongFormat { .. }) => {}
			Ok(_) => panic!("expected WrongFormat, got Ok"),
			Err(other) => panic!("expected WrongFormat, got {other:?}"),
		}
	});
}

#[test]
fn open_in_picks_up_alternate_codec_for_manifest() {
	// Create with the working-copy defaults (manifest=Json), then rewrite the manifest as JsonLines
	// at the same basename. Open should still succeed: the basename-keyed reader picks up whichever
	// extension exists on disk.
	futures::executor::block_on(async {
		let created = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(11), 0xBEEF, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));
		let (mut working, layout) = created.into_storage();

		// Read the current manifest, drop the .json copy, re-emit as .jsonl.
		let (manifest, _): (Manifest, _) = io::read_single_by_basename(&working, layout.manifest_basename()).await.unwrap();
		use document_container::AsyncContainer;
		working.remove(&io::path_for(layout.manifest_basename(), Codec::Json)).await.unwrap();
		io::write_single_by_basename(&mut working, layout.manifest_basename(), Codec::JsonLines, &manifest).await.unwrap();

		let reopened = match Gdd::<GddV1>::open_in(working, layout).await {
			Ok(gdd) => gdd,
			Err(error) => panic!("open_in failed: {error:?}"),
		};
		assert_eq!(reopened.session().peer(), PeerId(11));
	});
}

#[test]
fn read_manifest_returns_what_create_in_wrote() {
	futures::executor::block_on(async {
		let gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(13), 0xC0FFEE, "ed-1.2".into(), "std-0.7".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let manifest = gdd.read_manifest().await.unwrap_or_else(|error| panic!("read_manifest failed: {error:?}"));
		assert_eq!(manifest.peer_id, PeerId(13));
		assert_eq!(manifest.document_uuid, 0xC0FFEE);
		assert_eq!(manifest.editor_version, "ed-1.2");
		assert_eq!(manifest.stdlib_version, "std-0.7");
		assert_eq!(manifest.format, manifest::FORMAT_MAGIC);
	});
}

#[test]
fn update_manifest_changes_visible_after_reopen() {
	futures::executor::block_on(async {
		let mut gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(1), 0xAB, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		gdd.update_manifest(|m| m.editor_version = "ed-NEW".into())
			.await
			.unwrap_or_else(|error| panic!("update_manifest failed: {error:?}"));

		let (working, layout) = gdd.into_storage();
		let reopened = Gdd::<GddV1>::open_in(working, layout).await.unwrap_or_else(|error| panic!("open_in failed: {error:?}"));
		let manifest = reopened.read_manifest().await.unwrap();
		assert_eq!(manifest.editor_version, "ed-NEW");
	});
}

#[test]
fn apply_hot_op_persists_to_hot_log_and_survives_reopen() {
	futures::executor::block_on(async {
		let mut gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(5), 0xDEAD, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		// AddNetwork on the root network. Idempotent at apply, so two hot ops applied in sequence
		// produces one network in the registry.
		let hot_op = HotOp {
			op: RegistryDelta::AddNetwork {
				network: ROOT_NETWORK,
				contents: Network::default(),
			},
			timestamp: TimeStamp { counter: 1, peer: PeerId(5) },
			author: PeerId(5),
		};
		gdd.apply_hot_op(hot_op).await.unwrap_or_else(|error| panic!("apply_hot_op failed: {error:?}"));

		assert!(gdd.registry().networks.contains_key(&ROOT_NETWORK), "hot op should have created the root network in memory");

		let (working, layout) = gdd.into_storage();
		let reopened = Gdd::<GddV1>::open_in(working, layout).await.unwrap_or_else(|error| panic!("open_in failed: {error:?}"));

		assert!(reopened.registry().networks.contains_key(&ROOT_NETWORK), "hot op should have been replayed from the hot log on reopen");
	});
}

#[test]
fn retire_moves_eligible_hot_ops_to_history_and_keeps_rest() {
	futures::executor::block_on(async {
		let mut gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(5), 0xDEAD, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		// Two hot ops: one with low timestamp (will retire), one with high (will stay).
		let early = HotOp {
			op: RegistryDelta::AddNetwork {
				network: ROOT_NETWORK,
				contents: Network::default(),
			},
			timestamp: TimeStamp { counter: 1, peer: PeerId(5) },
			author: PeerId(5),
		};
		let late = HotOp {
			op: RegistryDelta::AddNetwork {
				network: NetworkId::from(42_u64),
				contents: Network::default(),
			},
			timestamp: TimeStamp { counter: 10, peer: PeerId(5) },
			author: PeerId(5),
		};
		gdd.apply_hot_op(early).await.unwrap();
		gdd.apply_hot_op(late).await.unwrap();
		assert_eq!(gdd.session().hot_log().len(), 2);

		// Retire only up to timestamp 5 → drains the early op, leaves the late one.
		let cutoff = TimeStamp { counter: 5, peer: PeerId(5) };
		gdd.retire(cutoff).await.unwrap_or_else(|error| panic!("retire failed: {error:?}"));

		assert_eq!(gdd.session().hot_log().len(), 1, "late hot op should still be in hot log");
		assert_eq!(gdd.session().history().count(), 1, "early hot op should be in retired history");

		// Reopen and confirm survival: hot log has the late op (replayed), history has the early op.
		let (working, layout) = gdd.into_storage();
		let reopened = Gdd::<GddV1>::open_in(working, layout).await.unwrap_or_else(|error| panic!("open_in failed: {error:?}"));

		assert!(reopened.registry().networks.contains_key(&ROOT_NETWORK), "retired op's effect should be in registry");
		assert!(reopened.registry().networks.contains_key(&NetworkId::from(42_u64)), "hot op's effect should be replayed");
		assert_eq!(reopened.session().history().count(), 1);
		assert_eq!(reopened.session().hot_log().len(), 1);

		// Manifest bumped.
		let manifest = reopened.read_manifest().await.unwrap();
		assert!(manifest.last_retired_at.is_some(), "retire should bump last_retired_at");
	});
}

#[test]
fn export_folder_round_trips_through_open() {
	use document_format::{ExportFormat, ExportOptions};

	futures::executor::block_on(async {
		let gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(3), 0xAB, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path().join("export");

		gdd.export(&dest, ExportFormat::Folder { codec: Codec::Json }, ExportOptions::default())
			.await
			.unwrap_or_else(|error| panic!("export failed: {error:?}"));

		// Manifest re-encoded with the export codec: registry as JSON instead of postcard.
		assert!(dest.join("registry.json").exists());
		assert!(dest.join("manifest.json").exists());
		// session.json + hot-log are peer-local / ephemeral and should not appear in exports.
		assert!(!dest.join("session.json").exists());
		assert!(!dest.join("session.bin").exists());
		assert!(!dest.join("hot-log.bin").exists());
		assert!(!dest.join("hot-log.frames").exists());

		// And the export is itself openable.
		let reopened = Gdd::<GddV1>::open(&dest).await.unwrap_or_else(|error| panic!("open failed: {error:?}"));
		assert_eq!(reopened.session().peer(), PeerId(3));
	});
}

#[test]
fn export_zip_round_trips_via_deserialize() {
	use document_container::archive::{Archive, Zip};
	use document_format::{ExportFormat, ExportOptions};

	futures::executor::block_on(async {
		let gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(4), 0xCD, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path().join("doc.gdd.zip");

		gdd.export(&dest, ExportFormat::Zip { codec: Codec::Postcard }, ExportOptions::default())
			.await
			.unwrap_or_else(|error| panic!("export failed: {error:?}"));

		let bytes = std::fs::read(&dest).unwrap();
		let restored = Zip::deserialize(&bytes).unwrap();
		use document_container::Container;
		assert!(restored.exists("manifest.bin"));
		assert!(restored.exists("registry.bin"));
		assert!(!restored.exists("session.json"));
		assert!(!restored.exists("session.bin"));
		assert!(!restored.exists("hot-log.frames"));
	});
}

#[test]
fn export_rejects_invalid_options() {
	use document_format::{ExportError, ExportFormat, ExportOptions};

	futures::executor::block_on(async {
		let gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(1), 0xEF, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path().join("nope");
		let options = ExportOptions {
			include_registry: false,
			include_history: false,
			embed_all_resources: false,
		};

		match gdd.export(&dest, ExportFormat::Folder { codec: Codec::Json }, options).await {
			Err(ExportError::InvalidOptions(_)) => {}
			Ok(_) => panic!("expected InvalidOptions, got Ok"),
			Err(other) => panic!("expected InvalidOptions, got {other:?}"),
		}
	});
}

#[test]
fn resource_round_trip_add_read_remove() {
	use graphene_resource::ResourceHash;

	futures::executor::block_on(async {
		let mut gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(99), 0xCAFE, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let payload = b"deadbeef cafe babe";
		let hash = ResourceHash::from(&payload[..]);

		assert!(!gdd.has_resource(&hash).await);
		gdd.add_resource(hash, payload).await.unwrap_or_else(|error| panic!("add_resource failed: {error:?}"));
		assert!(gdd.has_resource(&hash).await);

		let read_back = gdd.read_resource(&hash).await.unwrap();
		assert_eq!(read_back.as_slice(), payload);

		let hashes = gdd.resource_hashes().await.unwrap();
		assert_eq!(hashes, vec![hash]);

		gdd.remove_resource(&hash).await.unwrap();
		assert!(!gdd.has_resource(&hash).await);
	});
}

#[test]
fn resource_survives_reopen() {
	use graphene_resource::ResourceHash;

	futures::executor::block_on(async {
		let mut gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(7), 0xC0DE, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let payload = b"persistent bytes";
		let hash = ResourceHash::from(&payload[..]);
		gdd.add_resource(hash, payload).await.unwrap();

		let (working, layout) = gdd.into_storage();
		let reopened = Gdd::<GddV1>::open_in(working, layout).await.unwrap_or_else(|error| panic!("open_in failed: {error:?}"));

		assert!(reopened.has_resource(&hash).await);
		assert_eq!(reopened.read_resource(&hash).await.unwrap().as_slice(), payload);
	});
}

#[test]
fn resource_from_path_uses_fs_copy_on_folder_backend() {
	use document_container::AnyContainer;
	use document_container::backends::folder::FolderBackend;
	use graphene_resource::ResourceHash;

	futures::executor::block_on(async {
		// Need a folder-backed working copy to exercise the fs::copy path.
		let working_dir = tempfile::tempdir().unwrap();
		let working = AnyContainer::Folder(FolderBackend::create(working_dir.path()).unwrap());
		let mut gdd = Gdd::<GddV1>::create_in(working, GddV1, PeerId(1), 0xAB, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		// Source file outside the working copy.
		let payload = b"external resource bytes";
		let src_dir = tempfile::tempdir().unwrap();
		let src_path = src_dir.path().join("blob");
		std::fs::write(&src_path, payload).unwrap();

		let hash = ResourceHash::from(&payload[..]);
		gdd.add_resource_from_path(hash, &src_path)
			.await
			.unwrap_or_else(|error| panic!("add_resource_from_path failed: {error:?}"));

		assert!(gdd.has_resource(&hash).await);
		assert_eq!(gdd.read_resource(&hash).await.unwrap().as_slice(), payload);
	});
}

#[test]
fn export_carries_resources() {
	use document_format::{ExportFormat, ExportOptions};
	use graphene_resource::ResourceHash;

	futures::executor::block_on(async {
		let mut gdd = Gdd::<GddV1>::create_in(empty_container(), GddV1, PeerId(2), 0xBC, "ed".into(), "std".into())
			.await
			.unwrap_or_else(|error| panic!("create_in failed: {error:?}"));

		let payload = b"exported resource";
		let hash = ResourceHash::from(&payload[..]);
		gdd.add_resource(hash, payload).await.unwrap();

		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path().join("export");
		gdd.export(&dest, ExportFormat::Folder { codec: Codec::Json }, ExportOptions::default()).await.unwrap();

		let resource_file = dest.join("resources").join(format!("{hash}"));
		assert!(resource_file.exists(), "exported resource file should exist at {resource_file:?}");
		assert_eq!(std::fs::read(&resource_file).unwrap(), payload);
	});
}

#[test]
fn open_in_rejects_future_format_version() {
	futures::executor::block_on(async {
		let mut container = empty_container();
		let layout = GddV1;

		let mut future_version = Manifest::new(0xC0DE, PeerId(1), "ed".into(), "std".into());
		future_version.format_version = manifest::SUPPORTED_FORMAT_VERSION + 1;
		io::write_single_by_basename(&mut container, layout.manifest_basename(), Codec::Json, &future_version).await.unwrap();

		match Gdd::<GddV1>::open_in(container, layout).await {
			Err(OpenError::UnsupportedVersion { .. }) => {}
			Ok(_) => panic!("expected UnsupportedVersion, got Ok"),
			Err(other) => panic!("expected UnsupportedVersion, got {other:?}"),
		}
	});
}
