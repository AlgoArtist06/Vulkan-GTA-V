#ifndef GTA_MOVIE_PIXELS_H
#define GTA_MOVIE_PIXELS_H
#include <stddef.h>
#include <stdint.h>

typedef struct {
  const uint8_t *data;
  size_t bytes, row_stride, pixel_stride;
} GtaMoviePlane;

/* Android YUV_420_888 planes may overlap and have padded rows. Crop coordinates
 * refer to the complete image. Output is tightly packed RGBA, with opaque alpha. */
int gta_movie_yuv_rgba(const GtaMoviePlane planes[3], uint32_t width, uint32_t height,
                       uint32_t left, uint32_t top, uint32_t crop_width, uint32_t crop_height,
                       int bt709, int full_range, uint8_t *rgba, size_t capacity);
#endif
