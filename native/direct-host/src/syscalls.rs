//! Direct-native Emscripten/WASI filesystem and time imports.
//!
//! The JS glue inspected for this port uses little-endian guest memory and the
//! following records: iovec `{ ptr:u64, len:u64 }` (16 bytes), pollfd
//! `{ fd:i32, events:i16, revents:i16 }` (8 bytes), Emscripten `dirent64`
//! records of 280 bytes, and the stat/statfs offsets emitted below.  This
//! module keeps the filesystem policy outside the direct native engine.  It never
//! hands a host path to the guest and rejects paths which leave `/userdata`
//! or `/game`.

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const AT_FDCWD: i32 = -100;
pub const AT_REMOVEDIR: i32 = 512;
pub const AT_SYMLINK_NOFOLLOW: i32 = 256;
pub const AT_EMPTY_PATH: i32 = 4096;

// Emscripten's ErrnoError numbers used by the inspected glue.
pub const EACCES: i32 = 2;
pub const EBADF: i32 = 8;
pub const EEXIST: i32 = 20;
pub const EINVAL: i32 = 28;
pub const EIO: i32 = 29;
pub const EISDIR: i32 = 31;
pub const ENOENT: i32 = 44;
pub const ENOSPC: i32 = 51;
pub const ENOSYS: i32 = 52;
pub const ENOTDIR: i32 = 54;
pub const ENOTEMPTY: i32 = 55;
pub const ENOTTY: i32 = 59;
pub const ENOTSUP: i32 = 138;
pub const ENOSPC_OR_EMPTY_PATH: i32 = 44;
pub const ERANGE: i32 = 68;
pub const EROFS: i32 = 69;
pub const ESPIPE: i32 = 70;

const O_ACCMODE: i32 = 3;
const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 64;
const O_EXCL: i32 = 128;
const O_TRUNC: i32 = 512;
const O_APPEND: i32 = 1024;
const O_DIRECTORY: i32 = 65_536;
const O_NOFOLLOW: i32 = 131_072;
const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_GETLK: i32 = 5;
const F_SETLK: i32 = 6;
const F_SETLKW: i32 = 7;
const F_SETFL_MASK: i32 = 289_792;

const TCGETS: i32 = 21_505;
const TCSETS: i32 = 21_506;
const TCSETSW: i32 = 21_507;
const TCSETSF: i32 = 21_508;
const FIONBIO: i32 = 21_509;
const TCXONC: i32 = 21_510;
const TCFLSH: i32 = 21_512;
const TIOCGPGRP: i32 = 21_519;
const TIOCSPGRP: i32 = 21_520;
const TIOCGWINSZ: i32 = 21_523;
const TIOCSWINSZ: i32 = 21_524;
const FIONREAD: i32 = 21_531;
const FIONBIO_ALT: i32 = 21_537;

const POLLIN: i16 = 1;
const POLLOUT: i16 = 4;
const POLLERR: i16 = 8;
const POLLHUP: i16 = 16;
const POLLNVAL: i16 = 32;
const IOV_MAX: u64 = 1024;
const MAX_IO_BYTES: u64 = 16 * 1024 * 1024;
const MAX_GUEST_STRING: usize = 4096;
const DIRENT_SIZE: u64 = 280;

/// A bounds-checked view of guest linear memory.
///
/// Implementations must bounds-check their primitive `read_u8` and `write_u8`
/// methods.  The default methods add range checks before composing larger
/// accesses, so syscall code never aliases a host slice with the engine heap.
pub trait GuestMemory {
    fn len_bytes(&self) -> u64;
    fn read_u8(&self, offset: u64) -> Result<u8, SyscallError>;
    fn write_u8(&self, offset: u64, value: u8) -> Result<(), SyscallError>;

    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>, SyscallError> {
        checked_range(self, offset, len)?;
        let mut out = Vec::with_capacity(len);
        for index in 0..len {
            out.push(self.read_u8(offset + index as u64)?);
        }
        Ok(out)
    }

    fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), SyscallError> {
        checked_range(self, offset, bytes.len())?;
        for (index, value) in bytes.iter().copied().enumerate() {
            self.write_u8(offset + index as u64, value)?;
        }
        Ok(())
    }

    fn read_u16(&self, offset: u64) -> Result<u16, SyscallError> {
        let b = self.read_bytes(offset, 2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn read_i16(&self, offset: u64) -> Result<i16, SyscallError> {
        Ok(self.read_u16(offset)? as i16)
    }
    fn read_u32(&self, offset: u64) -> Result<u32, SyscallError> {
        let b = self.read_bytes(offset, 4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn read_i32(&self, offset: u64) -> Result<i32, SyscallError> {
        Ok(self.read_u32(offset)? as i32)
    }
    fn read_u64(&self, offset: u64) -> Result<u64, SyscallError> {
        let b = self.read_bytes(offset, 8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn read_i64(&self, offset: u64) -> Result<i64, SyscallError> {
        Ok(self.read_u64(offset)? as i64)
    }
    fn write_u16(&self, offset: u64, value: u16) -> Result<(), SyscallError> {
        self.write_bytes(offset, &value.to_le_bytes())
    }
    fn write_i16(&self, offset: u64, value: i16) -> Result<(), SyscallError> {
        self.write_u16(offset, value as u16)
    }
    fn write_u32(&self, offset: u64, value: u32) -> Result<(), SyscallError> {
        self.write_bytes(offset, &value.to_le_bytes())
    }
    fn write_i32(&self, offset: u64, value: i32) -> Result<(), SyscallError> {
        self.write_u32(offset, value as u32)
    }
    fn write_u64(&self, offset: u64, value: u64) -> Result<(), SyscallError> {
        self.write_bytes(offset, &value.to_le_bytes())
    }
    fn write_i64(&self, offset: u64, value: i64) -> Result<(), SyscallError> {
        self.write_u64(offset, value as u64)
    }

    fn read_c_string(&self, offset: u64, max_len: usize) -> Result<String, SyscallError> {
        if max_len == 0 {
            return Err(SyscallError::memory("zero-length guest string limit"));
        }
        for index in 0..max_len {
            let byte = self.read_u8(offset.checked_add(index as u64).ok_or_else(|| {
                SyscallError::memory("guest string address overflow")
            })?)?;
            if byte == 0 {
                let bytes = self.read_bytes(offset, index)?;
                return String::from_utf8(bytes)
                    .map_err(|_| SyscallError::memory("guest string is not UTF-8"));
            }
        }
        Err(SyscallError::memory("unterminated guest string"))
    }

    /// Text for display (log lines): invalid UTF-8 bytes become U+FFFD instead of an error.
    fn read_c_text(&self, offset: u64, max_len: usize) -> Result<String, SyscallError> {
        for index in 0..max_len {
            let byte = self.read_u8(offset.checked_add(index as u64).ok_or_else(|| SyscallError::memory("guest string address overflow"))?)?;
            if byte == 0 { return Ok(String::from_utf8_lossy(&self.read_bytes(offset, index)?).into_owned()); }
        }
        Err(SyscallError::memory("unterminated guest string"))
    }
}

fn checked_range<M: GuestMemory + ?Sized>(
    memory: &M,
    offset: u64,
    len: usize,
) -> Result<(), SyscallError> {
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| SyscallError::memory("guest address overflow"))?;
    if end > memory.len_bytes() {
        return Err(SyscallError::memory("guest access outside linear memory"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallError {
    pub message: String,
}

impl SyscallError {
    pub fn memory(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            message: format!("ENOSYS: {}", message.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DispatchValue {
    I32(i32),
    I64(i64),
    F64(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DispatchResult {
    Returned(DispatchValue),
    Trap(SyscallError),
}

impl DispatchResult {
    fn i32(value: i32) -> Self {
        Self::Returned(DispatchValue::I32(value))
    }
    fn i64(value: i64) -> Self {
        Self::Returned(DispatchValue::I64(value))
    }
    fn f64(value: f64) -> Self {
        Self::Returned(DispatchValue::F64(value))
    }
}

#[derive(Debug, Clone)]
pub struct FsConfig {
    pub userdata_root: PathBuf,
    pub game_root: Option<PathBuf>,
    /// Writable scratch for `/sandbox` (Sandbox Mode savegames), in-memory MEMFS in the browser build.
    pub sandbox_root: Option<PathBuf>,
    pub cwd: String,
}

impl FsConfig {
    pub fn new(userdata_root: impl Into<PathBuf>) -> Self {
        Self {
            userdata_root: userdata_root.into(),
            game_root: None,
            sandbox_root: None,
            cwd: "/".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mount {
    Root,
    Userdata,
    Sandbox,
    Game,
}

#[derive(Debug, Clone)]
struct ResolvedPath {
    virtual_path: String,
    mount: Mount,
    host: Option<PathBuf>,
}

#[derive(Debug)]
struct DirEntryInfo {
    name: String,
    is_dir: bool,
    is_symlink: bool,
}

#[derive(Debug)]
enum FdKind {
    Stdin,
    Stdout,
    Stderr,
    File(File),
    Directory,
}

#[derive(Debug)]
struct FdEntry {
    kind: FdKind,
    virtual_path: String,
    host_path: Option<PathBuf>,
    flags: i32,
    dir_entries: Vec<DirEntryInfo>,
    dir_pos: u64,
    stdin_pos: usize,
}

impl FdEntry {
    fn is_tty(&self) -> bool {
        matches!(self.kind, FdKind::Stdin | FdKind::Stdout | FdKind::Stderr)
    }

    fn is_directory(&self) -> bool {
        matches!(self.kind, FdKind::Directory)
    }

    fn duplicate(&self) -> Result<Self, i32> {
        let kind = match &self.kind {
            FdKind::Stdin => FdKind::Stdin,
            FdKind::Stdout => FdKind::Stdout,
            FdKind::Stderr => FdKind::Stderr,
            FdKind::File(file) => FdKind::File(file.try_clone().map_err(map_io_error)?),
            FdKind::Directory => FdKind::Directory,
        };
        Ok(Self {
            kind,
            virtual_path: self.virtual_path.clone(),
            host_path: self.host_path.clone(),
            flags: self.flags,
            dir_entries: self
                .dir_entries
                .iter()
                .map(|entry| DirEntryInfo {
                    name: entry.name.clone(),
                    is_dir: entry.is_dir,
                    is_symlink: entry.is_symlink,
                })
                .collect(),
            dir_pos: self.dir_pos,
            stdin_pos: self.stdin_pos,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct TimeParts {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    offset_east: i64,
    is_dst: i32,
}

/// Filesystem and time import state.  `dispatch` mutates only this service;
/// guest memory stays behind the narrow `GuestMemory` trait.
pub struct SyscallService {
    config: FsConfig,
    userdata_root: PathBuf,
    game_root: Option<PathBuf>,
    sandbox_root: Option<PathBuf>,
    cwd: String,
    fds: HashMap<i32, FdEntry>,
    next_fd: i32,
    stdin: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    started: Instant,
    wall_started: SystemTime,
}

impl SyscallService {
    pub fn new(config: FsConfig) -> Result<Self, SyscallError> {
        fs::create_dir_all(&config.userdata_root)
            .map_err(|error| SyscallError::memory(format!("userdata root: {error}")))?;
        let userdata_root = canonical_root(&config.userdata_root)?;
        let game_root = match config.game_root.as_ref() {
            Some(path) => Some(canonical_root(path)?),
            None => None,
        };
        let sandbox_root = match config.sandbox_root.as_ref() {
            Some(path) => Some(canonical_root(path)?),
            None => None,
        };
        let cwd = normalize_virtual_path("/", &config.cwd)
            .map_err(|errno| SyscallError::memory(format!("invalid cwd (errno {errno})")))?;
        let mut service = Self {
            config,
            userdata_root,
            game_root,
            sandbox_root,
            cwd,
            fds: HashMap::new(),
            next_fd: 3,
            stdin: Vec::new(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            started: Instant::now(),
            wall_started: SystemTime::now(),
        };
        // Match Emscripten's always-present stdio descriptors.
        service.fds.insert(0, service.stdio_entry(FdKind::Stdin));
        service.fds.insert(1, service.stdio_entry(FdKind::Stdout));
        service.fds.insert(2, service.stdio_entry(FdKind::Stderr));
        Ok(service)
    }

    pub fn config(&self) -> &FsConfig {
        &self.config
    }

    pub fn set_stdin(&mut self, bytes: Vec<u8>) {
        self.stdin = bytes;
        if let Some(entry) = self.fds.get_mut(&0) {
            entry.stdin_pos = 0;
        }
    }

    pub fn take_stdout(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stdout)
    }

    pub fn take_stderr(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stderr)
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Dispatch an exact import name.  Syscall imports return negative errno;
    /// WASI imports return positive errno, as in the inspected JS glue.
    pub fn dispatch<M: GuestMemory + ?Sized>(
        &mut self,
        name: &str,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let name = name
            .strip_prefix("env.")
            .or_else(|| name.strip_prefix("wasi_snapshot_preview1."))
            .unwrap_or(name);
        match name {
            "__syscall_openat" => self.sys_openat(args, memory),
            "__syscall_fstat64" => self.sys_fstat(args, memory),
            "__syscall_getcwd" => self.sys_getcwd(args, memory),
            "__syscall_getdents64" => self.sys_getdents(args, memory),
            "__syscall_fcntl64" => self.sys_fcntl(args, memory),
            "__syscall_ioctl" => self.sys_ioctl(args, memory),
            "__syscall_mkdirat" => self.sys_mkdirat(args, memory),
            "__syscall_newfstatat" => self.sys_newfstatat(args, memory),
            "__syscall_renameat" => self.sys_renameat(args, memory),
            "__syscall_rmdir" => self.sys_rmdir(args, memory),
            "__syscall_stat64" => self.sys_stat(args, memory, false),
            "__syscall_lstat64" => self.sys_stat(args, memory, true),
            "__syscall_ftruncate64" => self.sys_ftruncate(args),
            "__syscall_statfs64" => self.sys_statfs(args, memory),
            "__syscall_unlinkat" => self.sys_unlinkat(args, memory),
            "__syscall_poll" => self.sys_poll(args, memory),
            "fd_read" => self.wasi_read(args, memory),
            "fd_write" => self.wasi_write(args, memory),
            "fd_seek" => self.wasi_seek(args, memory),
            "fd_pread" => self.wasi_pread(args, memory),
            "fd_pwrite" => self.wasi_pwrite(args, memory),
            "fd_close" => self.wasi_close(args),
            "_emscripten_date_now" | "emscripten_date_now" => {
                DispatchResult::f64(self.date_now_ms())
            }
            "_emscripten_get_now" | "emscripten_get_now" | "wasm_now_ms" => {
                DispatchResult::f64(self.now_ms())
            }
            "clock_time_get" => self.clock_time_get(args, memory),
            "_gmtime_js" | "__gmtime_js" => self.gmtime_js(args, memory),
            "_localtime_js" | "__localtime_js" => self.localtime_js(args, memory),
            "_mktime_js" | "__mktime_js" => self.mktime_js(args, memory),
            "_tzset_js" | "__tzset_js" => self.tzset_js(args, memory),
            _ => {
                if is_wasi(name) {
                    DispatchResult::i32(ENOSYS)
                } else {
                    DispatchResult::i32(-ENOSYS)
                }
            }
        }
    }

    fn stdio_entry(&self, kind: FdKind) -> FdEntry {
        let virtual_path = match &kind {
            FdKind::Stdin => "/dev/stdin",
            FdKind::Stdout => "/dev/stdout",
            FdKind::Stderr => "/dev/stderr",
            _ => "/dev/unknown",
        };
        let flags = match &kind {
            FdKind::Stdin => O_RDONLY,
            _ => O_WRONLY,
        };
        FdEntry {
            kind,
            virtual_path: virtual_path.to_string(),
            host_path: None,
            flags,
            dir_entries: Vec::new(),
            dir_pos: 0,
            stdin_pos: 0,
        }
    }

    fn date_now_ms(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    fn now_ms(&self) -> f64 {
        self.wall_started
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
            + self.started.elapsed().as_secs_f64() * 1000.0
    }
}

fn is_wasi(name: &str) -> bool {
    name.starts_with("fd_")
        || name.starts_with("path_")
        || name.starts_with("sock_")
        || name.starts_with("proc_")
        || name.starts_with("sched_")
        || matches!(name, "environ_get" | "environ_sizes_get" | "random_get")
}

fn canonical_root(path: &Path) -> Result<PathBuf, SyscallError> {
    fs::create_dir_all(path)
        .map_err(|error| SyscallError::memory(format!("filesystem root: {error}")))?;
    fs::canonicalize(path)
        .map_err(|error| SyscallError::memory(format!("filesystem root: {error}")))
}

fn normalize_virtual_path(base: &str, path: &str) -> Result<String, i32> {
    if path.as_bytes().contains(&0) {
        return Err(EINVAL);
    }
    // The engine is PC code: it joins save and audio paths with backslashes, which Windows treats as separators.
    let path = path.replace('\\', "/");
    let source = if path.starts_with('/') {
        path
    } else {
        format!("{}/{}", base.trim_end_matches('/'), path)
    };
    let mut components = Vec::<String>::new();
    for component in Path::new(&source).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => {
                let value = value.to_str().ok_or(EINVAL)?;
                if value.is_empty() {
                    continue;
                }
                components.push(value.to_string());
            }
            Component::ParentDir => {
                if components.pop().is_none() {
                    return Err(EACCES);
                }
            }
            Component::Prefix(_) => return Err(EINVAL),
        }
    }
    if components.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

fn map_io_error(error: io::Error) -> i32 {
    match error.kind() {
        io::ErrorKind::NotFound => ENOENT,
        io::ErrorKind::PermissionDenied => EACCES,
        io::ErrorKind::AlreadyExists => EEXIST,
        io::ErrorKind::InvalidInput => EINVAL,
        io::ErrorKind::WouldBlock => ENOSPC,
        io::ErrorKind::IsADirectory => EISDIR,
        io::ErrorKind::NotADirectory => ENOTDIR,
        io::ErrorKind::ReadOnlyFilesystem => EROFS,
        io::ErrorKind::DirectoryNotEmpty => ENOTEMPTY,
        _ => EIO,
    }
}

fn arg(args: &[u64], index: usize) -> Result<u64, SyscallError> {
    args.get(index)
        .copied()
        .ok_or_else(|| SyscallError::memory(format!("missing syscall argument {index}")))
}

fn as_i32(value: u64) -> i32 {
    value as u32 as i32
}

impl SyscallService {
    fn resolve_from_fd(
        &self,
        dirfd: i32,
        path: &str,
        allow_empty: bool,
    ) -> Result<ResolvedPath, i32> {
        let absolute = path.starts_with(['/', '\\']);
        let base = if absolute {
            "/".to_string()
        } else if dirfd == AT_FDCWD {
            self.cwd.clone()
        } else {
            let entry = self.fds.get(&dirfd).ok_or(EBADF)?;
            if !entry.is_directory() {
                return Err(ENOTDIR);
            }
            entry.virtual_path.clone()
        };
        if path.is_empty() && !allow_empty {
            return Err(ENOSPC_OR_EMPTY_PATH);
        }
        let virtual_path = normalize_virtual_path(&base, path)?;
        if !absolute
            && ((base == "/userdata" || base.starts_with("/userdata/"))
                && !(virtual_path == "/userdata" || virtual_path.starts_with("/userdata/")))
            || (!absolute
                && (base == "/game" || base.starts_with("/game/"))
                && !(virtual_path == "/game" || virtual_path.starts_with("/game/")))
        {
            return Err(EACCES);
        }
        self.resolve_virtual(&virtual_path, false)
    }

    fn resolve_virtual(&self, virtual_path: &str, nofollow: bool) -> Result<ResolvedPath, i32> {
        let mount = if virtual_path == "/" {
            Mount::Root
        } else if virtual_path == "/userdata" || virtual_path.starts_with("/userdata/") {
            Mount::Userdata
        } else if virtual_path == "/game" || virtual_path.starts_with("/game/") {
            Mount::Game
        } else if self.sandbox_root.is_some() && (virtual_path == "/sandbox" || virtual_path.starts_with("/sandbox/")) {
            Mount::Sandbox
        } else {
            return Err(ENOENT);
        };
        let host = match mount {
            Mount::Root => None,
            Mount::Userdata => {
                let suffix = virtual_path.strip_prefix("/userdata").unwrap_or("");
                Some(self.host_path(&self.userdata_root, suffix, nofollow)?)
            }
            Mount::Sandbox => {
                let root = self.sandbox_root.as_ref().ok_or(ENOENT)?;
                let suffix = virtual_path.strip_prefix("/sandbox").unwrap_or("");
                Some(self.host_path(root, suffix, nofollow)?)
            }
            Mount::Game => {
                let root = self.game_root.as_ref().ok_or(ENOENT)?;
                let suffix = virtual_path.strip_prefix("/game").unwrap_or("");
                Some(self.host_path(root, suffix, nofollow)?)
            }
        };
        Ok(ResolvedPath {
            virtual_path: virtual_path.to_string(),
            mount,
            host,
        })
    }

    fn host_path(&self, root: &Path, suffix: &str, nofollow: bool) -> Result<PathBuf, i32> {
        let suffix = suffix.trim_start_matches('/');
        let candidate = if suffix.is_empty() {
            root.to_path_buf()
        } else {
            root.join(suffix)
        };
        let candidate_meta = fs::symlink_metadata(&candidate).map_err(map_io_error);
        let check_path = match candidate_meta {
            Ok(metadata) if nofollow && metadata.file_type().is_symlink() => {
                candidate.parent().unwrap_or(root).to_path_buf()
            }
            Ok(_) => candidate.clone(),
            Err(ENOENT) => candidate.parent().ok_or(ENOENT)?.to_path_buf(),
            Err(errno) => return Err(errno),
        };
        let canonical = fs::canonicalize(&check_path).map_err(map_io_error)?;
        if !canonical.starts_with(root) {
            return Err(EACCES);
        }
        Ok(candidate)
    }

    fn ensure_write_mount(&self, resolved: &ResolvedPath) -> Result<(), i32> {
        match resolved.mount {
            Mount::Userdata | Mount::Sandbox => Ok(()),
            Mount::Game | Mount::Root => Err(EROFS),
        }
    }

    fn next_fd(&mut self) -> Result<i32, i32> {
        for _ in 0..i32::MAX {
            let fd = self.next_fd;
            self.next_fd = self.next_fd.checked_add(1).unwrap_or(3);
            if fd >= 3 && !self.fds.contains_key(&fd) {
                return Ok(fd);
            }
        }
        Err(ENOSPC)
    }

    fn insert_fd(&mut self, entry: FdEntry) -> Result<i32, i32> {
        let fd = self.next_fd()?;
        self.fds.insert(fd, entry);
        Ok(fd)
    }

    fn sys_openat<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let dirfd = as_i32(match arg(args, 0) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        });
        let path_ptr = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let flags = as_i32(match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        });
        let varargs = match arg(args, 3) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let path = match memory.read_c_string(path_ptr, MAX_GUEST_STRING) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let mode = if flags & O_CREAT != 0 && varargs != 0 {
            match memory.read_i32(varargs) {
                Ok(value) => value & 0o7777,
                Err(error) => return DispatchResult::Trap(error),
            }
        } else {
            0o666
        };
        let resolved = match self.resolve_from_fd(dirfd, &path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        let access = flags & O_ACCMODE;
        if access > O_RDWR {
            return DispatchResult::i32(-EINVAL);
        }
        let wants_write = access != O_RDONLY || flags & (O_CREAT | O_TRUNC) != 0;
        if wants_write {
            if let Err(errno) = self.ensure_write_mount(&resolved) {
                return DispatchResult::i32(-errno);
            }
        }
        if flags & O_NOFOLLOW != 0 {
            if let Some(host) = resolved.host.as_ref() {
                if fs::symlink_metadata(host)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return DispatchResult::i32(-ENOTSUP);
                }
            }
        }
        let result = self.open_resolved(resolved, flags, mode);
        match result {
            Ok(fd) => DispatchResult::i32(fd),
            Err(errno) => DispatchResult::i32(-errno),
        }
    }

    fn open_resolved(&mut self, resolved: ResolvedPath, flags: i32, mode: i32) -> Result<i32, i32> {
        let host = resolved.host.clone();
        if host.is_none() {
            if flags & O_DIRECTORY == 0 && resolved.virtual_path == "/" {
                return Err(EISDIR);
            }
            let entries = self.virtual_dir_entries(&resolved.virtual_path)?;
            return self.insert_fd(FdEntry {
                kind: FdKind::Directory,
                virtual_path: resolved.virtual_path,
                host_path: None,
                flags,
                dir_entries: entries,
                dir_pos: 0,
                stdin_pos: 0,
            });
        }
        let host = host.expect("checked above");
        let existing = fs::symlink_metadata(&host);
        if let Ok(meta) = existing.as_ref() {
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                return Err(EEXIST);
            }
            if meta.is_dir() {
                if flags & (O_TRUNC | O_ACCMODE) != 0 {
                    return Err(EISDIR);
                }
                let entries = read_dir_entries(&host)?;
                return self.insert_fd(FdEntry {
                    kind: FdKind::Directory,
                    virtual_path: resolved.virtual_path,
                    host_path: Some(host),
                    flags,
                    dir_entries: entries,
                    dir_pos: 0,
                    stdin_pos: 0,
                });
            }
        } else if flags & O_CREAT == 0 {
            return Err(map_io_error(existing.err().unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "path does not exist")
            })));
        }
        let mut options = OpenOptions::new();
        match flags & O_ACCMODE {
            O_RDONLY => {
                options.read(true);
            }
            O_WRONLY => {
                options.write(true);
            }
            O_RDWR => {
                options.read(true).write(true);
            }
            _ => return Err(EINVAL),
        }
        options
            .create(flags & O_CREAT != 0)
            .create_new(flags & O_EXCL != 0)
            .truncate(flags & O_TRUNC != 0)
            .append(flags & O_APPEND != 0);
        // Keep the source mode available for a future Android host adapter.
        let _ = mode;
        let file = options.open(&host).map_err(map_io_error)?;
        self.insert_fd(FdEntry {
            kind: FdKind::File(file),
            virtual_path: resolved.virtual_path,
            host_path: Some(host),
            flags,
            dir_entries: Vec::new(),
            dir_pos: 0,
            stdin_pos: 0,
        })
    }

    fn virtual_dir_entries(&self, path: &str) -> Result<Vec<DirEntryInfo>, i32> {
        if path == "/" {
            let mut entries = vec![dir_entry(".", true, false), dir_entry("..", true, false)];
            entries.push(dir_entry("userdata", true, false));
            if self.sandbox_root.is_some() {
                entries.push(dir_entry("sandbox", true, false));
            }
            if self.game_root.is_some() {
                entries.push(dir_entry("game", true, false));
            }
            return Ok(entries);
        }
        let resolved = self.resolve_virtual(path, false)?;
        match resolved.host {
            Some(host) => read_dir_entries(&host),
            None => Err(ENOTDIR),
        }
    }

    fn sys_fstat<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let buf = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let entry = match self.fds.get(&fd) {
            Some(value) => value,
            None => return DispatchResult::i32(-EBADF),
        };
        match self.stat_for_entry(entry, false) {
            Ok(stat) => match write_stat(memory, buf, &stat) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i32(-errno),
        }
    }

    fn sys_stat<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M, nofollow:bool) -> DispatchResult {
        let path = match self.read_path_arg(args, 0, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let buf = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let resolved = match self.resolve_from_fd(AT_FDCWD, &path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        match self.stat_for_resolved(&resolved, nofollow) {
            Ok(stat) => match write_stat(memory, buf, &stat) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i32(-errno),
        }
    }

    fn sys_ftruncate(&mut self,args:&[u64])->DispatchResult {
        let (fd,length)=match arg(args,0).and_then(|fd|arg(args,1).map(|n|(as_i32(fd),n))) {
            Ok(v)=>v,Err(e)=>return DispatchResult::Trap(e),
        };
        if (length as i64)<0 {return DispatchResult::i32(-EINVAL);}
        // Archived glue rejects an off_t which JavaScript cannot represent.
        if length>9_007_199_254_740_991 {return DispatchResult::i32(-22);}
        let entry=match self.fds.get_mut(&fd){Some(e)=>e,None=>return DispatchResult::i32(-EBADF)};
        if entry.flags & 2_097_155==0 {return DispatchResult::i32(-EINVAL);}
        match &mut entry.kind {
            FdKind::File(file)=>match file.set_len(length){Ok(())=>DispatchResult::i32(0),Err(e)=>DispatchResult::i32(-map_io_error(e))},
            FdKind::Directory=>DispatchResult::i32(-EISDIR),
            _=>DispatchResult::i32(-EINVAL),
        }
    }

    fn sys_newfstatat<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let dirfd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let path = match self.read_path_arg(args, 1, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let buf = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let flags = match arg(args, 3) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
            return DispatchResult::i32(-EINVAL);
        }
        if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
            let entry = match self.fds.get(&dirfd) {
                Some(entry) => entry,
                None => return DispatchResult::i32(-EBADF),
            };
            return match self.stat_for_entry(entry, flags & AT_SYMLINK_NOFOLLOW != 0) {
                Ok(stat) => match write_stat(memory, buf, &stat) {
                    Ok(()) => DispatchResult::i32(0),
                    Err(error) => DispatchResult::Trap(error),
                },
                Err(errno) => DispatchResult::i32(-errno),
            };
        }
        let resolved = match self.resolve_from_fd(
            dirfd,
            &path,
            flags & AT_EMPTY_PATH != 0,
        ) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        match self.stat_for_resolved(&resolved, flags & AT_SYMLINK_NOFOLLOW != 0) {
            Ok(stat) => match write_stat(memory, buf, &stat) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i32(-errno),
        }
    }

    fn read_path_arg<M: GuestMemory + ?Sized>(
        &self,
        args: &[u64],
        index: usize,
        memory: &M,
    ) -> Result<String, DispatchResult> {
        let ptr = arg(args, index).map_err(DispatchResult::Trap)?;
        memory
            .read_c_string(ptr, MAX_GUEST_STRING)
            .map_err(DispatchResult::Trap)
    }

    fn stat_for_entry(&self, entry: &FdEntry, nofollow: bool) -> Result<StatData, i32> {
        if let Some(host) = entry.host_path.as_ref() {
            let metadata = if nofollow {
                fs::symlink_metadata(host)
            } else {
                fs::metadata(host)
            }
            .map_err(map_io_error)?;
            return stat_data_from_metadata(&metadata, &entry.virtual_path);
        }
        Ok(StatData::directory(&entry.virtual_path))
    }

    fn stat_for_resolved(
        &self,
        resolved: &ResolvedPath,
        nofollow: bool,
    ) -> Result<StatData, i32> {
        if let Some(host) = resolved.host.as_ref() {
            let metadata = if nofollow {
                fs::symlink_metadata(host)
            } else {
                fs::metadata(host)
            }
            .map_err(map_io_error)?;
            stat_data_from_metadata(&metadata, &resolved.virtual_path)
        } else {
            Ok(StatData::directory(&resolved.virtual_path))
        }
    }

    fn sys_getcwd<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let buf = match arg(args, 0) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let size = match arg(args, 1).and_then(|value| {
            usize::try_from(value).map_err(|_| SyscallError::memory("getcwd size is too large"))
        }) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        if size == 0 {
            return DispatchResult::i32(-ENOSPC);
        }
        let bytes = self.cwd.as_bytes();
        if size <= bytes.len() {
            return DispatchResult::i32(-ERANGE);
        }
        let mut out = bytes.to_vec();
        out.push(0);
        match memory.write_bytes(buf, &out) {
            Ok(()) => DispatchResult::i32(out.len() as i32),
            Err(error) => DispatchResult::Trap(error),
        }
    }

    fn sys_mkdirat<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let dirfd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let path = match self.read_path_arg(args, 1, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let mode = match arg(args, 2) {
            Ok(value) => value as u32 & 0o7777,
            Err(error) => return DispatchResult::Trap(error),
        };
        let resolved = match self.resolve_from_fd(dirfd, &path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        if let Err(errno) = self.ensure_write_mount(&resolved) {
            return DispatchResult::i32(-errno);
        }
        let host = match resolved.host {
            Some(path) => path,
            None => return DispatchResult::i32(-EROFS),
        };
        match fs::create_dir(&host) {
            Ok(()) => {
                let _ = mode;
                DispatchResult::i32(0)
            }
            Err(error) => DispatchResult::i32(-map_io_error(error)),
        }
    }

    fn sys_rmdir<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let path = match self.read_path_arg(args, 0, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let resolved = match self.resolve_from_fd(AT_FDCWD, &path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        if let Err(errno) = self.ensure_write_mount(&resolved) {
            return DispatchResult::i32(-errno);
        }
        match resolved.host {
            Some(host) => match fs::remove_dir(host) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::i32(-map_io_error(error)),
            },
            None => DispatchResult::i32(-EROFS),
        }
    }

    fn sys_unlinkat<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let dirfd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let path = match self.read_path_arg(args, 1, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let flags = match arg(args, 2) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        if flags != 0 && flags != AT_REMOVEDIR {
            // The browser glue returns -28 here.  Keep its error contract.
            return DispatchResult::i32(-ENOSPC);
        }
        let resolved = match self.resolve_from_fd(dirfd, &path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        if let Err(errno) = self.ensure_write_mount(&resolved) {
            return DispatchResult::i32(-errno);
        }
        let host = match resolved.host {
            Some(host) => host,
            None => return DispatchResult::i32(-EROFS),
        };
        let result = if flags == AT_REMOVEDIR {
            fs::remove_dir(host)
        } else {
            fs::remove_file(host)
        };
        match result {
            Ok(()) => DispatchResult::i32(0),
            Err(error) => DispatchResult::i32(-map_io_error(error)),
        }
    }

    fn sys_renameat<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let old_dirfd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let old_path = match self.read_path_arg(args, 1, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let new_dirfd = match arg(args, 2) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let new_path = match self.read_path_arg(args, 3, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let old = match self.resolve_from_fd(old_dirfd, &old_path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        let new = match self.resolve_from_fd(new_dirfd, &new_path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        if old.mount != new.mount || self.ensure_write_mount(&old).is_err() {
            return DispatchResult::i32(-EROFS);
        }
        let (old_host, new_host) = match (old.host, new.host) {
            (Some(old), Some(new)) => (old, new),
            _ => return DispatchResult::i32(-EROFS),
        };
        match fs::rename(old_host, new_host) {
            Ok(()) => DispatchResult::i32(0),
            Err(error) => DispatchResult::i32(-map_io_error(error)),
        }
    }
}

impl SyscallService {
    fn sys_getdents<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let dirp = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let count = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let count = match usize::try_from(count) {
            Ok(value) => value,
            Err(_) => return DispatchResult::Trap(SyscallError::memory("getdents count too large")),
        };
        let entry = match self.fds.get_mut(&fd) {
            Some(entry) => entry,
            None => return DispatchResult::i32(-EBADF),
        };
        if !entry.is_directory() {
            return DispatchResult::i32(-ENOTDIR);
        }
        let start = (entry.dir_pos / DIRENT_SIZE) as usize;
        let max_entries = count / DIRENT_SIZE as usize;
        let end = start.saturating_add(max_entries).min(entry.dir_entries.len());
        let selected: Vec<(usize, String, bool, bool)> = entry.dir_entries[start..end]
            .iter()
            .enumerate()
            .map(|(index, value)| {
                (
                    start + index,
                    value.name.clone(),
                    value.is_dir,
                    value.is_symlink,
                )
            })
            .collect();
        for (index, name, is_dir, is_symlink) in &selected {
            let offset = dirp + ((*index - start) as u64) * DIRENT_SIZE;
            let mut record = [0u8; DIRENT_SIZE as usize];
            let ino = stable_inode(&format!("{}:{}", entry.virtual_path, name));
            record[0..8].copy_from_slice(&ino.to_le_bytes());
            let next = ((*index + 1) as u64) * DIRENT_SIZE;
            record[8..16].copy_from_slice(&next.to_le_bytes());
            record[16..18].copy_from_slice(&(DIRENT_SIZE as u16).to_le_bytes());
            record[18] = if *is_symlink { 10 } else if *is_dir { 4 } else { 8 };
            let name_bytes = name.as_bytes();
            let copy_len = name_bytes.len().min(255);
            record[19..19 + copy_len].copy_from_slice(&name_bytes[..copy_len]);
            match memory.write_bytes(offset, &record) {
                Ok(()) => {}
                Err(error) => return DispatchResult::Trap(error),
            }
        }
        entry.dir_pos = (end as u64) * DIRENT_SIZE;
        DispatchResult::i32((selected.len() * DIRENT_SIZE as usize) as i32)
    }

    fn sys_fcntl<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let cmd = match arg(args, 1) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let varargs = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let entry = match self.fds.get(&fd) {
            Some(entry) => entry,
            None => return DispatchResult::i32(-EBADF),
        };
        match cmd {
            F_DUPFD => {
                let min_fd = match memory.read_i32(varargs) {
                    Ok(value) => value,
                    Err(error) => return DispatchResult::Trap(error),
                };
                if min_fd < 0 {
                    return DispatchResult::i32(-ENOSPC);
                }
                let duplicate = match entry.duplicate() {
                    Ok(value) => value,
                    Err(errno) => return DispatchResult::i32(-errno),
                };
                let fd = match self.next_fd_at_least(min_fd) {
                    Ok(value) => value,
                    Err(errno) => return DispatchResult::i32(-errno),
                };
                self.fds.insert(fd, duplicate);
                DispatchResult::i32(fd)
            }
            F_GETFD | F_SETFD => DispatchResult::i32(0),
            F_GETFL => DispatchResult::i32(entry.flags),
            F_SETFL => {
                let flags = match memory.read_i32(varargs) {
                    Ok(value) => value,
                    Err(error) => return DispatchResult::Trap(error),
                };
                if let Some(entry) = self.fds.get_mut(&fd) {
                    entry.flags = entry.flags & !F_SETFL_MASK | flags & F_SETFL_MASK;
                }
                DispatchResult::i32(0)
            }
            F_GETLK => {
                let argp = match memory.read_u64(varargs) {
                    Ok(value) => value,
                    Err(error) => return DispatchResult::Trap(error),
                };
                match memory.write_i16(argp, 2) {
                    Ok(()) => DispatchResult::i32(0),
                    Err(error) => DispatchResult::Trap(error),
                }
            }
            F_SETLK | F_SETLKW => DispatchResult::i32(0),
            _ => DispatchResult::i32(-ENOSPC),
        }
    }

    fn next_fd_at_least(&mut self, min_fd: i32) -> Result<i32, i32> {
        let mut fd = min_fd.max(3);
        loop {
            if !self.fds.contains_key(&fd) {
                return Ok(fd);
            }
            fd = fd.checked_add(1).ok_or(ENOSPC)?;
        }
    }

    fn sys_ioctl<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let op = match arg(args, 1) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let varargs = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let argp = match memory.read_u64(varargs) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let tty = match self.fds.get(&fd) {
            Some(entry) => entry.is_tty(),
            None => return DispatchResult::i32(-EBADF),
        };
        match op {
            FIONBIO | TCGETS | TCSETS | TCSETSW | TCSETSF | TCXONC | TCFLSH
            | TIOCGPGRP | TIOCSWINSZ => {
                if !tty {
                    return DispatchResult::i32(-ENOTTY);
                }
                if op == TCGETS {
                    for offset in [0, 4, 8, 12] {
                        if let Err(error) = memory.write_u32(argp + offset, 0) {
                            return DispatchResult::Trap(error);
                        }
                    }
                    for offset in 0..32u64 {
                        if let Err(error) = memory.write_u8(argp + 17 + offset, 0) {
                            return DispatchResult::Trap(error);
                        }
                    }
                } else if op == TIOCGPGRP {
                    if let Err(error) = memory.write_i32(argp, 0) {
                        return DispatchResult::Trap(error);
                    }
                }
                DispatchResult::i32(0)
            }
            TIOCGWINSZ => {
                if !tty {
                    return DispatchResult::i32(-ENOTTY);
                }
                match (memory.write_u16(argp, 24), memory.write_u16(argp + 2, 80)) {
                    (Ok(()), Ok(())) => DispatchResult::i32(0),
                    (Err(error), _) | (_, Err(error)) => DispatchResult::Trap(error),
                }
            }
            21_515 => {
                if !tty {
                    return DispatchResult::i32(-ENOTTY);
                }
                DispatchResult::i32(0)
            }
            FIONREAD | FIONBIO_ALT => DispatchResult::i32(-ENOSYS),
            TIOCSPGRP => {
                if tty {
                    DispatchResult::i32(-ENOSPC)
                } else {
                    DispatchResult::i32(-ENOTTY)
                }
            }
            _ => DispatchResult::i32(-ENOSPC),
        }
    }

    fn sys_poll<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let fds_ptr = match arg(args, 0) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let nfds = match arg(args, 1).and_then(|value| {
            i32::try_from(value as u32).map_err(|_| SyscallError::memory("poll nfds overflow"))
        }) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        if nfds < 0 || nfds as u64 > IOV_MAX {
            return DispatchResult::i32(-EINVAL);
        }
        let mut ready = 0i32;
        for index in 0..nfds as u64 {
            let pollfd = fds_ptr.saturating_add(index * 8);
            let fd = match memory.read_i32(pollfd) {
                Ok(value) => value,
                Err(error) => return DispatchResult::Trap(error),
            };
            let events = match memory.read_i16(pollfd + 4) {
                Ok(value) => value,
                Err(error) => return DispatchResult::Trap(error),
            };
            let revents = if !self.fds.contains_key(&fd) {
                POLLNVAL
            } else {
                let flags = POLLIN | POLLOUT;
                flags & (events | POLLERR | POLLHUP | POLLNVAL)
            };
            if revents != 0 {
                ready += 1;
            }
            if let Err(error) = memory.write_i16(pollfd + 6, revents) {
                return DispatchResult::Trap(error);
            }
        }
        DispatchResult::i32(ready)
    }

    fn wasi_read<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        self.wasi_rw(args, memory, false, None)
    }

    fn wasi_write<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        self.wasi_rw(args, memory, true, None)
    }

    fn wasi_pread<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let offset = match arg(args, 3) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        self.wasi_rw(args, memory, false, Some(offset))
    }

    fn wasi_pwrite<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let offset = match arg(args, 3) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        self.wasi_rw(args, memory, true, Some(offset))
    }

    fn wasi_rw<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
        write: bool,
        offset: Option<u64>,
    ) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let iov = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let iovcnt = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let result_ptr_index = if offset.is_some() { 4 } else { 3 };
        let result_ptr = match arg(args, result_ptr_index) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let result = if write {
            self.write_vectors(fd, iov, iovcnt, offset, memory)
        } else {
            self.read_vectors(fd, iov, iovcnt, offset, memory)
        };
        match result {
            Ok(count) => match memory.write_u64(result_ptr, count) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i32(errno),
        }
    }

    fn read_vectors<M: GuestMemory + ?Sized>(
        &mut self,
        fd: i32,
        iov: u64,
        iovcnt: u64,
        offset: Option<u64>,
        memory: &M,
    ) -> Result<u64, i32> {
        let vectors = self.read_iovecs(iov, iovcnt, memory)?;
        let mut total = 0u64;
        for (ptr, len) in vectors {
            if len == 0 {
                continue;
            }
            let data = self.read_fd(fd, len as usize, offset.map(|value| value + total))?;
            let amount = data.len() as u64;
            memory
                .write_bytes(ptr, &data)
                .map_err(|_| EINVAL)?;
            total = total.saturating_add(amount);
            if amount < len {
                break;
            }
        }
        Ok(total)
    }

    fn write_vectors<M: GuestMemory + ?Sized>(
        &mut self,
        fd: i32,
        iov: u64,
        iovcnt: u64,
        offset: Option<u64>,
        memory: &M,
    ) -> Result<u64, i32> {
        let vectors = self.read_iovecs(iov, iovcnt, memory)?;
        let mut total = 0u64;
        for (ptr, len) in vectors {
            if len == 0 {
                continue;
            }
            let data = memory.read_bytes(ptr, len as usize).map_err(|_| EINVAL)?;
            let amount = self.write_fd(fd, &data, offset.map(|value| value + total))?;
            total = total.saturating_add(amount);
            if amount < len {
                break;
            }
        }
        Ok(total)
    }

    fn read_iovecs<M: GuestMemory + ?Sized>(
        &self,
        iov: u64,
        iovcnt: u64,
        memory: &M,
    ) -> Result<Vec<(u64, u64)>, i32> {
        if iovcnt > IOV_MAX {
            return Err(EINVAL);
        }
        let mut vectors = Vec::with_capacity(iovcnt as usize);
        let mut total = 0u64;
        for index in 0..iovcnt {
            let base = iov.checked_add(index.saturating_mul(16)).ok_or(EINVAL)?;
            let ptr = memory.read_u64(base).map_err(|_| EINVAL)?;
            let len = memory.read_u64(base + 8).map_err(|_| EINVAL)?;
            total = total.checked_add(len).ok_or(EINVAL)?;
            if total > MAX_IO_BYTES {
                return Err(EINVAL);
            }
            vectors.push((ptr, len));
        }
        Ok(vectors)
    }

    fn read_fd(&mut self, fd: i32, len: usize, offset: Option<u64>) -> Result<Vec<u8>, i32> {
        let entry = self.fds.get_mut(&fd).ok_or(EBADF)?;
        match &mut entry.kind {
            FdKind::Stdin => {
                if offset.is_some() {
                    return Err(ESPIPE);
                }
                let start = entry.stdin_pos;
                let end = start.saturating_add(len).min(self.stdin.len());
                entry.stdin_pos = end;
                Ok(self.stdin[start..end].to_vec())
            }
            FdKind::File(file) => {
                if entry.flags & O_ACCMODE == O_WRONLY {
                    return Err(EBADF);
                }
                read_file(file, len, offset)
            }
            FdKind::Directory => Err(EISDIR),
            FdKind::Stdout | FdKind::Stderr => Err(EBADF),
        }
    }

    fn write_fd(&mut self, fd: i32, data: &[u8], offset: Option<u64>) -> Result<u64, i32> {
        let entry = self.fds.get_mut(&fd).ok_or(EBADF)?;
        match &mut entry.kind {
            FdKind::Stdout => {
                if offset.is_some() {
                    return Err(ESPIPE);
                }
                let take=data.len().min((256*1024usize).saturating_sub(self.stdout.len()));
                self.stdout.extend_from_slice(&data[..take]);
                Ok(data.len() as u64)
            }
            FdKind::Stderr => {
                if offset.is_some() {
                    return Err(ESPIPE);
                }
                let take=data.len().min((256*1024usize).saturating_sub(self.stderr.len()));
                self.stderr.extend_from_slice(&data[..take]);
                Ok(data.len() as u64)
            }
            FdKind::File(file) => {
                if entry.flags & O_ACCMODE == O_RDONLY {
                    return Err(EBADF);
                }
                write_file(file, data, offset, entry.flags & O_APPEND != 0)
            }
            FdKind::Stdin => Err(EBADF),
            FdKind::Directory => Err(EISDIR),
        }
    }

    fn wasi_seek<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let offset = match arg(args, 1) {
            Ok(value) => value as i64,
            Err(error) => return DispatchResult::Trap(error),
        };
        let whence = match arg(args, 2) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let new_offset = match arg(args, 3) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let result = self.seek_fd(fd, offset, whence);
        match result {
            Ok(position) => match memory.write_u64(new_offset, position) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i32(errno),
        }
    }

    fn seek_fd(&mut self, fd: i32, offset: i64, whence: i32) -> Result<u64, i32> {
        let entry = self.fds.get_mut(&fd).ok_or(EBADF)?;
        match &mut entry.kind {
            FdKind::File(file) => {
                let from = match whence {
                    0 => SeekFrom::Start(offset.max(0) as u64),
                    1 => SeekFrom::Current(offset),
                    2 => SeekFrom::End(offset),
                    _ => return Err(EINVAL),
                };
                file.seek(from).map_err(map_io_error)
            }
            FdKind::Directory => {
                let current = entry.dir_pos as i64;
                let next = match whence {
                    0 => offset,
                    1 => current.saturating_add(offset),
                    2 => (entry.dir_entries.len() as i64)
                        .saturating_mul(DIRENT_SIZE as i64)
                        .saturating_add(offset),
                    _ => return Err(EINVAL),
                };
                if next < 0 {
                    return Err(EINVAL);
                }
                entry.dir_pos = next as u64;
                Ok(entry.dir_pos)
            }
            _ => Err(ESPIPE),
        }
    }

    fn wasi_close(&mut self, args: &[u64]) -> DispatchResult {
        let fd = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        if self.fds.remove(&fd).is_some() {
            DispatchResult::i32(0)
        } else {
            DispatchResult::i32(EBADF)
        }
    }

    fn sys_statfs<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let path = match self.read_path_arg(args, 0, memory) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let buf = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let resolved = match self.resolve_from_fd(AT_FDCWD, &path, false) {
            Ok(value) => value,
            Err(errno) => return DispatchResult::i32(-errno),
        };
        if let Some(host) = resolved.host {
            if !host.exists() {
                return DispatchResult::i32(-ENOENT);
            }
        }
        // Exact defaults from FS.statfsNode in the inspected JS glue.
        match write_statfs(memory, buf) {
            Ok(()) => DispatchResult::i32(0),
            Err(error) => DispatchResult::Trap(error),
        }
    }
}

fn read_dir_entries(path: &Path) -> Result<Vec<DirEntryInfo>, i32> {
    let read_dir: ReadDir = fs::read_dir(path).map_err(map_io_error)?;
    let mut entries = vec![dir_entry(".", true, false), dir_entry("..", true, false)];
    for item in read_dir {
        let item = item.map_err(map_io_error)?;
        let file_type = item.file_type().map_err(map_io_error)?;
        let name = item.file_name().to_str().ok_or(EINVAL)?.to_string();
        entries.push(dir_entry(
            &name,
            file_type.is_dir(),
            file_type.is_symlink(),
        ));
    }
    Ok(entries)
}

fn dir_entry(name: &str, is_dir: bool, is_symlink: bool) -> DirEntryInfo {
    DirEntryInfo {
        name: name.to_string(),
        is_dir,
        is_symlink,
    }
}

fn stable_inode(path: &str) -> u64 {
    // Emscripten uses its own inode IDs.  A stable FNV-1a value preserves the
    // guest-visible identity without exposing a host inode or Android layout.
    let mut value = 0xcbf29ce484222325u64;
    for byte in path.as_bytes() {
        value ^= *byte as u64;
        value = value.wrapping_mul(0x100000001b3);
    }
    value.max(1)
}

#[derive(Debug, Clone, Copy)]
struct StatData {
    dev: u32,
    mode: u32,
    nlink: u64,
    uid: u32,
    gid: u32,
    rdev: u32,
    size: i64,
    blocks: i32,
    atime: i64,
    atime_nsec: u64,
    mtime: i64,
    mtime_nsec: u64,
    ctime: i64,
    ctime_nsec: u64,
    ino: i64,
}

impl StatData {
    fn directory(path: &str) -> Self {
        Self {
            dev: 0,
            mode: 0o040000 | 0o755,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            blocks: 0,
            atime: 0,
            atime_nsec: 0,
            mtime: 0,
            mtime_nsec: 0,
            ctime: 0,
            ctime_nsec: 0,
            ino: stable_inode(path) as i64,
        }
    }
}

fn stat_data_from_metadata(metadata: &fs::Metadata, path: &str) -> Result<StatData, i32> {
    let mode_type = if metadata.is_dir() {
        0o040000
    } else if metadata.file_type().is_symlink() {
        0o120000
    } else {
        0o100000
    };
    let permissions = if metadata.permissions().readonly() {
        0o444
    } else if metadata.is_dir() {
        0o755
    } else {
        0o644
    };
    let size = i64::try_from(metadata.len()).map_err(|_| EIO)?;
    let blocks = ((metadata.len().saturating_add(511)) / 512).min(i32::MAX as u64) as i32;
    let (atime, atime_nsec) = system_time_parts(metadata.accessed().unwrap_or(UNIX_EPOCH));
    let (mtime, mtime_nsec) = system_time_parts(metadata.modified().unwrap_or(UNIX_EPOCH));
    let (ctime, ctime_nsec) = system_time_parts(metadata.created().unwrap_or_else(|_| {
        metadata.modified().unwrap_or(UNIX_EPOCH)
    }));
    Ok(StatData {
        dev: 0,
        mode: mode_type | permissions,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        size,
        blocks,
        atime,
        atime_nsec,
        mtime,
        mtime_nsec,
        ctime,
        ctime_nsec,
        ino: stable_inode(path) as i64,
    })
}

fn system_time_parts(value: SystemTime) -> (i64, u64) {
    match value.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos() as u64),
        Err(error) => {
            let duration = error.duration();
            (-(duration.as_secs() as i64), duration.subsec_nanos() as u64)
        }
    }
}

fn write_stat<M: GuestMemory + ?Sized>(
    memory: &M,
    ptr: u64,
    stat: &StatData,
) -> Result<(), SyscallError> {
    memory.write_u32(ptr, stat.dev)?;
    memory.write_u32(ptr + 4, stat.mode)?;
    memory.write_u64(ptr + 8, stat.nlink)?;
    memory.write_u32(ptr + 16, stat.uid)?;
    memory.write_u32(ptr + 20, stat.gid)?;
    memory.write_u32(ptr + 24, stat.rdev)?;
    memory.write_i64(ptr + 32, stat.size)?;
    memory.write_i32(ptr + 40, 4096)?;
    memory.write_i32(ptr + 44, stat.blocks)?;
    memory.write_i64(ptr + 48, stat.atime)?;
    memory.write_u64(ptr + 56, stat.atime_nsec)?;
    memory.write_i64(ptr + 64, stat.mtime)?;
    memory.write_u64(ptr + 72, stat.mtime_nsec)?;
    memory.write_i64(ptr + 80, stat.ctime)?;
    memory.write_u64(ptr + 88, stat.ctime_nsec)?;
    memory.write_i64(ptr + 96, stat.ino)?;
    Ok(())
}

fn write_statfs<M: GuestMemory + ?Sized>(memory: &M, ptr: u64) -> Result<(), SyscallError> {
    memory.write_u32(ptr + 8, 4096)?;
    memory.write_u32(ptr + 72, 4096)?;
    memory.write_u64(ptr + 16, 1_000_000)?;
    memory.write_u64(ptr + 24, 500_000)?;
    memory.write_u64(ptr + 32, 500_000)?;
    memory.write_u64(ptr + 40, 1_000_000)?;
    memory.write_u64(ptr + 48, 999_999)?;
    memory.write_u32(ptr + 56, 42)?;
    memory.write_u32(ptr + 80, 2)?;
    memory.write_u32(ptr + 64, 255)?;
    Ok(())
}

fn read_file(file: &mut File, len: usize, offset: Option<u64>) -> Result<Vec<u8>, i32> {
    if let Some(offset) = offset {
        let saved = file.stream_position().map_err(map_io_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
        let result = read_file_current(file, len);
        let _ = file.seek(SeekFrom::Start(saved));
        result
    } else {
        read_file_current(file, len)
    }
}

fn read_file_current(file: &mut File, len: usize) -> Result<Vec<u8>, i32> {
    let mut bytes = vec![0u8; len];
    let count = file.read(&mut bytes).map_err(map_io_error)?;
    bytes.truncate(count);
    Ok(bytes)
}

fn write_file(
    file: &mut File,
    bytes: &[u8],
    offset: Option<u64>,
    append: bool,
) -> Result<u64, i32> {
    if let Some(offset) = offset {
        let saved = file.stream_position().map_err(map_io_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
        let result = file.write(bytes).map(|count| count as u64).map_err(map_io_error);
        let _ = file.seek(SeekFrom::Start(saved));
        return result;
    }
    if append {
        file.seek(SeekFrom::End(0)).map_err(map_io_error)?;
    }
    file.write(bytes)
        .map(|count| count as u64)
        .map_err(map_io_error)
}

// The Emscripten JS implementation writes a UTC `struct tm` at offsets 0..28.
fn gmtime_parts(seconds: i64) -> Option<TimeParts> {
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let hour = (day_seconds / 3_600) as i32;
    let minute = ((day_seconds % 3_600) / 60) as i32;
    let second = (day_seconds % 60) as i32;
    let (year, month, day) = civil_from_days(days);
    let yday = days - days_from_civil(year, 1, 1);
    let wday = (days + 4).rem_euclid(7);
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&year) {
        return None;
    }
    Some(TimeParts {
        sec: second,
        min: minute,
        hour,
        mday: day as i32,
        mon: month as i32 - 1,
        year: year as i32 - 1900,
        wday: wday as i32,
        yday: yday as i32,
        offset_east: 0,
        is_dst: 0,
    })
}

fn write_tm<M: GuestMemory + ?Sized>(
    memory: &M,
    ptr: u64,
    parts: TimeParts,
    local: bool,
) -> Result<(), SyscallError> {
    memory.write_i32(ptr, parts.sec)?;
    memory.write_i32(ptr + 4, parts.min)?;
    memory.write_i32(ptr + 8, parts.hour)?;
    memory.write_i32(ptr + 12, parts.mday)?;
    memory.write_i32(ptr + 16, parts.mon)?;
    memory.write_i32(ptr + 20, parts.year)?;
    memory.write_i32(ptr + 24, parts.wday)?;
    memory.write_i32(ptr + 28, parts.yday)?;
    if local {
        memory.write_i32(ptr + 32, parts.is_dst)?;
        memory.write_i64(ptr + 40, parts.offset_east)?;
    }
    Ok(())
}

impl SyscallService {
    fn clock_time_get<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let clock_id = match arg(args, 0) {
            Ok(value) => as_i32(value),
            Err(error) => return DispatchResult::Trap(error),
        };
        let ptime = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        if !(0..=3).contains(&clock_id) {
            return DispatchResult::i32(EINVAL);
        }
        let now_ms = if clock_id == 0 {
            self.date_now_ms()
        } else {
            self.now_ms()
        };
        let nanos = (now_ms * 1_000_000.0).round();
        if !nanos.is_finite() || nanos < i64::MIN as f64 || nanos > i64::MAX as f64 {
            return DispatchResult::i32(EIO);
        }
        match memory.write_i64(ptime, nanos as i64) {
            Ok(()) => DispatchResult::i32(0),
            Err(error) => DispatchResult::Trap(error),
        }
    }

    fn gmtime_js<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let seconds = match arg(args, 0) {
            Ok(value) => value as i64,
            Err(error) => return DispatchResult::Trap(error),
        };
        let ptr = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let parts = match gmtime_parts(seconds) {
            Some(parts) => parts,
            None => return DispatchResult::i32(1),
        };
        match write_tm(memory, ptr, parts, false) {
            Ok(()) => DispatchResult::i32(0),
            Err(error) => DispatchResult::Trap(error),
        }
    }

    fn localtime_js<M: GuestMemory + ?Sized>(
        &mut self,
        args: &[u64],
        memory: &M,
    ) -> DispatchResult {
        let seconds = match arg(args, 0) {
            Ok(value) => value as i64,
            Err(error) => return DispatchResult::Trap(error),
        };
        let ptr = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        match local_time_parts(seconds) {
            Ok(parts) => match write_tm(memory, ptr, parts, true) {
                Ok(()) => DispatchResult::i32(0),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i32(errno),
        }
    }

    fn mktime_js<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let ptr = match arg(args, 0) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let read = |offset: u64| memory.read_i32(ptr + offset);
        let input = match (
            read(0),
            read(4),
            read(8),
            read(12),
            read(16),
            read(20),
            read(32),
        ) {
            (Ok(sec), Ok(min), Ok(hour), Ok(mday), Ok(mon), Ok(year), Ok(is_dst)) => {
                (sec, min, hour, mday, mon, year, is_dst)
            }
            _ => return DispatchResult::Trap(SyscallError::memory("mktime tm outside guest memory")),
        };
        match local_mktime(input) {
            Ok((seconds, parts)) => match write_tm(memory, ptr, parts, true) {
                Ok(()) => DispatchResult::i64(seconds),
                Err(error) => DispatchResult::Trap(error),
            },
            Err(errno) => DispatchResult::i64(-(errno as i64)),
        }
    }

    fn tzset_js<M: GuestMemory + ?Sized>(&mut self, args: &[u64], memory: &M) -> DispatchResult {
        let timezone = match arg(args, 0) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let daylight = match arg(args, 1) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let std_name = match arg(args, 2) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        let dst_name = match arg(args, 3) {
            Ok(value) => value,
            Err(error) => return DispatchResult::Trap(error),
        };
        match timezone_parts() {
            Ok((west, has_dst, std, dst)) => {
                if let Err(error) = memory.write_i64(timezone, west) {
                    return DispatchResult::Trap(error);
                }
                if let Err(error) = memory.write_i32(daylight, has_dst) {
                    return DispatchResult::Trap(error);
                }
                if let Err(error) = write_guest_c_string(memory, std_name, &std, 17) {
                    return DispatchResult::Trap(error);
                }
                if let Err(error) = write_guest_c_string(memory, dst_name, &dst, 17) {
                    return DispatchResult::Trap(error);
                }
                DispatchResult::Returned(DispatchValue::I32(0))
            }
            Err(_) => DispatchResult::Trap(SyscallError::unsupported("timezone support")),
        }
    }
}

fn write_guest_c_string<M: GuestMemory + ?Sized>(
    memory: &M,
    ptr: u64,
    value: &str,
    capacity: usize,
) -> Result<(), SyscallError> {
    let mut bytes = value.as_bytes().to_vec();
    if capacity == 0 {
        return Ok(());
    }
    bytes.truncate(capacity - 1);
    bytes.push(0);
    memory.write_bytes(ptr, &bytes)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_from_march = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096)
        / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(unix)]
unsafe extern "C" {
    fn localtime_r(time: *const i64, out: *mut u8) -> *mut u8;
    fn strftime(out: *mut c_char, max: usize, format: *const c_char, tm: *const u8) -> usize;
}

#[repr(align(16))]
struct AlignedTm([u8;128]);

#[cfg(unix)]
fn local_time_parts(seconds: i64) -> Result<TimeParts, i32> {
    let mut raw = AlignedTm([0u8; 128]);
    let tm = unsafe { localtime_r(&seconds, raw.0.as_mut_ptr()) };
    if tm.is_null() {
        return Err(EINVAL);
    }
    let format = CString::new("%S|%M|%H|%d|%m|%Y|%w|%j|%z").map_err(|_| EINVAL)?;
    let mut output = [0 as c_char; 128];
    let length = unsafe {
        strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            raw.0.as_ptr(),
        )
    };
    if length == 0 {
        return Err(EINVAL);
    }
    let text = unsafe { CStr::from_ptr(output.as_ptr()) }
        .to_str()
        .map_err(|_| EINVAL)?;
    let fields: Vec<&str> = text.split('|').collect();
    if fields.len() != 9 {
        return Err(EINVAL);
    }
    let parse = |index: usize| fields[index].parse::<i32>().map_err(|_| EINVAL);
    let offset_east = parse_zone_offset(fields[8])?;
    let year = parse(5)?;
    let jan = timezone_offset_at(days_from_civil(year as i64, 1, 1) * 86_400)?;
    let jul = timezone_offset_at(days_from_civil(year as i64, 7, 1) * 86_400)?;
    let is_dst = if jan != jul && offset_east == jan.max(jul) { 1 } else { 0 };
    Ok(TimeParts {
        sec: parse(0)?,
        min: parse(1)?,
        hour: parse(2)?,
        mday: parse(3)?,
        mon: parse(4)? - 1,
        year: year - 1900,
        wday: parse(6)?,
        yday: parse(7)? - 1,
        offset_east,
        is_dst,
    })
}

#[cfg(not(unix))]
fn local_time_parts(_seconds: i64) -> Result<TimeParts, i32> {
    Err(ENOSYS)
}

#[cfg(unix)]
fn timezone_offset_at(seconds: i64) -> Result<i64, i32> {
    local_time_parts_raw_offset(seconds)
}

#[cfg(not(unix))]
fn timezone_offset_at(_seconds: i64) -> Result<i64, i32> {
    Err(ENOSYS)
}

#[cfg(unix)]
fn local_time_parts_raw_offset(seconds: i64) -> Result<i64, i32> {
    let mut raw = AlignedTm([0u8; 128]);
    let tm = unsafe { localtime_r(&seconds, raw.0.as_mut_ptr()) };
    if tm.is_null() {
        return Err(EINVAL);
    }
    let format = CString::new("%z").map_err(|_| EINVAL)?;
    let mut output = [0 as c_char; 16];
    let length = unsafe {
        strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            raw.0.as_ptr(),
        )
    };
    if length == 0 {
        return Err(EINVAL);
    }
    let text = unsafe { CStr::from_ptr(output.as_ptr()) }
        .to_str()
        .map_err(|_| EINVAL)?;
    parse_zone_offset(text)
}

fn parse_zone_offset(value: &str) -> Result<i64, i32> {
    if value.len() < 5 {
        return Err(EINVAL);
    }
    let sign = match value.as_bytes()[0] {
        b'+' => 1i64,
        b'-' => -1i64,
        _ => return Err(EINVAL),
    };
    let digits: String = value[1..].chars().filter(|character| *character != ':').collect();
    if digits.len() < 4 {
        return Err(EINVAL);
    }
    let hours = digits[0..2].parse::<i64>().map_err(|_| EINVAL)?;
    let minutes = digits[2..4].parse::<i64>().map_err(|_| EINVAL)?;
    Ok(sign * (hours * 3_600 + minutes * 60))
}

#[cfg(unix)]
fn timezone_parts() -> Result<(i64, i32, String, String), i32> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| EINVAL)?
        .as_secs() as i64;
    let (year, _, _) = civil_from_days(now.div_euclid(86_400));
    let winter = timezone_offset_at(days_from_civil(year, 1, 1) * 86_400)?;
    let summer = timezone_offset_at(days_from_civil(year, 7, 1) * 86_400)?;
    let standard = winter.min(summer);
    let dst = winter != summer;
    let (std_name, dst_name) = if summer > winter {
        (zone_name(winter), zone_name(summer))
    } else {
        (zone_name(summer), zone_name(winter))
    };
    Ok((-standard, i32::from(dst), std_name, dst_name))
}

#[cfg(not(unix))]
fn timezone_parts() -> Result<(i64, i32, String, String), i32> {
    Err(ENOSYS)
}

fn zone_name(offset_east: i64) -> String {
    let sign = if offset_east >= 0 { '+' } else { '-' };
    let absolute = offset_east.unsigned_abs();
    format!("UTC{sign}{:02}{:02}", absolute / 3_600, (absolute / 60) % 60)
}

fn local_mktime(input: (i32, i32, i32, i32, i32, i32, i32)) -> Result<(i64, TimeParts), i32> {
    let (sec, min, hour, mday, mon, year_since_1900, is_dst) = input;
    let year = year_since_1900 as i64 + 1900;
    if !(0..=11).contains(&mon) || !(1..=31).contains(&mday) || !(0..=23).contains(&hour)
        || !(0..=59).contains(&min) || !(0..=60).contains(&sec)
    {
        return Err(EINVAL);
    }
    let nominal = days_from_civil(year, mon as i64 + 1, mday as i64)
        .saturating_mul(86_400)
        .saturating_add(hour as i64 * 3_600 + min as i64 * 60 + sec as i64);
    let mut candidates = Vec::new();
    #[cfg(unix)]
    {
        for probe in [nominal, nominal - 180 * 86_400, nominal + 180 * 86_400] {
            if let Ok(offset) = timezone_offset_at(probe) {
                candidates.push(nominal - offset);
            }
        }
    }
    if candidates.is_empty() {
        return Err(ENOSYS);
    }
    candidates.sort_unstable();
    candidates.dedup();
    let mut matches = Vec::new();
    for candidate in candidates {
        for adjustment in [-7_200i64, -3_600, 0, 3_600, 7_200] {
            let value = candidate.saturating_add(adjustment);
            if let Ok(parts) = local_time_parts(value) {
                if parts.year == year_since_1900
                    && parts.mon == mon
                    && parts.mday == mday
                    && parts.hour == hour
                    && parts.min == min
                    && parts.sec == sec
                {
                    matches.push((value, parts));
                }
            }
        }
    }
    if matches.is_empty() {
        return Err(EINVAL);
    }
    matches.sort_by_key(|(_, parts)| {
        if is_dst > 0 {
            i32::from(parts.is_dst == 0)
        } else if is_dst == 0 {
            i32::from(parts.is_dst != 0)
        } else {
            0
        }
    });
    Ok(matches[0])
}


