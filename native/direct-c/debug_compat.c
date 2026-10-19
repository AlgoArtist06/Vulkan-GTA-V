/* Keep developer shortcuts out of text entry and normal gameplay. */
#include "native_host.h"
#include <stdatomic.h>
#include <string.h>

static bool read_guest(w2c_gta5__native *instance, uint64_t address,
                       void *value, size_t length) {
  const wasm_rt_shared_memory_t *memory = instance->w2c_env_memory;
  uint64_t size = atomic_load_explicit(&memory->size, memory_order_acquire);
  if (address > size || length > size - address) return false;
  memcpy(value, (const uint8_t *)(uintptr_t)memory->data + address, length);
  return true;
}

bool gta_native_debug_text_entry(w2c_gta5__native *instance) {
  /* CTextInputBox::Open sets this; Close clears it. Fail closed before init. */
  uint8_t active;
  return !read_guest(instance, 12838657u, &active, sizeof(active)) || active != 0;
}

bool gta_native_debug_flight_allowed(w2c_gta5__native *instance) {
  uint32_t mode;
  uint64_t physics;
  if (!read_guest(instance, 24048108u, &mode, sizeof(mode))) return false;
  if (mode == 0 && !gta_native_debug_text_entry(instance)) return true;
  /* CDebug::Update also accepts I in Game mode. Require deliberate Debug mode.
   * Restore the original flags through the engine, never overwrite physics bits. */
  if (read_guest(instance, 19352560u, &physics, sizeof(physics)) && physics != 0) {
    extern void w2c_gta5__native_f29313(w2c_gta5__native *);
    w2c_gta5__native_f29313(instance);
  }
  return false;
}
