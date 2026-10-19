//! C ABI over `GpuHost` for the direct C engine.
#![cfg(target_os = "android")]

use crate::{gpu_host::{GpuHost, GpuRingBinding}, memory::NativeMmapMemory};
use gta5_native_renderer::{MovieFrameCallback,MovieFrameSource};
use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::ptr;
use std::sync::{atomic::AtomicU64, Mutex};

/// Opaque C-owned handle. The C caller owns the heap mapping; stop the GPU
/// handle before unmapping it.
pub struct GtaNativeGpu {
    host: Mutex<Option<GpuHost>>,
    last_error: Mutex<String>,
}

impl GtaNativeGpu {
    fn set_error(&self, error: impl Into<String>) -> i32 {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = error.into();
        }
        GTA_NATIVE_GPU_ERROR
    }

    fn clear_error(&self) {
        if let Ok(mut slot) = self.last_error.lock() {
            slot.clear();
        }
    }
}

pub const GTA_NATIVE_GPU_OK: i32 = 0;
pub const GTA_NATIVE_GPU_ERROR: i32 = -1;
pub const GTA_NATIVE_GPU_INVALID_ARGUMENT: i32 = -2;
pub const GTA_NATIVE_GPU_NOT_BOUND: i32 = -3;

/// Create the dedicated Vulkan worker. Device creation is completed by bind,
/// after the C engine supplies its heap and shader directory.
#[unsafe(no_mangle)]
pub extern "C" fn gta_native_gpu_create() -> *mut GtaNativeGpu {
    match GpuHost::start() {
        Ok(host) => Box::into_raw(Box::new(GtaNativeGpu { host: Mutex::new(Some(host)), last_error: Mutex::new(String::new()) })),
        Err(_) => ptr::null_mut(),
    }
}

/// Bind a caller-owned native heap and the renderer ring inside it.
///
/// `shader_dir` is an optional NUL-terminated UTF-8 path. The C owner must
/// keep `heap..heap+heap_len` mapped and unchanged until `stop` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_bind(
    handle: *mut GtaNativeGpu,
    heap: *mut c_void,
    heap_len: usize,
    ring_base: usize,
    ring_len: usize,
    debug_address: usize,
    shader_dir: *const c_char,
) -> i32 {
    unsafe { bind_impl(handle, heap, heap_len, None, ring_base, ring_len, debug_address, shader_dir) }
}

/// Bind a reserved growable native heap. `logical_size_ptr` points to the
/// C engine's live `_Atomic uint64_t` logical byte length. The reservation may
/// be larger than the current length; every renderer and host-worker range
/// check loads the current length with Acquire. The C owner keeps the mapping
/// and size field live until `gta_native_gpu_stop` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_bind_with_size(
    handle: *mut GtaNativeGpu,
    heap: *mut c_void,
    reserved_length: usize,
    logical_size_ptr: *const c_void,
    ring_base: usize,
    ring_len: usize,
    debug_address: usize,
    shader_dir: *const c_char,
) -> i32 {
    unsafe {
        bind_impl(
            handle,
            heap,
            reserved_length,
            Some(logical_size_ptr),
            ring_base,
            ring_len,
            debug_address,
            shader_dir,
        )
    }
}

/// The C movie owner outlives the GPU handle. Register after bind and before
/// attaching the window, while the command consumer is still paused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_movies(handle: *mut GtaNativeGpu,
    callback: Option<MovieFrameCallback>, context: *mut c_void) -> i32 {
    let Some(handle)=(unsafe {handle.as_ref()}) else {return GTA_NATIVE_GPU_INVALID_ARGUMENT;};
    if callback.is_some()&&context.is_null() {return GTA_NATIVE_GPU_INVALID_ARGUMENT;}
    let source=callback.map(|f|unsafe {MovieFrameSource::new(f,context)});
    let result=(|| {
        let guard=handle.host.lock().map_err(|_|"GPU host mutex poisoned".to_string())?;
        let host=guard.as_ref().ok_or_else(||"GPU host is stopped".to_string())?;
        host.control().set_movie_source(source).map_err(|e|e.to_string())
    })();
    match result {Ok(())=>{handle.clear_error();GTA_NATIVE_GPU_OK},Err(e)=>handle.set_error(e)}
}

unsafe fn bind_impl(
    handle: *mut GtaNativeGpu,
    heap: *mut c_void,
    heap_len: usize,
    logical_size_ptr: Option<*const c_void>,
    ring_base: usize,
    ring_len: usize,
    debug_address: usize,
    shader_dir: *const c_char,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return GTA_NATIVE_GPU_INVALID_ARGUMENT; };
    let memory_result = match logical_size_ptr {
        None => unsafe { NativeMmapMemory::from_raw_parts(heap.cast(), heap_len) },
        Some(logical_size_ptr) => unsafe {
            NativeMmapMemory::from_raw_with_size(
                heap.cast(),
                heap_len,
                logical_size_ptr.cast::<AtomicU64>(),
            )
        },
    };
    let memory = match memory_result {
        Ok(memory) => memory,
        Err(error) => return handle.set_error(error.to_string()),
    };
    let shader_dir = if shader_dir.is_null() {
        None
    } else {
        let path = match unsafe { CStr::from_ptr(shader_dir) }.to_str() {
            Ok(path) => path,
            Err(error) => return handle.set_error(format!("shader path is not UTF-8: {error}")),
        };
        Some(PathBuf::from(path))
    };
    let result = (|| {
        let guard = handle.host.lock().map_err(|_| "GPU host mutex poisoned".to_string())?;
        let host = guard.as_ref().ok_or_else(|| "GPU host is stopped".to_string())?;
        host.bind(&memory, GpuRingBinding::new(ring_base, ring_len, debug_address), shader_dir)
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(()) => { handle.clear_error(); GTA_NATIVE_GPU_OK }
        Err(error) => handle.set_error(error),
    }
}

/// Wake and process the currently bound ring once. The worker also drains
/// published commands autonomously while a surface is active.
#[unsafe(no_mangle)]
pub extern "C" fn gta_native_gpu_process(handle: *mut GtaNativeGpu) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return GTA_NATIVE_GPU_INVALID_ARGUMENT; };
    let result = (|| {
        let guard = handle.host.lock().map_err(|_| "GPU host mutex poisoned".to_string())?;
        let host = guard.as_ref().ok_or_else(|| "GPU host is stopped".to_string())?;
        host.control().process_bound().map_err(|error| error.to_string())
    })();
    match result {
        Ok(_) => { handle.clear_error(); GTA_NATIVE_GPU_OK }
        Err(error) => handle.set_error(error),
    }
}

/// Apply one surface lifecycle operation. `operation` values:
/// 1 attach (`window` required), 2 resize, 3 suspend, 4 resume
/// (`window`, `width`, `height` required), 5 detach.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_surface(
    handle: *mut GtaNativeGpu,
    operation: u32,
    window: *mut c_void,
    width: u32,
    height: u32,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return GTA_NATIVE_GPU_INVALID_ARGUMENT; };
    let result = (|| {
        let guard = handle.host.lock().map_err(|_| "GPU host mutex poisoned".to_string())?;
        let host = guard.as_ref().ok_or_else(|| "GPU host is stopped".to_string())?;
        let control = host.control();
        match operation {
            1 => {
                if window.is_null() { return Err("attach requires a non-null ANativeWindow".to_string()); }
                control.attach_surface(window as usize).map_err(|error| error.to_string())?;
                if width != 0 && height != 0 { control.resize_surface(width, height).map_err(|error| error.to_string())?; }
                Ok(())
            }
            2 => control.resize_surface(width, height).map_err(|error| error.to_string()),
            3 => control.suspend_surface().map_err(|error| error.to_string()),
            4 => {
                if window.is_null() || width == 0 || height == 0 { return Err("resume requires window and non-zero size".to_string()); }
                control.resume_surface(window as usize, width, height).map_err(|error| error.to_string())
            }
            5 => control.detach_surface().map_err(|error| error.to_string()),
            _ => Err(format!("unknown surface operation {operation}")),
        }
    })();
    match result {
        Ok(()) => { handle.clear_error(); GTA_NATIVE_GPU_OK }
        Err(error) => handle.set_error(error),
    }
}

/// Set the current Android surface size. This is a surface configuration
/// operation; draw viewport commands still come from the native ring.
#[unsafe(no_mangle)]
pub extern "C" fn gta_native_gpu_set_viewport(handle: *mut GtaNativeGpu, width: u32, height: u32) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return GTA_NATIVE_GPU_INVALID_ARGUMENT; };
    let result = (|| {
        let guard = handle.host.lock().map_err(|_| "GPU host mutex poisoned".to_string())?;
        let host = guard.as_ref().ok_or_else(|| "GPU host is stopped".to_string())?;
        host.resize_surface(width, height).map_err(|error| error.to_string())
    })();
    match result {
        Ok(()) => { handle.clear_error(); GTA_NATIVE_GPU_OK }
        Err(error) => handle.set_error(error),
    }
}

/// Copy the last error into a caller-owned UTF-8 buffer. Returns required
/// bytes including NUL, or zero for an invalid argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_last_error(handle: *const GtaNativeGpu, output: *mut c_char, capacity: usize) -> usize {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return 0; };
    let Ok(error) = handle.last_error.lock() else { return 0; };
    let required = error.len().saturating_add(1);
    if !output.is_null() && capacity != 0 {
        let count = error.len().min(capacity.saturating_sub(1));
        unsafe { std::ptr::copy_nonoverlapping(error.as_ptr().cast::<c_char>(), output, count); *output.add(count) = 0; }
    }
    required
}

/// Stop the worker, wait until its renderer thread has dropped every mapping
/// Check asynchronous renderer errors without consuming synthetic frames.
#[unsafe(no_mangle)]
pub extern "C" fn gta_native_gpu_check(handle: *mut GtaNativeGpu) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return GTA_NATIVE_GPU_INVALID_ARGUMENT; };
    let result = (|| {
        let guard = handle.host.lock().map_err(|_| "GPU host mutex poisoned".to_string())?;
        let host = guard.as_ref().ok_or_else(|| "GPU host is stopped".to_string())?;
        // Error observation must not queue behind shader compilation/drawing.
        // This reads real worker-published failure/liveness, not GPU completion.
        host.check_failure().map_err(|error| error.to_string())
    })();
    match result {
        Ok(()) => GTA_NATIVE_GPU_OK,
        Err(error) => handle.set_error(error),
    }
}

/// Stop the worker, wait until its renderer thread has dropped every mapping
/// clone, and free the opaque handle. The caller must unmap the heap only
/// after this returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_stop(handle: *mut GtaNativeGpu) -> i32 {
    if handle.is_null() { return GTA_NATIVE_GPU_INVALID_ARGUMENT; }
    let mut boxed = unsafe { Box::from_raw(handle) };
    let result = boxed.host.lock().map_err(|_| "GPU host mutex poisoned".to_string()).and_then(|mut host| {
        host.take().ok_or_else(|| "GPU host is already stopped".to_string()).and_then(|host| host.shutdown().map_err(|error| error.to_string()))
    });
    match result {
        Ok(()) => GTA_NATIVE_GPU_OK,
        Err(_) => GTA_NATIVE_GPU_ERROR,
    }
}

/// Presented frames and whether the world is shown, read without queueing
/// behind drawing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_gpu_progress(handle: *mut GtaNativeGpu, presents: *mut u32, world: *mut bool) {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return; };
    if presents.is_null() || world.is_null() { return; }
    let (count, shown) = handle.host.lock().ok().and_then(|guard| guard.as_ref().map(GpuHost::progress)).unwrap_or((0, false));
    unsafe { *presents = count; *world = shown; }
}
