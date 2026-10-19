//! Android `SurfaceView` ownership and lifecycle boundary.
//!
//! Java receives an opaque registry ID, never an owned Rust pointer. The
//! registry keeps the acquired `ANativeWindow` reference alive until the GPU
//! worker acknowledges detach. This prevents stale JNI handles from turning
//! into use-after-free pointer dereferences.

use std::{ffi::c_void, fmt, ptr::NonNull};

#[repr(C)]
struct ANativeWindow {
    _private: [u8; 0],
}

#[cfg(target_os = "android")]
use crate::gpu_host::{GpuHostControl, GpuHostError};

#[cfg(target_os = "android")]
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, OnceLock},
};

#[cfg(target_os = "android")]
use jni::{
    objects::{JClass, JObject},
    sys::{jboolean, jint, jlong, jobject},
    JNIEnv,
};

#[cfg(target_os = "android")]
#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jobject,
    ) -> *mut ANativeWindow;
    fn ANativeWindow_release(window: *mut ANativeWindow);
}

/// The Java surface state visible to the renderer integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceState {
    Detached,
    Attached,
    Suspended,
}

/// A checked target returned by [`NativeSurfaceBridge::present_target`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentTarget {
    pub window_address: usize,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceError {
    NullSurface,
    NullWindow,
    UnsupportedPlatform,
    InvalidHandle,
    InvalidSize,
    NotAttached,
    AlreadyAttached,
    NotConfigured,
    Suspended,
    RegistryPoisoned,
    Gpu(String),
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullSurface => f.write_str("Java Surface is null"),
            Self::NullWindow => f.write_str("ANativeWindow_fromSurface returned null"),
            Self::UnsupportedPlatform => f.write_str("ANativeWindow is available only on Android"),
            Self::InvalidHandle => f.write_str("native surface handle is null or invalid"),
            Self::InvalidSize => f.write_str("surface size must be non-zero"),
            Self::NotAttached => f.write_str("surface is detached"),
            Self::AlreadyAttached => f.write_str("surface is already attached"),
            Self::NotConfigured => f.write_str("surface has no configured size"),
            Self::Suspended => f.write_str("surface is suspended"),
            Self::RegistryPoisoned => f.write_str("native surface registry is unavailable"),
            Self::Gpu(error) => write!(f, "native GPU surface operation failed: {error}"),
        }
    }
}

impl std::error::Error for SurfaceError {}

/// Pure state machine used by the JNI owner and its tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceLifecycle {
    state: SurfaceState,
    size: Option<(u32, u32)>,
}

impl Default for SurfaceLifecycle {
    fn default() -> Self {
        Self {
            state: SurfaceState::Detached,
            size: None,
        }
    }
}

impl SurfaceLifecycle {
    pub fn state(&self) -> SurfaceState {
        self.state
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    pub fn attach(&mut self) -> Result<(), SurfaceError> {
        if self.state != SurfaceState::Detached {
            return Err(SurfaceError::AlreadyAttached);
        }
        self.state = SurfaceState::Attached;
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::InvalidSize);
        }
        if self.state == SurfaceState::Detached {
            return Err(SurfaceError::NotAttached);
        }
        self.size = Some((width, height));
        Ok(())
    }

    pub fn suspend(&mut self) -> Result<(), SurfaceError> {
        match self.state {
            SurfaceState::Attached => {
                self.state = SurfaceState::Suspended;
                Ok(())
            }
            SurfaceState::Suspended => Ok(()),
            SurfaceState::Detached => Err(SurfaceError::NotAttached),
        }
    }

    pub fn resume(&mut self) -> Result<(), SurfaceError> {
        match self.state {
            SurfaceState::Suspended => {
                self.state = SurfaceState::Attached;
                Ok(())
            }
            SurfaceState::Attached => Ok(()),
            SurfaceState::Detached => Err(SurfaceError::NotAttached),
        }
    }

    pub fn detach(&mut self) {
        self.state = SurfaceState::Detached;
        self.size = None;
    }

    pub fn present_ready(&self) -> Result<(u32, u32), SurfaceError> {
        match self.state {
            SurfaceState::Detached => Err(SurfaceError::NotAttached),
            SurfaceState::Suspended => Err(SurfaceError::Suspended),
            SurfaceState::Attached => self.size.ok_or(SurfaceError::NotConfigured),
        }
    }
}

/// Owns one reference returned by `ANativeWindow_fromSurface`.
///
/// # Safety
///
/// The activity owns the Java `Surface` lifecycle. The bridge owns one NDK
/// window reference and is dropped only after the renderer has acknowledged
/// detach. Methods are serialized by the surface registry.
pub struct NativeSurfaceBridge {
    window: NonNull<ANativeWindow>,
    lifecycle: SurfaceLifecycle,
}

// The pointer is only accessed while a registry mutex is held. Ownership of
// the NDK reference remains with this value across the renderer channel.
unsafe impl Send for NativeSurfaceBridge {}

impl NativeSurfaceBridge {
    /// Take ownership of one already-acquired `ANativeWindow` reference.
    ///
    /// # Safety
    ///
    /// `window` must be a live pointer returned by
    /// `ANativeWindow_fromSurface` (or an equivalent acquired NDK reference).
    /// The caller transfers exactly one reference to this value.
    #[cfg(target_os = "android")]
    pub unsafe fn from_owned_anative_window(window: *mut c_void) -> Result<Self, SurfaceError> {
        let window = NonNull::new(window.cast::<ANativeWindow>()).ok_or(SurfaceError::NullWindow)?;
        Ok(Self { window, lifecycle: SurfaceLifecycle::default() })
    }

    #[cfg(not(target_os = "android"))]
    pub unsafe fn from_owned_anative_window(_window: *mut c_void) -> Result<Self, SurfaceError> {
        Err(SurfaceError::UnsupportedPlatform)
    }

    #[cfg(target_os = "android")]
    pub fn from_surface(env: &mut JNIEnv<'_>, surface: &JObject<'_>) -> Result<Self, SurfaceError> {
        if surface.as_raw().is_null() {
            return Err(SurfaceError::NullSurface);
        }
        let window = unsafe { ANativeWindow_fromSurface(env.get_native_interface(), surface.as_raw()) };
        // `fromSurface` transfers one acquired native reference to this value.
        unsafe { Self::from_owned_anative_window(window.cast()) }
    }

    pub fn state(&self) -> SurfaceState {
        self.lifecycle.state()
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.lifecycle.size()
    }

    pub fn window_ptr(&self) -> *mut c_void {
        self.window.as_ptr().cast()
    }

    pub fn window_address(&self) -> usize {
        self.window.as_ptr() as usize
    }

    pub fn attach(&mut self) -> Result<(), SurfaceError> {
        self.lifecycle.attach()
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), SurfaceError> {
        self.lifecycle.resize(width, height)
    }

    pub fn suspend(&mut self) -> Result<(), SurfaceError> {
        self.lifecycle.suspend()
    }

    pub fn resume(&mut self) -> Result<(), SurfaceError> {
        self.lifecycle.resume()
    }

    pub fn detach(&mut self) {
        self.lifecycle.detach();
    }

    /// Validate the native target for the renderer's next ring `PRESENT`.
    /// This method does not submit a GPU frame.
    pub fn present_target(&self) -> Result<PresentTarget, SurfaceError> {
        let (width, height) = self.lifecycle.present_ready()?;
        Ok(PresentTarget { window_address: self.window_address(), width, height })
    }
}

impl Drop for NativeSurfaceBridge {
    fn drop(&mut self) {
        #[cfg(target_os = "android")]
        unsafe {
            ANativeWindow_release(self.window.as_ptr());
        }
    }
}

#[cfg(target_os = "android")]
struct SurfaceEntry {
    bridge: NativeSurfaceBridge,
    gpu: Option<GpuHostControl>,
}

#[cfg(target_os = "android")]
struct SurfaceRegistry {
    next_id: u64,
    host: Option<GpuHostControl>,
    entries: HashMap<u64, SurfaceEntry>,
}

#[cfg(target_os = "android")]
impl Default for SurfaceRegistry {
    fn default() -> Self {
        Self { next_id: 1, host: None, entries: HashMap::new() }
    }
}

#[cfg(target_os = "android")]
static SURFACES: OnceLock<Mutex<SurfaceRegistry>> = OnceLock::new();

#[cfg(target_os = "android")]
fn registry() -> &'static Mutex<SurfaceRegistry> {
    SURFACES.get_or_init(|| Mutex::new(SurfaceRegistry::default()))
}

#[cfg(target_os = "android")]
fn lock_registry() -> Result<MutexGuard<'static, SurfaceRegistry>, SurfaceError> {
    registry().lock().map_err(|_| SurfaceError::RegistryPoisoned)
}

#[cfg(target_os = "android")]
fn gpu_error(error: GpuHostError) -> SurfaceError {
    SurfaceError::Gpu(error.to_string())
}

#[cfg(target_os = "android")]
fn handle_id(handle: jlong) -> Result<u64, SurfaceError> {
    u64::try_from(handle).ok().filter(|id| *id != 0).ok_or(SurfaceError::InvalidHandle)
}

#[cfg(target_os = "android")]
fn take_surface_entry(id: u64) -> Result<SurfaceEntry, SurfaceError> {
    let mut surfaces = lock_registry()?;
    surfaces.entries.remove(&id).ok_or(SurfaceError::InvalidHandle)
}

#[cfg(target_os = "android")]
fn restore_surface_entry(id: u64, entry: SurfaceEntry) -> Result<(), SurfaceError> {
    match lock_registry() {
        Ok(mut surfaces) => {
            if surfaces.entries.contains_key(&id) {
                std::mem::forget(entry);
                return Err(SurfaceError::Gpu("surface registry handle was reused".to_string()));
            }
            surfaces.entries.insert(id, entry);
            Ok(())
        }
        Err(error) => {
            // Keep the acquired ANativeWindow reference alive if the registry
            // cannot be restored after a GPU request.
            std::mem::forget(entry);
            Err(error)
        }
    }
}

/// Register the active GPU worker and attach all surfaces created before the
/// engine was ready. Calls block until each worker request is acknowledged.
#[cfg(target_os = "android")]
pub fn register_gpu_host(control: GpuHostControl) -> Result<(), SurfaceError> {
    let entries: Vec<(u64, usize, Option<(u32, u32)>)> = {
        let mut surfaces = lock_registry()?;
        if surfaces.host.is_some() {
            return Err(SurfaceError::Gpu("native GPU host is already registered".to_string()));
        }
        // Publish the control handle before waiting. A SurfaceView created
        // during registration then gets the same lifetime guard and control
        // path instead of racing an unregistered host.
        surfaces.host = Some(control.clone());
        surfaces
            .entries
            .iter()
            .map(|(id, entry)| (*id, entry.bridge.window_address(), entry.bridge.size()))
            .collect()
    };
    for (_id, window_address, size) in entries {
        if let Err(error) = control.attach_surface(window_address) {
            let detached = control.detach_surface().is_ok();
            let mut surfaces = lock_registry()?;
            for entry in surfaces.entries.values_mut() {
                entry.gpu = if detached { None } else { Some(control.clone()) };
            }
            if detached { surfaces.host = None; }
            return Err(gpu_error(error));
        }
        // Record the current id before resize. A failed resize must also
        // detach this just-attached window; otherwise the renderer keeps a
        // raw pointer after the registry reports failure.
        if let Some((width, height)) = size {
            if let Err(error) = control.resize_surface(width, height) {
                let detached = control.detach_surface().is_ok();
                let mut surfaces = lock_registry()?;
                for entry in surfaces.entries.values_mut() {
                    entry.gpu = if detached { None } else { Some(control.clone()) };
                }
                if detached {
                    surfaces.host = None;
                }
                return Err(gpu_error(error));
            }
        }
    }
    let mut surfaces = lock_registry()?;
    for entry in surfaces.entries.values_mut() {
        entry.gpu = Some(control.clone());
    }
    surfaces.host = Some(control);
    Ok(())
}

/// Detach every registered surface before dropping the worker.
#[cfg(target_os = "android")]
pub fn unregister_gpu_host() -> Result<(), SurfaceError> {
    let targets: Vec<(u64, GpuHostControl)> = {
        let surfaces = lock_registry()?;
        surfaces
            .entries
            .iter()
            .filter_map(|(id, entry)| entry.gpu.clone().map(|control| (*id, control)))
            .collect()
    };
    let mut detached = Vec::new();
    let mut first_error = None;
    for (id, control) in targets {
        match control.detach_surface() {
            Ok(()) => detached.push(id),
            Err(error) if first_error.is_none() => first_error = Some(gpu_error(error)),
            Err(_) => {}
        }
    }
    let mut surfaces = lock_registry()?;
    for id in detached {
        if let Some(entry) = surfaces.entries.get_mut(&id) {
            entry.gpu = None;
        }
    }
    if let Some(error) = first_error {
        // Keep the host and failed entries registered. Their NDK references
        // remain live so a delayed worker detach cannot use freed memory.
        return Err(error);
    }
    surfaces.host = None;
    Ok(())
}

#[cfg(target_os = "android")]
fn throw_surface_error(env: &mut JNIEnv<'_>, error: SurfaceError) {
    let _ = env.throw_new("java/lang/IllegalStateException", error.to_string());
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_algoartist_gta5native_NativeSurfaceView_nativeAttach(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    surface: JObject<'_>,
) -> jlong {
    let result = (|| {
        let mut bridge = NativeSurfaceBridge::from_surface(&mut env, &surface)?;
        bridge.attach()?;
        let window_address = bridge.window_address();
        let (id, gpu) = {
            let mut surfaces = lock_registry()?;
            let id = surfaces.next_id;
            if id > i64::MAX as u64 {
                return Err(SurfaceError::InvalidHandle);
            }
            surfaces.next_id = id.checked_add(1).ok_or(SurfaceError::InvalidHandle)?;
            let gpu = surfaces.host.clone();
            // Publish the lifetime guard before waiting for the renderer. If
            // the request times out, the entry stays reachable and its NDK
            // reference cannot be released while the renderer may still use
            // the window.
            surfaces.entries.insert(id, SurfaceEntry { bridge, gpu: gpu.clone() });
            (id, gpu)
        };
        if let Some(control) = gpu {
            if let Err(error) = control.attach_surface(window_address) {
                // A successful detach is the only point at which it is safe
                // to remove the lifetime guard after an attach failure.
                if control.detach_surface().is_ok() {
                    if let Ok(mut surfaces) = lock_registry() {
                        surfaces.entries.remove(&id);
                    }
                }
                return Err(gpu_error(error));
            }
        }
        Ok(id as jlong)
    })();
    match result {
        Ok(id) => id,
        Err(error) => {
            throw_surface_error(&mut env, error);
            0
        }
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_algoartist_gta5native_NativeSurfaceView_nativeResize(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    width: jint,
    height: jint,
) {
    let result = u32::try_from(width)
        .ok()
        .zip(u32::try_from(height).ok())
        .ok_or(SurfaceError::InvalidSize)
        .and_then(|(width, height)| {
            let id = handle_id(handle)?;
            let mut entry = take_surface_entry(id)?;
            let result = (|| {
                entry.bridge.resize(width, height)?;
                if let Some(control) = &entry.gpu {
                    control.resize_surface(width, height).map_err(gpu_error)?;
                }
                Ok(())
            })();
            result.and(restore_surface_entry(id, entry))
        });
    if let Err(error) = result {
        throw_surface_error(&mut env, error);
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_algoartist_gta5native_NativeSurfaceView_nativeSuspend(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let result = (|| {
        let id = handle_id(handle)?;
        let mut entry = take_surface_entry(id)?;
        let result = (|| {
            entry.bridge.suspend()?;
            if let Some(control) = &entry.gpu {
                if let Err(error) = control.suspend_surface() {
                    let _ = entry.bridge.resume();
                    return Err(gpu_error(error));
                }
            }
            Ok(())
        })();
        result.and(restore_surface_entry(id, entry))
    })();
    if let Err(error) = result {
        throw_surface_error(&mut env, error);
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_algoartist_gta5native_NativeSurfaceView_nativeResume(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let result = (|| {
        let id = handle_id(handle)?;
        let mut entry = take_surface_entry(id)?;
        let result = (|| {
            entry.bridge.resume()?;
            if let Some((width, height)) = entry.bridge.size() {
                if let Some(control) = &entry.gpu {
                    if let Err(error) = control.resume_surface(entry.bridge.window_address(), width, height) {
                        let _ = entry.bridge.suspend();
                        return Err(gpu_error(error));
                    }
                }
            }
            Ok(())
        })();
        result.and(restore_surface_entry(id, entry))
    })();
    if let Err(error) = result {
        throw_surface_error(&mut env, error);
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_algoartist_gta5native_NativeSurfaceView_nativePresentReady(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    let result = (|| {
        let id = handle_id(handle)?;
        let surfaces = lock_registry()?;
        let entry = surfaces.entries.get(&id).ok_or(SurfaceError::InvalidHandle)?;
        entry.bridge.present_target().map(|_| 1)
    })();
    match result {
        Ok(ready) => ready,
        Err(error) => {
            throw_surface_error(&mut env, error);
            0
        }
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_algoartist_gta5native_NativeSurfaceView_nativeDetach(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let result = (|| {
        let id = handle_id(handle)?;
        // Remove the entry while retaining its native-window reference. This
        // releases the registry mutex during the bounded GPU acknowledgement,
        // while still preventing ANativeWindow_release before detach completes.
        let mut entry = {
            let mut surfaces = lock_registry()?;
            surfaces.entries.remove(&id).ok_or(SurfaceError::InvalidHandle)?
        };
        if let Some(control) = entry.gpu.clone() {
            if let Err(error) = control.detach_surface() {
                match lock_registry() {
                    Ok(mut surfaces) => {
                        surfaces.entries.insert(id, entry);
                    }
                    Err(lock_error) => {
                        // The GPU did not acknowledge detach. Keep the
                        // acquired NDK reference alive rather than risking a
                        // renderer use-after-free after a poisoned registry.
                        std::mem::forget(entry);
                        return Err(lock_error);
                    }
                }
                return Err(gpu_error(error));
            }
        }
        entry.bridge.detach();
        Ok(())
    })();
    if let Err(error) = result {
        throw_surface_error(&mut env, error);
    }
}


