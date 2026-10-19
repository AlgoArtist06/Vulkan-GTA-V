//! Optional CPU shader artifacts. Cache failures always fall back to the compiler.

use crate::gpu::{compile_spirv, pack_binding};
use crate::shader::{PackMember, ShaderReflection, localize_registers, pack_uniforms, reflect_wgsl};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};

const MAGIC: &[u8; 8] = b"GTASHDR1";
const MAX_ENTRY_BYTES: usize = 4 << 20;
const MAX_CACHE_BYTES: u64 = 128 << 20;
const MAX_CACHE_ENTRIES: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PreparedShader {
    pub source: String,
    pub reflection: ShaderReflection,
    pub pack: Option<Vec<PackMember>>,
    pub spirv: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum Artifact {
    Prepared(PreparedShader),
    Spirv(Vec<u32>),
}

impl Artifact {
    fn words(&self) -> &[u32] {
        match self {
            Self::Prepared(shader) => &shader.spirv,
            Self::Spirv(words) => words,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    compiler: u64,
    source: String,
    stage: u32,
    packing: Option<u32>,
    artifact: Artifact,
}

struct PendingWrite {
    path: PathBuf,
    record: Record,
}

pub(crate) struct ShaderCache {
    directory: Option<PathBuf>,
    writer: Option<mpsc::SyncSender<PendingWrite>>,
}

impl ShaderCache {
    pub fn new(shader_dir: Option<&Path>) -> Self {
        let directory = shader_dir.and_then(Path::parent).map(|dir| dir.join("vk-shader-cache-v1"));
        let writer = directory.as_ref().and_then(|_| {
            let (send, receive) = mpsc::sync_channel::<PendingWrite>(2);
            std::thread::Builder::new().name("gta-shader-cache".into()).spawn(move || {
                for pending in receive {
                    // Serialization, eviction and filesystem writes never run on the graphics worker.
                    let _ = persist(pending);
                }
            }).ok().map(|_| send)
        });
        Self { directory, writer }
    }

    pub fn prepare(&self, source: &str, stage: u32) -> Result<PreparedShader, String> {
        let artifact = self.get_or_build(source, stage, None, true, || {
            let (source, pack) = match pack_uniforms(source)? {
                Some((source, members)) => (source, Some(members)),
                None => (source.to_owned(), None),
            };
            let source = localize_registers(&source);
            let reflection = reflect_wgsl(&source)?;
            let spirv = compile_spirv(&source, stage, pack.is_some().then(|| pack_binding(stage)))?;
            Ok(Artifact::Prepared(PreparedShader { source, reflection, pack, spirv }))
        })?;
        match artifact {
            Artifact::Prepared(shader) => Ok(shader),
            Artifact::Spirv(_) => unreachable!("prepared artifact requested"),
        }
    }

    pub fn compile(&self, source: &str, stage: u32, packing: Option<u32>) -> Result<Vec<u32>, String> {
        let artifact = self.get_or_build(source, stage, packing, false, || {
            compile_spirv(source, stage, packing).map(Artifact::Spirv)
        })?;
        match artifact {
            Artifact::Spirv(words) => Ok(words),
            Artifact::Prepared(_) => unreachable!("SPIR-V artifact requested"),
        }
    }

    fn get_or_build(&self, source: &str, stage: u32, packing: Option<u32>, prepared: bool,
        build: impl FnOnce() -> Result<Artifact, String>) -> Result<Artifact, String> {
        let path = self.directory.as_ref().map(|dir| dir.join(cache_name(source, stage, packing, prepared)));
        if let Some(record) = path.as_deref().and_then(read_record) {
            // The hash names the file; exact equality decides whether it is safe to reuse.
            if record.compiler == compiler_revision() && record.source == source && record.stage == stage
                && record.packing == packing && matches!(record.artifact, Artifact::Prepared(_)) == prepared {
                return Ok(record.artifact);
            }
        }
        let artifact = build()?;
        if let (Some(path), Some(writer)) = (path, &self.writer) {
            let mut estimated = source.len().saturating_add(artifact.words().len().saturating_mul(12));
            if let Artifact::Prepared(shader) = &artifact {
                estimated = estimated.saturating_add(shader.source.len());
            }
            if estimated <= MAX_ENTRY_BYTES {
                // A full queue drops only the optional cache write; it never delays a draw.
                let _ = writer.try_send(PendingWrite { path, record: Record {
                    compiler: compiler_revision(), source: source.to_owned(), stage, packing, artifact: artifact.clone(),
                }});
            }
        }
        Ok(artifact)
    }
}

fn fingerprint(parts: &[&[u8]]) -> u64 {
    parts.iter().flat_map(|bytes| bytes.iter()).fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn compiler_revision() -> u64 {
    static REVISION: OnceLock<u64> = OnceLock::new();
    *REVISION.get_or_init(|| fingerprint(&[
        include_bytes!("../Cargo.toml"), include_bytes!("../Cargo.lock"), include_bytes!("gpu.rs"), include_bytes!("shader.rs"),
        include_bytes!("shader_storage.rs"), include_bytes!("shader_constants.rs"), include_bytes!("shader_cache.rs"),
    ]))
}

fn cache_name(source: &str, stage: u32, packing: Option<u32>, prepared: bool) -> String {
    let hash = fingerprint(&[&compiler_revision().to_le_bytes(), &stage.to_le_bytes(),
        &packing.unwrap_or(u32::MAX).to_le_bytes(), &[u8::from(prepared)], source.as_bytes()]);
    format!("{hash:016x}.bin")
}

fn valid_spirv(words: &[u32]) -> bool {
    if words.len() < 5 || words[0] != 0x07230203 || words[1] != 0x00010600 || words[3] == 0 || words[4] != 0 {
        return false;
    }
    let mut at = 5;
    while at < words.len() {
        let count = (words[at] >> 16) as usize;
        if count == 0 || count > words.len() - at { return false; }
        at += count;
    }
    true
}

fn read_record(path: &Path) -> Option<Record> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take((MAX_ENTRY_BYTES + 1) as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() < 16 || bytes.len() > MAX_ENTRY_BYTES || &bytes[..8] != MAGIC { return None; }
    let checksum = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    if fingerprint(&[&bytes[16..]]) != checksum { return None; }
    let record: Record = serde_json::from_slice(&bytes[16..]).ok()?;
    valid_spirv(record.artifact.words()).then_some(record)
}

fn persist(pending: PendingWrite) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let data = serde_json::to_vec(&pending.record)?;
    let size = data.len().saturating_add(16);
    if size > MAX_ENTRY_BYTES { return Ok(()); }
    let directory = pending.path.parent().ok_or_else(|| std::io::Error::other("shader cache has no parent"))?;
    fs::create_dir_all(directory)?;
    let mut files = Vec::new();
    let mut total = size as u64;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == pending.path || !matches!(path.extension().and_then(|ext| ext.to_str()), Some("bin" | "tmp")) { continue; }
        let metadata = entry.metadata()?;
        if !metadata.is_file() { continue; }
        total = total.saturating_add(metadata.len());
        files.push((metadata.modified().unwrap_or(std::time::UNIX_EPOCH), path, metadata.len()));
    }
    files.sort_unstable_by_key(|entry| entry.0);
    let mut count = files.len() + 1;
    for (_, path, size) in files {
        if total <= MAX_CACHE_BYTES && count <= MAX_CACHE_ENTRIES { break; }
        fs::remove_file(path)?;
        total = total.saturating_sub(size);
        count -= 1;
    }
    let temporary = pending.path.with_extension(format!("{}-{}.tmp", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(MAGIC)?;
        file.write_all(&fingerprint(&[&data]).to_le_bytes())?;
        file.write_all(&data)?;
        drop(file);
        fs::rename(&temporary, &pending.path)
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}


