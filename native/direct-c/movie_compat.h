#ifndef GTA_MOVIE_COMPAT_H
#define GTA_MOVIE_COMPAT_H
#include "game.h"
#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdatomic.h>

typedef struct GtaMovieManager GtaMovieManager;
typedef int (*GtaMovieFrameCallback)(void *,uint32_t,uint8_t *,size_t,uint64_t *,uint32_t *,uint32_t *);
GtaMovieManager *gta_movie_manager_create(wasm_rt_shared_memory_t *,const char *assets_root);
/* Configure before publishing the manager; the session owns this live flag. */
void gta_movie_manager_stop_flag(GtaMovieManager *,const _Atomic bool *);
/* Stop decoder workers after the GPU and engine threads have stopped. */
void gta_movie_manager_destroy(GtaMovieManager *);
void gta_movie_manager_suspend(GtaMovieManager *,int suspended);
uint32_t gta_movie_manager_call(GtaMovieManager *,w2c_gta5__native *,uint32_t operation,uint64_t a,uint64_t b);
/* Renderer callback: returns new RGBA pixels, never exposes a decoder-owned pointer. */
int gta_movie_manager_frame(void *,uint32_t hash,uint8_t *,size_t,uint64_t *,uint32_t *,uint32_t *);
uint32_t gta_native_movie_call(w2c_gta5__native *,uint32_t operation,uint64_t a,uint64_t b);
uint32_t gta_native_movie_begin_draw(w2c_gta5__native *,uint32_t hash);
void gta_native_movie_end_draw(w2c_gta5__native *);
#endif
