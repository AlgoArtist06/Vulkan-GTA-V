//! Browser-free native renderer: the engine's D3D11-style command ring recorded straight into Vulkan.

mod attachments;
mod gpu;
mod mipmaps;
mod movie;
mod protocol;
mod renderer;
mod scissor;
mod shader;
mod shader_cache;
mod shader_constants;
mod shader_storage;
mod surface;
mod texture_backing;
mod texture_upload;
mod uav_counter;
mod vertex_step;

pub use protocol::{
    Command, DecodeError, Opcode, ResourceId, RingDecoder, RingHeader, SharedMemory,
    SharedMemoryError, SharedRingDecoder, SliceSharedMemory, COMMAND_HEADER_BYTES,
    MAX_RESOURCE_ID, RING_DATA_OFFSET, RING_HEADER_BYTES,
};
pub use renderer::{logging, NativeRenderer, ProcessReport, RendererError};
pub use movie::{MovieFrameCallback, MovieFrameSource};
pub use surface::{NativeSurface, SurfaceError};
