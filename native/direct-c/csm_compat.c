/* Compatibility between the engine globals and shipped PC shadow locals. */
#include "native_host.h"
#include <stdatomic.h>
#include <string.h>

static uint8_t *gta_native_memory_base(const wasm_rt_shared_memory_t *memory) {
  return (uint8_t *)(uintptr_t)memory->data;
}
static uint64_t gta_native_memory_size(const wasm_rt_shared_memory_t *memory) {
  return atomic_load_explicit(&memory->size, memory_order_acquire);
}

static bool gta_native_csm_read(const wasm_rt_shared_memory_t *memory,
                                uint64_t address, void *out, size_t length) {
  uint64_t size = gta_native_memory_size(memory);
  if (address > size || length > size - address) return false;
  memcpy(out, gta_native_memory_base(memory) + address, length);
  return true;
}

void gta_native_csm_locals(w2c_gta5__native *instance, uint64_t shader,
                           uint64_t cloud, uint64_t lookup, uint64_t params) {
  /* The engine sets gCSM globals, but the shipped PC effect has these locals.
   * Use the engine setters so its texture references and dirty flags stay valid.
   * Require the complete signature before touching any instance data. */
  const wasm_rt_shared_memory_t *memory = instance->w2c_env_memory;
  uint64_t effect, table;
  uint16_t count;
  unsigned char header[32], values[16];
  if (shader == 0 || !gta_native_csm_read(memory, shader, header, 16)) return;
  memcpy(&effect, header + 8, 8);
  if (effect == 0 || !gta_native_csm_read(memory, effect, header, 26)) return;
  memcpy(&table, header + 16, 8);
  memcpy(&count, header + 24, 2);
  uint64_t size = gta_native_memory_size(memory);
  if (table == 0 || count < 3 || count > 256 || table > size ||
      (uint64_t)count * 72 > size - table ||
      !gta_native_csm_read(memory, params, values, sizeof(values))) return;
  const uint32_t hashes[3] = {0xf743ac32u, 0xeeef140eu, 0x88b40761u};
  uint32_t handles[3] = {0};
  for (uint32_t n = 0; n < count; ++n) {
    const uint8_t *entry = gta_native_memory_base(memory) + table + n * 72;
    uint32_t name, semantic;
    memcpy(&name, entry + 24, 4);
    memcpy(&semantic, entry + 28, 4);
    for (unsigned target = 0; target < 3; ++target) {
      if (name != hashes[target] && semantic != hashes[target]) continue;
      if (entry[0] != (target == 2 ? 5 : 6)) return;
      handles[target] = n + 1;
    }
  }
  if (!handles[0] || !handles[1] || !handles[2]) return;
  extern void w2c_gta5__native_f9814(w2c_gta5__native *, u64, u32, u64);
  extern void w2c_gta5__native_f9875(w2c_gta5__native *, u64, u32, u64, u32, u32);
  w2c_gta5__native_f9814(instance, shader, handles[0], cloud);
  w2c_gta5__native_f9814(instance, shader, handles[1], lookup);
  w2c_gta5__native_f9875(instance, shader, handles[2], params, 16, 1);
}

