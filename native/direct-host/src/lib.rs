//! VM-free Android GPU bridge for the direct native engine.
//!
//! The C engine owns the native heap mapping and must keep it valid until
//! `gta_native_gpu_stop` returns. This crate owns only a checked pointer view,
//! the renderer worker, and its Vulkan/ANativeWindow references. It contains
//! no Wasm runtime and no browser fallback.
//!
//! The mapping wait ABI is the direct-C registry from
//! `native/direct-c/support/native_wait.h`:
//! `gta_native_wait32(identity, address, expected, timeout_ns)` and
//! `gta_native_notify(identity, address, count)`. `identity` is the mapping
//! base pointer and `address` is the checked absolute word pointer.

mod gpu_host;
#[cfg(target_os = "android")]
mod worker_health;
mod memory;
mod io_host;
mod audio_host;
mod syscalls;
mod services;
#[cfg(target_os = "android")]
mod android_surface;

#[cfg(target_os = "android")]
mod gpu_ffi;

#[cfg(target_os = "android")]
pub use gpu_host::{GpuHost, GpuHostControl, GpuHostError, GpuHostStatus, GpuRingBinding};
pub use memory::{MemoryError, NativeMmapMemory, WaitResult};
