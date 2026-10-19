#ifndef GTA_MOVIE_DECODER_H
#define GTA_MOVIE_DECODER_H
#include <stddef.h>
#include <stdint.h>

typedef struct GtaMovieDecoder GtaMovieDecoder;
typedef void (*GtaMovieNotify)(void *,GtaMovieDecoder *);
typedef struct {
  uint32_t width, height;
  uint64_t frame_serial;
  int64_t duration_us, position_us, frame_pts_us;
  uint64_t audio_frames;
  int has_audio;
  int ready, playing, finished, failed;
  char error[192];
} GtaMovieStatus;

/* Owns one bounded native decoder worker. Opening is asynchronous. The caller
 * must close it before its session is destroyed. No guest pointers are retained. */
GtaMovieDecoder *gta_movie_decoder_open(const char *path);
void gta_movie_decoder_control(GtaMovieDecoder *, int playing, int loop, int64_t seek_us);
void gta_movie_decoder_volume(GtaMovieDecoder *, float left, float right);
/* Called outside decoder locks on progress. Clear ownership only after close. */
void gta_movie_decoder_notify(GtaMovieDecoder *,GtaMovieNotify,void *);
void gta_movie_decoder_status(GtaMovieDecoder *, GtaMovieStatus *);
/* Returns 1 for a new frame, 0 if unchanged/not ready, -1 for a short buffer. */
int gta_movie_decoder_copy(GtaMovieDecoder *, uint8_t *rgba, size_t capacity, uint64_t *serial);
void gta_movie_decoder_close(GtaMovieDecoder *);
#endif
