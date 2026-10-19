use crate::gpu::{callbacks, Gpu};
use ash::vk;
use std::ffi::c_void;
use std::ptr::NonNull;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SurfaceError {
    #[error("ANativeWindow pointer is null")]
    NullWindow,
    #[error("Android surface creation failed: {0}")]
    Create(String),
    #[error("Android surface is unsupported: {0}")]
    Unsupported(String),
    #[error("surface size must be non-zero")]
    InvalidSize,
    #[error("Android surface needs reconfiguration")]
    NeedsReconfigure,
    #[error("Android surface frame acquisition timed out")]
    Timeout,
    #[error("surface GPU operation failed: {0}")]
    Gpu(String),
}

impl SurfaceError {
    /// The frame may be dropped and the ring can continue (normal during Android surface changes).
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Timeout | Self::NeedsReconfigure)
    }
}

/// A Vulkan swapchain on the activity's `ANativeWindow`. It holds its own window
/// reference until dropped; all calls stay on the renderer thread.
pub struct NativeSurface {
    window: NonNull<c_void>,
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    pub images: Vec<vk::Image>,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    /// Waited by the frame's submit; one more than the images so a slot is free while others wait.
    acquired: Vec<(vk::Semaphore, u64)>,
    /// Signalled by the frame's submit, waited by present; one per image.
    rendered: Vec<vk::Semaphore>,
    next: usize,
}

pub(crate) struct Frame {
    pub index: u32,
    pub image: vk::Image,
    pub acquired: vk::Semaphore,
    pub rendered: vk::Semaphore,
    slot: usize,
}

impl NativeSurface {
    /// # Safety
    /// `window` must be a live `ANativeWindow` for the duration of this call.
    pub(crate) unsafe fn new(gpu: &Gpu, window: *mut c_void) -> Result<Self, SurfaceError> {
        let window = NonNull::new(window).ok_or(SurfaceError::NullWindow)?;
        #[cfg(target_os = "android")]
        unsafe { ANativeWindow_acquire(window.as_ptr()) };
        let info = vk::AndroidSurfaceCreateInfoKHR::default().window(window.as_ptr().cast());
        let surface = unsafe { gpu.android_surface_fn.create_android_surface(&info, Some(callbacks())) }.map_err(|error| {
            #[cfg(target_os = "android")]
            unsafe { ANativeWindow_release(window.as_ptr()) };
            SurfaceError::Create(error.to_string())
        })?;
        Ok(Self { window, surface, swapchain: vk::SwapchainKHR::null(), images: Vec::new(), format: vk::Format::UNDEFINED, extent: vk::Extent2D::default(), acquired: Vec::new(), rendered: Vec::new(), next: 0 })
    }

    pub fn is_configured(&self) -> bool { self.swapchain != vk::SwapchainKHR::null() }

    /// Tells Android the rate this window presents at (0 = no preference), so the display can refresh slower.
    pub(crate) fn set_frame_rate(&self, fps: u32) -> i32 {
        #[cfg(target_os = "android")]
        {
            // API 30; looked up at run time because the library links against an older API level.
            // RTLD_DEFAULT does not see libandroid from the app's linker namespace: open it by name (already loaded).
            unsafe extern "C" {
                fn dlopen(name: *const std::ffi::c_char, flags: i32) -> *mut c_void;
                fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
            }
            const RTLD_NOW: i32 = 2;
            let library = unsafe { dlopen(c"libandroid.so".as_ptr(), RTLD_NOW) };
            if library.is_null() { return -2; }
            let address = unsafe { dlsym(library, c"ANativeWindow_setFrameRate".as_ptr()) };
            if address.is_null() { return -1; }
            let set_frame_rate: unsafe extern "C" fn(*mut c_void, f32, i8) -> i32 = unsafe { std::mem::transmute(address) };
            // ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_FIXED_SOURCE: the content is made at exactly this rate.
            return unsafe { set_frame_rate(self.window.as_ptr(), fps as f32, 1) };
        }
        #[cfg(not(target_os = "android"))]
        { let _ = fps; 0 }
    }

    /// (Re)creates the swapchain at `width`x`height`; `srgb` matches the presented back buffer's encoding.
    pub(crate) fn configure(&mut self, gpu: &mut Gpu, width: u32, height: u32, srgb: bool) -> Result<(), SurfaceError> {
        if width == 0 || height == 0 { return Err(SurfaceError::InvalidSize); }
        let error = |e: vk::Result| SurfaceError::Gpu(e.to_string());
        unsafe { gpu.device.device_wait_idle() }.map_err(error)?;
        gpu.refresh_completed();
        let caps = unsafe { gpu.surface_fn.get_physical_device_surface_capabilities(gpu.physical, self.surface) }.map_err(error)?;
        let formats = unsafe { gpu.surface_fn.get_physical_device_surface_formats(gpu.physical, self.surface) }.map_err(error)?;
        let modes = unsafe { gpu.surface_fn.get_physical_device_surface_present_modes(gpu.physical, self.surface) }.map_err(error)?;
        let format = surface_format(&formats.iter().map(|f| f.format).collect::<Vec<_>>(), srgb).ok_or_else(|| SurfaceError::Unsupported("surface exposes no formats".into()))?;
        let color_space = formats.iter().find(|f| f.format == format).map_or(vk::ColorSpaceKHR::SRGB_NONLINEAR, |f| f.color_space);
        let mode = [vk::PresentModeKHR::IMMEDIATE, vk::PresentModeKHR::MAILBOX].into_iter().find(|m| modes.contains(m)).unwrap_or(vk::PresentModeKHR::FIFO);
        let alpha = [vk::CompositeAlphaFlagsKHR::OPAQUE, vk::CompositeAlphaFlagsKHR::INHERIT].into_iter().find(|a| caps.supported_composite_alpha.contains(*a))
            .ok_or_else(|| SurfaceError::Unsupported("no opaque composite alpha".into()))?;
        if !caps.supported_usage_flags.contains(vk::ImageUsageFlags::TRANSFER_DST) { return Err(SurfaceError::Unsupported("swapchain images cannot be blit targets".into())); }
        let transform = if caps.supported_transforms.contains(vk::SurfaceTransformFlagsKHR::IDENTITY) { vk::SurfaceTransformFlagsKHR::IDENTITY } else { caps.current_transform };
        let count = if caps.max_image_count == 0 { caps.min_image_count.max(2) } else { caps.min_image_count.max(2).min(caps.max_image_count) };
        let old = self.swapchain;
        let info = vk::SwapchainCreateInfoKHR::default().surface(self.surface).min_image_count(count).image_format(format).image_color_space(color_space)
            .image_extent(vk::Extent2D { width, height }).image_array_layers(1).image_usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE).pre_transform(transform).composite_alpha(alpha).present_mode(mode).clipped(true).old_swapchain(old);
        let swapchain = unsafe { gpu.swapchain_fn.create_swapchain(&info, Some(callbacks())) }.map_err(|e| SurfaceError::Create(e.to_string()))?;
        self.destroy_swapchain(gpu, old);
        self.swapchain = swapchain;
        self.images = unsafe { gpu.swapchain_fn.get_swapchain_images(swapchain) }.map_err(error)?;
        self.format = format;
        self.extent = vk::Extent2D { width, height };
        for _ in 0..=self.images.len() {
            let semaphore = unsafe { gpu.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), Some(callbacks())) }.map_err(error)?;
            self.acquired.push((semaphore, 0));
        }
        for _ in 0..self.images.len() {
            self.rendered.push(unsafe { gpu.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), Some(callbacks())) }.map_err(error)?);
        }
        crate::renderer::log_line(&format!("swapchain {width}x{height} {format:?} {mode:?}, {} images", self.images.len()));
        Ok(())
    }

    /// The next image to draw into. `submission` is the timeline value of the submit that will wait on it.
    pub(crate) fn acquire(&mut self, gpu: &mut Gpu, submission: u64) -> Result<Frame, SurfaceError> {
        if !self.is_configured() { return Err(SurfaceError::NeedsReconfigure); }
        let slot = self.next % self.acquired.len();
        // The semaphore's previous waiter must have run before it is signalled again.
        gpu.wait_for(self.acquired[slot].1, std::time::Duration::from_secs(5)).map_err(SurfaceError::Gpu)?;
        let (semaphore, _) = self.acquired[slot];
        match unsafe { gpu.swapchain_fn.acquire_next_image(self.swapchain, 1_000_000_000, semaphore, vk::Fence::null()) } {
            Ok((index, _suboptimal)) => {
                self.next += 1;
                self.acquired[slot].1 = submission;
                Ok(Frame { index, image: self.images[index as usize], acquired: semaphore, rendered: self.rendered[index as usize], slot })
            }
            Err(vk::Result::TIMEOUT | vk::Result::NOT_READY) => Err(SurfaceError::Timeout),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR | vk::Result::ERROR_SURFACE_LOST_KHR) => Err(SurfaceError::NeedsReconfigure),
            Err(error) => Err(SurfaceError::Gpu(error.to_string())),
        }
    }

    pub(crate) fn present(&mut self, gpu: &Gpu, frame: &Frame) -> Result<(), SurfaceError> {
        let _ = frame.slot;
        let swapchains = [self.swapchain];
        let indices = [frame.index];
        let waits = [frame.rendered];
        match unsafe { gpu.swapchain_fn.queue_present(gpu.queue, &vk::PresentInfoKHR::default().wait_semaphores(&waits).swapchains(&swapchains).image_indices(&indices)) } {
            Ok(_) => Ok(()),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR | vk::Result::ERROR_SURFACE_LOST_KHR) => Err(SurfaceError::NeedsReconfigure),
            Err(error) => Err(SurfaceError::Gpu(error.to_string())),
        }
    }

    fn destroy_swapchain(&mut self, gpu: &Gpu, swapchain: vk::SwapchainKHR) {
        unsafe {
            for (semaphore, _) in self.acquired.drain(..) { gpu.device.destroy_semaphore(semaphore, Some(callbacks())); }
            for semaphore in self.rendered.drain(..) { gpu.device.destroy_semaphore(semaphore, Some(callbacks())); }
            if swapchain != vk::SwapchainKHR::null() { gpu.swapchain_fn.destroy_swapchain(swapchain, Some(callbacks())); }
        }
        self.images.clear();
        self.next = 0;
    }

    /// Destroys the swapchain and surface and releases the window. The device must be idle.
    pub(crate) fn destroy(mut self, gpu: &Gpu) {
        let swapchain = std::mem::replace(&mut self.swapchain, vk::SwapchainKHR::null());
        self.destroy_swapchain(gpu, swapchain);
        unsafe { gpu.surface_fn.destroy_surface(self.surface, Some(callbacks())) };
        #[cfg(target_os = "android")]
        unsafe { ANativeWindow_release(self.window.as_ptr()) };
    }
}

#[cfg(target_os = "android")]
#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_acquire(window: *mut c_void);
    fn ANativeWindow_release(window: *mut c_void);
}

fn surface_format(formats: &[vk::Format], srgb: bool) -> Option<vk::Format> {
    use vk::Format as F;
    let preferred = if srgb { [F::R8G8B8A8_SRGB, F::B8G8R8A8_SRGB] } else { [F::R8G8B8A8_UNORM, F::B8G8R8A8_UNORM] };
    let is_srgb = |f: &vk::Format| matches!(*f, F::R8G8B8A8_SRGB | F::B8G8R8A8_SRGB | F::A8B8G8R8_SRGB_PACK32);
    preferred.into_iter().find(|f| formats.contains(f))
        .or_else(|| formats.iter().copied().find(|f| is_srgb(f) == srgb))
        .or_else(|| formats.first().copied())
}


