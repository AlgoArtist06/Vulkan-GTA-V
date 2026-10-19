/* Bit-exact fast path for the Opus SILK eight-tap interpolation loop. */
#include "native_host.h"
#include <stdatomic.h>
#include <string.h>
#if defined(__aarch64__)
#include <arm_neon.h>
#endif

static bool range(uint64_t address, uint64_t bytes, uint64_t size) {
  return address <= size && bytes <= size - address;
}
static bool overlaps(uint64_t a, uint64_t an, uint64_t b, uint64_t bn) {
  return a < b + bn && b < a + an;
}

bool gta_native_opus_interpolate(w2c_gta5__native *instance, uint64_t output,
                                 uint64_t input, uint32_t limit, uint32_t step,
                                 uint64_t *output_end) {
  const uint64_t coefficients = 6430176u;
  if (!limit || limit > INT32_MAX || !step || step > INT32_MAX - limit ||
      ((input | output) & 1u)) return false;
  uint64_t count = ((uint64_t)limit + step - 1) / step;
  if (count > 65536) return false;
  uint64_t input_bytes = (((uint64_t)limit - 1) / 65536 + 8) * 2;
  uint64_t output_bytes = count * 2;
  const wasm_rt_shared_memory_t *memory = instance->w2c_env_memory;
  uint64_t size = atomic_load_explicit(&memory->size, memory_order_acquire);
  if (!range(input, input_bytes, size) || !range(output, output_bytes, size) ||
      !range(coefficients, 96, size) ||
      overlaps(input, input_bytes, output, output_bytes) ||
      overlaps(output, output_bytes, coefficients, 96)) return false;
  uint8_t *base = (uint8_t *)(uintptr_t)memory->data;
  const int16_t *table = (const int16_t *)(base + coefficients);
  int16_t *destination = (int16_t *)(base + output);
  uint64_t index = 0;
  for (uint64_t n = 0; n < count; ++n, index += step) {
    unsigned phase = ((index & 65535u) * 12) >> 16;
    const int16_t *samples = (const int16_t *)(base + input) + (index >> 16);
    const int16_t *first = table + phase * 4;
    const int16_t *second = table + (11 - phase) * 4;
    int32_t sum;
#if defined(__aarch64__)
    int32x4_t products = vmull_s16(vld1_s16(samples), vld1_s16(first));
    products = vmlal_s16(products, vld1_s16(samples + 4), vrev64_s16(vld1_s16(second)));
    sum = vaddvq_s32(products);
#else
    uint32_t total = 0;
    for (unsigned k = 0; k < 4; ++k) {
      total += (uint32_t)((int32_t)samples[k] * first[k]);
      total += (uint32_t)((int32_t)samples[k + 4] * second[3 - k]);
    }
    memcpy(&sum, &total, sizeof(sum));
#endif
    /* Match the engine's wrapping i32 sum and two arithmetic shifts exactly. */
    int32_t sample = ((sum >> 14) + 1) >> 1;
    if (sample < -32768) sample = -32768;
    if (sample > 32767) sample = 32767;
    destination[n] = (int16_t)sample;
  }
  *output_end = output + output_bytes;
  return true;
}
