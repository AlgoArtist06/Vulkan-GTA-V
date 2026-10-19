/* Native implementation of the shipped movie backend's verified CMovie ABI. */
#include "movie_compat.h"
#include "movie_decoder.h"
#include "movie_names.h"
#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#ifdef __ANDROID__
#include <android/log.h>
#define MOVIE_LOG(...) ((void)__android_log_print(ANDROID_LOG_INFO,"GtaNativeMovie",__VA_ARGS__))
#else
#define MOVIE_LOG(...) ((void)0)
#endif

#define MOVIE_TABLE 28366640u
#define MOVIE_COUNT 28366648u
#define MOVIE_STRIDE 160u
#define MOVIE_MAGIC 0x4156314du
#define MOVIE_LIMIT 16u
extern u64 w2c_gta5__native_f100(w2c_gta5__native *,u64);
extern void w2c_gta5__native_f102(w2c_gta5__native *,u64);
extern u64 w2c_gta5__native_f10879(w2c_gta5__native *);
extern u64 w2c_gta5__native_f10883(w2c_gta5__native *,u64);
extern u32 w2c_gta5__native_f2740(w2c_gta5__native *,u64,u32);
extern void w2c_gta5__native_f6987(w2c_gta5__native *,u64,u64);

typedef struct {
  uint32_t hash,generation;
  uint64_t movie,bw,fake;
  GtaMovieDecoder *decoder;
  int pending_seek_kind;
  float pending_seek_value;
  int reported_ready,reported_failure;
} Movie;
typedef struct { GtaMovieManager *manager;unsigned slot; } MovieNotifyRef;
struct GtaMovieManager {
  wasm_rt_shared_memory_t *memory;
  pthread_mutex_t mutex;
  char *assets_root;
  uint32_t generation;
  int suspended;
  Movie slots[MOVIE_LIMIT];
  MovieNotifyRef notify[MOVIE_LIMIT];
  const _Atomic bool *stop_requested;
#ifdef __ANDROID__
  _Atomic uint32_t seen_operations;
#endif
};

static int stopped(GtaMovieManager *m) {return m->stop_requested&&atomic_load_explicit(m->stop_requested,memory_order_acquire);}

static uint8_t *span(GtaMovieManager *m,uint64_t p,size_t size) {
  uint64_t limit=atomic_load_explicit(&m->memory->size,memory_order_acquire);
  if(!p||p>limit||size>limit-p)return NULL;
  return (uint8_t *)(uintptr_t)m->memory->data+p;
}
static uint32_t read32(GtaMovieManager *m,uint64_t p) {
  uint8_t *v=span(m,p,4);return v&&!(p&3)?atomic_load_explicit((_Atomic uint32_t *)v,memory_order_acquire):0;
}
static uint64_t read64(GtaMovieManager *m,uint64_t p) {
  uint8_t *v=span(m,p,8);return v&&!(p&7)?atomic_load_explicit((_Atomic uint64_t *)v,memory_order_acquire):0;
}
static uint8_t read8(GtaMovieManager *m,uint64_t p) {
  uint8_t *v=span(m,p,1);return v?atomic_load_explicit((_Atomic uint8_t *)v,memory_order_acquire):0;
}
static void write32(GtaMovieManager *m,uint64_t p,uint32_t value) {
  uint8_t *v=span(m,p,4);if(v&&!(p&3))atomic_store_explicit((_Atomic uint32_t *)v,value,memory_order_release);
}
static void write64(GtaMovieManager *m,uint64_t p,uint64_t value) {
  uint8_t *v=span(m,p,8);if(v&&!(p&7))atomic_store_explicit((_Atomic uint64_t *)v,value,memory_order_release);
}
static void write8(GtaMovieManager *m,uint64_t p,uint8_t value) {
  uint8_t *v=span(m,p,1);if(v)atomic_store_explicit((_Atomic uint8_t *)v,value,memory_order_release);
}
static float read_float(GtaMovieManager *m,uint64_t p) {uint32_t b=read32(m,p);float f;memcpy(&f,&b,4);return f;}
static void write_float(GtaMovieManager *m,uint64_t p,float f) {uint32_t b;memcpy(&b,&f,4);write32(m,p,b);}
static Movie *find(GtaMovieManager *m,uint32_t hash) {
  if(!hash)return NULL;for(unsigned i=0;i<MOVIE_LIMIT;i++)if(m->slots[i].hash==hash)return &m->slots[i];return NULL;
}
static Movie *by_movie(GtaMovieManager *m,uint64_t movie) {
  if(!movie)return NULL;for(unsigned i=0;i<MOVIE_LIMIT;i++)if(m->slots[i].movie==movie&&m->slots[i].hash)return &m->slots[i];return NULL;
}
static int movie_name(GtaMovieManager *m,uint64_t p,char out[64]) {
  char text[1024];size_t length=0;
  for(;length<sizeof(text);length++) {
    if(p>UINT64_MAX-length||!span(m,p+length,1))return 0;
    text[length]=(char)read8(m,p+length);if(!text[length])break;
  }
  if(length==sizeof(text))return 0;
  const char *name=text;
  for(size_t i=0;i<length;i++)if(text[i]=='/'||text[i]=='\\')name=text+i+1;
  size_t n=strlen(name);if(!n||n>=64)return 0;
  for(size_t i=0;i<=n;i++)out[i]=name[i]>='A'&&name[i]<='Z'?(char)(name[i]+32):name[i];
  char *dot=strrchr(out,'.');
  if(dot&&(strcmp(dot,".bik")==0||strcmp(dot,".bk2")==0||strcmp(dot,".mp4")==0))*dot=0;
  for(size_t i=0;i<sizeof(gta_movie_names)/sizeof(*gta_movie_names);i++)if(strcmp(out,gta_movie_names[i])==0)return 1;
  return 0;
}

GtaMovieManager *gta_movie_manager_create(wasm_rt_shared_memory_t *memory,const char *root) {
  if(!memory||!root||strlen(root)>3500)return NULL;
  GtaMovieManager *m=calloc(1,sizeof(*m));if(!m)return NULL;
  m->assets_root=malloc(strlen(root)+1);if(!m->assets_root){free(m);return NULL;}
  strcpy(m->assets_root,root);m->memory=memory;
  if(pthread_mutex_init(&m->mutex,NULL)){free(m->assets_root);free(m);return NULL;}
  for(unsigned i=0;i<MOVIE_LIMIT;i++)m->notify[i]=(MovieNotifyRef){m,i};
  return m;
}
void gta_movie_manager_destroy(GtaMovieManager *m) {
  if(!m)return;
  /* Engine and GPU workers are already stopped. Guest allocations disappear
   * with their whole heap; only independent platform workers need joining. */
  for(unsigned i=0;i<MOVIE_LIMIT;i++)if(m->slots[i].decoder)gta_movie_decoder_close(m->slots[i].decoder);
  pthread_mutex_destroy(&m->mutex);free(m->assets_root);free(m);
}
void gta_movie_manager_stop_flag(GtaMovieManager *m,const _Atomic bool *flag) {if(m)m->stop_requested=flag;}
void gta_movie_manager_suspend(GtaMovieManager *m,int suspended) {
  if(!m)return;
  pthread_mutex_lock(&m->mutex);m->suspended=!!suspended;
  if(suspended)for(unsigned i=0;i<MOVIE_LIMIT;i++)if(m->slots[i].decoder)
    gta_movie_decoder_control(m->slots[i].decoder,0,m->slots[i].bw?read8(m,m->slots[i].bw+174):0,-1);
  pthread_mutex_unlock(&m->mutex);
}

static void close_movie(GtaMovieManager *m,w2c_gta5__native *instance,uint32_t hash) {
  pthread_mutex_lock(&m->mutex);Movie *entry=find(m,hash);
  if(!entry){pthread_mutex_unlock(&m->mutex);return;}
  Movie saved=*entry;memset(entry,0,sizeof(*entry));
  MOVIE_LOG("close hash=%08x movie=%llu bw=%llu fake=%llu",hash,
      (unsigned long long)saved.movie,(unsigned long long)saved.bw,(unsigned long long)saved.fake);
  if(read64(m,saved.movie)==saved.bw) {
    uint8_t *record=span(m,saved.movie,MOVIE_STRIDE);if(record)memset(record,0,MOVIE_STRIDE);
  }
  if(saved.fake)write32(m,saved.fake+36,0);
  if(saved.bw)write64(m,saved.bw,0);
  for(unsigned i=0;i<8;i++)if(read32(m,28366608u+i*4)==hash)write32(m,28366608u+i*4,0);
  pthread_mutex_unlock(&m->mutex);
  if(saved.decoder)gta_movie_decoder_close(saved.decoder);
  /* Guest calls occur without the host lock: an allocation trap can unwind. */
  if(saved.fake)w2c_gta5__native_f102(instance,saved.fake);
  if(saved.bw){w2c_gta5__native_f10883(instance,saved.bw);w2c_gta5__native_f102(instance,saved.bw);}
}

static void publish_status(GtaMovieManager *m,Movie *entry,const GtaMovieStatus *s) {
  if(!entry->fake||!entry->bw||read64(m,entry->movie)!=entry->bw||read64(m,entry->bw)!=entry->fake||
      read32(m,entry->movie+24)!=entry->hash||read32(m,entry->fake+36)!=MOVIE_MAGIC||
      read32(m,entry->fake+44)!=entry->generation)return;
  if(s->ready&&!entry->reported_ready) {
    entry->reported_ready=1;
    MOVIE_LOG("ready hash=%08x size=%ux%u",entry->hash,s->width,s->height);
  }
  if(s->failed&&!entry->reported_failure) {
    entry->reported_failure=1;MOVIE_LOG("failed hash=%08x reason=%s",entry->hash,s->error);
  }
  if(s->width&&s->height){write32(m,entry->fake,s->width);write32(m,entry->fake+4,s->height);}
  if(s->duration_us>0)write32(m,entry->fake+8,(uint32_t)fmax(1,round((double)s->duration_us*30000/1001000000.0)));
  write32(m,entry->fake+12,(uint32_t)fmin(read32(m,entry->fake+8),fmax(0,round((double)s->position_us*30000/1001000000.0))));
  write8(m,entry->bw+171,(uint8_t)(s->ready||s->failed));
  /* Only the engine owner changes CMovie flags. A worker publishes completion
   * through the dedicated BW/fake fields, so it cannot overwrite Release flags. */
  if(s->finished||s->failed){write32(m,entry->fake+32,1);write8(m,entry->bw+172,0);}
}

static void apply_seek(GtaMovieManager *m,Movie *entry,const GtaMovieStatus *s) {
  if(!entry->pending_seek_kind||s->duration_us<=0||s->failed)return;
  double value=entry->pending_seek_value;
  double target=entry->pending_seek_kind==1?(double)s->duration_us*fmax(0,fmin(100,value))/100:fmax(0,fmin(86400,value))*1000000.0;
  if(target>s->duration_us)target=(double)s->duration_us;
  uint32_t flags=read32(m,entry->movie+96);
  gta_movie_decoder_control(entry->decoder,!m->suspended&&!s->finished&&(flags&2)&&!(flags&(4|64|128)),
      read8(m,entry->bw+174),(int64_t)target);
  write32(m,entry->fake+32,0);entry->pending_seek_kind=0;
}

static void decoded_progress(void *context,GtaMovieDecoder *decoder) {
  MovieNotifyRef *ref=context;GtaMovieManager *m=ref->manager;
  if(stopped(m)){gta_movie_decoder_control(decoder,0,0,-1);return;}
  pthread_mutex_lock(&m->mutex);Movie *entry=&m->slots[ref->slot];
  if(!stopped(m)&&entry->hash&&entry->decoder==decoder&&entry->fake) {
    GtaMovieStatus status={0};gta_movie_decoder_status(decoder,&status);
    apply_seek(m,entry,&status);gta_movie_decoder_status(decoder,&status);publish_status(m,entry,&status);
  }
  pthread_mutex_unlock(&m->mutex);
}

static uint32_t open_movie(GtaMovieManager *m,w2c_gta5__native *instance,uint64_t name_pointer) {
  if(stopped(m))return 0;
  char name[64],path[4096];if(!movie_name(m,name_pointer,name))return 0;
  int n=snprintf(path,sizeof(path),"%s/../_movie/av1/%s.mp4",m->assets_root,name);
  struct stat file;if(n<0||(size_t)n>=sizeof(path)||stat(path,&file)||!S_ISREG(file.st_mode)||file.st_size<=0)return 0;
  uint32_t hash=w2c_gta5__native_f2740(instance,name_pointer,0);if(!hash)return 0;
  pthread_mutex_lock(&m->mutex);
  Movie *entry=find(m,hash);
  if(entry) {
    uint32_t refs=read32(m,entry->movie+100);
    if(refs==UINT32_MAX){pthread_mutex_unlock(&m->mutex);return 0;}
    write32(m,entry->movie+100,refs+1);write32(m,entry->movie+96,read32(m,entry->movie+96)&~4u);
    pthread_mutex_unlock(&m->mutex);return hash;
  }
  uint64_t table=read64(m,MOVIE_TABLE);uint8_t *count_pointer=span(m,MOVIE_COUNT,2);
  uint16_t count=count_pointer?atomic_load_explicit((_Atomic uint16_t *)count_pointer,memory_order_acquire):0;
  uint64_t movie=0;
  if(table&&!(table&7)&&count<=64&&span(m,table,(size_t)count*MOVIE_STRIDE))for(unsigned i=0;i<count;i++) {
    uint64_t candidate=table+(uint64_t)i*MOVIE_STRIDE;int reserved=0;
    for(unsigned j=0;j<MOVIE_LIMIT;j++)if(m->slots[j].movie==candidate)reserved=1;
    if(!reserved&&!read64(m,candidate)){movie=candidate;break;}
  }
  for(unsigned i=0;i<MOVIE_LIMIT&&!entry;i++)if(!m->slots[i].movie)entry=&m->slots[i];
  if(!movie||!entry){pthread_mutex_unlock(&m->mutex);return 0;}
  if(++m->generation==0)++m->generation;
  *entry=(Movie){.hash=hash,.generation=m->generation,.movie=movie};
  pthread_mutex_unlock(&m->mutex);
  GtaMovieDecoder *decoder=gta_movie_decoder_open(path);
  if(decoder)gta_movie_decoder_volume(decoder,0,0);
  pthread_mutex_lock(&m->mutex);entry->decoder=decoder;pthread_mutex_unlock(&m->mutex);
  if(!decoder){close_movie(m,instance,hash);return 0;}
  uint64_t bw=w2c_gta5__native_f10879(instance);
  pthread_mutex_lock(&m->mutex);entry->bw=bw;pthread_mutex_unlock(&m->mutex);
  uint64_t fake=bw?w2c_gta5__native_f100(instance,128):0;
  pthread_mutex_lock(&m->mutex);entry->fake=fake;
  if(!span(m,bw,256)||!span(m,fake,128)){pthread_mutex_unlock(&m->mutex);close_movie(m,instance,hash);return 0;}
  memset(span(m,fake,128),0,128);write32(m,fake,640);write32(m,fake+4,360);write32(m,fake+8,1);
  write32(m,fake+20,30000);write32(m,fake+24,1001);write32(m,fake+36,MOVIE_MAGIC);write32(m,fake+44,entry->generation);
  write64(m,bw,fake);write8(m,bw+171,0);write8(m,bw+172,0);write8(m,bw+173,0);write8(m,bw+174,0);
  write8(m,bw+177,1);write8(m,bw+241,0);write32(m,bw+216,30000);write32(m,bw+220,1001);
  memset(span(m,movie,MOVIE_STRIDE),0,MOVIE_STRIDE);
  memcpy(span(m,movie+28,64),name,strlen(name)+1);write32(m,movie+100,1);write32(m,movie+116,UINT32_MAX);
  write64(m,movie,bw);write32(m,movie+24,hash);
  unsigned slot=(unsigned)(entry-m->slots);
  MOVIE_LOG("open name=%s hash=%08x slot=%u movie=%llu bw=%llu fake=%llu",name,hash,slot,
      (unsigned long long)movie,(unsigned long long)bw,(unsigned long long)fake);
  pthread_mutex_unlock(&m->mutex);
  gta_movie_decoder_notify(decoder,decoded_progress,&m->notify[slot]);
  return hash;
}

static void set_audio(GtaMovieManager *m,w2c_gta5__native *instance,Movie *entry) {
  uint64_t c=entry->movie,fake=entry->fake;float db=read_float(m,fake+48),sfx=read_float(m,6748144);
  float left=0,right=0;
  if(isfinite(db)&&isfinite(sfx)) {
    float gain=fmaxf(0,fminf(1,sfx))*powf(10,fmaxf(-100,fminf(0,db))/20);
    if(read8(m,c+104))left=right=gain;
    else {
      uint64_t entity=read64(m,c+16);
      if(span(m,entity,160)) {
        if(read8(m,entity+40)==4&&span(m,entity,5584)&&(read8(m,entity+5291)&64)) {
          uint64_t target=read64(m,entity+5576);if(target)entity=target;
        }
        if(span(m,entity,160)) {
          float xyz[4]={read_float(m,entity+144),read_float(m,entity+148),read_float(m,entity+152),0};
          if(isfinite(xyz[0])&&isfinite(xyz[1])&&isfinite(xyz[2])&&fabsf(xyz[0])<=100000&&fabsf(xyz[1])<=100000&&fabsf(xyz[2])<=100000) {
            memcpy(span(m,fake+96,16),xyz,16);
            /* UpdateFrame runs on the engine owner; this transform shares its listener. */
            w2c_gta5__native_f6987(instance,fake+64,fake+96);
            float x=read_float(m,fake+64),y=read_float(m,fake+68),z=read_float(m,fake+72);
            if(isfinite(x)&&isfinite(y)&&isfinite(z)) {
              float distance=sqrtf(x*x+y*y+z*z),pan=x/fmaxf(1,sqrtf(x*x+y*y));
              gain*=fmaxf(0,fminf(1,(40-distance)/15))/fmaxf(1,distance);
              left=gain*sqrtf(fmaxf(0,1-pan));right=gain*sqrtf(fmaxf(0,1+pan));
            }
          }
        }
      }
    }
  }
  gta_movie_decoder_volume(entry->decoder,left,right);
}

static void tick(GtaMovieManager *m,w2c_gta5__native *instance) {
  for(unsigned i=0;i<MOVIE_LIMIT;i++) {
    pthread_mutex_lock(&m->mutex);Movie *entry=&m->slots[i];
    if(!entry->hash||!entry->decoder||!entry->fake){pthread_mutex_unlock(&m->mutex);continue;}
    uint32_t hash=entry->hash,flags=read32(m,entry->movie+96);
    if(flags&4){pthread_mutex_unlock(&m->mutex);close_movie(m,instance,hash);continue;}
    GtaMovieStatus s={0};gta_movie_decoder_status(entry->decoder,&s);
    publish_status(m,entry,&s);
    if(s.finished||s.failed){write32(m,entry->fake+32,1);write32(m,entry->movie+96,flags&~2u);write8(m,entry->bw+172,0);}
    int play=(flags&2)&&!(flags&(4|64|128))&&!read32(m,entry->fake+32);
    Movie snapshot=*entry;
    pthread_mutex_unlock(&m->mutex);
    /* Guest transforms can trap. Keep host locks out of their unwind path.
     * UpdateFrame and movie deletion share the engine owner; GPU callers only
     * copy independent decoded pixels and cannot delete this record. */
    set_audio(m,instance,&snapshot);
    pthread_mutex_lock(&m->mutex);
    entry=find(m,hash);
    if(entry&&entry->generation==snapshot.generation)
      gta_movie_decoder_control(entry->decoder,!m->suspended&&play,read8(m,entry->bw+174),-1);
    pthread_mutex_unlock(&m->mutex);
  }
}

uint32_t gta_movie_manager_call(GtaMovieManager *m,w2c_gta5__native *instance,uint32_t op,uint64_t a,uint64_t b) {
  if(!m||!instance)return 0;
#ifdef __ANDROID__
  /* At most one line per operation per session, including the per-frame Tick. */
  if(op<32) {
    uint32_t bit=1u<<op;
    if(!(atomic_load_explicit(&m->seen_operations,memory_order_relaxed)&bit)&&
        !(atomic_fetch_or_explicit(&m->seen_operations,bit,memory_order_relaxed)&bit))
      MOVIE_LOG("first operation=%u a=%llu b=%llu",op,(unsigned long long)a,(unsigned long long)b);
  }
#endif
  if(stopped(m)&&op!=3&&op!=4)return 0;
  if(op==1)return open_movie(m,instance,a);
  if(op==2){tick(m,instance);return 0;}
  if(op==3){close_movie(m,instance,(uint32_t)a);return 0;}
  if(op==4){for(unsigned i=0;i<MOVIE_LIMIT;i++){uint32_t hash=m->slots[i].hash;if(hash)close_movie(m,instance,hash);}return 0;}
  pthread_mutex_lock(&m->mutex);
  Movie *entry=(op==5||op==9)?by_movie(m,a):find(m,(uint32_t)a);
  if(op==11){entry=NULL;for(unsigned i=0;i<MOVIE_LIMIT;i++)if(m->slots[i].bw==a&&m->slots[i].hash){entry=&m->slots[i];break;}}
  if(!entry||!entry->decoder||!entry->fake){pthread_mutex_unlock(&m->mutex);return 0;}
  uint32_t answer=0,flags=read32(m,entry->movie+96);
  if(op==5) {
    GtaMovieStatus s={0};gta_movie_decoder_status(entry->decoder,&s);
    if(s.failed)publish_status(m,entry,&s);
    else {
      write32(m,entry->fake+32,0);write32(m,entry->movie+96,(flags&~(1u|64u|128u))|2u);write8(m,entry->bw+172,1);
      gta_movie_decoder_control(entry->decoder,!m->suspended,read8(m,entry->bw+174),s.finished?0:-1);
    }
  } else if(op==6||op==7||op==11) {
    uint32_t bits=(uint32_t)b;float value;memcpy(&value,&bits,4);
    if(isfinite(value)) {
      GtaMovieStatus s={0};gta_movie_decoder_status(entry->decoder,&s);
      entry->pending_seek_kind=op==6?1:2;entry->pending_seek_value=value;
      apply_seek(m,entry,&s);
    }
  } else if(op==8)answer=read8(m,entry->bw+171)&&!(flags&(4|64|128))&&!read32(m,entry->fake+32);
  else if(op==12) {
    GtaMovieStatus s={0};gta_movie_decoder_status(entry->decoder,&s);publish_status(m,entry,&s);
    answer=!!(flags&2)&&!s.finished&&!s.failed;
  }
  else if(op==9) {uint32_t bits=(uint32_t)b;float value;memcpy(&value,&bits,4);if(isfinite(value)){write_float(m,entry->fake+48,value);write_float(m,entry->movie+92,value);}}
  pthread_mutex_unlock(&m->mutex);return answer;
}

int gta_movie_manager_frame(void *context,uint32_t hash,uint8_t *out,size_t capacity,uint64_t *serial,uint32_t *width,uint32_t *height) {
  GtaMovieManager *m=context;if(!m||!out||!serial||!width||!height)return 0;
  *width=*height=0;
  if(stopped(m))return 0;
  pthread_mutex_lock(&m->mutex);Movie *entry=find(m,hash);int result=0;
  if(entry&&entry->decoder) {
    GtaMovieStatus s={0};gta_movie_decoder_status(entry->decoder,&s);
    if(s.ready&&!s.failed) {
      *width=s.width;*height=s.height;
      uint64_t local=(uint32_t)(*serial>>32)==entry->generation?(uint32_t)*serial:0;
      result=gta_movie_decoder_copy(entry->decoder,out,capacity,&local);
      if(result>0)*serial=((uint64_t)entry->generation<<32)|(uint32_t)local;
    }
  }
  pthread_mutex_unlock(&m->mutex);return result;
}
