#ifndef GTA_NATIVE_HOST_H
#define GTA_NATIVE_HOST_H
#include "game.h"
#include <setjmp.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct GtaNativeThread GtaNativeThread;
typedef struct GtaNativeSession GtaNativeSession;
struct w2c_env { GtaNativeThread *thread; };
struct w2c_wasi__snapshot__preview1 { GtaNativeThread *thread; };
typedef struct GtaNativeJumpFrame {
  jmp_buf destination;
  struct GtaNativeJumpFrame *previous;
  uint64_t stack;
#if WASM_RT_STACK_DEPTH_COUNT
  uint32_t call_stack_depth;
#endif
} GtaNativeJumpFrame;
extern _Thread_local GtaNativeJumpFrame *gta_native_current_jump;

/** Session state exposed to the Android/JNI owner. */
typedef enum GtaNativeSessionState {
  GTA_NATIVE_SESSION_CREATED = 0,
  GTA_NATIVE_SESSION_BOOTING = 1,
  GTA_NATIVE_SESSION_RUNNING = 2,
  GTA_NATIVE_SESSION_STOPPING = 3,
  GTA_NATIVE_SESSION_STOPPED = 4,
  GTA_NATIVE_SESSION_FAILED = 5,
  GTA_NATIVE_SESSION_RESTARTING = 6,
} GtaNativeSessionState;

/**
 * Create a browser-free direct-C session.
 *
 * The engine owns the shared linear-memory mapping.  The service and renderer
 * workers borrow it and are stopped before the mapping is released.
 */
GtaNativeSession *gta_native_session_create(const char *assets_root,
                                             const char *save_root);

/** Start the engine thread.  Returns zero after the thread is created. */
int gta_native_session_boot(GtaNativeSession *session);

/** Request cooperative shutdown.  This call does not join workers. */
int gta_native_session_request_stop(GtaNativeSession *session);

/**
 * Request shutdown and join only workers that have reached a terminal state.
 * `timeout_ms == 0` is a non-blocking reap.  Returns zero when all workers
 * were joined, or -2 while a worker is still running.
 */
int gta_native_session_stop(GtaNativeSession *session, uint32_t timeout_ms);

/** Wall-clock milliseconds for the engine's time imports, after the same per-call checks as a dispatch. */
double gta_native_now_ms(GtaNativeThread *thread);

/** Bridge the shipped cascade-shadow effect's local cloud inputs. */
void gta_native_csm_locals(w2c_gta5__native *instance, uint64_t shader,
                           uint64_t cloud, uint64_t lookup, uint64_t params);

/** Read the state and copy the current diagnostic error if requested. */
GtaNativeSessionState gta_native_session_status(const GtaNativeSession *session,
                                                char *error,
                                                size_t error_capacity);

/** Destroy a stopped session.  An active session is retained and diagnosed. */
void gta_native_session_destroy(GtaNativeSession *session);

/** Borrow the shared memory object for JNI/GPU binding. */
wasm_rt_shared_memory_t *gta_native_session_memory(GtaNativeSession *session);
int gta_native_session_surface(GtaNativeSession *, void *, uint32_t, uint32_t, int);
int gta_native_session_input(GtaNativeSession *, int, int, float, float);

/**
 * Title-screen progress: moves queued engine output lines (newline-separated)
 * into `lines`, and reads the game data bytes loaded from storage, the frames
 * presented, whether the world is shown and whether the pause menu is open.
 */
size_t gta_native_session_loading(GtaNativeSession *session, char *lines,
                                  size_t capacity, uint64_t *data_bytes,
                                  uint32_t *presents, bool *world, bool *menu);

/** Return the thread that owns a generated import context. */
GtaNativeThread *gta_native_thread(struct w2c_env *context);

/** Generated import-memory accessor. */
wasm_rt_shared_memory_t *w2c_env_memory(struct w2c_env *context);

w2c_gta5__native *gta_native_instance(GtaNativeThread *);
uint64_t gta_native_dispatch(GtaNativeThread *, const char *, const uint64_t *, size_t);
_Noreturn void gta_native_fail(GtaNativeThread *, const char *);
_Noreturn void gta_native_trap(wasm_rt_trap_t code);
#endif
