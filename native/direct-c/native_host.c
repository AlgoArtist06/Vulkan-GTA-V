/*
 * Browser-free direct-C host for the generated engine.
 *
 * The generated module owns no process-wide state.  This file owns the
 * process boundary: one shared linear memory, one generated instance per
 * native thread, exact Emscripten pthread record offsets, and checked
 * forwarding to the Rust platform services.  There is no module loader,
 * browser object, or VM in this path.
 */
#include "native_host.h"
#include "movie_compat.h"
#include "native_wait.h"
#include "../dxvk-bridge/bridge.h"

#include <errno.h>
#ifdef __ANDROID__
#include <sys/system_properties.h>
#else
#define PROP_VALUE_MAX 92
static int __system_property_get(const char *name, char *value) { (void)name; value[0] = 0; return 0; }
#endif
#include <math.h>
#include <pthread.h>
#include <sched.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/mman.h>
#include <unwind.h>
#include <dlfcn.h>
#ifdef __ANDROID__
#include <android/log.h>
#include <android/native_window.h>
#include <sys/prctl.h>
#endif

#define GTA_NATIVE_MAX_THREADS 128u
#define GTA_NATIVE_INITIAL_PAGES 49152ull
#define GTA_NATIVE_MAX_PAGES 262144ull
#define GTA_NATIVE_PAGE_SIZE 65536ull
#define GTA_NATIVE_THREAD_STACK_HIGH_OFFSET 80ull
#define GTA_NATIVE_THREAD_STACK_SIZE_OFFSET 88ull
#define GTA_NATIVE_THREAD_MAILBOX_OFFSET 204ull
#define GTA_NATIVE_THREAD_RECORD_BYTES 208ull
#define GTA_NATIVE_EAGAIN 6u
#define GTA_NATIVE_MAX_PROXY_ARGS 1024u
#define GTA_NATIVE_MAX_PROXY_SIGNATURE 64u
/* Input kind handled here, not by the Rust services: the touch overlay's virtual gamepad. */
#define GTA_NATIVE_INPUT_PAD 10
#define GTA_NATIVE_PAD_CENTRE 0x80808080u
#define GTA_NATIVE_MAX_PROXY_QUEUE 32u
#define GTA_NATIVE_MAX_ERROR 2048u
/* Engine calls are not depth-counted (WASM_RT_NONCONFORMING_UNCHECKED_STACK_EXHAUSTION):
 * deep recursion ends on this stack's guard page instead of a counted trap. */
#define GTA_NATIVE_ENGINE_STACK_BYTES (16u << 20)

/* The Rust static library supplies these symbols.  They deliberately use
 * opaque pointers so the C ABI does not depend on Rust layout. */
typedef struct GtaNativeServices GtaNativeServices;
typedef struct GtaNativeGpu GtaNativeGpu;
extern GtaNativeServices *gta_native_services_create(
    void *base, size_t reserved, const _Atomic uint64_t *logical_size,
    const char *assets, const char *saves);
extern int gta_native_services_call(GtaNativeServices *services,
                                    const char *name, const uint64_t *args,
                                    size_t argc, uint64_t *result);
extern size_t gta_native_services_last_error(const GtaNativeServices *services,
                                             char *output, size_t capacity);
extern void gta_native_services_destroy(GtaNativeServices *services);
extern int gta_native_services_input(GtaNativeServices *, int, int, float, float);
extern int gta_native_services_viewport(GtaNativeServices *, uint32_t, uint32_t);
extern size_t gta_native_services_take_lines(GtaNativeServices *, char *, size_t);
extern uint64_t gta_native_services_data_bytes(GtaNativeServices *);

extern GtaNativeGpu *gta_native_gpu_create(void);
extern int gta_native_gpu_bind_with_size(
    GtaNativeGpu *handle, void *heap, size_t reserved,
    const void *logical_size, size_t ring_base, size_t ring_len,
    size_t debug_address, const char *shader_dir);
extern int gta_native_gpu_stop(GtaNativeGpu *handle);
#ifdef __ANDROID__
extern int gta_native_gpu_movies(GtaNativeGpu *,GtaMovieFrameCallback,void *);
#endif
extern int gta_native_gpu_surface(GtaNativeGpu *, uint32_t, void *, uint32_t, uint32_t);
extern size_t gta_native_gpu_last_error(const GtaNativeGpu *, char *, size_t);
extern int gta_native_gpu_check(GtaNativeGpu *);
extern void gta_native_gpu_progress(GtaNativeGpu *, uint32_t *, bool *);

/* DXVK backend (debug.gta5.dxvk=1): libgta5_dxvk.so runs the wasm_gpu protocol on DXVK's D3D11.
 * The patched wasm_gpu::Emit and EmitData (game_35.c, patch_dxvk_emit.py) call this hook once set. */
typedef void (*gta_dxvk_emit_fn)(uint32_t, uint64_t, uint32_t, uint64_t, uint64_t);
gta_dxvk_emit_fn gta_dxvk_emit_hook;

static struct {
  GtaDxvk *bridge;
  GtaDxvkEmit emit;
  GtaDxvkSurface surface;
  GtaDxvkCheck check;
  GtaDxvkLastError last_error;
  GtaDxvkProgress progress;
  GtaDxvkDestroy destroy;
} gta_dxvk;

static void gta_dxvk_emit_command(uint32_t op, uint64_t words, uint32_t count, uint64_t data, uint64_t size) {
  gta_dxvk.emit(gta_dxvk.bridge, op, words, count, data, size);
}

/* These two functions are generated in game_0.c but intentionally omitted
 * from game.h by wasm2c. */
extern void wasm2c_gta5__native_instantiate(
    w2c_gta5__native *, struct w2c_env *,
    struct w2c_wasi__snapshot__preview1 *);
extern void wasm2c_gta5__native_free(w2c_gta5__native *);

_Thread_local GtaNativeJumpFrame *gta_native_current_jump = NULL;
/* DedicatedVideoMemory the engine's null D3D adapter reports (patch_video_memory.py);
 * the engine sizes texture streaming to it. `setprop debug.gta5.vram_mb <MiB>`. */
uint64_t gta_native_video_memory_bytes = 4096ull << 20;
/* The engine's texture streaming cache (patch_streaming_budget.py). It is shared RAM on the
 * phone: 1536 MiB lets the five-pass benchmark finish without a low-memory kill and shows the
 * same texture detail as 3072. `setprop debug.gta5.stream_mb <MiB>`. */
uint64_t gta_native_streaming_memory_bytes = 1536ull << 20;
/* Set before engine workers start; only references already queued for Release drain. */
uint32_t gta_native_eager_resource_release = 0;

typedef uint64_t (*GtaResidencyBudget)(uint64_t);
static GtaResidencyBudget gta_native_residency_budget;
static pthread_once_t gta_native_residency_once = PTHREAD_ONCE_INIT;
static void gta_native_find_residency(void) {
  gta_native_residency_budget = (GtaResidencyBudget)dlsym(RTLD_DEFAULT, "gta_arch2_residency_budget");
}

/* Generated engine reads call here; the renderer owns the optional coordinator's lifetime. */
uint64_t gta_native_streaming_budget(void) {
  uint64_t configured = __atomic_load_n(&gta_native_streaming_memory_bytes, __ATOMIC_RELAXED);
  (void)pthread_once(&gta_native_residency_once, gta_native_find_residency);
  uint64_t shared = gta_native_residency_budget != NULL ? gta_native_residency_budget(configured) : 0;
  return shared != 0 ? shared : configured;
}

/* Get normally skips UpdateTotalAvailableMemory after its first nonzero result.
 * Called under the engine cache lock; an atomic handles other native callers safely.
 * No guest pointer or allocation backing crosses this boundary. */
uint32_t gta_native_streaming_budget_changed(void) {
  static uint64_t last_seen = UINT64_MAX;
  uint64_t desired = gta_native_streaming_budget();
  return __atomic_exchange_n(&last_seen, desired, __ATOMIC_RELAXED) != desired;
}

/* Memory governor (architecture stage 4, `debug.gta5.mem_governor=1`): the patched engine cache
 * refreshes its cached streaming budget when the native policy changes, so lowering it
 * while Android runs short of memory makes the engine release GPU textures before the low-memory killer
 * acts, and raising it again restores detail once memory recovers. Signals: memory pressure (PSI some
 * avg10, which reached 30-50% before today's kills) and MemAvailable. */
static bool gta_native_read_memory(long *available_mb, double *pressure) {
  char line[256];
  bool found = false;
  FILE *meminfo = fopen("/proc/meminfo", "r");
  if (meminfo != NULL) {
    while (fgets(line, sizeof(line), meminfo) != NULL) {
      long kb;
      if (sscanf(line, "MemAvailable: %ld kB", &kb) == 1) { *available_mb = kb / 1024; found = true; break; }
    }
    fclose(meminfo);
  }
  FILE *psi = fopen("/proc/pressure/memory", "r");
  if (psi != NULL) {
    if (fgets(line, sizeof(line), psi) == NULL || sscanf(line, "some avg10=%lf", pressure) != 1) *pressure = 0.0;
    fclose(psi);
  }
  return found;
}

static void *gta_native_memory_governor(void *opaque) {
  (void)opaque;
  (void)pthread_setname_np(pthread_self(), "gta-mem-governor");
  const uint64_t ceiling = gta_native_streaming_memory_bytes, floor_bytes = 768ull << 20;
  const uint64_t step_down = 256ull << 20, step_up = 128ull << 20;
  uint64_t budget = ceiling;
  int calm_seconds = 0, since_change = 0;
  for (;;) {
    sleep(1);
    long available = 0;
    double pressure = 0.0;
    if (!gta_native_read_memory(&available, &pressure)) continue;
    since_change++;
    bool short_of_memory = pressure > 20.0 || available < 500;
    calm_seconds = (pressure < 8.0 && available > 1000) ? calm_seconds + 1 : 0;
    uint64_t next = budget;
    if (short_of_memory && since_change >= 2 && budget > floor_bytes)
      next = budget - step_down < floor_bytes ? floor_bytes : budget - step_down;
    else if (calm_seconds >= 15 && budget < ceiling)
      next = budget + step_up > ceiling ? ceiling : budget + step_up;
    if (next != budget) {
      budget = next;
      since_change = 0;
      calm_seconds = 0;
      __atomic_store_n(&gta_native_streaming_memory_bytes, budget, __ATOMIC_RELAXED);
      __android_log_print(ANDROID_LOG_INFO, "GtaDirectHost", "memory governor: streaming budget %llu MiB (available %ld MiB, pressure %.1f%%)",
                          (unsigned long long)(budget >> 20), available, pressure);
    }
  }
  return NULL;
}
static _Thread_local GtaNativeThread *gta_native_current_thread = NULL;
static pthread_once_t gta_native_runtime_once = PTHREAD_ONCE_INIT;

static void *gta_native_thread_entry(void *opaque);
static void gta_native_reap_finished(GtaNativeSession *session,
                                     GtaNativeThread *caller);
static void gta_native_pump_proxies(GtaNativeThread *owner);
static int gta_native_spawn(pthread_t *native, GtaNativeThread *thread);

typedef struct GtaNativeProxyValue {
  bool is_float;
  uint64_t bits;
} GtaNativeProxyValue;

typedef struct GtaNativeProxyRequest {
  struct GtaNativeProxyRequest *next;
  uint64_t function_index;
  uint64_t asm_code;
  size_t argument_count;
  uint64_t context;
  uint64_t context_args;
  bool asynchronous;
  bool complete;
  bool canceled;
  double result;
  pthread_cond_t completion;
  GtaNativeProxyValue arguments[GTA_NATIVE_MAX_PROXY_ARGS];
} GtaNativeProxyRequest;

struct GtaNativeThread {
  GtaNativeSession *session;
  size_t slot;
  pthread_t native;
  bool native_started;
  bool joined;
  bool instance_initialized;
  bool crash_reporting;
  bool cleanup_requested;
  bool reaping;
  bool mailbox_dispatching;
  _Atomic bool done;
  _Atomic bool failed;
  _Atomic bool mailbox_pending;
  uint64_t pthread_ptr;
  uint64_t start_routine;
  uint64_t argument;
  int exit_code;
  uint64_t stack_high;
  uint64_t stack_low;
  jmp_buf runtime_unwind;
  bool runtime_unwind_ready;
  w2c_gta5__native *instance;
  struct w2c_env env;
  struct w2c_wasi__snapshot__preview1 wasi;
};

struct GtaNativeSession {
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  pthread_mutex_t proxy_mutex;
  _Atomic bool stop_requested;
  GtaNativeSessionState state;
  int exit_code;
  char error[GTA_NATIVE_MAX_ERROR];
  char *assets_root;
  char *save_root;
  char *shader_root;
  void *window;
  uint32_t width, height;
  bool suspended;
  wasm_rt_shared_memory_t memory;
  GtaNativeServices *services;
  GtaNativeGpu *gpu;
  _Atomic(GtaMovieManager *) movies;
  _Atomic bool movies_ready;
  /* The touch overlay's virtual gamepad (input kind 10), applied to rage::ioPad 0 each frame:
   * button bits in ioPad order, sticks LX|LY<<8|RX<<16|RY<<24 (0x80 = centre), triggers LT|RT<<8. */
  _Atomic uint32_t pad_buttons, pad_sticks, pad_triggers;
  GtaNativeThread *threads[GTA_NATIVE_MAX_THREADS];
  size_t active_threads;
  /* Raised after a thread finishes or is asked to be cleaned up: every import
   * call checks it, so the thread table is scanned only when one may be done. */
  _Atomic bool reap_hint;
  GtaNativeThread *main_thread;
  GtaNativeProxyRequest *proxy_head;
  GtaNativeProxyRequest *proxy_tail;
  GtaNativeProxyRequest *proxy_executing;
  size_t proxy_count;
  bool proxy_dispatching;
};

static void gta_native_runtime_init(void) { wasm_rt_init(); }

static char *gta_native_strdup(const char *value) {
  if (value == NULL) return NULL;
  size_t length = strlen(value);
  if (length == SIZE_MAX) return NULL;
  char *copy = (char *)malloc(length + 1u);
  if (copy != NULL) memcpy(copy, value, length + 1u);
  return copy;
}

static uint8_t *gta_native_memory_base(const wasm_rt_shared_memory_t *memory) {
  return (uint8_t *)(uintptr_t)memory->data;
}

static uint64_t gta_native_memory_size(const wasm_rt_shared_memory_t *memory) {
  return atomic_load_explicit(&memory->size, memory_order_acquire);
}

/* Called by rage::sysMemBuddyAllocator::Free (patch_release_freed.py) while it still holds the heap lock, and by
 * dlmalloc's free for chunks over 64 KiB (patch_release_malloc.py): the freed pages go back to Android instead of
 * staying dirty in RAM or zram (a browser heap never shrinks). The buddy heap keeps its bookkeeping in a separate
 * node table and the renderer copies resource data at emit time, so nothing reads a freed block; its next owner gets
 * zero pages. Measured: anonymous RAM plus swap -478 MiB (buddy heaps) and -20 MiB (dlmalloc) over the 3-iteration
 * memory benchmark. */
void gta_native_release_freed(wasm_rt_shared_memory_t *memory, uint64_t address, uint64_t size) {
  if (size < 65536u || address > gta_native_memory_size(memory) || size > gta_native_memory_size(memory) - address) return;
  const uintptr_t page = (uintptr_t)getpagesize();
  uintptr_t start = ((uintptr_t)gta_native_memory_base(memory) + (uintptr_t)address + page - 1u) & ~(page - 1u);
  uintptr_t end = ((uintptr_t)gta_native_memory_base(memory) + (uintptr_t)(address + size)) & ~(page - 1u);
  if (end > start) (void)madvise((void *)start, end - start, MADV_DONTNEED);
}

static bool gta_native_guest_range(const GtaNativeSession *session,
                                   uint64_t address, size_t length,
                                   size_t alignment) {
  const wasm_rt_shared_memory_t *memory = &session->memory;
  uint64_t size = gta_native_memory_size(memory);
  if (address > size || (uint64_t)length > size - address) return false;
  if (alignment != 0u && address % alignment != 0u) return false;
  uintptr_t absolute = (uintptr_t)gta_native_memory_base(memory) +
                       (uintptr_t)address;
  return absolute >= (uintptr_t)gta_native_memory_base(memory) &&
         (alignment == 0u || absolute % alignment == 0u);
}

static bool gta_native_guest_read(const GtaNativeSession *session,
                                  uint64_t address, void *output, size_t length,
                                  size_t alignment) {
  if (!gta_native_guest_range(session, address, length, alignment)) return false;
  memcpy(output, gta_native_memory_base(&session->memory) + address, length);
  return true;
}

static bool gta_native_guest_write(const GtaNativeSession *session,
                                   uint64_t address, const void *input,
                                   size_t length, size_t alignment) {
  if (!gta_native_guest_range(session, address, length, alignment)) return false;
  memcpy(gta_native_memory_base(&session->memory) + address, input, length);
  return true;
}

static bool gta_native_guest_store_u32(const GtaNativeSession *session,
                                       uint64_t address, uint32_t value) {
  if (!gta_native_guest_range(session, address, sizeof(value),
                              _Alignof(uint32_t))) return false;
  uint32_t *pointer = (uint32_t *)(void *)(gta_native_memory_base(&session->memory) +
                                           address);
  __atomic_store_n(pointer, value, __ATOMIC_SEQ_CST);
  return true;
}

static bool gta_native_guest_load_u64(const GtaNativeSession *session,
                                      uint64_t address, uint64_t *value) {
  if (!gta_native_guest_range(session, address, sizeof(*value),
                              _Alignof(uint64_t))) return false;
  uint64_t *pointer = (uint64_t *)(void *)(gta_native_memory_base(&session->memory) +
                                           address);
  *value = __atomic_load_n(pointer, __ATOMIC_SEQ_CST);
  return true;
}

static bool gta_native_guest_store_u64(const GtaNativeSession *session,
                                       uint64_t address, uint64_t value) {
  if (!gta_native_guest_range(session, address, sizeof(value),
                              _Alignof(uint64_t))) return false;
  uint64_t *pointer = (uint64_t *)(void *)(gta_native_memory_base(&session->memory) +
                                           address);
  __atomic_store_n(pointer, value, __ATOMIC_SEQ_CST);
  return true;
}

static void gta_native_set_error_locked(GtaNativeSession *session,
                                        const char *message) {
  if (message == NULL) message = "native host failure";
  (void)snprintf(session->error, sizeof(session->error), "%s", message);
}

static void gta_native_log_first_failure(const char *message) {
#ifdef __ANDROID__
  (void)__android_log_print(ANDROID_LOG_ERROR, "GtaDirectHost",
                            "first native failure: %s",
                            message == NULL ? "native host failure" : message);
#else
  (void)fprintf(stderr, "GtaDirectHost: first native failure: %s\n",
                message == NULL ? "native host failure" : message);
#endif
}

static void gta_native_mark_failed(GtaNativeThread *thread,
                                   const char *message) {
  if (thread == NULL || thread->session == NULL) return;
  GtaNativeSession *session = thread->session;
  atomic_store_explicit(&thread->failed, true, memory_order_release);
  atomic_store_explicit(&session->stop_requested, true, memory_order_release);
  (void)pthread_mutex_lock(&session->mutex);
  /* A real failure requests shutdown; keep the first diagnostic because it
   * identifies the cause. */
  if (session->state != GTA_NATIVE_SESSION_FAILED &&
      session->state != GTA_NATIVE_SESSION_RESTARTING) {
    gta_native_set_error_locked(session, message);
    session->state = GTA_NATIVE_SESSION_FAILED;
    gta_native_log_first_failure(session->error);
  }
  (void)pthread_cond_broadcast(&session->condition);
  (void)pthread_mutex_unlock(&session->mutex);
}

_Noreturn void gta_native_fail(GtaNativeThread *thread, const char *message) {
  if (thread == NULL) {
    fputs("gta_native_fail without a thread\n", stderr);
    abort();
  }
  gta_native_mark_failed(thread, message);
  /* The boot and worker entrypoints run on owned pthreads.  Exiting the
   * current worker keeps a guest error local while the session reports it to
   * JNI.  It also avoids unwinding C frames through generated imports. */
  pthread_exit(NULL);
  abort();
}

struct gta_native_trap_frames { uintptr_t pc[64]; size_t count; };

static _Unwind_Reason_Code gta_native_trap_frame(struct _Unwind_Context *context, void *opaque) {
  struct gta_native_trap_frames *frames = opaque;
  uintptr_t pc = _Unwind_GetIP(context);
  if (pc != 0 && frames->count < 64u) frames->pc[frames->count++] = pc;
  return frames->count == 64u ? _URC_END_OF_STACK : _URC_NO_REASON;
}

static void gta_native_log_error(const char *format, ...) {
  va_list args;
  va_start(args, format);
#ifdef __ANDROID__
  (void)__android_log_vprint(ANDROID_LOG_ERROR, "GtaDirectHost", format, args);
#else
  (void)fputs("GtaDirectHost: ", stderr);
  (void)vfprintf(stderr, format, args);
  (void)fputc('\n', stderr);
#endif
  va_end(args);
}

/* Logs the trapping thread's native frames as library offsets; native/trap_symbols.py names them. */
static void gta_native_log_trap_frames(const char *reason) {
  struct gta_native_trap_frames frames = {.count = 0};
  (void)_Unwind_Backtrace(gta_native_trap_frame, &frames);
  for (size_t i = 0; i < frames.count; ++i) {
    Dl_info info;
    if (dladdr((void *)frames.pc[i], &info) != 0 && info.dli_fbase != NULL) {
      const char *slash = info.dli_fname != NULL ? strrchr(info.dli_fname, '/') : NULL;
      gta_native_log_error("trap frame %zu: %s+0x%lx (%s)", i,
                           slash != NULL ? slash + 1 : "?", (unsigned long)(frames.pc[i] - (uintptr_t)info.dli_fbase), reason);
    } else {
      gta_native_log_error("trap frame %zu: 0x%lx (%s)", i, (unsigned long)frames.pc[i], reason);
    }
  }
}

_Noreturn void gta_native_trap(wasm_rt_trap_t code) {
  gta_native_log_trap_frames(wasm_rt_strerror(code));
  char message[256];
  (void)snprintf(message, sizeof(message), "direct engine trap: %s",
                 wasm_rt_strerror(code));
  gta_native_fail(gta_native_current_thread, message);
}

/* patch_alloc_guards.py: the null D3D11 layer would copy into a failed allocation from guest address 0. */
_Noreturn void gta_native_alloc_failed(uint32_t function, uint64_t count, uint64_t size) {
  gta_native_log_trap_frames("null D3D11 allocation failed");
  char message[256];
  (void)snprintf(message, sizeof(message), "null D3D11 allocation failed in f%u: %llu x %llu bytes",
                 function, (unsigned long long)count, (unsigned long long)size);
  gta_native_fail(gta_native_current_thread, message);
}

GtaNativeThread *gta_native_thread(struct w2c_env *context) {
  return context == NULL ? NULL : context->thread;
}

wasm_rt_shared_memory_t *w2c_env_memory(struct w2c_env *context) {
  GtaNativeThread *thread = gta_native_thread(context);
  if (thread == NULL || thread->session == NULL) {
    fputs("w2c_env_memory without a native thread\n", stderr);
    abort();
  }
  return &thread->session->memory;
}

w2c_gta5__native *gta_native_instance(GtaNativeThread *thread) {
  if (thread == NULL || thread->instance == NULL) {
    gta_native_fail(thread, "generated instance is not initialized");
  }
  return thread->instance;
}

static void gta_native_notify_waiters(GtaNativeSession *session) {
  uint8_t *base = gta_native_memory_base(&session->memory);
  (void)pthread_mutex_lock(&session->mutex);
  for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
    GtaNativeThread *thread = session->threads[i];
    if (thread == NULL || thread->pthread_ptr == 0u) continue;
    if (!gta_native_guest_range(session, thread->pthread_ptr, sizeof(uint32_t),
                                _Alignof(uint32_t))) continue;
    (void)gta_native_notify(base, base + thread->pthread_ptr, UINT32_MAX);
    atomic_store_explicit(&thread->mailbox_pending, true, memory_order_release);
  }
  (void)pthread_mutex_unlock(&session->mutex);
}

static GtaNativeThread *gta_native_find_thread_locked(
    GtaNativeSession *session, uint64_t pthread_ptr) {
  if (pthread_ptr == 0u) return NULL;
  for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
    GtaNativeThread *thread = session->threads[i];
    if (thread != NULL && thread->pthread_ptr == pthread_ptr) return thread;
  }
  return NULL;
}

static void gta_native_arm_mailbox(GtaNativeThread *thread, uint64_t pointer) {
  uint64_t waiting_address;
  if (pointer == 0u ||
      !gta_native_guest_range(thread->session, pointer,
                              GTA_NATIVE_THREAD_RECORD_BYTES,
                              _Alignof(uint32_t)) ||
      __builtin_add_overflow(pointer, GTA_NATIVE_THREAD_MAILBOX_OFFSET,
                             &waiting_address) ||
      !gta_native_guest_store_u32(thread->session, waiting_address, 1u)) {
    gta_native_fail(thread, "mailbox pthread record is outside guest memory");
  }
  thread->pthread_ptr = pointer;
}

static void gta_native_proxy_request_destroy(GtaNativeProxyRequest *request) {
  if (request == NULL) return;
  (void)pthread_cond_destroy(&request->completion);
  free(request);
}

/* Wake synchronous callers and discard asynchronous work after the main
 * owner or session stops.  Requests owned by their waiting caller are left
 * for that caller to free. */
static void gta_native_proxy_abort_all(GtaNativeSession *session) {
  (void)pthread_mutex_lock(&session->proxy_mutex);
  GtaNativeProxyRequest *request = session->proxy_head;
  session->proxy_head = NULL;
  session->proxy_tail = NULL;
  session->proxy_count = 0u;
  while (request != NULL) {
    GtaNativeProxyRequest *next = request->next;
    request->next = NULL;
    request->canceled = true;
    request->complete = true;
    (void)pthread_cond_broadcast(&request->completion);
    if (request->asynchronous) gta_native_proxy_request_destroy(request);
    request = next;
  }
  if (session->proxy_executing != NULL) {
    request = session->proxy_executing;
    request->canceled = true;
    request->complete = true;
    if (gta_native_current_thread == session->main_thread) {
      session->proxy_executing = NULL;
      if (request->asynchronous) gta_native_proxy_request_destroy(request);
    }
    (void)pthread_cond_broadcast(&request->completion);
  }
  (void)pthread_mutex_unlock(&session->proxy_mutex);
}

static void gta_native_pump_mailbox(GtaNativeThread *thread) {
  if (thread == NULL || thread->instance == NULL ||
      thread->mailbox_dispatching) return;
  bool pending = atomic_exchange_explicit(&thread->mailbox_pending, false,
                                          memory_order_acq_rel);
  if (!pending) return;
  thread->mailbox_dispatching = true;
  gta_native_arm_mailbox(thread, thread->pthread_ptr);
  w2c_gta5__native_0x5Femscripten_check_mailbox(thread->instance);
  thread->mailbox_dispatching = false;
}

static uint64_t gta_native_call_service(GtaNativeThread *thread,
                                        const char *name,
                                        const uint64_t *args, size_t argc) {
  GtaNativeSession *session = thread->session;
  uint64_t result = 0;
  if (session->services == NULL ||
      gta_native_services_call(session->services, name, args, argc, &result) !=
          0) {
    char message[GTA_NATIVE_MAX_ERROR];
    message[0] = '\0';
    size_t required = session->services == NULL
                          ? 0u
                          : gta_native_services_last_error(
                                session->services, message, sizeof(message));
    if (message[0] == '\0') {
      (void)snprintf(message, sizeof(message),
                     "native service failed: %s (%zu bytes)", name, required);
    }
    gta_native_fail(thread, message);
  }
  return result;
}

static int gta_native_resize_heap(GtaNativeThread *thread, uint64_t requested) {
  GtaNativeSession *session = thread->session;
  wasm_rt_shared_memory_t *memory = &session->memory;
  uint64_t old_size = gta_native_memory_size(memory);
  uint64_t max_size;
  if (__builtin_mul_overflow(memory->max_pages, (uint64_t)memory->page_size,
                             &max_size)) {
    gta_native_fail(thread, "linear-memory maximum overflows host size");
  }
  if (requested <= old_size) return 1;
  if (requested > max_size) return 0;
  uint64_t target = requested;
  uint64_t growth = old_size + old_size / 5u;
  if (growth > target) target = growth;
  uint64_t cap_increment = 96ull * 1024ull * 1024ull;
  uint64_t capped = requested > UINT64_MAX - cap_increment
                        ? UINT64_MAX
                        : requested + cap_increment;
  if (capped < target) target = capped;
  uint64_t page = memory->page_size;
  if (target > UINT64_MAX - page + 1u) return 0;
  target = (target + page - 1u) / page * page;
  if (target > max_size) target = max_size;
  uint64_t old_pages = atomic_load_explicit(&memory->pages, memory_order_acquire);
  uint64_t new_pages = target / page;
  if (new_pages <= old_pages) return 1;
  uint64_t previous = wasm_rt_grow_memory_shared(memory, new_pages - old_pages);
  return previous == old_pages ? 1 : 0;
}

static bool gta_native_stack_plan(GtaNativeThread *thread, uint64_t pointer,
                                  uint64_t *high, uint64_t *low) {
  uint64_t high_address;
  uint64_t stack_high;
  uint64_t stack_size;
  if (__builtin_add_overflow(pointer, GTA_NATIVE_THREAD_STACK_HIGH_OFFSET,
                             &high_address) ||
      !gta_native_guest_load_u64(thread->session, high_address, &stack_high)) {
    return false;
  }
  uint64_t size_address;
  if (__builtin_add_overflow(pointer, GTA_NATIVE_THREAD_STACK_SIZE_OFFSET,
                             &size_address) ||
      !gta_native_guest_load_u64(thread->session, size_address, &stack_size)) {
    return false;
  }
  if (stack_size > stack_high || stack_high > gta_native_memory_size(&thread->session->memory)) {
    return false;
  }
  *high = stack_high;
  *low = stack_high - stack_size;
  return true;
}

static uint64_t gta_native_stack_alloc_checked(GtaNativeThread *thread,
                                               uint64_t bytes,
                                               size_t alignment,
                                               const char *label) {
  if (thread == NULL || thread->instance == NULL || bytes == 0u ||
      bytes > (uint64_t)SIZE_MAX) {
    gta_native_fail(thread, label == NULL ? "guest stack allocation is invalid"
                                         : label);
  }
  uint64_t address = w2c_gta5__native_0x5Femscripten_stack_alloc(
      thread->instance, bytes);
  if (address == 0u ||
      !gta_native_guest_range(thread->session, address, (size_t)bytes,
                              alignment)) {
    gta_native_fail(thread, label == NULL ? "guest stack allocation is outside memory"
                                         : label);
  }
  return address;
}

static bool gta_native_integer_value(const GtaNativeProxyValue *value,
                                     uint64_t *output) {
  if (!value->is_float) {
    *output = value->bits;
    return true;
  }
  double number;
  memcpy(&number, &value->bits, sizeof(number));
  if (!isfinite(number) || trunc(number) != number ||
      number < -9007199254740992.0 || number > 9007199254740992.0) {
    return false;
  }
  *output = (uint64_t)(int64_t)number;
  return true;
}

static int32_t gta_native_asm_constant(GtaNativeThread *thread, uint64_t code,
                                       const GtaNativeProxyValue *args,
                                       size_t count) {
  if (code == 7980558ull) {
    if (count != 2u) gta_native_fail(thread, "locale ASM constant argument count");
    uint64_t address;
    uint64_t capacity;
    if (!gta_native_integer_value(&args[0], &address) ||
        !gta_native_integer_value(&args[1], &capacity)) {
      gta_native_fail(thread, "locale ASM constant needs integer arguments");
    }
    if ((int64_t)capacity < 0) gta_native_fail(thread, "negative locale capacity");
    if (capacity != 0u) {
      static const uint8_t locale[] = "en-US";
      size_t length = sizeof(locale) - 1u;
      if (capacity <= length) length = (size_t)capacity - 1u;
      uint64_t terminator;
      if (__builtin_add_overflow(address, (uint64_t)length, &terminator) ||
          !gta_native_guest_write(thread->session, address, locale, length, 1u) ||
          !gta_native_guest_write(thread->session, terminator, "\0", 1u, 1u)) {
        gta_native_fail(thread, "locale ASM output is outside guest memory");
      }
    }
    return 0;
  }
  if (code == 7980673ull) {
    /* The source contract returns zero when a URL cannot be opened. */
    fputs("native host: external URL opening unavailable\n", stderr);
    return 0;
  }
  if (code == 7980768ull) {
    GtaNativeSession *session = thread->session;
    (void)pthread_mutex_lock(&session->mutex);
    if (session->state != GTA_NATIVE_SESSION_FAILED)
      session->state = GTA_NATIVE_SESSION_RESTARTING;
    atomic_store_explicit(&session->stop_requested, true, memory_order_release);
    (void)pthread_cond_broadcast(&session->condition);
    (void)pthread_mutex_unlock(&session->mutex);
    pthread_exit(NULL);
  }
  gta_native_fail(thread, "unknown engine ASM constant");
}

static int32_t gta_native_asm_from_memory(GtaNativeThread *thread,
                                          uint64_t code, uint64_t signature,
                                          uint64_t cursor) {
  GtaNativeProxyValue args[GTA_NATIVE_MAX_PROXY_SIGNATURE];
  size_t count = 0;
  for (;;) {
    if (count >= GTA_NATIVE_MAX_PROXY_SIGNATURE) {
      gta_native_fail(thread, "ASM signature exceeds native argument limit");
    }
    uint8_t kind;
    if (!gta_native_guest_read(thread->session, signature, &kind, 1u, 1u)) {
      gta_native_fail(thread, "ASM signature is outside guest memory");
    }
    if (__builtin_add_overflow(signature, 1ull, &signature)) {
      gta_native_fail(thread, "ASM signature address overflows");
    }
    if (kind == 0u) return gta_native_asm_constant(thread, code, args, count);
    if (kind != 'i') {
      uint64_t aligned;
      if (__builtin_add_overflow(cursor, 7ull, &aligned)) {
        gta_native_fail(thread, "ASM argument address overflows");
      }
      cursor = aligned & ~7ull;
    }
    args[count].is_float = false;
    switch (kind) {
      case 'i': {
        uint32_t value;
        if (!gta_native_guest_read(thread->session, cursor, &value, 4u, 4u)) {
          gta_native_fail(thread, "ASM integer argument is outside guest memory");
        }
        args[count].bits = (uint64_t)(int64_t)(int32_t)value;
        if (__builtin_add_overflow(cursor, 4ull, &cursor)) {
          gta_native_fail(thread, "ASM integer address overflows");
        }
        break;
      }
      case 'p':
      case 'j':
        if (!gta_native_guest_load_u64(thread->session, cursor,
                                       &args[count].bits)) {
          gta_native_fail(thread, "ASM pointer argument is outside guest memory");
        }
        if (__builtin_add_overflow(cursor, 8ull, &cursor)) {
          gta_native_fail(thread, "ASM pointer address overflows");
        }
        break;
      case 'd':
      case 'f':
        args[count].is_float = true;
        if (!gta_native_guest_load_u64(thread->session, cursor,
                                       &args[count].bits)) {
          gta_native_fail(thread, "ASM floating argument is outside guest memory");
        }
        if (__builtin_add_overflow(cursor, 8ull, &cursor)) {
          gta_native_fail(thread, "ASM floating address overflows");
        }
        break;
      default:
        gta_native_fail(thread, "unknown engine ASM argument signature");
    }
    ++count;
  }
}

static const char *const gta_native_proxy_functions[] = {
    "wasi_snapshot_preview1.proc_exit", "env.exit",
    "env.__pthread_create_js",          "env.__syscall_accept4",
    "env.__syscall_bind",               "env.__syscall_connect",
    "env.__syscall_fcntl64",            "env.__syscall_fstat64",
    "env.__syscall_ftruncate64",        "env.__syscall_getcwd",
    "env.__syscall_getdents64",         "env.__syscall_getsockname",
    "env.__syscall_getsockopt",         "env.__syscall_ioctl",
    "env.__syscall_listen",             "env.__syscall_lstat64",
    "env.__syscall_mkdirat",            "env.__syscall_newfstatat",
    "env.__syscall_openat",             "env.__syscall_poll",
    "env.__syscall_recvfrom",           "env.__syscall_renameat",
    "env.__syscall_rmdir",              "env.__syscall_sendto",
    "env.__syscall_setsockopt",         "env.__syscall_shutdown",
    "env.__syscall_socket",             "env.__syscall_stat64",
    "env.__syscall_statfs64",           "env.__syscall_unlinkat",
    "wasi_snapshot_preview1.environ_get",
    "wasi_snapshot_preview1.environ_sizes_get",
    "wasi_snapshot_preview1.fd_close", "wasi_snapshot_preview1.fd_pread",
    "wasi_snapshot_preview1.fd_pwrite", "wasi_snapshot_preview1.fd_read",
    "wasi_snapshot_preview1.fd_seek",  "wasi_snapshot_preview1.fd_write",
};

static void gta_native_proxy_execute(GtaNativeThread *owner,
                                     GtaNativeProxyRequest *request) {
  double result = 0.0;
  if (request->asm_code != 0u) {
    result = (double)gta_native_asm_constant(owner, request->asm_code,
                                              request->arguments,
                                              request->argument_count);
  } else {
    if (request->function_index >= sizeof(gta_native_proxy_functions) /
                                       sizeof(gta_native_proxy_functions[0])) {
      gta_native_fail(owner, "unknown main-thread proxy function");
    }
    uint64_t call_args[GTA_NATIVE_MAX_PROXY_ARGS];
    for (size_t i = 0; i < request->argument_count; ++i) {
      if (!gta_native_integer_value(&request->arguments[i], &call_args[i])) {
        gta_native_fail(owner, "proxy integer is not safely representable");
      }
    }
    const char *name = gta_native_proxy_functions[request->function_index];
    uint64_t returned = gta_native_dispatch(owner, name, call_args,
                                            request->argument_count);
    result = (strstr(name, "proc_exit") != NULL || strcmp(name, "env.exit") == 0)
                 ? 0.0
                 : (double)(int64_t)returned;
  }
  request->result = result;
  if (request->context != 0u) {
    w2c_gta5__native_0x5Femscripten_run_js_on_main_thread_done(
        owner->instance, request->context, request->context_args, result);
  }
}

static void gta_native_pump_proxies(GtaNativeThread *owner) {
  GtaNativeSession *session = owner->session;
  if (owner != session->main_thread || owner->instance == NULL ||
      session->proxy_dispatching) return;
  session->proxy_dispatching = true;
  for (;;) {
    (void)pthread_mutex_lock(&session->proxy_mutex);
    GtaNativeProxyRequest *request = session->proxy_head;
    if (request != NULL) {
      session->proxy_head = request->next;
      if (session->proxy_head == NULL) session->proxy_tail = NULL;
      request->next = NULL;
      --session->proxy_count;
      session->proxy_executing = request;
    }
    (void)pthread_mutex_unlock(&session->proxy_mutex);
    if (request == NULL) {
      session->proxy_dispatching = false;
      return;
    }

    gta_native_proxy_execute(owner, request);

    (void)pthread_mutex_lock(&session->proxy_mutex);
    if (session->proxy_executing == request) session->proxy_executing = NULL;
    request->complete = true;
    (void)pthread_cond_broadcast(&request->completion);
    bool asynchronous = request->asynchronous;
    (void)pthread_mutex_unlock(&session->proxy_mutex);
    if (asynchronous) gta_native_proxy_request_destroy(request);
  }
}

static double gta_native_proxy_receive(GtaNativeThread *thread,
                                       const uint64_t *values, size_t count) {
  if (count < 7u) gta_native_fail(thread, "main-thread proxy needs seven arguments");
  uint64_t function_index = values[0];
  uint64_t asm_code = values[1];
  uint64_t buffer_size = values[3];
  uint64_t arguments_address = values[4];
  uint64_t context = values[5];
  uint64_t context_args = values[6];
  if (buffer_size % 16u != 0u || buffer_size / 16u > GTA_NATIVE_MAX_PROXY_ARGS) {
    gta_native_fail(thread, "proxy argument buffer is not bounded 16-byte tuples");
  }
  size_t argument_count = (size_t)(buffer_size / 16u);
  GtaNativeProxyRequest *request =
      (GtaNativeProxyRequest *)calloc(1u, sizeof(*request));
  if (request == NULL || pthread_cond_init(&request->completion, NULL) != 0) {
    free(request);
    gta_native_fail(thread, "main-thread proxy allocation failed");
  }
  request->function_index = function_index;
  request->asm_code = asm_code;
  request->argument_count = argument_count;
  request->context = context;
  request->context_args = context_args;
  request->asynchronous = context != 0u;
  for (size_t i = 0; i < argument_count; ++i) {
    uint64_t base;
    if (__builtin_mul_overflow((uint64_t)i, 16ull, &base) ||
        __builtin_add_overflow(arguments_address, base, &base)) {
      gta_native_fail(thread, "proxy argument address overflows");
    }
    uint64_t kind;
    uint64_t value_address;
    if (__builtin_add_overflow(base, 8ull, &value_address) ||
        !gta_native_guest_load_u64(thread->session, base, &kind) ||
        !gta_native_guest_load_u64(thread->session, value_address,
                                   &request->arguments[i].bits)) {
      gta_native_proxy_request_destroy(request);
      gta_native_fail(thread, "proxy argument tuple is outside guest memory");
    }
    request->arguments[i].is_float = kind == 0u;
  }
  GtaNativeSession *session = thread->session;
  if (thread == session->main_thread && context == 0u) {
    gta_native_proxy_execute(thread, request);
    double result = request->result;
    gta_native_proxy_request_destroy(request);
    return result;
  }
  (void)pthread_mutex_lock(&session->proxy_mutex);
  if (session->proxy_count >= GTA_NATIVE_MAX_PROXY_QUEUE ||
      atomic_load_explicit(&session->stop_requested, memory_order_acquire)) {
    (void)pthread_mutex_unlock(&session->proxy_mutex);
    gta_native_proxy_request_destroy(request);
    gta_native_fail(thread, "main-thread proxy queue is unavailable");
  }
  if (session->proxy_tail != NULL) session->proxy_tail->next = request;
  else session->proxy_head = request;
  session->proxy_tail = request;
  ++session->proxy_count;
  if (request->asynchronous) {
    (void)pthread_mutex_unlock(&session->proxy_mutex);
    return 0.0;
  }
  while (!request->complete || session->proxy_executing == request) {
    (void)pthread_cond_wait(&request->completion, &session->proxy_mutex);
  }
  bool canceled = request->canceled || !request->complete ||
                  session->proxy_executing == request;
  double result = request->result;
  (void)pthread_mutex_unlock(&session->proxy_mutex);
  gta_native_proxy_request_destroy(request);
  if (canceled) gta_native_fail(thread, "main-thread proxy was canceled");
  return result;
}

static bool gta_native_notify_thread(GtaNativeThread *thread,
                                     uint64_t target_pointer) {
  GtaNativeSession *session = thread->session;
  (void)pthread_mutex_lock(&session->mutex);
  GtaNativeThread *target = gta_native_find_thread_locked(session, target_pointer);
  if (target != NULL) {
    atomic_store_explicit(&target->mailbox_pending, true, memory_order_release);
  }
  (void)pthread_mutex_unlock(&session->mutex);
  /* Browser glue drops posts to an already removed worker. */
  if (target == NULL) return true;
  if (!gta_native_guest_range(session, target_pointer, sizeof(uint32_t),
                              _Alignof(uint32_t))) {
    return false;
  }
  uint8_t *base = gta_native_memory_base(&session->memory);
  (void)gta_native_notify(base, base + target_pointer, UINT32_MAX);
  return true;
}

static void gta_native_reap_finished(GtaNativeSession *session,
                                     GtaNativeThread *caller) {
  /* Load first: an exchange on every import would bounce this line between threads. */
  if (!atomic_load_explicit(&session->reap_hint, memory_order_relaxed) ||
      !atomic_exchange_explicit(&session->reap_hint, false, memory_order_acquire)) return;
  for (;;) {
    GtaNativeThread *target = NULL;
    (void)pthread_mutex_lock(&session->mutex);
    for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
      GtaNativeThread *candidate = session->threads[i];
      if (candidate != NULL && candidate != caller &&
          candidate != session->main_thread && candidate->cleanup_requested &&
          !candidate->reaping && !candidate->joined &&
          atomic_load_explicit(&candidate->done, memory_order_acquire)) {
        candidate->reaping = true;
        target = candidate;
        break;
      }
    }
    (void)pthread_mutex_unlock(&session->mutex);
    if (target == NULL) return;

    int join_error = pthread_join(target->native, NULL);
    (void)pthread_mutex_lock(&session->mutex);
    target->reaping = false;
    if (join_error == 0 && target->slot < GTA_NATIVE_MAX_THREADS &&
        session->threads[target->slot] == target) {
      session->threads[target->slot] = NULL;
      if (session->active_threads != 0u) --session->active_threads;
      target->joined = true;
    }
    (void)pthread_cond_broadcast(&session->condition);
    (void)pthread_mutex_unlock(&session->mutex);
    if (join_error != 0) return;
    free(target);
  }
}

static int gta_native_cleanup_request(GtaNativeThread *thread,
                                      uint64_t pointer) {
  GtaNativeSession *session = thread->session;
  if (pointer == 0u) return 0;
  (void)pthread_mutex_lock(&session->mutex);
  GtaNativeThread *target = gta_native_find_thread_locked(session, pointer);
  if (target == NULL) {
    (void)pthread_mutex_unlock(&session->mutex);
    return 0;
  }
  target->cleanup_requested = true;
  atomic_store_explicit(&session->reap_hint, true, memory_order_release);
  (void)pthread_mutex_unlock(&session->mutex);
  gta_native_reap_finished(session, thread);
  return 0;
}

static bool gta_dxvk_wanted(void) {
  char value[PROP_VALUE_MAX];
  return __system_property_get("debug.gta5.dxvk", value) > 0 && strcmp(value, "1") == 0;
}

/* Called with the session mutex held, from wasm_gpu::Init on the engine thread that emits first. */
static int gta_dxvk_start(GtaNativeSession *session) {
  /* DXVK reads DXVK_HUD while its libraries load, so it is set before dlopen (debug.gta5.dxvk_hud). */
  char hud[PROP_VALUE_MAX];
  if (__system_property_get("debug.gta5.dxvk_hud", hud) > 0 && hud[0] && strcmp(hud, "0") != 0) setenv("DXVK_HUD", hud, 1);
  void *library = dlopen("libgta5_dxvk.so", RTLD_NOW | RTLD_LOCAL);
  if (library == NULL) {
    __android_log_print(ANDROID_LOG_ERROR, "GtaDirectHost", "dlopen libgta5_dxvk.so: %s", dlerror());
    return -1;
  }
  GtaDxvkCreate create = (GtaDxvkCreate)dlsym(library, "gta_dxvk_create");
  gta_dxvk.emit = (GtaDxvkEmit)dlsym(library, "gta_dxvk_emit");
  gta_dxvk.surface = (GtaDxvkSurface)dlsym(library, "gta_dxvk_surface");
  gta_dxvk.check = (GtaDxvkCheck)dlsym(library, "gta_dxvk_check");
  gta_dxvk.last_error = (GtaDxvkLastError)dlsym(library, "gta_dxvk_last_error");
  gta_dxvk.progress = (GtaDxvkProgress)dlsym(library, "gta_dxvk_progress");
  gta_dxvk.destroy = (GtaDxvkDestroy)dlsym(library, "gta_dxvk_destroy");
  if (!create || !gta_dxvk.emit || !gta_dxvk.surface || !gta_dxvk.check || !gta_dxvk.last_error ||
      !gta_dxvk.progress || !gta_dxvk.destroy) return -1;
  GtaDxvkHost host = {gta_native_memory_base(&session->memory), (const size_t *)&session->memory.size,
                      gta_native_notify, session->assets_root};
  gta_dxvk.bridge = create(&host);
  if (gta_dxvk.bridge == NULL) return -1;
  if (session->window != NULL && !session->suspended &&
      gta_dxvk.surface(gta_dxvk.bridge, 1u, session->window, session->width, session->height) != 0) return -1;
  __atomic_store_n(&gta_dxvk_emit_hook, gta_dxvk_emit_command, __ATOMIC_RELEASE);
  return 0;
}

static uint64_t gta_native_dispatch_root(GtaNativeThread *thread,
                                         const char *name,
                                         const uint64_t *args, size_t argc) {
  GtaNativeSession *session = thread->session;
  if (strcmp(name, "env.__pthread_create_js") == 0) {
    if (argc < 4u) gta_native_fail(thread, "__pthread_create_js needs four arguments");
    if (args[0] == 0u || args[2] == 0u) {
      gta_native_fail(thread, "__pthread_create_js received a null pthread record or routine");
    }
    (void)pthread_mutex_lock(&session->mutex);
    if (atomic_load_explicit(&session->stop_requested, memory_order_acquire) ||
        session->active_threads >= GTA_NATIVE_MAX_THREADS ||
        gta_native_find_thread_locked(session, args[0]) != NULL) {
      (void)pthread_mutex_unlock(&session->mutex);
      return GTA_NATIVE_EAGAIN;
    }
    size_t slot = 0;
    while (slot < GTA_NATIVE_MAX_THREADS && session->threads[slot] != NULL) ++slot;
    if (slot == GTA_NATIVE_MAX_THREADS) {
      (void)pthread_mutex_unlock(&session->mutex);
      return GTA_NATIVE_EAGAIN;
    }
    GtaNativeThread *child = (GtaNativeThread *)calloc(1u, sizeof(*child));
    if (child == NULL) {
      (void)pthread_mutex_unlock(&session->mutex);
      return GTA_NATIVE_EAGAIN;
    }
    child->session = session;
    child->slot = slot;
    child->pthread_ptr = args[0];
    child->start_routine = args[2];
    child->argument = args[3];
    atomic_init(&child->done, false);
    atomic_init(&child->failed, false);
    atomic_init(&child->mailbox_pending, false);
    /* Mark the slot joinable before creation.  The worker may finish before
     * pthread_create returns to this caller. */
    child->native_started = true;
    session->threads[slot] = child;
    ++session->active_threads;
    (void)pthread_mutex_unlock(&session->mutex);
    int error = gta_native_spawn(&child->native, child);
    if (error != 0) {
      (void)pthread_mutex_lock(&session->mutex);
      session->threads[slot] = NULL;
      --session->active_threads;
      (void)pthread_mutex_unlock(&session->mutex);
      child->native_started = false;
      free(child);
      return GTA_NATIVE_EAGAIN;
    }
    return 0;
  }
  if (strcmp(name, "env._emscripten_init_main_thread_js") == 0) {
    if (argc < 1u || args[0] == 0u) gta_native_fail(thread, "main pthread pointer is invalid");
    thread->pthread_ptr = args[0];
    w2c_gta5__native_0x5Femscripten_thread_init(thread->instance, args[0], 0u,
                                                1u, 1u, 1048576u, 0u);
    (void)w2c_gta5__native_0x5Femscripten_tls_init(thread->instance);
    gta_native_arm_mailbox(thread, args[0]);
    return 0;
  }
  if (strcmp(name, "env._emscripten_thread_mailbox_await") == 0) {
    if (argc < 1u) gta_native_fail(thread, "mailbox await needs a pthread record");
    if (thread->pthread_ptr != 0u && thread->pthread_ptr != args[0]) {
      gta_native_fail(thread, "one native instance cannot own two mailboxes");
    }
    gta_native_arm_mailbox(thread, args[0]);
    return 0;
  }
  if (strcmp(name, "env._emscripten_notify_mailbox_postmessage") == 0) {
    if (argc < 2u || !gta_native_notify_thread(thread, args[0])) {
      gta_native_fail(thread, "mailbox notification references an unknown pthread");
    }
    return 0;
  }
  if (strcmp(name, "env._emscripten_thread_cleanup") == 0) {
    return (uint64_t)gta_native_cleanup_request(thread, argc == 0u ? 0u : args[0]);
  }
  if (strcmp(name, "env._emscripten_thread_set_strongref") == 0) return 0;
  if (strcmp(name, "env._emscripten_receive_on_main_thread_js") == 0) {
    double value = gta_native_proxy_receive(thread, args, argc);
    uint64_t bits;
    memcpy(&bits, &value, sizeof(bits));
    return bits;
  }
  if (strcmp(name, "env.emscripten_asm_const_int") == 0 ||
      strcmp(name, "env.emscripten_asm_const_int_sync_on_main_thread") == 0) {
    if (argc < 3u) gta_native_fail(thread, "ASM import needs three arguments");
    return (uint32_t)gta_native_asm_from_memory(thread, args[0], args[1], args[2]);
  }
  if (strcmp(name, "env._emscripten_system") == 0) {
    if (argc < 1u) gta_native_fail(thread, "system import needs one argument");
    return args[0] == 0u ? 0u : (uint32_t)-52;
  }
  if (strcmp(name, "env.emscripten_resize_heap") == 0) {
    if (argc < 1u) gta_native_fail(thread, "resize_heap needs one argument");
    return (uint32_t)gta_native_resize_heap(thread, args[0]);
  }
  if (strcmp(name, "env.emscripten_get_heap_max") == 0) {
    uint64_t maximum;
    if (__builtin_mul_overflow(session->memory.max_pages,
                               (uint64_t)session->memory.page_size, &maximum)) {
      gta_native_fail(thread, "heap maximum overflows host size");
    }
    return maximum;
  }
  /* No stacks: the engine captures them only for its debug logs (ragdoll and AI logging), and unwinding the
   * 191 MB library on the game thread took the loader lock. Emscripten answers the same with no stack. */
  if (strcmp(name, "env.emscripten_stack_snapshot") == 0 ||
      strcmp(name, "env.emscripten_stack_unwind_buffer") == 0) return 0;
  if (strcmp(name, "env.emscripten_exit_with_live_runtime") == 0) {
    if (!thread->runtime_unwind_ready)
      gta_native_fail(thread, "live-runtime exit has no native event boundary");
    gta_native_current_jump = NULL;
#if WASM_RT_STACK_DEPTH_COUNT
    wasm_rt_call_stack_depth = 0;
#endif
    longjmp(thread->runtime_unwind, 1);
  }
  if (strcmp(name, "env._abort_js") == 0) gta_native_fail(thread, "native engine aborted");
  if (strcmp(name, "env.exit") == 0 ||
      strcmp(name, "wasi_snapshot_preview1.proc_exit") == 0) {
    int code = argc == 0u ? 0 : (int)(uint32_t)args[0];
    thread->exit_code = code;
    (void)pthread_mutex_lock(&session->mutex);
    session->exit_code = code;
    atomic_store_explicit(&session->stop_requested, true, memory_order_release);
    if (session->state != GTA_NATIVE_SESSION_FAILED) session->state = GTA_NATIVE_SESSION_STOPPING;
    (void)pthread_cond_broadcast(&session->condition);
    (void)pthread_mutex_unlock(&session->mutex);
    pthread_exit(NULL);
  }
  if (strcmp(name, "env.wgpu_start_worker") == 0) {
    if (argc < 3u) gta_native_fail(thread, "wgpu_start_worker needs three arguments");
    (void)pthread_mutex_lock(&session->mutex);
    if (gta_dxvk.bridge != NULL) {
      (void)pthread_mutex_unlock(&session->mutex);
      gta_native_fail(thread, "GPU worker already started");
    }
    if (gta_dxvk_wanted()) {
      int started = gta_dxvk_start(session);
      (void)pthread_mutex_unlock(&session->mutex);
      if (started != 0) gta_native_fail(thread, "DXVK backend failed to start");
      return 0;
    }
    if (session->gpu != NULL) {
      (void)pthread_mutex_unlock(&session->mutex);
      gta_native_fail(thread, "GPU worker already started");
    }
    session->gpu = gta_native_gpu_create();
    if (session->gpu == NULL) {
      (void)pthread_mutex_unlock(&session->mutex);
      gta_native_fail(thread, "GPU worker creation failed");
    }
    int result = gta_native_gpu_bind_with_size(
        session->gpu, gta_native_memory_base(&session->memory),
        (size_t)session->memory.max_pages * session->memory.page_size +
            session->memory.page_size,
        &session->memory.size, (size_t)args[0], (size_t)args[1],
        (size_t)args[2], session->shader_root);
#ifdef __ANDROID__
    if(result==0) {
      GtaMovieManager *movies=gta_movie_manager_create(&session->memory,session->assets_root);
      if(movies) {
        gta_movie_manager_stop_flag(movies,&session->stop_requested);
        atomic_store_explicit(&session->movies,movies,memory_order_release);
        gta_movie_manager_suspend(movies,session->suspended||session->window==NULL);
        result=gta_native_gpu_movies(session->gpu,gta_movie_manager_frame,movies);
        if(result==0)atomic_store_explicit(&session->movies_ready,true,memory_order_release);
      }
    }
#endif
    if (result == 0 && session->window != NULL && !session->suspended)
      result = gta_native_gpu_surface(session->gpu, 1u, session->window,
                                      session->width, session->height);
    (void)pthread_mutex_unlock(&session->mutex);
    if (result != 0) {
      char gpu_error[GTA_NATIVE_MAX_ERROR] = "";
      (void)gta_native_gpu_last_error(session->gpu, gpu_error, sizeof(gpu_error));
      gta_native_fail(thread, gpu_error[0] ? gpu_error : "GPU worker binding failed");
    }
    return 0;
  }
  return UINT64_MAX;
}

static uint64_t gta_native_dispatch_impl(GtaNativeThread *thread,
                                         const char *name,
                                         const uint64_t *args, size_t argc) {
  uint64_t root_result = gta_native_dispatch_root(thread, name, args, argc);
  if (root_result != UINT64_MAX) return root_result;
  return gta_native_call_service(thread, name, args, argc);
}

static void gta_native_import_prologue(GtaNativeThread *thread) {
  gta_native_reap_finished(thread->session, thread);
  if (thread == thread->session->main_thread) {
    gta_native_pump_proxies(thread);
  }
  gta_native_pump_mailbox(thread);
}

double gta_native_now_ms(GtaNativeThread *thread) {
  if (thread == NULL || thread->session == NULL) gta_native_fail(thread, "native import has no owner");
  if (atomic_load_explicit(&thread->session->stop_requested, memory_order_acquire) && !thread->crash_reporting) {
    atomic_store_explicit(&thread->failed, true, memory_order_release);
    pthread_exit(NULL);
  }
  gta_native_import_prologue(thread);
  struct timespec now;
  (void)clock_gettime(CLOCK_REALTIME, &now);
  return (double)now.tv_sec * 1000.0 + (double)now.tv_nsec / 1e6;
}

uint64_t gta_native_dispatch(GtaNativeThread *thread, const char *name,
                             const uint64_t *args, size_t argc) {
  if (thread == NULL || thread->session == NULL || name == NULL) {
    gta_native_fail(thread, "native import has no owner");
  }
  if (atomic_load_explicit(&thread->session->stop_requested, memory_order_acquire) &&
      strcmp(name, "env._emscripten_thread_cleanup") != 0 &&
      !thread->crash_reporting) {
    /* A stop (engine exit, app shutdown, or an earlier failure, whose message
     * is already kept) cancels this thread; it is not a failure of its own. */
    atomic_store_explicit(&thread->failed, true, memory_order_release);
    pthread_exit(NULL);
  }
  gta_native_import_prologue(thread);
  return gta_native_dispatch_impl(thread, name, args, argc);
}

static bool gta_native_call_start(GtaNativeThread *thread) {
  wasm_rt_funcref_table_t *table =
      w2c_gta5__native_0x5F_indirect_function_table(thread->instance);
  if (thread->start_routine >= table->size) {
    gta_native_fail(thread, "pthread start routine is outside indirect table");
  }
  wasm_rt_funcref_t entry = table->data[thread->start_routine];
  if (entry.func == NULL || entry.func_type == NULL) {
    gta_native_fail(thread, "pthread start routine is null");
  }
  wasm_rt_func_type_t i64_i64 = wasm2c_gta5__native_get_func_type(
      1u, 1u, WASM_RT_I64, WASM_RT_I64);
  wasm_rt_func_type_t i32_i32 = wasm2c_gta5__native_get_func_type(
      1u, 1u, WASM_RT_I32, WASM_RT_I32);
  wasm_rt_func_type_t i64_i32 = wasm2c_gta5__native_get_func_type(
      1u, 1u, WASM_RT_I64, WASM_RT_I32);
  wasm_rt_func_type_t i32_i64 = wasm2c_gta5__native_get_func_type(
      1u, 1u, WASM_RT_I32, WASM_RT_I64);
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(thread->instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) != 0) {
    gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
    wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
    w2c_gta5__native_0x5Femscripten_stack_restore(thread->instance, frame.stack);
    w2c_gta5__native_setThrew(thread->instance, 1u, 0u);
    return false;
  }
  uint64_t result = 0;
  if (entry.func_type == i64_i64 ||
      (i64_i64 != NULL && memcmp(entry.func_type, i64_i64, 32u) == 0)) {
    result = ((uint64_t (*)(void *, uint64_t))entry.func)(
        thread->instance,
        thread->argument);
  } else if (entry.func_type == i64_i32 ||
             (i64_i32 != NULL && memcmp(entry.func_type, i64_i32, 32u) == 0)) {
    result = (uint64_t)(int64_t)((int32_t)((uint32_t (*)(void *, uint64_t))entry.func)(
        thread->instance,
        thread->argument));
  } else if (entry.func_type == i32_i32 ||
             (i32_i32 != NULL && memcmp(entry.func_type, i32_i32, 32u) == 0)) {
    result = (uint64_t)(int64_t)((int32_t)((uint32_t (*)(void *, uint32_t))entry.func)(
        thread->instance,
        (uint32_t)thread->argument));
  } else if (entry.func_type == i32_i64 ||
             (i32_i64 != NULL && memcmp(entry.func_type, i32_i64, 32u) == 0)) {
    result = ((uint64_t (*)(void *, uint32_t))entry.func)(
        thread->instance,
        (uint32_t)thread->argument);
  } else {
    gta_native_current_jump = frame.previous;
    gta_native_fail(thread, "pthread start routine has a non-integer signature");
  }
  gta_native_current_jump = frame.previous;
  thread->exit_code = (int)result;
  return true;
}

static void gta_native_thread_publish_done(void *opaque) {
  GtaNativeThread *thread = (GtaNativeThread *)opaque;
  GtaNativeSession *session = thread->session;
  if (thread == session->main_thread ||
      atomic_load_explicit(&session->stop_requested, memory_order_acquire)) {
    gta_native_proxy_abort_all(session);
  }
  (void)pthread_mutex_lock(&session->mutex);
  if (thread == session->main_thread &&
      session->state != GTA_NATIVE_SESSION_FAILED &&
      session->state != GTA_NATIVE_SESSION_RESTARTING) {
    session->state = atomic_load_explicit(&session->stop_requested,
                                          memory_order_acquire)
                         ? GTA_NATIVE_SESSION_STOPPING
                         : GTA_NATIVE_SESSION_STOPPED;
  }
  /* This is the final use of `thread` in its own pthread.  A reaper may free
   * it once the release store becomes visible. */
  atomic_store_explicit(&thread->done, true, memory_order_release);
  atomic_store_explicit(&session->reap_hint, true, memory_order_release);
  (void)pthread_cond_broadcast(&session->condition);
  (void)pthread_mutex_unlock(&session->mutex);
  gta_native_current_jump = NULL;
  gta_native_current_thread = NULL;
}

/* Emscripten gives every pthread its own module instance, and wasm2c gives each instance its own copy of the
 * 89,368-entry function table (2.9 MiB; about 50 engine threads). Nothing writes or grows the table after
 * instantiation and every call passes the caller's own instance (patch_shared_table.py, generate_imports.py),
 * so all instances read the first instance's table. It is kept for the process lifetime. */
static _Atomic(wasm_rt_funcref_t *) gta_native_shared_table;

static void gta_native_share_table(w2c_gta5__native *instance) {
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  wasm_rt_funcref_t *shared = NULL;
  if (atomic_compare_exchange_strong(&gta_native_shared_table, &shared, table->data)) return;
  free(table->data);
  table->data = shared;
}

static void gta_native_thread_cleanup(void *opaque) {
  GtaNativeThread *thread = (GtaNativeThread *)opaque;
  if (thread->instance != NULL) {
    if (atomic_load_explicit(&thread->failed, memory_order_acquire) &&
        thread->instance_initialized && !thread->crash_reporting) {
      thread->crash_reporting = true;
      w2c_gta5__native_0x5Femscripten_thread_crashed(thread->instance);
    }
    wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(thread->instance);
    if (table->data == atomic_load(&gta_native_shared_table)) table->data = NULL;
    wasm2c_gta5__native_free(thread->instance);
    free(thread->instance);
    thread->instance = NULL;
  }
  wasm_rt_free_thread();
}

static void gta_native_thread_run(GtaNativeThread *thread) {
  thread->instance = (w2c_gta5__native *)calloc(1u, sizeof(*thread->instance));
  if (thread->instance == NULL) gta_native_fail(thread, "generated instance allocation failed");
  thread->env.thread = thread;
  thread->wasi.thread = thread;
  wasm2c_gta5__native_instantiate(thread->instance, &thread->env, &thread->wasi);
  gta_native_share_table(thread->instance);
  thread->instance_initialized = true;
  if (thread != thread->session->main_thread) {
    if (!gta_native_stack_plan(thread, thread->pthread_ptr, &thread->stack_high,
                               &thread->stack_low)) {
      gta_native_fail(thread, "pthread stack record is outside guest memory");
    }
    w2c_gta5__native_emscripten_stack_set_limits(thread->instance,
                                                  thread->stack_high,
                                                  thread->stack_low);
    w2c_gta5__native_0x5Femscripten_stack_restore(thread->instance,
                                                   thread->stack_high);
    /* Worker identity must not claim the main runtime. The preserved
     * worker contract is (pthread, browserMain=0, runtimeMain=0,
     * canBlock=1, defaultStack=0, profiling=0). Misidentification turns
     * an ordinary worker exit into process exit. */
    w2c_gta5__native_0x5Femscripten_thread_init(
        thread->instance, thread->pthread_ptr, 0u, 0u, 1u, 0u, 0u);
    (void)w2c_gta5__native_0x5Femscripten_tls_init(thread->instance);
    gta_native_arm_mailbox(thread, thread->pthread_ptr);
    if (gta_native_call_start(thread)) {
      w2c_gta5__native_0x5Femscripten_thread_exit(
          thread->instance, (uint64_t)(int64_t)thread->exit_code);
    }
    return;
  }
  w2c_gta5__native_0x5F_wasm_call_ctors(thread->instance);
  (void)pthread_mutex_lock(&thread->session->mutex);
  if (thread->session->state != GTA_NATIVE_SESSION_FAILED)
    thread->session->state = GTA_NATIVE_SESSION_RUNNING;
  (void)pthread_cond_broadcast(&thread->session->condition);
  (void)pthread_mutex_unlock(&thread->session->mutex);

  /* Required engine startup contract, observed in the preserved launcher.
   * /game is the native immutable asset mount; -wgpu selects the command
   * stream served by our Vulkan backend. No browser executes these flags.
   * The launcher's log levels and script core follow. -frameLimit is the
   * game's own limiter in vertical syncs: 2 is 30 fps, 0 turns it off.
   * `setprop debug.gta5.frame_limit <syncs>`. */
  const char *arguments[24] = {
      "gta5-native", "-rootdir=/game/", "-wgpu", "-nodisplaycalibration",
      "-noSocialClub", "-pc:nosocialclub", "-nonetwork", "-output",
      "-Script_all=warning", "-replay_all=warning", "-fastscriptcore",
      "-frameLimit=0"
  };
  static char frame_limit[PROP_VALUE_MAX + 16];
  char syncs[PROP_VALUE_MAX];
  if (__system_property_get("debug.gta5.frame_limit", syncs) > 0 && atoi(syncs) >= 0) {
    (void)snprintf(frame_limit, sizeof(frame_limit), "-frameLimit=%d", atoi(syncs));
    arguments[11] = frame_limit;
  }
  uint32_t argc = 12u;
  /* `adb shell setprop debug.gta5.args "-wasmtestdrive"`: extra engine flags
   * (the engine's scripted walk/drive/flight tests) without a rebuild. */
  char vram[PROP_VALUE_MAX];
  gta_native_eager_resource_release =
      (__system_property_get("debug.gta5.eager_release", vram) > 0 && strcmp(vram, "1") == 0);
  if (__system_property_get("debug.gta5.vram_mb", vram) > 0 && atoll(vram) >= 256)
    gta_native_video_memory_bytes = (uint64_t)atoll(vram) << 20;
  if (__system_property_get("debug.gta5.stream_mb", vram) > 0 && atoll(vram) >= 256)
    gta_native_streaming_memory_bytes = (uint64_t)atoll(vram) << 20;
  static char extra[PROP_VALUE_MAX];
  if (__system_property_get("debug.gta5.args", extra) > 0) {
    char *cursor = NULL;
    for (char *token = strtok_r(extra, " ", &cursor); token != NULL && argc < 24u; token = strtok_r(NULL, " ", &cursor))
      arguments[argc++] = token;
  }
  uint64_t pointer_count = (uint64_t)argc + 1u;
  uint64_t argv_bytes;
  if (__builtin_mul_overflow(pointer_count, (uint64_t)sizeof(uint64_t),
                             &argv_bytes)) {
    gta_native_fail(thread, "main argv size overflows");
  }
  uint64_t argv = gta_native_stack_alloc_checked(
      thread, argv_bytes, _Alignof(uint64_t),
      "main argv allocation is outside guest memory");
  for (uint32_t index = 0; index < argc; ++index) {
    uint64_t length = (uint64_t)strlen(arguments[index]) + 1u;
    uint64_t address = gta_native_stack_alloc_checked(
        thread, length, 1u,
        "main argument string allocation is outside guest memory");
    if (!gta_native_guest_write(thread->session, address, arguments[index],
                                (size_t)length, 1u) ||
        !gta_native_guest_store_u64(thread->session,
                                    argv + (uint64_t)index * sizeof(uint64_t),
                                    address)) {
      gta_native_fail(thread, "main argv or argument string is outside guest memory");
    }
  }
  if (!gta_native_guest_store_u64(thread->session,
                                 argv + (uint64_t)argc * sizeof(uint64_t), 0u)) {
    gta_native_fail(thread, "main argv terminator is outside guest memory");
  }
  uint32_t code = w2c_gta5__native_0x5F_main_argc_argv(thread->instance, argc, argv);
  thread->exit_code = (int)code;
  if (!atomic_load_explicit(&thread->failed, memory_order_acquire)) {
    w2c_gta5__native_0x5F_funcs_on_exit(thread->instance);
    (void)pthread_mutex_lock(&thread->session->mutex);
    thread->session->exit_code = thread->exit_code;
    atomic_store_explicit(&thread->session->stop_requested, true, memory_order_release);
    if (thread->session->state != GTA_NATIVE_SESSION_FAILED)
      thread->session->state = GTA_NATIVE_SESSION_STOPPING;
    (void)pthread_cond_broadcast(&thread->session->condition);
    (void)pthread_mutex_unlock(&thread->session->mutex);
  }
}

static int gta_native_spawn(pthread_t *native, GtaNativeThread *thread) {
  pthread_attr_t attributes;
  int error = pthread_attr_init(&attributes);
  if (error != 0) return error;
  error = pthread_attr_setstacksize(&attributes, GTA_NATIVE_ENGINE_STACK_BYTES);
  if (error == 0)
    error = pthread_create(native, &attributes, gta_native_thread_entry, thread);
  (void)pthread_attr_destroy(&attributes);
  return error;
}

static void *gta_native_thread_entry(void *opaque) {
  GtaNativeThread *thread = (GtaNativeThread *)opaque;
  gta_native_current_thread = thread;
  wasm_rt_init_thread();
  pthread_cleanup_push(gta_native_thread_publish_done, thread);
  pthread_cleanup_push(gta_native_thread_cleanup, thread);
  thread->runtime_unwind_ready = true;
  if (setjmp(thread->runtime_unwind) == 0) {
    gta_native_thread_run(thread);
  } else {
    /* Keep the native instance and TLS alive after the engine abandons main.
     * Only this owning pthread may invoke its queued mailbox callbacks. */
    while (!atomic_load_explicit(&thread->session->stop_requested, memory_order_acquire)) {
      gta_native_reap_finished(thread->session, thread);
      gta_native_pump_proxies(thread);
      gta_native_pump_mailbox(thread);
      struct timespec pause = {0, 1000000L};
      (void)nanosleep(&pause, NULL);
    }
  }
  thread->runtime_unwind_ready = false;
  pthread_cleanup_pop(1);
  pthread_cleanup_pop(1);
  return NULL;
}

uint32_t gta_native_movie_call(w2c_gta5__native *instance,uint32_t operation,uint64_t a,uint64_t b) {
#ifdef __ANDROID__
  if(!instance||!instance->w2c_env_instance||!instance->w2c_env_instance->thread)return 0;
  GtaNativeSession *session=instance->w2c_env_instance->thread->session;
  if(!atomic_load_explicit(&session->movies_ready,memory_order_acquire))return 0;
  GtaMovieManager *movies=atomic_load_explicit(&session->movies,memory_order_acquire);
  if(!movies)return 0;
  if(atomic_load_explicit(&session->stop_requested,memory_order_acquire)&&operation!=3&&operation!=4)return 0;
  return operation==0?1:gta_movie_manager_call(movies,instance,operation,a,b);
#else
  (void)instance;(void)operation;(void)a;(void)b;return 0;
#endif
}

uint32_t gta_native_movie_begin_draw(w2c_gta5__native *instance,uint32_t hash) {
#ifdef __ANDROID__
  if(!gta_native_movie_call(instance,8,hash,0))return 0;
  uint64_t saved=instance->w2c_g0;
  if(saved<16)return 0;
  uint64_t address=saved-16,size=gta_native_memory_size(instance->w2c_env_memory);
  if((address&3)||address>size||size-address<4)return 0;
  atomic_store_explicit((_Atomic uint32_t *)(gta_native_memory_base(instance->w2c_env_memory)+address),hash,memory_order_release);
  instance->w2c_g0=address;
  extern void w2c_gta5__native_f550(w2c_gta5__native *,u32,u64,u32);
  w2c_gta5__native_f550(instance,48,address,1);
  instance->w2c_g0=saved;
  return 1;
#else
  (void)instance;(void)hash;return 0;
#endif
}

void gta_native_movie_end_draw(w2c_gta5__native *instance) {
#ifdef __ANDROID__
  if(gta_native_movie_call(instance,0,0,0)) {
    extern void w2c_gta5__native_f550(w2c_gta5__native *,u32,u64,u32);
    w2c_gta5__native_f550(instance,50,0,0);
  }
#else
  (void)instance;
#endif
}

GtaNativeSession *gta_native_session_create(const char *assets_root,
                                             const char *save_root) {
  if (assets_root == NULL || save_root == NULL) return NULL;
  GtaNativeSession *session = (GtaNativeSession *)calloc(1u, sizeof(*session));
  if (session == NULL) return NULL;
  if (pthread_mutex_init(&session->mutex, NULL) != 0) {
    free(session);
    return NULL;
  }
  if (pthread_cond_init(&session->condition, NULL) != 0) {
    pthread_mutex_destroy(&session->mutex);
    free(session);
    return NULL;
  }
  if (pthread_mutex_init(&session->proxy_mutex, NULL) != 0) {
    pthread_cond_destroy(&session->condition);
    pthread_mutex_destroy(&session->mutex);
    free(session);
    return NULL;
  }
  session->assets_root = gta_native_strdup(assets_root);
  session->save_root = gta_native_strdup(save_root);
  size_t asset_length = strlen(assets_root);
  session->shader_root = malloc(asset_length + 10u);
  if (session->shader_root != NULL) {
    memcpy(session->shader_root, assets_root, asset_length + 1u);
    char *slash = strrchr(session->shader_root, '/');
    if (slash == NULL) strcpy(session->shader_root, "shaders");
    else strcpy(slash + 1, "shaders");
  }
  if (session->assets_root == NULL || session->save_root == NULL || session->shader_root == NULL) {
    free(session->assets_root);
    free(session->save_root);
    free(session->shader_root);
    pthread_mutex_destroy(&session->proxy_mutex);
    pthread_cond_destroy(&session->condition);
    pthread_mutex_destroy(&session->mutex);
    free(session);
    return NULL;
  }
  atomic_init(&session->stop_requested, false);
  atomic_init(&session->movies,NULL);
  atomic_init(&session->movies_ready,false);
  atomic_init(&session->pad_buttons,0u);
  atomic_init(&session->pad_sticks,GTA_NATIVE_PAD_CENTRE);
  atomic_init(&session->pad_triggers,0u);
  atomic_init(&session->reap_hint, false);
  session->state = GTA_NATIVE_SESSION_CREATED;
  (void)pthread_once(&gta_native_runtime_once, gta_native_runtime_init);
  wasm_rt_allocate_memory_shared(&session->memory, GTA_NATIVE_INITIAL_PAGES,
                                GTA_NATIVE_MAX_PAGES, true,
                                GTA_NATIVE_PAGE_SIZE);
  uint64_t reserved64;
  if (__builtin_mul_overflow(session->memory.max_pages,
                             (uint64_t)session->memory.page_size, &reserved64) ||
      __builtin_add_overflow(reserved64, (uint64_t)session->memory.page_size,
                             &reserved64) || reserved64 > SIZE_MAX) {
    wasm_rt_free_memory_shared(&session->memory);
    free(session->assets_root);
    free(session->save_root);
    free(session->shader_root);
    pthread_mutex_destroy(&session->proxy_mutex);
    pthread_cond_destroy(&session->condition);
    pthread_mutex_destroy(&session->mutex);
    free(session);
    return NULL;
  }
#ifdef __ANDROID__
  /* Name only the anonymous reservation; smaps then attributes resident pages and swap to the engine. */
  (void)prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, gta_native_memory_base(&session->memory), (size_t)reserved64, "gta-engine-linear");
#endif
  session->services = gta_native_services_create(
      gta_native_memory_base(&session->memory), (size_t)reserved64,
      &session->memory.size, session->assets_root, session->save_root);
  if (session->services == NULL) {
    wasm_rt_free_memory_shared(&session->memory);
    free(session->assets_root);
    free(session->save_root);
    free(session->shader_root);
    pthread_mutex_destroy(&session->proxy_mutex);
    pthread_cond_destroy(&session->condition);
    pthread_mutex_destroy(&session->mutex);
    free(session);
    return NULL;
  }
  return session;
}

int gta_native_session_boot(GtaNativeSession *session) {
  if (session == NULL) return -1;
  (void)pthread_mutex_lock(&session->mutex);
  if (session->state != GTA_NATIVE_SESSION_CREATED) {
    (void)pthread_mutex_unlock(&session->mutex);
    return -1;
  }
  GtaNativeThread *main_thread = (GtaNativeThread *)calloc(1u, sizeof(*main_thread));
  if (main_thread == NULL) {
    (void)pthread_mutex_unlock(&session->mutex);
    return -1;
  }
  main_thread->session = session;
  main_thread->slot = 0u;
  atomic_init(&main_thread->done, false);
  atomic_init(&main_thread->failed, false);
  atomic_init(&main_thread->mailbox_pending, false);
  /* The stub or engine can return immediately, so publish joinability before
   * creating the pthread. */
  main_thread->native_started = true;
  session->threads[0] = main_thread;
  session->main_thread = main_thread;
  session->active_threads = 1u;
  session->state = GTA_NATIVE_SESSION_BOOTING;
  int error = gta_native_spawn(&main_thread->native, main_thread);
  char governor[PROP_VALUE_MAX], residency[PROP_VALUE_MAX];
  bool coordinated = __system_property_get("debug.gta5.residency", residency) > 0 && strcmp(residency, "1") == 0;
  if (error == 0 && !coordinated && __system_property_get("debug.gta5.mem_governor", governor) > 0 && strcmp(governor, "1") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, gta_native_memory_governor, NULL) == 0) (void)pthread_detach(thread);
  }
  if (error != 0) {
    session->threads[0] = NULL;
    session->main_thread = NULL;
    session->active_threads = 0u;
    session->state = GTA_NATIVE_SESSION_CREATED;
    main_thread->native_started = false;
    free(main_thread);
    (void)pthread_mutex_unlock(&session->mutex);
    return -1;
  }
  (void)pthread_mutex_unlock(&session->mutex);
  return 0;
}

int gta_native_session_request_stop(GtaNativeSession *session) {
  if (session == NULL) return -1;
  atomic_store_explicit(&session->stop_requested, true, memory_order_release);
#ifdef __ANDROID__
  gta_movie_manager_suspend(atomic_load_explicit(&session->movies,memory_order_acquire),1);
#endif
  (void)pthread_mutex_lock(&session->mutex);
  if (session->state != GTA_NATIVE_SESSION_FAILED &&
      session->state != GTA_NATIVE_SESSION_STOPPED)
    session->state = GTA_NATIVE_SESSION_STOPPING;
  (void)pthread_cond_broadcast(&session->condition);
  (void)pthread_mutex_unlock(&session->mutex);
  gta_native_proxy_abort_all(session);
  gta_native_notify_waiters(session);
  return 0;
}

static bool gta_native_all_done_locked(const GtaNativeSession *session) {
  for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
    const GtaNativeThread *thread = session->threads[i];
    if (thread != NULL && !atomic_load_explicit(&thread->done, memory_order_acquire))
      return false;
  }
  return true;
}

static struct timespec gta_native_deadline(uint32_t timeout_ms) {
  struct timespec deadline;
  (void)clock_gettime(CLOCK_REALTIME, &deadline);
  deadline.tv_sec += (time_t)(timeout_ms / 1000u);
  long nanos = deadline.tv_nsec + (long)(timeout_ms % 1000u) * 1000000L;
  deadline.tv_sec += nanos / 1000000000L;
  deadline.tv_nsec = nanos % 1000000000L;
  return deadline;
}

int gta_native_session_stop(GtaNativeSession *session, uint32_t timeout_ms) {
  if (session == NULL) return -1;
  (void)gta_native_session_request_stop(session);
  struct timespec deadline = gta_native_deadline(timeout_ms);
  (void)pthread_mutex_lock(&session->mutex);
  for (;;) {
    bool reaping = false;
    for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
      if (session->threads[i] != NULL && session->threads[i]->reaping) {
        reaping = true;
        break;
      }
    }
    if (gta_native_all_done_locked(session) && !reaping) break;
    if (timeout_ms == 0u) {
      (void)pthread_mutex_unlock(&session->mutex);
      return -2;
    }
    int wait_error = pthread_cond_timedwait(&session->condition, &session->mutex,
                                            &deadline);
    if (wait_error == ETIMEDOUT) {
      (void)pthread_mutex_unlock(&session->mutex);
      return -2;
    }
    if (wait_error != 0) {
      (void)pthread_mutex_unlock(&session->mutex);
      return -1;
    }
  }
  GtaNativeThread *finished[GTA_NATIVE_MAX_THREADS];
  size_t count = 0u;
  for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
    GtaNativeThread *thread = session->threads[i];
    if (thread != NULL && thread->native_started && !thread->joined &&
        pthread_equal(thread->native, pthread_self())) {
      (void)pthread_mutex_unlock(&session->mutex);
      return -2;
    }
  }
  for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
    GtaNativeThread *thread = session->threads[i];
    if (thread != NULL && thread->native_started && !thread->joined) {
      thread->reaping = true;
      finished[count++] = thread;
    }
  }
  (void)pthread_mutex_unlock(&session->mutex);
  for (size_t i = 0; i < count; ++i) {
    int join_error = pthread_join(finished[i]->native, NULL);
    (void)pthread_mutex_lock(&session->mutex);
    finished[i]->reaping = false;
    if (join_error == 0) finished[i]->joined = true;
    (void)pthread_cond_broadcast(&session->condition);
    (void)pthread_mutex_unlock(&session->mutex);
    if (join_error != 0) return -1;
  }
  (void)pthread_mutex_lock(&session->mutex);
  for (size_t i = 0; i < GTA_NATIVE_MAX_THREADS; ++i) {
    GtaNativeThread *thread = session->threads[i];
    if (thread != NULL && thread->joined) {
      session->threads[i] = NULL;
      if (session->active_threads != 0u) --session->active_threads;
      if (session->main_thread == thread) session->main_thread = NULL;
      free(thread);
    }
  }
  if (session->state != GTA_NATIVE_SESSION_FAILED) session->state = GTA_NATIVE_SESSION_STOPPED;
  (void)pthread_cond_broadcast(&session->condition);
  (void)pthread_mutex_unlock(&session->mutex);
  return 0;
}

GtaNativeSessionState gta_native_session_status(const GtaNativeSession *session,
                                                char *error,
                                                size_t error_capacity) {
  if (session == NULL) return GTA_NATIVE_SESSION_FAILED;
  GtaNativeSession *mutable_session = (GtaNativeSession *)(uintptr_t)session;
  (void)pthread_mutex_lock(&mutable_session->mutex);
  bool gpu_failed = mutable_session->gpu != NULL ? gta_native_gpu_check(mutable_session->gpu) != 0
                    : gta_dxvk.bridge != NULL && gta_dxvk.check(gta_dxvk.bridge) != 0;
  if (gpu_failed && mutable_session->state != GTA_NATIVE_SESSION_FAILED) {
    char gpu_error[GTA_NATIVE_MAX_ERROR];
    gpu_error[0] = '\0';
    if (mutable_session->gpu != NULL)
      (void)gta_native_gpu_last_error(mutable_session->gpu, gpu_error, sizeof(gpu_error));
    else
      (void)gta_dxvk.last_error(gta_dxvk.bridge, gpu_error, sizeof(gpu_error));
    gta_native_set_error_locked(
        mutable_session,
        gpu_error[0] == '\0' ? "native GPU worker failed" : gpu_error);
    mutable_session->state = GTA_NATIVE_SESSION_FAILED;
    gta_native_log_first_failure(mutable_session->error);
    atomic_store_explicit(&mutable_session->stop_requested, true,
                          memory_order_release);
    (void)pthread_cond_broadcast(&mutable_session->condition);
  }
  GtaNativeSessionState state = mutable_session->state;
  if (error != NULL && error_capacity != 0u) {
    (void)snprintf(error, error_capacity, "%s", mutable_session->error);
  }
  (void)pthread_mutex_unlock(&mutable_session->mutex);
  return state;
}

void gta_native_session_destroy(GtaNativeSession *session) {
  if (session == NULL) return;
  if (gta_native_session_stop(session, 0u) != 0) {
    fputs("gta_native_session_destroy: active workers retained\n", stderr);
    return;
  }
  if (session->gpu != NULL) {
    (void)gta_native_gpu_stop(session->gpu);
    session->gpu = NULL;
  }
  if (gta_dxvk.bridge != NULL) {
    __atomic_store_n(&gta_dxvk_emit_hook, NULL, __ATOMIC_RELEASE);
    gta_dxvk.destroy(gta_dxvk.bridge);
    gta_dxvk.bridge = NULL;
  }
  if (session->services != NULL) {
    gta_native_services_destroy(session->services);
    session->services = NULL;
  }
#ifdef __ANDROID__
  atomic_store_explicit(&session->movies_ready,false,memory_order_release);
  gta_movie_manager_destroy(atomic_exchange_explicit(&session->movies,NULL,memory_order_acq_rel));
#endif
  wasm_rt_free_memory_shared(&session->memory);
  free(session->assets_root);
  free(session->save_root);
  free(session->shader_root);
#ifdef __ANDROID__
  if (session->window != NULL) ANativeWindow_release(session->window);
#endif
  pthread_cond_destroy(&session->condition);
  pthread_mutex_destroy(&session->proxy_mutex);
  pthread_mutex_destroy(&session->mutex);
  free(session);
}

wasm_rt_shared_memory_t *gta_native_session_memory(GtaNativeSession *session) {
  return session == NULL ? NULL : &session->memory;
}

int gta_native_session_surface(GtaNativeSession *session, void *window,
                               uint32_t width, uint32_t height, int operation) {
  if (!session || operation < 1 || operation > 5) return -1;
  if ((operation == 1 || operation == 4) && (!window || !width || !height)) return -1;
  if (operation == 2 && (!width || !height)) return -1;
  (void)pthread_mutex_lock(&session->mutex);
  if (width && height) { session->width = width; session->height = height; }
  int result = 0;
  if (width && height) {
    /* The engine renders at `debug.gta5.scale_pct` percent of the window; the compositor scales the
     * smaller back buffer up to the window. The default is 75, the user's choice on 2026-10-17 (up from
     * 50, which cut GPU work per frame from 18.0 to 9.4 Mcycles and graphics memory by about 300 MB on High). */
    enum { GTA_DEFAULT_SCALE_PCT = 75 };
    char scale[PROP_VALUE_MAX];
    long percent = __system_property_get("debug.gta5.scale_pct", scale) > 0 ? atol(scale) : GTA_DEFAULT_SCALE_PCT;
    if (percent < 25 || percent > 100) percent = GTA_DEFAULT_SCALE_PCT;
    uint32_t render_width = (uint32_t)((uint64_t)width * (uint64_t)percent / 100u) & ~1u;
    uint32_t render_height = (uint32_t)((uint64_t)height * (uint64_t)percent / 100u) & ~1u;
    result = gta_native_services_viewport(session->services, render_width ? render_width : width, render_height ? render_height : height);
  }
  if (!result && session->gpu && (session->window || operation == 1 || operation == 4))
    result = gta_native_gpu_surface(session->gpu, (uint32_t)operation,
                                   window, session->width, session->height);
  else if (!result && gta_dxvk.bridge && (session->window || operation == 1 || operation == 4))
    result = gta_dxvk.surface(gta_dxvk.bridge, (uint32_t)operation, window, session->width, session->height);
  if (!result && (operation == 1 || operation == 4)) {
#ifdef __ANDROID__
    ANativeWindow_acquire(window);
    if (session->window) ANativeWindow_release(session->window);
#endif
    session->window = window;
    session->suspended = false;
  } else if (!result && operation == 3) {
    session->suspended = true;
  } else if (!result && operation == 5) {
#ifdef __ANDROID__
    if (session->window) ANativeWindow_release(session->window);
#endif
    session->window = NULL;
  }
  if (result && session->state != GTA_NATIVE_SESSION_FAILED)
    gta_native_set_error_locked(session, "native surface operation failed");
#ifdef __ANDROID__
  gta_movie_manager_suspend(atomic_load_explicit(&session->movies,memory_order_acquire),session->suspended||session->window==NULL);
#endif
  (void)pthread_mutex_unlock(&session->mutex);
  return result;
}

static void gta_native_pad_neutral(GtaNativeSession *session) {
  atomic_store_explicit(&session->pad_buttons,0u,memory_order_release);
  atomic_store_explicit(&session->pad_sticks,GTA_NATIVE_PAD_CENTRE,memory_order_release);
  atomic_store_explicit(&session->pad_triggers,0u,memory_order_release);
}

/* Three bytes per float: a float holds every integer below 2^24 exactly. */
static bool gta_native_pad_bytes(float value, uint32_t *out) {
  if (!(value >= 0.0f && value < 16777216.0f) || value != (float)(uint32_t)value) return false;
  *out = (uint32_t)value;
  return true;
}

int gta_native_session_input(GtaNativeSession *session, int kind, int code,
                             float value, float value2) {
  if (!session) return -1;
  if (kind == GTA_NATIVE_INPUT_PAD) {
    uint32_t low, high;
    if (code < 0 || code > 0xffff || !gta_native_pad_bytes(value, &low) || !gta_native_pad_bytes(value2, &high)) return -1;
    atomic_store_explicit(&session->pad_sticks,low|(high&0xffu)<<24,memory_order_release);
    atomic_store_explicit(&session->pad_triggers,high>>8,memory_order_release);
    atomic_store_explicit(&session->pad_buttons,(uint32_t)code,memory_order_release);
    return 0;
  }
  if (kind == 6 || (kind == 5 && code == 0)) gta_native_pad_neutral(session);
  return gta_native_services_input(session->services, kind, code, value, value2);
}

/* rage::ioPad 0 (CControlMgr::Update f65444 updates the four pads at 12840656 + 136 * n, then calls this).
 * Fields as ioPad::Update f10454 leaves them: +12 button bits, +20..23 stick bytes, +24/+25 L2/R2 pressure. */
void gta_native_pad_apply(w2c_gta5__native *instance) {
#ifdef __ANDROID__
  if (!instance || !instance->w2c_env_instance || !instance->w2c_env_instance->thread) return;
  GtaNativeSession *session = instance->w2c_env_instance->thread->session;
  const uint64_t first = 12840656u, size = gta_native_memory_size(instance->w2c_env_memory);
  if (first > size || size - first < 136u * 4u) return;
  uint32_t buttons = atomic_load_explicit(&session->pad_buttons,memory_order_acquire);
  uint32_t sticks = atomic_load_explicit(&session->pad_sticks,memory_order_acquire);
  uint32_t triggers = atomic_load_explicit(&session->pad_triggers,memory_order_acquire);
  uint8_t lt = (uint8_t)triggers, rt = (uint8_t)(triggers >> 8);
  /* A trigger past a quarter of its travel also counts as pressed, as on an XInput pad. */
  buttons |= (lt >= 64u ? 1u : 0u) | (rt >= 64u ? 2u : 0u);
  /* ioPad::UpdateDebug (f10456) swallows a press of a button in this mask (Start | Select = 0x900) and starts
   * reading the following presses as developer debug chords (debug camera and more). Its only reader is f10456. */
  const uint64_t debug_mask = 6666144u;
  if (debug_mask <= size - 4u) memset(gta_native_memory_base(instance->w2c_env_memory) + debug_mask, 0, 4);
  /* Pad 0 only: in this RAGE build pad 1 is the debug pad (it toggles the debug camera). */
  for (unsigned index = 0; index < 1u; index++) {
    uint8_t *pad = gta_native_memory_base(instance->w2c_env_memory) + first + 136u * index;
    uint32_t held;
    memcpy(&held, pad + 12, 4);
    held |= buttons;
    memcpy(pad + 12, &held, 4);
    for (int axis = 0; axis < 4; axis++) {
      uint8_t byte = (uint8_t)(sticks >> (8 * axis));
      if (byte != 0x80u) pad[20 + axis] = byte;
    }
    if (lt > pad[24]) pad[24] = lt;
    if (rt > pad[25]) pad[25] = rt;
  }
#else
  (void)instance;
#endif
}

/* The two flags CPauseMenu::IsActive (f36254) tests first: the pause menu is open or still closing. */
static const uint64_t gta_native_pause_menu_flags[] = {19565906u, 19566104u};

size_t gta_native_session_loading(GtaNativeSession *session, char *lines,
                                  size_t capacity, uint64_t *data_bytes,
                                  uint32_t *presents, bool *world, bool *menu) {
  if (!session || !lines || !capacity || !data_bytes || !presents || !world || !menu) return 0;
  *presents = 0u;
  *world = false;
  *menu = false;
  (void)pthread_mutex_lock(&session->mutex);
  if (session->gpu) gta_native_gpu_progress(session->gpu, presents, world);
  else if (gta_dxvk.bridge) gta_dxvk.progress(gta_dxvk.bridge, presents, world);
  (void)pthread_mutex_unlock(&session->mutex);
  for (size_t i = 0; i < sizeof(gta_native_pause_menu_flags) / sizeof(gta_native_pause_menu_flags[0]); ++i) {
    uint64_t address = gta_native_pause_menu_flags[i];
    if (gta_native_guest_range(session, address, 1u, 1u))
      *menu |= (__atomic_load_n(gta_native_memory_base(&session->memory) + address, __ATOMIC_RELAXED) & 1u) != 0;
  }
  *data_bytes = gta_native_services_data_bytes(session->services);
  return gta_native_services_take_lines(session->services, lines, capacity);
}

#ifdef GTA_NATIVE_HOST_TESTING
typedef struct GtaNativeHostTestProxy {
  GtaNativeThread *thread;
  _Atomic bool returned;
  double result;
} GtaNativeHostTestProxy;

static void *gta_native_host_test_done_worker(void *opaque) {
  GtaNativeThread *thread = (GtaNativeThread *)opaque;
  atomic_store_explicit(&thread->done, true, memory_order_release);
  return NULL;
}

static void *gta_native_host_test_proxy_worker(void *opaque) {
  GtaNativeHostTestProxy *test = (GtaNativeHostTestProxy *)opaque;
  const uint64_t values[] = {0u, 7980673u, 0u, 0u, 0u, 0u, 0u};
  test->result = gta_native_proxy_receive(test->thread, values,
                                          sizeof(values) / sizeof(values[0]));
  atomic_store_explicit(&test->returned, true, memory_order_release);
  return NULL;
}

static void *gta_native_host_test_stopped_import_worker(void *opaque) {
  GtaNativeHostTestProxy *test = (GtaNativeHostTestProxy *)opaque;
  (void)gta_native_dispatch(test->thread, "env.emscripten_get_now", NULL, 0u);
  atomic_store_explicit(&test->returned, true, memory_order_release);
  return NULL;
}

static void *gta_native_host_test_restart_worker(void *opaque) {
  GtaNativeHostTestProxy *test = (GtaNativeHostTestProxy *)opaque;
  (void)gta_native_asm_constant(test->thread, 7980768u, NULL, 0u);
  atomic_store_explicit(&test->returned, true, memory_order_release);
  return NULL;
}

/* Contract test only.  It uses no generated engine and is never built into
 * the APK.  It proves that cleanup permits repeated pointer reuse and that a
 * worker proxy waits for, then runs on, the designated main owner. */
int gta_native_host_contract_test(void) {
  GtaNativeSession session;
  memset(&session, 0, sizeof(session));
  if (pthread_mutex_init(&session.mutex, NULL) != 0 ||
      pthread_cond_init(&session.condition, NULL) != 0 ||
      pthread_mutex_init(&session.proxy_mutex, NULL) != 0) {
    return 1;
  }
  atomic_init(&session.stop_requested, false);
  atomic_init(&session.reap_hint, false);

  for (size_t iteration = 0; iteration <= GTA_NATIVE_MAX_THREADS; ++iteration) {
    GtaNativeThread *worker = (GtaNativeThread *)calloc(1u, sizeof(*worker));
    if (worker == NULL) return 2;
    worker->session = &session;
    worker->slot = 1u;
    worker->pthread_ptr = 0x1000u; /* Deliberately reused every iteration. */
    worker->native_started = true;
    worker->cleanup_requested = true;
    atomic_init(&worker->done, false);
    session.threads[worker->slot] = worker;
    ++session.active_threads;
    if (pthread_create(&worker->native, NULL, gta_native_host_test_done_worker,
                       worker) != 0) return 3;
    while (!atomic_load_explicit(&worker->done, memory_order_acquire))
      (void)sched_yield();
    gta_native_reap_finished(&session, NULL);
    if (session.threads[1] != worker) return 11; /* No hint raised: the table is not scanned. */
    atomic_store_explicit(&session.reap_hint, true, memory_order_release);
    gta_native_reap_finished(&session, NULL);
    if (session.threads[1] != NULL || session.active_threads != 0u) return 4;
  }

  GtaNativeThread owner;
  GtaNativeThread caller;
  memset(&owner, 0, sizeof(owner));
  memset(&caller, 0, sizeof(caller));
  owner.session = &session;
  owner.instance = (w2c_gta5__native *)(uintptr_t)1u;
  caller.session = &session;
  atomic_init(&owner.failed, false);
  atomic_init(&caller.failed, false);
  session.main_thread = &owner;
  GtaNativeHostTestProxy proxy = {.thread = &caller};
  atomic_init(&proxy.returned, false);
  pthread_t proxy_thread;
  if (pthread_create(&proxy_thread, NULL, gta_native_host_test_proxy_worker,
                     &proxy) != 0) return 5;
  for (;;) {
    (void)pthread_mutex_lock(&session.proxy_mutex);
    bool queued = session.proxy_count == 1u;
    (void)pthread_mutex_unlock(&session.proxy_mutex);
    if (queued) break;
    (void)sched_yield();
  }
  gta_native_pump_proxies(&owner);
  if (pthread_join(proxy_thread, NULL) != 0 ||
      !atomic_load_explicit(&proxy.returned, memory_order_acquire) ||
      proxy.result != 0.0) return 6;

  GtaNativeHostTestProxy canceled = {.thread = &caller};
  atomic_init(&canceled.returned, false);
  if (pthread_create(&proxy_thread, NULL, gta_native_host_test_proxy_worker,
                     &canceled) != 0) return 7;
  for (;;) {
    (void)pthread_mutex_lock(&session.proxy_mutex);
    bool queued = session.proxy_count == 1u;
    (void)pthread_mutex_unlock(&session.proxy_mutex);
    if (queued) break;
    (void)sched_yield();
  }
  gta_native_proxy_abort_all(&session);
  if (pthread_join(proxy_thread, NULL) != 0 ||
      session.state != GTA_NATIVE_SESSION_FAILED || session.proxy_count != 0u)
    return 8;

  /* After the engine exits, a thread's next import ends that thread without failing the session. */
  session.state = GTA_NATIVE_SESSION_STOPPING;
  atomic_store_explicit(&session.stop_requested, true, memory_order_release);
  GtaNativeHostTestProxy stopped = {.thread = &caller};
  atomic_init(&stopped.returned, false);
  if (pthread_create(&proxy_thread, NULL, gta_native_host_test_stopped_import_worker,
                     &stopped) != 0 || pthread_join(proxy_thread, NULL) != 0) return 9;
  if (atomic_load_explicit(&stopped.returned, memory_order_acquire) ||
      !atomic_load_explicit(&caller.failed, memory_order_acquire) ||
      session.state != GTA_NATIVE_SESSION_STOPPING) return 10;

  session.state = GTA_NATIVE_SESSION_RUNNING;
  session.error[0] = '\0';
  atomic_store_explicit(&session.stop_requested, false, memory_order_release);
  atomic_store_explicit(&owner.failed, false, memory_order_release);
  GtaNativeHostTestProxy restart = {.thread = &owner};
  atomic_init(&restart.returned, false);
  if (pthread_create(&proxy_thread, NULL, gta_native_host_test_restart_worker,
                     &restart) != 0 || pthread_join(proxy_thread, NULL) != 0) return 12;
  if (atomic_load_explicit(&restart.returned, memory_order_acquire) ||
      session.state != GTA_NATIVE_SESSION_RESTARTING || session.error[0] != '\0' ||
      !atomic_load_explicit(&session.stop_requested, memory_order_acquire)) return 13;
  gta_native_thread_publish_done(&owner);
  gta_native_mark_failed(&caller, "main-thread proxy was canceled");
  if (session.state != GTA_NATIVE_SESSION_RESTARTING || session.error[0] != '\0') return 14;

  pthread_mutex_destroy(&session.proxy_mutex);
  pthread_cond_destroy(&session.condition);
  pthread_mutex_destroy(&session.mutex);
  return 0;
}
#endif
