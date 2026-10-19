#define _POSIX_C_SOURCE 200809L

#include "native_wait.h"

#include <errno.h>
#include <limits.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <time.h>

#if defined(__ANDROID__) || defined(__linux__)
#include <linux/futex.h>
#include <sys/syscall.h>
#include <unistd.h>
#endif

/*
 * The registry is process-global because the generated engine and renderer
 * can be separate native threads. Its key is deliberately the pair supplied
 * by the generated wrapper, never a thread or module-instance pointer.
 */
#define GTA_WAIT_BUCKET_COUNT 256u

/* Each waiter sleeps on its own condition, so a notify wakes only the
 * waiters it grants instead of every thread waiting on the address. */
typedef struct gta_wait_waiter {
  struct gta_wait_waiter *next;
  bool granted;
  pthread_cond_t condition;
} gta_wait_waiter;

typedef struct gta_wait_entry {
  const void *memory_identity;
  void *address;
  pthread_mutex_t mutex;
  gta_wait_waiter *head;
  gta_wait_waiter *tail;
  uint32_t waiters;
  uint32_t references;
  struct gta_wait_entry *next;
} gta_wait_entry;

static pthread_mutex_t gta_wait_registry_mutex = PTHREAD_MUTEX_INITIALIZER;
static gta_wait_entry *gta_wait_registry[GTA_WAIT_BUCKET_COUNT];
/* Threads inside the registry wait path; notify skips the registry while it is zero. */
static _Atomic uint32_t gta_wait_registry_waiters;

#if defined(__GNUC__) || defined(__clang__)
__attribute__((noreturn))
#endif
static void gta_wait_fatal(const char *operation, int error) {
  (void)fprintf(stderr, "gta_native_wait: %s failed (%d)\n", operation,
                error);
  abort();
}

#if defined(__ANDROID__) || defined(__linux__)
/* 32-bit waits sleep in the kernel futex on the address itself: one syscall per wait or wake, and a
 * woken thread runs at once instead of blocking again on the entry mutex. Their only engine caller,
 * emscripten's _do_futex_wait, re-checks its value after every return, so the kernel's rare spurious
 * or signal wake is reported as a wake. 64-bit waits keep the registry: Linux futexes are 32-bit.
 * Measured on the S24 (h4b-session): +7.6% average FPS, 13 of 15 benchmark passes faster. */
static _Atomic int gta_wait_futex_mode = 1;

static bool gta_wait_use_futex(void) {
  return atomic_load_explicit(&gta_wait_futex_mode, memory_order_relaxed) == 1;
}

static int32_t gta_wait_futex32(void *address, uint32_t expected,
                                int64_t timeout_ns) {
  struct timespec relative;
  struct timespec *timeout = NULL;
  if (timeout_ns >= 0) {
    relative.tv_sec = (time_t)(timeout_ns / 1000000000);
    relative.tv_nsec = (long)(timeout_ns % 1000000000);
    timeout = &relative;
  }
  if (syscall(SYS_futex, address, FUTEX_WAIT_PRIVATE, expected, timeout, NULL, 0) == 0)
    return GTA_NATIVE_WAIT_WOKEN;
  switch (errno) {
  case EAGAIN:
    return GTA_NATIVE_WAIT_MISMATCH;
  case ETIMEDOUT:
    return GTA_NATIVE_WAIT_TIMED_OUT;
  case EINTR:
    return GTA_NATIVE_WAIT_WOKEN;
  default:
    gta_wait_fatal("futex wait", errno);
  }
}
#endif

#ifdef GTA_NATIVE_WAIT_TESTING
#if defined(__ANDROID__) || defined(__linux__)
void gta_native_wait_test_force_futex(int enabled) {
  atomic_store_explicit(&gta_wait_futex_mode, enabled ? 1 : 0, memory_order_relaxed);
}
#endif
static pthread_mutex_t gta_wait_test_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gta_wait_test_condition = PTHREAD_COND_INITIALIZER;
static bool gta_wait_test_pause_next;
static bool gta_wait_test_paused;
static bool gta_wait_test_resume;

void gta_native_wait_test_pause_next_wake(void) {
  (void)pthread_mutex_lock(&gta_wait_test_mutex);
  gta_wait_test_pause_next = true;
  gta_wait_test_paused = false;
  gta_wait_test_resume = false;
  (void)pthread_mutex_unlock(&gta_wait_test_mutex);
}

void gta_native_wait_test_wait_until_paused(void) {
  (void)pthread_mutex_lock(&gta_wait_test_mutex);
  while (!gta_wait_test_paused)
    (void)pthread_cond_wait(&gta_wait_test_condition, &gta_wait_test_mutex);
  (void)pthread_mutex_unlock(&gta_wait_test_mutex);
}

void gta_native_wait_test_resume(void) {
  (void)pthread_mutex_lock(&gta_wait_test_mutex);
  gta_wait_test_resume = true;
  (void)pthread_cond_broadcast(&gta_wait_test_condition);
  (void)pthread_mutex_unlock(&gta_wait_test_mutex);
}

/* Called after cond_wait reacquires entry->mutex and before grant inspection. */
static void gta_wait_test_pause_before_grant_check(
    pthread_mutex_t *entry_mutex) {
  (void)pthread_mutex_lock(&gta_wait_test_mutex);
  if (!gta_wait_test_pause_next) {
    (void)pthread_mutex_unlock(&gta_wait_test_mutex);
    return;
  }
  gta_wait_test_pause_next = false;
  gta_wait_test_paused = true;
  (void)pthread_cond_broadcast(&gta_wait_test_condition);

  /* Let a late waiter acquire the entry mutex while this waiter is paused. */
  (void)pthread_mutex_unlock(entry_mutex);
  while (!gta_wait_test_resume)
    (void)pthread_cond_wait(&gta_wait_test_condition, &gta_wait_test_mutex);
  gta_wait_test_resume = false;
  (void)pthread_mutex_unlock(&gta_wait_test_mutex);
  (void)pthread_mutex_lock(entry_mutex);
}
#else
static void gta_wait_test_pause_before_grant_check(
    pthread_mutex_t *entry_mutex) {
  (void)entry_mutex;
}
#endif

static size_t gta_wait_hash(const void *memory_identity, const void *address) {
  uintptr_t a = (uintptr_t)memory_identity;
  uintptr_t b = (uintptr_t)address;
  uint64_t x = (uint64_t)a ^ ((uint64_t)b + UINT64_C(0x9e3779b97f4a7c15) +
                              ((uint64_t)a << 6) + ((uint64_t)a >> 2));
  x ^= x >> 30;
  x *= UINT64_C(0xbf58476d1ce4e5b9);
  x ^= x >> 27;
  x *= UINT64_C(0x94d049bb133111eb);
  x ^= x >> 31;
  return (size_t)(x & (GTA_WAIT_BUCKET_COUNT - 1u));
}

static gta_wait_entry *gta_wait_find_locked(const void *memory_identity,
                                            void *address, size_t bucket) {
  for (gta_wait_entry *entry = gta_wait_registry[bucket]; entry;
       entry = entry->next) {
    if (entry->memory_identity == memory_identity && entry->address == address)
      return entry;
  }
  return NULL;
}

static int gta_wait_condition_init(pthread_cond_t *condition,
                                   clockid_t *clock_id) {
  pthread_condattr_t attributes;
  int error = pthread_condattr_init(&attributes);
  if (error != 0)
    return error;

  *clock_id = CLOCK_REALTIME;
#if defined(__linux__) || defined(__ANDROID__)
  if (pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC) == 0)
    *clock_id = CLOCK_MONOTONIC;
#endif

  error = pthread_cond_init(condition, &attributes);
  int destroy_error = pthread_condattr_destroy(&attributes);
  if (error == 0 && destroy_error != 0)
    gta_wait_fatal("condition attributes destroy", destroy_error);
  return error;
}

static gta_wait_entry *gta_wait_create_entry(const void *memory_identity,
                                             void *address, int *failure) {
  gta_wait_entry *entry = (gta_wait_entry *)calloc(1, sizeof(*entry));
  if (!entry) {
    *failure = ENOMEM;
    return NULL;
  }

  entry->memory_identity = memory_identity;
  entry->address = address;
  int error = pthread_mutex_init(&entry->mutex, NULL);
  if (error != 0) {
    *failure = error;
    free(entry);
    return NULL;
  }
  *failure = 0;
  return entry;
}

/* Caller owns one reference. The returned entry stays linked while held. */
static gta_wait_entry *gta_wait_acquire(const void *memory_identity,
                                        void *address, bool create,
                                        int *failure) {
  *failure = 0;
  const size_t bucket = gta_wait_hash(memory_identity, address);

  int error = pthread_mutex_lock(&gta_wait_registry_mutex);
  if (error != 0)
    gta_wait_fatal("registry lock", error);
  gta_wait_entry *entry = gta_wait_find_locked(memory_identity, address, bucket);
  if (entry) {
    ++entry->references;
    error = pthread_mutex_unlock(&gta_wait_registry_mutex);
    if (error != 0)
      gta_wait_fatal("registry unlock", error);
    return entry;
  }
  error = pthread_mutex_unlock(&gta_wait_registry_mutex);
  if (error != 0)
    gta_wait_fatal("registry unlock", error);

  if (!create)
    return NULL;

  gta_wait_entry *candidate =
      gta_wait_create_entry(memory_identity, address, failure);
  if (!candidate)
    return NULL;

  /* Another caller may have created the same key while we initialized it. */
  error = pthread_mutex_lock(&gta_wait_registry_mutex);
  if (error != 0)
    gta_wait_fatal("registry lock", error);
  entry = gta_wait_find_locked(memory_identity, address, bucket);
  if (entry) {
    ++entry->references;
    error = pthread_mutex_unlock(&gta_wait_registry_mutex);
    if (error != 0)
      gta_wait_fatal("registry unlock", error);
    error = pthread_mutex_destroy(&candidate->mutex);
    if (error != 0)
      gta_wait_fatal("candidate mutex destroy", error);
    free(candidate);
    return entry;
  }
  candidate->references = 1;
  candidate->next = gta_wait_registry[bucket];
  gta_wait_registry[bucket] = candidate;
  error = pthread_mutex_unlock(&gta_wait_registry_mutex);
  if (error != 0)
    gta_wait_fatal("registry unlock", error);
  return candidate;
}

/* Release one reference and reclaim idle address entries. */
static void gta_wait_release(gta_wait_entry *entry) {
  bool reclaim = false;
  const size_t bucket =
      gta_wait_hash(entry->memory_identity, entry->address);

  /* No registry lookup holds its mutex while taking entry->mutex. */
  int error = pthread_mutex_lock(&entry->mutex);
  if (error != 0)
    gta_wait_fatal("entry lock", error);
  error = pthread_mutex_lock(&gta_wait_registry_mutex);
  if (error != 0)
    gta_wait_fatal("registry lock", error);
  if (entry->references != 0)
    --entry->references;

  if (entry->references == 0 && entry->waiters == 0 && entry->head == NULL) {
    gta_wait_entry **link = &gta_wait_registry[bucket];
    while (*link && *link != entry)
      link = &(*link)->next;
    if (*link == entry) {
      *link = entry->next;
      reclaim = true;
    }
  }
  error = pthread_mutex_unlock(&gta_wait_registry_mutex);
  if (error != 0)
    gta_wait_fatal("registry unlock", error);
  error = pthread_mutex_unlock(&entry->mutex);
  if (error != 0)
    gta_wait_fatal("entry unlock", error);

  if (reclaim) {
    error = pthread_mutex_destroy(&entry->mutex);
    if (error != 0)
      gta_wait_fatal("entry mutex destroy", error);
    free(entry);
  }
}

static int gta_wait_make_deadline(clockid_t clock_id,
                                  int64_t timeout_ns,
                                  struct timespec *deadline) {
  if (timeout_ns < 0)
    return 0;

  struct timespec now;
  if (clock_gettime(clock_id, &now) != 0)
    return errno == 0 ? EIO : errno;

  const uint64_t timeout = (uint64_t)timeout_ns;
  uint64_t seconds = timeout / UINT64_C(1000000000);
  long nanoseconds = now.tv_nsec + (long)(timeout % UINT64_C(1000000000));
  if (nanoseconds >= 1000000000L) {
    nanoseconds -= 1000000000L;
    ++seconds;
  }

  const uint64_t now_seconds = now.tv_sec < 0 ? 0u : (uint64_t)now.tv_sec;
  if (seconds > (uint64_t)INT64_MAX - now_seconds)
    deadline->tv_sec = (time_t)INT64_MAX;
  else
    deadline->tv_sec = (time_t)(now_seconds + seconds);
  deadline->tv_nsec = nanoseconds;
  return 0;
}

static void gta_wait_enqueue_locked(gta_wait_entry *entry,
                                    gta_wait_waiter *waiter) {
  waiter->next = NULL;
  waiter->granted = false;
  if (entry->tail)
    entry->tail->next = waiter;
  else
    entry->head = waiter;
  entry->tail = waiter;
  ++entry->waiters;
}

static void gta_wait_remove_locked(gta_wait_entry *entry,
                                   gta_wait_waiter *waiter) {
  gta_wait_waiter **link = &entry->head;
  gta_wait_waiter *previous = NULL;
  while (*link && *link != waiter) {
    previous = *link;
    link = &(*link)->next;
  }
  if (*link != waiter)
    gta_wait_fatal("waiter queue removal", EINVAL);
  *link = waiter->next;
  if (entry->tail == waiter)
    entry->tail = previous;
  if (entry->head == NULL)
    entry->tail = NULL;
  if (entry->waiters == 0)
    gta_wait_fatal("waiter count underflow", EINVAL);
  --entry->waiters;
  waiter->next = NULL;
}

static uint64_t gta_wait_load32(void *address) {
  return (uint64_t)atomic_load_explicit(
      (_Atomic volatile uint32_t *)address, memory_order_seq_cst);
}

static uint64_t gta_wait_load64(void *address) {
  return atomic_load_explicit((_Atomic volatile uint64_t *)address,
                              memory_order_seq_cst);
}

typedef uint64_t (*gta_wait_load_fn)(void *address);

static int32_t gta_wait_common(const void *memory_identity, void *address,
                               uint64_t expected, int64_t timeout_ns,
                               gta_wait_load_fn load) {
  /* Bounds and natural alignment are checked by the generated wrapper. */
  if (!memory_identity || !address)
    gta_wait_fatal("invalid wait arguments", EINVAL);

  atomic_fetch_add_explicit(&gta_wait_registry_waiters, 1, memory_order_seq_cst);
  int failure = 0;
  gta_wait_entry *entry =
      gta_wait_acquire(memory_identity, address, true, &failure);
  if (!entry)
    gta_wait_fatal("wait registry allocation", failure == 0 ? EIO : failure);

  int error = pthread_mutex_lock(&entry->mutex);
  if (error != 0)
    gta_wait_fatal("entry lock", error);

  int32_t result = GTA_NATIVE_WAIT_TIMED_OUT;
  gta_wait_waiter waiter;
  clockid_t clock_id;
  error = gta_wait_condition_init(&waiter.condition, &clock_id);
  if (error != 0)
    gta_wait_fatal("waiter condition init", error);
  if (entry->waiters == UINT32_MAX)
    gta_wait_fatal("waiter queue full", EOVERFLOW);
  gta_wait_enqueue_locked(entry, &waiter);

  /* Register before loading. Notify takes this same mutex, so it cannot fall
   * between the value check and queue registration. */
  if (load(address) != expected) {
    gta_wait_remove_locked(entry, &waiter);
    result = GTA_NATIVE_WAIT_MISMATCH;
    goto unlock;
  }

  struct timespec deadline;
  error = gta_wait_make_deadline(clock_id, timeout_ns, &deadline);
  if (error != 0)
    gta_wait_fatal("clock_gettime", error);

  for (;;) {
    if (waiter.granted) {
      gta_wait_remove_locked(entry, &waiter);
      result = GTA_NATIVE_WAIT_WOKEN;
      break;
    }

    if (timeout_ns < 0)
      error = pthread_cond_wait(&waiter.condition, &entry->mutex);
    else
      error = pthread_cond_timedwait(&waiter.condition, &entry->mutex,
                                     &deadline);

    if (error == ETIMEDOUT) {
      /* A notify racing with the deadline wins if it already granted us. */
      if (waiter.granted) {
        gta_wait_remove_locked(entry, &waiter);
        result = GTA_NATIVE_WAIT_WOKEN;
      } else {
        gta_wait_remove_locked(entry, &waiter);
        result = GTA_NATIVE_WAIT_TIMED_OUT;
      }
      break;
    }
    if (error != 0) {
      gta_wait_fatal("condition wait", error);
    }

    gta_wait_test_pause_before_grant_check(&entry->mutex);
  }

unlock:
  error = pthread_mutex_unlock(&entry->mutex);
  if (error != 0)
    gta_wait_fatal("entry unlock", error);
  error = pthread_cond_destroy(&waiter.condition);
  if (error != 0)
    gta_wait_fatal("waiter condition destroy", error);
  gta_wait_release(entry);
  atomic_fetch_sub_explicit(&gta_wait_registry_waiters, 1, memory_order_seq_cst);
  return result;
}

int32_t gta_native_wait32(const void *memory_identity, void *address,
                          uint32_t expected, int64_t timeout_ns) {
#if defined(__ANDROID__) || defined(__linux__)
  if (gta_wait_use_futex()) {
    if (!memory_identity || !address)
      gta_wait_fatal("invalid wait arguments", EINVAL);
    return gta_wait_futex32(address, expected, timeout_ns);
  }
#endif
  return gta_wait_common(memory_identity, address, (uint64_t)expected,
                         timeout_ns, gta_wait_load32);
}

int32_t gta_native_wait64(const void *memory_identity, void *address,
                          uint64_t expected, int64_t timeout_ns) {
  return gta_wait_common(memory_identity, address, expected, timeout_ns,
                         gta_wait_load64);
}

uint32_t gta_native_notify(const void *memory_identity, void *address,
                           uint32_t count) {
  if (!memory_identity || !address)
    gta_wait_fatal("invalid notify arguments", EINVAL);

  uint32_t woken = 0;
#if defined(__ANDROID__) || defined(__linux__)
  if (gta_wait_use_futex()) {
    long result = syscall(SYS_futex, address, FUTEX_WAKE_PRIVATE,
                          count > (uint32_t)INT_MAX ? INT_MAX : (int)count, NULL, NULL, 0);
    if (result < 0)
      gta_wait_fatal("futex wake", errno);
    woken = (uint32_t)result;
    if (count != UINT32_MAX)
      count -= woken;
    if (count == 0 ||
        atomic_load_explicit(&gta_wait_registry_waiters, memory_order_seq_cst) == 0)
      return woken;
  }
#endif

  int failure = 0;
  gta_wait_entry *entry =
      gta_wait_acquire(memory_identity, address, false, &failure);
  if (failure != 0)
    gta_wait_fatal("notify registry lookup", failure);
  if (!entry)
    return woken;

  uint32_t granted = 0;
  int error = pthread_mutex_lock(&entry->mutex);
  if (error != 0)
    gta_wait_fatal("entry lock", error);
  for (gta_wait_waiter *waiter = entry->head; waiter;
       waiter = waiter->next) {
    if (!waiter->granted &&
        (count == UINT32_MAX || granted < count)) {
      waiter->granted = true;
      ++granted;
      error = pthread_cond_signal(&waiter->condition);
      if (error != 0)
        gta_wait_fatal("notify signal", error);
    }
  }

  error = pthread_mutex_unlock(&entry->mutex);
  if (error != 0)
    gta_wait_fatal("entry unlock", error);

  gta_wait_release(entry);
  return woken + granted;
}
