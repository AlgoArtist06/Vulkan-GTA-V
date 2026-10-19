//! Vulkan device, memory and D3D state translation shared by the command consumer.

use ash::vk;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc};
use gpu_allocator::{AllocationSizes, AllocatorDebugSettings, MemoryLocation};
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) const BIND_VERTEX: u32 = 1;
pub(crate) const BIND_INDEX: u32 = 2;
pub(crate) const BIND_CONSTANT: u32 = 4;
pub(crate) const BIND_SRV: u32 = 8;
pub(crate) const BIND_RENDER_TARGET: u32 = 0x20;
pub(crate) const BIND_DEPTH_STENCIL: u32 = 0x40;
pub(crate) const BIND_UAV: u32 = 0x80;
pub(crate) const MISC_DRAW_INDIRECT: u32 = 0x10;

/// Resources at least this large get their own `VkDeviceMemory`; smaller ones share 32 MiB blocks.
const DEDICATED_BYTES: u64 = 8 << 20;

/// The wgpu path mapped 16-bit UNORM textures to float (`debug.gta5.legacy`).
pub(crate) static LEGACY_FORMATS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Bytes the Vulkan driver holds in host memory through our allocation callbacks.
static DRIVER_HEAP: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn driver_heap_bytes() -> u64 { DRIVER_HEAP.load(Ordering::Relaxed) as u64 }

/// Each driver allocation is preceded by a header holding its total size and alignment.
const HEADER: usize = 16;

unsafe extern "system" fn driver_alloc(_: *mut c_void, size: usize, align: usize, _: vk::SystemAllocationScope) -> *mut c_void {
    let align = align.max(HEADER);
    let Ok(layout) = std::alloc::Layout::from_size_align(size + align, align) else { return std::ptr::null_mut() };
    let base = unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) };
    if base.is_null() { return std::ptr::null_mut(); }
    DRIVER_HEAP.fetch_add(size + align, Ordering::Relaxed);
    let user = unsafe { base.add(align) };
    unsafe { (user as *mut usize).sub(2).write(size + align); (user as *mut usize).sub(1).write(align); }
    user.cast()
}

unsafe extern "system" fn driver_free(_: *mut c_void, memory: *mut c_void) {
    if memory.is_null() { return; }
    let user = memory as *mut usize;
    let (total, align) = unsafe { (user.sub(2).read(), user.sub(1).read()) };
    DRIVER_HEAP.fetch_sub(total, Ordering::Relaxed);
    unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, (memory as *mut u8).sub(align), std::alloc::Layout::from_size_align_unchecked(total, align)); }
}

unsafe extern "system" fn driver_realloc(data: *mut c_void, original: *mut c_void, size: usize, align: usize, scope: vk::SystemAllocationScope) -> *mut c_void {
    if original.is_null() { return unsafe { driver_alloc(data, size, align, scope) }; }
    if size == 0 { unsafe { driver_free(data, original) }; return std::ptr::null_mut(); }
    let old = unsafe { (original as *mut usize).sub(2).read() - (original as *mut usize).sub(1).read() };
    let moved = unsafe { driver_alloc(data, size, align, scope) };
    if !moved.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(original as *const u8, moved as *mut u8, old.min(size)); driver_free(data, original); }
    }
    moved
}

pub(crate) fn callbacks() -> &'static vk::AllocationCallbacks<'static> {
    static CALLBACKS: std::sync::OnceLock<SendCallbacks> = std::sync::OnceLock::new();
    &CALLBACKS.get_or_init(|| SendCallbacks(vk::AllocationCallbacks::default()
        .pfn_allocation(Some(driver_alloc)).pfn_reallocation(Some(driver_realloc)).pfn_free(Some(driver_free)))).0
}

struct SendCallbacks(vk::AllocationCallbacks<'static>);
// SAFETY: the callbacks hold no user data; the functions are thread-safe.
unsafe impl Send for SendCallbacks {}
unsafe impl Sync for SendCallbacks {}

pub(crate) struct Gpu {
    _entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub family: u32,
    pub props: vk::PhysicalDeviceProperties,
    pub surface_fn: ash::khr::surface::Instance,
    pub android_surface_fn: ash::khr::android_surface::Instance,
    pub swapchain_fn: ash::khr::swapchain::Device,
    pub allocator: std::mem::ManuallyDrop<Allocator>,
    /// Signalled with each submission's number; `completed` is its last read value.
    pub timeline: vk::Semaphore,
    pub submitted: u64,
    pub completed: u64,
    pub host_query_reset: bool,
    /// Unbound D3D slots can be null descriptors (reads return zero, as in D3D).
    pub null_descriptor: bool,
    filterable: std::cell::RefCell<HashMap<vk::Format, bool>>,
    /// `debug.gta5.legacy`: the wgpu path's approximations (diagnostic A/B only).
    pub legacy: bool,
}

impl Gpu {
    pub fn new() -> Result<Self, String> {
        let entry = ash::Entry::linked();
        let app = vk::ApplicationInfo::default().application_name(c"gta5-native").api_version(vk::API_VERSION_1_3);
        let instance_extensions = [ash::khr::surface::NAME.as_ptr(), ash::khr::android_surface::NAME.as_ptr()];
        let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&instance_extensions), Some(callbacks())) }
            .map_err(|error| format!("vkCreateInstance: {error}"))?;
        let physical = unsafe { instance.enumerate_physical_devices() }.map_err(|error| format!("vkEnumeratePhysicalDevices: {error}"))?
            .into_iter().next().ok_or("no Vulkan physical device")?;
        let props = unsafe { instance.get_physical_device_properties(physical) };
        if props.api_version < vk::API_VERSION_1_3 { return Err(format!("Vulkan 1.3 required, device has {:#x}", props.api_version)); }
        let family = unsafe { instance.get_physical_device_queue_family_properties(physical) }.iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE))
            .ok_or("no graphics+compute queue family")? as u32;
        let available: Vec<String> = unsafe { instance.enumerate_device_extension_properties(physical) }.map_err(|error| error.to_string())?
            .iter().filter_map(|e| e.extension_name_as_c_str().ok().map(|name| name.to_string_lossy().into_owned())).collect();
        let required: [&CStr; 3] = [ash::khr::swapchain::NAME, ash::ext::custom_border_color::NAME, ash::ext::robustness2::NAME];
        for name in required {
            if !available.iter().any(|e| e.as_bytes() == name.to_bytes()) { return Err(format!("device lacks {}", name.to_string_lossy())); }
        }
        let extensions: Vec<*const std::ffi::c_char> = required.iter().map(|name| name.as_ptr()).collect();

        let mut supported12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut supported_robust = vk::PhysicalDeviceRobustness2FeaturesEXT::default();
        let mut supported_features = vk::PhysicalDeviceFeatures2::default().push_next(&mut supported12).push_next(&mut supported_robust);
        unsafe { instance.get_physical_device_features2(physical, &mut supported_features) };
        // Vulkan allows adjacency topologies only with this feature on.
        let geometry_shader = supported_features.features.geometry_shader == vk::TRUE;
        let host_query_reset = supported12.host_query_reset == vk::TRUE;
        let null_descriptor = supported_robust.null_descriptor == vk::TRUE;
        // `setprop debug.gta5.robust 0` (A/B switch): no shader bounds checks on buffer and image access. WebGPU
        // requires them; Direct3D 11 only defines out-of-range buffer reads as zero. Null descriptors stay on.
        let bounds_checked = crate::renderer::debug_switch("robust", true);
        let robust_buffer_access2 = bounds_checked && supported_robust.robust_buffer_access2 == vk::TRUE;
        let base = vk::PhysicalDeviceFeatures::default()
            .texture_compression_bc(true).dual_src_blend(true).independent_blend(true).sampler_anisotropy(true)
            .shader_clip_distance(true).image_cube_array(true).draw_indirect_first_instance(true)
            .fragment_stores_and_atomics(true).vertex_pipeline_stores_and_atomics(true).depth_bias_clamp(true)
            .depth_clamp(true).robust_buffer_access(bounds_checked).multi_draw_indirect(true).geometry_shader(geometry_shader);
        let mut v11 = vk::PhysicalDeviceVulkan11Features::default().shader_draw_parameters(true);
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true).host_query_reset(host_query_reset)
            .separate_depth_stencil_layouts(true).uniform_buffer_standard_layout(true).scalar_block_layout(true);
        let mut v13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true).synchronization2(true)
            .robust_image_access(bounds_checked).shader_zero_initialize_workgroup_memory(true).maintenance4(true);
        let mut border = vk::PhysicalDeviceCustomBorderColorFeaturesEXT::default().custom_border_colors(true).custom_border_color_without_format(true);
        let mut robust = vk::PhysicalDeviceRobustness2FeaturesEXT::default().robust_buffer_access2(robust_buffer_access2).null_descriptor(null_descriptor);
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&priorities)];
        let create = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&extensions).enabled_features(&base)
            .push_next(&mut v11).push_next(&mut v12).push_next(&mut v13).push_next(&mut border).push_next(&mut robust);
        let device = unsafe { instance.create_device(physical, &create, Some(callbacks())) }.map_err(|error| format!("vkCreateDevice: {error}"))?;
        let queue = unsafe { device.get_device_queue(family, 0) };
        let allocator = Allocator::new(&AllocatorCreateDesc {
            instance: instance.clone(), device: device.clone(), physical_device: physical,
            debug_settings: { let mut settings = AllocatorDebugSettings::default(); settings.log_leaks_on_shutdown = false; settings },
            buffer_device_address: false,
            // Small blocks: GPU memory is pinned RAM on this phone, grown in steps Android's low-memory killer tolerates.
            // 16/8 MiB blocks measured 105 MiB more than 32/16 (allocations spread over more partly used blocks).
            allocation_sizes: AllocationSizes::new(32 << 20, 16 << 20),
        }).map_err(|error| format!("GPU allocator: {error}"))?;
        let mut timeline_type = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
        let timeline = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut timeline_type), Some(callbacks())) }.map_err(|error| error.to_string())?;
        Ok(Self {
            surface_fn: ash::khr::surface::Instance::new(&entry, &instance),
            android_surface_fn: ash::khr::android_surface::Instance::new(&entry, &instance),
            swapchain_fn: ash::khr::swapchain::Device::new(&instance, &device),
            _entry: entry, instance, physical, device, queue, family, props,
            allocator: std::mem::ManuallyDrop::new(allocator), timeline, submitted: 0, completed: 0, host_query_reset, null_descriptor,
            filterable: Default::default(), legacy: crate::renderer::debug_switch("legacy", false),
        })
    }

    /// Reads how far the GPU has got. Cheap; call before deciding whether a resource is still in use.
    pub fn refresh_completed(&mut self) -> u64 {
        if let Ok(value) = unsafe { self.device.get_semaphore_counter_value(self.timeline) } { self.completed = value; }
        self.completed
    }

    pub fn wait_for(&mut self, value: u64, timeout: std::time::Duration) -> Result<(), String> {
        if self.completed >= value { return Ok(()); }
        let semaphores = [self.timeline];
        let values = [value];
        unsafe { self.device.wait_semaphores(&vk::SemaphoreWaitInfo::default().semaphores(&semaphores).values(&values), timeout.as_nanos() as u64) }
            .map_err(|error| format!("GPU wait for submission {value}: {error}"))?;
        self.refresh_completed();
        Ok(())
    }

    pub fn allocate(&mut self, name: &str, requirements: vk::MemoryRequirements, location: MemoryLocation, linear: bool, dedicated: AllocationScheme) -> Result<Allocation, String> {
        // A 1 MiB threshold saved 115 MiB of block slack but cost 6% FPS (evidence/memory-hillclimb-2026-10-19, M12).
        let scheme = if requirements.size >= DEDICATED_BYTES { dedicated } else { AllocationScheme::GpuAllocatorManaged };
        self.allocator.allocate(&AllocationCreateDesc { name, requirements, location, linear, allocation_scheme: scheme })
            .map_err(|error| format!("GPU memory for {name} ({} bytes): {error}", requirements.size))
    }

    pub fn free(&mut self, allocation: Allocation) {
        let _ = self.allocator.free(allocation);
    }

    /// A buffer in host-visible device-local memory, persistently mapped (this phone's memory is unified).
    pub fn create_buffer(&mut self, name: &str, size: u64, usage: vk::BufferUsageFlags, location: MemoryLocation) -> Result<(vk::Buffer, Allocation), String> {
        let buffer = unsafe { self.device.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE), Some(callbacks())) }
            .map_err(|error| format!("vkCreateBuffer {name}: {error}"))?;
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let allocation = match self.allocate(name, requirements, location, true, AllocationScheme::DedicatedBuffer(buffer)) {
            Ok(allocation) => allocation,
            Err(error) => { unsafe { self.device.destroy_buffer(buffer, Some(callbacks())) }; return Err(error); }
        };
        unsafe { self.device.bind_buffer_memory(buffer, allocation.memory(), allocation.offset()) }.map_err(|error| format!("bind {name}: {error}"))?;
        Ok((buffer, allocation))
    }

    /// Whether the device can sample `format` with linear filtering (Adreno cannot for D32_SFLOAT_S8_UINT).
    pub fn linear_filterable(&self, format: vk::Format) -> bool {
        *self.filterable.borrow_mut().entry(format).or_insert_with(|| {
            let properties = unsafe { self.instance.get_physical_device_format_properties(self.physical, format) };
            properties.optimal_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR)
        })
    }

    /// Allocator blocks grouped by size: count, capacity and used MiB, and the allocation names inside them.
    pub fn block_report(&self) -> String {
        let report = self.allocator.generate_report();
        let mut groups: std::collections::BTreeMap<u64, (u32, u64, u64, HashMap<String, u64>)> = Default::default();
        for block in &report.blocks {
            let entry = groups.entry(block.size).or_default();
            entry.0 += 1;
            entry.1 += block.size;
            for allocation in &report.allocations[block.allocations.clone()] {
                entry.2 += allocation.size;
                *entry.3.entry(allocation.name.clone()).or_default() += allocation.size;
            }
        }
        groups.iter().rev().map(|(size, (count, capacity, used, names))| {
            let mut names: Vec<_> = names.iter().collect();
            names.sort_by(|a, b| b.1.cmp(a.1));
            let names: Vec<String> = names.iter().take(3).map(|(name, bytes)| format!("{name} {}", *bytes >> 20)).collect();
            format!("{} KiB x{count}: {} of {} MiB used ({})", size >> 10, used >> 20, capacity >> 20, names.join(", "))
        }).collect::<Vec<_>>().join("; ")
    }

    pub fn memory_report(&self) -> (u64, u64, usize) {
        let report = self.allocator.generate_report();
        (report.total_allocated_bytes, report.total_capacity_bytes, report.blocks.len())
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_semaphore(self.timeline, Some(callbacks()));
            std::mem::ManuallyDrop::drop(&mut self.allocator);
            self.device.destroy_device(Some(callbacks()));
            self.instance.destroy_instance(Some(callbacks()));
        }
    }
}

/// How a texture format is laid out and which aspects it has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FormatInfo {
    pub block_bytes: u32,
    pub block_width: u32,
    pub block_height: u32,
    pub depth: bool,
    pub stencil: bool,
    pub bc: bool,
}

pub(crate) fn format_info(format: vk::Format) -> Option<FormatInfo> {
    use vk::Format as F;
    let plain = |bytes| FormatInfo { block_bytes: bytes, block_width: 1, block_height: 1, depth: false, stencil: false, bc: false };
    let bc = |bytes| FormatInfo { block_bytes: bytes, block_width: 4, block_height: 4, depth: false, stencil: false, bc: true };
    let ds = |bytes, stencil| FormatInfo { block_bytes: bytes, block_width: 1, block_height: 1, depth: true, stencil, bc: false };
    Some(match format {
        F::R8_UNORM | F::R8_SNORM | F::R8_UINT | F::R8_SINT => plain(1),
        F::R8G8_UNORM | F::R8G8_SNORM | F::R8G8_UINT | F::R8G8_SINT | F::R16_SFLOAT | F::R16_UNORM | F::R16_SNORM | F::R16_UINT | F::R16_SINT
            | F::R5G6B5_UNORM_PACK16 | F::A1R5G5B5_UNORM_PACK16 => plain(2),
        F::R8G8B8A8_UNORM | F::R8G8B8A8_SRGB | F::R8G8B8A8_SNORM | F::R8G8B8A8_UINT | F::R8G8B8A8_SINT | F::B8G8R8A8_UNORM | F::B8G8R8A8_SRGB
            | F::A2B10G10R10_UNORM_PACK32 | F::A2B10G10R10_UINT_PACK32 | F::B10G11R11_UFLOAT_PACK32 | F::E5B9G9R9_UFLOAT_PACK32
            | F::R16G16_SFLOAT | F::R16G16_UNORM | F::R16G16_SNORM | F::R16G16_UINT | F::R16G16_SINT | F::R32_SFLOAT | F::R32_UINT | F::R32_SINT => plain(4),
        F::R16G16B16A16_SFLOAT | F::R16G16B16A16_UNORM | F::R16G16B16A16_SNORM | F::R16G16B16A16_UINT | F::R16G16B16A16_SINT
            | F::R32G32_SFLOAT | F::R32G32_UINT | F::R32G32_SINT => plain(8),
        F::R32G32B32A32_SFLOAT | F::R32G32B32A32_UINT | F::R32G32B32A32_SINT => plain(16),
        F::BC1_RGBA_UNORM_BLOCK | F::BC1_RGBA_SRGB_BLOCK | F::BC4_UNORM_BLOCK | F::BC4_SNORM_BLOCK => bc(8),
        F::BC2_UNORM_BLOCK | F::BC2_SRGB_BLOCK | F::BC3_UNORM_BLOCK | F::BC3_SRGB_BLOCK | F::BC5_UNORM_BLOCK | F::BC5_SNORM_BLOCK
            | F::BC6H_UFLOAT_BLOCK | F::BC6H_SFLOAT_BLOCK | F::BC7_UNORM_BLOCK | F::BC7_SRGB_BLOCK => bc(16),
        F::D16_UNORM => ds(2, false),
        F::D32_SFLOAT => ds(4, false),
        F::D24_UNORM_S8_UINT => ds(4, true),
        F::D32_SFLOAT_S8_UINT => ds(8, true),
        _ => return None,
    })
}

/// A D3D (DXGI) format as a native texture format. `bind` decides typeless depth formats.
pub(crate) fn texture_format(dxgi: u32, bind: u32) -> Option<vk::Format> {
    use vk::Format as F;
    if LEGACY_FORMATS.load(std::sync::atomic::Ordering::Relaxed) && matches!(dxgi, 11 | 35) { return Some(if dxgi == 11 { F::R16G16B16A16_SFLOAT } else { F::R16G16_SFLOAT }); }
    let depth = bind & BIND_DEPTH_STENCIL != 0;
    Some(match dxgi {
        1 | 2 => F::R32G32B32A32_SFLOAT, 3 => F::R32G32B32A32_UINT, 4 => F::R32G32B32A32_SINT,
        9 | 10 => F::R16G16B16A16_SFLOAT, 11 => F::R16G16B16A16_UNORM, 12 => F::R16G16B16A16_UINT, 13 => F::R16G16B16A16_SNORM, 14 => F::R16G16B16A16_SINT,
        15 | 16 => F::R32G32_SFLOAT, 17 => F::R32G32_UINT, 18 => F::R32G32_SINT,
        19 | 20 | 21 | 22 => F::D32_SFLOAT_S8_UINT,
        23 | 24 => F::A2B10G10R10_UNORM_PACK32, 25 => F::A2B10G10R10_UINT_PACK32,
        26 => F::B10G11R11_UFLOAT_PACK32,
        27 | 28 => F::R8G8B8A8_UNORM, 29 => F::R8G8B8A8_SRGB, 30 => F::R8G8B8A8_UINT, 31 => F::R8G8B8A8_SNORM, 32 => F::R8G8B8A8_SINT,
        33 | 34 => F::R16G16_SFLOAT, 35 => F::R16G16_UNORM, 36 => F::R16G16_UINT, 37 => F::R16G16_SNORM, 38 => F::R16G16_SINT,
        39 => if depth { F::D32_SFLOAT } else { F::R32_SFLOAT }, 40 => F::D32_SFLOAT, 41 => F::R32_SFLOAT, 42 => F::R32_UINT, 43 => F::R32_SINT,
        44 | 45 | 46 | 47 => F::D24_UNORM_S8_UINT,
        48 | 49 => F::R8G8_UNORM, 50 => F::R8G8_UINT, 51 => F::R8G8_SNORM, 52 => F::R8G8_SINT,
        53 => if depth { F::D16_UNORM } else { F::R16_SFLOAT }, 54 => F::R16_SFLOAT, 55 => F::D16_UNORM, 56 => F::R16_UNORM, 57 => F::R16_UINT, 58 => F::R16_SNORM, 59 => F::R16_SINT,
        60 | 61 | 65 => F::R8_UNORM, 62 => F::R8_UINT, 63 => F::R8_SNORM, 64 => F::R8_SINT,
        67 => F::E5B9G9R9_UFLOAT_PACK32,
        70 | 71 => F::BC1_RGBA_UNORM_BLOCK, 72 => F::BC1_RGBA_SRGB_BLOCK, 73 | 74 => F::BC2_UNORM_BLOCK, 75 => F::BC2_SRGB_BLOCK,
        76 | 77 => F::BC3_UNORM_BLOCK, 78 => F::BC3_SRGB_BLOCK, 79 | 80 => F::BC4_UNORM_BLOCK, 81 => F::BC4_SNORM_BLOCK,
        82 | 83 => F::BC5_UNORM_BLOCK, 84 => F::BC5_SNORM_BLOCK,
        85 => F::R5G6B5_UNORM_PACK16, 86 => F::A1R5G5B5_UNORM_PACK16,
        87 | 88 | 90 | 92 => F::B8G8R8A8_UNORM, 91 | 93 => F::B8G8R8A8_SRGB,
        94 | 95 => F::BC6H_UFLOAT_BLOCK, 96 => F::BC6H_SFLOAT_BLOCK, 97 | 98 => F::BC7_UNORM_BLOCK, 99 => F::BC7_SRGB_BLOCK,
        _ => return None,
    })
}

/// Formats a D3D texture's views may reinterpret it as: its typeless family, or its sRGB twin.
pub(crate) fn view_family(format: vk::Format) -> &'static [vk::Format] {
    use vk::Format as F;
    match format {
        F::R8G8B8A8_UNORM | F::R8G8B8A8_SRGB | F::R8G8B8A8_UINT | F::R8G8B8A8_SNORM | F::R8G8B8A8_SINT => &[F::R8G8B8A8_UNORM, F::R8G8B8A8_SRGB, F::R8G8B8A8_UINT, F::R8G8B8A8_SNORM, F::R8G8B8A8_SINT],
        F::B8G8R8A8_UNORM | F::B8G8R8A8_SRGB => &[F::B8G8R8A8_UNORM, F::B8G8R8A8_SRGB],
        F::R16G16B16A16_SFLOAT | F::R16G16B16A16_UNORM | F::R16G16B16A16_UINT | F::R16G16B16A16_SNORM | F::R16G16B16A16_SINT => &[F::R16G16B16A16_SFLOAT, F::R16G16B16A16_UNORM, F::R16G16B16A16_UINT, F::R16G16B16A16_SNORM, F::R16G16B16A16_SINT],
        F::R16G16_SFLOAT | F::R16G16_UNORM | F::R16G16_UINT | F::R16G16_SNORM | F::R16G16_SINT => &[F::R16G16_SFLOAT, F::R16G16_UNORM, F::R16G16_UINT, F::R16G16_SNORM, F::R16G16_SINT],
        F::R32G32B32A32_SFLOAT | F::R32G32B32A32_UINT | F::R32G32B32A32_SINT => &[F::R32G32B32A32_SFLOAT, F::R32G32B32A32_UINT, F::R32G32B32A32_SINT],
        F::R32G32_SFLOAT | F::R32G32_UINT | F::R32G32_SINT => &[F::R32G32_SFLOAT, F::R32G32_UINT, F::R32G32_SINT],
        F::R32_SFLOAT | F::R32_UINT | F::R32_SINT => &[F::R32_SFLOAT, F::R32_UINT, F::R32_SINT],
        F::R16_SFLOAT | F::R16_UNORM | F::R16_UINT | F::R16_SNORM | F::R16_SINT => &[F::R16_SFLOAT, F::R16_UNORM, F::R16_UINT, F::R16_SNORM, F::R16_SINT],
        F::R8G8_UNORM | F::R8G8_UINT | F::R8G8_SNORM | F::R8G8_SINT => &[F::R8G8_UNORM, F::R8G8_UINT, F::R8G8_SNORM, F::R8G8_SINT],
        F::R8_UNORM | F::R8_UINT | F::R8_SNORM | F::R8_SINT => &[F::R8_UNORM, F::R8_UINT, F::R8_SNORM, F::R8_SINT],
        F::A2B10G10R10_UNORM_PACK32 | F::A2B10G10R10_UINT_PACK32 => &[F::A2B10G10R10_UNORM_PACK32, F::A2B10G10R10_UINT_PACK32],
        F::BC1_RGBA_UNORM_BLOCK | F::BC1_RGBA_SRGB_BLOCK => &[F::BC1_RGBA_UNORM_BLOCK, F::BC1_RGBA_SRGB_BLOCK],
        F::BC2_UNORM_BLOCK | F::BC2_SRGB_BLOCK => &[F::BC2_UNORM_BLOCK, F::BC2_SRGB_BLOCK],
        F::BC3_UNORM_BLOCK | F::BC3_SRGB_BLOCK => &[F::BC3_UNORM_BLOCK, F::BC3_SRGB_BLOCK],
        F::BC4_UNORM_BLOCK | F::BC4_SNORM_BLOCK => &[F::BC4_UNORM_BLOCK, F::BC4_SNORM_BLOCK],
        F::BC5_UNORM_BLOCK | F::BC5_SNORM_BLOCK => &[F::BC5_UNORM_BLOCK, F::BC5_SNORM_BLOCK],
        F::BC6H_UFLOAT_BLOCK | F::BC6H_SFLOAT_BLOCK => &[F::BC6H_UFLOAT_BLOCK, F::BC6H_SFLOAT_BLOCK],
        F::BC7_UNORM_BLOCK | F::BC7_SRGB_BLOCK => &[F::BC7_UNORM_BLOCK, F::BC7_SRGB_BLOCK],
        _ => &[],
    }
}

/// The formats views of a texture created as `dxgi` may use: the whole family for a typeless
/// format, the sRGB twin for a typed one. A short list keeps the driver's framebuffer compression.
pub(crate) fn view_formats(dxgi: u32, format: vk::Format) -> &'static [vk::Format] {
    use vk::Format as F;
    let typeless = matches!(dxgi, 1 | 9 | 15 | 23 | 27 | 33 | 48 | 60 | 70 | 73 | 76 | 79 | 82 | 90 | 92 | 94 | 97)
        || (matches!(dxgi, 39 | 53) && format_info(format).is_some_and(|info| !info.depth));
    if typeless { return view_family(format); }
    match format {
        F::R8G8B8A8_UNORM | F::R8G8B8A8_SRGB => &[F::R8G8B8A8_UNORM, F::R8G8B8A8_SRGB],
        F::B8G8R8A8_UNORM | F::B8G8R8A8_SRGB => &[F::B8G8R8A8_UNORM, F::B8G8R8A8_SRGB],
        F::BC1_RGBA_UNORM_BLOCK | F::BC1_RGBA_SRGB_BLOCK | F::BC2_UNORM_BLOCK | F::BC2_SRGB_BLOCK | F::BC3_UNORM_BLOCK | F::BC3_SRGB_BLOCK
            | F::BC7_UNORM_BLOCK | F::BC7_SRGB_BLOCK => view_family(format),
        _ => &[],
    }
}

/// Channel remapping a D3D format needs on top of its native storage format.
pub(crate) fn format_swizzle(dxgi: u32) -> vk::ComponentMapping {
    use vk::ComponentSwizzle as S;
    match dxgi {
        // A8_UNORM stored as R8: alpha reads the red channel, colour reads zero.
        65 => vk::ComponentMapping { r: S::ZERO, g: S::ZERO, b: S::ZERO, a: S::R },
        // B8G8R8X8: the X byte is undefined and reads as opaque.
        88 | 92 | 93 => vk::ComponentMapping { r: S::IDENTITY, g: S::IDENTITY, b: S::IDENTITY, a: S::ONE },
        // X32_G8X24 / X24_G8 stencil views: D3D returns stencil in green; Vulkan's stencil aspect has it in red.
        22 | 47 => vk::ComponentMapping { r: S::R, g: S::R, b: S::ZERO, a: S::ONE },
        _ => vk::ComponentMapping::default(),
    }
}

pub(crate) fn vertex_format(dxgi: u32) -> Option<vk::Format> {
    use vk::Format as F;
    Some(match dxgi {
        2 => F::R32G32B32A32_SFLOAT, 6 => F::R32G32B32_SFLOAT, 16 => F::R32G32_SFLOAT, 41 => F::R32_SFLOAT,
        3 => F::R32G32B32A32_UINT, 7 => F::R32G32B32_UINT, 17 => F::R32G32_UINT, 42 => F::R32_UINT,
        4 => F::R32G32B32A32_SINT, 8 => F::R32G32B32_SINT, 18 => F::R32G32_SINT, 43 => F::R32_SINT,
        10 => F::R16G16B16A16_SFLOAT, 34 => F::R16G16_SFLOAT, 54 => F::R16_SFLOAT,
        11 => F::R16G16B16A16_UNORM, 35 => F::R16G16_UNORM, 56 => F::R16_UNORM,
        12 => F::R16G16B16A16_UINT, 36 => F::R16G16_UINT, 57 => F::R16_UINT,
        13 => F::R16G16B16A16_SNORM, 37 => F::R16G16_SNORM, 58 => F::R16_SNORM,
        14 => F::R16G16B16A16_SINT, 38 => F::R16G16_SINT, 59 => F::R16_SINT,
        28 => F::R8G8B8A8_UNORM, 30 => F::R8G8B8A8_UINT, 31 => F::R8G8B8A8_SNORM, 32 => F::R8G8B8A8_SINT,
        49 => F::R8G8_UNORM, 50 => F::R8G8_UINT, 51 => F::R8G8_SNORM, 52 => F::R8G8_SINT,
        61 => F::R8_UNORM, 62 => F::R8_UINT, 63 => F::R8_SNORM, 64 => F::R8_SINT,
        87 => F::B8G8R8A8_UNORM, 24 => F::A2B10G10R10_UNORM_PACK32, 25 => F::A2B10G10R10_UINT_PACK32, 26 => F::B10G11R11_UFLOAT_PACK32,
        _ => return None,
    })
}

pub(crate) fn compare_op(value: u32) -> Result<vk::CompareOp, String> {
    Ok(match value {
        1 => vk::CompareOp::NEVER, 2 => vk::CompareOp::LESS, 3 => vk::CompareOp::EQUAL, 4 => vk::CompareOp::LESS_OR_EQUAL,
        5 => vk::CompareOp::GREATER, 6 => vk::CompareOp::NOT_EQUAL, 7 => vk::CompareOp::GREATER_OR_EQUAL, 0 | 8 => vk::CompareOp::ALWAYS,
        value => return Err(format!("unsupported D3D comparison function {value}")),
    })
}

pub(crate) fn stencil_op(value: u32) -> Result<vk::StencilOp, String> {
    Ok(match value {
        0 | 1 => vk::StencilOp::KEEP, 2 => vk::StencilOp::ZERO, 3 => vk::StencilOp::REPLACE, 4 => vk::StencilOp::INCREMENT_AND_CLAMP,
        5 => vk::StencilOp::DECREMENT_AND_CLAMP, 6 => vk::StencilOp::INVERT, 7 => vk::StencilOp::INCREMENT_AND_WRAP, 8 => vk::StencilOp::DECREMENT_AND_WRAP,
        value => return Err(format!("unsupported D3D stencil operation {value}")),
    })
}

pub(crate) fn blend_factor(value: u32) -> Result<vk::BlendFactor, String> {
    use vk::BlendFactor as B;
    Ok(match value {
        1 => B::ZERO, 2 => B::ONE, 3 => B::SRC_COLOR, 4 => B::ONE_MINUS_SRC_COLOR, 5 => B::SRC_ALPHA, 6 => B::ONE_MINUS_SRC_ALPHA,
        7 => B::DST_ALPHA, 8 => B::ONE_MINUS_DST_ALPHA, 9 => B::DST_COLOR, 10 => B::ONE_MINUS_DST_COLOR, 11 => B::SRC_ALPHA_SATURATE,
        14 => B::CONSTANT_COLOR, 15 => B::ONE_MINUS_CONSTANT_COLOR, 16 => B::SRC1_COLOR, 17 => B::ONE_MINUS_SRC1_COLOR,
        18 => B::SRC1_ALPHA, 19 => B::ONE_MINUS_SRC1_ALPHA,
        value => return Err(format!("unsupported D3D blend factor {value}")),
    })
}

pub(crate) fn blend_op(value: u32) -> Result<vk::BlendOp, String> {
    Ok(match value {
        1 => vk::BlendOp::ADD, 2 => vk::BlendOp::SUBTRACT, 3 => vk::BlendOp::REVERSE_SUBTRACT, 4 => vk::BlendOp::MIN, 5 => vk::BlendOp::MAX,
        value => return Err(format!("unsupported D3D blend operation {value}")),
    })
}

pub(crate) fn topology(value: u32) -> Result<vk::PrimitiveTopology, String> {
    Ok(match value {
        1 => vk::PrimitiveTopology::POINT_LIST, 2 => vk::PrimitiveTopology::LINE_LIST, 3 => vk::PrimitiveTopology::LINE_STRIP,
        4 => vk::PrimitiveTopology::TRIANGLE_LIST, 5 => vk::PrimitiveTopology::TRIANGLE_STRIP,
        // rage's drawTrisAdj maps to 12. With no geometry shader the rasterizer skips the adjacent vertices, as in D3D11.
        10 => vk::PrimitiveTopology::LINE_LIST_WITH_ADJACENCY, 11 => vk::PrimitiveTopology::LINE_STRIP_WITH_ADJACENCY,
        12 => vk::PrimitiveTopology::TRIANGLE_LIST_WITH_ADJACENCY, 13 => vk::PrimitiveTopology::TRIANGLE_STRIP_WITH_ADJACENCY,
        value => return Err(format!("unsupported topology {value}")),
    })
}

/// Whether a D3D11 sampler filters linearly in any way (minification, magnification, mips or anisotropy).
pub(crate) fn sampler_is_linear(words: &[u32]) -> bool { words.first().copied().unwrap_or(0) & 0x55 != 0 }

/// The same sampler with point filtering, for textures the device cannot filter linearly.
pub(crate) fn nearest_words(words: &[u32]) -> Vec<u32> {
    let mut words = words.to_vec();
    if let Some(filter) = words.first_mut() { *filter &= !0x7f; }
    words
}

/// A D3D11 sampler description (`D3D11_SAMPLER_DESC` words) as a native sampler.
pub(crate) fn create_sampler(gpu: &Gpu, words: &[u32]) -> Result<vk::Sampler, String> {
    let word = |index: usize| words.get(index).copied().unwrap_or(0);
    let address = |value: u32| Ok(match value {
        1 => vk::SamplerAddressMode::REPEAT, 2 => vk::SamplerAddressMode::MIRRORED_REPEAT, 0 | 3 => vk::SamplerAddressMode::CLAMP_TO_EDGE,
        4 => vk::SamplerAddressMode::CLAMP_TO_BORDER, 5 => vk::SamplerAddressMode::MIRROR_CLAMP_TO_EDGE,
        value => return Err(format!("unsupported sampler address mode {value}")),
    });
    let filter = word(0);
    let linear = |bit: u32| if filter & bit != 0 { vk::Filter::LINEAR } else { vk::Filter::NEAREST };
    let anisotropic = filter & 0x7f == 0x55;
    let compare = filter & 0x80 != 0;
    let rgba = [7, 8, 9, 10].map(|index| f32::from_bits(word(index)));
    let border = match rgba { [0.0, 0.0, 0.0, 0.0] => vk::BorderColor::FLOAT_TRANSPARENT_BLACK, [0.0, 0.0, 0.0, 1.0] => vk::BorderColor::FLOAT_OPAQUE_BLACK, [1.0, 1.0, 1.0, 1.0] => vk::BorderColor::FLOAT_OPAQUE_WHITE, _ => vk::BorderColor::FLOAT_CUSTOM_EXT };
    let mut custom = vk::SamplerCustomBorderColorCreateInfoEXT::default().custom_border_color(vk::ClearColorValue { float32: rgba }).format(vk::Format::UNDEFINED);
    let lod_min = f32::from_bits(word(11));
    let lod_max = f32::from_bits(word(12));
    let bias = f32::from_bits(word(4));
    if !lod_min.is_finite() || !bias.is_finite() { return Err("sampler LOD values are not finite".into()); }
    let lod_max = if lod_max.is_finite() { lod_max.max(lod_min.max(0.0)) } else { vk::LOD_CLAMP_NONE };
    let limit = gpu.props.limits.max_sampler_lod_bias;
    let mut info = vk::SamplerCreateInfo::default()
        .mag_filter(linear(1 << 2)).min_filter(linear(1 << 4))
        .mipmap_mode(if filter & 1 != 0 { vk::SamplerMipmapMode::LINEAR } else { vk::SamplerMipmapMode::NEAREST })
        .address_mode_u(address(word(1))?).address_mode_v(address(word(2))?).address_mode_w(address(word(3))?)
        .mip_lod_bias(if gpu.legacy { 0.0 } else { bias.clamp(-limit, limit) })
        .anisotropy_enable(anisotropic).max_anisotropy(if anisotropic { (word(5).clamp(1, 16) as f32).min(gpu.props.limits.max_sampler_anisotropy) } else { 1.0 })
        .compare_enable(compare).compare_op(if compare { compare_op(word(6))? } else { vk::CompareOp::ALWAYS })
        .min_lod(lod_min.max(0.0)).max_lod(lod_max).border_color(border);
    if anisotropic { info = info.mag_filter(vk::Filter::LINEAR).min_filter(vk::Filter::LINEAR).mipmap_mode(vk::SamplerMipmapMode::LINEAR); }
    if border == vk::BorderColor::FLOAT_CUSTOM_EXT { info = info.push_next(&mut custom); }
    unsafe { gpu.device.create_sampler(&info, Some(callbacks())) }.map_err(|error| format!("vkCreateSampler: {error}"))
}

/// Set and binding of a WGSL resource: VS resources in set 0, PS in set 1, CS in set 0; a packed
/// constant block goes to `pack_binding` of set 2 (a dynamic uniform buffer).
pub(crate) fn native_binding(stage: u32, binding: u32, pack_binding: Option<u32>) -> (u32, u32) {
    match pack_binding { Some(slot) => (2, slot), None => (if stage == 1 { 1 } else { 0 }, binding) }
}

/// Binding of a stage's packed constants in set 2: vertex 0, pixel 1, compute 0.
pub(crate) fn pack_binding(stage: u32) -> u32 { u32::from(stage == 1) }

pub(crate) fn pack_bindings(vs_packed: bool, ps_packed: bool) -> [Option<u32>; 2] {
    [vs_packed.then_some(pack_binding(0)), ps_packed.then_some(pack_binding(1))]
}

/// WGSL to SPIR-V with the same naga options wgpu used on this device, bindings moved to native sets.
pub(crate) fn compile_spirv(source: &str, stage: u32, pack_binding: Option<u32>) -> Result<Vec<u32>, String> {
    use naga::back::spv;
    let mut module = naga::front::wgsl::parse_str(source).map_err(|error| error.emit_to_string(source))?;
    for (_, variable) in module.global_variables.iter_mut() {
        if let Some(binding) = variable.binding.as_mut() {
            let is_pack = variable.space == naga::AddressSpace::Uniform && binding.binding == crate::shader::PACK_BINDING;
            let (group, slot) = native_binding(stage, binding.binding, pack_binding.filter(|_| is_pack));
            binding.group = group;
            binding.binding = slot;
        }
    }
    let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module).map_err(|error| error.emit_to_string(source))?;
    // Index clamps and loop counters cost no measurable GPU work (Adreno's compiler drops most of them):
    // switching both off measured 9.3 against 9.4 million cycles per frame (b66, 2026-10-15).
    let options = spv::Options {
        lang_version: (1, 6),
        flags: spv::WriterFlags::FORCE_POINT_SIZE,
        bounds_check_policies: naga::proc::BoundsCheckPolicies {
            index: naga::proc::BoundsCheckPolicy::Restrict,
            buffer: naga::proc::BoundsCheckPolicy::Unchecked,
            image_load: naga::proc::BoundsCheckPolicy::Unchecked,
            binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
        },
        zero_initialize_workgroup_memory: spv::ZeroInitializeWorkgroupMemoryMode::Native,
        force_loop_bounding: true,
        ..Default::default()
    };
    let shader_stage = match stage { 0 => naga::ShaderStage::Vertex, 1 => naga::ShaderStage::Fragment, _ => naga::ShaderStage::Compute };
    let pipeline = spv::PipelineOptions { shader_stage, entry_point: "main".into() };
    spv::write_vec(&module, &info, &options, Some(&pipeline)).map_err(|error| format!("SPIR-V: {error}"))
}

/// Pipeline and access stages a layout implies, for barriers into and out of it.
pub(crate) fn layout_scope(layout: vk::ImageLayout) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    use vk::{AccessFlags2 as A, ImageLayout as L, PipelineStageFlags2 as P};
    let tests = P::EARLY_FRAGMENT_TESTS | P::LATE_FRAGMENT_TESTS;
    let shaders = P::VERTEX_SHADER | P::FRAGMENT_SHADER | P::COMPUTE_SHADER;
    match layout {
        L::UNDEFINED => (P::NONE, A::NONE),
        L::COLOR_ATTACHMENT_OPTIMAL => (P::COLOR_ATTACHMENT_OUTPUT, A::COLOR_ATTACHMENT_READ | A::COLOR_ATTACHMENT_WRITE),
        L::DEPTH_STENCIL_ATTACHMENT_OPTIMAL => (tests, A::DEPTH_STENCIL_ATTACHMENT_READ | A::DEPTH_STENCIL_ATTACHMENT_WRITE),
        L::DEPTH_READ_ONLY_STENCIL_ATTACHMENT_OPTIMAL | L::DEPTH_ATTACHMENT_STENCIL_READ_ONLY_OPTIMAL =>
            (tests | shaders, A::DEPTH_STENCIL_ATTACHMENT_READ | A::DEPTH_STENCIL_ATTACHMENT_WRITE | A::SHADER_SAMPLED_READ),
        L::DEPTH_STENCIL_READ_ONLY_OPTIMAL => (tests | shaders, A::DEPTH_STENCIL_ATTACHMENT_READ | A::SHADER_SAMPLED_READ),
        L::SHADER_READ_ONLY_OPTIMAL => (shaders, A::SHADER_SAMPLED_READ),
        L::TRANSFER_SRC_OPTIMAL => (P::ALL_TRANSFER, A::TRANSFER_READ),
        L::TRANSFER_DST_OPTIMAL => (P::ALL_TRANSFER, A::TRANSFER_WRITE),
        L::PRESENT_SRC_KHR => (P::NONE, A::NONE),
        _ => (P::ALL_COMMANDS, A::MEMORY_READ | A::MEMORY_WRITE),
    }
}

/// Cache of descriptor-set and pipeline layouts by content.
#[derive(Default)]
pub(crate) struct LayoutCache {
    sets: HashMap<Vec<(u32, vk::DescriptorType, vk::ShaderStageFlags)>, vk::DescriptorSetLayout>,
    pipelines: HashMap<[vk::DescriptorSetLayout; 3], vk::PipelineLayout>,
}

impl LayoutCache {
    pub fn set_layout(&mut self, device: &ash::Device, bindings: Vec<(u32, vk::DescriptorType, vk::ShaderStageFlags)>) -> Result<vk::DescriptorSetLayout, String> {
        if let Some(layout) = self.sets.get(&bindings) { return Ok(*layout); }
        let native: Vec<vk::DescriptorSetLayoutBinding> = bindings.iter().map(|(binding, kind, stages)| vk::DescriptorSetLayoutBinding::default()
            .binding(*binding).descriptor_type(*kind).descriptor_count(1).stage_flags(*stages)).collect();
        let layout = unsafe { device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&native), Some(callbacks())) }
            .map_err(|error| format!("vkCreateDescriptorSetLayout: {error}"))?;
        self.sets.insert(bindings, layout);
        Ok(layout)
    }

    pub fn pipeline_layout(&mut self, device: &ash::Device, sets: [vk::DescriptorSetLayout; 3]) -> Result<vk::PipelineLayout, String> {
        if let Some(layout) = self.pipelines.get(&sets) { return Ok(*layout); }
        let layout = unsafe { device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&sets), Some(callbacks())) }
            .map_err(|error| format!("vkCreatePipelineLayout: {error}"))?;
        self.pipelines.insert(sets, layout);
        Ok(layout)
    }
}


