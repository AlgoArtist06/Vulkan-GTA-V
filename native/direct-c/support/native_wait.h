#ifndef GTA_NATIVE_WAIT_H
#define GTA_NATIVE_WAIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* WebAssembly atomic.wait result values. */
enum {
  GTA_NATIVE_WAIT_WOKEN = 0,
  GTA_NATIVE_WAIT_MISMATCH = 1,
  GTA_NATIVE_WAIT_TIMED_OUT = 2,
};

/*
 * Wait on one address in one shared linear memory.
 *
 * memory_identity must identify the backing shared memory, and address must
 * point into that memory. The generated wrapper owns overflow, bounds, and
 * natural-alignment checks and must trap before calling these functions. The
 * address is loaded with a C11 sequentially-consistent atomic operation.
 *
 * A negative timeout is infinite. Zero is an immediate timeout. Positive
 * values are nanoseconds. The return values match memory.atomic.wait32/64:
 * 0 means a notify was consumed, 1 means the value did not match, and 2
 * means the deadline elapsed. Invalid arguments and operational failures are
 * fatal helper errors with a diagnostic; generated wrappers validate inputs
 * before calling this helper.
 */
int32_t gta_native_wait32(const void *memory_identity, void *address,
                          uint32_t expected, int64_t timeout_ns);
int32_t gta_native_wait64(const void *memory_identity, void *address,
                          uint64_t expected, int64_t timeout_ns);

/*
 * Wake at most count waiters at (memory_identity, address). A count of
 * UINT32_MAX wakes all current waiters. The return value is the number of
 * waiters granted a wake ticket. Notify never creates a queue, so a notify
 * before a waiter is registered cannot be lost by a later address reuse.
 * Invalid arguments and pthread failures are fatal helper errors; they are
 * diagnosed and abort rather than being reported as a false zero.
 */
uint32_t gta_native_notify(const void *memory_identity, void *address,
                           uint32_t count);

#ifdef GTA_NATIVE_WAIT_TESTING
/* Host-test-only scheduler seam. Never enabled in the native APK build. */
void gta_native_wait_test_pause_next_wake(void);
void gta_native_wait_test_wait_until_paused(void);
void gta_native_wait_test_resume(void);
#endif

#ifdef __cplusplus
}
#endif

#endif /* GTA_NATIVE_WAIT_H */
