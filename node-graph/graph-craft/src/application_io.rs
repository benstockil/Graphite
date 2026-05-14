use dyn_any::StaticType;
use std::sync::Mutex;
#[cfg(feature = "wgpu")]
use wgpu_executor::WgpuExecutor;

pub mod resource;

pub use graphene_application_io::{ApplicationIo, Resource, ResourceHash, ResourceStorage};
pub use resource::HashMapResourceStorage;
#[cfg(target_family = "wasm")]
pub use resource::indexed_db::IndexedDbResourceStorage;
#[cfg(not(target_family = "wasm"))]
pub use resource::mmap::MmapResourceStorage;

pub struct PlatformApplicationIo {
	#[cfg(feature = "wgpu")]
	pub(crate) gpu_executor: Option<WgpuExecutor>,
	resources: Mutex<Box<dyn ResourceStorage>>,
}

impl PlatformApplicationIo {
	pub async fn new(resources: Box<dyn ResourceStorage>) -> Self {
		#[cfg(feature = "wgpu")]
		let executor = WgpuExecutor::new().await;

		#[cfg(not(feature = "wgpu"))]
		let wgpu_available = false;
		#[cfg(feature = "wgpu")]
		let wgpu_available = executor.is_some();
		set_wgpu_available(wgpu_available);

		Self {
			#[cfg(feature = "wgpu")]
			gpu_executor: executor,
			resources: Mutex::new(resources),
		}
	}

	#[cfg(feature = "wgpu")]
	pub fn new_with_context(context: wgpu_executor::WgpuContext, resources: Box<dyn ResourceStorage>) -> Self {
		let executor = WgpuExecutor::with_context(context);

		let wgpu_available = executor.is_some();
		set_wgpu_available(wgpu_available);

		Self {
			gpu_executor: executor,
			resources: Mutex::new(resources),
		}
	}
}

impl ApplicationIo for PlatformApplicationIo {
	#[cfg(feature = "wgpu")]
	type Executor = WgpuExecutor;
	#[cfg(not(feature = "wgpu"))]
	type Executor = ();

	#[cfg(feature = "wgpu")]
	fn gpu_executor(&self) -> Option<&Self::Executor> {
		self.gpu_executor.as_ref()
	}

	fn load_resource(&self, hash: &ResourceHash) -> Option<Resource> {
		self.resources().read(hash)
	}
}

impl PlatformApplicationIo {
	pub fn resources(&self) -> std::sync::MutexGuard<'_, Box<dyn ResourceStorage>> {
		self.resources.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

impl std::fmt::Debug for PlatformApplicationIo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PlatformApplicationIo").finish_non_exhaustive()
	}
}

unsafe impl StaticType for PlatformApplicationIo {
	type Static = PlatformApplicationIo;
}

pub type PlatformEditorApi = graphene_application_io::EditorApi<PlatformApplicationIo>;

static WGPU_AVAILABLE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

/// Returns:
/// - `None` if the availability of WGPU has not been determined yet
/// - `Some(true)` if WGPU is available
/// - `Some(false)` if WGPU is not available
pub fn wgpu_available() -> Option<bool> {
	match WGPU_AVAILABLE.load(std::sync::atomic::Ordering::SeqCst) {
		-1 => None,
		0 => Some(false),
		_ => Some(true),
	}
}

pub(crate) fn set_wgpu_available(available: bool) {
	WGPU_AVAILABLE.store(available as i8, std::sync::atomic::Ordering::SeqCst);
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[derive(Clone, Debug, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EditorPreferences {
	/// Maximum render region size in pixels along one dimension of the square area.
	pub max_render_region_size: u32,
}

impl graphene_application_io::GetEditorPreferences for EditorPreferences {
	fn max_render_region_area(&self) -> u32 {
		let size = self.max_render_region_size.min(u32::MAX.isqrt());
		size.pow(2)
	}
}

impl Default for EditorPreferences {
	fn default() -> Self {
		Self { max_render_region_size: 1280 }
	}
}

unsafe impl StaticType for EditorPreferences {
	type Static = EditorPreferences;
}
