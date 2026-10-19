//! Native replacements for the browser asset and userdata services.
//!
//! This crate deliberately has no HTTP, WebView, IndexedDB, or Wasm runtime
//! dependency.  It reads the immutable game mirror through a bounded block
//! cache and persists `/userdata/` below an app-private directory.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Open asset files kept for reuse; the kernel page cache holds their data.
pub const MAX_OPEN_FILES: usize = 256;
pub const USERDATA_PAGE_AVAILABLE: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    MalformedManifest(String),
    InvalidPath(String),
    DuplicatePath(String),
    UnknownAsset(String),
    UnknownFileId(usize),
    AssetRoot(String),
    AssetShorterThanManifest(String),
    OffsetOverflow,
    BufferTooSmall,
    GuestBounds {
        offset: u64,
        len: u64,
        memory_len: u64,
    },
    InvalidIoTable(String),
    Io(String),
    SavePath(String),
    InvalidMtime,
    CorruptSaveMetadata(String),
    CorruptSaveTransaction(String),
    InjectedSaveFailure(&'static str),
    UnsupportedContract(&'static str),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedManifest(s) => write!(f, "malformed manifest: {s}"),
            Self::InvalidPath(s) => write!(f, "invalid relative path: {s}"),
            Self::DuplicatePath(s) => write!(f, "duplicate manifest path: {s}"),
            Self::UnknownAsset(s) => write!(f, "unknown asset: {s}"),
            Self::UnknownFileId(id) => write!(f, "unknown file id: {id}"),
            Self::AssetRoot(s) => write!(f, "invalid asset root: {s}"),
            Self::AssetShorterThanManifest(s) => {
                write!(f, "asset is shorter than its manifest entry: {s}")
            }
            Self::OffsetOverflow => write!(f, "asset offset overflow"),
            Self::BufferTooSmall => write!(f, "destination buffer is too small"),
            Self::GuestBounds { offset, len, memory_len } => write!(
                f,
                "guest memory range {offset}..{} exceeds {memory_len} bytes",
                offset.saturating_add(*len)
            ),
            Self::InvalidIoTable(s) => write!(f, "invalid shared I/O table: {s}"),
            Self::Io(s) => write!(f, "I/O error: {s}"),
            Self::SavePath(s) => write!(f, "invalid save path: {s}"),
            Self::InvalidMtime => write!(f, "save mtime must be finite"),
            Self::CorruptSaveMetadata(s) => write!(f, "corrupt save metadata: {s}"),
            Self::CorruptSaveTransaction(s) => write!(f, "corrupt save transaction: {s}"),
            Self::InjectedSaveFailure(s) => write!(f, "injected save failure: {s}"),
            Self::UnsupportedContract(s) => write!(f, "unsupported browser contract: {s}"),
        }
    }
}

impl std::error::Error for PlatformError {}

fn io_error(err: std::io::Error) -> PlatformError {
    PlatformError::Io(err.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
}

impl ManifestFile {
    pub fn new(path: impl Into<String>, size: u64, mtime: u64) -> Self {
        Self { path: path.into(), size, mtime }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub mount: String,
    pub version: String,
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    pub fn new(
        mount: impl Into<String>,
        version: impl Into<String>,
        files: Vec<ManifestFile>,
    ) -> Result<Self, PlatformError> {
        let manifest = Self { mount: mount.into(), version: version.into(), files };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, PlatformError> {
        let mut parser = JsonParser::new(bytes);
        parser.skip_ws();
        parser.expect(b'{')?;

        let mut mount = None;
        let mut version = None;
        let mut files = None;
        parser.skip_ws();
        if parser.consume(b'}') {
            return Err(parser.error("manifest object is empty"));
        }

        loop {
            let key = parser.string()?;
            parser.skip_ws();
            parser.expect(b':')?;
            parser.skip_ws();
            match key.as_str() {
                "mount" => {
                    if mount.is_some() {
                        return Err(parser.error("duplicate mount field"));
                    }
                    mount = Some(parser.string()?);
                }
                "version" => {
                    if version.is_some() {
                        return Err(parser.error("duplicate version field"));
                    }
                    version = Some(parser.string()?);
                }
                "files" => {
                    if files.is_some() {
                        return Err(parser.error("duplicate files field"));
                    }
                    files = Some(parser.files()?);
                }
                _ => return Err(parser.error("unknown manifest field")),
            }
            parser.skip_ws();
            if parser.consume(b'}') {
                break;
            }
            parser.expect(b',')?;
            parser.skip_ws();
            if parser.peek() == Some(b'}') {
                return Err(parser.error("trailing comma"));
            }
        }
        parser.skip_ws();
        if parser.peek().is_some() {
            return Err(parser.error("trailing data"));
        }

        Self::new(
            mount.ok_or_else(|| parser.error("missing mount field"))?,
            version.ok_or_else(|| parser.error("missing version field"))?,
            files.ok_or_else(|| parser.error("missing files field"))?,
        )
    }

    pub fn validate(&self) -> Result<(), PlatformError> {
        validate_mount(&self.mount)?;
        if self.version.is_empty() || self.version.contains('\0') {
            return Err(PlatformError::MalformedManifest(
                "version must be a non-empty string".to_owned(),
            ));
        }
        let mut paths = HashMap::with_capacity(self.files.len());
        for file in &self.files {
            validate_relative_path(&file.path)?;
            if paths.insert(file.path.as_str(), ()).is_some() {
                return Err(PlatformError::DuplicatePath(file.path.clone()));
            }
        }
        Ok(())
    }

    pub fn file_id(&self, path: &str) -> Result<usize, PlatformError> {
        validate_relative_path(path)?;
        self.files
            .iter()
            .position(|file| file.path == path)
            .ok_or_else(|| PlatformError::UnknownAsset(path.to_owned()))
    }

    pub fn to_json(&self) -> Result<String, PlatformError> {
        self.validate()?;
        let mut out = String::with_capacity(64 + self.files.len() * 48);
        out.push('{');
        out.push_str("\"mount\":");
        append_json_string(&mut out, &self.mount);
        out.push_str(",\"version\":");
        append_json_string(&mut out, &self.version);
        out.push_str(",\"files\":[");
        for (index, file) in self.files.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push('[');
            append_json_string(&mut out, &file.path);
            out.push(',');
            out.push_str(&file.size.to_string());
            out.push(',');
            out.push_str(&file.mtime.to_string());
            out.push(']');
        }
        out.push_str("]}");
        Ok(out)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>, PlatformError> {
        Ok(self.to_json()?.into_bytes())
    }
}

/// Copy the manifest using the exact observed browser glue rule.
///
/// The browser function returns the byte length.  It copies and appends a NUL
/// only when `cap > length`; an equal or smaller cap performs no write.
pub fn manifest_abi_copy(
    manifest: &Manifest,
    destination: &mut [u8],
    cap: usize,
) -> Result<usize, PlatformError> {
    let bytes = manifest.to_json_bytes()?;
    if cap > bytes.len() {
        if destination.len() < bytes.len() + 1 {
            return Err(PlatformError::BufferTooSmall);
        }
        destination[..bytes.len()].copy_from_slice(&bytes);
        destination[bytes.len()] = 0;
    }
    Ok(bytes.len())
}

pub fn userdata_page_js() -> i32 {
    USERDATA_PAGE_AVAILABLE
}

/// A memory-independent handle for the observed shared I/O table.
///
/// `wasm_httpfs_io_start_js` only receives the guest table address.  Memory
/// validation and processing happen later through [`GuestMemory`], which
/// prevents this crate from taking an aliased `&mut` reference to the whole
/// engine heap.
pub fn httpfs_io_start_js(table: u64) -> Result<IoTable, PlatformError> {
    IoTable::new(table)
}

pub const IO_HEADER_WORDS: u64 = 6;
pub const IO_HEADER_BYTES: u64 = IO_HEADER_WORDS * 4;
pub const IO_SLOT_BASE_WORD: u64 = 16;
pub const IO_SLOT_WORDS: u64 = 16;
pub const IO_SLOT_BYTES: u64 = IO_SLOT_WORDS * 4;
pub const IO_STATE_REQUESTED: i32 = 1;
pub const IO_STATE_TAKEN: i32 = 3;
pub const IO_STATE_DONE: i32 = 2;
pub const IO_COPY_CHUNK: usize = 1024 * 1024;

/// Atomic access to the guest's shared memory without borrowing the whole
/// heap. Implementations must bounds-check byte ranges and make the i32
/// operations atomic. `store_i32` publishes bytes written by `write_bytes`.
pub trait GuestMemory {
    fn len_bytes(&self) -> u64;
    fn load_i32(&self, byte_offset: u64) -> Result<i32, PlatformError>;
    fn compare_exchange_i32(
        &self,
        byte_offset: u64,
        expected: i32,
        replacement: i32,
    ) -> Result<i32, PlatformError>;
    fn store_i32(&self, byte_offset: u64, value: i32) -> Result<(), PlatformError>;
    fn write_bytes(&self, byte_offset: u64, bytes: &[u8]) -> Result<(), PlatformError>;
    fn notify_i32(&self, byte_offset: u64, waiters: u32) -> Result<(), PlatformError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoTable {
    table: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoTableReady {
    pub table: u64,
    pub slot_count: u32,
    pub hint_ring_words: u32,
    pub hint_capacity: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IoProcessReport {
    pub slot_count: u32,
    pub claimed: u32,
    pub completed: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub hints_consumed: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IoServiceStatus {
    pub slots_claimed: u64,
    pub slots_completed: u64,
    pub successes: u64,
    pub failures: u64,
    pub hints_consumed: u64,
    pub errors: u64,
    pub last_error: Option<String>,
}

impl IoTable {
    pub fn new(table: u64) -> Result<Self, PlatformError> {
        if table % 4 != 0 {
            return Err(PlatformError::InvalidIoTable(
                "table address is not 4-byte aligned".to_owned(),
            ));
        }
        Ok(Self { table })
    }

    pub fn address(self) -> u64 {
        self.table
    }

    pub fn ready<M: GuestMemory>(&self, memory: &M) -> Result<IoTableReady, PlatformError> {
        checked_guest_range(memory.len_bytes(), self.table, IO_HEADER_BYTES)?;
        let slot_count_raw = memory.load_i32(self.header_word(1)?)?;
        if slot_count_raw < 0 {
            return Err(PlatformError::InvalidIoTable(
                "negative slot count".to_owned(),
            ));
        }
        let slot_count = slot_count_raw as u32;
        let slot_bytes = (slot_count as u64)
            .checked_mul(IO_SLOT_BYTES)
            .ok_or(PlatformError::OffsetOverflow)?;
        let slot_start = self.word_addr(IO_SLOT_BASE_WORD)?;
        checked_guest_range(memory.len_bytes(), slot_start, slot_bytes)?;

        let ring_raw = memory.load_i32(self.header_word(2)?)?;
        let capacity_raw = memory.load_i32(self.header_word(3)?)?;
        if ring_raw < 0 || capacity_raw < 0 {
            return Err(PlatformError::InvalidIoTable(
                "negative hint ring field".to_owned(),
            ));
        }
        let (hint_ring_words, hint_capacity) = if ring_raw == 0 || capacity_raw == 0 {
            (0, 0)
        } else {
            let ring_words = ring_raw as u32;
            let capacity = capacity_raw as u32;
            let ring_bytes = (capacity as u64)
                .checked_mul(16)
                .ok_or(PlatformError::OffsetOverflow)?;
            let ring_start = self.word_addr(ring_words as u64)?;
            checked_guest_range(memory.len_bytes(), ring_start, ring_bytes)?;
            (ring_words, capacity)
        };
        Ok(IoTableReady {
            table: self.table,
            slot_count,
            hint_ring_words,
            hint_capacity,
        })
    }

    fn word_addr(&self, word: u64) -> Result<u64, PlatformError> {
        self.table
            .checked_add(word.checked_mul(4).ok_or(PlatformError::OffsetOverflow)?)
            .ok_or(PlatformError::OffsetOverflow)
    }

    fn header_word(&self, word: u64) -> Result<u64, PlatformError> {
        if word >= IO_HEADER_WORDS {
            return Err(PlatformError::InvalidIoTable(
                "header field outside six-word header".to_owned(),
            ));
        }
        self.word_addr(word)
    }

    fn slot_word(&self, slot: u64, field: u64) -> Result<u64, PlatformError> {
        if field >= IO_SLOT_WORDS {
            return Err(PlatformError::InvalidIoTable(
                "slot field outside sixteen-word slot".to_owned(),
            ));
        }
        let word = IO_SLOT_BASE_WORD
            .checked_add(slot.checked_mul(IO_SLOT_WORDS).ok_or(PlatformError::OffsetOverflow)?)
            .ok_or(PlatformError::OffsetOverflow)?
            .checked_add(field)
            .ok_or(PlatformError::OffsetOverflow)?;
        self.word_addr(word)
    }

    fn hint_word(&self, ready: IoTableReady, entry: u32, field: u64) -> Result<u64, PlatformError> {
        if field >= 4 || entry >= ready.hint_capacity {
            return Err(PlatformError::InvalidIoTable(
                "hint entry outside configured ring".to_owned(),
            ));
        }
        let word = (ready.hint_ring_words as u64)
            .checked_add((entry as u64).checked_mul(4).ok_or(PlatformError::OffsetOverflow)?)
            .ok_or(PlatformError::OffsetOverflow)?
            .checked_add(field)
            .ok_or(PlatformError::OffsetOverflow)?;
        self.word_addr(word)
    }
}

fn checked_guest_range(memory_len: u64, offset: u64, len: u64) -> Result<(), PlatformError> {
    let end = offset.checked_add(len).ok_or(PlatformError::OffsetOverflow)?;
    if end > memory_len {
        return Err(PlatformError::GuestBounds { offset, len, memory_len });
    }
    Ok(())
}

pub struct NativeIoService {
    table: IoTable,
    assets: AssetStore,
    saves: SaveStore,
    status: IoServiceStatus,
    /// Reused for every read: file pages pass through it into guest memory.
    scratch: Vec<u8>,
}

impl NativeIoService {
    pub fn new(
        table: u64,
        assets: AssetStore,
        saves: SaveStore,
    ) -> Result<Self, PlatformError> {
        Ok(Self {
            table: IoTable::new(table)?,
            assets,
            saves,
            status: IoServiceStatus::default(),
            scratch: Vec::new(),
        })
    }

    pub fn from_table(table: IoTable, assets: AssetStore, saves: SaveStore) -> Self {
        Self { table, assets, saves, status: IoServiceStatus::default(), scratch: Vec::new() }
    }

    pub fn table(&self) -> IoTable {
        self.table
    }

    pub fn assets(&self) -> &AssetStore {
        &self.assets
    }

    pub fn assets_mut(&mut self) -> &mut AssetStore {
        &mut self.assets
    }

    pub fn saves(&self) -> &SaveStore {
        &self.saves
    }

    pub fn status(&self) -> &IoServiceStatus {
        &self.status
    }

    pub fn ready<M: GuestMemory>(&mut self, memory: &M) -> Result<IoTableReady, PlatformError> {
        match self.table.ready(memory) {
            Ok(ready) => Ok(ready),
            Err(error) => {
                self.record_error(&error);
                Err(error)
            }
        }
    }

    /// Claim and complete every currently requested slot. Slot-level errors
    /// are published as result `-1`, state `DONE`, and a futex notification;
    /// the error is also retained in `status`.
    pub fn process_available<M: GuestMemory>(
        &mut self,
        memory: &M,
    ) -> Result<IoProcessReport, PlatformError> {
        let ready = self.ready(memory)?;
        let mut report = IoProcessReport { slot_count: ready.slot_count, ..Default::default() };
        for slot in 0..ready.slot_count {
            let state_addr = self.table.slot_word(slot as u64, 0)?;
            let previous = memory.compare_exchange_i32(
                state_addr,
                IO_STATE_REQUESTED,
                IO_STATE_TAKEN,
            )?;
            if previous != IO_STATE_REQUESTED {
                continue;
            }
            report.claimed += 1;
            self.status.slots_claimed += 1;

            let outcome = self.serve_slot(memory, slot as u64);
            let result = match outcome {
                Ok(bytes) => {
                    report.succeeded += 1;
                    self.status.successes += 1;
                    bytes
                }
                Err(error) => {
                    self.record_error(&error);
                    report.failed += 1;
                    self.status.failures += 1;
                    -1
                }
            };
            self.finish_slot(memory, slot as u64, result)?;
            report.completed += 1;
            self.status.slots_completed += 1;
        }
        self.drain_hints(memory, ready, &mut report)?;
        Ok(report)
    }

    fn serve_slot<M: GuestMemory>(&mut self, memory: &M, slot: u64) -> Result<i32, PlatformError> {
        let id_raw = memory.load_i32(self.table.slot_word(slot, 1)?)?;
        if id_raw < 0 {
            return Err(PlatformError::UnknownFileId(id_raw as usize));
        }
        let file_id = id_raw as usize;
        let file = self
            .assets
            .manifest()
            .files
            .get(file_id)
            .ok_or(PlatformError::UnknownFileId(file_id))?;
        let file_size = file.size;
        let file_path = file.path.clone();
        let offset = read_u64_words(
            memory,
            self.table.slot_word(slot, 2)?,
            self.table.slot_word(slot, 3)?,
        )?;
        let destination = read_u64_words(
            memory,
            self.table.slot_word(slot, 4)?,
            self.table.slot_word(slot, 5)?,
        )?;
        let length = memory.load_i32(self.table.slot_word(slot, 6)?)?;
        if length < 0 {
            return Err(PlatformError::InvalidIoTable(
                "negative slot length".to_owned(),
            ));
        }
        if offset >= file_size || length == 0 {
            return Ok(0);
        }
        let amount = (length as u64).min(file_size - offset);
        checked_guest_range(memory.len_bytes(), destination, amount)?;

        let mut copied = 0u64;
        let mut buffer = std::mem::take(&mut self.scratch);
        buffer.resize(IO_COPY_CHUNK, 0);
        while copied < amount {
            let chunk_len = (amount - copied).min(IO_COPY_CHUNK as u64) as usize;
            let chunk = &mut buffer[..chunk_len];
            let got = self.assets.read_file_page(
                file_id,
                offset.checked_add(copied).ok_or(PlatformError::OffsetOverflow)?,
                chunk,
            )?;
            if got != chunk_len {
                return Err(PlatformError::AssetShorterThanManifest(file_path.clone()));
            }
            memory.write_bytes(
                destination
                    .checked_add(copied)
                    .ok_or(PlatformError::OffsetOverflow)?,
                chunk,
            )?;
            copied += got as u64;
        }
        self.scratch = buffer;
        Ok(amount as i32)
    }

    fn finish_slot<M: GuestMemory>(
        &self,
        memory: &M,
        slot: u64,
        result: i32,
    ) -> Result<(), PlatformError> {
        memory.store_i32(self.table.slot_word(slot, 7)?, result)?;
        let state_addr = self.table.slot_word(slot, 0)?;
        memory.store_i32(state_addr, IO_STATE_DONE)?;
        memory.notify_i32(state_addr, 1)
    }

    fn drain_hints<M: GuestMemory>(
        &mut self,
        memory: &M,
        ready: IoTableReady,
        report: &mut IoProcessReport,
    ) -> Result<(), PlatformError> {
        if ready.hint_ring_words == 0 || ready.hint_capacity == 0 {
            return Ok(());
        }
        let head = memory.load_i32(self.table.header_word(4)?)? as u32;
        let mut tail = memory.load_i32(self.table.header_word(5)?)? as u32;
        if head.wrapping_sub(tail) > ready.hint_capacity {
            tail = head.wrapping_sub(ready.hint_capacity);
        }
        while tail != head {
            let entry = (tail % ready.hint_capacity) as u32;
            let raw = memory.load_i32(self.table.hint_word(ready, entry, 3)?)? as u32;
            if raw == 0 {
                break;
            }
            let id = memory.load_i32(self.table.hint_word(ready, entry, 0)?)?;
            let offset = read_u64_words(
                memory,
                self.table.hint_word(ready, entry, 1)?,
                self.table.hint_word(ready, entry, 2)?,
            )?;
            let length = raw & 0x3fff_ffff;
            if id < 0
                || self.assets.manifest().files.get(id as usize).is_none()
                || (length != 0
                    && self.assets.manifest().files[id as usize].size <= offset)
            {
                self.record_error(&PlatformError::InvalidIoTable(
                    "invalid hint entry".to_owned(),
                ));
            }
            memory.store_i32(self.table.hint_word(ready, entry, 3)?, 0)?;
            tail = tail.wrapping_add(1);
            report.hints_consumed += 1;
            self.status.hints_consumed += 1;
        }
        memory.store_i32(self.table.header_word(5)?, tail as i32)
    }

    fn record_error(&mut self, error: &PlatformError) {
        self.status.errors += 1;
        self.status.last_error = Some(error.to_string());
    }
}

fn read_u64_words<M: GuestMemory>(
    memory: &M,
    low_addr: u64,
    high_addr: u64,
) -> Result<u64, PlatformError> {
    let low = memory.load_i32(low_addr)? as u32 as u64;
    let high = memory.load_i32(high_addr)? as u32 as u64;
    Ok(low | (high << 32))
}

pub struct AssetStore {
    root: PathBuf,
    root_canonical: PathBuf,
    manifest: Manifest,
    file_ids: HashMap<String, usize>,
    open: HashMap<usize, File>,
    disk_bytes: u64,
}

impl AssetStore {
    pub fn new(root: impl AsRef<Path>, manifest: Manifest) -> Result<Self, PlatformError> {
        manifest.validate()?;
        let root = root.as_ref();
        let metadata = fs::metadata(root).map_err(io_error)?;
        if !metadata.is_dir() {
            return Err(PlatformError::AssetRoot(root.display().to_string()));
        }
        let root_canonical = fs::canonicalize(root).map_err(io_error)?;
        let mut file_ids = HashMap::with_capacity(manifest.files.len());
        for (id, file) in manifest.files.iter().enumerate() {
            if file_ids.insert(file.path.clone(), id).is_some() {
                return Err(PlatformError::DuplicatePath(file.path.clone()));
            }
        }
        Ok(Self {
            root: root_canonical.clone(),
            root_canonical,
            manifest,
            file_ids,
            open: HashMap::new(),
            disk_bytes: 0,
        })
    }

    pub fn from_manifest_json(root: impl AsRef<Path>, manifest_json: &[u8]) -> Result<Self, PlatformError> {
        Self::new(root, Manifest::from_json(manifest_json)?)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn file_id(&self, path: &str) -> Result<usize, PlatformError> {
        validate_relative_path(path)?;
        self.file_ids
            .get(path)
            .copied()
            .ok_or_else(|| PlatformError::UnknownAsset(path.to_owned()))
    }

    /// Game data read from storage so far (the browser worker's downloaded bytes).
    pub fn disk_bytes(&self) -> u64 {
        self.disk_bytes
    }

    /// Read a bounded page from a manifest path.  Offset-at-or-after EOF is a
    /// successful zero-byte read, matching the browser worker.
    pub fn read_page(
        &mut self,
        path: &str,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<usize, PlatformError> {
        let file_id = self.file_id(path)?;
        self.read_file_page(file_id, offset, destination)
    }

    /// Read a bounded page by the file id used by the observed shared table.
    pub fn read_file_page(
        &mut self,
        file_id: usize,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<usize, PlatformError> {
        let file = self
            .manifest
            .files
            .get(file_id)
            .ok_or(PlatformError::UnknownFileId(file_id))?;
        let declared_size = file.size;
        if offset >= declared_size || destination.is_empty() {
            return Ok(0);
        }
        let wanted = (declared_size - offset).min(destination.len() as u64) as usize;
        let handle = self.open_file(file_id)?;
        if let Err(error) = handle.read_exact_at(&mut destination[..wanted], offset) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(PlatformError::AssetShorterThanManifest(self.manifest.files[file_id].path.clone()));
            }
            return Err(io_error(error));
        }
        self.disk_bytes += wanted as u64;
        Ok(wanted)
    }

    fn open_file(&mut self, file_id: usize) -> Result<&File, PlatformError> {
        if !self.open.contains_key(&file_id) {
            // ponytail: closes every handle at the limit; an LRU if reopening shows up in profiles.
            if self.open.len() >= MAX_OPEN_FILES {
                self.open.clear();
            }
            let path = self.safe_asset_path(&self.manifest.files[file_id].path)?;
            self.open.insert(file_id, File::open(path).map_err(io_error)?);
        }
        Ok(&self.open[&file_id])
    }

    fn safe_asset_path(&self, path: &str) -> Result<PathBuf, PlatformError> {
        validate_relative_path(path)?;
        let candidate = self.root.join(path);
        let canonical = match fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(PlatformError::UnknownAsset(path.to_owned()));
            }
            Err(error) => return Err(io_error(error)),
        };
        if !canonical.starts_with(&self.root_canonical) {
            return Err(PlatformError::AssetRoot(path.to_owned()));
        }
        let metadata = fs::metadata(&canonical).map_err(io_error)?;
        if !metadata.is_file() {
            return Err(PlatformError::UnknownAsset(path.to_owned()));
        }
        Ok(canonical)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SaveRecord {
    pub data: Vec<u8>,
    pub mtime_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveFailurePoint {
    BeforeDataCommit,
    AfterDataCommit,
    BeforeMetadataCommit,
    AfterMetadataCommit,
}

const PUT_TRANSACTION_MARKER: &[u8] = b"gta5-save-put-v1\n";
const DELETE_TRANSACTION_MARKER: &[u8] = b"gta5-save-delete-v1\n";

impl SaveFailurePoint {
    fn label(self) -> &'static str {
        match self {
            Self::BeforeDataCommit => "before-data-commit",
            Self::AfterDataCommit => "after-data-commit",
            Self::BeforeMetadataCommit => "before-metadata-commit",
            Self::AfterMetadataCommit => "after-metadata-commit",
        }
    }
}

pub struct SaveStore {
    root: PathBuf,
    root_canonical: PathBuf,
    commit_lock: Mutex<()>,
    failure: Mutex<Option<SaveFailurePoint>>,
}

impl SaveStore {
    /// `root` is an app-private directory.  A virtual `/userdata/path` is
    /// stored as `<root>/userdata/path`; mirror assets are never written.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, PlatformError> {
        fs::create_dir_all(root.as_ref()).map_err(io_error)?;
        let metadata = fs::metadata(root.as_ref()).map_err(io_error)?;
        if !metadata.is_dir() {
            return Err(PlatformError::SavePath(root.as_ref().display().to_string()));
        }
        let root_canonical = fs::canonicalize(root.as_ref()).map_err(io_error)?;
        let store = Self {
            root: root_canonical.clone(),
            root_canonical,
            commit_lock: Mutex::new(()),
            failure: Mutex::new(None),
        };
        store.recover_pending_locked()?;
        Ok(store)
    }



    pub fn put(
        &self,
        virtual_path: &str,
        data: &[u8],
        mtime_ms: f64,
    ) -> Result<(), PlatformError> {
        let _lock = self.commit_lock.lock().map_err(|_| {
            PlatformError::Io("save commit lock poisoned".to_owned())
        })?;
        self.recover_pending_locked()?;
        if !mtime_ms.is_finite() {
            return Err(PlatformError::InvalidMtime);
        }
        let target = self.target_path(virtual_path)?;
        self.ensure_parent(target.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)?;
        reject_existing_nonfile(&target, virtual_path)?;

        let metadata_path = self.metadata_path(virtual_path)?;
        self.ensure_parent(metadata_path.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)?;
        let transaction = self.stage_transaction(virtual_path, data, mtime_ms)?;
        match self.commit_transaction(&transaction, &target, &metadata_path) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Complete the journal before returning. This keeps the two
                // raw engine files pair-consistent even when a commit hook or
                // a transient filesystem error fires between renames.
                if let Err(recovery) = self.recover_pending_locked() {
                    return Err(PlatformError::CorruptSaveTransaction(format!(
                        "commit failed: {error}; recovery failed: {recovery}"
                    )));
                }
                Err(error)
            }
        }
    }

    pub fn get(&self, virtual_path: &str) -> Result<Option<SaveRecord>, PlatformError> {
        let _lock = self.commit_lock.lock().map_err(|_| {
            PlatformError::Io("save commit lock poisoned".to_owned())
        })?;
        self.recover_pending_locked()?;
        let target = self.target_path(virtual_path)?;
        if !self.parent_is_inside(target.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)? {
            return Err(PlatformError::AssetRoot(virtual_path.to_owned()));
        }
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(PlatformError::SavePath(virtual_path.to_owned()));
        }
        let data = fs::read(&target).map_err(io_error)?;
        let metadata_path = self.metadata_path(virtual_path)?;
        if !self.parent_is_inside(metadata_path.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)? {
            return Err(PlatformError::AssetRoot(virtual_path.to_owned()));
        }
        let mtime_ms = match fs::read_to_string(&metadata_path) {
            Ok(text) => text
                .parse::<f64>()
                .map_err(|_| PlatformError::CorruptSaveMetadata(virtual_path.to_owned()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0.0,
            Err(error) => return Err(io_error(error)),
        };
        if !mtime_ms.is_finite() {
            return Err(PlatformError::CorruptSaveMetadata(virtual_path.to_owned()));
        }
        Ok(Some(SaveRecord { data, mtime_ms }))
    }

    pub fn delete(&self, virtual_path: &str) -> Result<bool, PlatformError> {
        let _lock = self.commit_lock.lock().map_err(|_| {
            PlatformError::Io("save commit lock poisoned".to_owned())
        })?;
        self.recover_pending_locked()?;
        let target = self.target_path(virtual_path)?;
        if !self.parent_is_inside(target.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)? {
            return Err(PlatformError::AssetRoot(virtual_path.to_owned()));
        }
        let metadata_path = self.metadata_path(virtual_path)?;
        if !self.parent_is_inside(metadata_path.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)? {
            return Err(PlatformError::AssetRoot(virtual_path.to_owned()));
        }
        let target_present = raw_file_present(&target, virtual_path)?;
        let metadata_present = raw_file_present(&metadata_path, virtual_path)?;
        if !target_present && !metadata_present {
            return Ok(false);
        }
        let transaction = self.stage_delete_transaction(virtual_path)?;
        match self.commit_delete_transaction(
            &transaction,
            &target,
            &metadata_path,
            virtual_path,
        ) {
            Ok(()) => Ok(target_present),
            Err(error) => {
                if let Err(recovery) = self.recover_pending_locked() {
                    return Err(PlatformError::CorruptSaveTransaction(format!(
                        "delete failed: {error}; recovery failed: {recovery}"
                    )));
                }
                Err(error)
            }
        }
    }

    fn transaction_root(&self) -> PathBuf {
        self.root.join(".gta5-txn")
    }

    fn stage_transaction(
        &self,
        virtual_path: &str,
        data: &[u8],
        mtime_ms: f64,
    ) -> Result<PathBuf, PlatformError> {
        let transaction_root = self.transaction_root();
        self.ensure_parent(&transaction_root)?;
        let transaction = transaction_root.join(format!("{}.txn", hex_bytes(virtual_path.as_bytes())));
        match fs::symlink_metadata(&transaction) {
            Ok(_) => {
                return Err(PlatformError::CorruptSaveTransaction(
                    "transaction already exists".to_owned(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        fs::create_dir(&transaction).map_err(io_error)?;
        let staged_data = transaction.join("new.data");
        let staged_mtime = transaction.join("new.mtime");
        atomic_write(&staged_data, data, &self.root_canonical)?;
        atomic_write(
            &staged_mtime,
            format!("{mtime_ms:.17}").as_bytes(),
            &self.root_canonical,
        )?;
        // The marker is the single commit record. Before it exists, the
        // target pair was not touched and recovery discards the staging dir.
        atomic_write(
            &transaction.join("ready"),
            PUT_TRANSACTION_MARKER,
            &self.root_canonical,
        )?;
        Ok(transaction)
    }

    fn stage_delete_transaction(&self, virtual_path: &str) -> Result<PathBuf, PlatformError> {
        let transaction_root = self.transaction_root();
        self.ensure_parent(&transaction_root)?;
        let transaction = transaction_root.join(format!("{}.txn", hex_bytes(virtual_path.as_bytes())));
        match fs::symlink_metadata(&transaction) {
            Ok(_) => {
                return Err(PlatformError::CorruptSaveTransaction(
                    "transaction already exists".to_owned(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        fs::create_dir(&transaction).map_err(io_error)?;
        atomic_write(
            &transaction.join("ready"),
            DELETE_TRANSACTION_MARKER,
            &self.root_canonical,
        )?;
        Ok(transaction)
    }

    fn commit_transaction(
        &self,
        transaction: &Path,
        target: &Path,
        metadata_path: &Path,
    ) -> Result<(), PlatformError> {
        self.maybe_fail(SaveFailurePoint::BeforeDataCommit)?;
        atomic_copy(
            target,
            &transaction.join("new.data"),
            &self.root_canonical,
        )?;
        self.maybe_fail(SaveFailurePoint::AfterDataCommit)?;
        self.maybe_fail(SaveFailurePoint::BeforeMetadataCommit)?;
        atomic_copy(
            metadata_path,
            &transaction.join("new.mtime"),
            &self.root_canonical,
        )?;
        self.maybe_fail(SaveFailurePoint::AfterMetadataCommit)?;
        fs::remove_dir_all(transaction).map_err(io_error)
    }

    fn commit_delete_transaction(
        &self,
        transaction: &Path,
        target: &Path,
        metadata_path: &Path,
        virtual_path: &str,
    ) -> Result<(), PlatformError> {
        self.maybe_fail(SaveFailurePoint::BeforeDataCommit)?;
        remove_raw_file(target, virtual_path)?;
        self.maybe_fail(SaveFailurePoint::AfterDataCommit)?;
        self.maybe_fail(SaveFailurePoint::BeforeMetadataCommit)?;
        remove_raw_file(metadata_path, virtual_path)?;
        self.maybe_fail(SaveFailurePoint::AfterMetadataCommit)?;
        fs::remove_dir_all(transaction).map_err(io_error)
    }

    fn recover_pending_locked(&self) -> Result<(), PlatformError> {
        let transaction_root = self.transaction_root();
        match fs::symlink_metadata(&transaction_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(PlatformError::AssetRoot(transaction_root.display().to_string()))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        }
        let entries = match fs::read_dir(&transaction_root) {
            Ok(entries) => entries,
            Err(error) => return Err(io_error(error)),
        };
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            let transaction = entry.path();
            let metadata = fs::symlink_metadata(&transaction).map_err(io_error)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(PlatformError::CorruptSaveTransaction(
                    transaction.display().to_string(),
                ));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(hex) = name.strip_suffix(".txn") else {
                return Err(PlatformError::CorruptSaveTransaction(name));
            };
            let path_bytes = decode_hex(hex).map_err(|_| {
                PlatformError::CorruptSaveTransaction(transaction.display().to_string())
            })?;
            let virtual_path = String::from_utf8(path_bytes).map_err(|_| {
                PlatformError::CorruptSaveTransaction(transaction.display().to_string())
            })?;
            validate_userdata_path(&virtual_path).map_err(|_| {
                PlatformError::CorruptSaveTransaction(virtual_path.clone())
            })?;
            let marker = transaction.join("ready");
            match fs::symlink_metadata(&marker) {
                Ok(marker_metadata) => {
                    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
                        return Err(PlatformError::CorruptSaveTransaction(
                            transaction.display().to_string(),
                        ));
                    }
                    let marker_data = fs::read(&marker).map_err(io_error)?;
                    match marker_data.as_slice() {
                        PUT_TRANSACTION_MARKER => {
                            self.complete_transaction(&virtual_path, &transaction)?;
                        }
                        DELETE_TRANSACTION_MARKER => {
                            self.complete_delete_transaction(&virtual_path, &transaction)?;
                        }
                        _ => {
                            return Err(PlatformError::CorruptSaveTransaction(
                                transaction.display().to_string(),
                            ))
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Staging was interrupted before the commit record. The
                    // old raw pair remains authoritative.
                    fs::remove_dir_all(&transaction).map_err(io_error)?;
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }

    fn complete_transaction(
        &self,
        virtual_path: &str,
        transaction: &Path,
    ) -> Result<(), PlatformError> {
        let target = self.target_path(virtual_path)?;
        self.ensure_parent(target.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)?;
        reject_existing_nonfile(&target, virtual_path)?;
        let metadata_path = self.metadata_path(virtual_path)?;
        self.ensure_parent(metadata_path.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)?;
        let staged_data = transaction.join("new.data");
        let staged_mtime = transaction.join("new.mtime");
        require_private_file(&staged_data, &self.root_canonical)?;
        require_private_file(&staged_mtime, &self.root_canonical)?;
        atomic_copy(&target, &staged_data, &self.root_canonical)?;
        atomic_copy(&metadata_path, &staged_mtime, &self.root_canonical)?;
        fs::remove_dir_all(transaction).map_err(io_error)
    }

    fn complete_delete_transaction(
        &self,
        virtual_path: &str,
        transaction: &Path,
    ) -> Result<(), PlatformError> {
        let target = self.target_path(virtual_path)?;
        self.ensure_parent(target.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)?;
        let metadata_path = self.metadata_path(virtual_path)?;
        self.ensure_parent(metadata_path.parent().ok_or_else(|| {
            PlatformError::SavePath(virtual_path.to_owned())
        })?)?;
        remove_raw_file(&target, virtual_path)?;
        remove_raw_file(&metadata_path, virtual_path)?;
        fs::remove_dir_all(transaction).map_err(io_error)
    }

    fn maybe_fail(&self, point: SaveFailurePoint) -> Result<(), PlatformError> {
        let mut failure = self.failure.lock().map_err(|_| {
            PlatformError::Io("save failure lock poisoned".to_owned())
        })?;
        if *failure == Some(point) {
            *failure = None;
            return Err(PlatformError::InjectedSaveFailure(point.label()));
        }
        Ok(())
    }

    fn target_path(&self, virtual_path: &str) -> Result<PathBuf, PlatformError> {
        let relative = validate_userdata_path(virtual_path)?;
        Ok(self.root.join("userdata").join(relative))
    }

    fn metadata_path(&self, virtual_path: &str) -> Result<PathBuf, PlatformError> {
        validate_userdata_path(virtual_path)?;
        Ok(self
            .root
            .join(".gta5-metadata")
            .join(format!("{}.mtime", hex_bytes(virtual_path.as_bytes()))))
    }

    fn ensure_parent(&self, parent: &Path) -> Result<(), PlatformError> {
        let relative = parent
            .strip_prefix(&self.root)
            .map_err(|_| PlatformError::AssetRoot(parent.display().to_string()))?;
        let mut cursor = self.root.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(PlatformError::SavePath(parent.display().to_string()));
            };
            cursor.push(name);
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(PlatformError::AssetRoot(cursor.display().to_string()));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&cursor).map_err(io_error)?;
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        self.parent_is_inside(parent)?.then_some(()).ok_or_else(|| {
            PlatformError::AssetRoot(parent.display().to_string())
        })
    }

    fn parent_is_inside(&self, parent: &Path) -> Result<bool, PlatformError> {
        let canonical = match fs::canonicalize(parent) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(io_error(error)),
        };
        Ok(canonical.starts_with(&self.root_canonical))
    }
}

fn validate_mount(mount: &str) -> Result<(), PlatformError> {
    if mount.is_empty()
        || !mount.starts_with('/')
        || !mount.ends_with('/')
        || mount.contains('\\')
        || mount.contains('\0')
        || mount.split('/').any(|part| part == "..")
    {
        return Err(PlatformError::MalformedManifest(
            "mount must be an absolute slash-delimited path".to_owned(),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), PlatformError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.contains(':')
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(PlatformError::InvalidPath(path.to_owned()));
    }
    Ok(())
}

fn validate_userdata_path(path: &str) -> Result<&str, PlatformError> {
    let Some(relative) = path.strip_prefix("/userdata/") else {
        return Err(PlatformError::SavePath(path.to_owned()));
    };
    validate_relative_path(relative).map_err(|_| PlatformError::SavePath(path.to_owned()))?;
    Ok(relative)
}

fn reject_existing_nonfile(path: &Path, virtual_path: &str) -> Result<(), PlatformError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(PlatformError::SavePath(virtual_path.to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn raw_file_present(path: &Path, virtual_path: &str) -> Result<bool, PlatformError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(PlatformError::SavePath(virtual_path.to_owned()))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

fn remove_raw_file(path: &Path, virtual_path: &str) -> Result<(), PlatformError> {
    if raw_file_present(path, virtual_path)? {
        fs::remove_file(path).map_err(io_error)?;
    }
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

fn atomic_write(path: &Path, data: &[u8], root: &Path) -> Result<(), PlatformError> {
    let parent = path
        .parent()
        .ok_or_else(|| PlatformError::AssetRoot(path.display().to_string()))?;
    let parent_canonical = fs::canonicalize(parent).map_err(io_error)?;
    if !parent_canonical.starts_with(root) {
        return Err(PlatformError::AssetRoot(path.display().to_string()));
    }
    let pid = std::process::id();
    let mut temp = None;
    for _ in 0..16 {
        let candidate = parent.join(format!(
            ".gta5-tmp-{pid}-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(data).and_then(|_| file.sync_all()) {
                    let _ = fs::remove_file(&candidate);
                    return Err(io_error(error));
                }
                temp = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    let temp = temp.ok_or_else(|| PlatformError::Io("temporary save name exhausted".to_owned()))?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(error));
    }
    Ok(())
}

fn require_private_file(path: &Path, root: &Path) -> Result<(), PlatformError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PlatformError::CorruptSaveTransaction(path.display().to_string()));
    }
    let canonical = fs::canonicalize(path).map_err(io_error)?;
    if !canonical.starts_with(root) {
        return Err(PlatformError::AssetRoot(path.display().to_string()));
    }
    Ok(())
}

fn atomic_copy(path: &Path, source: &Path, root: &Path) -> Result<(), PlatformError> {
    require_private_file(source, root)?;
    let parent = path
        .parent()
        .ok_or_else(|| PlatformError::AssetRoot(path.display().to_string()))?;
    let parent_canonical = fs::canonicalize(parent).map_err(io_error)?;
    if !parent_canonical.starts_with(root) {
        return Err(PlatformError::AssetRoot(path.display().to_string()));
    }
    let pid = std::process::id();
    let mut temp = None;
    for _ in 0..16 {
        let candidate = parent.join(format!(
            ".gta5-tmp-{pid}-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(mut destination) => {
                let result = (|| {
                    let mut source_file = File::open(source).map_err(io_error)?;
                    std::io::copy(&mut source_file, &mut destination).map_err(io_error)?;
                    destination.sync_all().map_err(io_error)
                })();
                if let Err(error) = result {
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                temp = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    let temp = temp.ok_or_else(|| PlatformError::Io("temporary save name exhausted".to_owned()))?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(error));
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    if text.is_empty() || text.len() % 2 != 0 {
        return Err(());
    }
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = hex_digit(pair[0]).ok_or(())?;
        let low = hex_digit(pair[1]).ok_or(())?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn append_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < '\u{20}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct JsonParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn error(&self, message: &str) -> PlatformError {
        PlatformError::MalformedManifest(format!("{message} at byte {}", self.pos))
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), PlatformError> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", byte as char)))
        }
    }

    fn string(&mut self) -> Result<String, PlatformError> {
        self.expect(b'"')?;
        let mut bytes = Vec::new();
        loop {
            let byte = self.peek().ok_or_else(|| self.error("unterminated string"))?;
            self.pos += 1;
            match byte {
                b'"' => return String::from_utf8(bytes).map_err(|_| self.error("invalid UTF-8")),
                b'\\' => {
                    let escaped = self.peek().ok_or_else(|| self.error("bad escape"))?;
                    self.pos += 1;
                    match escaped {
                        b'"' => bytes.push(b'"'),
                        b'\\' => bytes.push(b'\\'),
                        b'/' => bytes.push(b'/'),
                        b'b' => bytes.push(8),
                        b'f' => bytes.push(12),
                        b'n' => bytes.push(b'\n'),
                        b'r' => bytes.push(b'\r'),
                        b't' => bytes.push(b'\t'),
                        b'u' => {
                            let code = self.hex_u16()?;
                            if (0xd800..=0xdfff).contains(&code) {
                                return Err(self.error("surrogate escapes are not supported"));
                            }
                            let ch = char::from_u32(code as u32)
                                .ok_or_else(|| self.error("invalid Unicode escape"))?;
                            let mut encoded = [0u8; 4];
                            bytes.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                b if b < 0x20 => return Err(self.error("control byte in string")),
                b => bytes.push(b),
            }
        }
    }

    fn hex_u16(&mut self) -> Result<u16, PlatformError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self.peek().ok_or_else(|| self.error("short Unicode escape"))?;
            self.pos += 1;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("invalid Unicode escape")),
            };
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<u64, PlatformError> {
        let start = self.pos;
        if self.consume(b'0') {
            if matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'e' | b'E')) {
                return Err(self.error("invalid integer"));
            }
        } else {
            if !matches!(self.peek(), Some(b'1'..=b'9')) {
                return Err(self.error("expected non-negative integer"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| self.error("invalid integer"))?;
        text.parse::<u64>().map_err(|_| self.error("integer overflow"))
    }

    fn files(&mut self) -> Result<Vec<ManifestFile>, PlatformError> {
        self.expect(b'[')?;
        let mut files = Vec::new();
        self.skip_ws();
        if self.consume(b']') {
            return Ok(files);
        }
        loop {
            self.skip_ws();
            self.expect(b'[')?;
            self.skip_ws();
            let path = self.string()?;
            self.skip_ws();
            self.expect(b',')?;
            self.skip_ws();
            let size = self.number()?;
            self.skip_ws();
            self.expect(b',')?;
            self.skip_ws();
            let mtime = self.number()?;
            self.skip_ws();
            self.expect(b']')?;
            files.push(ManifestFile::new(path, size, mtime));
            self.skip_ws();
            if self.consume(b']') {
                return Ok(files);
            }
            self.expect(b',')?;
            self.skip_ws();
            if self.peek() == Some(b']') {
                return Err(self.error("trailing comma in files"));
            }
        }
    }
}


