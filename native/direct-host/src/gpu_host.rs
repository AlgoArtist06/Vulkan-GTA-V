//! Android GPU worker boundary.
//!
//! The worker owns one Vulkan-backed renderer and consumes the direct C
//! engine's command ring until shutdown. The live shared heap is never
//! exposed as `&mut [u8]`. Ring waits use the direct C atomic wait/notify
//! registry, so the engine and renderer share one `(mapping,address)` queue.

#![cfg(target_os = "android")]

use gta5_native_renderer::{
    NativeRenderer, ProcessReport, RendererError, MovieFrameSource,
    SharedMemory,
};
use crate::memory::NativeMmapMemory;
use crate::worker_health::{WorkerHealth, HealthError};
use std::{
    ffi::c_void,
    fmt,
    path::PathBuf,
    sync::{
        atomic::Ordering,
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const RING_HEADER_BYTES: usize = 64;
const RING_STATE_OFFSET: usize = 8;
const RING_TAIL_OFFSET: usize = 4;
const RING_VALIDATION_OFFSET: usize = 16;
const RING_SLEEPING_OFFSET: usize = 20;
const RING_WAIT_TIMEOUT: Duration = Duration::from_millis(100);
/// Idle wake-up while the engine waits for GPU results.
const COMPLETION_POLL: Duration = Duration::from_millis(1);
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const SURFACE_RESPONSE_TIMEOUT: Duration = Duration::from_millis(750);

fn receive_response<T>(receiver: Receiver<T>) -> Result<T, GpuHostError> {
    receiver.recv_timeout(CONTROL_RESPONSE_TIMEOUT).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => GpuHostError::WorkerFailed("GPU worker response timed out".to_string()),
        mpsc::RecvTimeoutError::Disconnected => GpuHostError::WorkerClosed,
    })
}

/// Addresses passed by `wgpu_start_worker(ring, size, dbg)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuRingBinding {
    pub ring_base: usize,
    pub ring_len: usize,
    pub debug_address: Option<usize>,
}

impl GpuRingBinding {
    pub fn new(ring_base: usize, ring_len: usize, debug_address: usize) -> Self {
        Self {
            ring_base,
            ring_len,
            debug_address: (debug_address != 0).then_some(debug_address),
        }
    }

    fn validate(&self, memory: &NativeMmapMemory) -> Result<(), GpuHostError> {
        let memory_len = memory
            .logical_len()
            .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
        if self.ring_base % 4 != 0 {
            return Err(GpuHostError::Renderer("GPU ring base is not u32 aligned".to_string()));
        }
        if self.ring_len < RING_HEADER_BYTES + 8 {
            return Err(GpuHostError::Renderer("GPU ring is smaller than its header".to_string()));
        }
        let end = self
            .ring_base
            .checked_add(self.ring_len)
            .ok_or_else(|| GpuHostError::Renderer("GPU ring address overflows".to_string()))?;
        if end > memory_len {
            return Err(GpuHostError::Renderer(format!(
                "GPU ring {:#x}..{:#x} is outside shared memory ({})",
                self.ring_base,
                end,
                memory_len
            )));
        }
        if let Some(debug_address) = self.debug_address {
            if debug_address % 4 != 0 {
                return Err(GpuHostError::Renderer("GPU debug address is not u32 aligned".to_string()));
            }
            let debug_end = debug_address
                .checked_add(4)
                .ok_or_else(|| GpuHostError::Renderer("GPU debug address overflows".to_string()))?;
            if debug_end > memory_len {
                return Err(GpuHostError::Renderer("GPU debug address is outside shared memory".to_string()));
            }
        }
        Ok(())
    }
}

/// Errors returned by the dedicated renderer worker.
#[derive(Debug, Eq, PartialEq)]
pub enum GpuHostError {
    WorkerClosed,
    WorkerFailed(String),
    Renderer(String),
    ThreadPanic,
}

impl fmt::Display for GpuHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerClosed => f.write_str("native GPU worker channel closed"),
            Self::WorkerFailed(error) => write!(f, "native GPU worker failed: {error}"),
            Self::Renderer(error) => write!(f, "native renderer error: {error}"),
            Self::ThreadPanic => f.write_str("native GPU worker thread panicked"),
        }
    }
}

impl std::error::Error for GpuHostError {}

/// Last worker state. `failure` is set when the ring enters state 2.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GpuHostStatus {
    pub bound: bool,
    pub ring_state: u32,
    pub report: ProcessReport,
    pub failure: Option<String>,
}

struct BoundWake {
    memory: NativeMmapMemory,
    binding: GpuRingBinding,
}

/// Cloneable control handle used by the Android surface registry.
#[derive(Clone)]
pub struct GpuHostControl {
    commands: Sender<Request>,
    wake: Arc<Mutex<Option<BoundWake>>>,
    health: Arc<WorkerHealth>,
}

impl GpuHostControl {
    pub fn bind(
        &self,
        memory: &NativeMmapMemory,
        binding: GpuRingBinding,
        shader_dir: Option<PathBuf>,
    ) -> Result<(), GpuHostError> {
        binding.validate(memory)?;
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.commands
            .send(Request::Bind {
                memory: memory.clone(),
                binding,
                shader_dir,
                response: response_sender,
            })
            .map_err(|_| GpuHostError::WorkerClosed)?;
        let result = receive_response(response_receiver)?;
        if result.is_ok() {
            if let Ok(mut wake) = self.wake.lock() {
                *wake = Some(BoundWake {
                    memory: memory.clone(),
                    binding,
                });
            }
        }
        result
    }

    pub fn status(&self) -> Result<GpuHostStatus, GpuHostError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.commands
            .send(Request::Status { response: response_sender })
            .map_err(|_| GpuHostError::WorkerClosed)?;
        self.wake_bound();
        receive_response(response_receiver)?
    }

    fn wake_bound(&self) {
        let wake = self.wake.lock().ok().and_then(|wake| {
            wake.as_ref().map(|wake| (wake.memory.clone(), wake.binding))
        });
        if let Some((memory, binding)) = wake {
            let _ = memory.notify_u32(binding.ring_base, u32::MAX);
        }
    }

    fn request_unit(
        &self,
        make_request: impl FnOnce(SyncSender<Result<(), GpuHostError>>) -> Request,
    ) -> Result<(), GpuHostError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.commands
            .send(make_request(response_sender))
            .map_err(|_| GpuHostError::WorkerClosed)?;
        self.wake_bound();
        response_receiver.recv_timeout(SURFACE_RESPONSE_TIMEOUT)
            .map_err(|error|GpuHostError::WorkerFailed(format!("Surface control response: {error}")))?
    }

    pub fn attach_surface(&self, window_address: usize) -> Result<(), GpuHostError> {
        if window_address == 0 {
            return Err(GpuHostError::Renderer("ANativeWindow pointer is null".to_string()));
        }
        self.request_unit(|response| Request::Attach { window_address, response })
    }
    pub fn set_movie_source(&self, source: Option<MovieFrameSource>) -> Result<(), GpuHostError> {
        self.request_unit(|response| Request::MovieSource { source,response })
    }

    pub fn resize_surface(&self, width: u32, height: u32) -> Result<(), GpuHostError> {
        self.request_unit(|response| Request::Resize { width, height, response })
    }

    pub fn suspend_surface(&self) -> Result<(), GpuHostError> {
        self.request_unit(|response| Request::Suspend { response })
    }

    pub fn resume_surface(
        &self,
        window_address: usize,
        width: u32,
        height: u32,
    ) -> Result<(), GpuHostError> {
        if window_address == 0 {
            return Err(GpuHostError::Renderer("ANativeWindow pointer is null".to_string()));
        }
        self.request_unit(|response| Request::Resume {
            window_address,
            width,
            height,
            response,
        })
    }

    pub fn detach_surface(&self) -> Result<(), GpuHostError> {
        self.request_unit(|response| Request::Detach { response })
    }

    pub fn process_ring(
        &self,
        memory: &NativeMmapMemory,
        ring_base: usize,
        ring_len: usize,
    ) -> Result<ProcessReport, GpuHostError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.commands
            .send(Request::Process {
                memory: memory.clone(),
                ring_base,
                ring_len,
                response: response_sender,
            })
            .map_err(|_| GpuHostError::WorkerClosed)?;
        self.wake_bound();
        receive_response(response_receiver)?
    }

    pub fn process_binding(
        &self,
        memory: &NativeMmapMemory,
        binding: GpuRingBinding,
    ) -> Result<ProcessReport, GpuHostError> {
        self.process_ring(memory, binding.ring_base, binding.ring_len)
    }

    /// Process the ring that was bound to this worker. This is the C bridge's
    /// one-shot entry point; the worker also drains the ring autonomously
    /// after a real surface is attached.
    pub fn process_bound(&self) -> Result<ProcessReport, GpuHostError> {
        let wake = self.wake.lock().ok().and_then(|wake| {
            wake.as_ref().map(|wake| (wake.memory.clone(), wake.binding))
        });
        let Some((memory, binding)) = wake else {
            return Err(GpuHostError::WorkerFailed("GPU ring is not bound".to_string()));
        };
        self.process_binding(&memory, binding)
    }
}

enum Request {
    MovieSource { source: Option<MovieFrameSource>, response: SyncSender<Result<(),GpuHostError>> },
    Bind {
        memory: NativeMmapMemory,
        binding: GpuRingBinding,
        shader_dir: Option<PathBuf>,
        response: SyncSender<Result<(), GpuHostError>>,
    },
    Attach {
        window_address: usize,
        response: SyncSender<Result<(), GpuHostError>>,
    },
    Resize {
        width: u32,
        height: u32,
        response: SyncSender<Result<(), GpuHostError>>,
    },
    Suspend {
        response: SyncSender<Result<(), GpuHostError>>,
    },
    Resume {
        window_address: usize,
        width: u32,
        height: u32,
        response: SyncSender<Result<(), GpuHostError>>,
    },
    Detach {
        response: SyncSender<Result<(), GpuHostError>>,
    },
    Process {
        memory: NativeMmapMemory,
        ring_base: usize,
        ring_len: usize,
        response: SyncSender<Result<ProcessReport, GpuHostError>>,
    },
    Status {
        response: SyncSender<Result<GpuHostStatus, GpuHostError>>,
    },
    Shutdown {
        response: SyncSender<Result<(), GpuHostError>>,
    },
}

/// Owns a Vulkan-backed [`NativeRenderer`] and its command thread.
pub struct GpuHost {
    control: GpuHostControl,
    thread: Option<JoinHandle<()>>,
}

impl GpuHost {
    /// Start the control thread. Real Vulkan/device creation occurs in
    /// [`Self::bind`], after the engine supplies its ring and shader directory.
    pub fn start() -> Result<Self, GpuHostError> {
        let (commands, requests) = mpsc::channel();
        let wake = Arc::new(Mutex::new(None));
        let health = Arc::new(WorkerHealth::new());
        let worker_health = Arc::clone(&health);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("gta5-native-gpu".to_string())
            .spawn(move || worker(requests, ready_sender, worker_health))
            .map_err(|error| GpuHostError::WorkerFailed(error.to_string()))?;
        match ready_receiver.recv_timeout(CONTROL_RESPONSE_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                control: GpuHostControl { commands, wake, health },
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(GpuHostError::WorkerFailed("GPU worker startup timed out".to_string()))
            }
        }
    }

    pub fn control(&self) -> GpuHostControl {
        self.control.clone()
    }

    pub fn bind(
        &self,
        memory: &NativeMmapMemory,
        binding: GpuRingBinding,
        shader_dir: Option<PathBuf>,
    ) -> Result<(), GpuHostError> {
        self.control.bind(memory, binding, shader_dir)
    }

    pub fn check_failure(&self) -> Result<(), GpuHostError> {
        self.control.health.check().map_err(|error| match error {
            HealthError::Failed(error) => GpuHostError::WorkerFailed(error),
            HealthError::Closed => GpuHostError::WorkerClosed,
        })
    }

    pub fn status(&self) -> Result<GpuHostStatus, GpuHostError> {
        self.control.status()
    }

    /// `WorkerHealth::progress`, read without queueing behind the worker.
    pub fn progress(&self) -> (u32, bool) {
        self.control.health.progress()
    }

    pub fn attach_surface(&self, window_address: usize) -> Result<(), GpuHostError> {
        self.control.attach_surface(window_address)
    }
    pub fn resize_surface(&self, width: u32, height: u32) -> Result<(), GpuHostError> {
        self.control.resize_surface(width, height)
    }
    pub fn suspend_surface(&self) -> Result<(), GpuHostError> {
        self.control.suspend_surface()
    }
    pub fn resume_surface(&self, window_address: usize, width: u32, height: u32) -> Result<(), GpuHostError> {
        self.control.resume_surface(window_address, width, height)
    }
    pub fn detach_surface(&self) -> Result<(), GpuHostError> {
        self.control.detach_surface()
    }
    pub fn process_ring(&self, memory: &NativeMmapMemory, ring_base: usize, ring_len: usize) -> Result<ProcessReport, GpuHostError> {
        self.control.process_ring(memory, ring_base, ring_len)
    }
    pub fn process_binding(&self, memory: &NativeMmapMemory, binding: GpuRingBinding) -> Result<ProcessReport, GpuHostError> {
        self.control.process_binding(memory, binding)
    }
    pub fn process_bound(&self) -> Result<ProcessReport, GpuHostError> {
        self.control.process_bound()
    }

    pub fn shutdown(mut self) -> Result<(), GpuHostError> {
        let result = self.shutdown_in_place();
        // The worker owns the only Rust clones of the caller's mapping. Join
        // it before returning so the C owner may safely unmap after stop.
        // Ring waits are bounded and `shutdown_in_place` already wakes them.
        let join_result = self
            .thread
            .take()
            .map(|thread| thread.join().map_err(|_| GpuHostError::ThreadPanic))
            .unwrap_or(Ok(()));
        result.and(join_result)
    }

    fn shutdown_in_place(&mut self) -> Result<(), GpuHostError> {
        if self.thread.is_none() {
            return Ok(());
        }
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.control
            .commands
            .send(Request::Shutdown { response: response_sender })
            .map_err(|_| GpuHostError::WorkerClosed)?;
        self.control.wake_bound();
        receive_response(response_receiver)?
    }
}

impl Drop for GpuHost {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        if self
            .control
            .commands
            .send(Request::Shutdown { response: response_sender })
            .is_ok()
        {
            self.control.wake_bound();
            let _ = response_receiver.recv_timeout(CONTROL_RESPONSE_TIMEOUT);
        }
        // A detached GPU worker could still dereference the caller-owned
        // mapping. Always join before dropping the last control handle.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct BoundRuntime {
    renderer: NativeRenderer,
    memory: NativeMmapMemory,
    binding: GpuRingBinding,
    report: ProcessReport,
    failure: Option<String>,
    paused: bool,
}

impl BoundRuntime {
    fn status(&self) -> GpuHostStatus {
        let ring_state = self
            .memory
            .load_u32(self.binding.ring_base + RING_STATE_OFFSET, Ordering::Acquire)
            .unwrap_or(2);
        GpuHostStatus {
            bound: true,
            ring_state,
            report: self.report.clone(),
            failure: self.failure.clone(),
        }
    }

    fn mark_failure(&mut self, error: String) {
        let _ = self.memory.store_u32(
            self.binding.ring_base + RING_STATE_OFFSET,
            2,
            Ordering::Release,
        );
        let _ = self.memory.store_u32(
            self.binding.ring_base + RING_SLEEPING_OFFSET,
            0,
            Ordering::Release,
        );
        let _ = self
            .memory
            .notify_u32(self.binding.ring_base + RING_TAIL_OFFSET, u32::MAX);
        if let Ok(errors) = self
            .memory
            .load_u32(self.binding.ring_base + RING_VALIDATION_OFFSET, Ordering::Acquire)
        {
            self.report.validation_errors = errors;
        }
        self.failure.get_or_insert(error);
    }

    fn process(&mut self) -> Result<ProcessReport, GpuHostError> {
        if let Some(error) = &self.failure {
            return Err(GpuHostError::WorkerFailed(error.clone()));
        }
        if self.paused {
            return Ok(self.report.clone());
        }
        match self.renderer.process_shared_ring(
            &self.memory,
            self.binding.ring_base,
            self.binding.ring_len,
        ) {
            Ok(report) => {
                self.report = report.clone();
                let _ = self
                    .memory
                    .notify_u32(self.binding.ring_base + RING_TAIL_OFFSET, u32::MAX);
                Ok(report)
            }
            Err(error) => {
                let text = error.to_string();
                self.mark_failure(text.clone());
                Err(GpuHostError::Renderer(text))
            }
        }
    }

    fn pump(&mut self) -> Result<bool, GpuHostError> {
        if self.failure.is_some() || self.paused {
            return Ok(false);
        }
        let state = self
            .memory
            .load_u32(self.binding.ring_base + RING_STATE_OFFSET, Ordering::Acquire)
            .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
        if state == 2 {
            if let Ok(errors) = self
                .memory
                .load_u32(self.binding.ring_base + RING_VALIDATION_OFFSET, Ordering::Acquire)
            {
                self.report.validation_errors = errors;
            }
            self.failure = Some("GPU ring entered failure state 2".to_string());
            return Ok(false);
        }
        if state != 1 {
            return Err(GpuHostError::Renderer(format!(
                "GPU ring has invalid running state {state}"
            )));
        }
        let head = self
            .memory
            .load_u32(self.binding.ring_base, Ordering::Acquire)
            .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
        let tail = self
            .memory
            .load_u32(self.binding.ring_base + RING_TAIL_OFFSET, Ordering::Acquire)
            .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
        if head == tail {
            return Ok(false);
        }
        self.process().map(|_| true)
    }

    /// Publishes finished GPU results (fences, events, readbacks) the engine waits for.
    fn complete(&mut self) -> Result<(), GpuHostError> {
        self.renderer.complete(&self.memory).map_err(|error| GpuHostError::Renderer(error.to_string()))
    }

    fn wait_for_head(&self) -> Result<(), GpuHostError> {
        let expected = self
            .memory
            .load_u32(self.binding.ring_base, Ordering::Acquire)
            .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
        self.memory
            .store_u32(
                self.binding.ring_base + RING_SLEEPING_OFFSET,
                1,
                Ordering::Release,
            )
            .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
        let result = self
            .memory
            .wait32(self.binding.ring_base, expected, Some(if self.renderer.has_pending_completions() { COMPLETION_POLL } else { RING_WAIT_TIMEOUT }))
            .map(|_| ())
            .map_err(|error| GpuHostError::WorkerFailed(error.to_string()));
        let clear_result = self
            .memory
            .store_u32(
                self.binding.ring_base + RING_SLEEPING_OFFSET,
                0,
                Ordering::Release,
            )
            .map_err(|error| GpuHostError::Renderer(error.to_string()));
        result.and(clear_result)
    }
}

fn renderer_error(error: RendererError) -> GpuHostError {
    GpuHostError::Renderer(error.to_string())
}

fn unbound_status() -> GpuHostStatus {
    GpuHostStatus::default()
}

fn handle_unbound(request: Request, runtime: &mut Option<BoundRuntime>) -> bool {
    match request {
        Request::Bind { memory, binding, shader_dir, response } => {
            let result = (|| {
                binding.validate(&memory)?;
                let state = memory
                    .load_u32(binding.ring_base + RING_STATE_OFFSET, Ordering::Acquire)
                    .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
                if state > 2 {
                    return Err(GpuHostError::WorkerFailed(
                        format!("GPU ring has invalid state {state}"),
                    ));
                }
                if state == 0 {
                    memory
                        .store_u32(binding.ring_base + RING_TAIL_OFFSET, RING_HEADER_BYTES as u32, Ordering::Release)
                        .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
                    memory
                        .store_u32(binding.ring_base + RING_STATE_OFFSET, 1, Ordering::Release)
                        .map_err(|error| GpuHostError::Renderer(error.to_string()))?;
                }
                let renderer = NativeRenderer::with_shader_dir(shader_dir.as_deref())
                    .map_err(renderer_error)?;
                let candidate = BoundRuntime {
                    renderer,
                    memory,
                    binding,
                    report: ProcessReport::default(),
                    failure: None,
                    // Do not consume the ring until a real ANativeWindow is
                    // attached. PRESENT has no valid native fallback.
                    paused: true,
                };
                *runtime = Some(candidate);
                Ok(())
            })();
            let _ = response.send(result);
        }
        Request::Status { response } => {
            let _ = response.send(Ok(unbound_status()));
        }
        Request::Shutdown { response } => {
            let _ = response.send(Ok(()));
            return true;
        }
        Request::MovieSource { response, .. }
        | Request::Attach { response, .. }
        | Request::Resize { response, .. }
        | Request::Suspend { response }
        | Request::Resume { response, .. }
        | Request::Detach { response } => {
            let _ = response.send(Err(GpuHostError::WorkerFailed(
                "GPU ring is not bound".to_string(),
            )));
        }
        Request::Process { response, .. } => {
            let _ = response.send(Err(GpuHostError::WorkerFailed(
                "GPU ring is not bound".to_string(),
            )));
        }
    }
    false
}

fn handle_bound(request: Request, runtime: &mut BoundRuntime) -> bool {
    match request {
        Request::MovieSource { source,response } => {
            let result=runtime.renderer.set_movie_source(source).map_err(renderer_error);
            let _=response.send(result);
        }
        Request::Bind { response, .. } => {
            let _ = response.send(Err(GpuHostError::WorkerFailed(
                "GPU ring is already bound".to_string(),
            )));
        }
        Request::Status { response } => {
            let _ = response.send(Ok(runtime.status()));
        }
        Request::Attach { window_address, response } => {
            let result = if window_address == 0 {
                Err(GpuHostError::Renderer("ANativeWindow pointer is null".to_string()))
            } else {
                unsafe {
                    runtime
                        .renderer
                        .attach_anative_window(window_address as *mut c_void)
                        .map_err(renderer_error)
                        .map(|()| {
                            runtime.paused = false;
                        })
                }
            };
            let _ = response.send(result);
        }
        Request::Resize { width, height, response } => {
            let _ = response.send(runtime.renderer.resize_surface(width, height).map_err(renderer_error));
        }
        Request::Suspend { response } => {
            runtime.paused = true;
            runtime.renderer.detach_surface();
            let _ = response.send(Ok(()));
        }
        Request::Detach { response } => {
            runtime.paused = true;
            runtime.renderer.detach_surface();
            let _ = response.send(Ok(()));
        }
        Request::Resume { window_address, width, height, response } => {
            let result = if window_address == 0 {
                Err(GpuHostError::Renderer("ANativeWindow pointer is null".to_string()))
            } else {
                unsafe {
                    runtime
                        .renderer
                        .attach_anative_window(window_address as *mut c_void)
                        .map_err(renderer_error)
                        .and_then(|()| runtime.renderer.resize_surface(width, height).map_err(renderer_error))
                        .map(|()| {
                            runtime.paused = false;
                        })
                }
            };
            let _ = response.send(result);
        }
        Request::Process { memory, ring_base, ring_len, response } => {
            if memory.len() != runtime.memory.len() || ring_base != runtime.binding.ring_base || ring_len != runtime.binding.ring_len {
                let _ = response.send(Err(GpuHostError::WorkerFailed(
                    "process request does not match the bound ring".to_string(),
                )));
            } else {
                let _ = response.send(runtime.process());
            }
        }
        Request::Shutdown { response } => {
            runtime.paused = true;
            runtime.renderer.detach_surface();
            let _ = response.send(Ok(()));
            return true;
        }
    }
    false
}

struct HealthLifetime(Arc<WorkerHealth>);
impl Drop for HealthLifetime {
    fn drop(&mut self) {
        if thread::panicking() { self.0.fail("GPU worker thread panicked".into()); }
        self.0.stopped();
    }
}

/// `setprop debug.gta5.worker_cpus 7` (comma-separated CPU numbers): pins this thread to those cores (diagnostic).
#[cfg(target_os = "android")]
fn pin_worker() {
    use std::ffi::{c_char, CStr};
    unsafe extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
        fn sched_setaffinity(pid: i32, size: usize, mask: *const u64) -> i32;
    }
    let mut value = [0 as c_char; 92];
    if unsafe { __system_property_get(c"debug.gta5.worker_cpus".as_ptr(), value.as_mut_ptr()) } <= 0 { return; }
    let text = unsafe { CStr::from_ptr(value.as_ptr()) }.to_string_lossy().into_owned();
    let mut mask = [0u64; 16];
    for cpu in text.split(',').filter_map(|cpu| cpu.trim().parse::<usize>().ok()).filter(|cpu| *cpu < 1024) { mask[cpu / 64] |= 1 << (cpu % 64); }
    if mask == [0; 16] { return; }
    let result = unsafe { sched_setaffinity(0, std::mem::size_of_val(&mask), mask.as_ptr()) };
    crate::services::android_log_line(&format!("GPU worker pinned to CPUs {text}: {}", if result == 0 { "ok".to_string() } else { std::io::Error::last_os_error().to_string() }), false);
}

/// Panic messages go to stderr, which Android drops: log them (message and source line) before the default hook.
fn log_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::services::android_log_line(&format!("panic in {}: {info}", thread::current().name().unwrap_or("unnamed thread")), true);
            previous(info);
        }));
    });
}

fn worker(requests: Receiver<Request>, ready: SyncSender<Result<(), GpuHostError>>, health: Arc<WorkerHealth>) {
    log_panics();
    let _health_lifetime = HealthLifetime(Arc::clone(&health));
    #[cfg(target_os = "android")]
    pin_worker();
    if ready.send(Ok(())).is_err() {
        return;
    }
    let mut runtime = None;
    loop {
        if runtime.is_none() {
            let request = match requests.recv() {
                Ok(request) => request,
                Err(_) => break,
            };
            if handle_unbound(request, &mut runtime) {
                break;
            }
            continue;
        }

        let failure = runtime.as_ref().and_then(|runtime| runtime.failure.clone());
        if let Some(error) = &failure { health.fail(error.clone()); }
        if failure.is_some() {
            let request = match requests.recv() {
                Ok(request) => request,
                Err(_) => break,
            };
            if let Some(runtime) = runtime.as_mut() {
                if handle_bound(request, runtime) {
                    break;
                }
            }
            continue;
        }

        // Drain control requests before touching the ring, then pump all
        // currently published commands. Surface controls wake the atomic wait.
        let mut stopped = false;
        while let Ok(request) = requests.try_recv() {
            if let Some(runtime) = runtime.as_mut() {
                stopped = handle_bound(request, runtime);
            }
            if stopped {
                break;
            }
        }
        if stopped {
            break;
        }
        let progressed = match runtime.as_mut().expect("bound runtime").pump() {
            Ok(progressed) => progressed,
            Err(error) => {
                runtime.as_mut().expect("bound runtime").mark_failure(error.to_string());
                false
            }
        };
        if runtime.as_ref().and_then(|runtime| runtime.failure.as_ref()).is_some() {
            continue;
        }
        if let Some(runtime) = runtime.as_ref() { health.set_progress(runtime.report.presents, runtime.report.world); }
        if !progressed {
            let wait_error = runtime
                .as_mut()
                .and_then(|runtime| runtime.wait_for_head().and_then(|()| runtime.complete()).err());
            if let Some(error) = wait_error {
                if let Some(runtime) = runtime.as_mut() {
                    runtime.mark_failure(error.to_string());
                }
            }
        }
    }
}


