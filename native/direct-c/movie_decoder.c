#define _POSIX_C_SOURCE 200809L
#include "movie_decoder.h"
#include "movie_pixels.h"
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <math.h>

#ifdef __ANDROID__
#include <android/hardware_buffer.h>
#include <aaudio/AAudio.h>
#include <fcntl.h>
#include <media/NdkImage.h>
#include <media/NdkImageReader.h>
#include <media/NdkMediaCodec.h>
#include <media/NdkMediaExtractor.h>
#include <media/NdkMediaFormat.h>
#include <sys/stat.h>
#include <unistd.h>

struct GtaMovieDecoder {
  pthread_t thread;
  pthread_mutex_t mutex;
  char *path;
  uint8_t *rgba;
  size_t rgba_capacity;
  GtaMovieStatus status;
  int stop, loop;
  uint64_t seek_serial;
  int64_t anchor_ns, position_us;
  float volume[2];
  GtaMovieNotify notify;
  void *notify_context;
};

static int64_t clock_ns(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC,&now);
  return (int64_t)now.tv_sec*1000000000+now.tv_nsec;
}
static void rest(void) {
  struct timespec delay={0,2000000};nanosleep(&delay,NULL);
}
static int64_t position_locked(GtaMovieDecoder *d) {
  int64_t p=d->position_us;
  if(d->status.playing&&!d->status.finished)p+=(clock_ns()-d->anchor_ns)/1000;
  if(d->status.duration_us>0&&p>d->status.duration_us)p=d->status.duration_us;
  return p;
}
static void progress(GtaMovieDecoder *d) {
  pthread_mutex_lock(&d->mutex);GtaMovieNotify callback=d->notify;void *context=d->notify_context;pthread_mutex_unlock(&d->mutex);
  if(callback)callback(context,d);
}
static void failure(GtaMovieDecoder *d,const char *operation,int code) {
  pthread_mutex_lock(&d->mutex);
  d->status.failed=1;d->status.playing=0;
  snprintf(d->status.error,sizeof(d->status.error),"%s (%d)",operation,code);
  pthread_mutex_unlock(&d->mutex);
  progress(d);
}

static int receive_image(GtaMovieDecoder *d,AImageReader *reader,int bt709,int full_range,
                         int64_t expected_pts) {
  AImage *image=NULL;
  media_status_t result=AImageReader_acquireLatestImage(reader,&image);
  if(result==AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE)return 0;
  if(result!=AMEDIA_OK){failure(d,"acquire video image",result);return -1;}
  int64_t timestamp=0;
  if(AImage_getTimestamp(image,&timestamp)!=AMEDIA_OK){AImage_delete(image);return 0;}
  /* A release before a seek may still be queued. Never publish that old image. */
  if(timestamp/1000!=expected_pts){AImage_delete(image);return 0;}
  int32_t width=0,height=0,format=0,count=0;
  AImageCropRect crop={0};GtaMoviePlane planes[3]={{0}};
  media_status_t rw=AImage_getWidth(image,&width),rh=AImage_getHeight(image,&height);
  media_status_t rf=AImage_getFormat(image,&format),rn=AImage_getNumberOfPlanes(image,&count);
  media_status_t rcrop=AImage_getCropRect(image,&crop);
  int valid=rw==AMEDIA_OK&&rh==AMEDIA_OK&&rf==AMEDIA_OK&&
      (format==AIMAGE_FORMAT_YUV_420_888||format==0x32315659)&&rn==AMEDIA_OK&&count==3&&
      rcrop==AMEDIA_OK&&crop.left>=0&&crop.top>=0&&
      crop.right>crop.left&&crop.bottom>crop.top;
  for(int i=0;i<3&&valid;i++) {
    uint8_t *data=NULL;int length=0;int32_t row=0,pixel=0;
    valid=AImage_getPlaneData(image,i,&data,&length)==AMEDIA_OK&&length>0&&
        AImage_getPlaneRowStride(image,i,&row)==AMEDIA_OK&&row>0&&
        AImage_getPlanePixelStride(image,i,&pixel)==AMEDIA_OK&&pixel>0;
    planes[i]=(GtaMoviePlane){data,(size_t)length,(size_t)row,(size_t)pixel};
  }
  pthread_mutex_lock(&d->mutex);
  int converted=valid?gta_movie_yuv_rgba(planes,(uint32_t)width,(uint32_t)height,
      (uint32_t)crop.left,(uint32_t)crop.top,(uint32_t)(crop.right-crop.left),
      (uint32_t)(crop.bottom-crop.top),bt709,full_range,d->rgba,d->rgba_capacity):-1;
  if(converted==0) {
    d->status.width=(uint32_t)(crop.right-crop.left);d->status.height=(uint32_t)(crop.bottom-crop.top);
    d->status.frame_pts_us=expected_pts;++d->status.frame_serial;d->status.ready=1;
  }
  pthread_mutex_unlock(&d->mutex);
  if(converted)fprintf(stderr,"video layout: valid=%d image=%dx%d format=%x planes=%d crop=%d,%d,%d,%d capacity=%zu Y=%zu/%zu/%zu U=%zu/%zu/%zu V=%zu/%zu/%zu\n",
      valid,width,height,format,count,crop.left,crop.top,crop.right,crop.bottom,d->rgba_capacity,
      planes[0].bytes,planes[0].row_stride,planes[0].pixel_stride,
      planes[1].bytes,planes[1].row_stride,planes[1].pixel_stride,
      planes[2].bytes,planes[2].row_stride,planes[2].pixel_stride);
  AImage_delete(image);
  if(converted){failure(d,"unsupported video plane layout",converted);return -1;}
  return 1;
}

typedef struct {
  AMediaExtractor *extractor;
  AMediaCodec *codec;
  AAudioStream *stream;
  int rate,channels,encoding,input_eos,codec_started;
  ssize_t pending;
  size_t frames,cursor;
  float pcm[8192*2];
} MovieAudio;

static void audio_close(MovieAudio *a) {
  if(a->stream)AAudioStream_close(a->stream);
  if(a->codec){if(a->codec_started)AMediaCodec_stop(a->codec);AMediaCodec_delete(a->codec);}
  if(a->extractor)AMediaExtractor_delete(a->extractor);
  memset(a,0,sizeof(*a));a->pending=-1;
}

static int audio_open(GtaMovieDecoder *d,MovieAudio *a,int fd,int64_t bytes) {
  a->pending=-1;a->encoding=2;
  a->extractor=AMediaExtractor_new();
  if(!a->extractor||AMediaExtractor_setDataSourceFd(a->extractor,fd,0,bytes)!=AMEDIA_OK)goto fail;
  AMediaFormat *format=NULL;
  for(size_t i=0;i<AMediaExtractor_getTrackCount(a->extractor);i++) {
    AMediaFormat *f=AMediaExtractor_getTrackFormat(a->extractor,i);const char *mime=NULL;
    if(f&&AMediaFormat_getString(f,AMEDIAFORMAT_KEY_MIME,&mime)&&mime&&strcmp(mime,"audio/mp4a-latm")==0) {
      format=f;
      if(AMediaExtractor_selectTrack(a->extractor,i)!=AMEDIA_OK){AMediaFormat_delete(f);goto fail;}
      break;
    }
    if(f)AMediaFormat_delete(f);
  }
  if(!format){audio_close(a);return 0;}
  int32_t rate=0,channels=0;
  int valid=AMediaFormat_getInt32(format,AMEDIAFORMAT_KEY_SAMPLE_RATE,&rate)&&
      AMediaFormat_getInt32(format,AMEDIAFORMAT_KEY_CHANNEL_COUNT,&channels)&&
      rate>=8000&&rate<=96000&&(channels==1||channels==2);
  if(!valid){AMediaFormat_delete(format);goto fail;}
  a->rate=rate;a->channels=channels;
  AMediaFormat_setInt32(format,"pcm-encoding",2);
  a->codec=AMediaCodec_createDecoderByType("audio/mp4a-latm");
  int rc=a->codec?AMediaCodec_configure(a->codec,format,NULL,NULL,0):-1;
  AMediaFormat_delete(format);
  if(rc!=AMEDIA_OK||AMediaCodec_start(a->codec)!=AMEDIA_OK)goto fail;
  a->codec_started=1;
  AAudioStreamBuilder *builder=NULL;
  if(AAudio_createStreamBuilder(&builder)!=AAUDIO_OK)goto fail;
  AAudioStreamBuilder_setDirection(builder,AAUDIO_DIRECTION_OUTPUT);
  AAudioStreamBuilder_setSharingMode(builder,AAUDIO_SHARING_MODE_SHARED);
  AAudioStreamBuilder_setPerformanceMode(builder,AAUDIO_PERFORMANCE_MODE_NONE);
  AAudioStreamBuilder_setUsage(builder,AAUDIO_USAGE_GAME);
  AAudioStreamBuilder_setContentType(builder,AAUDIO_CONTENT_TYPE_MOVIE);
  AAudioStreamBuilder_setFormat(builder,AAUDIO_FORMAT_PCM_FLOAT);
  AAudioStreamBuilder_setChannelCount(builder,2);
  AAudioStreamBuilder_setSampleRate(builder,rate);
  AAudioStreamBuilder_setBufferCapacityInFrames(builder,rate/4);
  rc=AAudioStreamBuilder_openStream(builder,&a->stream);
  AAudioStreamBuilder_delete(builder);
  if(rc!=AAUDIO_OK||AAudioStream_getSampleRate(a->stream)!=rate||
      AAudioStream_getChannelCount(a->stream)!=2||AAudioStream_getFormat(a->stream)!=AAUDIO_FORMAT_PCM_FLOAT)goto fail;
  AAudioStream_setBufferSizeInFrames(a->stream,rate/10);
  pthread_mutex_lock(&d->mutex);d->status.has_audio=1;pthread_mutex_unlock(&d->mutex);
  return 0;
fail:
  failure(d,"initialize AAC movie audio",-1);return -1;
}

static int audio_pause(MovieAudio *a) {
  if(!a->stream)return 0;
  aaudio_stream_state_t state=AAudioStream_getState(a->stream);
  if(state==AAUDIO_STREAM_STATE_OPEN||state==AAUDIO_STREAM_STATE_PAUSED||state==AAUDIO_STREAM_STATE_FLUSHED)return 0;
  if(AAudioStream_requestPause(a->stream)!=AAUDIO_OK)return -1;
  for(int i=0;i<25;i++) {
    state=AAudioStream_getState(a->stream);
    if(state==AAUDIO_STREAM_STATE_PAUSED)return 0;
    aaudio_stream_state_t next=state;
    AAudioStream_waitForStateChange(a->stream,state,&next,20000000);
  }
  return -1;
}

static int audio_seek(MovieAudio *a,int64_t target) {
  if(!a->codec)return 0;
  if(audio_pause(a))return -1;
  if(AAudioStream_getState(a->stream)==AAUDIO_STREAM_STATE_PAUSED) {
    if(AAudioStream_requestFlush(a->stream)!=AAUDIO_OK)return -1;
    for(int i=0;i<25;i++) {
      aaudio_stream_state_t state=AAudioStream_getState(a->stream),next=state;
      if(state==AAUDIO_STREAM_STATE_FLUSHED)break;
      AAudioStream_waitForStateChange(a->stream,state,&next,20000000);
    }
    if(AAudioStream_getState(a->stream)!=AAUDIO_STREAM_STATE_FLUSHED)return -1;
  }
  if(a->pending>=0){AMediaCodec_releaseOutputBuffer(a->codec,(size_t)a->pending,0);a->pending=-1;}
  a->cursor=a->frames=0;a->input_eos=0;
  return AMediaExtractor_seekTo(a->extractor,target,AMEDIAEXTRACTOR_SEEK_PREVIOUS_SYNC)==AMEDIA_OK&&
      AMediaCodec_flush(a->codec)==AMEDIA_OK?0:-1;
}

static int audio_step(GtaMovieDecoder *d,MovieAudio *a,int playing,int64_t target) {
  if(!a->codec)return 0;
  if(!playing)return audio_pause(a);
  aaudio_stream_state_t state=AAudioStream_getState(a->stream);
  if(state==AAUDIO_STREAM_STATE_OPEN||state==AAUDIO_STREAM_STATE_PAUSED||state==AAUDIO_STREAM_STATE_FLUSHED) {
    if(AAudioStream_requestStart(a->stream)!=AAUDIO_OK)return -1;
  }
  if(!a->input_eos) {
    int64_t pts=AMediaExtractor_getSampleTime(a->extractor);
    if(pts<0||pts<=target+500000) {
      ssize_t index=AMediaCodec_dequeueInputBuffer(a->codec,0);
      if(index>=0) {
        size_t capacity=0;uint8_t *buffer=AMediaCodec_getInputBuffer(a->codec,(size_t)index,&capacity);
        ssize_t size=AMediaExtractor_getSampleSize(a->extractor);media_status_t rc;
        if(size<0){rc=AMediaCodec_queueInputBuffer(a->codec,(size_t)index,0,0,0,AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM);a->input_eos=1;}
        else {
          if(!buffer||(uint64_t)size>capacity||AMediaExtractor_readSampleData(a->extractor,buffer,capacity)!=size)return -1;
          rc=AMediaCodec_queueInputBuffer(a->codec,(size_t)index,0,(size_t)size,(uint64_t)pts,0);
          AMediaExtractor_advance(a->extractor);
        }
        if(rc!=AMEDIA_OK)return -1;
      }
    }
  }
  if(a->pending<0) {
    AMediaCodecBufferInfo info={0};a->pending=AMediaCodec_dequeueOutputBuffer(a->codec,&info,0);
    if(a->pending==AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED) {
      AMediaFormat *format=AMediaCodec_getOutputFormat(a->codec);int32_t rate=0,channels=0,encoding=2;
      int valid=format&&AMediaFormat_getInt32(format,AMEDIAFORMAT_KEY_SAMPLE_RATE,&rate)&&
          AMediaFormat_getInt32(format,AMEDIAFORMAT_KEY_CHANNEL_COUNT,&channels)&&rate==a->rate&&(channels==1||channels==2);
      if(format){AMediaFormat_getInt32(format,"pcm-encoding",&encoding);AMediaFormat_delete(format);}
      if(!valid||(encoding!=2&&encoding!=4))return -1;
      a->channels=channels;a->encoding=encoding;
    } else if(a->pending<AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED)return -1;
    if(a->pending>=0) {
      if(info.flags&AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM){AMediaCodec_releaseOutputBuffer(a->codec,(size_t)a->pending,0);a->pending=-1;return 0;}
      size_t capacity=0;uint8_t *buffer=AMediaCodec_getOutputBuffer(a->codec,(size_t)a->pending,&capacity);
      size_t sample_bytes=a->encoding==2?2:4;
      if(info.size<0||(size_t)info.size>capacity||(!buffer&&info.size))return -1;
      a->frames=(size_t)info.size/sample_bytes/(size_t)a->channels;a->cursor=0;
      if(a->frames>8192)return -1;
      if(info.presentationTimeUs<target) {
        int64_t skip=(target-info.presentationTimeUs)*a->rate/1000000;
        a->cursor=(uint64_t)skip>a->frames?a->frames:(size_t)skip;
      }
      pthread_mutex_lock(&d->mutex);float left=d->volume[0],right=d->volume[1];pthread_mutex_unlock(&d->mutex);
      for(size_t n=0;n<a->frames;n++)for(size_t channel=0;channel<2;channel++) {
        size_t input=n*(size_t)a->channels+(a->channels==1?0:channel);float sample;
        if(a->encoding==2){int16_t value;memcpy(&value,buffer+input*2,2);sample=(float)value/32768.0f;}
        else memcpy(&sample,buffer+input*4,4);
        if(!isfinite(sample))sample=0;
        if(sample>1)sample=1;if(sample< -1)sample= -1;
        a->pcm[n*2+channel]=sample*(channel?right:left);
      }
    }
  }
  if(a->pending>=0) {
    /* Blocking-mode AAudio streams need initial PCM while STARTING as well;
     * waiting for STARTED before priming can leave the stream with no data. */
    if(a->cursor<a->frames) {
      int32_t written=AAudioStream_write(a->stream,a->pcm+a->cursor*2,(int32_t)(a->frames-a->cursor),0);
      if(written<0)return -1;
      a->cursor+=(size_t)written;
      pthread_mutex_lock(&d->mutex);d->status.audio_frames+=(uint32_t)written;pthread_mutex_unlock(&d->mutex);
    }
    if(a->cursor==a->frames){AMediaCodec_releaseOutputBuffer(a->codec,(size_t)a->pending,0);a->pending=-1;}
  }
  return 0;
}

static void *decode_worker(void *argument) {
  GtaMovieDecoder *d=argument;
  AMediaExtractor *extractor=NULL;AMediaFormat *format=NULL;AMediaCodec *codec=NULL;
  AImageReader *reader=NULL;ANativeWindow *window=NULL;
  MovieAudio audio={.pending=-1};
  int fd=-1,started=0;media_status_t rc;
  fd=open(d->path,O_RDONLY|O_CLOEXEC);
  struct stat file;
  if(fd<0||fstat(fd,&file)||file.st_size<=0){failure(d,"open movie",-1);goto done;}
  extractor=AMediaExtractor_new();
  if(!extractor){failure(d,"create media extractor",-1);goto done;}
  rc=AMediaExtractor_setDataSourceFd(extractor,fd,0,file.st_size);
  if(rc!=AMEDIA_OK){failure(d,"read MP4 movie",rc);goto done;}
  size_t tracks=AMediaExtractor_getTrackCount(extractor);int selected=-1;
  for(size_t i=0;i<tracks;i++) {
    AMediaFormat *candidate=AMediaExtractor_getTrackFormat(extractor,i);const char *mime=NULL;
    if(candidate&&AMediaFormat_getString(candidate,AMEDIAFORMAT_KEY_MIME,&mime)&&
        mime&&strcmp(mime,"video/av01")==0){format=candidate;selected=(int)i;break;}
    if(candidate)AMediaFormat_delete(candidate);
  }
  int32_t width=0,height=0;int64_t duration=0;
  if(selected<0||!AMediaFormat_getInt32(format,AMEDIAFORMAT_KEY_WIDTH,&width)||
      !AMediaFormat_getInt32(format,AMEDIAFORMAT_KEY_HEIGHT,&height)||
      !AMediaFormat_getInt64(format,AMEDIAFORMAT_KEY_DURATION,&duration)||
      width<=0||height<=0||width>2048||height>2048||duration<=0||duration>86400000000LL) {
    failure(d,"unsupported AV1 movie metadata",-1);goto done;
  }
  rc=AMediaExtractor_selectTrack(extractor,(size_t)selected);
  if(rc!=AMEDIA_OK){failure(d,"select AV1 track",rc);goto done;}
  d->rgba_capacity=(size_t)width*(size_t)height*4;
  d->rgba=malloc(d->rgba_capacity);
  if(!d->rgba){failure(d,"allocate movie frame",-1);goto done;}
  rc=AImageReader_newWithUsage(width,height,AIMAGE_FORMAT_YUV_420_888,
      AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,3,&reader);
  if(rc!=AMEDIA_OK){failure(d,"create video image reader",rc);goto done;}
  rc=AImageReader_getWindow(reader,&window);
  if(rc!=AMEDIA_OK){failure(d,"get video output window",rc);goto done;}
  /* The surface decoder returned a proprietary compressed image despite
   * a CPU-readable YUV reader. The standard software codec exposes safe planes. */
  AMediaFormat_setInt32(format,"color-format",0x7f420888);
  codec=AMediaCodec_createCodecByName("c2.android.av1.decoder");
  if(!codec){failure(d,"create Android AV1 decoder",-1);goto done;}
  rc=AMediaCodec_configure(codec,format,window,NULL,0);
  if(rc!=AMEDIA_OK){failure(d,"configure Android AV1 decoder",rc);goto done;}
  rc=AMediaCodec_start(codec);
  if(rc!=AMEDIA_OK){failure(d,"start Android AV1 decoder",rc);goto done;}
  started=1;
  if(audio_open(d,&audio,fd,file.st_size))goto done;
  pthread_mutex_lock(&d->mutex);
  d->status.width=(uint32_t)width;d->status.height=(uint32_t)height;d->status.duration_us=duration;
  d->anchor_ns=clock_ns();
  pthread_mutex_unlock(&d->mutex);
  progress(d);
  uint64_t seek=0;int input_eos=0,need_frame=1,awaiting=0,bt709=0,full_range=0;
  int64_t image_deadline=0,image_pts=0;
  ssize_t pending=-1;AMediaCodecBufferInfo output={0};
  for(;;) {
    pthread_mutex_lock(&d->mutex);
    int stop=d->stop,playing=d->status.playing,loop=d->loop;
    int64_t target=position_locked(d);uint64_t requested=d->seek_serial;
    pthread_mutex_unlock(&d->mutex);
    if(stop)break;
    if(seek!=requested) {
      if(pending>=0){AMediaCodec_releaseOutputBuffer(codec,(size_t)pending,0);pending=-1;}
      rc=AMediaExtractor_seekTo(extractor,target,AMEDIAEXTRACTOR_SEEK_PREVIOUS_SYNC);
      if(rc!=AMEDIA_OK){failure(d,"seek movie",rc);break;}
      rc=AMediaCodec_flush(codec);
      if(rc!=AMEDIA_OK){failure(d,"flush movie after seek",rc);break;}
      if(audio_seek(&audio,target)){failure(d,"seek movie audio",-1);break;}
      AImage *old=NULL;
      while(AImageReader_acquireLatestImage(reader,&old)==AMEDIA_OK){AImage_delete(old);old=NULL;}
      seek=requested;input_eos=0;need_frame=1;awaiting=0;
    }
    if(audio_step(d,&audio,playing,target)){failure(d,"play movie audio",-1);break;}
    if(awaiting) {
      int received=receive_image(d,reader,bt709,full_range,image_pts);
      if(received<0)break;
      if(received>0){awaiting=0;need_frame=0;progress(d);}
      else if(clock_ns()>image_deadline){failure(d,"video frame delivery timed out",-1);break;}
    }
    if(!playing&&!need_frame){rest();continue;}
    if(!input_eos) {
      int64_t sample_time=AMediaExtractor_getSampleTime(extractor);
      if(sample_time<0||sample_time<=target+500000||need_frame) {
        ssize_t index=AMediaCodec_dequeueInputBuffer(codec,0);
        if(index>=0) {
          size_t capacity=0;uint8_t *buffer=AMediaCodec_getInputBuffer(codec,(size_t)index,&capacity);
          ssize_t size=AMediaExtractor_getSampleSize(extractor);
          if(size>=0&&(!buffer||(uint64_t)size>capacity)) {
            failure(d,"encoded video sample exceeds decoder buffer",-1);break;
          }
          if(size<0) {
            rc=AMediaCodec_queueInputBuffer(codec,(size_t)index,0,0,0,AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM);input_eos=1;
          } else {
            ssize_t count=AMediaExtractor_readSampleData(extractor,buffer,capacity);
            if(count!=size){failure(d,"read encoded video sample",-1);break;}
            rc=AMediaCodec_queueInputBuffer(codec,(size_t)index,0,(size_t)count,(uint64_t)sample_time,0);
            AMediaExtractor_advance(extractor);
          }
          if(rc!=AMEDIA_OK){failure(d,"queue encoded video sample",rc);break;}
        }
      }
    }
    if(pending<0&&!awaiting) {
      pending=AMediaCodec_dequeueOutputBuffer(codec,&output,0);
      if(pending==AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED) {
        AMediaFormat *f=AMediaCodec_getOutputFormat(codec);int32_t standard=0,range=0;
        if(f){AMediaFormat_getInt32(f,"color-standard",&standard);AMediaFormat_getInt32(f,"color-range",&range);AMediaFormat_delete(f);}
        bt709=standard==1;full_range=range==1;
      } else if(pending<AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED) {failure(d,"decode AV1 movie",(int)pending);break;}
    }
    if(pending>=0) {
      if(output.flags&AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM) {
        AMediaCodec_releaseOutputBuffer(codec,(size_t)pending,0);pending=-1;
        pthread_mutex_lock(&d->mutex);
        if(loop){d->position_us=0;d->anchor_ns=clock_ns();++d->seek_serial;}
        else{d->position_us=duration;d->status.finished=1;d->status.playing=0;}
        pthread_mutex_unlock(&d->mutex);
        progress(d);
      } else if(output.presentationTimeUs<target-100000) {
        AMediaCodec_releaseOutputBuffer(codec,(size_t)pending,0);pending=-1;
      } else if(output.presentationTimeUs<=target+5000||(need_frame&&!playing)) {
        image_pts=output.presentationTimeUs;
        rc=AMediaCodec_releaseOutputBufferAtTime(codec,(size_t)pending,image_pts*1000);pending=-1;
        if(rc!=AMEDIA_OK){failure(d,"release decoded video frame",rc);break;}
        awaiting=1;image_deadline=clock_ns()+2000000000LL;
      }
    }
    rest();
  }
done:
  audio_close(&audio);
  if(codec){if(started)AMediaCodec_stop(codec);AMediaCodec_delete(codec);}
  if(reader)AImageReader_delete(reader);
  if(format)AMediaFormat_delete(format);
  if(extractor)AMediaExtractor_delete(extractor);
  if(fd>=0)close(fd);
  return NULL;
}

GtaMovieDecoder *gta_movie_decoder_open(const char *path) {
  if(!path||strlen(path)>4095)return NULL;
  GtaMovieDecoder *d=calloc(1,sizeof(*d));if(!d)return NULL;
  d->path=strdup(path);
  if(!d->path){free(d);return NULL;}
  if(pthread_mutex_init(&d->mutex,NULL)){free(d->path);free(d);return NULL;}
  d->anchor_ns=clock_ns();
  d->volume[0]=d->volume[1]=1;
  if(pthread_create(&d->thread,NULL,decode_worker,d)) {
    pthread_mutex_destroy(&d->mutex);free(d->path);free(d);return NULL;
  }
  return d;
}
void gta_movie_decoder_control(GtaMovieDecoder *d,int playing,int loop,int64_t seek_us) {
  if(!d)return;
  pthread_mutex_lock(&d->mutex);
  d->position_us=position_locked(d);d->anchor_ns=clock_ns();d->loop=!!loop;
  if(seek_us>=0){d->position_us=seek_us;d->status.finished=0;++d->seek_serial;}
  if(d->position_us<0)d->position_us=0;
  if(d->status.duration_us>0&&d->position_us>d->status.duration_us)d->position_us=d->status.duration_us;
  d->status.playing=!!playing&&!d->status.failed&&!d->status.finished;
  pthread_mutex_unlock(&d->mutex);
}
void gta_movie_decoder_status(GtaMovieDecoder *d,GtaMovieStatus *out) {
  if(!d||!out)return;
  pthread_mutex_lock(&d->mutex);*out=d->status;out->position_us=position_locked(d);pthread_mutex_unlock(&d->mutex);
}
void gta_movie_decoder_volume(GtaMovieDecoder *d,float left,float right) {
  if(!d)return;
  if(!isfinite(left)||left<0)left=0;if(left>1)left=1;
  if(!isfinite(right)||right<0)right=0;if(right>1)right=1;
  pthread_mutex_lock(&d->mutex);d->volume[0]=left;d->volume[1]=right;pthread_mutex_unlock(&d->mutex);
}
void gta_movie_decoder_notify(GtaMovieDecoder *d,GtaMovieNotify callback,void *context) {
  if(!d)return;
  pthread_mutex_lock(&d->mutex);d->notify=callback;d->notify_context=context;pthread_mutex_unlock(&d->mutex);
  progress(d);
}
int gta_movie_decoder_copy(GtaMovieDecoder *d,uint8_t *out,size_t capacity,uint64_t *serial) {
  if(!d||!out||!serial)return -1;
  pthread_mutex_lock(&d->mutex);
  int result=0;
  if(d->status.ready&&*serial!=d->status.frame_serial) {
    size_t bytes=(size_t)d->status.width*d->status.height*4;
    if(capacity<bytes)result=-1;
    else{memcpy(out,d->rgba,bytes);*serial=d->status.frame_serial;result=1;}
  }
  pthread_mutex_unlock(&d->mutex);return result;
}
void gta_movie_decoder_close(GtaMovieDecoder *d) {
  if(!d)return;
  pthread_mutex_lock(&d->mutex);d->stop=1;pthread_mutex_unlock(&d->mutex);
  pthread_join(d->thread,NULL);pthread_mutex_destroy(&d->mutex);
  free(d->rgba);free(d->path);free(d);
}
#else
GtaMovieDecoder *gta_movie_decoder_open(const char *path){(void)path;return NULL;}
void gta_movie_decoder_control(GtaMovieDecoder *d,int p,int l,int64_t s){(void)d;(void)p;(void)l;(void)s;}
void gta_movie_decoder_volume(GtaMovieDecoder *d,float l,float r){(void)d;(void)l;(void)r;}
void gta_movie_decoder_notify(GtaMovieDecoder *d,GtaMovieNotify n,void *c){(void)d;(void)n;(void)c;}
void gta_movie_decoder_status(GtaMovieDecoder *d,GtaMovieStatus *s){(void)d;if(s){memset(s,0,sizeof(*s));s->failed=1;}}
int gta_movie_decoder_copy(GtaMovieDecoder *d,uint8_t *p,size_t n,uint64_t *s){(void)d;(void)p;(void)n;(void)s;return -1;}
void gta_movie_decoder_close(GtaMovieDecoder *d){(void)d;}
#endif
