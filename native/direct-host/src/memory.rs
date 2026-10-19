//! Checked access to the C engine's caller-owned shared heap.
//!
//! `NativeMmapMemory` never unmaps or frees the pointer. The C owner must keep
//! the mapping live until the GPU handle has stopped. Clones share the same
//! pointer/size lease so the worker cannot outlive its Rust view.

use gta5_native_renderer::{SharedMemory, SharedMemoryError};
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Return values from the direct C wait registry.
///
/// These values intentionally match `native/direct-c/support/native_wait.h`:
/// zero means a notify was consumed, one means the value did not match, and
/// two means the deadline elapsed.
const WAIT_OK: i32 = 0;
const WAIT_MISMATCH: i32 = 1;
const WAIT_TIMED_OUT: i32 = 2;

#[cfg(target_os = "android")]
unsafe extern "C" {
    fn gta_native_notify(memory_identity: *const u8, address: *mut u8, waiters: u32) -> u32;
    fn gta_native_wait32(
        memory_identity: *const u8,
        address: *mut u8,
        expected: u32,
        timeout_ns: i64,
    ) -> i32;
}

#[cfg(not(target_os = "android"))]
unsafe fn gta_native_notify(_memory_identity: *const u8, _address: *mut u8, _waiters: u32) -> u32 { u32::MAX }

#[cfg(not(target_os = "android"))]
unsafe fn gta_native_wait32(
    _memory_identity: *const u8,
    _address: *mut u8,
    _expected: u32,
    _timeout_ns: i64,
) -> i32 { -1 }

/// Result of one checked 32-bit wait.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitResult {
    Woken = WAIT_OK,
    Mismatch = WAIT_MISMATCH,
    TimedOut = WAIT_TIMED_OUT,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryError {
    NullMapping,
    EmptyMapping,
    NullLogicalSize,
    MisalignedLogicalSize,
    LogicalSizeOutOfBounds { size: u64, reserved: usize },
    AddressOverflow { address: usize, size: usize },
    OutOfBounds { address: usize, end: usize, length: usize },
    Misaligned { address: usize },
    WaitFailed(i32),
    NotifyFailed(i32),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullMapping => f.write_str("native heap pointer is null"),
            Self::EmptyMapping => f.write_str("native heap length is zero"),
            Self::NullLogicalSize => f.write_str("native heap logical-size pointer is null"),
            Self::MisalignedLogicalSize => f.write_str("native heap logical-size pointer is not u64 aligned"),
            Self::LogicalSizeOutOfBounds { size, reserved } => write!(f, "native heap logical size {size} exceeds reserved mapping {reserved} bytes"),
            Self::AddressOverflow { address, size } => write!(f, "native heap address {address} + {size} overflows"),
            Self::OutOfBounds { address, end, length } => write!(f, "native heap range {address}..{end} exceeds {length} bytes"),
            Self::Misaligned { address } => write!(f, "native heap address {address} is not naturally aligned"),
            Self::WaitFailed(code) => write!(f, "gta_native_wait32 failed with status {code}"),
            Self::NotifyFailed(code) => write!(f, "gta_native_notify failed with status {code}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<MemoryError> for SharedMemoryError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::AddressOverflow { address, size } => Self::AddressOverflow { address, size },
            MemoryError::OutOfBounds { address, end, length } => Self::OutOfBounds { address, end, length },
            MemoryError::Misaligned { address } => Self::Misaligned { address },
            other => Self::Synchronization(other.to_string()),
        }
    }
}

struct MappingLease {
    pointer: NonNull<u8>,
    reserved_length: usize,
    logical_size: Option<NonNull<AtomicU64>>,
}

// The C owner guarantees that the mapping remains valid until the GPU handle
// is stopped. Atomic operations are the synchronization boundary shared with
// the direct engine.
unsafe impl Send for MappingLease {}
unsafe impl Sync for MappingLease {}

/// Cloneable, non-owning view over a native C heap mapping.
#[derive(Clone)]
pub struct NativeMmapMemory {
    mapping: Arc<MappingLease>,
}

// The mapping lease is intentionally shared with the dedicated worker thread.
unsafe impl Send for NativeMmapMemory {}
unsafe impl Sync for NativeMmapMemory {}

impl NativeMmapMemory {
    /// Construct a view over a live C mapping. The caller retains unmap/free
    /// ownership and must keep it valid until every clone is dropped.
    pub unsafe fn from_raw_parts(pointer: *mut u8, length: usize) -> Result<Self, MemoryError> {
        unsafe { Self::from_raw_mapping(pointer, length, None) }
    }

    /// Construct a view over a reserved C mapping whose usable size grows at
    /// runtime. `logical_size` must point to the C engine's `_Atomic uint64_t`
    /// size field. The field is loaded with Acquire before every range check.
    /// The caller owns both the reservation and the size field and must keep
    /// both live until every clone is dropped.
    pub unsafe fn from_raw_with_size(
        pointer: *mut u8,
        reserved_length: usize,
        logical_size: *const AtomicU64,
    ) -> Result<Self, MemoryError> {
        let logical_size = NonNull::new(logical_size.cast_mut()).ok_or(MemoryError::NullLogicalSize)?;
        if logical_size.as_ptr() as usize % std::mem::align_of::<AtomicU64>() != 0 {
            return Err(MemoryError::MisalignedLogicalSize);
        }
        let mapping = unsafe { Self::from_raw_mapping(pointer, reserved_length, Some(logical_size)) }?;
        mapping.logical_len()?;
        Ok(mapping)
    }

    unsafe fn from_raw_mapping(
        pointer: *mut u8,
        length: usize,
        logical_size: Option<NonNull<AtomicU64>>,
    ) -> Result<Self, MemoryError> {
        let pointer = NonNull::new(pointer).ok_or(MemoryError::NullMapping)?;
        if length == 0 { return Err(MemoryError::EmptyMapping); }
        (pointer.as_ptr() as usize)
            .checked_add(length)
            .ok_or(MemoryError::AddressOverflow { address: pointer.as_ptr() as usize, size: length })?;
        Ok(Self { mapping: Arc::new(MappingLease { pointer, reserved_length: length, logical_size }) })
    }

    /// Return the current logical size. This is an Acquire snapshot of the
    /// C engine's growable heap length.
    pub fn logical_len(&self) -> Result<usize, MemoryError> {
        let Some(pointer) = self.mapping.logical_size else {
            return Ok(self.mapping.reserved_length);
        };
        let size = unsafe { pointer.as_ref().load(Ordering::Acquire) };
        let size = usize::try_from(size).map_err(|_| MemoryError::LogicalSizeOutOfBounds {
            size,
            reserved: self.mapping.reserved_length,
        })?;
        if size > self.mapping.reserved_length {
            return Err(MemoryError::LogicalSizeOutOfBounds {
                size: size as u64,
                reserved: self.mapping.reserved_length,
            });
        }
        Ok(size)
    }

    /// Return the current logical size for callers implementing a trait that
    /// cannot return a memory error. Invalid C size metadata is exposed as a
    /// zero-length view; checked operations still return the exact error.
    pub fn len(&self) -> usize { self.logical_len().unwrap_or(0) }

    /// Return the beginning of the caller-owned mapping.
    ///
    /// This pointer is an identity only. The C owner still controls its
    /// lifetime and must keep the mapping live until every clone is dropped.
    pub fn data_ptr(&self) -> *mut u8 { self.mapping.pointer.as_ptr() }

    /// Alias used by native workers when passing the wait-registry identity.
    pub fn identity(&self) -> *const u8 { self.mapping.pointer.as_ptr() as *const u8 }

    /// Validate a byte range and its required alignment, returning the byte
    /// offset. `align == 1` validates an arbitrary byte range.
    pub fn checked(&self, offset: u64, length: usize, align: usize) -> Result<usize, MemoryError> {
        let offset = usize::try_from(offset).map_err(|_| MemoryError::AddressOverflow {
            address: usize::MAX,
            size: length,
        })?;
        if align == 0 || offset % align != 0 {
            return Err(MemoryError::Misaligned { address: offset });
        }
        self.range(offset, length)?;
        let absolute = (self.mapping.pointer.as_ptr() as usize)
            .checked_add(offset)
            .ok_or(MemoryError::AddressOverflow { address: offset, size: length })?;
        if absolute % align != 0 {
            return Err(MemoryError::Misaligned { address: offset });
        }
        Ok(offset)
    }

    fn range(&self, address: usize, size: usize) -> Result<usize, MemoryError> {
        let end = address.checked_add(size).ok_or(MemoryError::AddressOverflow { address, size })?;
        let length = self.logical_len()?;
        if end > length { return Err(MemoryError::OutOfBounds { address, end, length }); }
        Ok(end)
    }

    fn aligned_word(&self, address: usize) -> Result<*mut u32, MemoryError> {
        let offset = self.checked(address as u64, 4, 4)?;
        Ok(unsafe { self.mapping.pointer.as_ptr().add(offset).cast::<u32>() })
    }

    fn aligned_dword(&self, address: usize) -> Result<*mut u64, MemoryError> {
        let offset = self.checked(address as u64, 8, 8)?;
        Ok(unsafe { self.mapping.pointer.as_ptr().add(offset).cast::<u64>() })
    }

    fn byte_pointer(&self, address: usize) -> Result<*mut u8, MemoryError> {
        let offset = self.checked(address as u64, 1, 1)?;
        Ok(unsafe { self.mapping.pointer.as_ptr().add(offset) })
    }

    /// Atomically read one byte from the native heap.
    pub fn atomic_read_u8(&self, address: usize, ordering: Ordering) -> Result<u8, MemoryError> {
        let pointer = self.byte_pointer(address)?;
        Ok(unsafe { AtomicU8::from_ptr(pointer).load(ordering) })
    }

    /// Atomically publish one byte to the native heap.
    pub fn atomic_write_u8(&self, address: usize, value: u8, ordering: Ordering) -> Result<(), MemoryError> {
        let pointer = self.byte_pointer(address)?;
        unsafe { AtomicU8::from_ptr(pointer).store(value, ordering); }
        Ok(())
    }

    pub fn atomic_read_byte(&self, address: usize, ordering: Ordering) -> Result<u8, MemoryError> {
        self.atomic_read_u8(address, ordering)
    }

    pub fn atomic_write_byte(&self, address: usize, value: u8, ordering: Ordering) -> Result<(), MemoryError> {
        self.atomic_write_u8(address, value, ordering)
    }

    pub fn atomic_read_u32(&self, address: usize, ordering: Ordering) -> Result<u32, MemoryError> {
        let pointer = self.aligned_word(address)?;
        Ok(unsafe { AtomicU32::from_ptr(pointer).load(ordering) })
    }

    pub fn atomic_write_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<(), MemoryError> {
        let pointer = self.aligned_word(address)?;
        unsafe { AtomicU32::from_ptr(pointer).store(value, ordering); }
        Ok(())
    }

    pub fn atomic_fetch_add_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<u32, MemoryError> {
        let pointer = self.aligned_word(address)?;
        Ok(unsafe { AtomicU32::from_ptr(pointer).fetch_add(value, ordering) })
    }

    pub fn compare_exchange_u32(
        &self,
        address: usize,
        expected: u32,
        replacement: u32,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u32, MemoryError> {
        let pointer = self.aligned_word(address)?;
        let atomic = unsafe { AtomicU32::from_ptr(pointer) };
        Ok(match atomic.compare_exchange(expected, replacement, success, failure) {
            Ok(previous) | Err(previous) => previous,
        })
    }

    /// Compare and exchange one i32 slot, returning the previous value on
    /// both success and mismatch, like C11 atomic_compare_exchange.
    pub fn atomic_compare_exchange_i32(
        &self,
        address: u64,
        expected: i32,
        replacement: i32,
    ) -> Result<i32, MemoryError> {
        let pointer = self.aligned_word(usize::try_from(address).map_err(|_| MemoryError::AddressOverflow { address: usize::MAX, size: 4 })?)?;
        let atomic = unsafe { AtomicU32::from_ptr(pointer) };
        Ok(match atomic.compare_exchange(
            expected as u32,
            replacement as u32,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(previous) | Err(previous) => previous as i32,
        })
    }

    pub fn atomic_read_u64(&self, address: usize, ordering: Ordering) -> Result<u64, MemoryError> {
        let pointer = self.aligned_dword(address)?;
        Ok(unsafe { AtomicU64::from_ptr(pointer).load(ordering) })
    }

    pub fn atomic_write_u64(&self, address: usize, value: u64, ordering: Ordering) -> Result<(), MemoryError> {
        let pointer = self.aligned_dword(address)?;
        unsafe { AtomicU64::from_ptr(pointer).store(value, ordering); }
        Ok(())
    }

    /// Convenience names matching `native::platform::GuestMemory`.
    pub fn len_bytes(&self) -> u64 { self.len() as u64 }
    pub fn load_i32(&self, address: u64) -> Result<i32, MemoryError> {
        Ok(self.atomic_read_u32(usize::try_from(address).map_err(|_| MemoryError::AddressOverflow { address: usize::MAX, size: 4 })?, Ordering::Acquire)? as i32)
    }
    pub fn store_i32(&self, address: u64, value: i32) -> Result<(), MemoryError> {
        self.atomic_write_u32(usize::try_from(address).map_err(|_| MemoryError::AddressOverflow { address: usize::MAX, size: 4 })?, value as u32, Ordering::Release)
    }
    pub fn compare_exchange_i32(&self, address: u64, expected: i32, replacement: i32) -> Result<i32, MemoryError> {
        self.atomic_compare_exchange_i32(address, expected, replacement)
    }
    /// Bulk copies. The bytes are handed over by the atomics around them (ring
    /// heads and tails, I/O slot states), and the guest's own code reads and
    /// writes this memory with plain loads and stores, so one fenced copy
    /// replaces a per-byte atomic loop.
    pub fn read_bytes(&self, address: u64, destination: &mut [u8]) -> Result<(), MemoryError> {
        let offset = self.checked(address, destination.len(), 1)?;
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe { std::ptr::copy_nonoverlapping(self.mapping.pointer.as_ptr().add(offset), destination.as_mut_ptr(), destination.len()); }
        Ok(())
    }
    pub fn write_bytes(&self, address: u64, source: &[u8]) -> Result<(), MemoryError> {
        let offset = self.checked(address, source.len(), 1)?;
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), self.mapping.pointer.as_ptr().add(offset), source.len()); }
        std::sync::atomic::fence(Ordering::Release);
        Ok(())
    }
    pub fn notify_i32(&self, address: u64, waiters: u32) -> Result<(), MemoryError> {
        self.notify(usize::try_from(address).map_err(|_| MemoryError::AddressOverflow { address: usize::MAX, size: 4 })?, waiters)
    }

    /// Wait using the C registry shared with the direct engine. Timeout is
    /// expressed in nanoseconds by the C ABI; `None` means infinite.
    pub fn wait32(&self, address: usize, expected: u32, timeout: Option<Duration>) -> Result<WaitResult, MemoryError> {
        let pointer = self.aligned_word(address)?;
        let timeout_ns = timeout
            .map(|value| i64::try_from(value.as_nanos().min(i64::MAX as u128)).unwrap_or(i64::MAX))
            .unwrap_or(-1);
        let status = unsafe { gta_native_wait32(self.identity(), pointer.cast(), expected, timeout_ns) };
        match status {
            WAIT_OK => Ok(WaitResult::Woken),
            WAIT_MISMATCH => Ok(WaitResult::Mismatch),
            WAIT_TIMED_OUT => Ok(WaitResult::TimedOut),
            code => Err(MemoryError::WaitFailed(code)),
        }
    }

    /// Compatibility form used by the native asset worker. A finite
    /// `Duration` maps directly to the C nanosecond timeout.
    pub fn wait_u32(&self, address: usize, expected: u32, timeout: Duration) -> Result<WaitResult, MemoryError> {
        self.wait32(address, expected, Some(timeout))
    }

    pub fn notify(&self, address: usize, waiters: u32) -> Result<(), MemoryError> {
        let pointer = self.aligned_word(address)?;
        #[cfg(not(target_os = "android"))]
        {
            let _ = (pointer, waiters);
            return Err(MemoryError::NotifyFailed(-1));
        }
        #[cfg(target_os = "android")]
        let _ = unsafe { gta_native_notify(self.identity(), pointer.cast(), waiters) };
        #[cfg(target_os = "android")]
        Ok(())
    }
}

impl SharedMemory for NativeMmapMemory {
    fn len(&self) -> usize { NativeMmapMemory::len(self) }

    fn read_u32(&self, address: usize) -> Result<u32, SharedMemoryError> {
        let mut bytes = [0u8; 4];
        self.read(address, &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn load_u32(&self, address: usize, ordering: Ordering) -> Result<u32, SharedMemoryError> {
        let pointer = self.aligned_word(address)?;
        Ok(unsafe { AtomicU32::from_ptr(pointer).load(ordering) })
    }

    fn store_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<(), SharedMemoryError> {
        let pointer = self.aligned_word(address)?;
        unsafe { AtomicU32::from_ptr(pointer).store(value, ordering); }
        Ok(())
    }

    fn fetch_add_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<u32, SharedMemoryError> {
        let pointer = self.aligned_word(address)?;
        Ok(unsafe { AtomicU32::from_ptr(pointer).fetch_add(value, ordering) })
    }

    fn read(&self, address: usize, destination: &mut [u8]) -> Result<(), SharedMemoryError> {
        Ok(self.read_bytes(address as u64, destination)?)
    }

    fn read_ordered(&self, address: usize, destination: &mut [u8]) -> Result<(), SharedMemoryError> {
        let offset = self.checked(address as u64, destination.len(), 1)?;
        unsafe { std::ptr::copy_nonoverlapping(self.mapping.pointer.as_ptr().add(offset), destination.as_mut_ptr(), destination.len()); }
        Ok(())
    }

    fn write(&self, address: usize, source: &[u8]) -> Result<(), SharedMemoryError> {
        Ok(self.write_bytes(address as u64, source)?)
    }

    fn notify_u32(&self, address: usize, waiters: u32) -> Result<(), SharedMemoryError> {
        self.notify(address, waiters).map_err(Into::into)
    }
}


