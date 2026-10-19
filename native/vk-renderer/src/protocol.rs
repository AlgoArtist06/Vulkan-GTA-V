use std::marker::PhantomData;
use std::ptr;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};

pub const RING_HEADER_BYTES: usize = 64;
pub const RING_DATA_OFFSET: usize = 64;
pub const COMMAND_HEADER_BYTES: usize = 8;
pub const MAX_RESOURCE_ID: u32 = 1 << 24;
const MAX_COMMAND_WORDS: usize = 16 * 1024 * 1024;
const MAX_BINDINGS: u32 = 128;
const MAX_VERTEX_BUFFERS: u32 = 16;
const MAX_VIEWPORTS: u32 = 16;

/// Checked access to the native engine shared heap. Keep the backing
/// allocation alive and provide acquire/release ordering for the atomic
/// ring header while the engine also owns a mutable view.
pub trait SharedMemory {
    fn len(&self) -> usize;
    fn read_u32(&self, address: usize) -> Result<u32, SharedMemoryError>;
    fn load_u32(&self, address: usize, ordering: Ordering) -> Result<u32, SharedMemoryError>;
    fn store_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<(), SharedMemoryError>;
    fn fetch_add_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<u32, SharedMemoryError>;
    fn read(&self, address: usize, destination: &mut [u8]) -> Result<(), SharedMemoryError>;
    fn write(&self, address: usize, source: &[u8]) -> Result<(), SharedMemoryError>;
    /// `read` for bytes an earlier acquire load already ordered (ring commands before the loaded head):
    /// adapters whose `read` fences may skip that fence here.
    fn read_ordered(&self, address: usize, destination: &mut [u8]) -> Result<(), SharedMemoryError> { self.read(address, destination) }

    /// Wake a producer waiting for a fence value. Native shared-memory
    /// adapters override this with the platform atomic-notify primitive.
    /// Slice-backed test memory has no waiter and keeps the default no-op.
    fn notify_u32(&self, _address: usize, _waiters: u32) -> Result<(), SharedMemoryError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SharedMemoryError {
    OutOfBounds { address: usize, end: usize, length: usize },
    Misaligned { address: usize },
    AddressOverflow { address: usize, size: usize },
    Synchronization(String),
}

impl std::fmt::Display for SharedMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfBounds { address, end, length } => write!(f, "shared memory range {address}..{end} is outside {length} bytes"),
            Self::Misaligned { address } => write!(f, "shared memory address {address} is not u32 aligned"),
            Self::AddressOverflow { address, size } => write!(f, "shared memory address {address} + {size} overflows"),
            Self::Synchronization(error) => write!(f, "shared memory synchronization failed: {error}"),
        }
    }
}

impl std::error::Error for SharedMemoryError {}

/// Small adapter used by tests and by callers that already own a mutable
/// slice. Production Android code should implement [`SharedMemory`] over its
/// Wasmtime shared memory instead of borrowing the entire guest heap mutably.
pub struct SliceSharedMemory<'a> {
    pointer: *mut u8,
    length: usize,
    _lifetime: std::marker::PhantomData<&'a mut [u8]>,
}

impl<'a> SliceSharedMemory<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { pointer: bytes.as_mut_ptr(), length: bytes.len(), _lifetime: std::marker::PhantomData }
    }

    fn range(&self, address: usize, size: usize) -> Result<usize, SharedMemoryError> {
        let end = address.checked_add(size).ok_or(SharedMemoryError::AddressOverflow { address, size })?;
        if end > self.length { return Err(SharedMemoryError::OutOfBounds { address, end, length: self.length }); }
        Ok(end)
    }
}

impl SharedMemory for SliceSharedMemory<'_> {
    fn len(&self) -> usize { self.length }

    fn read_u32(&self, address: usize) -> Result<u32, SharedMemoryError> {
        self.range(address, 4)?;
        Ok(unsafe { u32::from_le(std::ptr::read_unaligned(self.pointer.add(address).cast())) })
    }

    fn load_u32(&self, address: usize, ordering: Ordering) -> Result<u32, SharedMemoryError> {
        if address % 4 != 0 { return Err(SharedMemoryError::Misaligned { address }); }
        self.range(address, 4)?;
        Ok(unsafe { AtomicU32::from_ptr(self.pointer.add(address).cast()).load(ordering) })
    }

    fn store_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<(), SharedMemoryError> {
        if address % 4 != 0 { return Err(SharedMemoryError::Misaligned { address }); }
        self.range(address, 4)?;
        unsafe { AtomicU32::from_ptr(self.pointer.add(address).cast()).store(value, ordering); }
        Ok(())
    }

    fn fetch_add_u32(&self, address: usize, value: u32, ordering: Ordering) -> Result<u32, SharedMemoryError> {
        if address % 4 != 0 { return Err(SharedMemoryError::Misaligned { address }); }
        self.range(address, 4)?;
        Ok(unsafe { AtomicU32::from_ptr(self.pointer.add(address).cast()).fetch_add(value, ordering) })
    }

    fn read(&self, address: usize, destination: &mut [u8]) -> Result<(), SharedMemoryError> {
        self.range(address, destination.len())?;
        unsafe { std::ptr::copy_nonoverlapping(self.pointer.add(address), destination.as_mut_ptr(), destination.len()); }
        Ok(())
    }

    fn write(&self, address: usize, source: &[u8]) -> Result<(), SharedMemoryError> {
        self.range(address, source.len())?;
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), self.pointer.add(address), source.len()); }
        Ok(())
    }
}

/// Resource handles are producer-owned non-zero IDs. Zero remains the
/// protocol's explicit "unbound" value for state-setting commands.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceId(NonZeroU32);

impl ResourceId {
    pub const fn new(value: u32) -> Option<Self> {
        if value == 0 || value > MAX_RESOURCE_ID {
            None
        } else {
            match NonZeroU32::new(value) {
                Some(value) => Some(Self(value)),
                None => None,
            }
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Header layout shared with `wgpu_worker.js` and the D3D11 command producer.
/// All fields are little-endian u32 values.  The first six words are live
/// counters; the remaining words keep the header at exactly 64 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RingHeader {
    pub head: u32,
    pub tail: u32,
    pub state: u32,
    pub executed: u32,
    pub validation_errors: u32,
    pub sleeping: u32,
    pub reserved: [u32; 10],
}

/// The complete command set from the browser worker.  Keeping this enum
/// closed makes a newly observed opcode fail explicitly at the decoder.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Opcode {
    Nop = 0,
    Wrap = 1,
    Init = 2,
    CreateTexture = 3,
    DestroyObject = 4,
    ClearRt = 5,
    ClearDs = 6,
    Present = 7,
    Fence = 8,
    Screenshot = 9,
    Log = 10,
    CreateBuffer = 11,
    UploadBuffer = 12,
    UploadTexture = 13,
    CreateShader = 14,
    CreateLayout = 15,
    CreateState = 16,
    CreateSrv = 17,
    SetShader = 18,
    SetInputLayout = 19,
    SetTopology = 20,
    SetVertexBuffers = 21,
    SetIndexBuffer = 22,
    SetCbuffers = 23,
    SetSrvs = 24,
    SetSamplers = 25,
    SetRenderTargets = 26,
    SetBlend = 27,
    SetDepthStencil = 28,
    SetRaster = 29,
    SetViewports = 30,
    SetScissors = 31,
    Draw = 32,
    ClearState = 33,
    CopyRegion = 34,
    CopyResource = 35,
    Resolve = 36,
    CreateUav = 37,
    SetUavs = 38,
    Dispatch = 39,
    ClearUav = 40,
    GenerateMips = 41,
    Readback = 42,
    CopyCounter = 43,
    CreateQuery = 44,
    BeginQuery = 45,
    EndQuery = 46,
    UploadBufferNoOverwrite = 47,
    MovieBlit = 48,
    MovieEnd = 50,
}

impl Opcode {
    pub const ALL: [Self; 50] = [
        Self::Nop,
        Self::Wrap,
        Self::Init,
        Self::CreateTexture,
        Self::DestroyObject,
        Self::ClearRt,
        Self::ClearDs,
        Self::Present,
        Self::Fence,
        Self::Screenshot,
        Self::Log,
        Self::CreateBuffer,
        Self::UploadBuffer,
        Self::UploadTexture,
        Self::CreateShader,
        Self::CreateLayout,
        Self::CreateState,
        Self::CreateSrv,
        Self::SetShader,
        Self::SetInputLayout,
        Self::SetTopology,
        Self::SetVertexBuffers,
        Self::SetIndexBuffer,
        Self::SetCbuffers,
        Self::SetSrvs,
        Self::SetSamplers,
        Self::SetRenderTargets,
        Self::SetBlend,
        Self::SetDepthStencil,
        Self::SetRaster,
        Self::SetViewports,
        Self::SetScissors,
        Self::Draw,
        Self::ClearState,
        Self::CopyRegion,
        Self::CopyResource,
        Self::Resolve,
        Self::CreateUav,
        Self::SetUavs,
        Self::Dispatch,
        Self::ClearUav,
        Self::GenerateMips,
        Self::Readback,
        Self::CopyCounter,
        Self::CreateQuery,
        Self::BeginQuery,
        Self::EndQuery,
        Self::UploadBufferNoOverwrite,
        Self::MovieBlit,
        Self::MovieEnd,
    ];

    pub fn from_u32(value: u32) -> Option<Self> {
        Self::ALL.iter().copied().find(|op| *op as u32 == value)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Nop => "NOP",
            Self::Wrap => "WRAP",
            Self::Init => "INIT",
            Self::CreateTexture => "CREATE_TEXTURE",
            Self::DestroyObject => "DESTROY_OBJECT",
            Self::ClearRt => "CLEAR_RT",
            Self::ClearDs => "CLEAR_DS",
            Self::Present => "PRESENT",
            Self::Fence => "FENCE",
            Self::Screenshot => "SCREENSHOT",
            Self::Log => "LOG",
            Self::CreateBuffer => "CREATE_BUFFER",
            Self::UploadBuffer => "UPLOAD_BUFFER",
            Self::UploadTexture => "UPLOAD_TEXTURE",
            Self::CreateShader => "CREATE_SHADER",
            Self::CreateLayout => "CREATE_LAYOUT",
            Self::CreateState => "CREATE_STATE",
            Self::CreateSrv => "CREATE_SRV",
            Self::SetShader => "SET_SHADER",
            Self::SetInputLayout => "SET_INPUT_LAYOUT",
            Self::SetTopology => "SET_TOPOLOGY",
            Self::SetVertexBuffers => "SET_VERTEX_BUFFERS",
            Self::SetIndexBuffer => "SET_INDEX_BUFFER",
            Self::SetCbuffers => "SET_CBUFFERS",
            Self::SetSrvs => "SET_SRVS",
            Self::SetSamplers => "SET_SAMPLERS",
            Self::SetRenderTargets => "SET_RENDER_TARGETS",
            Self::SetBlend => "SET_BLEND",
            Self::SetDepthStencil => "SET_DEPTH_STENCIL",
            Self::SetRaster => "SET_RASTER",
            Self::SetViewports => "SET_VIEWPORTS",
            Self::SetScissors => "SET_SCISSORS",
            Self::Draw => "DRAW",
            Self::ClearState => "CLEAR_STATE",
            Self::CopyRegion => "COPY_REGION",
            Self::CopyResource => "COPY_RESOURCE",
            Self::Resolve => "RESOLVE",
            Self::CreateUav => "CREATE_UAV",
            Self::SetUavs => "SET_UAVS",
            Self::Dispatch => "DISPATCH",
            Self::ClearUav => "CLEAR_UAV",
            Self::GenerateMips => "GENERATE_MIPS",
            Self::Readback => "READBACK",
            Self::CopyCounter => "COPY_COUNTER",
            Self::CreateQuery => "CREATE_QUERY",
            Self::BeginQuery => "BEGIN_QUERY",
            Self::EndQuery => "END_QUERY",
            Self::UploadBufferNoOverwrite => "UPLOAD_BUFFER_NO_OVERWRITE",
            Self::MovieBlit => "MOVIE_BLIT",
            Self::MovieEnd => "MOVIE_END",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    RingTooSmall { actual: usize },
    RingMisaligned,
    InvalidOffset { field: &'static str, value: u32 },
    InvalidHeadTail { head: u32, tail: u32 },
    UnknownOpcode { opcode: u32, offset: u32 },
    TruncatedCommand { offset: u32, words: u32 },
    PayloadTooLarge { words: u32 },
    InvalidPayload { opcode: Opcode, detail: &'static str },
    InvalidResourceId { opcode: Opcode, index: usize, value: u32 },
    SharedMemory(SharedMemoryError),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RingTooSmall { actual } => write!(f, "ring is {actual} bytes; need at least 64"),
            Self::RingMisaligned => write!(f, "ring base is not u32 aligned"),
            Self::InvalidOffset { field, value } => write!(f, "invalid {field} offset {value}"),
            Self::InvalidHeadTail { head, tail } => write!(f, "invalid ring head {head} tail {tail}"),
            Self::UnknownOpcode { opcode, offset } => write!(f, "unknown opcode {opcode} at {offset}"),
            Self::TruncatedCommand { offset, words } => write!(f, "truncated command at {offset}, payload words {words}"),
            Self::PayloadTooLarge { words } => write!(f, "payload has too many words: {words}"),
            Self::InvalidPayload { opcode, detail } => write!(f, "{}: invalid payload ({detail})", opcode.name()),
            Self::InvalidResourceId { opcode, index, value } => write!(f, "{}: invalid resource id at word {index}: {value}", opcode.name()),
            Self::SharedMemory(error) => write!(f, "shared memory access failed: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// One validated command.  Payload words are copied out of the shared ring so
/// the producer may reuse the slot after `tail` is published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    pub opcode: Opcode,
    pub offset: u32,
    pub payload: Vec<u32>,
}

impl Command {
    pub fn word(&self, index: usize) -> Option<u32> {
        self.payload.get(index).copied()
    }

    pub fn bytes(&self, start_word: usize, len: usize) -> Option<Vec<u8>> {
        let words = len.checked_add(3)?.checked_div(4)?;
        let end = start_word.checked_add(words)?;
        if end > self.payload.len() {
            return None;
        }
        let mut out = vec![0; len];
        for (i, dst) in out.chunks_mut(4).enumerate() {
            let bytes = self.payload[start_word + i].to_le_bytes();
            dst.copy_from_slice(&bytes[..dst.len()]);
        }
        Some(out)
    }
}

/// Bounds-checked consumer for the shared command ring.
pub struct RingDecoder<'a> {
    ptr: *mut u8,
    len: usize,
    _borrow: PhantomData<&'a mut [u8]>,
}

impl<'a> RingDecoder<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Result<Self, DecodeError> {
        if bytes.len() < RING_HEADER_BYTES + COMMAND_HEADER_BYTES {
            return Err(DecodeError::RingTooSmall { actual: bytes.len() });
        }
        if bytes.as_ptr().align_offset(4) != 0 {
            return Err(DecodeError::RingMisaligned);
        }
        Ok(Self { ptr: bytes.as_mut_ptr(), len: bytes.len(), _borrow: PhantomData })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn start(&mut self) {
        self.store(1, RING_DATA_OFFSET as u32, Ordering::Release);
        self.store(2, 1, Ordering::Release);
    }

    pub fn state(&self) -> u32 {
        self.load(2, Ordering::Acquire)
    }

    pub fn head(&self) -> u32 {
        self.load(0, Ordering::Acquire)
    }

    pub fn tail(&self) -> u32 {
        self.load(1, Ordering::Acquire)
    }

    pub fn executed(&self) -> u32 {
        self.load(3, Ordering::Acquire)
    }

    pub fn validation_errors(&self) -> u32 {
        self.load(4, Ordering::Acquire)
    }

    pub fn fail(&mut self) {
        self.fetch_add(4, 1, Ordering::AcqRel);
        self.store(2, 2, Ordering::Release);
    }

    pub fn set_head_for_test(&mut self, head: u32) -> Result<(), DecodeError> {
        self.validate_offset("head", head, true)?;
        self.store(0, head, Ordering::Release);
        Ok(())
    }

    /// Decode and consume one command. `None` means head == tail.
    pub fn next(&mut self) -> Result<Option<Command>, DecodeError> {
        let head = self.head();
        let tail = self.tail();
        self.validate_offset("head", head, true)?;
        self.validate_offset("tail", tail, true)?;
        if head == tail {
            return Ok(None);
        }

        let wrapped = head < tail;
        loop {
            if tail.checked_add(COMMAND_HEADER_BYTES as u32).is_none() || tail as usize + COMMAND_HEADER_BYTES > self.len {
                return self.bad(DecodeError::TruncatedCommand { offset: tail, words: 0 });
            }
            let opcode_word = self.read_word(tail as usize);
            let words = self.read_word(tail as usize + 4);
            let opcode = match Opcode::from_u32(opcode_word) {
                Some(opcode) => opcode,
                None => return self.bad(DecodeError::UnknownOpcode { opcode: opcode_word, offset: tail }),
            };
            if words as usize > MAX_COMMAND_WORDS {
                return self.bad(DecodeError::PayloadTooLarge { words });
            }
            let payload_bytes = match (words as usize).checked_mul(4) {
                Some(bytes) => bytes,
                None => return self.bad(DecodeError::PayloadTooLarge { words }),
            };
            let byte_len = match COMMAND_HEADER_BYTES.checked_add(payload_bytes) {
                Some(bytes) => bytes,
                None => return self.bad(DecodeError::PayloadTooLarge { words }),
            };
            let end = match (tail as usize).checked_add(byte_len) {
                Some(end) => end,
                None => return self.bad(DecodeError::TruncatedCommand { offset: tail, words }),
            };
            if end > self.len || (!wrapped && end > head as usize) {
                return self.bad(DecodeError::TruncatedCommand { offset: tail, words });
            }
            // head may wrap while ordinary commands before the WRAP
            // marker are still queued. Consume them before the marker.
            if opcode == Opcode::Wrap {
                if words != 0 {
                    return self.bad(DecodeError::InvalidPayload { opcode, detail: "WRAP payload must be empty" });
                }
                let wrap_offset = tail;
                self.store(1, RING_DATA_OFFSET as u32, Ordering::Release);
                if head == RING_DATA_OFFSET as u32 {
                    return Ok(Some(Command { opcode, offset: wrap_offset, payload: Vec::new() }));
                }
                // Return WRAP as a real command. The following call consumes
                // the first command after the wrap and cannot miss it.
                return Ok(Some(Command { opcode, offset: wrap_offset, payload: Vec::new() }));
            }
            let mut payload = Vec::with_capacity(words as usize);
            for i in 0..words as usize {
                payload.push(self.read_word(tail as usize + COMMAND_HEADER_BYTES + i * 4));
            }
            if let Err(error) = validate_payload(opcode, &payload) {
                return self.bad(error);
            }
            self.store(1, end as u32, Ordering::Release);
            self.fetch_add(3, 1, Ordering::AcqRel);
            return Ok(Some(Command { opcode, offset: tail, payload }));
        }
    }

    fn bad<T>(&mut self, error: DecodeError) -> Result<T, DecodeError> {
        self.fail();
        Err(error)
    }

    fn validate_offset(&self, field: &'static str, value: u32, allow_end: bool) -> Result<(), DecodeError> {
        let v = value as usize;
        let upper_ok = if allow_end { v <= self.len } else { v < self.len };
        if v < RING_DATA_OFFSET || !upper_ok || v % 4 != 0 {
            return Err(DecodeError::InvalidOffset { field, value });
        }
        Ok(())
    }

    fn read_word(&self, offset: usize) -> u32 {
        // The ring is a little-endian protocol. `read_unaligned` keeps this
        // valid for a shared-memory base whose alignment is only checked once.
        unsafe { u32::from_le(ptr::read_unaligned(self.ptr.add(offset) as *const u32)) }
    }

    fn load(&self, word: usize, ordering: Ordering) -> u32 {
        unsafe { AtomicU32::from_ptr(self.ptr.add(word * 4) as *mut u32).load(ordering) }
    }

    fn store(&self, word: usize, value: u32, ordering: Ordering) {
        unsafe { AtomicU32::from_ptr(self.ptr.add(word * 4) as *mut u32).store(value, ordering) }
    }

    fn fetch_add(&self, word: usize, value: u32, ordering: Ordering) {
        unsafe { AtomicU32::from_ptr(self.ptr.add(word * 4) as *mut u32).fetch_add(value, ordering); }
    }
}

/// Ring decoder over a live shared heap. Unlike [`RingDecoder`], it never
/// requests an exclusive slice of guest memory; the adapter owns the
/// native heap synchronization and lifetime.
pub struct SharedRingDecoder<'a, M: SharedMemory + ?Sized> {
    memory: &'a M,
    ring_base: usize,
    len: usize,
    /// Decoded commands awaiting caller execution and tail publication.
    unpublished: u32,
    tail: u32,
    /// The head from the last acquire load: commands before it are complete, so the
    /// decoder reloads it only on reaching it (one acquire per batch, not per command).
    head_seen: Option<u32>,
}

/// Successfully executed commands between tail publications.
const PUBLISH_EVERY: u32 = 64;

impl<'a, M: SharedMemory + ?Sized> SharedRingDecoder<'a, M> {
    pub fn new(memory: &'a M, ring_base: usize, ring_len: usize) -> Result<Self, DecodeError> {
        if ring_len < RING_HEADER_BYTES + COMMAND_HEADER_BYTES { return Err(DecodeError::RingTooSmall { actual: ring_len }); }
        if ring_base % 4 != 0 { return Err(DecodeError::RingMisaligned); }
        let end = ring_base.checked_add(ring_len).ok_or(DecodeError::SharedMemory(SharedMemoryError::AddressOverflow { address: ring_base, size: ring_len }))?;
        if end > memory.len() { return Err(DecodeError::SharedMemory(SharedMemoryError::OutOfBounds { address: ring_base, end, length: memory.len() })); }
        Ok(Self { memory, ring_base, len: ring_len, unpublished: 0, tail: 0, head_seen: None })
    }

    pub fn len(&self) -> usize { self.len }

    pub fn start(&self) -> Result<(), DecodeError> {
        self.store_header(1, RING_DATA_OFFSET as u32, Ordering::Release)?;
        self.store_header(2, 1, Ordering::Release)
    }

    pub fn state(&self) -> Result<u32, DecodeError> { self.load_header(2, Ordering::Acquire) }
    pub fn head(&self) -> Result<u32, DecodeError> { self.load_header(0, Ordering::Acquire) }
    pub fn tail(&self) -> Result<u32, DecodeError> { self.load_header(1, Ordering::Acquire) }
    pub fn validation_errors(&self) -> Result<u32, DecodeError> { self.load_header(4, Ordering::Acquire) }

    pub fn fail(&self) -> Result<(), DecodeError> {
        self.fetch_header(4, 1, Ordering::AcqRel)?;
        self.store_header(2, 2, Ordering::Release)
    }

    /// Publishes the tail and executed count after the caller has executed this batch.
    pub fn publish(&mut self) -> Result<(), DecodeError> {
        if self.unpublished == 0 { return Ok(()); }
        self.store_header(1, self.tail, Ordering::Release)?;
        self.fetch_header(3, self.unpublished, Ordering::AcqRel)?;
        self.unpublished = 0;
        Ok(())
    }

    /// Publishes a full batch. Call only after successfully executing the most
    /// recently decoded command; the decoder cannot execute GPU work itself.
    pub(crate) fn publish_if_due(&mut self) -> Result<(), DecodeError> {
        if self.unpublished >= PUBLISH_EVERY { self.publish()?; }
        Ok(())
    }

    pub fn next(&mut self) -> Result<Option<Command>, DecodeError> {
        let head = self.head()?;
        let tail = if self.unpublished == 0 { self.tail()? } else { self.tail };
        self.validate_offset("head", head, true)?;
        self.validate_offset("tail", tail, true)?;
        if head == tail {
            self.publish()?;
            return Ok(None);
        }
        let wrapped = head < tail;
        if tail.checked_add(COMMAND_HEADER_BYTES as u32).is_none() || tail as usize + COMMAND_HEADER_BYTES > self.len {
            return self.bad(DecodeError::TruncatedCommand { offset: tail, words: 0 });
        }
        let opcode_word = self.read_word(tail as usize)?;
        let words = self.read_word(tail as usize + 4)?;
        let opcode = match Opcode::from_u32(opcode_word) {
            Some(opcode) => opcode,
            None => return self.bad(DecodeError::UnknownOpcode { opcode: opcode_word, offset: tail }),
        };
        if words as usize > MAX_COMMAND_WORDS { return self.bad(DecodeError::PayloadTooLarge { words }); }
        let payload_bytes = match (words as usize).checked_mul(4) {
            Some(bytes) => bytes,
            None => return self.bad(DecodeError::PayloadTooLarge { words }),
        };
        let byte_len = match COMMAND_HEADER_BYTES.checked_add(payload_bytes) {
            Some(bytes) => bytes,
            None => return self.bad(DecodeError::PayloadTooLarge { words }),
        };
        let end = match (tail as usize).checked_add(byte_len) {
            Some(end) => end,
            None => return self.bad(DecodeError::TruncatedCommand { offset: tail, words }),
        };
        if end > self.len || (!wrapped && end > head as usize) { return self.bad(DecodeError::TruncatedCommand { offset: tail, words }); }
        // A wrapped producer head does not imply tail is at WRAP yet.
        if opcode == Opcode::Wrap {
            if words != 0 { return self.bad(DecodeError::InvalidPayload { opcode, detail: "WRAP payload must be empty" }); }
            self.publish()?;
            self.store_header(1, RING_DATA_OFFSET as u32, Ordering::Release)?;
            return Ok(Some(Command { opcode, offset: tail, payload: Vec::new() }));
        }
        // One bulk copy: texture uploads carry megabytes of payload.
        let mut payload = vec![0u32; words as usize];
        // SAFETY: a u32 slice viewed as its bytes; every byte pattern is a valid u32.
        let bytes = unsafe { std::slice::from_raw_parts_mut(payload.as_mut_ptr().cast::<u8>(), payload_bytes) };
        self.memory.read(self.address(tail as usize + COMMAND_HEADER_BYTES)?, bytes).map_err(DecodeError::SharedMemory)?;
        if cfg!(target_endian = "big") { for word in &mut payload { *word = u32::from_le(*word); } }
        if let Err(error) = validate_payload(opcode, &payload) { return self.bad(error); }
        self.tail = end as u32;
        self.unpublished += 1;
        Ok(Some(Command { opcode, offset: tail, payload }))
    }

    fn bad<T>(&self, error: DecodeError) -> Result<T, DecodeError> {
        self.fail()?;
        Err(error)
    }

    /// `next` without a new allocation per command: the payload is decoded into `payload`.
    pub fn next_into(&mut self, payload: &mut Vec<u32>) -> Result<Option<Opcode>, DecodeError> {
        let tail = if self.unpublished == 0 { self.tail()? } else { self.tail };
        self.validate_offset("tail", tail, true)?;
        let head = match self.head_seen {
            Some(head) if head != tail => head,
            _ => {
                let head = self.head()?;
                self.validate_offset("head", head, true)?;
                self.head_seen = Some(head);
                head
            }
        };
        if head == tail {
            self.publish()?;
            return Ok(None);
        }
        let wrapped = head < tail;
        if tail as usize + COMMAND_HEADER_BYTES > self.len {
            return self.bad(DecodeError::TruncatedCommand { offset: tail, words: 0 });
        }
        let mut header = [0u8; COMMAND_HEADER_BYTES];
        self.memory.read_ordered(self.address(tail as usize)?, &mut header).map_err(DecodeError::SharedMemory)?;
        let opcode_word = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
        let words = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
        let Some(opcode) = Opcode::from_u32(opcode_word) else { return self.bad(DecodeError::UnknownOpcode { opcode: opcode_word, offset: tail }) };
        if words as usize > MAX_COMMAND_WORDS { return self.bad(DecodeError::PayloadTooLarge { words }); }
        let end = tail as usize + COMMAND_HEADER_BYTES + words as usize * 4;
        if end > self.len || (!wrapped && end > head as usize) { return self.bad(DecodeError::TruncatedCommand { offset: tail, words }); }
        if opcode == Opcode::Wrap {
            if words != 0 { return self.bad(DecodeError::InvalidPayload { opcode, detail: "WRAP payload must be empty" }); }
            self.publish()?;
            self.store_header(1, RING_DATA_OFFSET as u32, Ordering::Release)?;
            // The cached head belongs to the lap just finished.
            self.head_seen = None;
            payload.clear();
            return Ok(Some(opcode));
        }
        payload.clear();
        payload.resize(words as usize, 0);
        // SAFETY: a u32 slice viewed as its bytes; every byte pattern is a valid u32.
        let bytes = unsafe { std::slice::from_raw_parts_mut(payload.as_mut_ptr().cast::<u8>(), words as usize * 4) };
        self.memory.read_ordered(self.address(tail as usize + COMMAND_HEADER_BYTES)?, bytes).map_err(DecodeError::SharedMemory)?;
        if cfg!(target_endian = "big") { for word in payload.iter_mut() { *word = u32::from_le(*word); } }
        if let Err(error) = validate_payload(opcode, payload) { return self.bad(error); }
        self.tail = end as u32;
        self.unpublished += 1;
        Ok(Some(opcode))
    }

    fn validate_offset(&self, field: &'static str, value: u32, allow_end: bool) -> Result<(), DecodeError> {
        let v = value as usize;
        let upper_ok = if allow_end { v <= self.len } else { v < self.len };
        if v < RING_DATA_OFFSET || !upper_ok || v % 4 != 0 { return Err(DecodeError::InvalidOffset { field, value }); }
        Ok(())
    }

    fn address(&self, relative: usize) -> Result<usize, DecodeError> {
        self.ring_base.checked_add(relative).ok_or(DecodeError::SharedMemory(SharedMemoryError::AddressOverflow { address: self.ring_base, size: relative }))
    }

    fn read_word(&self, relative: usize) -> Result<u32, DecodeError> { self.memory.read_u32(self.address(relative)?).map_err(DecodeError::SharedMemory) }
    fn header_address(&self, word: usize) -> Result<usize, DecodeError> { self.address(word.checked_mul(4).ok_or(DecodeError::SharedMemory(SharedMemoryError::AddressOverflow { address: word, size: 4 }))?) }
    fn load_header(&self, word: usize, ordering: Ordering) -> Result<u32, DecodeError> { self.memory.load_u32(self.header_address(word)?, ordering).map_err(DecodeError::SharedMemory) }
    fn store_header(&self, word: usize, value: u32, ordering: Ordering) -> Result<(), DecodeError> { self.memory.store_u32(self.header_address(word)?, value, ordering).map_err(DecodeError::SharedMemory) }
    fn fetch_header(&self, word: usize, value: u32, ordering: Ordering) -> Result<u32, DecodeError> { self.memory.fetch_add_u32(self.header_address(word)?, value, ordering).map_err(DecodeError::SharedMemory) }
}

fn valid_id(opcode: Opcode, payload: &[u32], index: usize) -> Result<(), DecodeError> {
    let value = payload.get(index).copied().unwrap_or(0);
    if value == 0 || value > MAX_RESOURCE_ID {
        return Err(DecodeError::InvalidResourceId { opcode, index, value });
    }
    Ok(())
}

fn optional_id(opcode: Opcode, payload: &[u32], index: usize) -> Result<(), DecodeError> {
    let value = payload.get(index).copied().unwrap_or(0);
    if value > MAX_RESOURCE_ID {
        return Err(DecodeError::InvalidResourceId { opcode, index, value });
    }
    Ok(())
}

fn need(opcode: Opcode, payload: &[u32], words: usize) -> Result<(), DecodeError> {
    if payload.len() < words {
        return Err(DecodeError::InvalidPayload { opcode, detail: "payload is shorter than the command schema" });
    }
    Ok(())
}

fn count(opcode: Opcode, payload: &[u32], index: usize, max: u32, per: usize, base: usize) -> Result<(), DecodeError> {
    let n = *payload.get(index).ok_or(DecodeError::InvalidPayload { opcode, detail: "missing count" })?;
    if n > max {
        return Err(DecodeError::InvalidPayload { opcode, detail: "count exceeds protocol bound" });
    }
    let required = base.checked_add((n as usize).checked_mul(per).ok_or(DecodeError::PayloadTooLarge { words: u32::MAX })?).ok_or(DecodeError::PayloadTooLarge { words: u32::MAX })?;
    need(opcode, payload, required)
}

fn validate_payload(opcode: Opcode, payload: &[u32]) -> Result<(), DecodeError> {
    match opcode {
        Opcode::Nop | Opcode::Wrap | Opcode::Init | Opcode::ClearState | Opcode::MovieEnd => {
            if !payload.is_empty() { return Err(DecodeError::InvalidPayload { opcode, detail: "command must have no payload" }); }
        }
        Opcode::CreateTexture => { need(opcode, payload, 10)?; valid_id(opcode, payload, 0)?; }
        Opcode::DestroyObject => { need(opcode, payload, 1)?; valid_id(opcode, payload, 0)?; }
        Opcode::ClearRt => { need(opcode, payload, 7)?; optional_id(opcode, payload, 0)?; }
        Opcode::ClearDs => { need(opcode, payload, 6)?; optional_id(opcode, payload, 0)?; }
        Opcode::Present => { need(opcode, payload, 1)?; optional_id(opcode, payload, 0)?; }
        Opcode::Fence => { need(opcode, payload, 3)?; }
        Opcode::Screenshot => { need(opcode, payload, 2)?; let len = payload[1] as usize; need(opcode, payload, 2 + len.div_ceil(4))?; }
        Opcode::Log => { need(opcode, payload, 1)?; let len = payload[0] as usize; need(opcode, payload, 1 + len.div_ceil(4))?; }
        Opcode::CreateBuffer => { need(opcode, payload, 5)?; valid_id(opcode, payload, 0)?; }
        Opcode::UploadBuffer | Opcode::UploadBufferNoOverwrite => { need(opcode, payload, 3)?; optional_id(opcode, payload, 0)?; let len = payload[2] as usize; need(opcode, payload, 3 + len.div_ceil(4))?; }
        Opcode::UploadTexture => { need(opcode, payload, 12)?; optional_id(opcode, payload, 0)?; let len = payload[11] as usize; need(opcode, payload, 12 + len.div_ceil(4))?; }
        Opcode::CreateShader => { need(opcode, payload, 5)?; valid_id(opcode, payload, 0)?; let n = payload[4]; if n > 16 { return Err(DecodeError::InvalidPayload { opcode, detail: "signature count exceeds protocol bound" }); } need(opcode, payload, 5 + n as usize * 4)?; }
        Opcode::CreateLayout => { need(opcode, payload, 2)?; valid_id(opcode, payload, 0)?; count(opcode, payload, 1, MAX_BINDINGS, 7, 2)?; }
        Opcode::CreateState => { need(opcode, payload, 2)?; valid_id(opcode, payload, 0)?; }
        Opcode::CreateSrv => { need(opcode, payload, 9)?; valid_id(opcode, payload, 0)?; optional_id(opcode, payload, 2)?; }
        Opcode::SetShader => { need(opcode, payload, 2)?; if payload[0] > 2 { return Err(DecodeError::InvalidPayload { opcode, detail: "shader stage must be 0, 1, or 2" }); } optional_id(opcode, payload, 1)?; }
        Opcode::SetInputLayout | Opcode::SetTopology | Opcode::SetRaster => { need(opcode, payload, 1)?; optional_id(opcode, payload, 0)?; }
        Opcode::SetDepthStencil => { need(opcode, payload, 2)?; optional_id(opcode, payload, 0)?; }
        Opcode::SetVertexBuffers => { need(opcode, payload, 2)?; if payload[0] > MAX_VERTEX_BUFFERS { return Err(DecodeError::InvalidPayload { opcode, detail: "start slot out of range" }); } count(opcode, payload, 1, MAX_VERTEX_BUFFERS - payload[0], 3, 2)?; for i in 0..payload[1] as usize { optional_id(opcode, payload, 2 + i * 3)?; } }
        Opcode::SetIndexBuffer => { need(opcode, payload, 3)?; optional_id(opcode, payload, 0)?; }
        Opcode::SetCbuffers => { need(opcode, payload, 3)?; if payload[0] > 2 || payload[1] > MAX_BINDINGS { return Err(DecodeError::InvalidPayload { opcode, detail: "stage or slot out of range" }); } count(opcode, payload, 2, MAX_BINDINGS - payload[1], 3, 3)?; for i in 0..payload[2] as usize { optional_id(opcode, payload, 3 + i * 3)?; } }
        Opcode::SetSrvs => { need(opcode, payload, 3)?; if payload[0] > 2 || payload[1] > MAX_BINDINGS { return Err(DecodeError::InvalidPayload { opcode, detail: "stage or slot out of range" }); } count(opcode, payload, 2, MAX_BINDINGS - payload[1], 1, 3)?; for i in 0..payload[2] as usize { optional_id(opcode, payload, 3 + i)?; } }
        Opcode::SetSamplers => { need(opcode, payload, 3)?; if payload[0] > 2 || payload[1] > 16 { return Err(DecodeError::InvalidPayload { opcode, detail: "stage or slot out of range" }); } count(opcode, payload, 2, 16 - payload[1], 1, 3)?; for i in 0..payload[2] as usize { optional_id(opcode, payload, 3 + i)?; } }
        Opcode::SetRenderTargets => { need(opcode, payload, 37)?; for i in (0..32).step_by(4) { optional_id(opcode, payload, i)?; } optional_id(opcode, payload, 32)?; }
        Opcode::SetBlend => { need(opcode, payload, 6)?; optional_id(opcode, payload, 0)?; }
        Opcode::SetViewports => { need(opcode, payload, 1)?; count(opcode, payload, 0, MAX_VIEWPORTS, 6, 1)?; }
        Opcode::SetScissors => { need(opcode, payload, 1)?; count(opcode, payload, 0, MAX_VIEWPORTS, 4, 1)?; }
        Opcode::Draw => { need(opcode, payload, 6)?; }
        Opcode::MovieBlit => { need(opcode, payload, 1)?; }
        Opcode::CopyRegion => { need(opcode, payload, 14)?; optional_id(opcode, payload, 0)?; optional_id(opcode, payload, 5)?; }
        Opcode::CopyResource => { need(opcode, payload, 2)?; optional_id(opcode, payload, 0)?; optional_id(opcode, payload, 1)?; }
        Opcode::Resolve => { need(opcode, payload, 4)?; optional_id(opcode, payload, 0)?; optional_id(opcode, payload, 2)?; }
        Opcode::CreateUav => { need(opcode, payload, 9)?; valid_id(opcode, payload, 0)?; optional_id(opcode, payload, 2)?; }
        Opcode::SetUavs => { need(opcode, payload, 3)?; if payload[0] > 2 || payload[1] > 8 { return Err(DecodeError::InvalidPayload { opcode, detail: "stage or slot out of range" }); } count(opcode, payload, 2, 8 - payload[1], 2, 3)?; for i in 0..payload[2] as usize { optional_id(opcode, payload, 3 + i)?; } }
        Opcode::Dispatch => { need(opcode, payload, 3)?; }
        Opcode::ClearUav => { need(opcode, payload, 6)?; optional_id(opcode, payload, 0)?; }
        Opcode::GenerateMips => { need(opcode, payload, 1)?; optional_id(opcode, payload, 0)?; }
        Opcode::Readback => { need(opcode, payload, 17)?; optional_id(opcode, payload, 0)?; }
        Opcode::CopyCounter => { need(opcode, payload, 3)?; optional_id(opcode, payload, 0)?; optional_id(opcode, payload, 2)?; }
        Opcode::CreateQuery => { need(opcode, payload, 4)?; valid_id(opcode, payload, 0)?; }
        Opcode::BeginQuery => { need(opcode, payload, 1)?; optional_id(opcode, payload, 0)?; }
        Opcode::EndQuery => { need(opcode, payload, 2)?; optional_id(opcode, payload, 0)?; }
    }
    Ok(())
}


