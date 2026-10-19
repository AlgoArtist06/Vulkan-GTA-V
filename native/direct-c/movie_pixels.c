#include "movie_pixels.h"

static int plane_valid(const GtaMoviePlane *p, uint32_t row, uint32_t column) {
  if (!p->data || !p->row_stride || !p->pixel_stride ||
      p->row_stride > (1u << 24) || p->pixel_stride > 16) return 0;
  uint64_t offset = (uint64_t)row * p->row_stride + (uint64_t)column * p->pixel_stride;
  return offset < p->bytes;
}

static uint8_t channel(int value) {
  if (value <= 0) return 0;
  if (value >= 255 * 256) return 255;
  return (uint8_t)((value + 128) / 256);
}

int gta_movie_yuv_rgba(const GtaMoviePlane p[3], uint32_t width, uint32_t height,
                       uint32_t left, uint32_t top, uint32_t cw, uint32_t ch,
                       int bt709, int full_range, uint8_t *out, size_t capacity) {
  if (!p || !out || !width || !height || width > 4096 || height > 4096 ||
      !cw || !ch || left >= width || top >= height || cw > width-left || ch > height-top ||
      (uint64_t)cw * ch * 4 > capacity) return -1;
  uint32_t last_x = left+cw-1, last_y = top+ch-1;
  if (!plane_valid(&p[0],last_y,last_x) || !plane_valid(&p[1],last_y/2,last_x/2) ||
      !plane_valid(&p[2],last_y/2,last_x/2)) return -1;
  if (p[0].row_stride < (uint64_t)(width-1)*p[0].pixel_stride+1 ||
      p[1].row_stride < (uint64_t)((width-1)/2)*p[1].pixel_stride+1 ||
      p[2].row_stride < (uint64_t)((width-1)/2)*p[2].pixel_stride+1) return -1;
  const int yc = full_range ? 256 : 298;
  const int rv = full_range ? (bt709 ? 403 : 359) : (bt709 ? 459 : 409);
  const int gu = full_range ? (bt709 ? 48 : 88) : (bt709 ? 55 : 100);
  const int gv = full_range ? (bt709 ? 120 : 183) : (bt709 ? 136 : 208);
  const int bu = full_range ? (bt709 ? 475 : 454) : (bt709 ? 541 : 516);
  for (uint32_t y=0; y<ch; ++y) {
    const uint32_t sy=top+y;
    const uint8_t *yp=p[0].data+(size_t)sy*p[0].row_stride;
    const uint8_t *up=p[1].data+(size_t)(sy/2)*p[1].row_stride;
    const uint8_t *vp=p[2].data+(size_t)(sy/2)*p[2].row_stride;
    for (uint32_t x=0; x<cw; ++x) {
      const uint32_t sx=left+x;
      int luma=((int)yp[(size_t)sx*p[0].pixel_stride]-(full_range?0:16))*yc;
      int u=(int)up[(size_t)(sx/2)*p[1].pixel_stride]-128;
      int v=(int)vp[(size_t)(sx/2)*p[2].pixel_stride]-128;
      *out++=channel(luma+rv*v); *out++=channel(luma-gu*u-gv*v);
      *out++=channel(luma+bu*u); *out++=255;
    }
  }
  return 0;
}
