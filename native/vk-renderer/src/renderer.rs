//! Native D3D11-command consumer that records Vulkan directly.
//!
//! The engine writes the browser worker's command ring; this thread decodes it and
//! records one command buffer per submit. State the browser worker kept in WebGPU
//! objects is kept here as plain Vulkan handles plus per-subresource image layouts.

use crate::gpu::*;
use crate::protocol::{DecodeError, Opcode, SharedMemory, SharedMemoryError, SharedRingDecoder, MAX_RESOURCE_ID};
use crate::shader::{reflect_wgsl, BindingKind, BindingReflection, PackMember, ShaderReflection, ShaderSourceStore, StorageFormat, TextureDimension, TextureSample};
use crate::surface::{NativeSurface, SurfaceError};
use crate::movie::{CapturedFrame, MovieFrameSource, MovieScope, MovieTarget};
use ash::vk;
use gpu_allocator::vulkan::{Allocation, AllocationScheme};
use gpu_allocator::MemoryLocation;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use thiserror::Error;

const MAX_BUFFER_BYTES: u64 = 1 << 30;
const MAX_TEXTURE_SIDE: u32 = 16_384;
const GPU_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COMMANDS_PER_SHARED_PASS: usize = 1024;
const OCCLUSION_SLOTS: u32 = 2048;
/// Live draw pipelines. One frame uses at most ~350 (S24: Sandbox, driving, flying); each costs
/// ~0.12 MiB of driver memory, and re-creating an evicted one from the warm pipeline cache ~0.5 ms.
// ponytail: linear LRU scan on insert; a min-heap if inserts show up in profiles.
const PIPELINE_CACHE: usize = 512;
/// Command-buffer slots rotated at present: at most this many frames are queued on the GPU.
const FRAMES_IN_FLIGHT: usize = 2;
/// Submits without a present after which the slot rotates anyway (long loading screens).
const SUBMITS_PER_SLOT: u32 = 32;
const UNIFORM_CHUNK: u64 = 4 << 20;
const STAGING_CHUNK: u64 = 8 << 20;
/// Staging bytes queued on the GPU before an upload waits for the oldest to finish.
const STAGING_IN_FLIGHT: u64 = 96 << 20;
const TEXTURE_EVICT_AGE: u64 = 1800;
const TEXTURE_EVICT_IDLE_TIME: Duration = Duration::from_secs(60);
const TEXTURE_EVICT_CHUNK: u64 = 4 << 20;
const TEXTURE_EVICT_MIN_BYTES: u64 = 512 << 10;
const TEXTURE_BACKING_LIMIT: u64 = 1 << 30;
/// The null D3D11 layer's storage counters in guest memory (this game.wasm build): CPU storage live bytes,
/// resource count, peak bytes, upload shadow bytes, each a u64 (evidence/loop-2026-10-13/re/heap2/report.md).
const NULL_D3D_STORAGE_COUNTERS: usize = 8_324_208;
/// Timestamps per frame slot for `debug.gta5.pass_profile`: two per render pass.
const PROFILE_QUERIES: u32 = 1024;

#[derive(Debug, Error)]
pub enum RendererError {
    #[error("ring decode failed: {0}")]
    Decode(#[from] DecodeError),
    #[error("Vulkan unavailable: {0}")]
    GpuUnavailable(String),
    #[error("Vulkan operation failed: {0}")]
    Vulkan(String),
    #[error("{opcode:?} unsupported: {detail}")]
    Unsupported { opcode: Opcode, detail: String },
    #[error("{opcode:?} rejected: {detail}")]
    Command { opcode: Opcode, detail: String },
    #[error("shared memory range {address}..{end} is outside {length} bytes")]
    MemoryRange { address: usize, end: usize, length: usize },
    #[error("shared memory access failed: {0}")]
    SharedMemory(#[from] SharedMemoryError),
    #[error("surface operation failed: {0}")]
    Surface(#[from] SurfaceError),
}

fn vk_error(context: &str) -> impl Fn(vk::Result) -> RendererError + '_ {
    move |error| RendererError::Vulkan(format!("{context}: {error}"))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessReport {
    pub executed: u32,
    pub validation_errors: u32,
    pub presents: u32,
    pub draws: u32,
    pub dispatches: u32,
    pub unsupported: u32,
    /// The browser worker's "world shown": the title screen may go.
    pub world: bool,
}

/// Resource ids are small dense integers: multiply-shift hashing beats SipHash on the hot path.
#[derive(Default)]
struct IdHasher(u64);
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        // splitmix64's finalizer: hashbrown reads both the low (bucket) and top (tag) bits.
        let mut x = self.0;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
    fn write(&mut self, bytes: &[u8]) { for byte in bytes { self.0 = (self.0.rotate_left(5) ^ u64::from(*byte)).wrapping_mul(0x51_7cc1_b727_220a_95); } }
    fn write_u32(&mut self, value: u32) { self.0 = (self.0.rotate_left(5) ^ u64::from(value)).wrapping_mul(0x51_7cc1_b727_220a_95); }
    fn write_u64(&mut self, value: u64) { self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x51_7cc1_b727_220a_95); }
    fn write_usize(&mut self, value: usize) { self.write_u64(value as u64); }
}
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;
type FastSet<K> = HashSet<K, BuildHasherDefault<IdHasher>>;

struct BufferResource {
    raw: vk::Buffer,
    memory: Allocation,
    usage: vk::BufferUsageFlags,
    size: u64,
    misc: u32,
    stride: u32,
    /// Constant buffers: the CPU copy draws pack from; `generation` counts its updates.
    shadow: Option<Vec<u8>>,
    generation: u64,
    /// The GPU copy of a constant buffer lags its shadow until a non-draw command needs it.
    stale: bool,
    /// The preserved shader ABI has one hidden atomic counter per UAV buffer.
    counter: Option<(vk::Buffer, Allocation)>,
    /// Timeline value of the last submission that used it (the recording one if still open).
    last_use: u64,
    /// `last_use` before the open recording first used it: the latest use by submitted work.
    submitted_use: u64,
    /// Recording values that hold a staged copy into it, before (`pre_write`) or among (`cb_write`) its commands.
    pre_write: u64,
    cb_write: u64,
}

/// Where a buffer write goes so that it lands after every submitted command that uses the buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WritePlacement {
    /// Straight into mapped memory: no unfinished GPU work can see the old bytes.
    InPlace,
    /// A staged copy before the open recording's commands (after every submitted one).
    Before,
    /// A staged copy among the open recording's commands, after those already recorded.
    InOrder,
}

impl WritePlacement {
    fn of(buffer: &BufferResource, no_overwrite: bool, recording: u64, completed: u64) -> Self {
        // An ordered write keeps the old bytes for this recording's commands; a no-overwrite write
        // need not, but must stay behind an ordered one already queued in this recording.
        let in_order = if no_overwrite { buffer.cb_write == recording } else { buffer.last_use == recording };
        if in_order { Self::InOrder }
        else if buffer.pre_write == recording || buffer.submitted_before(recording) > completed { Self::Before }
        else { Self::InPlace }
    }
}

impl BufferResource {
    fn touch(&mut self, value: u64) {
        if value != self.last_use { self.submitted_use = self.last_use; self.last_use = value; }
    }
    /// The last submission other than the open one (`recording`) that reads or writes it.
    fn submitted_before(&self, recording: u64) -> u64 { if self.last_use == recording { self.submitted_use } else { self.last_use } }
}

struct TextureResource {
    raw: vk::Image,
    /// Bound on first GPU use; an unused target has no pixel storage.
    memory: Option<Allocation>,
    format: vk::Format,
    /// Other formats its views may use (a typeless family or an sRGB twin).
    views: &'static [vk::Format],
    info: FormatInfo,
    dxgi: u32,
    /// D3D11 bind flags it was created with.
    bind: u32,
    dimension: vk::ImageType,
    width: u32,
    height: u32,
    depth_or_layers: u32,
    mip_levels: u32,
    samples: u32,
    /// Per subresource (`mip + mip_levels * layer`); 3D textures have one layer.
    layouts: Vec<vk::ImageLayout>,
    /// `debug.gta5.exact_barriers`: false once part of the image was made readable without the vertex stage.
    vertex_visible: bool,
    last_use: u64,
    use_serial: u64,
    last_use_at: Option<Instant>,
    eviction_ticket: u64,
    backing: Option<TextureBacking>,
    /// A returned texture stays resident for this session, preventing eviction/reload loops.
    age_restored: bool,
    /// Bumped on every GPU write, for the float-to-depth proxy refresh.
    generation: u64,
    /// The device can sample it with linear filtering.
    filterable: bool,
}

#[derive(Clone)]
struct TextureBacking { offset: u64, size: u64, regions: Vec<vk::BufferImageCopy>, evicted_at: u64 }

enum EvictionCopy {
    Gpu { buffer: vk::Buffer, memory: Allocation, value: u64 },
    Disk(mpsc::Receiver<std::io::Result<()>>),
}

struct PendingEviction {
    id: u32, ticket: u64, serial: u64, generation: u64,
    backing: TextureBacking, copy: EvictionCopy,
}

#[derive(Default)]
struct TextureEviction {
    file: Option<Arc<std::fs::File>>, pending: Option<PendingEviction>,
    backing_space: crate::texture_backing::BackingSpace,
    ticket: u64, rotations: u64, checked_rotation: u64,
    clock: Option<Instant>,
    evicted: u64, released: u64, restored: u64, restored_bytes: u64,
    overwritten: u64, overwritten_bytes: u64,
    rapid_returns: u64, cancelled: u64, errors: u64, failed: bool,
    restore_time: Duration,
}

impl TextureEviction {
    fn release_backing(&mut self, backing: &TextureBacking) {
        if !self.backing_space.release(backing.offset, backing.size) {
            self.errors += 1;
            self.failed = true;
            log_line("texture eviction disabled: backing extent return failed; existing contents retained");
        }
    }

    fn new(shaders: Option<&Path>) -> Self {
        let mut state = Self { clock: Some(Instant::now()), ..Self::default() };
        if !debug_switch("evict_textures", true) { return state; }
        let file = (|| -> std::io::Result<std::fs::File> {
            let parent = shaders.and_then(Path::parent).ok_or_else(|| std::io::Error::other("no writable texture backing directory"))?;
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            let path = parent.join(format!(".texture-backing-{}-{nonce}", std::process::id()));
            let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
            // The open file lives only for this process, including an unexpected process exit.
            std::fs::remove_file(&path)?;
            Ok(file)
        })();
        match file {
            Ok(file) => { state.file = Some(Arc::new(file)); log_line("texture eviction enabled: 1800 submissions and 60 seconds idle, 512 KiB to 4 MiB contents, unbound textures only, lossless backing, returned textures pinned"); }
            Err(error) => { state.errors += 1; state.failed = true; log_line(&format!("texture eviction disabled: {error}")); }
        }
        state
    }
}

impl TextureResource {
    fn copy_regions(&self) -> (Vec<vk::BufferImageCopy>, u64) {
        let mut regions = Vec::new();
        let mut offset: u64 = 0;
        for layer in 0..self.layers() {
            for mip in 0..self.mip_levels {
                if self.layouts[(mip + self.mip_levels * layer) as usize] == vk::ImageLayout::UNDEFINED { continue; }
                let (w, h, d) = self.mip_extent(mip);
                offset = offset.next_multiple_of(u64::from(self.info.block_bytes).max(4));
                regions.push(vk::BufferImageCopy { buffer_offset: offset, buffer_row_length: 0, buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: mip, base_array_layer: layer, layer_count: 1 },
                    image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width: w, height: h, depth: d } });
                offset += u64::from(w.div_ceil(self.info.block_width)) * u64::from(h.div_ceil(self.info.block_height)) * u64::from(d) * u64::from(self.info.block_bytes);
            }
        }
        (regions, offset)
    }

    fn new_unbound_image(&self, device: &ash::Device) -> Result<vk::Image, RendererError> {
        let mut usage = vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
        if self.bind & BIND_RENDER_TARGET != 0 { usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT; }
        if self.bind & BIND_UAV != 0 { usage |= vk::ImageUsageFlags::STORAGE; }
        let mut flags = vk::ImageCreateFlags::empty();
        if !self.views.is_empty() { flags |= vk::ImageCreateFlags::MUTABLE_FORMAT; }
        if self.dimension == vk::ImageType::TYPE_2D && self.layers() >= 6 && self.layers() % 6 == 0 && self.width == self.height { flags |= vk::ImageCreateFlags::CUBE_COMPATIBLE; }
        let mut formats = vk::ImageFormatListCreateInfo::default().view_formats(self.views);
        let mut create = vk::ImageCreateInfo::default().image_type(self.dimension).format(self.format)
            .extent(vk::Extent3D { width: self.width, height: self.height, depth: if self.dimension == vk::ImageType::TYPE_3D { self.depth_or_layers } else { 1 } })
            .mip_levels(self.mip_levels).array_layers(self.layers()).samples(vk::SampleCountFlags::TYPE_1).tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage).flags(flags).sharing_mode(vk::SharingMode::EXCLUSIVE).initial_layout(vk::ImageLayout::UNDEFINED);
        if !self.views.is_empty() { create = create.push_next(&mut formats); }
        unsafe { device.create_image(&create, Some(callbacks())) }.map_err(vk_error("recreate evicted image"))
    }
    fn layers(&self) -> u32 { if self.dimension == vk::ImageType::TYPE_3D { 1 } else { self.depth_or_layers } }
    fn mip_extent(&self, mip: u32) -> (u32, u32, u32) {
        let depth = if self.dimension == vk::ImageType::TYPE_3D { (self.depth_or_layers >> mip).max(1) } else { 1 };
        ((self.width >> mip).max(1), (self.height >> mip).max(1), depth)
    }
    fn aspect(&self) -> vk::ImageAspectFlags {
        match (self.info.depth, self.info.stencil) {
            (true, true) => vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL,
            (true, false) => vk::ImageAspectFlags::DEPTH,
            _ => vk::ImageAspectFlags::COLOR,
        }
    }
    fn bytes(&self) -> u64 {
        (0..self.mip_levels).map(|mip| {
            let (w, h, d) = self.mip_extent(mip);
            u64::from(w.div_ceil(self.info.block_width)) * u64::from(h.div_ceil(self.info.block_height)) * u64::from(d) * u64::from(self.info.block_bytes)
        }).sum::<u64>() * u64::from(self.layers()) * u64::from(self.samples)
    }
}

struct Compiled {
    /// `effect::program (hash)`, for diagnostics.
    label: String,
    /// Prepared WGSL, kept only where it is rewritten later: vertex shaders (interpolation
    /// variants) and compute shaders (storage specialization). Pixel shaders drop theirs.
    source: Option<String>,
    reflection: Rc<ShaderReflection>,
    module: vk::ShaderModule,
    constants: Option<(vk::Buffer, Allocation)>,
    pack: Option<Rc<[PackMember]>>,
    pack_size: u64,
    /// Vertex modules rewritten to a pixel shader's interpolation qualifiers.
    variants: HashMap<Vec<(u32, String)>, vk::ShaderModule>,
}

struct ShaderResource {
    stage: u32,
    hash: String,
    signatures: Vec<[u32; 4]>,
    compiled: Option<Box<Compiled>>,
}

#[derive(Clone)]
struct InputElement { semantic_hash: u32, semantic_index: u32, dxgi_format: u32, slot: u32, offset: u32, slot_class: u32, step_rate: u32 }

struct StateResource { kind: u32, words: Vec<u32>, sampler: Option<vk::Sampler> }

#[derive(Clone, Copy)]
struct ViewResource { resource: u32, is_buffer: bool, format: u32, view_dimension: u32, a: u32, b: u32, c: u32, d: u32 }

struct QueryResource { query_type: u32, address: u64 }

struct QuerySlots {
    pools: Vec<vk::QueryPool>,
    available: Vec<u32>,
    next: u32,
    count: u32,
    kind: vk::QueryType,
}

impl QuerySlots {
    fn new(pool: vk::QueryPool, count: u32, kind: vk::QueryType) -> Self {
        Self { pools: vec![pool], available: Vec::new(), next: 0, count, kind }
    }

    fn reserve(&mut self, device: &ash::Device) -> Result<u32, RendererError> {
        if let Some(slot) = self.available.pop() { return Ok(slot); }
        if self.next / self.count >= self.pools.len() as u32 {
            if self.pools.len() >= 16 { return Err(RendererError::Vulkan("too many unpublished GPU queries".into())); }
            let create = vk::QueryPoolCreateInfo::default().query_type(self.kind).query_count(self.count);
            let pool = unsafe { device.create_query_pool(&create, Some(callbacks())) }.map_err(vk_error("grow query pool"))?;
            unsafe { device.reset_query_pool(pool, 0, self.count) };
            self.pools.push(pool);
        }
        let slot = self.next;
        self.next += 1;
        Ok(slot)
    }

    fn location(&self, slot: u32) -> (vk::QueryPool, u32) {
        (self.pools[(slot / self.count) as usize], slot % self.count)
    }

    fn release(&mut self, device: &ash::Device, slot: u32) {
        let (pool, index) = self.location(slot);
        unsafe { device.reset_query_pool(pool, index, 1) };
        self.available.push(slot);
    }
}

/// Buffers and textures are boxed: the resource map holds ~35k entries and its table is sized for its peak,
/// so every slot would otherwise be as large as the largest variant (37 MiB measured, diag-1 heap profile).
enum Resource {
    Buffer(Box<BufferResource>),
    Texture(Box<TextureResource>),
    Shader(ShaderResource),
    Layout(Vec<InputElement>),
    State(StateResource),
    View(ViewResource),
    Query(QueryResource),
}

#[derive(Clone, Copy, Debug, Default)]
struct VertexBinding { buffer: u32, stride: u32, offset: u32 }
#[derive(Clone, Copy, Debug, Default)]
struct IndexBinding { buffer: u32, format: u32, offset: u32 }
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
struct RenderTarget { texture: u32, mip: u32, slice: u32, format: u32, flags: u32 }
#[derive(Clone, Copy, Debug, Default)]
struct ConstantBinding { buffer: u32, first: u32, num: u32 }

struct PipelineState {
    shaders: [u32; 3],
    layout: u32,
    topology: u32,
    vertex: [VertexBinding; 16],
    index: IndexBinding,
    render_targets: [RenderTarget; 8],
    depth_stencil: RenderTarget,
    viewports: Vec<[f32; 6]>,
    scissors: Vec<[u32; 4]>,
    constants: [[ConstantBinding; 16]; 3],
    srvs: [[u32; 128]; 3],
    samplers: [[u32; 16]; 3],
    uavs: [[u32; 8]; 3],
    blend: u32,
    blend_factor: [f32; 4],
    depth_stencil_state: u32,
    stencil_reference: u32,
    raster_state: u32,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self { shaders: [0; 3], layout: 0, topology: 4, vertex: [VertexBinding::default(); 16], index: IndexBinding::default(),
            render_targets: [RenderTarget::default(); 8], depth_stencil: RenderTarget::default(), viewports: Vec::new(), scissors: Vec::new(),
            constants: [[ConstantBinding::default(); 16]; 3], srvs: [[0; 128]; 3], samplers: [[0; 16]; 3], uavs: [[0; 8]; 3],
            blend: 0, blend_factor: [1.0; 4], depth_stencil_state: 0, stencil_reference: 0, raster_state: 0 }
    }
}

/// Everything a draw pipeline depends on; ids name immutable shader, layout and state objects.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PipelineKey {
    vs: u32,
    ps: u32,
    layout: u32,
    strides: [u32; 16],
    topology: u32,
    blend: u32,
    depth_stencil: u32,
    raster: u32,
    /// Attachment formats; slots at and past `color_count` are UNDEFINED.
    colors: [vk::Format; 8],
    color_count: u8,
    depth: vk::Format,
    samples: u32,
    depth_flags: u32,
}

impl PipelineKey {
    fn uses(&self, id: u32) -> bool { [self.vs, self.ps, self.layout, self.blend, self.depth_stencil, self.raster].contains(&id) }
}

struct DrawPipeline {
    id: u64,
    raw: vk::Pipeline,
    layout: vk::PipelineLayout,
    sets: [vk::DescriptorSetLayout; 2],
    /// Set 2: each packed stage's constants as a dynamic uniform buffer.
    pack_set: vk::DescriptorSetLayout,
    used: Cell<u32>,
}

struct StageCache {
    shader: u32,
    epoch: (u64, u64),
    layout: vk::DescriptorSetLayout,
    attachments: Vec<(u32, u32, u32, vk::ImageLayout)>,
    set: vk::DescriptorSet,
    uses: Vec<ImageUse>,
    buffers: Vec<u32>,
    /// Storage textures the stage writes (marked written on every draw).
    writes: Vec<u32>,
}

/// A draw's attachments, image uses and read buffers, kept between draws so a draw allocates nothing for them.
#[derive(Default)]
struct DrawScratch {
    attachments: Vec<(u32, u32, u32, vk::ImageLayout)>,
    uses: Vec<ImageUse>,
    read_buffers: Vec<u32>,
    attachment_uses: Vec<ImageUse>,
}

struct VertexInput {
    attributes: Vec<vk::VertexInputAttributeDescription>,
    steps: [Option<crate::vertex_step::InputStep>; 16],
}

struct ComputePipeline {
    raw: vk::Pipeline,
    layout: vk::PipelineLayout,
    set: vk::DescriptorSetLayout,
    pack_set: vk::DescriptorSetLayout,
    /// The shader with storage formats specialized to the bound textures, if they differed.
    specialized: Option<(Rc<ShaderReflection>, vk::ShaderModule)>,
}

/// Image view parameters; D3D views are immutable so one native view serves every use.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ViewKey { format: vk::Format, kind: vk::ImageViewType, aspect: vk::ImageAspectFlags, base_mip: u32, mips: u32, base_layer: u32, layers: u32, swizzle: u32 }

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Descriptor {
    Buffer { buffer: vk::Buffer, offset: u64, range: u64 },
    Image { view: vk::ImageView, layout: vk::ImageLayout },
    Sampler(vk::Sampler),
}

/// A subresource range a command needs in `layout`.
#[derive(Clone, Copy, Debug)]
struct ImageUse { texture: u32, mips: (u32, u32), layers: (u32, u32), layout: vk::ImageLayout, stages: vk::PipelineStageFlags2 }

/// A texture rewritten on the GPU into a proxy another binding kind can read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProxyCopy {
    /// Red channel of a colour texture into depth, for comparison reads (D3D compares any float texture).
    FloatToDepth,
    /// Depth plane of a depth texture into R32F, for float reads.
    DepthToFloat,
}

const PROXY_VERTEX: &str = "@vertex fn main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    var corners = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    return vec4f(corners[i], 0.0, 1.0);
}";

impl ProxyCopy {
    fn fragment(self) -> &'static str {
        match self {
            Self::FloatToDepth => "@group(1) @binding(0) var source: texture_2d<f32>;
@fragment fn main(@builtin(position) position: vec4f) -> @builtin(frag_depth) f32 { return textureLoad(source, vec2i(position.xy), 0).x; }",
            Self::DepthToFloat => "@group(1) @binding(0) var source: texture_depth_2d;
@fragment fn main(@builtin(position) position: vec4f) -> @location(0) vec4f { return vec4f(textureLoad(source, vec2i(position.xy), 0), 0.0, 0.0, 1.0); }",
        }
    }
    fn proxy_dxgi(self) -> u32 { match self { Self::FloatToDepth => 40, Self::DepthToFloat => 41 } }
    fn proxy_bind(self) -> u32 { match self { Self::FloatToDepth => BIND_SRV | BIND_DEPTH_STENCIL, Self::DepthToFloat => BIND_SRV | BIND_RENDER_TARGET } }
}

#[derive(Clone, Copy, Default)]
struct PendingClear { color: Option<[f32; 4]>, depth: Option<f32>, stencil: Option<u32> }

/// Attachments of an open rendering instance.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PassKey { colors: [Option<(u32, u32, u32, vk::Format)>; 8], color_count: usize, depth: Option<(u32, u32, u32, u32)>, extent: (u32, u32) }

#[derive(Default)]
struct Bound {
    pipeline: Option<u64>,
    sets: [Option<vk::DescriptorSet>; 2],
    packs: [Option<(vk::Buffer, u64, u64)>; 2],
    vertex: [Option<(vk::Buffer, u64)>; 16],
    index: Option<(vk::Buffer, u64, vk::IndexType)>,
    viewport: Option<[f32; 6]>,
    scissor: Option<[u32; 4]>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
}

struct Recording {
    cb: vk::CommandBuffer,
    /// Uploads to resources this recording has not touched run here, before `cb`, in the same submit.
    pre: Option<vk::CommandBuffer>,
    /// Staged buffer writes that run in `pre`, merged when contiguous: (staging, destination, region).
    pre_copies: Vec<(vk::Buffer, vk::Buffer, vk::BufferCopy)>,
    pass: Option<PassKey>,
    bound: Bound,
    open_query: Option<u32>,
    /// Buffer/storage writes not yet made visible to later reads.
    pending: (vk::PipelineStageFlags2, vk::AccessFlags2),
    after_submit: Vec<Completion>,
    /// The open pass's index in the slot's PassProfile (its end timestamp is query 2 * index + 1).
    profiled: Option<u32>,
}

/// GPU results the engine waits for, published in order once their submission finished.
enum Completion {
    /// `result` (if any) goes to +8, then `word` to `address`, then waiters wake.
    Word { address: usize, word: u32, result: Option<u64> },
    Readback { buffer: vk::Buffer, memory: Allocation, copy: ReadbackCopy },
    /// Occlusion: visible if any slot (one per rendering instance the query spanned) passed samples.
    Query { slots: Vec<u32>, timestamp: bool, address: usize, seq: u32 },
}

struct Pending { value: u64, completion: Completion, queued: Instant }

/// One packed constant-buffer member: (binding, offset, size, buffer, generation, first register, register count).
type PackKey = (u32, u64, u64, u32, u64, u32, u32);

/// Where a readback's mapped bytes go in engine memory.
struct ReadbackCopy {
    address: usize,
    requested: usize,
    is_buffer: bool,
    row_bytes: usize,
    rows: usize,
    slices: usize,
    output_row_pitch: usize,
    output_slice_pitch: usize,
    flag: Option<usize>,
}

impl ReadbackCopy {
    fn validate(&self, memory_len: usize) -> Result<(), RendererError> {
        let error = |detail: &str| RendererError::Command { opcode: Opcode::Readback, detail: detail.into() };
        if self.is_buffer { return Ok(()); }
        if self.output_row_pitch < self.row_bytes || self.output_slice_pitch < self.output_row_pitch.checked_mul(self.rows).ok_or_else(|| error("slice pitch overflows"))? {
            return Err(error("texture readback row or slice pitch is smaller than the copied data"));
        }
        let total = self.output_slice_pitch.checked_mul(self.slices).ok_or_else(|| error("output range overflows"))?;
        if self.requested != 0 && total > self.requested { return Err(error("texture readback output exceeds the requested byte count")); }
        let end = self.address.checked_add(total).ok_or_else(|| error("readback address overflows"))?;
        if end > memory_len { return Err(RendererError::MemoryRange { address: self.address, end, length: memory_len }); }
        Ok(())
    }

    fn publish<M: SharedMemory + ?Sized>(&self, mapped: &[u8], memory: &M) -> Result<(), RendererError> {
        if self.is_buffer {
            memory.write(self.address, &mapped[..self.requested.min(mapped.len())])?;
        } else {
            for slice in 0..self.slices {
                for row in 0..self.rows {
                    let input = (slice * self.rows + row) * self.row_bytes;
                    memory.write(self.address + slice * self.output_slice_pitch + row * self.output_row_pitch, &mapped[input..input + self.row_bytes])?;
                }
            }
        }
        if let Some(flag) = self.flag { memory.write(flag, &0u32.to_le_bytes())?; }
        Ok(())
    }
}

/// Host-visible chunks handed out linearly; a chunk is reused once its last submission finished.
struct Ring {
    usage: vk::BufferUsageFlags,
    chunk_size: u64,
    chunks: Vec<Chunk>,
    current: Option<usize>,
}

struct Chunk { buffer: vk::Buffer, memory: Allocation, size: u64, cursor: u64, value: u64 }

impl Ring {
    fn new(usage: vk::BufferUsageFlags, chunk_size: u64) -> Self { Self { usage, chunk_size, chunks: Vec::new(), current: None } }

    /// `size` bytes at `align`, tagged with the recording submission `value`.
    fn alloc(&mut self, gpu: &mut Gpu, size: u64, align: u64, value: u64) -> Result<(vk::Buffer, u64, *mut u8), RendererError> {
        let fits = |chunk: &Chunk| chunk.cursor.next_multiple_of(align) + size <= chunk.size;
        if !self.current.is_some_and(|index| fits(&self.chunks[index])) {
            let completed = gpu.completed;
            let current = self.current;
            let reusable = (0..self.chunks.len()).find(|index| Some(*index) != current && self.chunks[*index].value <= completed && self.chunks[*index].size >= size);
            let index = match reusable {
                Some(index) => { self.chunks[index].cursor = 0; index }
                None => {
                    let chunk_size = size.max(self.chunk_size);
                    let (buffer, memory) = gpu.create_buffer("gta5 ring chunk", chunk_size, self.usage, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?;
                    self.chunks.push(Chunk { buffer, memory, size: chunk_size, cursor: 0, value });
                    self.chunks.len() - 1
                }
            };
            self.current = Some(index);
        }
        let chunk = &mut self.chunks[self.current.expect("current chunk")];
        let offset = chunk.cursor.next_multiple_of(align);
        chunk.cursor = offset + size;
        chunk.value = value;
        let base = chunk.memory.mapped_ptr().ok_or_else(|| RendererError::Vulkan("ring chunk is not host mapped".into()))?.as_ptr() as *mut u8;
        Ok((chunk.buffer, offset, unsafe { base.add(offset as usize) }))
    }

    /// Keeps the chunk holding `buffer` alive for submission `value` (a reused allocation).
    fn touch(&mut self, buffer: vk::Buffer, value: u64) {
        if let Some(chunk) = self.chunks.iter_mut().find(|chunk| chunk.buffer == buffer) { chunk.value = chunk.value.max(value); }
    }

    /// Bytes in chunks the GPU may still read.
    fn in_flight(&self, completed: u64) -> u64 { self.chunks.iter().filter(|chunk| chunk.value > completed).map(|chunk| chunk.size).sum() }

    /// Frees idle chunks beyond the first two regular ones (big uploads get one-off chunks).
    fn trim(&mut self, gpu: &mut Gpu, garbage: &mut Garbage) {
        let completed = gpu.completed;
        let mut kept = 0;
        let mut index = 0;
        while index < self.chunks.len() {
            let chunk = &self.chunks[index];
            let idle = chunk.value <= completed && Some(index) != self.current;
            if idle && (chunk.size > self.chunk_size || kept >= 2) {
                let chunk = self.chunks.swap_remove(index);
                if self.current == Some(self.chunks.len()) { self.current = Some(index); }
                garbage.buffer(0, chunk.buffer, chunk.memory);
                continue;
            }
            if chunk.size == self.chunk_size { kept += 1; }
            index += 1;
        }
    }
}

/// Vulkan objects released once the submission that last used them finished.
#[derive(Default)]
struct Garbage {
    items: VecDeque<(u64, Trash)>,
    /// `debug.gta5.buffer_pool` (A/B switch): retired engine buffers wait here, by usage and size, for the next
    /// CreateBuffer of the same shape instead of being destroyed; a new buffer then skips vkCreateBuffer and the
    /// allocator's free-list search. Bounded by BUFFER_POOL_BYTES.
    pooling: bool,
    pool: FastMap<(u32, u64), Vec<(vk::Buffer, Allocation)>>,
    pool_bytes: u64,
}

const BUFFER_POOL_BYTES: u64 = 16 << 20;

enum Trash {
    Buffer(vk::Buffer, Allocation),
    Reusable(vk::BufferUsageFlags, u64, vk::Buffer, Allocation),
    Image(vk::Image, Allocation),
    View(vk::ImageView),
    Pipeline(vk::Pipeline),
    Module(vk::ShaderModule),
    Sampler(vk::Sampler),
}

impl Garbage {
    fn buffer(&mut self, value: u64, buffer: vk::Buffer, memory: Allocation) { self.items.push_back((value, Trash::Buffer(buffer, memory))); }
    fn push(&mut self, value: u64, trash: Trash) { self.items.push_back((value, trash)); }

    fn reusable(&mut self, value: u64, usage: vk::BufferUsageFlags, size: u64, buffer: vk::Buffer, memory: Allocation) {
        let trash = if self.pooling { Trash::Reusable(usage, size, buffer, memory) } else { Trash::Buffer(buffer, memory) };
        self.items.push_back((value, trash));
    }

    fn take(&mut self, usage: vk::BufferUsageFlags, size: u64) -> Option<(vk::Buffer, Allocation)> {
        let reused = self.pool.get_mut(&(usage.as_raw(), size))?.pop()?;
        self.pool_bytes -= size;
        Some(reused)
    }

    fn collect(&mut self, gpu: &mut Gpu, all: bool) {
        let completed = gpu.completed;
        let mut keep = VecDeque::new();
        while let Some((value, trash)) = self.items.pop_front() {
            if !all && value > completed { keep.push_back((value, trash)); continue; }
            let device = &gpu.device;
            unsafe {
                match trash {
                    Trash::Buffer(buffer, memory) => { device.destroy_buffer(buffer, Some(callbacks())); gpu.free(memory); }
                    Trash::Reusable(usage, size, buffer, memory) if !all && self.pool_bytes + size <= BUFFER_POOL_BYTES => {
                        self.pool.entry((usage.as_raw(), size)).or_default().push((buffer, memory));
                        self.pool_bytes += size;
                    }
                    Trash::Reusable(_, _, buffer, memory) => { device.destroy_buffer(buffer, Some(callbacks())); gpu.free(memory); }
                    Trash::Image(image, memory) => { device.destroy_image(image, Some(callbacks())); gpu.free(memory); }
                    Trash::View(view) => device.destroy_image_view(view, Some(callbacks())),
                    Trash::Pipeline(pipeline) => device.destroy_pipeline(pipeline, Some(callbacks())),
                    Trash::Module(module) => device.destroy_shader_module(module, Some(callbacks())),
                    Trash::Sampler(sampler) => device.destroy_sampler(sampler, Some(callbacks())),
                }
            }
        }
        self.items = keep;
        if all {
            for (buffer, memory) in self.pool.drain().flat_map(|(_, buffers)| buffers) {
                unsafe { gpu.device.destroy_buffer(buffer, Some(callbacks())) };
                gpu.free(memory);
            }
            self.pool_bytes = 0;
        }
    }
}

/// `debug.gta5.pass_profile` (diagnostic): GPU timestamps around each render pass of one frame slot, keyed by pass shape.
struct PassProfile { pool: vk::QueryPool, keys: Vec<String> }

/// Command and descriptor memory of the submissions between two presents.
struct Slot {
    pool: vk::CommandPool,
    buffers: Vec<vk::CommandBuffer>,
    used: usize,
    descriptor_pools: Vec<vk::DescriptorPool>,
    descriptor_pool: usize,
    /// Sets written this slot, by layout and contents: identical bindings reuse one.
    sets: FastMap<(vk::DescriptorSetLayout, Vec<(u32, vk::DescriptorType, Descriptor)>), vk::DescriptorSet>,
    /// Constant-pack sets by layout and each stage's (ring buffer, range): looked up on most draws without allocating.
    pack_sets: FastMap<(vk::DescriptorSetLayout, [Option<(vk::Buffer, u64)>; 2]), vk::DescriptorSet>,
    submits: u32,
    last: u64,
    profile: Option<PassProfile>,
}

/// The browser worker's 5 s "fps:" line: frame rate and where the worker's time went.
#[derive(Default)]
struct FrameStats {
    since: Option<Instant>,
    frames: u32,
    draws: u32,
    busy: Duration,
    pipelines_created: u32,
    pipeline_time: Duration,
    submits: u32,
    submit: Duration,
    flush_reasons: std::collections::BTreeMap<u32, u32>,
    frame_pipelines: FastSet<u64>,
    pipelines_per_frame: usize,
    last_present: Option<Instant>,
    frame_times: Vec<u32>,
    passes: u32,
    /// Why draws started a new pass: [different targets, a pending barrier, a layout transition].
    pass_breaks: [u32; 3],
    barriers: u32,
    sets_written: u32,
    packs_written: u32,
    zero_fills: u32,
    proxy_copies: u32,
    /// Buffer writes staged behind GPU work instead of written in place.
    staged_writes: u32,
    /// Worker time blocked on the GPU (slot reuse, swapchain acquire, blocking readbacks).
    gpu_wait: Duration,
    /// Worker time held back by the frame cap (not counted as busy).
    paced: Duration,
    /// Engine CreateBuffer commands: count, bytes and worker time (Vulkan buffer plus GPU allocator search).
    buffer_creates: u32,
    buffer_create_bytes: u64,
    buffer_create_time: Duration,
}

impl FrameStats {
    fn present(&mut self, now: Instant) -> Option<String> {
        self.frames += 1;
        if let Some(last) = self.last_present.replace(now) { self.frame_times.push((now.duration_since(last).as_micros() / 100) as u32); }
        self.pipelines_per_frame = self.pipelines_per_frame.max(self.frame_pipelines.len());
        self.frame_pipelines.clear();
        let since = *self.since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if elapsed < Duration::from_secs(5) { return None; }
        let frames = self.frames.max(1);
        let per_frame = |time: Duration| time.as_secs_f64() * 1000.0 / f64::from(frames);
        let mut reasons: Vec<(u32, u32)> = self.flush_reasons.iter().map(|(op, n)| (*op, *n)).collect();
        reasons.sort_by(|a, b| b.1.cmp(&a.1));
        let reasons: Vec<String> = reasons.iter().take(6).map(|(op, n)| format!("{} {:.1}", Opcode::from_u32(*op).map_or_else(|| op.to_string(), |op| format!("{op:?}")), f64::from(*n) / f64::from(frames))).collect();
        let times: Vec<String> = self.frame_times.iter().map(u32::to_string).collect();
        let line = format!(
            "fps: {:.1} ({} frames, {} draws/frame), worker busy {:.0}% ({:.1} ms/frame), pipelines +{}, {:.1} submits/frame ({:.1} ms/frame) by cause: {}; at most {} pipelines in one frame, new pipelines {:.1} ms each; per frame: {:.1} ms GPU wait, {:.1} ms paced, {:.0} passes (draw breaks: {:.0} targets, {:.0} barrier, {:.0} transition), {:.0} barriers, {:.0} sets, {:.0} packs, {:.1} depth copies; {} zero-filled subresources, {} staged buffer writes; frametimes_0.1ms {}; buffer creates {:.1}/frame, {:.1} KiB avg, {:.0} us each",
            f64::from(self.frames) / elapsed.as_secs_f64(), self.frames, self.draws / frames,
            self.busy.as_secs_f64() * 100.0 / elapsed.as_secs_f64(), per_frame(self.busy), self.pipelines_created,
            f64::from(self.submits) / f64::from(frames), per_frame(self.submit), reasons.join(", "), self.pipelines_per_frame,
            if self.pipelines_created > 0 { self.pipeline_time.as_secs_f64() * 1000.0 / f64::from(self.pipelines_created) } else { 0.0 },
            per_frame(self.gpu_wait), per_frame(self.paced), f64::from(self.passes) / f64::from(frames),
            f64::from(self.pass_breaks[0]) / f64::from(frames), f64::from(self.pass_breaks[1]) / f64::from(frames), f64::from(self.pass_breaks[2]) / f64::from(frames),
            f64::from(self.barriers) / f64::from(frames), f64::from(self.sets_written) / f64::from(frames),
            f64::from(self.packs_written) / f64::from(frames), f64::from(self.proxy_copies) / f64::from(frames), self.zero_fills, self.staged_writes, times.join(","),
            f64::from(self.buffer_creates) / f64::from(frames), self.buffer_create_bytes as f64 / 1024.0 / f64::from(self.buffer_creates.max(1)),
            self.buffer_create_time.as_secs_f64() * 1e6 / f64::from(self.buffer_creates.max(1)));
        *self = Self { since: Some(now), last_present: Some(now), ..Self::default() };
        Some(line)
    }
}

/// The browser worker's title-screen signal: a real scene starts with the first
/// present after more than 300 draws, and the world shows 1.5 s later.
#[derive(Debug, Default)]
struct WorldShown { draws_at_present: u32, since: Option<Instant> }

impl WorldShown {
    fn present(&mut self, draws: u32, now: Instant) -> bool {
        let frame = draws.wrapping_sub(self.draws_at_present);
        self.draws_at_present = draws;
        if self.since.is_none() && frame <= 300 { return false; }
        now.duration_since(*self.since.get_or_insert(now)) >= Duration::from_millis(1500)
    }
}

#[cfg(target_os = "android")]
#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(priority: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
}

/// Session logging (renderer reports, engine and host lines) is off in normal play; test runs turn it on with
/// `setprop debug.gta5.log 1`. Errors that stop the game still reach the screen and the Android log.
pub fn logging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| debug_switch("log", false))
}

/// The session log (Android log tag GtaDirectGpu), written only while `logging()` is on.
pub(crate) fn log_line(text: &str) {
    if !logging() { return; }
    #[cfg(target_os = "android")]
    if let Ok(text) = std::ffi::CString::new(text.replace('\0', " ")) {
        unsafe { __android_log_write(4, c"GtaDirectGpu".as_ptr(), text.as_ptr()); }
    }
    #[cfg(not(target_os = "android"))]
    eprintln!("{text}");
}

/// Probe, not safe to ship (`setprop debug.gta5.frag_read_barrier 1`): barriers into read layouts stop waiting for the
/// vertex stage, so a later pass's vertex work may overlap the earlier pass. Measures what the wide stage mask costs.
static FRAG_READ_BARRIER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
/// `setprop debug.gta5.exact_barriers 1` (A/B switch): a barrier into a sampled-read layout waits only for the
/// shader stages that read it (vertex only when a vertex shader does), so a later pass's vertex work can overlap
/// the earlier pass. A vertex read of an image made readable without the vertex stage first gets a catch-up barrier.
static EXACT_BARRIERS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// An Android system property switch (`adb shell setprop debug.gta5.<name> 0|1`), read at startup.
pub(crate) fn debug_switch(name: &str, default: bool) -> bool {
    #[cfg(target_os = "android")]
    {
        unsafe extern "C" { fn __system_property_get(name: *const std::ffi::c_char, value: *mut std::ffi::c_char) -> i32; }
        let Ok(key) = std::ffi::CString::new(format!("debug.gta5.{name}")) else { return default };
        let mut value = [0 as std::ffi::c_char; 92];
        if unsafe { __system_property_get(key.as_ptr(), value.as_mut_ptr()) } > 0 { return value[0] as u8 == b'1'; }
    }
    let _ = name;
    default
}

/// A numeric Android system property (`adb shell setprop debug.gta5.<name> N`), read at startup.
/// Frames per second the renderer presents at most (`debug.gta5.fps_cap`, 0 = uncapped). 60 by default, the
/// user's choice on 2026-10-15: it stops menus and loading screens running at 90-260 FPS (an uncapped menu took
/// the phone from 41 to 48.5 C) while leaving gameplay frames free; slower frames are never held back.
const DEFAULT_FPS_CAP: u32 = 60;

fn frame_interval() -> Option<Duration> {
    Some(debug_number("fps_cap").unwrap_or(DEFAULT_FPS_CAP)).filter(|fps| *fps > 0).map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps)))
}

fn debug_number(name: &str) -> Option<u32> {
    debug_string(name)?.parse().ok()
}

fn debug_string(name: &str) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        unsafe extern "C" { fn __system_property_get(name: *const std::ffi::c_char, value: *mut std::ffi::c_char) -> i32; }
        let key = std::ffi::CString::new(format!("debug.gta5.{name}")).ok()?;
        let mut value = [0u8; 92];
        let length = unsafe { __system_property_get(key.as_ptr(), value.as_mut_ptr().cast()) };
        if length > 0 { return Some(std::str::from_utf8(&value[..length as usize]).ok()?.trim().to_owned()); }
    }
    let _ = name;
    None
}

/// Bytes in use by the C allocator (bionic mallinfo).
fn malloc_in_use() -> u64 {
    #[cfg(target_os = "android")]
    {
        #[repr(C)]
        struct MallInfo { arena: usize, ordblks: usize, smblks: usize, hblks: usize, hblkhd: usize, usmblks: usize, fsmblks: usize, uordblks: usize, fordblks: usize, keepcost: usize }
        unsafe extern "C" { fn mallinfo() -> MallInfo; }
        return unsafe { mallinfo() }.uordblks as u64;
    }
    #[cfg(not(target_os = "android"))]
    0
}

fn heap_purge_due(enabled: bool, elapsed: Duration, pipelines: usize) -> bool {
    enabled && elapsed >= Duration::from_secs(60) && pipelines <= 1
}

fn process_rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status").ok()?.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse().ok()
    })
}

fn purge_unused_heap() -> i32 {
    #[cfg(target_os = "android")]
    {
        unsafe extern "C" { fn mallopt(parameter: i32, value: i32) -> i32; }
        // Android M_PURGE returns freed allocator pages; live game allocations stay intact.
        return unsafe { mallopt(-101, 0) };
    }
    #[cfg(not(target_os = "android"))]
    0
}

fn kgsl_bytes() -> u64 {
    std::fs::read_to_string("/sys/class/kgsl/kgsl/page_alloc").ok().and_then(|text| text.trim().parse().ok()).unwrap_or(0)
}

fn words_as_bytes(words: &[u32], len: usize) -> &[u8] {
    // SAFETY: a u32 slice viewed as its little-endian bytes (the ring protocol is little-endian).
    let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 4) };
    &bytes[..len.min(bytes.len())]
}

/// The bytes of a D3D constant buffer that a `size`-byte binding at register
/// `first` (and `num` registers, 0 or u32::MAX for all) reads; the rest are zeros.
fn constant_range(len: usize, first: u32, num: u32, size: usize) -> std::ops::Range<usize> {
    let start = (first as usize).saturating_mul(16).min(len);
    let mut end = len.min(start.saturating_add(size));
    if num != 0 && num != u32::MAX { end = end.min(start.saturating_add((num as usize).saturating_mul(16))); }
    start..end
}

fn is_integer(format: vk::Format) -> bool {
    use vk::Format as F;
    matches!(format, F::R8_UINT | F::R8_SINT | F::R8G8_UINT | F::R8G8_SINT | F::R8G8B8A8_UINT | F::R8G8B8A8_SINT | F::R16_UINT | F::R16_SINT
        | F::R16G16_UINT | F::R16G16_SINT | F::R16G16B16A16_UINT | F::R16G16B16A16_SINT | F::R32_UINT | F::R32_SINT | F::R32G32_UINT | F::R32G32_SINT
        | F::R32G32B32A32_UINT | F::R32G32B32A32_SINT | F::A2B10G10R10_UINT_PACK32)
}

fn is_srgb(format: vk::Format) -> bool {
    matches!(format, vk::Format::R8G8B8A8_SRGB | vk::Format::B8G8R8A8_SRGB | vk::Format::BC1_RGBA_SRGB_BLOCK | vk::Format::BC2_SRGB_BLOCK | vk::Format::BC3_SRGB_BLOCK | vk::Format::BC7_SRGB_BLOCK)
}

fn storage_format(format: StorageFormat) -> vk::Format {
    match format {
        StorageFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM, StorageFormat::Rgba8Snorm => vk::Format::R8G8B8A8_SNORM,
        StorageFormat::Rgba8Uint => vk::Format::R8G8B8A8_UINT, StorageFormat::Rgba8Sint => vk::Format::R8G8B8A8_SINT,
        StorageFormat::R32Float => vk::Format::R32_SFLOAT, StorageFormat::R32Uint => vk::Format::R32_UINT, StorageFormat::R32Sint => vk::Format::R32_SINT,
        StorageFormat::Rg32Float => vk::Format::R32G32_SFLOAT, StorageFormat::Rgba16Float => vk::Format::R16G16B16A16_SFLOAT,
        StorageFormat::Rgba32Float => vk::Format::R32G32B32A32_SFLOAT,
    }
}

fn storage_format_from_native(format: vk::Format) -> Option<StorageFormat> {
    use StorageFormat::*;
    [Rgba8Unorm, Rgba8Snorm, Rgba8Uint, Rgba8Sint, R32Float, R32Uint, R32Sint, Rg32Float, Rgba16Float, Rgba32Float].into_iter().find(|value| storage_format(*value) == format)
}

fn view_type(dimension: TextureDimension) -> vk::ImageViewType {
    match dimension {
        TextureDimension::D1 => vk::ImageViewType::TYPE_1D, TextureDimension::D2 => vk::ImageViewType::TYPE_2D,
        TextureDimension::D2Array => vk::ImageViewType::TYPE_2D_ARRAY, TextureDimension::D3 => vk::ImageViewType::TYPE_3D,
        TextureDimension::Cube => vk::ImageViewType::CUBE, TextureDimension::CubeArray => vk::ImageViewType::CUBE_ARRAY,
    }
}

fn descriptor_type(kind: BindingKind) -> vk::DescriptorType {
    match kind {
        BindingKind::Uniform => vk::DescriptorType::UNIFORM_BUFFER,
        BindingKind::Storage { .. } => vk::DescriptorType::STORAGE_BUFFER,
        BindingKind::Sampler { .. } => vk::DescriptorType::SAMPLER,
        BindingKind::Texture { .. } => vk::DescriptorType::SAMPLED_IMAGE,
        BindingKind::StorageTexture { .. } => vk::DescriptorType::STORAGE_IMAGE,
    }
}

fn is_pack(compiled: &Compiled, binding: &BindingReflection) -> bool {
    compiled.pack.is_some() && binding.kind == BindingKind::Uniform && binding.binding == crate::shader::PACK_BINDING
}

/// The descriptor layout a depth attachment with D3D read-only `flags` needs.
fn depth_attachment_layout(flags: u32, stencil: bool) -> vk::ImageLayout {
    match (flags & 3, stencil) {
        (0, _) => vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
        (1, true) => vk::ImageLayout::DEPTH_READ_ONLY_STENCIL_ATTACHMENT_OPTIMAL,
        (2, true) => vk::ImageLayout::DEPTH_ATTACHMENT_STENCIL_READ_ONLY_OPTIMAL,
        (2, false) => vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
        _ => vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
    }
}

/// Native command consumer. One thread owns every Vulkan object.
struct MovieTexture { hash: u32, id: u32, width: u32, height: u32, serial: u64 }

pub struct NativeRenderer {
    gpu: Gpu,
    resources: FastMap<u32, Resource>,
    state: PipelineState,
    movie_source: Option<MovieFrameSource>,
    movie_scope: MovieScope,
    movie_pixels: Vec<u8>,
    movie_upload: Vec<u32>,
    movie_textures: Vec<MovieTexture>, // least recently used first; at most eight
    shader_store: Option<ShaderSourceStore>,
    shader_cache: crate::shader_cache::ShaderCache,
    pipeline_cache_path: Option<PathBuf>,
    pipeline_cache: vk::PipelineCache,
    pipeline_cache_dirty: u32,
    /// When the pipeline cache was last written: it is several MB, so at most once a minute during play.
    pipeline_cache_saved: Instant,
    purge_heap: bool,
    heap_purged_at: Instant,
    layouts: LayoutCache,
    empty_set: vk::DescriptorSetLayout,
    surface: Option<NativeSurface>,
    report: ProcessReport,
    world: WorldShown,
    stats: FrameStats,
    pipelines: FastMap<PipelineKey, Rc<DrawPipeline>>,
    next_pipeline_id: u64,
    compute_pipelines: FastMap<(u32, Vec<(u32, vk::Format)>), Rc<ComputePipeline>>,
    views: FastMap<(u32, ViewKey), vk::ImageView>,
    /// Comparison reads of float textures through a depth copy: (proxy texture id, source generation).
    depth_proxies: FastMap<u32, (u32, u64)>,
    /// Float reads of 32-bit depth through an R32F copy, as the wgpu path did: (proxy id, source generation).
    float_proxies: FastMap<u32, (u32, u64)>,
    float_proxy_enabled: bool,
    /// Pipeline, layout and source set layout of each `ProxyCopy` pass.
    proxy_pipelines: [Option<(vk::Pipeline, vk::PipelineLayout, vk::DescriptorSetLayout)>; 2],
    /// `debug.gta5.stencil_green`: stencil views read stencil in green, as D3D does (the wgpu path had it in red).
    stencil_green: bool,
    /// `debug.gta5.zero_images`: unbound image slots read a real zero image instead of a null descriptor.
    zero_images: bool,
    zero_images_by_kind: FastMap<(u32, u32, bool), u32>,
    /// Zero images a descriptor resolved this draw; they join the draw's image uses.
    pending_zero_uses: Vec<ImageUse>,
    next_internal_id: u32,
    pending_clears: FastMap<(u32, u32, u32), PendingClear>,
    recording: Option<Recording>,
    slots: Vec<Slot>,
    slot: usize,
    uniforms: Ring,
    staging: Ring,
    garbage: Garbage,
    completions: VecDeque<Pending>,
    occlusion: QuerySlots,
    timestamps: QuerySlots,
    /// The engine's active occlusion query and the slots it used so far (one per rendering instance).
    active_query: Option<(u32, Vec<u32>)>,
    /// The last packed constants per stage: (key, buffer, offset, size), reused while nothing changed.
    last_pack: [Option<(Vec<PackKey>, vk::Buffer, u64, u64)>; 3],
    zero_vertex: vk::Buffer,
    zero_vertex_memory: Option<Allocation>,
    dummy_sampler: vk::Sampler,
    dummy_compare_sampler: vk::Sampler,
    noted: HashSet<&'static str>,
    /// The decoder's reusable payload buffer.
    payload: Vec<u32>,
    /// Point-filtering twins of linear sampler states, by state id.
    nearest_samplers: FastMap<u32, vk::Sampler>,
    frames_in_flight: usize,
    /// Per-stage descriptor sets resolved for the current bindings, reused until something they depend on changes.
    stage_cache: [Option<StageCache>; 2],
    draw_scratch: DrawScratch,
    /// Bumped when a stage's textures, samplers, UAVs or shader change.
    stage_epoch: [u64; 3],
    /// Bumped when any object is destroyed, a clear is queued, a depth-proxy source is written,
    /// or the command slot rotates (its descriptor pool is reset).
    global_epoch: u64,
    /// Vertex attributes and input steps per (input layout, vertex shader).
    vertex_inputs: FastMap<(u32, u32), Rc<VertexInput>>,
    device: Rc<ash::Device>,
    /// `debug.gta5.dump_frame`: log every draw of the Nth frame after the world shows.
    dump_frame: Option<u32>,
    /// `debug.gta5.dump_in`: a new value N dumps the frame N presents later, like `dump_frame` (diagnostic).
    dump_in: Option<u32>,
    /// `debug.gta5.fps_cap`: at most this many presents per second once the world has shown (0 = uncapped).
    /// An early frame waits for its slot; a late one is presented at once and starts the next interval, so
    /// slow frames keep their rate.
    frame_interval: Option<Duration>,
    /// When the last present's interval started.
    paced_from: Option<Instant>,
    /// All time held back by the frame cap; never reset, unlike `stats.paced` (busy time subtracts its growth).
    paced_total: Duration,
    /// The null D3D11 layer's CPU storage counters, read each ring pass (NULL_D3D_STORAGE_COUNTERS).
    null_storage: [u64; 4],
    /// References the engine has queued for COM Release (grcResourceCache + 131128).
    engine_release_queue: u32,
    lazy_targets: bool,
    texture_eviction: TextureEviction,
    /// debug.gta5.attach_probe (diagnostic, wrong image): 1 depth/stencil stores, 2 depth/stencil loads, 4 color loads become DONT_CARE,
    /// to measure the GPU cost of attachment traffic before building whole-frame load/store analysis.
    attach_probe: u32,
    /// `debug.gta5.pass_profile` totals since the last FPS line: GPU ns and pass count by pass shape, and frames read.
    pass_times: FastMap<String, (u64, u32)>,
    pass_frames: u32,
    /// The frame rate last told to the window (`set_frame_rate`), if any.
    voted_fps: Option<u32>,
    /// The dump frame's lines go to `dump-<frame>.log` beside the pipeline cache: logcat drops lines at this rate.
    dump_out: RefCell<Option<std::io::BufWriter<std::fs::File>>>,
    frames_since_world: u32,
    /// `debug.gta5.show_rt`: present color target N (1-4) as it was right after the last draw whose
    /// shader labels contain `debug.gta5.show_after`, instead of the final image.
    show_rt: Option<(u32, String)>,
    /// The current draw belongs to the `debug.gta5.dump_frame` frame.
    dumping: bool,
    /// `debug.gta5.split_after`: end the pass and add a full barrier after draws whose shader labels contain it.
    split_after: Option<String>,
    /// `debug.gta5.skip_ps`: skip draws whose pixel shader label contains it (diagnostic).
    skip_ps: Option<String>,
    split_pending: bool,
    /// Target to copy at the next draw, and the copy that Present shows.
    snapshot_source: Option<u32>,
    shown_texture: Option<u32>,
    /// `debug.gta5.probe_x/probe_y`: one pixel of the `show_rt` target copied here for the log (diagnostic).
    probe: Option<(vk::Buffer, Allocation, u32)>,
}

impl NativeRenderer {
    pub fn new() -> Result<Self, RendererError> { Self::with_shader_dir(None) }

    pub fn with_shader_dir(shader_dir: Option<&Path>) -> Result<Self, RendererError> {
        let mut gpu = Gpu::new().map_err(RendererError::GpuUnavailable)?;
        let device = gpu.device.clone();
        let mut slots = Vec::with_capacity(FRAMES_IN_FLIGHT);
        for _ in 0..FRAMES_IN_FLIGHT {
            // Per-frame pools use the driver's own allocator: through our callbacks, recording and resets cost a malloc/free each.
            let pool = unsafe { device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(gpu.family).flags(vk::CommandPoolCreateFlags::TRANSIENT), None) }
                .map_err(vk_error("vkCreateCommandPool"))?;
            let profile = if debug_switch("pass_profile", false) && gpu.host_query_reset {
                let pool = unsafe { device.create_query_pool(&vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::TIMESTAMP).query_count(PROFILE_QUERIES), Some(callbacks())) }.map_err(vk_error("pass profile query pool"))?;
                unsafe { device.reset_query_pool(pool, 0, PROFILE_QUERIES) };
                Some(PassProfile { pool, keys: Vec::new() })
            } else { None };
            slots.push(Slot { pool, buffers: Vec::new(), used: 0, descriptor_pools: Vec::new(), descriptor_pool: 0, sets: FastMap::default(), pack_sets: FastMap::default(), submits: 0, last: 0, profile });
        }
        let mut layouts = LayoutCache::default();
        let empty_set = layouts.set_layout(&device, Vec::new()).map_err(RendererError::Vulkan)?;
        let occlusion = unsafe { device.create_query_pool(&vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::OCCLUSION).query_count(OCCLUSION_SLOTS), Some(callbacks())) }.map_err(vk_error("occlusion query pool"))?;
        let timestamps = unsafe { device.create_query_pool(&vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::TIMESTAMP).query_count(256), Some(callbacks())) }.map_err(vk_error("timestamp query pool"))?;
        if gpu.host_query_reset {
            unsafe { device.reset_query_pool(occlusion, 0, OCCLUSION_SLOTS); device.reset_query_pool(timestamps, 0, 256); }
        }
        let (zero_vertex, zero_memory) = gpu.create_buffer("D3D null vertex binding zeros", 4096, vk::BufferUsageFlags::VERTEX_BUFFER, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?;
        if let Some(pointer) = zero_memory.mapped_ptr() { unsafe { std::ptr::write_bytes(pointer.as_ptr() as *mut u8, 0, 4096) }; }
        crate::gpu::LEGACY_FORMATS.store(gpu.legacy, std::sync::atomic::Ordering::Relaxed);
        let dummy_sampler = create_sampler(&gpu, &[if gpu.legacy { 0 } else { 0x15 }, 3, 3, 3]).map_err(RendererError::Vulkan)?;
        let dummy_compare_sampler = create_sampler(&gpu, &[0x95, 3, 3, 3, 0, 1, 8]).map_err(RendererError::Vulkan)?;
        let pipeline_cache_path = shader_dir.and_then(Path::parent).map(|dir| dir.join("vk-pipeline-cache.bin"));
        let initial = pipeline_cache_path.as_ref().and_then(|path| std::fs::read(path).ok()).unwrap_or_default();
        let pipeline_cache = unsafe { device.create_pipeline_cache(&vk::PipelineCacheCreateInfo::default().initial_data(&initial), Some(callbacks())) }
            .or_else(|_| unsafe { device.create_pipeline_cache(&vk::PipelineCacheCreateInfo::default(), Some(callbacks())) })
            .map_err(vk_error("pipeline cache"))?;
        log_line(&format!("native Vulkan renderer on {:?}, driver {:#x}; pipeline cache {} KiB loaded; null descriptors {}",
            gpu.props.device_name_as_c_str().unwrap_or(c"?"), gpu.props.driver_version, initial.len() / 1024, gpu.null_descriptor));
        gpu.refresh_completed();
        Ok(Self {
            gpu, resources: FastMap::default(), state: PipelineState::default(), shader_store: shader_dir.map(|dir| ShaderSourceStore::new(dir.to_path_buf())),
            shader_cache: crate::shader_cache::ShaderCache::new(shader_dir),
            movie_source: None, movie_scope: MovieScope::default(), movie_pixels: Vec::new(), movie_upload: Vec::new(), movie_textures: Vec::new(),
            pipeline_cache_path, pipeline_cache, pipeline_cache_dirty: 0, pipeline_cache_saved: Instant::now(),
            purge_heap: debug_switch("purge_heap", false), heap_purged_at: Instant::now(), layouts, empty_set, surface: None, report: ProcessReport::default(),
            world: WorldShown::default(), stats: FrameStats::default(), pipelines: FastMap::default(), next_pipeline_id: 0,
            compute_pipelines: FastMap::default(), views: FastMap::default(), depth_proxies: FastMap::default(), float_proxies: FastMap::default(), float_proxy_enabled: debug_switch("depth_proxy", false), proxy_pipelines: [None; 2], stencil_green: debug_switch("stencil_green", true), zero_images: debug_switch("zero_images", true), zero_images_by_kind: FastMap::default(), pending_zero_uses: Vec::new(), next_internal_id: MAX_RESOURCE_ID + 1,
            pending_clears: FastMap::default(), recording: None, slots, slot: 0,
            uniforms: Ring::new(vk::BufferUsageFlags::UNIFORM_BUFFER, UNIFORM_CHUNK), staging: Ring::new(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST, STAGING_CHUNK),
            garbage: Garbage { pooling: debug_switch("buffer_pool", false), ..Garbage::default() }, completions: VecDeque::new(),
            occlusion: QuerySlots::new(occlusion, OCCLUSION_SLOTS, vk::QueryType::OCCLUSION),
            timestamps: QuerySlots::new(timestamps, 256, vk::QueryType::TIMESTAMP), active_query: None,
            last_pack: [None, None, None], zero_vertex, zero_vertex_memory: Some(zero_memory), dummy_sampler, dummy_compare_sampler, noted: HashSet::new(), payload: Vec::new(), nearest_samplers: FastMap::default(),
            frames_in_flight: if debug_switch("one_frame", false) { 1 } else { FRAMES_IN_FLIGHT },
            dump_frame: debug_number("dump_frame").filter(|frame| *frame > 0), dump_in: debug_number("dump_in"), frame_interval: frame_interval(), paced_from: None, paced_total: Duration::ZERO, null_storage: [0; 4], engine_release_queue: 0, lazy_targets: debug_switch("lazy_targets", true), attach_probe: debug_number("attach_probe").unwrap_or(0), pass_times: FastMap::default(), pass_frames: 0, voted_fps: None, dump_out: RefCell::new(None), frames_since_world: 0,
            texture_eviction: TextureEviction::new(shader_dir),
            show_rt: None, dumping: false, split_after: None, skip_ps: None, split_pending: false, snapshot_source: None, shown_texture: None, probe: None,
            stage_cache: [None, None], draw_scratch: DrawScratch::default(), stage_epoch: [0; 3], global_epoch: 0, vertex_inputs: FastMap::default(), device: Rc::new(device),
        })
    }

    /// Attach an Android `ANativeWindow` owned by the native activity.
    ///
    /// # Safety
    /// The caller must pass a live `ANativeWindow` pointer; the surface takes its own reference.
    pub unsafe fn attach_anative_window(&mut self, window: *mut c_void) -> Result<(), RendererError> {
        self.detach_surface();
        self.surface = Some(unsafe { NativeSurface::new(&self.gpu, window) }?);
        self.voted_fps = None;
        Ok(())
    }

    pub fn resize_surface(&mut self, width: u32, height: u32) -> Result<(), RendererError> {
        let surface = self.surface.as_mut().ok_or(RendererError::Surface(SurfaceError::NeedsReconfigure))?;
        let srgb = is_srgb(surface.format);
        surface.configure(&mut self.gpu, width, height, srgb)?;
        Ok(())
    }

    pub fn detach_surface(&mut self) {
        // The app may be killed while in the background: keep what this session compiled.
        self.save_pipeline_cache();
        if let Some(surface) = self.surface.take() {
            unsafe { let _ = self.gpu.device.device_wait_idle(); }
            surface.destroy(&self.gpu);
        }
    }

    pub fn report(&self) -> &ProcessReport { &self.report }

    pub fn has_pending_completions(&self) -> bool { !self.completions.is_empty() }

    /// Drain a ring that lives inside the live engine heap. `ring_base` is an absolute guest address.
    pub fn process_shared_ring<M: SharedMemory + ?Sized>(&mut self, memory: &M, ring_base: usize, ring_len: usize) -> Result<ProcessReport, RendererError> {
        let started = Instant::now();
        self.texture_eviction.clock = Some(started);
        let paced_before = self.paced_total;
        let mut decoder = SharedRingDecoder::new(memory, ring_base, ring_len)?;
        if decoder.state()? == 0 { decoder.start()?; }
        self.complete(memory)?;
        for (i, counter) in self.null_storage.iter_mut().enumerate() {
            let address = NULL_D3D_STORAGE_COUNTERS + i * 8;
            if let (Ok(low), Ok(high)) = (memory.read_u32(address), memory.read_u32(address + 4)) { *counter = u64::from(high) << 32 | u64::from(low); }
        }
        self.engine_release_queue = memory.read_u32(12_485_568 + 131_128).unwrap_or(0);
        let mut commands = 0usize;
        let mut payload = std::mem::take(&mut self.payload);
        loop {
            if commands == MAX_COMMANDS_PER_SHARED_PASS { break; }
            let opcode = match decoder.next_into(&mut payload)? {
                Some(opcode) => opcode,
                None => {
                    // The producer may wait on recorded work: submit it before the worker idles.
                    self.submit(Opcode::Nop, None)?;
                    break;
                }
            };
            commands += 1;
            if opcode == Opcode::Wrap { continue; }
            if let Err(error) = self.execute(opcode, &payload, memory) {
                decoder.fail()?;
                return Err(error);
            }
            decoder.publish_if_due()?;
            self.report.executed = self.report.executed.saturating_add(1);
        }
        // Big uploads leave a large buffer behind: keep at most 1 MiB for the next pass.
        if payload.capacity() > 1 << 18 { payload = Vec::new(); }
        self.payload = payload;
        decoder.publish()?;
        self.report.validation_errors = decoder.validation_errors()?;
        self.complete(memory)?;
        self.stats.busy += started.elapsed().saturating_sub(self.paced_total - paced_before);
        Ok(self.report.clone())
    }

    pub fn set_movie_source(&mut self, source: Option<MovieFrameSource>) -> Result<(), RendererError> {
        self.movie_scope.end();
        let textures=std::mem::take(&mut self.movie_textures);
        for texture in textures { self.destroy(texture.id)?; }
        self.movie_source=source;
        Ok(())
    }

    fn movie_blit(&mut self, hash: u32) -> Result<(), RendererError> {
        let target=self.state.render_targets[0];
        self.movie_scope.begin(MovieTarget { texture:target.texture,mip:target.mip,layer:target.slice });
        if target.texture==0 { return Ok(()); }
        let clear=[target.texture,target.mip,target.slice,0,0,0,1f32.to_bits()];
        let Some(source)=self.movie_source else { return self.clear_rt(&clear); };
        let position=self.movie_textures.iter().position(|t| t.hash==hash);
        let previous=position.map(|i| self.movie_textures[i].serial).unwrap_or(0);
        let captured=source.capture(hash,&mut self.movie_pixels,previous);
        let captured=match captured {
            Ok(frame)=>frame,
            Err(error)=>{
                let note=format!("movie {hash:#x}: {error}");
                if self.noted.insert("movie-frame-error") { log_line(&note); }
                return self.clear_rt(&clear);
            }
        };
        if captured==CapturedFrame::Inactive { return self.clear_rt(&clear); }
        let (width,height,serial)=match captured {
            CapturedFrame::New { width,height,serial }=>(width,height,serial),
            CapturedFrame::Unchanged { width,height }=>(width,height,previous),
            CapturedFrame::Inactive=>unreachable!(),
        };
        let mut cached=position.map(|i| self.movie_textures.remove(i));
        if cached.as_ref().is_some_and(|t| t.width!=width||t.height!=height) {
            self.destroy(cached.take().unwrap().id)?;
        }
        if cached.is_none() {
            if !matches!(captured,CapturedFrame::New {..}) { return self.clear_rt(&clear); }
            if self.movie_textures.len()>=8 { let old=self.movie_textures.remove(0); self.destroy(old.id)?; }
            let id=self.next_internal_id;
            self.next_internal_id=self.next_internal_id.checked_add(1).ok_or_else(||self.command_error(Opcode::MovieBlit,"internal movie texture ids exhausted"))?;
            let texture=self.build_texture(&[id,2,width,height,1,1,1,28,BIND_SRV,1])?;
            self.resources.insert(id,Resource::Texture(Box::new(texture)));
            cached=Some(MovieTexture { hash,id,width,height,serial:0 });
        }
        let mut cached=cached.unwrap();
        if matches!(captured,CapturedFrame::New {..}) {
            let size=width as usize*height as usize*4;
            let mut upload=std::mem::take(&mut self.movie_upload);
            upload.clear();
            upload.extend_from_slice(&[cached.id,0,0,0,0,0,width,height,1,width*4,height,size as u32]);
            upload.extend(self.movie_pixels[..size].chunks_exact(4).map(|b|u32::from_le_bytes([b[0],b[1],b[2],b[3]])));
            let result=self.upload_texture(&upload);
            self.movie_upload=upload;
            result?;cached.serial=serial;
        }
        let src=cached.id;
        self.movie_textures.push(cached);
        let texture=self.texture(Opcode::MovieBlit,target.texture)?;
        if texture.info.depth||texture.samples!=1||texture.dimension!=vk::ImageType::TYPE_2D||
            target.mip>=texture.mip_levels||target.slice>=texture.layers() { return self.clear_rt(&clear); }
        let (tw,th,_)=texture.mip_extent(target.mip);
        let Some(region)=crate::movie::blit_region([width,height],[tw,th],self.state.viewports.first().copied(),self.state.scissors.first().copied()) else { return Ok(()); };
        let dst_raw=texture.raw;let dst_format=texture.format;
        let undefined=texture.layouts[(target.mip+texture.mip_levels*target.slice) as usize]==vk::ImageLayout::UNDEFINED;
        let src_texture=self.texture(Opcode::MovieBlit,src)?;let src_raw=src_texture.raw;let src_format=src_texture.format;
        let src_features=unsafe { self.gpu.instance.get_physical_device_format_properties(self.gpu.physical,src_format) }.optimal_tiling_features;
        let dst_features=unsafe { self.gpu.instance.get_physical_device_format_properties(self.gpu.physical,dst_format) }.optimal_tiling_features;
        if !src_features.contains(vk::FormatFeatureFlags::BLIT_SRC)||!dst_features.contains(vk::FormatFeatureFlags::BLIT_DST) {
            let note=format!("movie target does not support image blits: {src_format:?} -> {dst_format:?}");
            if self.noted.insert("movie-blit-format") { log_line(&note); }
            return self.clear_rt(&clear);
        }
        if undefined&&(region.target[0]!=[0,0,0]||region.target[1]!=[tw as i32,th as i32,1]) { self.clear_rt(&clear)?; }
        self.end_pass();
        self.materialize_clears(target.texture)?;
        let cb=self.cb()?;
        self.flush_pending_writes(cb);
        self.transition(cb,&[
            ImageUse { texture:src,mips:(0,1),layers:(0,1),layout:vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE },
            ImageUse { texture:target.texture,mips:(target.mip,1),layers:(target.slice,1),layout:vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE },
        ],false)?;
        let offsets=|o:[[i32;3];2]|o.map(|v|vk::Offset3D {x:v[0],y:v[1],z:v[2]});
        let copy=vk::ImageBlit::default()
            .src_subresource(vk::ImageSubresourceLayers { aspect_mask:vk::ImageAspectFlags::COLOR,mip_level:0,base_array_layer:0,layer_count:1 })
            .src_offsets(offsets(region.source))
            .dst_subresource(vk::ImageSubresourceLayers { aspect_mask:vk::ImageAspectFlags::COLOR,mip_level:target.mip,base_array_layer:target.slice,layer_count:1 })
            .dst_offsets(offsets(region.target));
        let filter=if self.gpu.linear_filterable(src_format) {vk::Filter::LINEAR}else{vk::Filter::NEAREST};
        unsafe { self.gpu.device.cmd_blit_image(cb,src_raw,vk::ImageLayout::TRANSFER_SRC_OPTIMAL,dst_raw,vk::ImageLayout::TRANSFER_DST_OPTIMAL,&[copy],filter); }
        self.mark_written(target.texture);
        Ok(())
    }

    fn execute<M: SharedMemory + ?Sized>(&mut self, opcode: Opcode, p: &[u32], memory: &M) -> Result<(), RendererError> {
        // Commands that may read or write constant buffers' GPU copies (draws and dispatches pack from the shadows).
        match opcode {
            Opcode::CopyRegion => { self.sync_constant(p[0])?; self.sync_constant(p[5])?; }
            Opcode::CopyResource => { self.sync_constant(p[0])?; self.sync_constant(p[1])?; }
            Opcode::Readback | Opcode::CopyCounter => self.sync_constant(p[0])?,
            _ => {}
        }
        if self.dump_frame == Some(self.frames_since_world) && self.report.world && !matches!(opcode, Opcode::Draw | Opcode::Nop) && (!format!("{opcode:?}").starts_with("Set") || matches!(opcode, Opcode::SetSrvs | Opcode::SetRenderTargets)) {
            self.dump_line(&format!("dump command {} after draw {}: {opcode:?} {:?}", self.stats.draws, self.stats.draws, &p[..p.len().min(12)]));
        }
        let outcome = match opcode {
            Opcode::Nop | Opcode::Init | Opcode::Wrap => Ok(()),
            Opcode::CreateTexture => self.create_texture(p),
            Opcode::DestroyObject => self.destroy(p[0]),
            Opcode::ClearRt => self.clear_rt(p),
            Opcode::ClearDs => self.clear_ds(p),
            Opcode::Present => { self.movie_scope.end(); self.present(p) },
            Opcode::Fence => self.fence(p, memory),
            Opcode::Screenshot => Err(self.unsupported(opcode, "native screenshot sink is not wired")),
            Opcode::Log => { log_line(&format!("[gta5-native-renderer] {}", String::from_utf8_lossy(words_as_bytes(&p[1..], p[0] as usize)))); Ok(()) }
            Opcode::CreateBuffer => self.create_buffer(p),
            Opcode::UploadBuffer | Opcode::UploadBufferNoOverwrite => self.upload_buffer(p, opcode == Opcode::UploadBuffer),
            Opcode::UploadTexture => self.upload_texture(p),
            Opcode::CreateShader => self.create_shader(p),
            Opcode::CreateLayout => {
                let elements = (0..p[1] as usize).map(|i| { let q = 2 + i * 7; InputElement { semantic_hash: p[q], semantic_index: p[q + 1], dxgi_format: p[q + 2], slot: p[q + 3], offset: p[q + 4], slot_class: p[q + 5], step_rate: p[q + 6] } }).collect();
                self.insert(Opcode::CreateLayout, p[0], Resource::Layout(elements))
            }
            Opcode::CreateState => {
                let words = p[2..].to_vec();
                let sampler = if p[1] == 3 { Some(create_sampler(&self.gpu, &words).map_err(|error| self.command_error(Opcode::CreateState, error))?) } else { None };
                self.insert(Opcode::CreateState, p[0], Resource::State(StateResource { kind: p[1], words, sampler }))
            }
            Opcode::CreateSrv | Opcode::CreateUav => {
                let is_buffer = p[1] == 1;
                match self.resources.get(&p[2]) {
                    Some(Resource::Buffer(_)) if is_buffer => {}
                    Some(Resource::Texture(_)) if !is_buffer => {}
                    Some(_) => return Err(self.command_error(opcode, "view resource kind does not match the descriptor")),
                    None => return Err(self.command_error(opcode, format!("resource {} does not exist", p[2]))),
                }
                let d = if opcode == Opcode::CreateSrv { p[8] } else { 0 };
                self.insert(opcode, p[0], Resource::View(ViewResource { resource: p[2], is_buffer, format: p[3], view_dimension: p[4], a: p[5], b: p[6], c: p[7], d }))
            }
            Opcode::SetShader => {
                let stage = p[0] as usize;
                if stage >= 3 { return Err(self.command_error(Opcode::SetShader, "shader stage is outside vertex/pixel/compute")); }
                if self.state.shaders[stage] != p[1] { self.state.shaders[stage] = p[1]; self.stage_epoch[stage] += 1; }
                Ok(())
            }
            Opcode::SetInputLayout => { self.state.layout = p[0]; Ok(()) }
            Opcode::SetTopology => { self.state.topology = p[0]; Ok(()) }
            Opcode::SetVertexBuffers => {
                for i in 0..p[1] as usize { self.state.vertex[p[0] as usize + i] = VertexBinding { buffer: p[2 + i * 3], stride: p[3 + i * 3], offset: p[4 + i * 3] }; }
                Ok(())
            }
            Opcode::SetIndexBuffer => { self.state.index = IndexBinding { buffer: p[0], format: p[1], offset: p[2] }; Ok(()) }
            Opcode::SetCbuffers => {
                let (stage, start, count) = (p[0] as usize, p[1] as usize, p[2] as usize);
                if stage >= 3 || start + count > 16 { return Err(self.command_error(Opcode::SetCbuffers, "constant binding range is outside the three-stage 16-slot ABI")); }
                for i in 0..count { self.state.constants[stage][start + i] = ConstantBinding { buffer: p[3 + i * 3], first: p[4 + i * 3], num: p[5 + i * 3] }; }
                Ok(())
            }
            Opcode::SetSrvs => {
                let (stage, start, count) = (p[0] as usize, p[1] as usize, p[2] as usize);
                if stage >= 3 || start + count > 128 { return Err(self.command_error(Opcode::SetSrvs, "SRV binding range is outside the three-stage 128-slot ABI")); }
                if self.state.srvs[stage][start..start + count] != p[3..3 + count] {
                    self.state.srvs[stage][start..start + count].copy_from_slice(&p[3..3 + count]);
                    self.stage_epoch[stage] += 1;
                }
                Ok(())
            }
            Opcode::SetSamplers => {
                let (stage, start, count) = (p[0] as usize, p[1] as usize, p[2] as usize);
                if stage >= 3 || start + count > 16 { return Err(self.command_error(Opcode::SetSamplers, "sampler binding range is outside the three-stage 16-slot ABI")); }
                if self.state.samplers[stage][start..start + count] != p[3..3 + count] {
                    self.state.samplers[stage][start..start + count].copy_from_slice(&p[3..3 + count]);
                    self.stage_epoch[stage] += 1;
                }
                Ok(())
            }
            Opcode::SetBlend => {
                self.state.blend = p[0];
                self.state.blend_factor = [f32::from_bits(p[1]), f32::from_bits(p[2]), f32::from_bits(p[3]), f32::from_bits(p[4])];
                if !self.state.blend_factor.iter().all(|value| value.is_finite()) { return Err(self.command_error(Opcode::SetBlend, "blend constant is not finite")); }
                Ok(())
            }
            Opcode::SetDepthStencil => { self.state.depth_stencil_state = p[0]; self.state.stencil_reference = p[1]; Ok(()) }
            Opcode::SetRaster => { self.state.raster_state = p[0]; Ok(()) }
            Opcode::SetRenderTargets => {
                for i in 0..8 { self.state.render_targets[i] = RenderTarget { texture: p[i * 4], mip: p[i * 4 + 1], slice: p[i * 4 + 2], format: p[i * 4 + 3], flags: 0 }; }
                self.state.depth_stencil = RenderTarget { texture: p[32], mip: p[33], slice: p[34], format: p[35], flags: p[36] };
                Ok(())
            }
            Opcode::SetViewports => {
                self.state.viewports = (0..p[0] as usize).map(|i| std::array::from_fn(|k| f32::from_bits(p[1 + i * 6 + k]))).collect();
                Ok(())
            }
            Opcode::SetScissors => {
                self.state.scissors = (0..p[0] as usize).map(|i| [p[1 + i * 4], p[2 + i * 4], p[3 + i * 4], p[4 + i * 4]]).collect();
                Ok(())
            }
            Opcode::Draw => {
                let target=self.state.render_targets[0];
                if self.movie_scope.skip(MovieTarget { texture:target.texture,mip:target.mip,layer:target.slice }) { return Ok(()); }
                self.draw(p)?;
                self.report.draws = self.report.draws.saturating_add(1);
                self.stats.draws += 1;
                Ok(())
            }
            Opcode::ClearState => {
                // The native movie quad setup can clear ordinary graphics
                // state. Its replacement scope lasts until MOVIE_END/Present.
                self.state = PipelineState::default();
                for epoch in &mut self.stage_epoch { *epoch += 1; }
                Ok(())
            }
            Opcode::CopyRegion => self.copy_region(p),
            Opcode::MovieBlit => self.movie_blit(p[0]),
            Opcode::MovieEnd => { self.movie_scope.end(); Ok(()) },
            Opcode::CopyResource => self.copy_resource(p),
            Opcode::Resolve => self.resolve(p),
            Opcode::SetUavs => self.set_uavs(p),
            Opcode::Dispatch => {
                self.report.dispatches = self.report.dispatches.saturating_add(1);
                self.dispatch(p)
            }
            Opcode::ClearUav => self.clear_uav(p),
            Opcode::GenerateMips => self.generate_mips(p),
            Opcode::Readback => self.readback(p, memory),
            Opcode::CopyCounter => self.copy_counter(p),
            Opcode::CreateQuery => self.create_query(p),
            Opcode::BeginQuery => self.begin_query(p),
            Opcode::EndQuery => self.end_query(p, memory),
        };
        outcome?;
        // Finish the command first: its new bindings and uses must protect it from eviction.
        self.maintain_texture_residency()
    }

    fn unsupported(&mut self, opcode: Opcode, detail: &str) -> RendererError {
        self.report.unsupported = self.report.unsupported.saturating_add(1);
        RendererError::Unsupported { opcode, detail: detail.to_string() }
    }

    fn command_error(&self, opcode: Opcode, detail: impl Into<String>) -> RendererError {
        RendererError::Command { opcode, detail: detail.into() }
    }

    /// Logs the first occurrence of each kind of note (ids in `text` would make the set grow all session).
    fn note_once(&mut self, kind: &'static str, text: String) {
        if self.noted.insert(kind) { log_line(&text); }
    }

    fn insert(&mut self, opcode: Opcode, id: u32, resource: Resource) -> Result<(), RendererError> {
        if id == 0 || id > MAX_RESOURCE_ID { return Err(self.command_error(opcode, format!("resource id {id} is outside 1..={MAX_RESOURCE_ID}"))); }
        if self.resources.insert(id, resource).is_some() { return Err(self.command_error(opcode, format!("resource id {id} was already allocated"))); }
        Ok(())
    }

    fn texture(&self, opcode: Opcode, id: u32) -> Result<&TextureResource, RendererError> {
        match self.resources.get(&id) {
            Some(Resource::Texture(texture)) => Ok(texture),
            Some(_) => Err(self.command_error(opcode, format!("resource {id} is not a texture"))),
            None => Err(self.command_error(opcode, format!("texture {id} does not exist"))),
        }
    }

    fn buffer(&self, opcode: Opcode, id: u32) -> Result<&BufferResource, RendererError> {
        match self.resources.get(&id) {
            Some(Resource::Buffer(buffer)) => Ok(buffer),
            Some(_) => Err(self.command_error(opcode, format!("resource {id} is not a buffer"))),
            None => Err(self.command_error(opcode, format!("buffer {id} does not exist"))),
        }
    }

    /// The timeline value the open (or next) recording will signal.
    fn recording_value(&self) -> u64 { self.gpu.submitted + 1 }

    fn create_buffer(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let started = Instant::now();
        let result = self.create_buffer_resource(p);
        self.stats.buffer_creates += 1;
        self.stats.buffer_create_bytes += u64::from(p[1]);
        self.stats.buffer_create_time += started.elapsed();
        result
    }

    fn create_buffer_resource(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let (id, size, bind, misc, stride) = (p[0], u64::from(p[1]), p[2], p[3], p[4]);
        if size == 0 || size > MAX_BUFFER_BYTES { return Err(self.command_error(Opcode::CreateBuffer, format!("buffer size {size} is outside 1..={MAX_BUFFER_BYTES}"))); }
        let mut usage = vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST;
        if bind & BIND_VERTEX != 0 { usage |= vk::BufferUsageFlags::VERTEX_BUFFER; }
        if bind & BIND_INDEX != 0 { usage |= vk::BufferUsageFlags::INDEX_BUFFER; }
        if bind & BIND_CONSTANT != 0 { usage |= vk::BufferUsageFlags::UNIFORM_BUFFER; }
        if bind & (BIND_SRV | BIND_UAV) != 0 { usage |= vk::BufferUsageFlags::STORAGE_BUFFER; }
        if misc & MISC_DRAW_INDIRECT != 0 { usage |= vk::BufferUsageFlags::INDIRECT_BUFFER; }
        let aligned = size.next_multiple_of(4);
        // Draws and dispatches read constant buffers from their CPU shadow (packed constants): the GPU copy is made
        // only when a copy or readback first touches the buffer (sync_constant).
        let (raw, memory) = if bind == BIND_CONSTANT { (vk::Buffer::null(), Allocation::default()) } else {
            match self.garbage.take(usage, aligned) {
                Some(reused) => reused,
                None => self.gpu.create_buffer("gta5 buffer", aligned, usage, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?,
            }
        };
        let shadow = (bind & BIND_CONSTANT != 0).then(|| vec![0u8; size as usize]);
        let counter = if bind & BIND_UAV != 0 {
            let (buffer, memory) = self.gpu.create_buffer("D3D UAV counter", 16, vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?;
            if let Some(pointer) = memory.mapped_ptr() { unsafe { std::ptr::write_bytes(pointer.as_ptr() as *mut u8, 0, 16) }; }
            Some((buffer, memory))
        } else { None };
        self.insert(Opcode::CreateBuffer, id, Resource::Buffer(Box::new(BufferResource { raw, memory, usage, size, misc, stride, shadow, generation: 0, stale: false, counter, last_use: 0, submitted_use: 0, pre_write: 0, cb_write: 0 })))
    }

    fn bind_texture_memory(&mut self, raw: vk::Image) -> Result<Allocation, RendererError> {
        let requirements = unsafe { self.gpu.device.get_image_memory_requirements(raw) };
        let memory = self.gpu.allocate("gta5 texture", requirements, MemoryLocation::GpuOnly, false, AllocationScheme::DedicatedImage(raw)).map_err(RendererError::Vulkan)?;
        if let Err(error) = unsafe { self.gpu.device.bind_image_memory(raw, memory.memory(), memory.offset()) } {
            self.gpu.free(memory);
            return Err(RendererError::Vulkan(format!("vkBindImageMemory: {error}")));
        }
        Ok(memory)
    }

    fn ensure_texture_memory(&mut self, id: u32) -> Result<(), RendererError> {
        let texture = self.texture(Opcode::CreateTexture, id)?;
        if texture.memory.is_some() { return Ok(()); }
        let raw = texture.raw;
        if let Some(backing) = texture.backing.clone() {
            if self.full_pending_color_clear(id) { return self.overwrite_texture(id); }
            return self.restore_texture(id, raw, backing);
        }
        let memory = self.bind_texture_memory(raw)?;
        if let Some(Resource::Texture(texture)) = self.resources.get_mut(&id) { texture.memory = Some(memory); }
        Ok(())
    }

    /// Every mip and layer must have a full color clear queued before old pixels can be discarded.
    fn full_pending_color_clear(&self, id: u32) -> bool {
        let Some(Resource::Texture(texture)) = self.resources.get(&id) else { return false };
        texture.backing.is_some() && !texture.info.depth && texture.dimension != vk::ImageType::TYPE_3D
            && (0..texture.layers()).all(|layer| (0..texture.mip_levels).all(|mip|
                self.pending_clears.get(&(id, mip, layer)).is_some_and(|clear| clear.color.is_some())))
    }

    /// The caller has proved that the next operation replaces every pixel of the image.
    fn overwrite_texture(&mut self, id: u32) -> Result<(), RendererError> {
        let texture = self.texture(Opcode::CreateTexture, id)?;
        if texture.memory.is_some() || texture.backing.is_none() { return Ok(()); }
        let raw = texture.raw;
        // Keep the saved contents available if allocation fails.
        let memory = self.bind_texture_memory(raw)?;
        let Some(Resource::Texture(texture)) = self.resources.get_mut(&id) else { unreachable!("validated texture") };
        let backing = texture.backing.take().expect("evicted texture");
        texture.memory = Some(memory);
        texture.age_restored = true;
        self.texture_eviction.release_backing(&backing);
        self.texture_eviction.overwritten += 1;
        self.texture_eviction.overwritten_bytes += backing.size;
        log_line(&format!("texture overwrite: id {id}, {} bytes of obsolete backing skipped; pinned for session", backing.size));
        Ok(())
    }

    fn restore_texture(&mut self, id: u32, raw: vk::Image, backing: TextureBacking) -> Result<(), RendererError> {
        let started = Instant::now();
        let file = self.texture_eviction.file.clone().ok_or_else(|| RendererError::Vulkan("missing texture backing file".into()))?;
        let (buffer, allocation) = self.gpu.create_buffer("texture restoration", backing.size, vk::BufferUsageFlags::TRANSFER_SRC, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?;
        let prepared = (|| -> Result<vk::CommandBuffer, RendererError> {
            let pointer = allocation.mapped_ptr().ok_or_else(|| RendererError::Vulkan("texture restoration buffer is not mapped".into()))?;
            let bytes = unsafe { std::slice::from_raw_parts_mut(pointer.as_ptr() as *mut u8, backing.size as usize) };
            file.read_exact_at(bytes, backing.offset).map_err(|e| RendererError::Vulkan(format!("restore texture {id}: {e}")))?;
            if !allocation.memory_properties().contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
                unsafe { self.gpu.device.flush_mapped_memory_ranges(&[vk::MappedMemoryRange::default().memory(allocation.memory()).offset(0).size(vk::WHOLE_SIZE)]) }.map_err(vk_error("flush texture restoration"))?;
            }
            self.pre_cb()
        })();
        let cb = match prepared {
            Ok(cb) => cb,
            Err(error) => { unsafe { self.gpu.device.destroy_buffer(buffer, Some(callbacks())) }; self.gpu.free(allocation); return Err(error); }
        };
        let memory = match self.bind_texture_memory(raw) {
            Ok(memory) => memory,
            Err(error) => { unsafe { self.gpu.device.destroy_buffer(buffer, Some(callbacks())) }; self.gpu.free(allocation); return Err(error); }
        };
        let barriers: Vec<_> = backing.regions.iter().map(|r| vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE).src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED).new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).image(raw)
            .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: r.image_subresource.mip_level, level_count: 1, base_array_layer: r.image_subresource.base_array_layer, layer_count: 1 })).collect();
        unsafe {
            self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().image_memory_barriers(&barriers));
            self.gpu.device.cmd_copy_buffer_to_image(cb, buffer, raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &backing.regions);
        }
        let value = self.recording_value();
        self.garbage.buffer(value, buffer, allocation);
        if let Some(Resource::Texture(texture)) = self.resources.get_mut(&id) {
            texture.memory = Some(memory);
            texture.backing = None;
            texture.age_restored = true;
            for region in &backing.regions {
                texture.layouts[(region.image_subresource.mip_level + texture.mip_levels * region.image_subresource.base_array_layer) as usize] = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
            }
        }
        // GPU restoration reads the staging copy, so the file extent is no longer needed.
        self.texture_eviction.release_backing(&backing);
        self.texture_eviction.restored += 1;
        self.texture_eviction.restored_bytes += backing.size;
        self.texture_eviction.rapid_returns += u64::from(value.saturating_sub(backing.evicted_at) < TEXTURE_EVICT_AGE);
        self.texture_eviction.restore_time += started.elapsed();
        log_line(&format!("texture restore: id {id}, {} bytes, {} us, {} submissions since eviction; pinned for session", backing.size, started.elapsed().as_micros(), value.saturating_sub(backing.evicted_at)));
        Ok(())
    }

    fn bound_texture_ids(&self) -> FastSet<u32> {
        let mut ids: FastSet<u32> = self.state.render_targets.iter().chain(std::iter::once(&self.state.depth_stencil))
            .map(|target| target.texture).filter(|id| *id != 0).collect();
        for id in self.state.srvs.iter().flatten().chain(self.state.uavs.iter().flatten()).filter(|id| **id != 0) {
            if let Some(Resource::View(view)) = self.resources.get(id) {
                if !view.is_buffer { ids.insert(view.resource); }
            }
        }
        ids
    }

    fn eviction_unchanged(&self, pending: &PendingEviction) -> bool {
        matches!(self.resources.get(&pending.id), Some(Resource::Texture(t)) if t.eviction_ticket == pending.ticket && t.use_serial == pending.serial && t.generation == pending.generation)
            && !self.pending_clears.keys().any(|key| key.0 == pending.id)
            && !self.bound_texture_ids().contains(&pending.id)
    }

    fn maintain_texture_residency(&mut self) -> Result<(), RendererError> {
        if self.texture_eviction.file.is_none() || self.texture_eviction.checked_rotation == self.texture_eviction.rotations { return Ok(()); }
        self.texture_eviction.checked_rotation = self.texture_eviction.rotations;
        if let Some(mut pending) = self.texture_eviction.pending.take() {
            let unchanged = self.eviction_unchanged(&pending);
            match pending.copy {
                EvictionCopy::Gpu { buffer, memory, value } => {
                    if self.gpu.completed < value {
                        self.texture_eviction.pending = Some(PendingEviction { copy: EvictionCopy::Gpu { buffer, memory, value }, ..pending });
                        return Ok(());
                    }
                    if !unchanged {
                        self.texture_eviction.cancelled += 1;
                        self.texture_eviction.release_backing(&pending.backing);
                        unsafe { self.gpu.device.destroy_buffer(buffer, Some(callbacks())) }; self.gpu.free(memory);
                        return Ok(());
                    }
                    if !memory.memory_properties().contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
                        if let Err(error) = unsafe { self.gpu.device.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default().memory(memory.memory()).offset(0).size(vk::WHOLE_SIZE)]) } {
                            self.texture_eviction.release_backing(&pending.backing);
                            unsafe { self.gpu.device.destroy_buffer(buffer, Some(callbacks())) }; self.gpu.free(memory);
                            self.texture_eviction.errors += 1; self.texture_eviction.failed = true;
                            log_line(&format!("texture backup cancelled: invalidate {error}")); return Ok(());
                        }
                    }
                    let data = memory.mapped_ptr().and_then(|pointer| {
                        let mut data = Vec::new();
                        data.try_reserve_exact(pending.backing.size as usize).ok()?;
                        data.extend_from_slice(unsafe { std::slice::from_raw_parts(pointer.as_ptr() as *const u8, pending.backing.size as usize) });
                        Some(data)
                    });
                    unsafe { self.gpu.device.destroy_buffer(buffer, Some(callbacks())) }; self.gpu.free(memory);
                    let Some(data) = data else {
                        self.texture_eviction.release_backing(&pending.backing);
                        self.texture_eviction.errors += 1; self.texture_eviction.failed = true;
                        log_line("texture backup cancelled: CPU snapshot storage unavailable; original retained");
                        return Ok(());
                    };
                    let file = self.texture_eviction.file.clone().expect("backing file");
                    let offset = pending.backing.offset;
                    let (send, receive) = mpsc::sync_channel(1);
                    if let Err(error) = std::thread::Builder::new().name("texture-backup".into()).spawn(move || { let _ = send.send(file.write_all_at(&data, offset)); }) {
                        self.texture_eviction.release_backing(&pending.backing);
                        self.texture_eviction.errors += 1; self.texture_eviction.failed = true;
                        log_line(&format!("texture backup cancelled: {error}")); return Ok(());
                    }
                    pending.copy = EvictionCopy::Disk(receive);
                    self.texture_eviction.pending = Some(pending);
                    return Ok(());
                }
                EvictionCopy::Disk(receive) => {
                    match receive.try_recv() {
                        Err(mpsc::TryRecvError::Empty) => {
                            self.texture_eviction.pending = Some(PendingEviction { copy: EvictionCopy::Disk(receive), ..pending }); return Ok(());
                        }
                        Ok(Ok(())) if unchanged => {
                            let texture = self.texture(Opcode::CreateTexture, pending.id)?;
                            let raw = match texture.new_unbound_image(&self.gpu.device) {
                                Ok(raw) => raw,
                                Err(error) => {
                                    self.texture_eviction.release_backing(&pending.backing);
                                    self.texture_eviction.errors += 1; self.texture_eviction.failed = true;
                                    log_line(&format!("texture backup cancelled; original retained: {error}"));
                                    return Ok(());
                                }
                            };
                            self.global_epoch += 1;
                            for slot in &mut self.slots { slot.sets.clear(); }
                            self.drop_texture_views(pending.id, 0);
                            self.garbage.collect(&mut self.gpu, false);
                            if let Some(Resource::Texture(texture)) = self.resources.get_mut(&pending.id) {
                                let memory = texture.memory.take().expect("resident backup source");
                                self.texture_eviction.released += memory.size();
                                self.texture_eviction.evicted += 1;
                                unsafe { self.gpu.device.destroy_image(texture.raw, Some(callbacks())) }; self.gpu.free(memory);
                                texture.raw = raw;
                                texture.layouts.fill(vk::ImageLayout::UNDEFINED);
                                pending.backing.evicted_at = self.recording_value();
                                if let Some(Resource::Texture(texture)) = self.resources.get_mut(&pending.id) { texture.backing = Some(pending.backing); }
                            }
                        }
                        Ok(Ok(())) => { self.texture_eviction.release_backing(&pending.backing); self.texture_eviction.cancelled += 1; }
                        other => {
                            self.texture_eviction.release_backing(&pending.backing);
                            self.texture_eviction.errors += 1; self.texture_eviction.failed = true;
                            log_line(&format!("texture backup failed; original retained: {other:?}"));
                        }
                    }
                }
            }
        }
        if self.texture_eviction.pending.is_some() || self.texture_eviction.failed || self.texture_eviction.rotations % 16 != 0 { return Ok(()); }
        let now = self.texture_eviction.clock.unwrap_or_else(Instant::now);
        let bound = self.bound_texture_ids();
        let candidate = self.resources.iter().filter_map(|(&id, r)| {
            let Resource::Texture(t) = r else { return None };
            if id > MAX_RESOURCE_ID || t.memory.as_ref().is_none_or(|memory| memory.size() < TEXTURE_EVICT_MIN_BYTES)
                || t.backing.is_some() || t.age_restored || t.info.depth || t.samples != 1 || bound.contains(&id)
                || t.last_use == 0 || t.last_use > self.gpu.completed || self.gpu.submitted.saturating_sub(t.last_use) < TEXTURE_EVICT_AGE
                || t.last_use_at.is_none_or(|used| now.saturating_duration_since(used) < TEXTURE_EVICT_IDLE_TIME)
                || self.pending_clears.keys().any(|key| key.0 == id) { return None; }
            let (regions, size) = t.copy_regions();
            (size >= TEXTURE_EVICT_MIN_BYTES && size <= TEXTURE_EVICT_CHUNK && self.texture_eviction.backing_space.can_allocate(size, TEXTURE_BACKING_LIMIT)).then_some((t.last_use, id, regions, size))
        }).min_by_key(|(age, id, _, size)| (std::cmp::Reverse(*size), *age, *id));
        let Some((_, id, regions, size)) = candidate else { return Ok(()); };
        let Some(offset) = self.texture_eviction.backing_space.allocate(size, TEXTURE_BACKING_LIMIT) else { return Ok(()); };
        let backing = TextureBacking { offset, size, regions, evicted_at: 0 };
        let (buffer, memory) = match self.gpu.create_buffer("cold texture backup", size, vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::GpuToCpu) {
            Ok(pair) => pair,
            Err(error) => { self.texture_eviction.release_backing(&backing); self.texture_eviction.errors += 1; self.texture_eviction.failed = true; log_line(&format!("texture backup cancelled: {error}")); return Ok(()); }
        };
        let prepared = (|| -> Result<_, RendererError> {
            let cb = self.cb()?;
            self.flush_pending_writes(cb);
            let uses: Vec<_> = backing.regions.iter().map(|r| ImageUse { texture: id, mips: (r.image_subresource.mip_level, 1), layers: (r.image_subresource.base_array_layer, 1), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }).collect();
            self.transition(cb, &uses, false)?;
            let texture = self.texture(Opcode::CreateTexture, id)?;
            let (serial, generation) = (texture.use_serial, texture.generation);
            // All fallible preparation finishes before commands reference this buffer.
            unsafe {
                self.gpu.device.cmd_copy_image_to_buffer(cb, texture.raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buffer, &backing.regions);
                let barrier = [vk::MemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::HOST).dst_access_mask(vk::AccessFlags2::HOST_READ)];
                self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().memory_barriers(&barrier));
            }
            Ok((serial, generation, self.recording_value()))
        })();
        let (serial, generation, value) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                unsafe { self.gpu.device.destroy_buffer(buffer, Some(callbacks())) }; self.gpu.free(memory);
                self.texture_eviction.release_backing(&backing);
                return Err(error);
            }
        };
        self.texture_eviction.ticket += 1;
        let ticket = self.texture_eviction.ticket;
        if let Some(Resource::Texture(t)) = self.resources.get_mut(&id) { t.eviction_ticket = ticket; }
        self.texture_eviction.pending = Some(PendingEviction { id, ticket, serial, generation, backing, copy: EvictionCopy::Gpu { buffer, memory, value } });
        Ok(())
    }

    fn create_texture(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let texture = self.build_texture(p)?;
        self.insert(Opcode::CreateTexture, p[0], Resource::Texture(Box::new(texture)))
    }

    fn build_texture(&mut self, p: &[u32]) -> Result<TextureResource, RendererError> {
        let opcode = Opcode::CreateTexture;
        let dimension = match p[1] { 1 => vk::ImageType::TYPE_1D, 2 => vk::ImageType::TYPE_2D, 3 => vk::ImageType::TYPE_3D, value => return Err(self.command_error(opcode, format!("unsupported texture dimension {value}"))) };
        let width = p[2];
        let height = if p[1] == 1 { 1 } else { p[3] };
        let depth_or_layers = if p[1] == 3 { p[4] } else { p[6] };
        let (mip_levels, dxgi, bind, samples) = (p[5], p[7], p[8], p[9]);
        if width == 0 || height == 0 || depth_or_layers == 0 || width > MAX_TEXTURE_SIDE || height > MAX_TEXTURE_SIDE || mip_levels == 0 || mip_levels > 16 || !matches!(samples, 1 | 2 | 4 | 8) {
            return Err(self.command_error(opcode, "invalid texture extent, mip count, or sample count"));
        }
        let format = texture_format(dxgi, bind).ok_or_else(|| self.command_error(opcode, format!("DXGI format {dxgi} has no native mapping")))?;
        let info = format_info(format).ok_or_else(|| self.command_error(opcode, format!("{format:?} has no layout")))?;
        let mut usage = vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
        if bind & BIND_RENDER_TARGET != 0 { usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT; }
        if bind & BIND_DEPTH_STENCIL != 0 { usage |= vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT; }
        if bind & BIND_UAV != 0 { usage |= vk::ImageUsageFlags::STORAGE; }
        let layers = if dimension == vk::ImageType::TYPE_3D { 1 } else { depth_or_layers };
        let mut flags = vk::ImageCreateFlags::empty();
        let family = view_formats(dxgi, format);
        if !family.is_empty() { flags |= vk::ImageCreateFlags::MUTABLE_FORMAT; }
        if dimension == vk::ImageType::TYPE_2D && layers >= 6 && layers % 6 == 0 && width == height && samples == 1 { flags |= vk::ImageCreateFlags::CUBE_COMPATIBLE; }
        let mut list = vk::ImageFormatListCreateInfo::default().view_formats(family);
        let mut create = vk::ImageCreateInfo::default().image_type(dimension).format(format)
            .extent(vk::Extent3D { width, height, depth: if dimension == vk::ImageType::TYPE_3D { depth_or_layers } else { 1 } })
            .mip_levels(mip_levels).array_layers(layers).samples(vk::SampleCountFlags::from_raw(samples)).tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage).flags(flags).sharing_mode(vk::SharingMode::EXCLUSIVE).initial_layout(vk::ImageLayout::UNDEFINED);
        if !family.is_empty() { create = create.push_next(&mut list); }
        let raw = unsafe { self.gpu.device.create_image(&create, Some(callbacks())) }.map_err(|error| self.command_error(opcode, format!("vkCreateImage {format:?} {width}x{height}x{depth_or_layers} mips {mip_levels} usage {usage:?}: {error}")))?;
        let memory = if self.lazy_targets && bind & (BIND_RENDER_TARGET | BIND_DEPTH_STENCIL) != 0 {
            None
        } else {
            match self.bind_texture_memory(raw) {
                Ok(memory) => Some(memory),
                Err(error) => { unsafe { self.gpu.device.destroy_image(raw, Some(callbacks())) }; return Err(error); }
            }
        };
        let layouts = vec![vk::ImageLayout::UNDEFINED; (mip_levels * layers) as usize];
        let filterable = self.gpu.linear_filterable(format);
        Ok(TextureResource { raw, memory, format, views: family, info, dxgi, bind, dimension, width, height, depth_or_layers, mip_levels, samples, layouts, vertex_visible: true, last_use: 0, use_serial: 0, last_use_at: None, eviction_ticket: 0, backing: None, age_restored: false, generation: 0, filterable })
    }

    fn create_shader(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let stage = p[1];
        if stage > 2 { return Err(self.command_error(Opcode::CreateShader, format!("shader stage {stage} is outside vertex/pixel/compute"))); }
        let hash = format!("{:08x}{:08x}", p[3], p[2]);
        let signatures = (0..p[4] as usize).map(|i| [p[5 + i * 4], p[6 + i * 4], p[7 + i * 4], p[8 + i * 4]]).collect();
        // The engine creates unused variants too: compile only when a draw or dispatch uses one.
        self.insert(Opcode::CreateShader, p[0], Resource::Shader(ShaderResource { stage, hash, signatures, compiled: None }))
    }

    fn module(&self, spirv: &[u32]) -> Result<vk::ShaderModule, RendererError> {
        unsafe { self.gpu.device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(spirv), Some(callbacks())) }.map_err(vk_error("vkCreateShaderModule"))
    }

    fn ensure_shader(&mut self, id: u32, opcode: Opcode) -> Result<(), RendererError> {
        let (hash, stage) = match self.resources.get(&id) {
            Some(Resource::Shader(shader)) if shader.compiled.is_some() => return Ok(()),
            Some(Resource::Shader(shader)) => (shader.hash.clone(), shader.stage),
            Some(_) => return Err(self.command_error(opcode, format!("bound resource {id} is not a shader"))),
            None => return Err(self.command_error(opcode, format!("shader {id} does not exist"))),
        };
        let asset = match self.shader_store.as_mut() {
            Some(store) => store.load(&hash).map_err(|error| RendererError::Command { opcode, detail: format!("used shader {id}: {error}") })?,
            None => return Err(self.unsupported(opcode, "no native shader directory was configured")),
        };
        let crate::shader_cache::PreparedShader { source, reflection, pack, spirv } = self.shader_cache.prepare(&asset.source, stage)
            .map_err(|error| self.command_error(opcode, format!("{}: {error}", asset.label)))?;
        let module = self.module(&spirv)?;
        let constants = match asset.constants {
            Some(words) => {
                let bytes = crate::shader_constants::pack(&words).map_err(|error| self.command_error(opcode, error))?;
                if bytes.len() as u64 > u64::from(self.gpu.props.limits.max_uniform_buffer_range) { return Err(self.command_error(opcode, format!("shader {hash} constant table exceeds the uniform-buffer limit"))); }
                let (buffer, memory) = self.gpu.create_buffer("gta5 shader constant table", bytes.len() as u64, vk::BufferUsageFlags::UNIFORM_BUFFER, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?;
                let pointer = memory.mapped_ptr().ok_or_else(|| RendererError::Vulkan("constant table is not mapped".into()))?;
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.as_ptr() as *mut u8, bytes.len()) };
                Some((buffer, memory))
            }
            None => None,
        };
        let pack_size = pack.as_deref().unwrap_or_default().iter().map(|member| member.offset + member.size).max().unwrap_or(0);
        if pack_size > u64::from(self.gpu.props.limits.max_uniform_buffer_range) {
            return Err(self.command_error(opcode, format!("shader {hash} packs {pack_size} bytes of constants, above the device limit")));
        }
        if let Some(Resource::Shader(shader)) = self.resources.get_mut(&id) {
            shader.compiled = Some(Box::new(Compiled { label: asset.label.clone(), source: (stage != 1).then_some(source), reflection: Rc::new(reflection), module, constants, pack: pack.map(Rc::from), pack_size, variants: HashMap::new() }));
        }
        Ok(())
    }

    fn ensure_vertex_variant(&mut self, vs_id: u32, inputs: &[(u32, String)]) -> Result<vk::ShaderModule, RendererError> {
        let source = match self.resources.get(&vs_id) {
            Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => {
                if let Some(module) = compiled.variants.get(inputs) { return Ok(*module); }
                compiled.source.clone().ok_or_else(|| self.command_error(Opcode::Draw, "vertex shader source was not kept"))?
            }
            _ => return Err(self.command_error(Opcode::Draw, "vertex shader is not compiled")),
        };
        let has_pack = matches!(self.resources.get(&vs_id), Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) if compiled.pack.is_some());
        let source = crate::shader::vertex_for_fragment_inputs(&source, inputs).map_err(|error| self.command_error(Opcode::Draw, format!("vertex shader {vs_id}: {error}")))?;
        let spirv = self.shader_cache.compile(&source, 0, has_pack.then(|| pack_binding(0))).map_err(|error| self.command_error(Opcode::Draw, format!("vertex variant {vs_id}: {error}")))?;
        let module = self.module(&spirv)?;
        if let Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) = self.resources.get_mut(&vs_id) {
            compiled.variants.insert(inputs.to_vec(), module);
        }
        Ok(module)
    }

    fn destroy(&mut self, id: u32) -> Result<(), RendererError> {
        self.global_epoch += 1;
        self.vertex_inputs.retain(|(layout, shader), _| *layout != id && *shader != id);
        self.pending_clears.retain(|key, _| key.0 != id);
        let resource = self.resources.remove(&id).ok_or_else(|| self.command_error(Opcode::DestroyObject, format!("resource {id} does not exist")))?;
        let value = self.recording_value();
        match resource {
            Resource::Buffer(buffer) => {
                // A new constant buffer may reuse this id with the same upload count: packs keyed on it are stale.
                if buffer.shadow.is_some() { self.last_pack = [None, None, None]; }
                // A constant buffer no copy or readback touched never got a GPU copy.
                if let Some((counter, memory)) = buffer.counter {
                    self.garbage.buffer(value, buffer.raw, buffer.memory);
                    self.garbage.buffer(value, counter, memory);
                } else if buffer.raw != vk::Buffer::null() {
                    self.garbage.reusable(value, buffer.usage, buffer.size.next_multiple_of(4), buffer.raw, buffer.memory);
                }
            }
            Resource::Texture(texture) => {
                self.drop_texture_views(id, value);
                if let Some(backing) = texture.backing.as_ref() { self.texture_eviction.release_backing(backing); }
                if let Some(memory) = texture.memory {
                    self.garbage.push(value, Trash::Image(texture.raw, memory));
                } else {
                    unsafe { self.gpu.device.destroy_image(texture.raw, Some(callbacks())) };
                }
                if let Some((proxy, _)) = self.depth_proxies.remove(&id) { let _ = self.destroy(proxy); }
                if let Some((proxy, _)) = self.float_proxies.remove(&id) { let _ = self.destroy(proxy); }
            }
            Resource::Shader(shader) => {
                self.drop_pipelines_using(id, value);
                if let Some(compiled) = shader.compiled {
                    self.garbage.push(value, Trash::Module(compiled.module));
                    for module in compiled.variants.into_values() { self.garbage.push(value, Trash::Module(module)); }
                    if let Some((buffer, memory)) = compiled.constants { self.garbage.buffer(value, buffer, memory); }
                }
            }
            Resource::Layout(_) => self.drop_pipelines_using(id, value),
            Resource::State(state) => {
                self.drop_pipelines_using(id, value);
                if let Some(sampler) = state.sampler { self.garbage.push(value, Trash::Sampler(sampler)); }
                if let Some(twin) = self.nearest_samplers.remove(&id) { self.garbage.push(value, Trash::Sampler(twin)); }
            }
            Resource::View(_) | Resource::Query(_) => {}
        }
        Ok(())
    }

    fn drop_texture_views(&mut self, id: u32, value: u64) {
        let garbage = &mut self.garbage;
        self.views.retain(|(texture, _), view| { if *texture == id { garbage.push(value, Trash::View(*view)); false } else { true } });
    }

    fn drop_pipelines_using(&mut self, id: u32, value: u64) {
        let garbage = &mut self.garbage;
        self.pipelines.retain(|key, pipeline| { if key.uses(id) { garbage.push(value, Trash::Pipeline(pipeline.raw)); false } else { true } });
        self.compute_pipelines.retain(|(shader, _), pipeline| {
            if *shader == id {
                garbage.push(value, Trash::Pipeline(pipeline.raw));
                if let Some((_, module)) = pipeline.specialized { garbage.push(value, Trash::Module(module)); }
                false
            } else { true }
        });
    }

    // ---- recording ---------------------------------------------------------------------------

    fn slot_mut(&mut self) -> &mut Slot { &mut self.slots[self.slot] }

    fn command_buffer(&mut self) -> Result<vk::CommandBuffer, RendererError> {
        let device = self.gpu.device.clone();
        let slot = self.slot_mut();
        if slot.used == slot.buffers.len() {
            let info = vk::CommandBufferAllocateInfo::default().command_pool(slot.pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1);
            let buffer = unsafe { device.allocate_command_buffers(&info) }.map_err(vk_error("vkAllocateCommandBuffers"))?[0];
            slot.buffers.push(buffer);
        }
        let buffer = slot.buffers[slot.used];
        slot.used += 1;
        unsafe { device.begin_command_buffer(buffer, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)) }.map_err(vk_error("vkBeginCommandBuffer"))?;
        Ok(buffer)
    }

    /// The open recording's command buffer, outside any rendering instance.
    fn cb(&mut self) -> Result<vk::CommandBuffer, RendererError> {
        self.end_pass();
        self.recording_cb()
    }

    fn recording_cb(&mut self) -> Result<vk::CommandBuffer, RendererError> {
        if self.recording.is_none() {
            let cb = self.command_buffer()?;
            self.recording = Some(Recording { cb, pre: None, pre_copies: Vec::new(), pass: None, bound: Bound::default(), open_query: None, pending: (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE), after_submit: Vec::new(), profiled: None });
        }
        Ok(self.recording.as_ref().expect("recording").cb)
    }

    /// The command buffer that runs before the open recording in the same submit.
    fn pre_cb(&mut self) -> Result<vk::CommandBuffer, RendererError> {
        self.recording_cb()?;
        if let Some(pre) = self.recording.as_ref().and_then(|recording| recording.pre) { return Ok(pre); }
        let pre = self.command_buffer()?;
        // Writes here must wait for earlier submissions still reading the same resources.
        let barrier = [vk::MemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS).dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)];
        unsafe { self.gpu.device.cmd_pipeline_barrier2(pre, &vk::DependencyInfo::default().memory_barriers(&barrier)) };
        self.recording.as_mut().expect("recording").pre = Some(pre);
        Ok(pre)
    }

    fn end_pass(&mut self) {
        let Some(recording) = self.recording.as_mut() else { return };
        if recording.pass.take().is_some() {
            if let Some(slot) = recording.open_query.take() {
                let (pool, index) = self.occlusion.location(slot);
                unsafe { self.gpu.device.cmd_end_query(recording.cb, pool, index) };
            }
            unsafe { self.gpu.device.cmd_end_rendering(recording.cb) };
            recording.bound = Bound::default();
            if let (Some(index), Some(profile)) = (recording.profiled.take(), self.slots[self.slot].profile.as_ref()) {
                unsafe { self.gpu.device.cmd_write_timestamp2(recording.cb, vk::PipelineStageFlags2::ALL_COMMANDS, profile.pool, index * 2 + 1) };
            }
        }
    }

    /// Makes earlier buffer and storage writes visible before a command that reads them.
    fn flush_pending_writes(&mut self, cb: vk::CommandBuffer) {
        let Some(recording) = self.recording.as_mut() else { return };
        if recording.pending.0 == vk::PipelineStageFlags2::NONE { return; }
        let barrier = [vk::MemoryBarrier2::default().src_stage_mask(recording.pending.0).src_access_mask(recording.pending.1)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS).dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)];
        unsafe { self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().memory_barriers(&barrier)) };
        recording.pending = (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE);
        self.stats.barriers += 1;
    }

    fn note_pending_write(&mut self, stage: vk::PipelineStageFlags2, access: vk::AccessFlags2) {
        if let Some(recording) = self.recording.as_mut() { recording.pending.0 |= stage; recording.pending.1 |= access; }
    }

    /// Transitions each subresource of `uses` into its layout, in `cb` (outside rendering).
    fn transition(&mut self, cb: vk::CommandBuffer, uses: &[ImageUse], force_write_sync: bool) -> Result<(), RendererError> {
        for usage in uses { self.ensure_texture_memory(usage.texture)?; }
        self.zero_unwritten(cb, uses)?;
        let value = self.recording_value();
        let exact = *EXACT_BARRIERS.get_or_init(|| debug_switch("exact_barriers", false));
        let mut barriers: Vec<vk::ImageMemoryBarrier2> = Vec::new();
        for usage in uses {
            let Some(Resource::Texture(texture)) = self.resources.get_mut(&usage.texture) else { continue };
            if exact && usage.stages.contains(vk::PipelineStageFlags2::VERTEX_SHADER) && !texture.vertex_visible {
                // Every subresource already in a read layout becomes visible to the vertex stage.
                let aspect = texture.aspect();
                for (index, layout) in texture.layouts.iter().enumerate() {
                    let (stage, access) = layout_scope(*layout);
                    if access & vk::AccessFlags2::SHADER_SAMPLED_READ == vk::AccessFlags2::NONE { continue; }
                    let (mip, layer) = (index as u32 % texture.mip_levels, index as u32 / texture.mip_levels);
                    barriers.push(vk::ImageMemoryBarrier2::default().src_stage_mask(stage & !vk::PipelineStageFlags2::VERTEX_SHADER).dst_stage_mask(vk::PipelineStageFlags2::VERTEX_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ).old_layout(*layout).new_layout(*layout).image(texture.raw)
                        .subresource_range(vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: mip, level_count: 1, base_array_layer: layer, layer_count: 1 }));
                }
                texture.vertex_visible = true;
            }
            texture.last_use = value;
            texture.use_serial += 1;
            texture.last_use_at = self.texture_eviction.clock;
            let aspect = texture.aspect();
            for layer in usage.layers.0..usage.layers.0 + usage.layers.1 {
                for mip in usage.mips.0..usage.mips.0 + usage.mips.1 {
                    let index = (mip + texture.mip_levels * layer) as usize;
                    let old = texture.layouts[index];
                    let writes = matches!(old, vk::ImageLayout::TRANSFER_DST_OPTIMAL | vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL | vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL | vk::ImageLayout::GENERAL
                        | vk::ImageLayout::DEPTH_READ_ONLY_STENCIL_ATTACHMENT_OPTIMAL | vk::ImageLayout::DEPTH_ATTACHMENT_STENCIL_READ_ONLY_OPTIMAL);
                    // A transfer destination is written again even if its layout stays the same.
                    if old == usage.layout && !(force_write_sync && writes) && old != vk::ImageLayout::GENERAL && old != vk::ImageLayout::TRANSFER_DST_OPTIMAL { continue; }
                    if old == usage.layout && old == vk::ImageLayout::GENERAL && !force_write_sync { continue; }
                    let (src_stage, src_access) = layout_scope(old);
                    let (mut dst_stage, dst_access) = layout_scope(usage.layout);
                    if *FRAG_READ_BARRIER.get_or_init(|| debug_switch("frag_read_barrier", false)) && dst_access == vk::AccessFlags2::SHADER_SAMPLED_READ { dst_stage &= !vk::PipelineStageFlags2::VERTEX_SHADER; }
                    if exact && dst_access.contains(vk::AccessFlags2::SHADER_SAMPLED_READ) && !usage.stages.contains(vk::PipelineStageFlags2::VERTEX_SHADER) {
                        dst_stage &= !vk::PipelineStageFlags2::VERTEX_SHADER;
                        texture.vertex_visible = false;
                    }
                    barriers.push(vk::ImageMemoryBarrier2::default().src_stage_mask(src_stage).src_access_mask(src_access).dst_stage_mask(dst_stage).dst_access_mask(dst_access)
                        .old_layout(old).new_layout(usage.layout).image(texture.raw)
                        .subresource_range(vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: mip, level_count: 1, base_array_layer: layer, layer_count: 1 }));
                    texture.layouts[index] = usage.layout;
                }
            }
        }
        if barriers.is_empty() { return Ok(()); }
        unsafe { self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().image_memory_barriers(&barriers)) };
        self.stats.barriers += 1;
        Ok(())
    }

    /// Clears never-written subresources (still UNDEFINED) to zero before their first read, as WebGPU
    /// and D3D-on-WebGPU did: Vulkan memory may hold another resource's old bytes. A subresource that
    /// is about to be uploaded whole (TRANSFER_DST) or loaded with a pending clear needs no fill.
    fn zero_unwritten(&mut self, cb: vk::CommandBuffer, uses: &[ImageUse]) -> Result<(), RendererError> {
        let mut fills: Vec<(u32, u32, u32)> = Vec::new();
        for usage in uses {
            if usage.layout == vk::ImageLayout::TRANSFER_DST_OPTIMAL { continue; }
            let Some(Resource::Texture(texture)) = self.resources.get(&usage.texture) else { continue };
            for layer in usage.layers.0..usage.layers.0 + usage.layers.1 {
                for mip in usage.mips.0..usage.mips.0 + usage.mips.1 {
                    if texture.layouts[(mip + texture.mip_levels * layer) as usize] != vk::ImageLayout::UNDEFINED { continue; }
                    if self.pending_clears.contains_key(&(usage.texture, mip, layer)) { continue; }
                    fills.push((usage.texture, mip, layer));
                }
            }
        }
        for (texture, mip, layer) in fills { self.zero_fill(cb, texture, mip, layer)?; }
        Ok(())
    }

    fn zero_fill(&mut self, cb: vk::CommandBuffer, id: u32, mip: u32, layer: u32) -> Result<(), RendererError> {
        self.ensure_texture_memory(id)?;
        let Some(Resource::Texture(texture)) = self.resources.get_mut(&id) else { unreachable!("validated texture") };
        let index = (mip + texture.mip_levels * layer) as usize;
        // Allocating an evicted image restores its saved subresources first.
        // Those pixels are initialized, even if the caller saw UNDEFINED before restoration.
        if texture.layouts[index] != vk::ImageLayout::UNDEFINED { return Ok(()); }
        let aspect = texture.aspect();
        let range = vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: mip, level_count: 1, base_array_layer: layer, layer_count: 1 };
        let barrier = [vk::ImageMemoryBarrier2::default().dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED).new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).image(texture.raw).subresource_range(range)];
        let device = &self.gpu.device;
        unsafe {
            device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().image_memory_barriers(&barrier));
            if texture.info.depth {
                device.cmd_clear_depth_stencil_image(cb, texture.raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearDepthStencilValue { depth: 0.0, stencil: 0 }, &[range]);
            } else if !texture.info.bc {
                device.cmd_clear_color_image(cb, texture.raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearColorValue { uint32: [0; 4] }, &[range]);
            }
        }
        texture.layouts[index] = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
        self.stats.zero_fills += 1;
        if texture.info.bc { self.zero_fill_compressed(cb, id, mip, layer); }
        Ok(())
    }

    /// Compressed formats cannot be cleared: copy zero blocks from staging (all-zero BC blocks decode to black).
    fn zero_fill_compressed(&mut self, cb: vk::CommandBuffer, id: u32, mip: u32, layer: u32) {
        let Some(Resource::Texture(texture)) = self.resources.get(&id) else { return };
        let (width, height, depth) = texture.mip_extent(mip);
        let info = texture.info;
        let raw = texture.raw;
        let bytes = u64::from(width.div_ceil(info.block_width)) * u64::from(height.div_ceil(info.block_height)) * u64::from(depth) * u64::from(info.block_bytes);
        let value = self.recording_value();
        let Ok((staging, at, pointer)) = self.staging.alloc(&mut self.gpu, bytes, 16, value) else { return };
        unsafe { std::ptr::write_bytes(pointer, 0, bytes as usize) };
        let region = vk::BufferImageCopy { buffer_offset: at, buffer_row_length: 0, buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: mip, base_array_layer: layer, layer_count: 1 },
            image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width, height, depth } };
        unsafe { self.gpu.device.cmd_copy_buffer_to_image(cb, staging, raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]) };
    }

    /// Whether any subresource of `uses` is not yet in its layout.
    fn needs_transition(&self, uses: &[ImageUse]) -> bool {
        uses.iter().any(|usage| match self.resources.get(&usage.texture) {
            Some(Resource::Texture(texture)) => (usage.layers.0..usage.layers.0 + usage.layers.1).any(|layer| (usage.mips.0..usage.mips.0 + usage.mips.1)
                .any(|mip| texture.layouts[(mip + texture.mip_levels * layer) as usize] != usage.layout)),
            _ => false,
        })
    }

    fn mark_written(&mut self, id: u32) {
        if self.depth_proxies.contains_key(&id) || self.float_proxies.contains_key(&id) { self.global_epoch += 1; }
        if let Some(Resource::Texture(texture)) = self.resources.get_mut(&id) { texture.generation += 1; }
    }

    /// Submits the recorded work. `present` adds the swapchain wait/signal semaphores.
    fn submit(&mut self, reason: Opcode, present: Option<(vk::Semaphore, vk::Semaphore)>) -> Result<(), RendererError> {
        if self.recording.is_none() && present.is_none() { return Ok(()); }
        self.recording_cb()?;
        self.end_pass();
        let started = Instant::now();
        let copies = std::mem::take(&mut self.recording.as_mut().expect("recording").pre_copies);
        if !copies.is_empty() {
            let pre = self.pre_cb()?;
            for group in copies.chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
                let regions: Vec<vk::BufferCopy> = group.iter().map(|copy| copy.2).collect();
                unsafe { self.gpu.device.cmd_copy_buffer(pre, group[0].0, group[0].1, &regions) };
            }
        }
        let recording = self.recording.take().expect("recording");
        let device = &self.gpu.device;
        if let Some(pre) = recording.pre {
            let barrier = [vk::MemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS).dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)];
            unsafe { device.cmd_pipeline_barrier2(pre, &vk::DependencyInfo::default().memory_barriers(&barrier)) };
            unsafe { device.end_command_buffer(pre) }.map_err(vk_error("vkEndCommandBuffer"))?;
        }
        unsafe { device.end_command_buffer(recording.cb) }.map_err(vk_error("vkEndCommandBuffer"))?;
        let value = self.gpu.submitted + 1;
        let buffers: Vec<vk::CommandBufferSubmitInfo> = recording.pre.into_iter().chain([recording.cb]).map(|cb| vk::CommandBufferSubmitInfo::default().command_buffer(cb)).collect();
        let mut signals = vec![vk::SemaphoreSubmitInfo::default().semaphore(self.gpu.timeline).value(value).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let mut waits = Vec::new();
        if let Some((acquired, rendered)) = present {
            waits.push(vk::SemaphoreSubmitInfo::default().semaphore(acquired).stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER));
            signals.push(vk::SemaphoreSubmitInfo::default().semaphore(rendered).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS));
        }
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&buffers).wait_semaphore_infos(&waits).signal_semaphore_infos(&signals)];
        unsafe { device.queue_submit2(self.gpu.queue, &submit, vk::Fence::null()) }.map_err(vk_error("vkQueueSubmit2"))?;
        self.gpu.submitted = value;
        let slot = &mut self.slots[self.slot];
        slot.last = value;
        slot.submits += 1;
        let queued = Instant::now();
        for completion in recording.after_submit { self.completions.push_back(Pending { value, completion, queued }); }
        *self.stats.flush_reasons.entry(reason as u32).or_default() += 1;
        self.stats.submits += 1;
        self.stats.submit += started.elapsed();
        if present.is_none() && self.slots[self.slot].submits >= SUBMITS_PER_SLOT { self.rotate_slot()?; }
        Ok(())
    }

    /// Moves to the next command slot, waiting until the GPU finished its previous use.
    fn rotate_slot(&mut self) -> Result<(), RendererError> {
        self.slot = (self.slot + 1) % self.frames_in_flight;
        let last = self.slots[self.slot].last;
        let waited = Instant::now();
        self.gpu.wait_for(last, GPU_WAIT_TIMEOUT).map_err(RendererError::Vulkan)?;
        self.stats.gpu_wait += waited.elapsed();
        let device = self.gpu.device.clone();
        if let Some(profile) = self.slots[self.slot].profile.as_mut() {
            if !profile.keys.is_empty() {
                let count = profile.keys.len() as u32 * 2;
                let mut stamps = vec![0u64; count as usize];
                let period = f64::from(self.gpu.props.limits.timestamp_period);
                if unsafe { device.get_query_pool_results(profile.pool, 0, &mut stamps, vk::QueryResultFlags::TYPE_64) }.is_ok() {
                    for (i, key) in profile.keys.drain(..).enumerate() {
                        let ns = (stamps[2 * i + 1].saturating_sub(stamps[2 * i]) as f64 * period) as u64;
                        let entry = self.pass_times.entry(key).or_insert((0, 0));
                        entry.0 += ns;
                        entry.1 += 1;
                    }
                    self.pass_frames += 1;
                }
                profile.keys.clear();
                unsafe { device.reset_query_pool(profile.pool, 0, count) };
            }
        }
        let slot = &mut self.slots[self.slot];
        unsafe { device.reset_command_pool(slot.pool, vk::CommandPoolResetFlags::empty()) }.map_err(vk_error("vkResetCommandPool"))?;
        slot.used = 0;
        for pool in &slot.descriptor_pools { unsafe { device.reset_descriptor_pool(*pool, vk::DescriptorPoolResetFlags::empty()) }.map_err(vk_error("vkResetDescriptorPool"))?; }
        slot.descriptor_pool = 0;
        slot.sets.clear();
        slot.pack_sets.clear();
        slot.submits = 0;
        self.global_epoch += 1;
        self.last_pack = [None, None, None];
        self.gpu.refresh_completed();
        self.garbage.collect(&mut self.gpu, false);
        self.uniforms.trim(&mut self.gpu, &mut self.garbage);
        self.staging.trim(&mut self.gpu, &mut self.garbage);
        self.texture_eviction.rotations += 1;
        self.texture_eviction.clock = Some(Instant::now());
        Ok(())
    }

    fn after_recorded_work(&mut self, completion: Completion) {
        match self.recording.as_mut() {
            Some(recording) => recording.after_submit.push(completion),
            None => self.completions.push_back(Pending { value: self.gpu.submitted, completion, queued: Instant::now() }),
        }
    }

    /// Publishes GPU results whose submission finished, in order.
    pub fn complete<M: SharedMemory + ?Sized>(&mut self, memory: &M) -> Result<(), RendererError> {
        if self.completions.is_empty() { return Ok(()); }
        let completed = self.gpu.refresh_completed();
        while self.completions.front().is_some_and(|pending| pending.value <= completed) {
            let pending = self.completions.pop_front().expect("front");
            self.publish(pending.completion, memory)?;
        }
        Ok(())
    }

    fn publish<M: SharedMemory + ?Sized>(&mut self, completion: Completion, memory: &M) -> Result<(), RendererError> {
        let write_result = |address: usize, word: u32, result: Option<u64>| -> Result<(), RendererError> {
            if let Some(result) = result {
                memory.write(address + 8, &(result as u32).to_le_bytes())?;
                memory.write(address + 12, &((result >> 32) as u32).to_le_bytes())?;
            }
            memory.write(address, &word.to_le_bytes())?;
            memory.notify_u32(address, u32::MAX)?;
            Ok(())
        };
        match completion {
            Completion::Word { address, word, result } => write_result(address, word, result),
            Completion::Readback { buffer, memory: allocation, copy } => {
                let mapped = allocation.mapped_slice().ok_or_else(|| RendererError::Vulkan("readback buffer is not mapped".into()))?;
                let result = copy.publish(mapped, memory);
                self.garbage.buffer(0, buffer, allocation);
                result
            }
            Completion::Query { slots, timestamp, address, seq } => {
                let queries = if timestamp { &mut self.timestamps } else { &mut self.occlusion };
                let mut total = 0u64;
                for slot in slots {
                    let (pool, index) = queries.location(slot);
                    let mut value = [0u64; 1];
                    unsafe { self.gpu.device.get_query_pool_results(pool, index, &mut value, vk::QueryResultFlags::TYPE_64) }.map_err(vk_error("vkGetQueryPoolResults"))?;
                    queries.release(&self.gpu.device, slot);
                    total = if timestamp { value[0] } else { total | value[0] };
                }
                let result = if timestamp {
                    (total as f64 * f64::from(self.gpu.props.limits.timestamp_period)).round() as u64
                } else if total != 0 { 65_536 } else { 0 };
                write_result(address, seq, Some(result))
            }
        }
    }

    // ---- constants ---------------------------------------------------------------------------

    /// Creates constant buffer `id`'s GPU copy on first use and brings it up to date before a copy or readback reads it.
    fn sync_constant(&mut self, id: u32) -> Result<(), RendererError> {
        let Some(Resource::Buffer(buffer)) = self.resources.get(&id) else { return Ok(()) };
        if buffer.raw == vk::Buffer::null() {
            let (usage, aligned) = (buffer.usage, buffer.size.next_multiple_of(4));
            let (raw, memory) = match self.garbage.take(usage, aligned) {
                Some(reused) => reused,
                None => self.gpu.create_buffer("gta5 buffer", aligned, usage, MemoryLocation::CpuToGpu).map_err(RendererError::Vulkan)?,
            };
            if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) { buffer.raw = raw; buffer.memory = memory; buffer.stale = true; }
        }
        let Some(Resource::Buffer(buffer)) = self.resources.get(&id) else { return Ok(()) };
        if !buffer.stale { return Ok(()); }
        let Some(shadow) = buffer.shadow.clone() else { return Ok(()) };
        self.write_buffer(id, 0, &shadow, false)?;
        if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) { buffer.stale = false; }
        Ok(())
    }

    /// The bound D3D constant buffers of `stage` copied back to back into the uniform ring.
    fn pack_constants(&mut self, stage: usize, members: &[PackMember], size: u64) -> Result<(vk::Buffer, u64, u64), RendererError> {
        let bindings = self.state.constants[stage];
        // The block's layout (each member's binding, offset and size) is part of the key: shaders
        // bound to the same buffers may declare them with different sizes.
        let key_of = |resources: &FastMap<u32, Resource>, member: &PackMember| -> PackKey {
            let state = bindings.get(member.binding as usize).copied().unwrap_or_default();
            let generation = match resources.get(&state.buffer) { Some(Resource::Buffer(buffer)) => buffer.generation, _ => 0 };
            (member.binding, member.offset, member.size, state.buffer, generation, state.first, state.num)
        };
        if let Some((last, buffer, offset, last_size)) = &self.last_pack[stage] {
            if !self.dumping && *last_size == size && last.len() == members.len() && members.iter().zip(last).all(|(member, key)| key_of(&self.resources, member) == *key) {
                let (buffer, offset) = (*buffer, *offset);
                let value = self.recording_value();
                self.uniforms.touch(buffer, value);
                return Ok((buffer, offset, size));
            }
        }
        let mut key = self.last_pack[stage].take().map(|(key, ..)| key).unwrap_or_default();
        key.clear();
        key.extend(members.iter().map(|member| key_of(&self.resources, member)));
        let align = self.gpu.props.limits.min_uniform_buffer_offset_alignment.max(16);
        let value = self.recording_value();
        let (buffer, offset, pointer) = self.uniforms.alloc(&mut self.gpu, size, align, value)?;
        // SAFETY: the ring handed out `size` mapped bytes at `pointer` for this submission only.
        let block = unsafe { std::slice::from_raw_parts_mut(pointer, size as usize) };
        block.fill(0);
        for member in members {
            let Some(state) = bindings.get(member.binding as usize) else { continue };
            match self.resources.get(&state.buffer) {
                Some(Resource::Buffer(buffer)) if buffer.shadow.is_some() => {
                    let shadow = buffer.shadow.as_deref().unwrap_or_default();
                    let range = constant_range(shadow.len(), state.first, state.num, member.size as usize);
                    let at = member.offset as usize;
                    block[at..at + range.len()].copy_from_slice(&shadow[range]);
                }
                Some(Resource::Buffer(_)) => return Err(self.command_error(Opcode::Draw, format!("buffer {} is bound as constants but was not created as a constant buffer", state.buffer))),
                _ => {}
            }
        }
        if self.dumping {
            let floats: Vec<String> = block.chunks_exact(4).take(48).map(|c| format!("{:.3}", f32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect();
            self.dump_line(&format!("dump pack {} stage {stage}: members {:?} [{}]", self.stats.draws, members.iter().map(|m| (m.binding, m.offset, m.size)).collect::<Vec<_>>(), floats.join(",")));
        }
        self.stats.packs_written += 1;
        self.last_pack[stage] = Some((key, buffer, offset, size));
        Ok((buffer, offset, size))
    }

    // ---- uploads -----------------------------------------------------------------------------

    /// Writes bytes into a buffer with the browser worker's queue.writeBuffer ordering, which the engine was
    /// built against: the write lands after every submitted command and before the open recording runs.
    /// An ordered write (`no_overwrite` false) also keeps the old bytes for commands already recorded.
    fn write_buffer(&mut self, id: u32, offset: u64, data: &[u8], no_overwrite: bool) -> Result<(), RendererError> {
        if data.is_empty() { return Ok(()); }
        // May submit: the recording value below must be read after it.
        self.limit_staging()?;
        let recording = self.recording_value();
        if self.buffer(Opcode::UploadBuffer, id)?.submitted_before(recording) > self.gpu.completed { self.gpu.refresh_completed(); }
        let buffer = self.buffer(Opcode::UploadBuffer, id)?;
        let (raw, mapped) = (buffer.raw, buffer.memory.mapped_ptr());
        let placement = WritePlacement::of(buffer, no_overwrite, recording, self.gpu.completed);
        if placement == WritePlacement::InPlace {
            // No GPU work can see the old bytes: write in place (memory is unified and host-coherent).
            let pointer = mapped.ok_or_else(|| RendererError::Vulkan("buffer is not host mapped".into()))?;
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), (pointer.as_ptr() as *mut u8).add(offset as usize), data.len()) };
            return Ok(());
        }
        let (staging, at, pointer) = self.staging.alloc(&mut self.gpu, data.len() as u64, 4, recording)?;
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len()) };
        let copy = vk::BufferCopy { src_offset: at, dst_offset: offset, size: data.len() as u64 };
        let in_order = placement == WritePlacement::InOrder;
        if in_order {
            let cb = self.cb()?;
            self.flush_pending_writes(cb);
            unsafe { self.gpu.device.cmd_copy_buffer(cb, staging, raw, &[copy]) };
            self.note_pending_write(vk::PipelineStageFlags2::ALL_TRANSFER, vk::AccessFlags2::TRANSFER_WRITE);
        } else {
            self.recording_cb()?;
            let copies = &mut self.recording.as_mut().expect("recording").pre_copies;
            match copies.last_mut() {
                Some((s, d, last)) if (*s, *d) == (staging, raw) && last.src_offset + last.size == at && last.dst_offset + last.size == offset => last.size += copy.size,
                _ => copies.push((staging, raw, copy)),
            }
        }
        self.stats.staged_writes += 1;
        if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) {
            if in_order { buffer.cb_write = recording; } else { buffer.pre_write = recording; }
            buffer.touch(recording);
        }
        Ok(())
    }

    /// Bounds staging memory during long uploads: wait for the oldest submissions first.
    fn limit_staging(&mut self) -> Result<(), RendererError> {
        if self.staging.in_flight(self.gpu.completed) < STAGING_IN_FLIGHT { return Ok(()); }
        self.gpu.refresh_completed();
        if self.staging.in_flight(self.gpu.completed) < STAGING_IN_FLIGHT { return Ok(()); }
        self.submit(Opcode::UploadTexture, None)?;
        let target = self.gpu.submitted;
        self.gpu.wait_for(target, GPU_WAIT_TIMEOUT).map_err(RendererError::Vulkan)

    }

    fn upload_buffer(&mut self, p: &[u32], overwrite: bool) -> Result<(), RendererError> {
        let (id, offset, size) = (p[0], u64::from(p[1]), p[2] as usize);
        let data = words_as_bytes(&p[3..], size);
        let buffer = self.buffer(Opcode::UploadBuffer, id)?;
        if data.len() != size || offset.checked_add(size as u64).is_none_or(|end| end > buffer.size) { return Err(self.command_error(Opcode::UploadBuffer, "upload exceeds buffer")); }
        if buffer.shadow.is_some() {
            if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) {
                buffer.shadow.as_mut().expect("shadow")[offset as usize..offset as usize + size].copy_from_slice(data);
                buffer.generation += 1;
                buffer.stale = true;
            }
            return Ok(());
        }
        self.write_buffer(id, offset, data, !overwrite)
    }

    fn upload_texture(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::UploadTexture;
        let id = p[0];
        let (mip, slice, x, y, z, width, height, depth, bytes_per_row, rows_per_image, size) = (p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9], p[10], p[11] as usize);
        let data = words_as_bytes(&p[12..], size);
        let texture = self.texture(opcode, id)?;
        if mip >= texture.mip_levels || width == 0 || height == 0 || depth == 0 || bytes_per_row == 0 || rows_per_image == 0 {
            return Err(self.command_error(opcode, "upload region is outside texture"));
        }
        let (mip_width, mip_height, mip_depth) = texture.mip_extent(mip);
        let is_3d = texture.dimension == vk::ImageType::TYPE_3D;
        let mip_depth = if is_3d { mip_depth } else { texture.depth_or_layers };
        let z_origin = if is_3d { z } else { slice };
        let info = texture.info;
        crate::texture_upload::validate_upload([mip_width, mip_height, mip_depth], [x, y, z_origin], [width, height, depth],
            [info.block_bytes, info.block_width, info.block_height], bytes_per_row, rows_per_image, data.len())
            .map_err(|error| self.command_error(opcode, format!("{error}: id={id} format={:?} mip={mip} extent={mip_width}x{mip_height}x{mip_depth} origin={x},{y},{z_origin} copy={width}x{height}x{depth} row={bytes_per_row} rows={rows_per_image} bytes={size}", texture.format)))?;
        let whole = x == 0 && y == 0 && z == 0 && width >= mip_width && height >= mip_height && (!is_3d || depth >= mip_depth);
        // Depth uploads do not replace stencil; its queued clear must still execute.
        let full_image = !info.depth && whole && mip == 0 && texture.mip_levels == 1 && (is_3d || (slice == 0 && depth == texture.layers()));
        if !full_image { self.materialize_clears(id)?; }
        // A partial update must observe the clears' current submission before choosing its command buffer.
        let texture = self.texture(opcode, id)?;
        let (raw, aspect, last_use) = (texture.raw, texture.aspect(), texture.last_use);
        // May submit: the recording value below must be read after it.
        self.limit_staging()?;
        let recording = self.recording_value();
        let (staging, at, pointer) = self.staging.alloc(&mut self.gpu, data.len() as u64, 16, recording)?;
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len()) };
        let region = vk::BufferImageCopy {
            buffer_offset: at,
            buffer_row_length: bytes_per_row / info.block_bytes * info.block_width,
            buffer_image_height: rows_per_image * info.block_height,
            image_subresource: vk::ImageSubresourceLayers { aspect_mask: if info.depth { vk::ImageAspectFlags::DEPTH } else { aspect }, mip_level: mip, base_array_layer: if is_3d { 0 } else { slice }, layer_count: if is_3d { 1 } else { depth } },
            image_offset: vk::Offset3D { x: x as i32, y: y as i32, z: if is_3d { z as i32 } else { 0 } },
            // Compressed mip tails copy whole blocks; the image region stops at the mip edge.
            image_extent: vk::Extent3D { width: width.min(mip_width - x), height: height.min(mip_height - y), depth: if is_3d { depth } else { 1 } },
        };
        let in_order = last_use == recording;
        let cb = if in_order { self.cb()? } else { self.pre_cb()? };
        let layers = if is_3d { (0, 1) } else { (slice, depth) };
        if full_image {
            // Staging and command-buffer allocation succeeded; retain backing/clears if image allocation fails.
            self.overwrite_texture(id)?;
            self.pending_clears.retain(|key, _| key.0 != id);
        }
        if !whole {
            for layer in layers.0..layers.0 + layers.1 {
                let undefined = matches!(self.resources.get(&id), Some(Resource::Texture(t)) if t.layouts[(mip + t.mip_levels * layer) as usize] == vk::ImageLayout::UNDEFINED);
                if undefined { self.zero_fill(cb, id, mip, layer)?; }
            }
        }
        let target = [ImageUse { texture: id, mips: (mip, 1), layers, layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }];
        // Restoration or a pending clear may have written this destination in the same layout.
        self.transition(cb, &target, true)?;
        unsafe { self.gpu.device.cmd_copy_buffer_to_image(cb, staging, raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]) };
        let ready = [ImageUse { layout: if info.depth { vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL } else { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL }, ..target[0] }];
        self.transition(cb, &ready, false)?;
        self.mark_written(id);
        Ok(())
    }

    // ---- clears --------------------------------------------------------------------------------

    /// D3D clears become the next rendering instance's load operation on that subresource.
    fn clear_rt(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let texture = self.texture(Opcode::ClearRt, p[0])?;
        if texture.dimension == vk::ImageType::TYPE_3D { return Err(self.unsupported(Opcode::ClearRt, "3D render-target view translation")); }
        if texture.info.depth || p[1] >= texture.mip_levels || p[2] >= texture.depth_or_layers { return Err(self.command_error(Opcode::ClearRt, "render-target subresource is outside a color texture")); }
        self.end_pass_using(p[0]);
        self.global_epoch += 1;
        self.pending_clears.insert((p[0], p[1], p[2]), PendingClear { color: Some([f32::from_bits(p[3]), f32::from_bits(p[4]), f32::from_bits(p[5]), f32::from_bits(p[6])]), depth: None, stencil: None });
        Ok(())
    }

    fn clear_ds(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let texture = self.texture(Opcode::ClearDs, p[0])?;
        if !texture.info.depth || p[1] >= texture.mip_levels || p[2] >= texture.depth_or_layers { return Err(self.command_error(Opcode::ClearDs, "depth/stencil subresource is outside the texture")); }
        let stencil = texture.info.stencil;
        self.end_pass_using(p[0]);
        self.global_epoch += 1;
        let pending = self.pending_clears.entry((p[0], p[1], p[2])).or_default();
        if p[3] & 1 != 0 { pending.depth = Some(f32::from_bits(p[4])); }
        if p[3] & 2 != 0 && stencil { pending.stencil = Some(p[5]); }
        Ok(())
    }

    fn end_pass_using(&mut self, texture: u32) {
        let uses = self.recording.as_ref().and_then(|recording| recording.pass.as_ref())
            .is_some_and(|key| key.colors.iter().flatten().any(|color| color.0 == texture) || key.depth.is_some_and(|depth| depth.0 == texture));
        if uses { self.end_pass(); }
    }

    /// Records the pending clears of `texture` before something reads, copies or uploads it.
    fn materialize_clears(&mut self, texture: u32) -> Result<(), RendererError> {
        if !self.pending_clears.keys().any(|key| key.0 == texture) { return Ok(()); }
        // Check full-clear coverage before removing the queued clear operations.
        self.ensure_texture_memory(texture)?;
        let mut pending = Vec::new();
        self.pending_clears.retain(|key, clear| { if key.0 == texture { pending.push((*key, *clear)); false } else { true } });
        let cb = self.cb()?;
        for ((_, mip, slice), clear) in pending {
            let usage = [ImageUse { texture, mips: (mip, 1), layers: (slice, 1), layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }];
            self.transition(cb, &usage, false)?;
            let Some(Resource::Texture(resource)) = self.resources.get(&texture) else { continue };
            let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: mip, level_count: 1, base_array_layer: slice, layer_count: 1 };
            unsafe {
                if let Some(color) = clear.color {
                    self.gpu.device.cmd_clear_color_image(cb, resource.raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearColorValue { float32: color }, &[range]);
                } else {
                    let mut aspect = vk::ImageAspectFlags::empty();
                    if clear.depth.is_some() { aspect |= vk::ImageAspectFlags::DEPTH; }
                    if clear.stencil.is_some() { aspect |= vk::ImageAspectFlags::STENCIL; }
                    if aspect.is_empty() { continue; }
                    let value = vk::ClearDepthStencilValue { depth: clear.depth.unwrap_or(0.0), stencil: clear.stencil.unwrap_or(0) };
                    self.gpu.device.cmd_clear_depth_stencil_image(cb, resource.raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &value, &[vk::ImageSubresourceRange { aspect_mask: aspect, ..range }]);
                }
            }
            self.mark_written(texture);
        }
        Ok(())
    }

    // ---- views and descriptors -------------------------------------------------------------------

    fn image_view(&mut self, id: u32, key: ViewKey) -> Result<vk::ImageView, RendererError> {
        self.ensure_texture_memory(id)?;
        if let Some(view) = self.views.get(&(id, key)) { return Ok(*view); }
        let texture = self.texture(Opcode::Draw, id)?;
        let info = vk::ImageViewCreateInfo::default().image(texture.raw).view_type(key.kind).format(key.format).components(format_swizzle(key.swizzle))
            .subresource_range(vk::ImageSubresourceRange { aspect_mask: key.aspect, base_mip_level: key.base_mip, level_count: key.mips, base_array_layer: key.base_layer, layer_count: key.layers });
        let view = unsafe { self.gpu.device.create_image_view(&info, Some(callbacks())) }
            .map_err(|error| self.command_error(Opcode::Draw, format!("vkCreateImageView {key:?} on {:?}: {error}", texture.format)))?;
        self.views.insert((id, key), view);
        Ok(view)
    }

    /// The native format a D3D view (DXGI 0 = the texture's own) reads a texture as.
    fn view_format(&self, texture: &TextureResource, dxgi: u32) -> Result<vk::Format, String> {
        if dxgi == 0 || texture.info.depth { return Ok(texture.format); }
        let format = texture_format(dxgi, 0).ok_or_else(|| format!("view format DXGI {dxgi} has no native mapping"))?;
        if format != texture.format && !texture.views.contains(&format) { return Err(format!("view format {format:?} is not among texture format {:?}'s view formats", texture.format)); }
        Ok(format)
    }

    /// An image descriptor for a sampled or storage binding, with the layout it needs.
    fn image_descriptor(&mut self, view_id: u32, binding: &BindingReflection, attachments: &[(u32, u32, u32, vk::ImageLayout)], uses: &mut Vec<ImageUse>) -> Result<Descriptor, RendererError> {
        let view = match self.resources.get(&view_id) {
            Some(Resource::View(view)) if !view.is_buffer => *view,
            Some(Resource::View(_)) => return Err(self.command_error(Opcode::Draw, format!("texture binding {} refers to a buffer view", binding.binding))),
            Some(_) => return Err(self.command_error(Opcode::Draw, format!("texture binding {} refers to a non-view resource", binding.binding))),
            None => return self.unbound(binding.kind),
        };
        self.materialize_clears(view.resource)?;
        let (dimension, sample, storage) = match binding.kind {
            BindingKind::Texture { sample, dimension, .. } => (dimension, Some(sample), None),
            BindingKind::StorageTexture { format, dimension, .. } => (dimension, None, Some(format)),
            _ => return Err(self.command_error(Opcode::Draw, "internal texture binding kind mismatch")),
        };
        let mut texture_id = view.resource;
        let mut view_dxgi = view.format;
        // Comparison reads of a float texture go through a depth copy (no comparison on colour formats).
        if sample == Some(TextureSample::Depth) && !self.texture(Opcode::Draw, texture_id)?.info.depth {
            texture_id = self.depth_proxy(texture_id)?;
        }
        let float_depth = matches!(self.texture(Opcode::Draw, texture_id)?.format, vk::Format::D32_SFLOAT | vk::Format::D32_SFLOAT_S8_UINT);
        if self.float_proxy_enabled && float_depth && matches!(sample, Some(TextureSample::Float { .. })) && !matches!(view.format, 22 | 47) {
            texture_id = self.float_proxy(texture_id)?;
            view_dxgi = 0;
        }
        let texture = self.texture(Opcode::Draw, texture_id)?;
        let format = match storage {
            Some(format) => {
                let format = storage_format(format);
                if format != texture.format { return Err(self.command_error(Opcode::Draw, format!("storage view format {format:?} does not match texture format {:?}", texture.format))); }
                format
            }
            None => self.view_format(texture, view_dxgi).map_err(|error| self.command_error(Opcode::Draw, format!("view {view_id}: {error}")))?,
        };
        let is_storage = storage.is_some();
        let mip = view.a;
        if mip >= texture.mip_levels { return Err(self.command_error(Opcode::Draw, format!("view {view_id} mip {mip} is outside texture"))); }
        let mips = if is_storage { 1 } else if view.b == 0 || view.b == u32::MAX { texture.mip_levels - mip } else { view.b.min(texture.mip_levels - mip) };
        let total = texture.layers();
        let first = if is_storage { if view.view_dimension == 5 { view.b } else { 0 } } else { view.c };
        let cube = matches!(dimension, TextureDimension::Cube | TextureDimension::CubeArray);
        if cube && total.saturating_sub(first) < 6 {
            self.note_once("cube view", format!("cube view over a texture with {total} layer(s): bound as an empty cube"));
            return self.unbound(binding.kind);
        }
        let layers = match dimension {
            _ if texture.dimension == vk::ImageType::TYPE_3D => 1,
            TextureDimension::D1 | TextureDimension::D2 | TextureDimension::D3 => 1,
            TextureDimension::Cube => 6,
            TextureDimension::CubeArray => (if view.d == 0 || view.d == u32::MAX { total - first } else { view.d.min(total - first) }) / 6 * 6,
            TextureDimension::D2Array => if is_storage { if view.view_dimension == 5 { view.c.max(1) } else { 1 } } else if view.d == 0 || view.d == u32::MAX { total.saturating_sub(first) } else { view.d },
        };
        let first = if texture.dimension == vk::ImageType::TYPE_3D { 0 } else { first };
        if first >= total || layers == 0 || first + layers > total { return Err(self.command_error(Opcode::Draw, format!("view {view_id} array range is outside texture"))); }
        let expected = match dimension {
            TextureDimension::D1 => vk::ImageType::TYPE_1D,
            TextureDimension::D3 => vk::ImageType::TYPE_3D,
            _ => vk::ImageType::TYPE_2D,
        };
        if texture.dimension != expected {
            if is_storage { return Err(self.command_error(Opcode::Draw, "view and texture dimensions differ")); }
            // A stale sampled SRV must not kill the GPU worker or create an invalid
            // Vulkan view. Use the same typed empty-image path as an unbound slot.
            self.note_once("sampled view dimension", format!("sampled view {view_id} texture {texture_id} is {:?}, binding {} expects {dimension:?}: bound empty", texture.dimension, binding.binding));
            return self.unbound(binding.kind);
        }
        let stencil_read = texture.info.stencil && (matches!(sample, Some(TextureSample::Uint)) || matches!(view.format, 22 | 47));
        let aspect = if !texture.info.depth { vk::ImageAspectFlags::COLOR } else if stencil_read { vk::ImageAspectFlags::STENCIL } else { vk::ImageAspectFlags::DEPTH };
        let effective = if view.format != 0 { view.format } else { texture.dxgi };
        let swizzle = if matches!(effective, 65 | 88 | 92 | 93) || (self.stencil_green && aspect == vk::ImageAspectFlags::STENCIL && matches!(effective, 22 | 47)) { effective } else { 0 };
        let depth = texture.info.depth;
        // A subresource the draw also renders to: D3D unbinds the conflicting view.
        let mut layout = if is_storage { vk::ImageLayout::GENERAL } else if depth { vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL } else { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL };
        for (attached, attached_mip, attached_layer, attached_layout) in attachments {
            if *attached != texture_id || !(mip..mip + mips).contains(attached_mip) || !(first..first + layers).contains(attached_layer) { continue; }
            let readable = match *attached_layout {
                vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL => true,
                vk::ImageLayout::DEPTH_READ_ONLY_STENCIL_ATTACHMENT_OPTIMAL => aspect == vk::ImageAspectFlags::DEPTH,
                vk::ImageLayout::DEPTH_ATTACHMENT_STENCIL_READ_ONLY_OPTIMAL => aspect == vk::ImageAspectFlags::STENCIL,
                _ => false,
            };
            if !readable || mips != 1 || layers != 1 {
                self.note_once("sampled and rendered", format!("texture {texture_id} is sampled and rendered in one draw: bound as unbound, as D3D does"));
                return self.unbound(binding.kind);
            }
            layout = *attached_layout;
        }
        let key = ViewKey { format, kind: if texture.dimension == vk::ImageType::TYPE_3D { vk::ImageViewType::TYPE_3D } else { view_type(dimension) }, aspect, base_mip: mip, mips, base_layer: first, layers, swizzle };
        let native = self.image_view(texture_id, key)?;
        uses.push(ImageUse { texture: texture_id, mips: (mip, mips), layers: (first, layers), layout, stages: vk::PipelineStageFlags2::NONE });
        Ok(Descriptor::Image { view: native, layout })
    }

    /// A 1x1 all-zero image of the binding's type: D3D reads (0,0,0,0) from unbound views, alpha included.
    fn zero_image(&mut self, kind: BindingKind) -> Result<Descriptor, RendererError> {
        let (dimension, dxgi, storage) = match kind {
            BindingKind::Texture { sample, dimension, .. } => (dimension, match sample { TextureSample::Float { .. } => 28, TextureSample::Uint => 30, TextureSample::Sint => 32, TextureSample::Depth => 40 }, false),
            BindingKind::StorageTexture { format, dimension, .. } => (dimension, match storage_format(format) {
                vk::Format::R8G8B8A8_SNORM => 31, vk::Format::R8G8B8A8_UINT => 30, vk::Format::R8G8B8A8_SINT => 32, vk::Format::R32_SFLOAT => 41,
                vk::Format::R32_UINT => 42, vk::Format::R32_SINT => 43, vk::Format::R32G32_SFLOAT => 16, vk::Format::R16G16B16A16_SFLOAT => 10,
                vk::Format::R32G32B32A32_SFLOAT => 2, _ => 28 }, true),
            _ => unreachable!("zero images back texture bindings only"),
        };
        let key = (dxgi, dimension as u32, storage);
        let id = match self.zero_images_by_kind.get(&key) {
            Some(id) => *id,
            None => {
                let id = self.next_internal_id;
                self.next_internal_id += 1;
                let (kind_word, layers) = match dimension { TextureDimension::D1 => (1, 1), TextureDimension::D3 => (3, 1), TextureDimension::Cube | TextureDimension::CubeArray => (2, 6), _ => (2, 1) };
                let bind = if storage { BIND_SRV | BIND_UAV } else if dxgi == 40 { BIND_SRV | BIND_DEPTH_STENCIL } else { BIND_SRV };
                let texture = self.build_texture(&[id, kind_word, 1, 1, 1, 1, layers, dxgi, bind, 1])?;
                self.resources.insert(id, Resource::Texture(Box::new(texture)));
                self.zero_images_by_kind.insert(key, id);
                id
            }
        };
        let texture = self.texture(Opcode::Draw, id)?;
        let (format, depth) = (texture.format, texture.info.depth);
        let layers = if matches!(dimension, TextureDimension::Cube | TextureDimension::CubeArray) { 6 } else { 1 };
        let view_kind = view_type(dimension);
        let layout = if storage { vk::ImageLayout::GENERAL } else if depth { vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL } else { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL };
        let aspect = if depth { vk::ImageAspectFlags::DEPTH } else { vk::ImageAspectFlags::COLOR };
        let view = self.image_view(id, ViewKey { format, kind: view_kind, aspect, base_mip: 0, mips: 1, base_layer: 0, layers, swizzle: 0 })?;
        self.pending_zero_uses.push(ImageUse { texture: id, mips: (0, 1), layers: (0, layers), layout, stages: vk::PipelineStageFlags2::NONE });
        Ok(Descriptor::Image { view, layout })
    }

    /// Whether sampler binding `sampler` samples a bound texture the device cannot filter linearly.
    fn samples_unfilterable(&self, stage: usize, reflection: &ShaderReflection, sampler: u32) -> bool {
        reflection.sampled_with.iter().filter(|(s, _)| *s == sampler).any(|(_, texture)| {
            let Some(slot) = texture.checked_sub(32) else { return false };
            let Some(Resource::View(view)) = self.state.srvs[stage].get(slot as usize).and_then(|id| self.resources.get(id)) else { return false };
            matches!(self.resources.get(&view.resource), Some(Resource::Texture(texture)) if !texture.filterable)
        })
    }

    /// The point-filtering twin of sampler state `id` (D3D filters depth linearly; this GPU cannot for D32S8).
    fn nearest_sampler(&mut self, id: u32) -> Result<vk::Sampler, RendererError> {
        if let Some(sampler) = self.nearest_samplers.get(&id) { return Ok(*sampler); }
        let words = match self.resources.get(&id) { Some(Resource::State(state)) => nearest_words(&state.words), _ => return Err(self.command_error(Opcode::Draw, "sampler state vanished")) };
        let sampler = create_sampler(&self.gpu, &words).map_err(RendererError::Vulkan)?;
        self.note_once("point-filtered", format!("sampler {id} point-filtered for a texture this GPU cannot filter linearly"));
        self.nearest_samplers.insert(id, sampler);
        Ok(sampler)
    }

    /// What an unbound D3D slot reads: zeros (a null descriptor) or a default sampler.
    fn unbound(&mut self, kind: BindingKind) -> Result<Descriptor, RendererError> {
        if self.zero_images && matches!(kind, BindingKind::Texture { .. } | BindingKind::StorageTexture { .. }) { return self.zero_image(kind); }
        match kind {
            BindingKind::Sampler { comparison } => Ok(Descriptor::Sampler(if comparison { self.dummy_compare_sampler } else { self.dummy_sampler })),
            BindingKind::Uniform | BindingKind::Storage { .. } if self.gpu.null_descriptor => Ok(Descriptor::Buffer { buffer: vk::Buffer::null(), offset: 0, range: vk::WHOLE_SIZE }),
            BindingKind::Texture { .. } | BindingKind::StorageTexture { .. } if self.gpu.null_descriptor => Ok(Descriptor::Image { view: vk::ImageView::null(), layout: vk::ImageLayout::UNDEFINED }),
            _ => Err(RendererError::Vulkan("device has no null descriptors for unbound D3D slots".into())),
        }
    }

    /// The descriptors of one stage's non-packed bindings, recording the images and buffers they touch.
    fn stage_descriptors(&mut self, shader_id: u32, stage: usize, reflection: &ShaderReflection, attachments: &[(u32, u32, u32, vk::ImageLayout)], uses: &mut Vec<ImageUse>, buffers: &mut Vec<u32>) -> Result<Vec<(u32, vk::DescriptorType, Descriptor)>, RendererError> {
        let first_use = uses.len();
        let result = self.stage_descriptors_inner(shader_id, stage, reflection, attachments, uses, buffers);
        let stages = match stage { 0 => vk::PipelineStageFlags2::VERTEX_SHADER, 1 => vk::PipelineStageFlags2::FRAGMENT_SHADER, _ => vk::PipelineStageFlags2::COMPUTE_SHADER };
        for usage in &mut uses[first_use..] { usage.stages |= stages; }
        result
    }

    fn stage_descriptors_inner(&mut self, shader_id: u32, stage: usize, reflection: &ShaderReflection, attachments: &[(u32, u32, u32, vk::ImageLayout)], uses: &mut Vec<ImageUse>, buffers: &mut Vec<u32>) -> Result<Vec<(u32, vk::DescriptorType, Descriptor)>, RendererError> {
        let opcode = if stage == 2 { Opcode::Dispatch } else { Opcode::Draw };
        let (packed, constants) = match self.resources.get(&shader_id) {
            Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => (compiled.pack.is_some(), compiled.constants.as_ref().map(|(buffer, _)| *buffer)),
            _ => return Err(self.command_error(opcode, "shader is not compiled")),
        };
        let mut out = Vec::with_capacity(reflection.bindings.len());
        for binding in &reflection.bindings {
            if packed && binding.kind == BindingKind::Uniform && binding.binding == crate::shader::PACK_BINDING { continue; }
            let descriptor = match binding.kind {
                BindingKind::Uniform if binding.binding == 15 => match constants {
                    Some(buffer) => Descriptor::Buffer { buffer, offset: 0, range: vk::WHOLE_SIZE },
                    None => return Err(self.command_error(opcode, format!("shader {shader_id} declares binding 15 without a constant table"))),
                },
                BindingKind::Uniform => return Err(self.command_error(opcode, format!("constant binding {} is outside the packed block", binding.binding))),
                BindingKind::Sampler { comparison } => {
                    let slot = binding.binding.checked_sub(16).ok_or_else(|| self.command_error(opcode, "sampler binding is below the D3D sampler range"))? as usize;
                    let id = self.state.samplers[stage].get(slot).copied().unwrap_or(0);
                    match self.resources.get(&id) {
                        Some(Resource::State(StateResource { kind: 3, words, sampler: Some(sampler) })) => {
                            let has_compare = words.first().copied().unwrap_or(0) & 0x80 != 0;
                            let (sampler, linear) = (*sampler, sampler_is_linear(words));
                            if has_compare != comparison {
                                // The browser worker binds a default sampler of the shader's type.
                                self.note_once("comparison mismatch", format!("sampler state with comparison={has_compare} bound where the shader declares comparison={comparison}"));
                                self.unbound(binding.kind)?
                            } else if linear && self.samples_unfilterable(stage, reflection, binding.binding) {
                                Descriptor::Sampler(self.nearest_sampler(id)?)
                            } else { Descriptor::Sampler(sampler) }
                        }
                        Some(_) => return Err(self.command_error(opcode, format!("sampler slot {slot} is not a sampler state"))),
                        None => self.unbound(binding.kind)?,
                    }
                }
                BindingKind::Texture { .. } => {
                    let slot = binding.binding.checked_sub(32).ok_or_else(|| self.command_error(opcode, "texture binding is below the D3D SRV range"))? as usize;
                    let id = self.state.srvs[stage].get(slot).copied().unwrap_or(0);
                    self.image_descriptor(id, binding, attachments, uses)?
                }
                BindingKind::StorageTexture { .. } => {
                    let slot = binding.binding.checked_sub(160).ok_or_else(|| self.command_error(opcode, "storage texture binding is below the D3D UAV range"))? as usize;
                    let id = self.state.uavs[stage].get(slot).copied().unwrap_or(0);
                    let descriptor = self.image_descriptor(id, binding, attachments, uses)?;
                    if let Some(Resource::View(view)) = self.resources.get(&id) { let texture = view.resource; self.mark_written(texture); }
                    descriptor
                }
                BindingKind::Storage { .. } => {
                    let storage_slot = crate::uav_counter::slot(binding.binding).map_err(|error| self.command_error(opcode, error))?;
                    let id = match storage_slot {
                        crate::uav_counter::StorageSlot::Srv(slot) => self.state.srvs[stage][slot],
                        crate::uav_counter::StorageSlot::Uav(slot) | crate::uav_counter::StorageSlot::Counter(slot) => self.state.uavs[stage][slot],
                    };
                    let view = match self.resources.get(&id) {
                        Some(Resource::View(view)) if view.is_buffer => *view,
                        Some(_) => return Err(self.command_error(opcode, format!("storage binding {} requires a buffer view", binding.binding))),
                        None => { out.push((binding.binding, descriptor_type(binding.kind), self.unbound(binding.kind)?)); continue; }
                    };
                    let buffer = self.buffer(opcode, view.resource)?;
                    buffers.push(view.resource);
                    if matches!(storage_slot, crate::uav_counter::StorageSlot::Counter(_)) {
                        let (counter, _) = buffer.counter.as_ref().ok_or_else(|| self.command_error(opcode, "counter binding has no native allocation"))?;
                        Descriptor::Buffer { buffer: *counter, offset: 0, range: vk::WHOLE_SIZE }
                    } else {
                        let stride = u64::from(buffer.stride.max(1));
                        let offset = u64::from(view.a) * stride;
                        if offset % self.gpu.props.limits.min_storage_buffer_offset_alignment != 0 { return Err(self.command_error(opcode, format!("storage binding {} offset {offset} is not aligned", binding.binding))); }
                        if offset >= buffer.size { return Err(self.command_error(opcode, format!("storage binding {} starts past the buffer", binding.binding))); }
                        let range = if view.b == 0 || view.b == u32::MAX { vk::WHOLE_SIZE } else { (u64::from(view.b) * stride).min(buffer.size - offset) };
                        Descriptor::Buffer { buffer: buffer.raw, offset, range }
                    }
                }
            };
            out.push((binding.binding, descriptor_type(binding.kind), descriptor));
        }
        uses.append(&mut self.pending_zero_uses);
        Ok(out)
    }

    /// A descriptor set with `contents`, written once per command slot and reused for equal contents.
    fn descriptor_set(&mut self, layout: vk::DescriptorSetLayout, contents: Vec<(u32, vk::DescriptorType, Descriptor)>) -> Result<vk::DescriptorSet, RendererError> {
        let key = (layout, contents);
        if let Some(set) = self.slots[self.slot].sets.get(&key) { return Ok(*set); }
        let device = self.gpu.device.clone();
        let set = loop {
            let slot = &mut self.slots[self.slot];
            if slot.descriptor_pool == slot.descriptor_pools.len() {
                let sizes = [
                    vk::DescriptorPoolSize { ty: vk::DescriptorType::SAMPLED_IMAGE, descriptor_count: 8192 },
                    vk::DescriptorPoolSize { ty: vk::DescriptorType::SAMPLER, descriptor_count: 4096 },
                    vk::DescriptorPoolSize { ty: vk::DescriptorType::UNIFORM_BUFFER, descriptor_count: 1024 },
                    vk::DescriptorPoolSize { ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC, descriptor_count: 1024 },
                    vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: 1024 },
                    vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_IMAGE, descriptor_count: 256 },
                ];
                let pool = unsafe { device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(2048).pool_sizes(&sizes), None) }.map_err(vk_error("vkCreateDescriptorPool"))?;
                slot.descriptor_pools.push(pool);
            }
            let pool = slot.descriptor_pools[slot.descriptor_pool];
            let layouts = [layout];
            match unsafe { device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(pool).set_layouts(&layouts)) } {
                Ok(sets) => break sets[0],
                Err(vk::Result::ERROR_OUT_OF_POOL_MEMORY | vk::Result::ERROR_FRAGMENTED_POOL) => slot.descriptor_pool += 1,
                Err(error) => return Err(RendererError::Vulkan(format!("vkAllocateDescriptorSets: {error}"))),
            }
        };
        let images: Vec<[vk::DescriptorImageInfo; 1]> = key.1.iter().map(|(_, _, descriptor)| match descriptor {
            Descriptor::Image { view, layout } => [vk::DescriptorImageInfo { sampler: vk::Sampler::null(), image_view: *view, image_layout: *layout }],
            Descriptor::Sampler(sampler) => [vk::DescriptorImageInfo { sampler: *sampler, image_view: vk::ImageView::null(), image_layout: vk::ImageLayout::UNDEFINED }],
            Descriptor::Buffer { .. } => [vk::DescriptorImageInfo::default()],
        }).collect();
        let buffers: Vec<[vk::DescriptorBufferInfo; 1]> = key.1.iter().map(|(_, _, descriptor)| match descriptor {
            Descriptor::Buffer { buffer, offset, range } => [vk::DescriptorBufferInfo { buffer: *buffer, offset: *offset, range: *range }],
            _ => [vk::DescriptorBufferInfo::default()],
        }).collect();
        let writes: Vec<vk::WriteDescriptorSet> = key.1.iter().enumerate().map(|(index, (binding, kind, descriptor))| {
            let write = vk::WriteDescriptorSet::default().dst_set(set).dst_binding(*binding).descriptor_type(*kind);
            match descriptor { Descriptor::Buffer { .. } => write.buffer_info(&buffers[index]), _ => write.image_info(&images[index]) }
        }).collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        self.stats.sets_written += 1;
        self.slots[self.slot].sets.insert(key, set);
        Ok(set)
    }

    fn stage_set_layout(&mut self, shader_id: u32, stage: usize) -> Result<vk::DescriptorSetLayout, RendererError> {
        let flags = match stage { 0 => vk::ShaderStageFlags::VERTEX, 1 => vk::ShaderStageFlags::FRAGMENT, _ => vk::ShaderStageFlags::COMPUTE };
        let Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) = self.resources.get(&shader_id) else { return Ok(self.empty_set) };
        let bindings: Vec<(u32, vk::DescriptorType, vk::ShaderStageFlags)> = compiled.reflection.bindings.iter().filter(|binding| !is_pack(compiled, binding))
            .map(|binding| (binding.binding, descriptor_type(binding.kind), flags)).collect();
        self.layouts.set_layout(&self.gpu.device, bindings).map_err(RendererError::Vulkan)
    }

    // ---- draws ---------------------------------------------------------------------------------

    fn draw(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::Draw;
        if let Some(source) = self.snapshot_source.take() { self.snapshot(source)?; }
        if std::mem::take(&mut self.split_pending) {
            let cb = self.cb()?;
            let barrier = [vk::MemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS).src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS).dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)];
            unsafe { self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().memory_barriers(&barrier)) };
        }
        let kind = p[0];
        let indexed = matches!(kind, 1 | 3 | 4);
        let indirect = if kind >= 4 {
            let (id, offset) = (p[1], u64::from(p[2]));
            let buffer = self.buffer(opcode, id)?;
            if buffer.misc & MISC_DRAW_INDIRECT == 0 { return Err(self.command_error(opcode, format!("indirect argument buffer {id} was not created with the indirect usage flag"))); }
            if offset % 4 != 0 || offset + if indexed { 20 } else { 16 } > buffer.size { return Err(self.command_error(opcode, "indirect argument range is outside the buffer or not 4-byte aligned")); }
            Some((buffer.raw, offset, id))
        } else { None };
        let (vs_id, ps_id) = (self.state.shaders[0], self.state.shaders[1]);
        let colors: [u32; 8] = std::array::from_fn(|slot| self.state.render_targets[slot].texture);
        let color_outputs = crate::attachments::color_outputs(vs_id, ps_id, &colors, self.state.depth_stencil.texture)
            .map_err(|error| self.command_error(opcode, format!("{error}; VS={vs_id}, PS={ps_id}, colors={colors:?}, depth={}", self.state.depth_stencil.texture)))?;
        self.ensure_shader(vs_id, opcode)?;
        if ps_id != 0 { self.ensure_shader(ps_id, opcode)?; }
        let shader_info = |resources: &FastMap<u32, Resource>, id: u32| match resources.get(&id) {
            Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => Some((Rc::clone(&compiled.reflection), compiled.module, compiled.pack.clone(), compiled.pack_size)),
            _ => None,
        };
        let (vs_reflection, vs_module, vs_pack, vs_pack_size) = shader_info(&self.resources, vs_id).ok_or_else(|| self.command_error(opcode, "vertex shader is not compiled"))?;
        let ps_info = if ps_id == 0 { None } else { Some(shader_info(&self.resources, ps_id).ok_or_else(|| self.command_error(opcode, "pixel shader is not compiled"))?) };
        let fragment_inputs = ps_info.as_ref().map(|info| info.0.fragment_inputs.clone()).unwrap_or_default();
        let vs_module = if fragment_inputs.is_empty() { vs_module } else { self.ensure_vertex_variant(vs_id, &fragment_inputs)? };

        // Attachments.
        let depth_target = self.state.depth_stencil;
        let depth = if depth_target.texture == 0 { None } else {
            let texture = self.texture(opcode, depth_target.texture)?;
            if !texture.info.depth || texture.dimension != vk::ImageType::TYPE_2D || depth_target.mip >= texture.mip_levels || depth_target.slice >= texture.depth_or_layers {
                return Err(self.command_error(opcode, "depth target format, dimensions, or subresource are invalid"));
            }
            Some((texture.format, texture.info.stencil, crate::attachments::Extent { width: (texture.width >> depth_target.mip).max(1), height: (texture.height >> depth_target.mip).max(1), samples: texture.samples }))
        };
        let mut color_formats = [vk::Format::UNDEFINED; 8];
        let mut color_extents = [None; 8];
        for (slot, target) in self.state.render_targets.iter().enumerate() {
            if target.texture == 0 || !color_outputs { continue; }
            let texture = self.texture(opcode, target.texture)?;
            if texture.dimension != vk::ImageType::TYPE_2D || texture.info.depth || target.mip >= texture.mip_levels || target.slice >= texture.depth_or_layers {
                return Err(self.command_error(opcode, format!("color target slot {slot} has invalid format/dimension/subresource")));
            }
            color_extents[slot] = Some(crate::attachments::Extent { width: (texture.width >> target.mip).max(1), height: (texture.height >> target.mip).max(1), samples: texture.samples });
            color_formats[slot] = self.view_format(texture, target.format).map_err(|error| self.command_error(opcode, format!("color target slot {slot}: {error}")))?;
        }
        let (extent, color_count) = crate::attachments::plan(&color_extents, depth.map(|d| d.2)).map_err(|error| self.command_error(opcode, error))?;
        if self.state.viewports.len() > 1 || self.state.scissors.len() > 1 { return Err(self.unsupported(opcode, "multiple viewports or scissors are not implemented")); }
        if let Some(viewport) = self.state.viewports.first() {
            if !viewport.iter().all(|value| value.is_finite()) || viewport[2] <= 0.0 || viewport[3] <= 0.0 || viewport[4] < 0.0 || viewport[5] > 1.0 || viewport[4] > viewport[5] {
                return Err(self.command_error(opcode, "viewport has invalid bounds"));
            }
        }
        let depth_flags = if depth.is_some() { depth_target.flags & 3 } else { 0 };
        let depth_layout = depth.map(|(_, stencil, _)| depth_attachment_layout(depth_flags, stencil));
        let mut scratch = std::mem::take(&mut self.draw_scratch);
        let DrawScratch { attachments, uses, read_buffers, attachment_uses } = &mut scratch;
        attachments.clear();
        uses.clear();
        read_buffers.clear();
        attachment_uses.clear();
        attachments.extend((0..color_count).filter(|slot| color_formats[*slot] != vk::Format::UNDEFINED)
            .map(|slot| { let t = self.state.render_targets[slot]; (t.texture, t.mip, t.slice, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL) }));
        if let Some(layout) = depth_layout { attachments.push((depth_target.texture, depth_target.mip, depth_target.slice, layout)); }

        // Vertex input.
        let input = self.vertex_input(vs_id)?;
        let steps = input.steps;
        let strides: [u32; 16] = std::array::from_fn(|slot| steps[slot].map_or(0, |step| step.stride(self.state.vertex[slot].stride, self.state.vertex[slot].buffer != 0) as u32));

        let key = PipelineKey {
            vs: vs_id, ps: ps_id, layout: self.state.layout, strides, topology: self.state.topology,
            blend: self.state.blend, depth_stencil: self.state.depth_stencil_state, raster: self.state.raster_state,
            colors: color_formats, color_count: color_count as u8, depth: depth.map_or(vk::Format::UNDEFINED, |d| d.0), samples: extent.samples, depth_flags,
        };
        let pipeline = match self.pipelines.get(&key) {
            Some(pipeline) => Rc::clone(pipeline),
            None => {
                let started = Instant::now();
                let vs_set = self.stage_set_layout(vs_id, 0)?;
                let ps_set = if ps_id == 0 { self.empty_set } else { self.stage_set_layout(ps_id, 1)? };
                let [vs_binding, ps_binding] = crate::gpu::pack_bindings(vs_pack.is_some(), ps_info.as_ref().is_some_and(|info| info.2.is_some()));
                let packs: Vec<_> = [(vs_binding, vk::ShaderStageFlags::VERTEX), (ps_binding, vk::ShaderStageFlags::FRAGMENT)].into_iter()
                    .filter_map(|(binding, stage)| binding.map(|binding| (binding, vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC, stage))).collect();
                let pack_set = if packs.is_empty() { self.empty_set } else { self.layouts.set_layout(&self.gpu.device, packs).map_err(RendererError::Vulkan)? };
                let layout = self.layouts.pipeline_layout(&self.gpu.device, [vs_set, ps_set, pack_set]).map_err(RendererError::Vulkan)?;
                let raw = self.create_draw_pipeline(&key, layout, vs_module, ps_info.as_ref().map(|info| info.1), &input.attributes, &steps)?;
                self.stats.pipeline_time += started.elapsed();
                self.stats.pipelines_created += 1;
                self.next_pipeline_id += 1;
                let pipeline = Rc::new(DrawPipeline { id: self.next_pipeline_id, raw, layout, sets: [vs_set, ps_set], pack_set, used: Cell::new(0) });
                if self.pipelines.len() >= PIPELINE_CACHE {
                    let oldest = self.pipelines.iter().min_by_key(|(_, cached)| cached.used.get()).map(|(key, _)| key.clone());
                    if let Some(oldest) = oldest.and_then(|key| self.pipelines.remove(&key)) { let value = self.recording_value(); self.garbage.push(value, Trash::Pipeline(oldest.raw)); }
                }
                self.pipelines.insert(key.clone(), Rc::clone(&pipeline));
                self.pipeline_cache_dirty += 1;
                pipeline
            }
        };
        pipeline.used.set(self.report.presents);
        if let Some(skip) = &self.skip_ps {
            if matches!(self.resources.get(&ps_id), Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) if compiled.label.contains(skip.as_str())) { return Ok(()); }
        }
        if let Some(after) = &self.split_after {
            let label = |id| match self.resources.get(&id) { Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => compiled.label.contains(after.as_str()), _ => false };
            if label(vs_id) || label(ps_id) { self.split_pending = true; }
        }
        if let Some((slot, after)) = &self.show_rt {
            let label = |id| match self.resources.get(&id) { Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => compiled.label.contains(after.as_str()), _ => false };
            if color_count as u32 >= *slot && (label(vs_id) || label(ps_id)) {
                let target = self.state.render_targets[*slot as usize - 1].texture;
                if self.report.presents % 32 == 0 { log_line(&format!("show_rt {after} frame {} copies tex{target} {:?} depth tex{} ref {}", self.report.presents, color_formats[*slot as usize - 1], self.state.depth_stencil.texture, self.state.stencil_reference)); }
                self.snapshot_source = Some(target);
            }
        }
        self.stats.frame_pipelines.insert(pipeline.id);

        let dumping = self.dump_frame == Some(self.frames_since_world) && self.report.world;
        if dumping { self.dump_draw(vs_id, ps_id, &key); }
        self.dumping = dumping;

        // Resources.
        let mut sets = [None; 2];
        for (stage, shader, reflection) in [(0usize, vs_id, Some(&vs_reflection)), (1, ps_id, ps_info.as_ref().map(|info| &info.0))] {
            let Some(reflection) = reflection else { continue };
            let layout = pipeline.sets[stage];
            if layout == self.empty_set { continue; }
            sets[stage] = Some(self.stage_set(stage, shader, layout, reflection, attachments, uses, read_buffers)?);
        }
        let vs_packed = match &vs_pack { Some(members) => Some(self.pack_constants(0, members, vs_pack_size)?), None => None };
        let ps_packed = match ps_info.as_ref().and_then(|info| info.2.clone().map(|members| (members, info.3))) { Some((members, size)) => Some(self.pack_constants(1, &members, size)?), None => None };
        if dumping {
            let used: Vec<String> = uses.iter().map(|u| format!("tex{}:{:?}", u.texture, u.layout)).collect();
            self.dump_line(&format!("dump bound {}: sets {:?} uses [{}]", self.stats.draws, sets, used.join(" ")));
        }
        // A read-only depth target cannot load a clear: record it first.
        if depth_flags != 0 { self.materialize_clears(depth_target.texture)?; }

        let pass = PassKey {
            colors: std::array::from_fn(|slot| (slot < color_count && color_formats[slot] != vk::Format::UNDEFINED).then(|| { let t = self.state.render_targets[slot]; (t.texture, t.mip, t.slice, color_formats[slot]) })),
            color_count,
            depth: depth.map(|_| (depth_target.texture, depth_target.mip, depth_target.slice, depth_flags)),
            extent: (extent.width, extent.height),
        };
        attachment_uses.extend(attachments.iter().map(|(texture, mip, layer, layout)| ImageUse { texture: *texture, mips: (*mip, 1), layers: (*layer, 1), layout: *layout, stages: vk::PipelineStageFlags2::NONE }));
        let pending = self.recording.as_ref().is_some_and(|recording| recording.pending.0 != vk::PipelineStageFlags2::NONE);
        let open = self.recording.as_ref().and_then(|recording| recording.pass.as_ref()) == Some(&pass);
        let transition = open && !pending && (self.needs_transition(uses) || self.needs_transition(attachment_uses));
        if !open || pending || transition {
            self.stats.pass_breaks[if !open { 0 } else if pending { 1 } else { 2 }] += 1;
            self.end_pass();
            let cb = self.recording_cb()?;
            self.flush_pending_writes(cb);
            self.transition(cb, uses, false)?;
            self.transition(cb, attachment_uses, true)?;
            self.begin_rendering(cb, &pass, depth.map(|d| d.1).unwrap_or(false), depth_flags)?;
        }
        let cb = self.recording.as_ref().expect("recording").cb;
        let value = self.recording_value();
        let device = Rc::clone(&self.device);

        // An active occlusion query counts in every rendering instance it spans: one slot per instance.
        let recording = self.recording.as_mut().expect("recording");
        if let Some((_, slots)) = self.active_query.as_mut() {
            if recording.open_query.is_none() {
                let slot = self.occlusion.reserve(&device)?;
                let (pool, index) = self.occlusion.location(slot);
                unsafe { device.cmd_begin_query(cb, pool, index, vk::QueryControlFlags::empty()) };
                recording.open_query = Some(slot);
                slots.push(slot);
            }
        }
        if recording.bound.pipeline != Some(pipeline.id) {
            unsafe { device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.raw) };
            recording.bound.pipeline = Some(pipeline.id);
            recording.bound.sets = [None, None];
            recording.bound.packs = [None, None];
        }
        for (index, set) in sets.into_iter().enumerate() {
            let Some(set) = set else { continue };
            let recording = self.recording.as_mut().expect("recording");
            if recording.bound.sets[index] != Some(set) {
                unsafe { device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.layout, index as u32, &[set], &[]) };
                recording.bound.sets[index] = Some(set);
            }
        }
        let packs = [vs_packed, ps_packed];
        let stale = self.recording.as_ref().expect("recording").bound.packs != packs && packs.iter().any(Option::is_some);
        if stale {
            let pack_key = (pipeline.pack_set, packs.map(|pack| pack.map(|(buffer, _, range)| (buffer, range))));
            let set = match self.slots[self.slot].pack_sets.get(&pack_key) {
                Some(set) => *set,
                None => {
                    let bindings = crate::gpu::pack_bindings(vs_packed.is_some(), ps_packed.is_some());
                    let contents = (0..2).filter_map(|stage| bindings[stage].zip(packs[stage]))
                        .map(|(binding, (buffer, _, range))| (binding, vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC, Descriptor::Buffer { buffer, offset: 0, range })).collect();
                    let set = self.descriptor_set(pipeline.pack_set, contents)?;
                    self.slots[self.slot].pack_sets.insert(pack_key, set);
                    set
                }
            };
            let mut offsets = [0u32; 2];
            let mut count = 0;
            for (_, offset, _) in packs.iter().flatten() { offsets[count] = *offset as u32; count += 1; }
            unsafe { device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.layout, 2, &[set], &offsets[..count]) };
            self.recording.as_mut().expect("recording").bound.packs = packs;
        }

        // Vertex and index buffers.
        let (start, base_vertex, start_instance) = (u64::from(p[3]), p[4] as i32, p[5]);
        let mut changed = [None::<(vk::Buffer, u64)>; 16];
        for slot in 0..16 {
            let Some(step) = steps[slot] else { continue };
            let binding = self.state.vertex[slot];
            let (buffer, offset) = if binding.buffer == 0 { (self.zero_vertex, 0) } else {
                let buffer = match self.resources.get_mut(&binding.buffer) { Some(Resource::Buffer(buffer)) => buffer, _ => return Err(RendererError::Command { opcode, detail: format!("vertex buffer {} does not exist", binding.buffer) }) };
                buffer.touch(value);
                let shift = step.shift(start, base_vertex, start_instance, indexed, vs_reflection.uses_vertex_index, vs_reflection.uses_instance_index);
                let offset = u64::from(binding.offset) + shift * u64::from(binding.stride);
                if offset >= buffer.size { return Err(RendererError::Command { opcode, detail: format!("vertex buffer {} offset is outside", binding.buffer) }); }
                (buffer.raw, offset)
            };
            let recording = self.recording.as_mut().expect("recording");
            if recording.bound.vertex[slot] != Some((buffer, offset)) {
                changed[slot] = Some((buffer, offset));
                recording.bound.vertex[slot] = Some((buffer, offset));
            }
        }
        // One bind per run of consecutive changed slots: the driver's per-call cost dominates a single slot's.
        let mut slot = 0;
        while slot < 16 {
            if changed[slot].is_none() { slot += 1; continue; }
            let first = slot;
            let (mut buffers, mut offsets) = ([vk::Buffer::null(); 16], [0u64; 16]);
            while let Some((buffer, offset)) = changed.get(slot).copied().flatten() {
                (buffers[slot - first], offsets[slot - first]) = (buffer, offset);
                slot += 1;
            }
            unsafe { device.cmd_bind_vertex_buffers(cb, first as u32, &buffers[..slot - first], &offsets[..slot - first]) };
        }
        if indexed {
            let binding = self.state.index;
            let index_type = if binding.format == 57 { vk::IndexType::UINT16 } else { vk::IndexType::UINT32 };
            let buffer = match self.resources.get_mut(&binding.buffer) { Some(Resource::Buffer(buffer)) => buffer, _ => return Err(RendererError::Command { opcode, detail: "indexed draw has no index buffer".into() }) };
            buffer.touch(value);
            if binding.offset % if index_type == vk::IndexType::UINT16 { 2 } else { 4 } != 0 || u64::from(binding.offset) >= buffer.size { return Err(RendererError::Command { opcode, detail: "index buffer offset is outside or not aligned".into() }); }
            let entry = (buffer.raw, u64::from(binding.offset), index_type);
            let recording = self.recording.as_mut().expect("recording");
            if recording.bound.index != Some(entry) {
                unsafe { device.cmd_bind_index_buffer(cb, entry.0, entry.1, entry.2) };
                recording.bound.index = Some(entry);
            }
        }
        for id in read_buffers.iter() { if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(id) { buffer.touch(value); } }

        // Dynamic state. The viewport is flipped (negative height), as wgpu did, so clip space matches D3D.
        let recording = self.recording.as_mut().expect("recording");
        let viewport = self.state.viewports.first().copied().unwrap_or([0.0, 0.0, extent.width as f32, extent.height as f32, 0.0, 1.0]);
        if recording.bound.viewport != Some(viewport) {
            unsafe { device.cmd_set_viewport(cb, 0, &[vk::Viewport { x: viewport[0], y: viewport[1] + viewport[3], width: viewport[2], height: -viewport[3], min_depth: viewport[4], max_depth: viewport[5] }]) };
            recording.bound.viewport = Some(viewport);
        }
        let scissor_enabled = match self.resources.get(&self.state.raster_state) { Some(Resource::State(state)) => state.words.get(7).copied().unwrap_or(0) != 0, _ => false };
        let scissor = crate::scissor::native_rect(scissor_enabled, self.state.scissors.first().copied(), extent.width, extent.height)
            .map_err(|error| RendererError::Command { opcode, detail: error })?.unwrap_or([0, 0, extent.width, extent.height]);
        if recording.bound.scissor != Some(scissor) {
            unsafe { device.cmd_set_scissor(cb, 0, &[vk::Rect2D { offset: vk::Offset2D { x: scissor[0] as i32, y: scissor[1] as i32 }, extent: vk::Extent2D { width: scissor[2], height: scissor[3] } }]) };
            recording.bound.scissor = Some(scissor);
        }
        if recording.bound.blend != Some(self.state.blend_factor) {
            unsafe { device.cmd_set_blend_constants(cb, &self.state.blend_factor) };
            recording.bound.blend = Some(self.state.blend_factor);
        }
        if recording.bound.stencil != Some(self.state.stencil_reference) {
            unsafe { device.cmd_set_stencil_reference(cb, vk::StencilFaceFlags::FRONT_AND_BACK, self.state.stencil_reference) };
            recording.bound.stencil = Some(self.state.stencil_reference);
        }

        match indirect {
            Some((buffer, offset, id)) => {
                if let Some(Resource::Buffer(resource)) = self.resources.get_mut(&id) { resource.touch(value); }
                unsafe { if indexed { device.cmd_draw_indexed_indirect(cb, buffer, offset, 1, 20) } else { device.cmd_draw_indirect(cb, buffer, offset, 1, 16) } };
            }
            None => {
                let (count, instances) = (p[1], p[2]);
                let first_instance = if vs_reflection.uses_instance_index { 0 } else { start_instance };
                unsafe {
                    if indexed {
                        device.cmd_draw_indexed(cb, count, instances, start as u32, if vs_reflection.uses_vertex_index { 0 } else { base_vertex }, first_instance);
                    } else {
                        device.cmd_draw(cb, count, instances, if vs_reflection.uses_vertex_index { 0 } else { start as u32 }, first_instance);
                    }
                }
            }
        }
        for (texture, _, _, layout) in attachments.iter() {
            if !matches!(*layout, vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL) { self.mark_written(*texture); }
        }
        self.draw_scratch = scratch;
        Ok(())
    }

    /// The vertex attributes the bound input layout feeds the vertex shader, cached per (layout, shader).
    fn vertex_input(&mut self, vs_id: u32) -> Result<Rc<VertexInput>, RendererError> {
        let opcode = Opcode::Draw;
        let layout_id = self.state.layout;
        if let Some(input) = self.vertex_inputs.get(&(layout_id, vs_id)) { return Ok(Rc::clone(input)); }
        let signatures = match self.resources.get(&vs_id) { Some(Resource::Shader(shader)) => shader.signatures.clone(), _ => return Err(self.command_error(opcode, "vertex shader vanished")) };
        let elements = if layout_id == 0 {
            if !signatures.is_empty() { return Err(self.unsupported(opcode, "vertex shader inputs require a D3D input layout")); }
            Vec::new()
        } else {
            match self.resources.get(&layout_id) {
                Some(Resource::Layout(elements)) => elements.clone(),
                Some(_) => return Err(self.command_error(opcode, "input layout id is not a layout resource")),
                None => return Err(self.command_error(opcode, format!("input layout resource {layout_id} does not exist"))),
            }
        };
        let mut attributes = Vec::new();
        let mut steps = [None::<crate::vertex_step::InputStep>; 16];
        for element in &elements {
            let slot = element.slot as usize;
            if slot >= 16 { return Err(self.command_error(opcode, "vertex slot is outside the 16-slot ABI")); }
            let Some(signature) = signatures.iter().find(|signature| signature[0] == element.semantic_hash && signature[1] == element.semantic_index) else { continue };
            let format = vertex_format(element.dxgi_format).ok_or_else(|| self.command_error(opcode, format!("DXGI vertex format {} is not supported", element.dxgi_format)))?;
            let step = crate::vertex_step::InputStep::new(element.slot_class, element.step_rate).map_err(|error| self.command_error(opcode, format!("layout={layout_id} slot={slot}: {error}")))?;
            if steps[slot].is_some_and(|existing| existing != step) { return Err(self.command_error(opcode, format!("layout={layout_id} slot={slot} has conflicting input step modes"))); }
            steps[slot] = Some(step);
            attributes.push(vk::VertexInputAttributeDescription { location: signature[2], binding: slot as u32, format, offset: element.offset });
        }
        let input = Rc::new(VertexInput { attributes, steps });
        self.vertex_inputs.insert((layout_id, vs_id), Rc::clone(&input));
        Ok(input)
    }

    /// A stage's descriptor set: the cached one while nothing it depends on changed, else resolved anew.
    fn stage_set(&mut self, stage: usize, shader: u32, layout: vk::DescriptorSetLayout, reflection: &ShaderReflection, attachments: &[(u32, u32, u32, vk::ImageLayout)], uses: &mut Vec<ImageUse>, buffers: &mut Vec<u32>) -> Result<vk::DescriptorSet, RendererError> {
        let epoch = (self.stage_epoch[stage], self.global_epoch);
        if let Some(cache) = &self.stage_cache[stage] {
            if cache.shader == shader && cache.epoch == epoch && cache.layout == layout && cache.attachments == attachments {
                uses.extend_from_slice(&cache.uses);
                buffers.extend_from_slice(&cache.buffers);
                let (set, writes) = (cache.set, cache.writes.clone());
                for texture in writes { self.mark_written(texture); }
                return Ok(set);
            }
        }
        let (first_use, first_buffer) = (uses.len(), buffers.len());
        let contents = self.stage_descriptors(shader, stage, reflection, attachments, uses, buffers)?;
        let set = self.descriptor_set(layout, contents)?;
        // Resolving may itself have bumped the global epoch (clears recorded, proxies refreshed).
        let epoch = (self.stage_epoch[stage], self.global_epoch);
        let cache = self.stage_cache[stage].get_or_insert_with(|| StageCache { shader, epoch, layout, attachments: Vec::new(), set, uses: Vec::new(), buffers: Vec::new(), writes: Vec::new() });
        (cache.shader, cache.epoch, cache.layout, cache.set) = (shader, epoch, layout, set);
        cache.attachments.clear();
        cache.attachments.extend_from_slice(attachments);
        cache.uses.clear();
        cache.uses.extend_from_slice(&uses[first_use..]);
        cache.buffers.clear();
        cache.buffers.extend_from_slice(&buffers[first_buffer..]);
        cache.writes.clear();
        cache.writes.extend(uses[first_use..].iter().filter(|usage| usage.layout == vk::ImageLayout::GENERAL).map(|usage| usage.texture));
        Ok(set)
    }

    fn dump_line(&self, text: &str) {
        let mut out = self.dump_out.borrow_mut();
        if out.is_none() {
            let path = self.pipeline_cache_path.as_ref().and_then(|path| path.parent()).map(|dir| dir.join(format!("dump-{}.log", self.frames_since_world)));
            match path.map(|path| std::fs::File::create(&path).map(|file| (path, file))) {
                Some(Ok((path, file))) => { log_line(&format!("dump file {}", path.display())); *out = Some(std::io::BufWriter::new(file)); }
                Some(Err(error)) => log_line(&format!("dump file failed: {error}")),
                None => {}
            }
        }
        match out.as_mut() {
            Some(file) => { let _ = writeln!(file, "{text}"); }
            None => log_line(text),
        }
    }

    /// One log line per draw of the dumped frame: shaders, targets, and the formats of bound textures.
    fn dump_draw(&self, vs: u32, ps: u32, key: &PipelineKey) {
        let label = |id: u32| match self.resources.get(&id) { Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => compiled.label.clone(), _ => format!("#{id}") };
        let mut srvs = Vec::new();
        for (stage, name) in [(0usize, "vs"), (1, "ps")] {
            for (slot, id) in self.state.srvs[stage].iter().enumerate().filter(|(_, id)| **id != 0) {
                let Some(Resource::View(view)) = self.resources.get(id) else { continue };
                match self.resources.get(&view.resource) {
                    Some(Resource::Texture(texture)) => srvs.push(format!("{name}t{slot}=tex{} dxgi{}/view{} {:?} {}x{} mips{} dim{}", view.resource, texture.dxgi, view.format, texture.format, texture.width, texture.height, texture.mip_levels, view.view_dimension)),
                    Some(Resource::Buffer(_)) => srvs.push(format!("{name}t{slot}=buf{}", view.resource)),
                    _ => {}
                }
            }
        }
        let mut samplers = Vec::new();
        for (stage, name) in [(0usize, "vs"), (1, "ps")] {
            for (slot, id) in self.state.samplers[stage].iter().enumerate().filter(|(_, id)| **id != 0) {
                if let Some(Resource::State(state)) = self.resources.get(id) { samplers.push(format!("{name}s{slot}={:?}", &state.words[..state.words.len().min(13)])); }
            }
        }
        let elements = match self.resources.get(&self.state.layout) {
            Some(Resource::Layout(elements)) => elements.iter().map(|e| format!("{:x}/{} dxgi{} slot{} +{} class{}", e.semantic_hash, e.semantic_index, e.dxgi_format, e.slot, e.offset, e.slot_class)).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        let vertex: Vec<String> = self.state.vertex.iter().enumerate().filter(|(_, v)| v.buffer != 0).map(|(slot, v)| format!("vb{slot}=buf{} stride{} +{}", v.buffer, v.stride, v.offset)).collect();
        self.dump_line(&format!("dump input {}: [{elements}] {} | {}", self.stats.draws, vertex.join(" "), samplers.join(" ")));
        let first_vertex = self.state.vertex.iter().find(|v| v.buffer != 0).and_then(|v| match self.resources.get(&v.buffer) {
            Some(Resource::Buffer(buffer)) => buffer.memory.mapped_slice().and_then(|bytes| bytes.get(v.offset as usize..(v.offset + v.stride.max(4)) as usize)).map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            _ => None,
        });
        let mut constants = Vec::new();
        for (stage, name) in [(0usize, "vs"), (1, "ps")] {
            for (slot, binding) in self.state.constants[stage].iter().enumerate().filter(|(_, b)| b.buffer != 0) {
                let Some(Resource::Buffer(buffer)) = self.resources.get(&binding.buffer) else { continue };
                let Some(shadow) = buffer.shadow.as_ref() else { continue };
                let start = binding.first as usize * 16;
                let floats: Vec<String> = shadow.get(start..(start + 64).min(shadow.len())).unwrap_or_default().chunks_exact(4).map(|c| format!("{:.3}", f32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect();
                constants.push(format!("{name}cb{slot}=buf{}+{} [{}]", binding.buffer, binding.first, floats.join(",")));
            }
        }
        let words = |id: u32| match self.resources.get(&id) { Some(Resource::State(state)) => format!("{:?}", &state.words[..state.words.len().min(24)]), _ => String::from("-") };
        self.dump_line(&format!("dump data {}: vertex0 {} | {} | blend {} ds {}", self.stats.draws, first_vertex.unwrap_or_default(), constants.join(" "), words(self.state.blend), words(self.state.depth_stencil_state)));
        let targets: Vec<String> = self.state.render_targets.iter().filter(|t| t.texture != 0).map(|t| format!("tex{}/view{}", t.texture, t.format)).collect();
        self.dump_line(&format!("dump draw {}: VS {} | PS {} | layout {} topo {} blend {} ds {} raster {} ref {} | colors {:?} depth {:?} flags {} | rt {:?} ds tex{} | viewport {:?} scissor {:?} | {}",
            self.stats.draws, label(vs), label(ps), key.layout, key.topology, key.blend, key.depth_stencil, key.raster, self.state.stencil_reference,
            key.colors, key.depth, key.depth_flags, targets, self.state.depth_stencil.texture, self.state.viewports.first(), self.state.scissors.first(), srvs.join(", ")));
    }

    fn begin_rendering(&mut self, cb: vk::CommandBuffer, pass: &PassKey, stencil: bool, depth_flags: u32) -> Result<(), RendererError> {
        let mut colors = Vec::with_capacity(pass.color_count);
        for color in &pass.colors[..pass.color_count] {
            let Some((texture, mip, slice, format)) = *color else { colors.push(vk::RenderingAttachmentInfo::default().image_view(vk::ImageView::null())); continue };
            let view = self.image_view(texture, ViewKey { format, kind: vk::ImageViewType::TYPE_2D, aspect: vk::ImageAspectFlags::COLOR, base_mip: mip, mips: 1, base_layer: slice, layers: 1, swizzle: 0 })?;
            let clear = self.pending_clears.remove(&(texture, mip, slice)).and_then(|clear| clear.color);
            colors.push(vk::RenderingAttachmentInfo::default().image_view(view).image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(if clear.is_some() { vk::AttachmentLoadOp::CLEAR } else if self.attach_probe & 4 != 0 { vk::AttachmentLoadOp::DONT_CARE } else { vk::AttachmentLoadOp::LOAD }).store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32: clear.unwrap_or_default() } }));
        }
        let mut depth_info = None;
        if let Some((texture, mip, slice, _)) = pass.depth {
            let format = self.texture(Opcode::Draw, texture)?.format;
            let aspect = if stencil { vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL } else { vk::ImageAspectFlags::DEPTH };
            let view = self.image_view(texture, ViewKey { format, kind: vk::ImageViewType::TYPE_2D, aspect, base_mip: mip, mips: 1, base_layer: slice, layers: 1, swizzle: 0 })?;
            let clear = self.pending_clears.remove(&(texture, mip, slice)).unwrap_or_default();
            let layout = depth_attachment_layout(depth_flags, stencil);
            let depth_load = if depth_flags & 1 == 0 && clear.depth.is_some() { vk::AttachmentLoadOp::CLEAR } else { vk::AttachmentLoadOp::LOAD };
            let stencil_load = if depth_flags & 2 == 0 && clear.stencil.is_some() { vk::AttachmentLoadOp::CLEAR } else { vk::AttachmentLoadOp::LOAD };
            let value = vk::ClearValue { depth_stencil: vk::ClearDepthStencilValue { depth: clear.depth.unwrap_or(0.0), stencil: clear.stencil.unwrap_or(0) } };
            let probe_load = |load: vk::AttachmentLoadOp| if self.attach_probe & 2 != 0 && load == vk::AttachmentLoadOp::LOAD { vk::AttachmentLoadOp::DONT_CARE } else { load };
            let probe_store = if self.attach_probe & 1 != 0 { vk::AttachmentStoreOp::DONT_CARE } else { vk::AttachmentStoreOp::STORE };
            let depth = vk::RenderingAttachmentInfo::default().image_view(view).image_layout(layout).load_op(probe_load(depth_load)).store_op(if depth_flags & 1 != 0 { vk::AttachmentStoreOp::NONE } else { probe_store }).clear_value(value);
            let stencil_info = vk::RenderingAttachmentInfo::default().image_view(view).image_layout(layout).load_op(probe_load(stencil_load)).store_op(if depth_flags & 2 != 0 { vk::AttachmentStoreOp::NONE } else { probe_store }).clear_value(value);
            depth_info = Some((depth, stencil.then_some(stencil_info)));
        }
        let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: vk::Extent2D { width: pass.extent.0, height: pass.extent.1 } };
        let mut info = vk::RenderingInfo::default().render_area(area).layer_count(1).color_attachments(&colors);
        if let Some((depth, stencil)) = depth_info.as_ref() {
            info = info.depth_attachment(depth);
            if let Some(stencil) = stencil { info = info.stencil_attachment(stencil); }
        }
        let profiled = match self.slots[self.slot].profile.as_mut() {
            Some(profile) if (profile.keys.len() as u32 + 1) * 2 <= PROFILE_QUERIES => {
                let index = profile.keys.len() as u32;
                let colors: Vec<String> = pass.colors[..pass.color_count].iter().map(|c| c.map_or_else(|| "-".to_string(), |(_, _, _, f)| format!("{f:?}"))).collect();
                let depth = pass.depth.and_then(|(texture, ..)| self.resources.get(&texture)).and_then(|r| match r { Resource::Texture(t) => Some(format!("{:?}", t.format)), _ => None });
                profile.keys.push(format!("{}x{} [{}] {}", pass.extent.0, pass.extent.1, colors.join("+"), depth.unwrap_or_else(|| "no-depth".into())));
                unsafe { self.gpu.device.cmd_write_timestamp2(cb, vk::PipelineStageFlags2::ALL_COMMANDS, profile.pool, index * 2) };
                Some(index)
            }
            _ => None,
        };
        unsafe { self.gpu.device.cmd_begin_rendering(cb, &info) };
        let recording = self.recording.as_mut().expect("recording");
        recording.profiled = profiled;
        recording.pass = Some(pass.clone());
        recording.bound = Bound::default();
        self.stats.passes += 1;
        Ok(())
    }

    fn create_draw_pipeline(&mut self, key: &PipelineKey, layout: vk::PipelineLayout, vs: vk::ShaderModule, ps: Option<vk::ShaderModule>, attributes: &[vk::VertexInputAttributeDescription], steps: &[Option<crate::vertex_step::InputStep>; 16]) -> Result<vk::Pipeline, RendererError> {
        let opcode = Opcode::Draw;
        let state = |id: u32, kind: u32| -> Result<Option<Vec<u32>>, RendererError> {
            if id == 0 { return Ok(None); }
            match self.resources.get(&id) {
                Some(Resource::State(state)) if state.kind == kind => Ok(Some(state.words.clone())),
                Some(_) => Err(RendererError::Command { opcode, detail: format!("state {id} is not of kind {kind}") }),
                None => Err(RendererError::Command { opcode, detail: format!("state resource {id} does not exist") }),
            }
        };
        let blend = state(key.blend, 0)?;
        let depth_state = state(key.depth_stencil, 1)?;
        let raster = state(key.raster, 2)?;
        let word = |words: &Option<Vec<u32>>, index: usize, default: u32| words.as_ref().and_then(|w| w.get(index).copied()).unwrap_or(default);
        let topology = topology(key.topology).map_err(|error| self.command_error(opcode, error))?;
        let triangles = matches!(topology, vk::PrimitiveTopology::TRIANGLE_LIST | vk::PrimitiveTopology::TRIANGLE_STRIP
            | vk::PrimitiveTopology::TRIANGLE_LIST_WITH_ADJACENCY | vk::PrimitiveTopology::TRIANGLE_STRIP_WITH_ADJACENCY);

        let bindings: Vec<vk::VertexInputBindingDescription> = (0..16).filter_map(|slot| steps[slot].map(|step| vk::VertexInputBindingDescription {
            binding: slot as u32, stride: key.strides[slot],
            input_rate: if step == crate::vertex_step::InputStep::Vertex { vk::VertexInputRate::VERTEX } else { vk::VertexInputRate::INSTANCE },
        })).collect();
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default().vertex_binding_descriptions(&bindings).vertex_attribute_descriptions(attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology)
            .primitive_restart_enable(matches!(topology, vk::PrimitiveTopology::TRIANGLE_STRIP | vk::PrimitiveTopology::LINE_STRIP
                | vk::PrimitiveTopology::TRIANGLE_STRIP_WITH_ADJACENCY | vk::PrimitiveTopology::LINE_STRIP_WITH_ADJACENCY));
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        if word(&raster, 0, 3) == 2 { return Err(self.unsupported(opcode, "wireframe rasterization is not implemented")); }
        let cull = match word(&raster, 1, if self.gpu.legacy { 1 } else { 3 }) { 1 => vk::CullModeFlags::NONE, 2 => vk::CullModeFlags::FRONT, 3 => vk::CullModeFlags::BACK, value => return Err(self.command_error(opcode, format!("unsupported D3D cull mode {value}"))) };
        let finite = |value: f32| if value.is_finite() { value } else { 0.0 };
        let (bias, bias_clamp, slope) = if raster.is_some() && triangles && depth_state_present(key) { (word(&raster, 3, 0) as i32 as f32, finite(f32::from_bits(word(&raster, 4, 0))), finite(f32::from_bits(word(&raster, 5, 0)))) } else { (0.0, 0.0, 0.0) };
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL).cull_mode(cull)
            .front_face(if word(&raster, 2, 0) != 0 { vk::FrontFace::COUNTER_CLOCKWISE } else { vk::FrontFace::CLOCKWISE })
            .depth_clamp_enable(!self.gpu.legacy && raster.is_some() && word(&raster, 6, 1) == 0)
            .depth_bias_enable(bias != 0.0 || slope != 0.0).depth_bias_constant_factor(bias).depth_bias_clamp(bias_clamp).depth_bias_slope_factor(slope).line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::from_raw(key.samples))
            // D3D honours alpha-to-coverage on single-sample targets too (grass and foliage cut-outs).
            .alpha_to_coverage_enable(word(&blend, 0, 0) != 0 && (!self.gpu.legacy || key.samples > 1));
        let compare = |value| compare_op(value).map_err(|error| RendererError::Command { opcode, detail: error });
        let stencil_op_of = |value| stencil_op(value).map_err(|error| RendererError::Command { opcode, detail: error });
        let mut depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default().depth_compare_op(vk::CompareOp::ALWAYS);
        if key.depth != vk::Format::UNDEFINED {
            if word(&depth_state, 0, 0) != 0 {
                depth_stencil = depth_stencil.depth_test_enable(true).depth_write_enable(word(&depth_state, 1, 0) != 0 && key.depth_flags & 1 == 0).depth_compare_op(compare(word(&depth_state, 2, 8))?);
            }
            if word(&depth_state, 3, 0) != 0 && format_info(key.depth).is_some_and(|info| info.stencil) {
                let mask = word(&depth_state, 4, 0);
                let write_mask = if key.depth_flags & 2 != 0 { 0 } else { (mask >> 8) & 0xff };
                let face = |base: usize| -> Result<vk::StencilOpState, RendererError> { Ok(vk::StencilOpState {
                    fail_op: stencil_op_of(word(&depth_state, base, 1))?, depth_fail_op: stencil_op_of(word(&depth_state, base + 1, 1))?,
                    pass_op: stencil_op_of(word(&depth_state, base + 2, 1))?, compare_op: compare(word(&depth_state, base + 3, 8))?,
                    compare_mask: mask & 0xff, write_mask, reference: 0 }) };
                depth_stencil = depth_stencil.stencil_test_enable(true).front(face(5)?).back(face(9)?);
            }
        }
        let blend_error = |error| RendererError::Command { opcode, detail: error };
        let independent = word(&blend, 1, 0) != 0;
        let colors = &key.colors[..usize::from(key.color_count)];
        let mut attachments = Vec::with_capacity(colors.len());
        for (slot, format) in colors.iter().enumerate() {
            let base = 2 + if independent { slot } else { 0 } * 8;
            let enabled = blend.is_some() && word(&blend, base, 0) != 0 && *format != vk::Format::UNDEFINED && !is_integer(*format);
            let mut attachment = vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::from_raw(if blend.is_some() { word(&blend, base + 7, 0xf) & 0xf } else { 0xf }));
            if enabled {
                attachment = attachment.blend_enable(true)
                    .src_color_blend_factor(blend_factor(word(&blend, base + 1, 2)).map_err(blend_error)?).dst_color_blend_factor(blend_factor(word(&blend, base + 2, 1)).map_err(blend_error)?)
                    .color_blend_op(blend_op(word(&blend, base + 3, 1)).map_err(blend_error)?)
                    .src_alpha_blend_factor(blend_factor(word(&blend, base + 4, 2)).map_err(blend_error)?).dst_alpha_blend_factor(blend_factor(word(&blend, base + 5, 1)).map_err(blend_error)?)
                    .alpha_blend_op(blend_op(word(&blend, base + 6, 1)).map_err(blend_error)?);
            }
            attachments.push(attachment);
        }
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR, vk::DynamicState::BLEND_CONSTANTS, vk::DynamicState::STENCIL_REFERENCE];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let depth_info = format_info(key.depth);
        let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(colors)
            .depth_attachment_format(key.depth)
            .stencil_attachment_format(if depth_info.is_some_and(|info| info.stencil) { key.depth } else { vk::Format::UNDEFINED });
        let mut stages = vec![vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main")];
        if let Some(ps) = ps { stages.push(vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(ps).name(c"main")); }
        let info = vk::GraphicsPipelineCreateInfo::default().stages(&stages).vertex_input_state(&vertex_input).input_assembly_state(&assembly)
            .viewport_state(&viewport).rasterization_state(&rasterization).multisample_state(&multisample).depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend).dynamic_state(&dynamic).layout(layout).push_next(&mut rendering);
        let pipelines = unsafe { self.gpu.device.create_graphics_pipelines(self.pipeline_cache, &[info], Some(callbacks())) }
            .map_err(|(_, error)| self.command_error(opcode, format!("vkCreateGraphicsPipelines VS={} PS={}: {error}", key.vs, key.ps)))?;
        Ok(pipelines[0])
    }

    // ---- compute -------------------------------------------------------------------------------

    fn dispatch(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::Dispatch;
        let id = self.state.shaders[2];
        if id == 0 { return Err(self.command_error(opcode, "compute shader is not bound")); }
        self.ensure_shader(id, opcode)?;
        // The translator guesses storage formats from lane counts: specialize to the actual textures.
        let mut formats = Vec::new();
        let (reflection, pack, pack_size, module) = match self.resources.get(&id) {
            Some(Resource::Shader(ShaderResource { stage: 2, compiled: Some(compiled), .. })) => (Rc::clone(&compiled.reflection), compiled.pack.clone(), compiled.pack_size, compiled.module),
            _ => return Err(self.command_error(opcode, format!("shader {id} is not a compute shader"))),
        };
        for binding in &reflection.bindings {
            let BindingKind::StorageTexture { format, .. } = binding.kind else { continue };
            let Some(slot) = binding.binding.checked_sub(160).filter(|slot| *slot < 8) else { continue };
            let Some(Resource::View(view)) = self.resources.get(&self.state.uavs[2][slot as usize]) else { continue };
            if let Some(Resource::Texture(texture)) = self.resources.get(&view.resource) {
                if storage_format(format) != texture.format { formats.push((binding.binding, texture.format)); }
            }
        }
        let key = (id, formats);
        let pipeline = match self.compute_pipelines.get(&key) {
            Some(pipeline) => Rc::clone(pipeline),
            None => {
                let mut specialized = None;
                let mut reflection_used = Rc::clone(&reflection);
                let mut module_used = module;
                if !key.1.is_empty() {
                    let mut text = match self.resources.get(&id) { Some(Resource::Shader(ShaderResource { compiled: Some(compiled), .. })) => compiled.source.clone().unwrap_or_default(), _ => String::new() };
                    for (binding_number, actual) in &key.1 {
                        let binding = reflection.bindings.iter().find(|b| b.binding == *binding_number).expect("reflected binding");
                        let BindingKind::StorageTexture { format, .. } = binding.kind else { continue };
                        let actual = storage_format_from_native(*actual).ok_or_else(|| self.command_error(opcode, format!("texture format {actual:?} has no WGSL storage declaration")))?;
                        text = crate::shader_storage::specialize(&text, &binding.name, format, actual).map_err(|error| self.command_error(opcode, error))?;
                    }
                    reflection_used = Rc::new(reflect_wgsl(&text).map_err(|error| self.command_error(opcode, format!("specialized compute reflection: {error}")))?);
                    let spirv = self.shader_cache.compile(&text, 2, pack.is_some().then(|| pack_binding(2))).map_err(|error| self.command_error(opcode, error))?;
                    module_used = self.module(&spirv)?;
                    specialized = Some((Rc::clone(&reflection_used), module_used));
                }
                let bindings: Vec<(u32, vk::DescriptorType, vk::ShaderStageFlags)> = reflection_used.bindings.iter()
                    .filter(|binding| !(pack.is_some() && binding.kind == BindingKind::Uniform && binding.binding == crate::shader::PACK_BINDING))
                    .map(|binding| (binding.binding, descriptor_type(binding.kind), vk::ShaderStageFlags::COMPUTE)).collect();
                let set = self.layouts.set_layout(&self.gpu.device, bindings).map_err(RendererError::Vulkan)?;
                let pack_set = if pack.is_some() { self.layouts.set_layout(&self.gpu.device, vec![(0, vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC, vk::ShaderStageFlags::COMPUTE)]).map_err(RendererError::Vulkan)? } else { self.empty_set };
                let layout = self.layouts.pipeline_layout(&self.gpu.device, [set, self.empty_set, pack_set]).map_err(RendererError::Vulkan)?;
                let stage = vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(module_used).name(c"main");
                let raw = unsafe { self.gpu.device.create_compute_pipelines(self.pipeline_cache, &[vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout)], Some(callbacks())) }
                    .map_err(|(_, error)| self.command_error(opcode, format!("vkCreateComputePipelines: {error}")))?[0];
                let pipeline = Rc::new(ComputePipeline { raw, layout, set, pack_set, specialized });
                self.compute_pipelines.insert(key.clone(), Rc::clone(&pipeline));
                pipeline
            }
        };
        let mut uses = Vec::new();
        let mut buffers = Vec::new();
        // Descriptors follow the specialized reflection (same bindings, other storage formats).
        let used = pipeline.specialized.as_ref().map_or_else(|| Rc::clone(&reflection), |(specialized, _)| Rc::clone(specialized));
        let descriptors = self.stage_descriptors(id, 2, &used, &[], &mut uses, &mut buffers)?;
        let packed = match &pack { Some(members) => Some(self.pack_constants(2, members, pack_size)?), None => None };
        let set = self.descriptor_set(pipeline.set, descriptors)?;
        let pack_set = match packed {
            Some((buffer, _, range)) => Some(self.descriptor_set(pipeline.pack_set, vec![(0, vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC, Descriptor::Buffer { buffer, offset: 0, range })])?),
            _ => None,
        };
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        self.transition(cb, &uses, true)?;
        let device = self.gpu.device.clone();
        unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipeline.raw);
            device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, pipeline.layout, 0, &[set], &[]);
            if let Some((_, offset, _)) = packed {
                device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, pipeline.layout, 2, &[pack_set.expect("pack set")], &[offset as u32]);
            }
            device.cmd_dispatch(cb, p[0], p[1], p[2]);
        }
        let value = self.recording_value();
        for id in buffers { if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) { buffer.touch(value); } }
        // D3D serializes UAV access between dispatches.
        self.note_pending_write(vk::PipelineStageFlags2::COMPUTE_SHADER, vk::AccessFlags2::SHADER_STORAGE_WRITE);
        Ok(())
    }

    fn set_uavs(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let (stage, start, count) = (p[0] as usize, p[1] as usize, p[2] as usize);
        if stage >= 3 || start + count > 8 { return Err(self.command_error(Opcode::SetUavs, "UAV binding range is outside the three-stage 8-slot ABI")); }
        for i in 0..count {
            let id = p[3 + i];
            if let (Some(value), true) = (crate::uav_counter::reset(p[3 + count + i]), id != 0) {
                let view = match self.resources.get(&id) { Some(Resource::View(view)) => *view, _ => return Err(self.command_error(Opcode::SetUavs, "UAV slot is not a view")) };
                if view.is_buffer {
                    let counter = self.buffer(Opcode::SetUavs, view.resource)?.counter.as_ref().map(|(buffer, _)| *buffer).ok_or_else(|| self.command_error(Opcode::SetUavs, "UAV buffer has no counter"))?;
                    let cb = self.cb()?;
                    self.flush_pending_writes(cb);
                    unsafe { self.gpu.device.cmd_fill_buffer(cb, counter, 0, 4, value) };
                    self.note_pending_write(vk::PipelineStageFlags2::ALL_TRANSFER, vk::AccessFlags2::TRANSFER_WRITE);
                    let recording = self.recording_value();
                    if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&view.resource) { buffer.touch(recording); }
                }
            }
            self.state.uavs[stage][start + i] = id;
        }
        self.stage_epoch[stage] += 1;
        Ok(())
    }

    fn copy_counter(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::CopyCounter;
        let target = self.buffer(opcode, p[0])?;
        crate::uav_counter::copy_range(target.size, u64::from(p[1])).map_err(|error| self.command_error(opcode, error))?;
        let target = target.raw;
        let view = match self.resources.get(&p[2]) { Some(Resource::View(view)) if view.is_buffer => *view, _ => return Err(self.command_error(opcode, "counter source is not a buffer UAV")) };
        let counter = self.buffer(opcode, view.resource)?.counter.as_ref().map(|(buffer, _)| *buffer).ok_or_else(|| self.command_error(opcode, "counter source has no allocation"))?;
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        unsafe { self.gpu.device.cmd_copy_buffer(cb, counter, target, &[vk::BufferCopy { src_offset: 0, dst_offset: u64::from(p[1]), size: 4 }]) };
        self.note_pending_write(vk::PipelineStageFlags2::ALL_TRANSFER, vk::AccessFlags2::TRANSFER_WRITE);
        let value = self.recording_value();
        for id in [p[0], view.resource] { if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) { buffer.touch(value); } }
        Ok(())
    }

    fn clear_uav(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::ClearUav;
        let view = match self.resources.get(&p[0]) { Some(Resource::View(view)) => *view, _ => return Err(self.command_error(opcode, "UAV id is not a view")) };
        if !view.is_buffer { return self.clear_texture_uav(view, [p[2], p[3], p[4], p[5]]); }
        let buffer = self.buffer(opcode, view.resource)?;
        let (raw, size) = (buffer.raw, buffer.size.next_multiple_of(4));
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        if p[2..6].iter().all(|value| *value == p[2]) {
            unsafe { self.gpu.device.cmd_fill_buffer(cb, raw, 0, size, p[2]) };
        } else {
            let pattern: Vec<u8> = (0..size as usize / 4).flat_map(|i| p[2 + i % 4].to_le_bytes()).collect();
            let value = self.recording_value();
            let (staging, at, pointer) = self.staging.alloc(&mut self.gpu, size, 16, value)?;
            unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), pointer, pattern.len()) };
            unsafe { self.gpu.device.cmd_copy_buffer(cb, staging, raw, &[vk::BufferCopy { src_offset: at, dst_offset: 0, size }]) };
        }
        self.note_pending_write(vk::PipelineStageFlags2::ALL_TRANSFER, vk::AccessFlags2::TRANSFER_WRITE);
        let value = self.recording_value();
        if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&view.resource) { buffer.touch(value); }
        Ok(())
    }

    /// D3D ClearUnorderedAccessView on a texture: every layer of the view's mip, with the raw words the
    /// engine sent (float clears arrive as float bits, so one union serves float, unorm and integer formats).
    fn clear_texture_uav(&mut self, view: ViewResource, words: [u32; 4]) -> Result<(), RendererError> {
        let opcode = Opcode::ClearUav;
        self.materialize_clears(view.resource)?;
        let texture = self.texture(opcode, view.resource)?;
        if texture.info.depth || texture.info.bc || view.a >= texture.mip_levels { return Err(self.command_error(opcode, format!("UAV clear of {:?} mip {}", texture.format, view.a))); }
        let (raw, layers) = (texture.raw, texture.layers());
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        self.transition(cb, &[ImageUse { texture: view.resource, mips: (view.a, 1), layers: (0, layers), layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }], false)?;
        let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: view.a, level_count: 1, base_array_layer: 0, layer_count: layers };
        unsafe { self.gpu.device.cmd_clear_color_image(cb, raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearColorValue { uint32: words }, &[range]) };
        self.mark_written(view.resource);
        Ok(())
    }

    /// D3D ResolveSubresource: a multisampled colour subresource averaged into a single-sample one.
    fn resolve(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::Resolve;
        let (dst, dst_sub, src, src_sub) = (p[0], p[1], p[2], p[3]);
        self.materialize_clears(src)?;
        self.materialize_clears(dst)?;
        let (s, d) = (self.texture(opcode, src)?, self.texture(opcode, dst)?);
        if s.samples < 2 || d.samples != 1 || s.info.depth || s.format != d.format || s.dimension != vk::ImageType::TYPE_2D || d.dimension != vk::ImageType::TYPE_2D {
            return Err(self.command_error(opcode, format!("resolve of {:?} x{} into {:?} x{}", s.format, s.samples, d.format, d.samples)));
        }
        let (src_mip, src_layer, dst_mip, dst_layer) = (src_sub % s.mip_levels, src_sub / s.mip_levels, dst_sub % d.mip_levels, dst_sub / d.mip_levels);
        if src_layer >= s.layers() || dst_layer >= d.layers() { return Err(self.command_error(opcode, "resolve subresource is outside the texture")); }
        let (sw, sh, _) = s.mip_extent(src_mip);
        let (dw, dh, _) = d.mip_extent(dst_mip);
        let (src_raw, dst_raw) = (s.raw, d.raw);
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        self.transition(cb, &[ImageUse { texture: src, mips: (src_mip, 1), layers: (src_layer, 1), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE },
            ImageUse { texture: dst, mips: (dst_mip, 1), layers: (dst_layer, 1), layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }], false)?;
        let layers = |mip, layer| vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: mip, base_array_layer: layer, layer_count: 1 };
        let region = vk::ImageResolve { src_subresource: layers(src_mip, src_layer), src_offset: vk::Offset3D::default(), dst_subresource: layers(dst_mip, dst_layer), dst_offset: vk::Offset3D::default(),
            extent: vk::Extent3D { width: sw.min(dw), height: sh.min(dh), depth: 1 } };
        unsafe { self.gpu.device.cmd_resolve_image(cb, src_raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst_raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]) };
        self.mark_written(dst);
        Ok(())
    }

    // ---- copies, mips, proxies -------------------------------------------------------------------

    fn copy_buffers(&mut self, src: u32, dst: u32, src_offset: u64, dst_offset: u64, size: u64) -> Result<(), RendererError> {
        let (src_raw, dst_raw) = (self.buffer(Opcode::CopyRegion, src)?.raw, self.buffer(Opcode::CopyRegion, dst)?.raw);
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        unsafe { self.gpu.device.cmd_copy_buffer(cb, src_raw, dst_raw, &[vk::BufferCopy { src_offset, dst_offset, size }]) };
        self.note_pending_write(vk::PipelineStageFlags2::ALL_TRANSFER, vk::AccessFlags2::TRANSFER_WRITE);
        let value = self.recording_value();
        for id in [src, dst] { if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&id) { buffer.touch(value); } }
        Ok(())
    }

    fn copy_replaces_all_pixels(&self, src: u32, dst: u32, regions: &[vk::ImageCopy]) -> bool {
        let (Some(Resource::Texture(source)), Some(Resource::Texture(target))) = (self.resources.get(&src), self.resources.get(&dst)) else { return false; };
        if src == dst || target.info.depth || target.samples != 1 || regions.len() != target.mip_levels as usize
            || source.info.block_width != target.info.block_width || source.info.block_height != target.info.block_height { return false; }
        (0..target.mip_levels).all(|mip| {
            let (width, height, depth) = target.mip_extent(mip);
            regions.iter().any(|region| region.dst_subresource.mip_level == mip
                && region.dst_subresource.aspect_mask == vk::ImageAspectFlags::COLOR
                && region.dst_subresource.base_array_layer == 0 && region.dst_subresource.layer_count == target.layers()
                && region.dst_offset == vk::Offset3D::default()
                && region.extent == vk::Extent3D { width, height, depth })
        })
    }

    fn copy_textures(&mut self, src: u32, dst: u32, regions: &[vk::ImageCopy]) -> Result<(), RendererError> {
        let full_image = self.copy_replaces_all_pixels(src, dst, regions);
        if !full_image { self.materialize_clears(dst)?; }
        // A source restore must succeed before discarding the destination's old contents.
        self.ensure_texture_memory(src)?;
        let (src_raw, dst_raw) = (self.texture(Opcode::CopyRegion, src)?.raw, self.texture(Opcode::CopyRegion, dst)?.raw);
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        if full_image {
            self.overwrite_texture(dst)?;
            self.pending_clears.retain(|key, _| key.0 != dst);
        }
        let mut uses = Vec::new();
        for region in regions {
            let layers = |s: vk::ImageSubresourceLayers| (s.base_array_layer, s.layer_count);
            uses.push(ImageUse { texture: src, mips: (region.src_subresource.mip_level, 1), layers: layers(region.src_subresource), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE });
            uses.push(ImageUse { texture: dst, mips: (region.dst_subresource.mip_level, 1), layers: layers(region.dst_subresource), layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE });
        }
        self.transition(cb, &uses, false)?;
        unsafe { self.gpu.device.cmd_copy_image(cb, src_raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst_raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, regions) };
        self.mark_written(dst);
        Ok(())
    }

    fn copy_region(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::CopyRegion;
        let (dst, src) = (p[0], p[5]);
        self.materialize_clears(src)?;
        match (self.resources.get(&src), self.resources.get(&dst)) {
            (Some(Resource::Buffer(s)), Some(Resource::Buffer(d))) => {
                let src_offset = if p[7] != 0 { u64::from(p[8]) } else { 0 };
                let dst_offset = u64::from(p[2]);
                let size = if p[7] != 0 { u64::from(p[11].saturating_sub(p[8])) } else { s.size.min(d.size) };
                if size == 0 || src_offset + size > s.size || dst_offset + size > d.size { return Err(self.command_error(opcode, "buffer copy range is outside its resources")); }
                if src_offset % 4 != 0 || dst_offset % 4 != 0 || size & !3 == 0 { return Err(self.command_error(opcode, "buffer copy is not 4-byte aligned")); }
                self.copy_buffers(src, dst, src_offset, dst_offset, size & !3)
            }
            (Some(Resource::Texture(s)), Some(Resource::Texture(d))) => {
                // D3D copies between formats of one block size, block-compressed or not (a compute shader
                // writes BC blocks as R32G32[B32A32]_UINT texels); vkCmdCopyImage maps one block to one texel.
                if s.info.block_bytes != d.info.block_bytes || s.info.depth != d.info.depth || s.dimension != d.dimension || s.samples != d.samples {
                    return Err(self.unsupported(opcode, &format!("texture copy from {:?} {:?} x{} to {:?} {:?} x{}", s.format, s.dimension, s.samples, d.format, d.dimension, d.samples)));
                }
                let (src_mip, dst_mip) = (p[6] % s.mip_levels, p[1] % d.mip_levels);
                let is_3d = s.dimension == vk::ImageType::TYPE_3D;
                let (src_layer, dst_layer) = if is_3d { (0, 0) } else { (p[6] / s.mip_levels, p[1] / d.mip_levels) };
                if src_layer >= s.layers() || dst_layer >= d.layers() { return Err(self.command_error(opcode, "texture subresource is outside the source or destination")); }
                let (sw, sh, sd) = s.mip_extent(src_mip);
                let (dw, dh, dd) = d.mip_extent(dst_mip);
                let has_box = p[7] != 0;
                let (sx, sy, sz) = if has_box { (p[8], p[9], p[10]) } else { (0, 0, 0) };
                let (w, h, depth) = if has_box { (p[11].saturating_sub(sx), p[12].saturating_sub(sy), p[13].saturating_sub(sz)) } else { (sw, sh, sd) };
                // The extent is in source texels; the destination receives the same number of blocks.
                let (dst_w, dst_h) = (w.div_ceil(s.info.block_width) * d.info.block_width, h.div_ceil(s.info.block_height) * d.info.block_height);
                let fits = |offset: u32, size: u32, extent: u32, block: u32| offset + size <= extent.next_multiple_of(block);
                if w == 0 || h == 0 || depth == 0 || sx + w > sw || sy + h > sh || sz + depth > sd
                    || !fits(p[2], dst_w, dw, d.info.block_width) || !fits(p[3], dst_h, dh, d.info.block_height) || p[4] + depth > dd {
                    return Err(self.command_error(opcode, "texture copy box is outside the source or destination mip"));
                }
                let aspect = s.aspect();
                let region = vk::ImageCopy {
                    src_subresource: vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: src_mip, base_array_layer: src_layer, layer_count: 1 },
                    src_offset: vk::Offset3D { x: sx as i32, y: sy as i32, z: sz as i32 },
                    dst_subresource: vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: dst_mip, base_array_layer: dst_layer, layer_count: 1 },
                    dst_offset: vk::Offset3D { x: p[2] as i32, y: p[3] as i32, z: p[4] as i32 },
                    extent: vk::Extent3D { width: w, height: h, depth },
                };
                self.copy_textures(src, dst, &[region])
            }
            _ => Err(self.command_error(opcode, "copy resources must both be buffers or both be textures")),
        }
    }

    fn copy_resource(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::CopyResource;
        let (dst, src) = (p[0], p[1]);
        self.materialize_clears(src)?;
        match (self.resources.get(&src), self.resources.get(&dst)) {
            (Some(Resource::Buffer(s)), Some(Resource::Buffer(d))) => {
                let size = s.size.min(d.size) & !3;
                if size == 0 { return Err(self.command_error(opcode, "buffer is smaller than the 4-byte GPU alignment")); }
                self.copy_buffers(src, dst, 0, 0, size)
            }
            (Some(Resource::Texture(s)), Some(Resource::Texture(d))) => {
                if s.info.block_bytes != d.info.block_bytes || s.info.depth != d.info.depth || s.dimension != d.dimension || s.samples != d.samples {
                    return Err(self.unsupported(opcode, &format!("texture copy from {:?} {:?} x{} to {:?} {:?} x{}", s.format, s.dimension, s.samples, d.format, d.dimension, d.samples)));
                }
                let aspect = s.aspect();
                let layers = s.layers().min(d.layers());
                let regions: Vec<vk::ImageCopy> = (0..s.mip_levels.min(d.mip_levels)).map(|mip| {
                    let (sw, sh, sd) = s.mip_extent(mip);
                    let (dw, dh, dd) = d.mip_extent(mip);
                    // In source texels, covering the blocks both mips have.
                    let width = (sw.div_ceil(s.info.block_width).min(dw.div_ceil(d.info.block_width)) * s.info.block_width).min(sw);
                    let height = (sh.div_ceil(s.info.block_height).min(dh.div_ceil(d.info.block_height)) * s.info.block_height).min(sh);
                    let sub = vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: mip, base_array_layer: 0, layer_count: layers };
                    vk::ImageCopy { src_subresource: sub, src_offset: vk::Offset3D::default(), dst_subresource: sub, dst_offset: vk::Offset3D::default(), extent: vk::Extent3D { width, height, depth: sd.min(dd) } }
                }).collect();
                self.copy_textures(src, dst, &regions)
            }
            _ => Err(self.command_error(opcode, "copy resources must both be buffers or both be textures")),
        }
    }

    /// D3D GenerateMips: each level is a linear downsample of the one above.
    fn generate_mips(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::GenerateMips;
        let view = match self.resources.get(&p[0]) { Some(Resource::View(view)) if !view.is_buffer => *view, _ => return Err(self.command_error(opcode, "mipmap source must be a texture SRV")) };
        self.materialize_clears(view.resource)?;
        let texture = self.texture(opcode, view.resource)?;
        if texture.dimension != vk::ImageType::TYPE_2D || texture.samples != 1 || texture.info.depth || texture.info.bc {
            return Err(RendererError::Unsupported { opcode, detail: format!("mipmap generation for {:?}", texture.format) });
        }
        let passes = crate::mipmaps::passes(texture.mip_levels, texture.layers(), view.a, view.b, view.c, view.d).map_err(|error| self.command_error(opcode, error))?;
        if passes.is_empty() { return Ok(()); }
        let raw = texture.raw;
        let extent = |mip: u32| { let (w, h, _) = texture.mip_extent(mip); vk::Offset3D { x: w as i32, y: h as i32, z: 1 } };
        let blits: Vec<(crate::mipmaps::Pass, vk::ImageBlit)> = passes.iter().map(|pass| (*pass, vk::ImageBlit {
            src_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: pass.source_mip, base_array_layer: pass.layer, layer_count: 1 },
            src_offsets: [vk::Offset3D::default(), extent(pass.source_mip)],
            dst_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: pass.destination_mip, base_array_layer: pass.layer, layer_count: 1 },
            dst_offsets: [vk::Offset3D::default(), extent(pass.destination_mip)],
        })).collect();
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        for (pass, blit) in blits {
            let uses = [
                ImageUse { texture: view.resource, mips: (pass.source_mip, 1), layers: (pass.layer, 1), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE },
                ImageUse { texture: view.resource, mips: (pass.destination_mip, 1), layers: (pass.layer, 1), layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE },
            ];
            self.transition(cb, &uses, false)?;
            unsafe { self.gpu.device.cmd_blit_image(cb, raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[blit], vk::Filter::LINEAR) };
        }
        self.mark_written(view.resource);
        Ok(())
    }

    /// A Depth32 copy of a colour texture's red channel, for comparison sampling (the browser worker's floatToDepthProxy).
    fn depth_proxy(&mut self, source: u32) -> Result<u32, RendererError> {
        let texture = self.texture(Opcode::Draw, source)?;
        if texture.samples != 1 || texture.dimension != vk::ImageType::TYPE_2D || is_integer(texture.format) {
            return Err(self.command_error(Opcode::Draw, format!("comparison sampling of {:?} {:?} x{} has no depth conversion", texture.format, texture.dimension, texture.samples)));
        }
        self.refreshed_proxy(ProxyCopy::FloatToDepth, source)
    }

    /// An R32F copy of a depth texture's depth plane, for float reads.
    fn float_proxy(&mut self, source: u32) -> Result<u32, RendererError> {
        let texture = self.texture(Opcode::Draw, source)?;
        if texture.samples != 1 || texture.dimension != vk::ImageType::TYPE_2D { return Err(self.unsupported(Opcode::Draw, "float reads of multisampled or non-2D depth")); }
        self.refreshed_proxy(ProxyCopy::DepthToFloat, source)
    }

    /// The proxy of `source` for `kind`, rewritten on the GPU if the source changed since the last rewrite.
    fn refreshed_proxy(&mut self, kind: ProxyCopy, source: u32) -> Result<u32, RendererError> {
        let texture = self.texture(Opcode::Draw, source)?;
        let (generation, width, height, mips, layers, format) = (texture.generation, texture.width, texture.height, texture.mip_levels, texture.layers(), texture.format);
        let proxies = match kind { ProxyCopy::FloatToDepth => &self.depth_proxies, ProxyCopy::DepthToFloat => &self.float_proxies };
        let existing = proxies.get(&source).copied();
        if let Some((proxy, built)) = existing { if built == generation { return Ok(proxy); } }
        let proxy = match existing {
            Some((proxy, _)) => proxy,
            None => {
                let id = self.next_internal_id;
                self.next_internal_id += 1;
                let texture = self.build_texture(&[id, 2, width, height, 1, mips, layers, kind.proxy_dxgi(), kind.proxy_bind(), 1])?;
                self.resources.insert(id, Resource::Texture(Box::new(texture)));
                id
            }
        };
        let (pipeline, layout, set_layout) = self.proxy_pipeline(kind)?;
        let proxy_format = self.texture(Opcode::Draw, proxy)?.format;
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        let (source_layout, proxy_layout, source_aspect, proxy_aspect) = match kind {
            ProxyCopy::FloatToDepth => (vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL, vk::ImageAspectFlags::COLOR, vk::ImageAspectFlags::DEPTH),
            ProxyCopy::DepthToFloat => (vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageAspectFlags::DEPTH, vk::ImageAspectFlags::COLOR),
        };
        self.transition(cb, &[ImageUse { texture: source, mips: (0, mips), layers: (0, layers), layout: source_layout, stages: vk::PipelineStageFlags2::NONE }, ImageUse { texture: proxy, mips: (0, mips), layers: (0, layers), layout: proxy_layout, stages: vk::PipelineStageFlags2::NONE }], true)?;
        let device = Rc::clone(&self.device);
        for layer in 0..layers {
            for mip in 0..mips {
                let view = |format, aspect| ViewKey { format, kind: vk::ImageViewType::TYPE_2D, aspect, base_mip: mip, mips: 1, base_layer: layer, layers: 1, swizzle: 0 };
                let source_view = self.image_view(source, view(format, source_aspect))?;
                let proxy_view = self.image_view(proxy, view(proxy_format, proxy_aspect))?;
                let set = self.descriptor_set(set_layout, vec![(0, vk::DescriptorType::SAMPLED_IMAGE, Descriptor::Image { view: source_view, layout: source_layout })])?;
                let (w, h) = ((width >> mip).max(1), (height >> mip).max(1));
                let target = [vk::RenderingAttachmentInfo::default().image_view(proxy_view).image_layout(proxy_layout).load_op(vk::AttachmentLoadOp::DONT_CARE).store_op(vk::AttachmentStoreOp::STORE)];
                let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: vk::Extent2D { width: w, height: h } };
                let info = vk::RenderingInfo::default().render_area(area).layer_count(1);
                let info = match kind { ProxyCopy::FloatToDepth => info.depth_attachment(&target[0]), ProxyCopy::DepthToFloat => info.color_attachments(&target) };
                unsafe {
                    device.cmd_begin_rendering(cb, &info);
                    device.cmd_set_viewport(cb, 0, &[vk::Viewport { x: 0.0, y: 0.0, width: w as f32, height: h as f32, min_depth: 0.0, max_depth: 1.0 }]);
                    device.cmd_set_scissor(cb, 0, &[area]);
                    device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                    device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, layout, 1, &[set], &[]);
                    device.cmd_draw(cb, 3, 1, 0, 0);
                    device.cmd_end_rendering(cb);
                }
            }
        }
        match kind { ProxyCopy::FloatToDepth => &mut self.depth_proxies, ProxyCopy::DepthToFloat => &mut self.float_proxies }.insert(source, (proxy, generation));
        self.stats.proxy_copies += 1;
        Ok(proxy)
    }

    /// The full-screen pipeline that rewrites a texture into a proxy of `kind`, created on first use.
    fn proxy_pipeline(&mut self, kind: ProxyCopy) -> Result<(vk::Pipeline, vk::PipelineLayout, vk::DescriptorSetLayout), RendererError> {
        if let Some(cached) = self.proxy_pipelines[kind as usize] { return Ok(cached); }
        let set_layout = self.layouts.set_layout(&self.gpu.device, vec![(0, vk::DescriptorType::SAMPLED_IMAGE, vk::ShaderStageFlags::FRAGMENT)]).map_err(RendererError::Vulkan)?;
        let layout = self.layouts.pipeline_layout(&self.gpu.device, [self.empty_set, set_layout, self.empty_set]).map_err(RendererError::Vulkan)?;
        let vs = self.module(&self.shader_cache.compile(PROXY_VERTEX, 0, None).map_err(RendererError::Vulkan)?)?;
        let fs = self.module(&self.shader_cache.compile(kind.fragment(), 1, None).map_err(RendererError::Vulkan)?)?;
        let stages = [vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main"),
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main")];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL).cull_mode(vk::CullModeFlags::NONE).line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let to_depth = kind == ProxyCopy::FloatToDepth;
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default().depth_test_enable(to_depth).depth_write_enable(to_depth).depth_compare_op(vk::CompareOp::ALWAYS);
        let colour = [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::R)];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(if to_depth { &[] } else { &colour });
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let colour_formats = [vk::Format::R32_SFLOAT];
        let mut rendering = if to_depth { vk::PipelineRenderingCreateInfo::default().depth_attachment_format(vk::Format::D32_SFLOAT) } else { vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&colour_formats) };
        let info = vk::GraphicsPipelineCreateInfo::default().stages(&stages).vertex_input_state(&vertex_input).input_assembly_state(&assembly)
            .viewport_state(&viewport).rasterization_state(&rasterization).multisample_state(&multisample).depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend).dynamic_state(&dynamic).layout(layout).push_next(&mut rendering);
        let created = unsafe { self.gpu.device.create_graphics_pipelines(self.pipeline_cache, &[info], Some(callbacks())) };
        unsafe { self.gpu.device.destroy_shader_module(vs, Some(callbacks())); self.gpu.device.destroy_shader_module(fs, Some(callbacks())); }
        let pipeline = created.map_err(|(_, error)| RendererError::Vulkan(format!("proxy pipeline {kind:?}: {error}")))?[0];
        self.proxy_pipelines[kind as usize] = Some((pipeline, layout, set_layout));
        Ok((pipeline, layout, set_layout))
    }

    /// `debug.gta5.show_rt`: copies a color target right after the matched draw so Present can show it.
    fn snapshot(&mut self, source: u32) -> Result<(), RendererError> {
        let opcode = Opcode::Draw;
        let texture = self.texture(opcode, source)?;
        let (width, height, raw) = (texture.width, texture.height, texture.raw);
        if texture.info.depth { return Ok(()); }
        // Shown at the screen's size, scaled, so half- and quarter-size targets can be inspected too.
        let (shown_width, shown_height) = self.surface.as_ref().filter(|surface| surface.extent.width > 0).map_or((width, height), |surface| (surface.extent.width, surface.extent.height));
        let fits = self.shown_texture.and_then(|id| self.texture(opcode, id).ok()).is_some_and(|shown| (shown.width, shown.height) == (shown_width, shown_height));
        if !fits {
            if let Some(old) = self.shown_texture.take() { let _ = self.destroy(old); }
            let id = self.next_internal_id;
            self.next_internal_id += 1;
            let texture = self.build_texture(&[id, 2, shown_width, shown_height, 1, 1, 1, 87, BIND_SRV, 1])?;
            self.resources.insert(id, Resource::Texture(Box::new(texture)));
            self.shown_texture = Some(id);
        }
        let shown = self.shown_texture.expect("snapshot texture");
        let shown_raw = self.texture(opcode, shown)?.raw;
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        self.transition(cb, &[ImageUse { texture: source, mips: (0, 1), layers: (0, 1), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }, ImageUse { texture: shown, mips: (0, 1), layers: (0, 1), layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }], false)?;
        let layers = vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 };
        let corner = vk::Offset3D { x: width as i32, y: height as i32, z: 1 };
        let shown_corner = vk::Offset3D { x: shown_width as i32, y: shown_height as i32, z: 1 };
        let region = vk::ImageBlit { src_subresource: layers, src_offsets: [vk::Offset3D::default(), corner], dst_subresource: layers, dst_offsets: [vk::Offset3D::default(), shown_corner] };
        unsafe { self.gpu.device.cmd_blit_image(cb, raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, shown_raw, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region], vk::Filter::NEAREST) };
        if let (Some(x), Some(y)) = (debug_number("probe_x"), debug_number("probe_y")) {
            if self.probe.is_none() {
                let (buffer, memory) = self.gpu.create_buffer("show_rt pixel probe", 256, vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::GpuToCpu).map_err(RendererError::Vulkan)?;
                self.probe = Some((buffer, memory, 0));
            }
            if let Some((buffer, _, probed)) = self.probe.as_mut() {
                *probed = source;
                let copy = vk::BufferImageCopy { buffer_offset: 0, buffer_row_length: 0, buffer_image_height: 0, image_subresource: layers,
                    image_offset: vk::Offset3D { x: x.min(width - 1) as i32, y: y.min(height - 1) as i32, z: 0 }, image_extent: vk::Extent3D { width: 1, height: 1, depth: 1 } };
                unsafe { self.gpu.device.cmd_copy_image_to_buffer(cb, raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, *buffer, &[copy]) };
            }
        }
        Ok(())
    }


    // ---- readback, fences, queries ---------------------------------------------------------------

    fn fence<M: SharedMemory + ?Sized>(&mut self, p: &[u32], memory: &M) -> Result<(), RendererError> {
        let address = usize::try_from(u64::from(p[0]) | u64::from(p[1]) << 32).map_err(|_| self.command_error(Opcode::Fence, "fence address does not fit host address"))?;
        if address % 4 != 0 { return Err(RendererError::SharedMemory(SharedMemoryError::Misaligned { address })); }
        if address + 4 > memory.len() { return Err(RendererError::MemoryRange { address, end: address + 4, length: memory.len() }); }
        self.after_recorded_work(Completion::Word { address, word: p[2], result: None });
        self.complete(memory)
    }

    fn readback<M: SharedMemory + ?Sized>(&mut self, p: &[u32], memory: &M) -> Result<(), RendererError> {
        let opcode = Opcode::Readback;
        let address = usize::try_from(u64::from(p[9]) | u64::from(p[10]) << 32).map_err(|_| self.command_error(opcode, "readback address does not fit host address"))?;
        let requested = p[14] as usize;
        let is_buffer = p[13] != 0;
        let flag_address = u64::from(p[15]) | u64::from(p[16]) << 32;
        let flag = if flag_address != 0 {
            let flag = usize::try_from(flag_address).map_err(|_| self.command_error(opcode, "readback completion address does not fit host address"))?;
            if flag + 4 > memory.len() { return Err(RendererError::MemoryRange { address: flag, end: flag + 4, length: memory.len() }); }
            Some(flag)
        } else { None };
        self.materialize_clears(p[0])?;
        let (mut row_bytes, mut rows, mut slices) = (0usize, 0usize, 1usize);
        let size;
        let image_copy;
        if is_buffer {
            let buffer = self.buffer(opcode, p[0])?;
            let source_offset = if p[2] != 0 { u64::from(p[3]) } else { 0 };
            let bytes = requested.min(buffer.size.saturating_sub(source_offset) as usize);
            if address + bytes > memory.len() { return Err(RendererError::MemoryRange { address, end: address + bytes, length: memory.len() }); }
            size = (bytes as u64).next_multiple_of(4).max(4);
            if source_offset + size > buffer.size.next_multiple_of(4) { return Err(self.command_error(opcode, "readback exceeds source buffer")); }
            image_copy = Some((None, source_offset));
        } else {
            let texture = self.texture(opcode, p[0])?;
            if texture.samples != 1 || texture.info.depth { return Err(RendererError::Unsupported { opcode, detail: "multisampled and depth texture readback".into() }); }
            let (mip, layer) = (p[1] % texture.mip_levels, p[1] / texture.mip_levels);
            let (mw, mh, md) = texture.mip_extent(mip);
            let has_box = p[2] != 0;
            let (x, y, z) = if has_box { (p[3], p[4], p[5]) } else { (0, 0, 0) };
            let (w, h, d) = if has_box { (p[6].saturating_sub(x), p[7].saturating_sub(y), p[8].saturating_sub(z)) } else { (mw, mh, md) };
            let info = texture.info;
            if w == 0 || h == 0 || d == 0 || x + w > mw || y + h > mh || z + d > md { return Err(self.command_error(opcode, "texture readback box is outside the source mip")); }
            if x % info.block_width != 0 || y % info.block_height != 0 { return Err(self.command_error(opcode, "texture readback origin is not block aligned")); }
            let is_3d = texture.dimension == vk::ImageType::TYPE_3D;
            if !is_3d && (layer >= texture.layers() || d != 1) { return Err(self.command_error(opcode, "texture array readback subresource is outside the source")); }
            row_bytes = (w.div_ceil(info.block_width) * info.block_bytes) as usize;
            rows = h.div_ceil(info.block_height) as usize;
            slices = d as usize;
            size = (row_bytes * rows * slices) as u64;
            image_copy = Some((Some(vk::BufferImageCopy {
                buffer_offset: 0, buffer_row_length: 0, buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: mip, base_array_layer: if is_3d { 0 } else { layer }, layer_count: 1 },
                image_offset: vk::Offset3D { x: x as i32, y: y as i32, z: z as i32 }, image_extent: vk::Extent3D { width: w.min(mw - x), height: h.min(mh - y), depth: d },
            }), 0));
        }
        let copy = ReadbackCopy { address, requested, is_buffer, row_bytes, rows, slices, output_row_pitch: p[11] as usize, output_slice_pitch: p[12] as usize, flag };
        copy.validate(memory.len())?;
        let (staging, staging_memory) = self.gpu.create_buffer("gta5 readback", size, vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::GpuToCpu).map_err(RendererError::Vulkan)?;
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        match image_copy {
            Some((Some(region), _)) => {
                let usage = [ImageUse { texture: p[0], mips: (region.image_subresource.mip_level, 1), layers: (region.image_subresource.base_array_layer, 1), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }];
                self.transition(cb, &usage, false)?;
                let raw = self.texture(opcode, p[0])?.raw;
                unsafe { self.gpu.device.cmd_copy_image_to_buffer(cb, raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, staging, &[region]) };
            }
            Some((None, source_offset)) => {
                let raw = self.buffer(opcode, p[0])?.raw;
                unsafe { self.gpu.device.cmd_copy_buffer(cb, raw, staging, &[vk::BufferCopy { src_offset: source_offset, dst_offset: 0, size }]) };
                let value = self.recording_value();
                if let Some(Resource::Buffer(buffer)) = self.resources.get_mut(&p[0]) { buffer.touch(value); }
            }
            None => unreachable!("readback has a source"),
        }
        let barrier = [vk::MemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).src_access_mask(vk::AccessFlags2::TRANSFER_WRITE).dst_stage_mask(vk::PipelineStageFlags2::HOST).dst_access_mask(vk::AccessFlags2::HOST_READ)];
        unsafe { self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().memory_barriers(&barrier)) };
        let blocking = flag.is_none();
        self.after_recorded_work(Completion::Readback { buffer: staging, memory: staging_memory, copy });
        if blocking {
            self.submit(opcode, None)?;
            let value = self.gpu.submitted;
            let waited = Instant::now();
            self.gpu.wait_for(value, GPU_WAIT_TIMEOUT).map_err(RendererError::Vulkan)?;
            self.stats.gpu_wait += waited.elapsed();
        }
        self.complete(memory)
    }

    fn create_query(&mut self, p: &[u32]) -> Result<(), RendererError> {
        if !matches!(p[1], 0 | 1 | 2 | 5) { return Err(self.unsupported(Opcode::CreateQuery, &format!("query type {} has no safe native result translation", p[1]))); }
        self.insert(Opcode::CreateQuery, p[0], Resource::Query(QueryResource { query_type: p[1], address: u64::from(p[2]) | u64::from(p[3]) << 32 }))
    }

    fn begin_query(&mut self, p: &[u32]) -> Result<(), RendererError> {
        if self.active_query.is_some() { return Err(self.command_error(Opcode::BeginQuery, "a query is already active")); }
        match self.resources.get(&p[0]) {
            Some(Resource::Query(query)) if matches!(query.query_type, 1 | 5) => {}
            Some(Resource::Query(_)) => return Err(self.unsupported(Opcode::BeginQuery, "only occlusion queries are supported")),
            _ => return Err(self.command_error(Opcode::BeginQuery, "resource is not a query")),
        }
        if !self.gpu.host_query_reset { return Err(self.unsupported(Opcode::BeginQuery, "occlusion queries need host query reset")); }
        self.active_query = Some((p[0], Vec::new()));
        Ok(())
    }

    /// The worker stores a query's result at +8 and publishes the sequence at +0 once the GPU produced it.
    fn end_query<M: SharedMemory + ?Sized>(&mut self, p: &[u32], memory: &M) -> Result<(), RendererError> {
        let (query_id, seq) = (p[0], p[1]);
        let (address, query_type) = match self.resources.get(&query_id) { Some(Resource::Query(query)) => (query.address, query.query_type), _ => return Err(self.command_error(Opcode::EndQuery, "resource is not a query")) };
        let address = usize::try_from(address).map_err(|_| self.command_error(Opcode::EndQuery, "query result address does not fit host address"))?;
        if address % 4 != 0 { return Err(RendererError::SharedMemory(SharedMemoryError::Misaligned { address })); }
        if address + 16 > memory.len() { return Err(RendererError::MemoryRange { address, end: address + 16, length: memory.len() }); }
        match query_type {
            0 => self.after_recorded_work(Completion::Word { address, word: seq, result: Some(1) }),
            2 => {
                let slot = self.timestamps.reserve(&self.gpu.device)?;
                let (pool, index) = self.timestamps.location(slot);
                let cb = self.recording_cb()?;
                unsafe { self.gpu.device.cmd_write_timestamp2(cb, vk::PipelineStageFlags2::ALL_COMMANDS, pool, index) };
                self.after_recorded_work(Completion::Query { slots: vec![slot], timestamp: true, address, seq });
            }
            _ => {
                let (active, slots) = self.active_query.take().ok_or_else(|| self.command_error(Opcode::EndQuery, "no occlusion query is active"))?;
                if active != query_id {
                    self.active_query = Some((active, slots));
                    return Err(self.command_error(Opcode::EndQuery, format!("active query is {active}, command ended {query_id}")));
                }
                if let Some(recording) = self.recording.as_mut() {
                    if let Some(slot) = recording.open_query.take() {
                        let (pool, index) = self.occlusion.location(slot);
                        unsafe { self.gpu.device.cmd_end_query(recording.cb, pool, index) };
                    }
                }
                self.after_recorded_work(Completion::Query { slots, timestamp: false, address, seq });
            }
        }
        self.complete(memory)
    }

    // ---- present -------------------------------------------------------------------------------

    fn present(&mut self, p: &[u32]) -> Result<(), RendererError> {
        let opcode = Opcode::Present;
        self.report.presents = self.report.presents.saturating_add(1);
        let back_buffer = p[0];
        if self.report.presents % 8 == 0 {
            let show_rt = debug_number("show_rt").filter(|slot| (1..=4).contains(slot)).zip(debug_string("show_after").filter(|after| !after.is_empty()));
            if show_rt != self.show_rt { log_line(&format!("show_rt now {show_rt:?}")); }
            let split_after = debug_string("split_after").filter(|after| !after.is_empty());
            if split_after != self.split_after { log_line(&format!("split_after now {split_after:?}")); }
            self.split_after = split_after;
            let skip_ps = debug_string("skip_ps").filter(|skip| !skip.is_empty());
            if skip_ps != self.skip_ps { log_line(&format!("skip_ps now {skip_ps:?}")); }
            self.skip_ps = skip_ps;
            let frame_interval = frame_interval();
            if frame_interval != self.frame_interval { log_line(&format!("frame cap now {frame_interval:?}")); }
            self.frame_interval = frame_interval;
            let dump_in = debug_number("dump_in");
            if dump_in != self.dump_in {
                self.dump_in = dump_in;
                if let Some(n) = dump_in.filter(|n| *n > 0) {
                    self.dump_frame = Some(self.frames_since_world + n);
                    log_line(&format!("dump_in {n}: dumping world frame {}", self.frames_since_world + n));
                }
            }
            self.show_rt = show_rt;
            if self.show_rt.is_none() { self.snapshot_source = None; }
            if let Some((_, memory, probed)) = &self.probe {
                if let Some(bytes) = memory.mapped_slice() { log_line(&format!("probe tex{probed}: {:02x?}", &bytes[..16])); }
            }
        }
        let source = match self.shown_texture.filter(|id| self.show_rt.is_some() && self.resources.contains_key(id)) {
            Some(id) if self.texture(opcode, id).ok().map(|t| (t.width, t.height)) == self.texture(opcode, back_buffer).ok().map(|t| (t.width, t.height)) => id,
            _ => back_buffer,
        };
        self.materialize_clears(source)?;
        let texture = self.texture(opcode, source)?;
        if texture.info.depth { return Err(self.command_error(opcode, "present target is not a color texture")); }
        let (width, height, format, raw) = (texture.width, texture.height, texture.format, texture.raw);
        if self.surface.is_none() { return Err(self.unsupported(opcode, "Android Surface/ANativeWindow has not been attached")); }
        let srgb = is_srgb(format);
        let reconfigure = self.surface.as_ref().is_some_and(|surface| !surface.is_configured() || surface.extent.width != width || surface.extent.height != height || is_srgb(surface.format) != srgb);
        if reconfigure {
            self.submit(opcode, None)?;
            let surface = self.surface.as_mut().expect("surface");
            surface.configure(&mut self.gpu, width, height, srgb)?;
        }
        let fps = self.frame_interval.map_or(0, |interval| (1.0 / interval.as_secs_f64()).round() as u32);
        if self.voted_fps != Some(fps) {
            let result = self.surface.as_ref().expect("surface").set_frame_rate(fps);
            log_line(&format!("window frame rate {fps}: {result}"));
            self.voted_fps = Some(fps);
        }
        let now = Instant::now();
        // The first load stays uncapped: the engine streams part of it per frame (18.1 s capped vs ~14 s).
        let cap = self.frame_interval.filter(|_| self.report.world);
        self.paced_from = Some(match (cap, self.paced_from) {
            (Some(interval), Some(from)) if now < from + interval => {
                std::thread::sleep(from + interval - now);
                self.stats.paced += from + interval - now;
                self.paced_total += from + interval - now;
                from + interval
            }
            _ => now,
        });
        let value = self.recording_value();
        let waited = Instant::now();
        let acquired = self.surface.as_mut().expect("surface").acquire(&mut self.gpu, value);
        self.stats.gpu_wait += waited.elapsed();
        let frame = match acquired {
            Ok(frame) => frame,
            Err(error) if error.is_recoverable() => {
                // Android reports lost or outdated buffers while the SurfaceView resizes: drop this frame.
                if let Some(surface) = self.surface.as_mut() { let _ = surface.configure(&mut self.gpu, width, height, srgb); }
                self.submit(opcode, None)?;
                return self.after_present();
            }
            Err(error) => return Err(error.into()),
        };
        let cb = self.cb()?;
        self.flush_pending_writes(cb);
        self.transition(cb, &[ImageUse { texture: source, mips: (0, 1), layers: (0, 1), layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL, stages: vk::PipelineStageFlags2::NONE }], false)?;
        let to_transfer = [vk::ImageMemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED).new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).image(frame.image)
            .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 })];
        let corner = vk::Offset3D { x: width as i32, y: height as i32, z: 1 };
        let layers = vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 };
        let blit = vk::ImageBlit { src_subresource: layers, src_offsets: [vk::Offset3D::default(), corner], dst_subresource: layers, dst_offsets: [vk::Offset3D::default(), corner] };
        let to_present = [vk::ImageMemoryBarrier2::default().src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER).src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).new_layout(vk::ImageLayout::PRESENT_SRC_KHR).image(frame.image)
            .subresource_range(to_transfer[0].subresource_range)];
        unsafe {
            self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().image_memory_barriers(&to_transfer));
            self.gpu.device.cmd_blit_image(cb, raw, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, frame.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[blit], vk::Filter::NEAREST);
            self.gpu.device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().image_memory_barriers(&to_present));
        }
        self.submit(opcode, Some((frame.acquired, frame.rendered)))?;
        let presented = self.surface.as_mut().expect("surface").present(&self.gpu, &frame);
        if let Err(error) = presented {
            if !error.is_recoverable() { return Err(error.into()); }
            if let Some(surface) = self.surface.as_mut() { let _ = surface.configure(&mut self.gpu, width, height, srgb); }
        }
        self.after_present()
    }

    fn after_present(&mut self) -> Result<(), RendererError> {
        self.rotate_slot()?;
        if self.report.world { self.frames_since_world += 1; }
        if let Some(mut out) = self.dump_out.get_mut().take() {
            if let Err(error) = out.flush() { log_line(&format!("dump file flush failed: {error}")); } else { log_line("dump file written"); }
        }
        let now = Instant::now();
        if !self.report.world && self.world.present(self.report.draws, now) {
            self.report.world = true;
            log_line("world shown");
        }
        if heap_purge_due(self.purge_heap, self.heap_purged_at.elapsed(), self.stats.frame_pipelines.len()) {
            let rss = process_rss_kib();
            let live = malloc_in_use();
            let started = Instant::now();
            let result = purge_unused_heap();
            let elapsed = started.elapsed();
            self.heap_purged_at = Instant::now();
            log_line(&format!("[RAM-purge] result {result}, {:.2} ms; RSS KiB {:?} -> {:?}, malloc live MiB {} -> {}",
                elapsed.as_secs_f64() * 1000.0, rss, process_rss_kib(), live >> 20, malloc_in_use() >> 20));
        }
        if let Some(line) = self.stats.present(now) && logging() {
            log_line(&line);
            // High-FPS loading frame lists can fill Android's log limit before the RAM counters.
            log_line(&self.memory_summary());
            if self.pass_frames > 0 {
                let frames = f64::from(self.pass_frames);
                let total: u64 = self.pass_times.values().map(|(ns, _)| ns).sum();
                let mut passes: Vec<(&String, &(u64, u32))> = self.pass_times.iter().collect();
                passes.sort_by(|a, b| b.1.0.cmp(&a.1.0));
                let top: Vec<String> = passes.iter().take(14).map(|(key, (ns, n))| format!("{key} {:.2} ms x{:.1}", *ns as f64 / 1e6 / frames, f64::from(*n) / frames)).collect();
                log_line(&format!("pass profile: {:.2} ms GPU in {} pass shapes per frame over {} frames; {}", total as f64 / 1e6 / frames, passes.len(), self.pass_frames, top.join("; ")));
                self.pass_times.clear();
                self.pass_frames = 0;
            }
            static WINDOWS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if WINDOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 6 == 0 {
                // Android drops log text past ~4 KB, so long lists go out in pieces.
                for line in self.texture_breakdown() { log_line(&line); }
            }
        }
        if self.pipeline_cache_dirty > 0 && self.pipeline_cache_saved.elapsed() >= Duration::from_secs(60) { self.save_pipeline_cache(); }
        Ok(())
    }

    fn save_pipeline_cache(&mut self) {
        if self.pipeline_cache_dirty == 0 { return; }
        self.pipeline_cache_dirty = 0;
        self.pipeline_cache_saved = Instant::now();
        let Some(path) = self.pipeline_cache_path.clone() else { return };
        if let Ok(data) = unsafe { self.gpu.device.get_pipeline_cache_data(self.pipeline_cache) } {
            let temporary = path.with_extension("tmp");
            if std::fs::write(&temporary, &data).is_ok() { let _ = std::fs::rename(&temporary, &path); }
        }
    }

    /// Live texture bytes by role, by format class, by size, and the largest textures.
    fn texture_breakdown(&self) -> Vec<String> {
        let mut roles = [0u64; 5];
        let mut classes: std::collections::BTreeMap<String, u64> = Default::default();
        let mut sides = [0u64; 5];
        let mut largest: Vec<(u64, u32, &TextureResource)> = Vec::new();
        let mut targets: std::collections::BTreeMap<(String, u32, u32, u32, u32), (u32, u64)> = Default::default();
        // Idle: no use in the last IDLE_SUBMITS submissions (never used counts as idle).
        const IDLE_SUBMITS: u64 = 1800;
        let recording = self.recording_value();
        let mut idle_roles = [0u64; 5];
        let mut idle_targets: std::collections::BTreeMap<(String, u32, u32, u32, u32), (u32, u64)> = Default::default();
        for (&id, resource) in &self.resources {
            let Resource::Texture(texture) = resource else { continue };
            let bytes = texture.memory.as_ref().map_or(0, Allocation::size);
            let idle = texture.last_use == 0 || recording.saturating_sub(texture.last_use) > IDLE_SUBMITS;
            if texture.bind & (BIND_RENDER_TARGET | BIND_DEPTH_STENCIL) != 0 || id > MAX_RESOURCE_ID {
                let key = (format!("{:?}", texture.format), texture.width, texture.height, texture.depth_or_layers, texture.mip_levels);
                if idle {
                    let group = idle_targets.entry(key.clone()).or_default();
                    group.0 += 1;
                    group.1 += bytes;
                }
                let group = targets.entry(key).or_default();
                group.0 += 1;
                group.1 += bytes;
            }
            let role = if id > MAX_RESOURCE_ID { 4 } else if texture.bind & BIND_DEPTH_STENCIL != 0 { 1 } else if texture.bind & BIND_RENDER_TARGET != 0 { 0 } else if texture.bind & BIND_UAV != 0 { 2 } else { 3 };
            roles[role] += bytes;
            if idle { idle_roles[role] += bytes; }
            if role == 3 {
                let class = if texture.info.bc { format!("BC{}B", texture.info.block_bytes) } else { format!("{}Bpp", texture.info.block_bytes) };
                *classes.entry(class).or_default() += bytes;
                let side = texture.width.max(texture.height);
                sides[match side { 4096.. => 0, 2048..=4095 => 1, 1024..=2047 => 2, 512..=1023 => 3, _ => 4 }] += bytes;
            }
            largest.push((bytes, id, texture));
        }
        largest.sort_by(|a, b| b.0.cmp(&a.0));
        let mib = |bytes: u64| format!("{:.1}", bytes as f64 / 1048576.0);
        let top: Vec<String> = largest.iter().take(8).map(|(bytes, id, t)| format!("#{id} {:?} {}x{}x{} mips {} bind {:#x} {} MiB", t.format, t.width, t.height, t.depth_or_layers, t.mip_levels, t.bind, mib(*bytes))).collect();
        let mut groups: Vec<_> = targets.into_iter().collect();
        groups.sort_by(|a, b| b.1.1.cmp(&a.1.1));
        let groups: Vec<String> = groups.iter().map(|((format, w, h, d, mips), (count, bytes))| format!("{count}x {format} {w}x{h}x{d} m{mips} {}", mib(*bytes))).collect();
        let deferred: Vec<_> = self.resources.values().filter_map(|r| match r { Resource::Texture(t) if t.memory.is_none() && t.backing.is_none() => Some(t.bytes()), _ => None }).collect();
        let mut lines = vec![format!("textures by role MiB (allocated): render targets {}, depth {}, uav {}, sampled {}, proxies {}; sampled by class {}; sampled by largest side >=4096 {}, 2048 {}, 1024 {}, 512 {}, smaller {}; largest: {}",
            mib(roles[0]), mib(roles[1]), mib(roles[2]), mib(roles[3]), mib(roles[4]),
            classes.iter().map(|(class, bytes)| format!("{class} {}", mib(*bytes))).collect::<Vec<_>>().join(", "),
            mib(sides[0]), mib(sides[1]), mib(sides[2]), mib(sides[3]), mib(sides[4]), top.join("; "))];
        lines.push(format!("never-bound targets: {} textures, {} MiB logical contents; zero GPU pixel allocation", deferred.len(), mib(deferred.iter().sum())));
        lines.push(format!("allocator blocks by size: {}", self.gpu.block_report()));
        let e = &self.texture_eviction;
        lines.push(format!("texture overwrite skips: {} textures, {} MiB obsolete backing not restored", e.overwritten, mib(e.overwritten_bytes)));
        let cold_bytes: u64 = self.resources.values().filter_map(|r| match r { Resource::Texture(t) => t.backing.as_ref().map(|b| b.size), _ => None }).sum();
        let pinned = self.resources.values().filter(|r| matches!(r, Resource::Texture(t) if t.age_restored)).count();
        lines.push(format!("texture age eviction: enabled {}, age {TEXTURE_EVICT_AGE}, evicted {} textures, {} MiB released total, {} MiB currently on disk; restored {} textures/{} MiB, {} rapid returns, {} pinned, {} cancelled, {} errors; restore CPU {} us; backup file {} MiB; reusable backing {} MiB; minimum wall idle 60s, contents 512 KiB to 4 MiB, unbound only",
            e.file.is_some() && !e.failed, e.evicted, mib(e.released), mib(cold_bytes), e.restored, mib(e.restored_bytes), e.rapid_returns, pinned, e.cancelled, e.errors, e.restore_time.as_micros(), mib(e.backing_space.high_water()), mib(e.backing_space.reusable_bytes())));
        for chunk in groups.chunks(40) { lines.push(format!("targets and proxies: {}", chunk.join(", "))); }
        let mut idle: Vec<_> = idle_targets.into_iter().collect();
        idle.sort_by(|a, b| b.1.1.cmp(&a.1.1));
        let idle: Vec<String> = idle.iter().take(24).map(|((format, w, h, d, mips), (count, bytes))| format!("{count}x {format} {w}x{h}x{d} m{mips} {}", mib(*bytes))).collect();
        lines.push(format!("idle textures MiB (unused for {IDLE_SUBMITS}+ submissions): render targets {}, depth {}, uav {}, sampled {}, proxies {}; idle targets: {}",
            mib(idle_roles[0]), mib(idle_roles[1]), mib(idle_roles[2]), mib(idle_roles[3]), mib(idle_roles[4]), idle.join(", ")));
        lines.push(format!("deferred engine releases: {} references (already relinquished by engine owners)", self.engine_release_queue));
        lines
    }

    /// Live GPU and host memory by owner, to account for the process's footprint.
    fn memory_summary(&self) -> String {
        let mib = |bytes: u64| bytes >> 20;
        let (mut textures, mut texture_bytes, mut buffers, mut buffer_bytes, mut shadow_bytes) = (0u64, 0u64, 0u64, 0u64, 0u64);
        for resource in self.resources.values() {
            match resource {
                Resource::Texture(texture) => { textures += 1; texture_bytes += texture.bytes(); }
                Resource::Buffer(buffer) => {
                    if buffer.raw != vk::Buffer::null() { buffers += 1; buffer_bytes += buffer.size; }
                    shadow_bytes += buffer.shadow.as_ref().map_or(0, |s| s.len() as u64);
                }
                _ => {}
            }
        }
        let (used, capacity, blocks) = self.gpu.memory_report();
        let ring = |ring: &Ring| ring.chunks.iter().map(|chunk| chunk.size).sum::<u64>();
        let oldest = self.completions.front().map_or(0, |pending| pending.queued.elapsed().as_millis());
        let pools: usize = self.slots.iter().map(|slot| slot.descriptor_pools.len()).sum();
        let heap = kgsl_bytes();
        format!("live GPU: {textures} textures {} MiB, {buffers} buffers {} MiB (constant shadows {} MiB), uniform ring {} MiB, staging {} MiB, {} draw pipelines + {} compute, {} views, {pools} descriptor pools, {} pending results (oldest {oldest} ms); allocator {} MiB used of {} MiB in {blocks} blocks; kgsl {} MiB ({} MiB outside the allocator), malloc {} MiB of which Vulkan driver {} MiB; null D3D11 CPU storage {} MiB in {} resources (peak {} MiB), upload shadows {} MiB",
            mib(texture_bytes), mib(buffer_bytes), mib(shadow_bytes), mib(ring(&self.uniforms)), mib(ring(&self.staging)), self.pipelines.len(), self.compute_pipelines.len(), self.views.len(),
            self.completions.len(), mib(used), mib(capacity), mib(heap), mib(heap.saturating_sub(capacity)), mib(malloc_in_use()), mib(driver_heap_bytes()),
            mib(self.null_storage[0]), self.null_storage[1], mib(self.null_storage[2]), mib(self.null_storage[3]))
    }
}

fn depth_state_present(key: &PipelineKey) -> bool { key.depth != vk::Format::UNDEFINED }

impl Drop for NativeRenderer {
    fn drop(&mut self) {
        unsafe { let _ = self.gpu.device.device_wait_idle(); }
        self.save_pipeline_cache();
        self.detach_surface();
        self.gpu.refresh_completed();
        if let Some(PendingEviction { copy: EvictionCopy::Gpu { buffer, memory, value }, .. }) = self.texture_eviction.pending.take() {
            self.garbage.buffer(value, buffer, memory);
        }
        let value = 0;
        let ids: Vec<u32> = self.resources.keys().copied().collect();
        for id in ids { let _ = self.destroy(id); }
        for (_, pipeline) in self.pipelines.drain() { self.garbage.push(value, Trash::Pipeline(pipeline.raw)); }
        for (_, pipeline) in self.compute_pipelines.drain() { self.garbage.push(value, Trash::Pipeline(pipeline.raw)); }
        for (pipeline, _, _) in self.proxy_pipelines.iter_mut().filter_map(Option::take) { self.garbage.push(value, Trash::Pipeline(pipeline)); }
        for (_, view) in self.views.drain() { self.garbage.push(value, Trash::View(view)); }
        for ring in [&mut self.uniforms, &mut self.staging] { for chunk in ring.chunks.drain(..) { self.garbage.buffer(value, chunk.buffer, chunk.memory); } }
        if let Some(memory) = self.zero_vertex_memory.take() { self.garbage.buffer(value, self.zero_vertex, memory); }
        if let Some((buffer, memory, _)) = self.probe.take() { self.garbage.buffer(value, buffer, memory); }
        for pending in self.completions.drain(..) { if let Completion::Readback { buffer, memory, .. } = pending.completion { self.garbage.buffer(value, buffer, memory); } }
        self.garbage.collect(&mut self.gpu, true);
        let device = &self.gpu.device;
        unsafe {
            for slot in &self.slots {
                for pool in &slot.descriptor_pools { device.destroy_descriptor_pool(*pool, None); }
                device.destroy_command_pool(slot.pool, None);
                if let Some(profile) = &slot.profile { device.destroy_query_pool(profile.pool, Some(callbacks())); }
            }
            device.destroy_sampler(self.dummy_sampler, Some(callbacks()));
            device.destroy_sampler(self.dummy_compare_sampler, Some(callbacks()));
            for pool in self.occlusion.pools.iter().chain(&self.timestamps.pools) {
                device.destroy_query_pool(*pool, Some(callbacks()));
            }
            device.destroy_pipeline_cache(self.pipeline_cache, Some(callbacks()));
        }
    }
}


