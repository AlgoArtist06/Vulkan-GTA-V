/* Java owns opaque IDs; no stale Java value is dereferenced as a pointer. */
#include "native_host.h"
#include <jni.h>
#include <android/native_window_jni.h>
#include <android/log.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static pthread_mutex_t registry_mutex = PTHREAD_MUTEX_INITIALIZER;
static struct { jlong id; GtaNativeSession *session; } sessions[8];
static jlong next_id = 1;

static GtaNativeSession *lookup(jlong id) {
  for (size_t i = 0; i < 8; ++i)
    if (id > 0 && sessions[i].id == id) return sessions[i].session;
  return NULL;
}

#define JNI_NAME(name) Java_com_algoartist_gta5native_NativeBridge_##name

JNIEXPORT jlong JNICALL JNI_NAME(create)(JNIEnv *env, jclass cls,
                                        jstring assets, jstring saves) {
  (void)cls;
  if (!assets || !saves) return 0;
  const char *asset_path = (*env)->GetStringUTFChars(env, assets, NULL);
  if (!asset_path) return 0;
  const char *save_path = (*env)->GetStringUTFChars(env, saves, NULL);
  if (!save_path) { (*env)->ReleaseStringUTFChars(env, assets, asset_path); return 0; }
  pthread_mutex_lock(&registry_mutex);
  size_t slot = 0;
  while (slot < 8 && sessions[slot].session) ++slot;
  jlong id = 0;
  if (slot < 8 && next_id < INT64_MAX) {
    GtaNativeSession *session = gta_native_session_create(asset_path, save_path);
    if (session) {
      id = next_id++;
      sessions[slot].id = id;
      sessions[slot].session = session;
    } else {
      __android_log_write(ANDROID_LOG_ERROR, "GtaDirectHost", "native session creation failed");
    }
  }
  pthread_mutex_unlock(&registry_mutex);
  (*env)->ReleaseStringUTFChars(env, saves, save_path);
  (*env)->ReleaseStringUTFChars(env, assets, asset_path);
  return id;
}

JNIEXPORT jint JNICALL JNI_NAME(boot)(JNIEnv *env, jclass cls, jlong id) {
  (void)env; (void)cls;
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  int result = session ? gta_native_session_boot(session) : -1;
  pthread_mutex_unlock(&registry_mutex);
  return result;
}

JNIEXPORT jint JNICALL JNI_NAME(surface)(JNIEnv *env, jclass cls, jlong id,
                                       jobject surface, jint width, jint height,
                                       jint operation) {
  (void)cls;
  if (width < 0 || height < 0) return -1;
  ANativeWindow *window = surface ? ANativeWindow_fromSurface(env, surface) : NULL;
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  int result = session ? gta_native_session_surface(session, window,
                 (uint32_t)width, (uint32_t)height, operation) : -1;
  pthread_mutex_unlock(&registry_mutex);
  if (window) ANativeWindow_release(window);
  return result;
}

JNIEXPORT jint JNICALL JNI_NAME(status)(JNIEnv *env, jclass cls, jlong id) {
  (void)env; (void)cls;
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  int result = session ? gta_native_session_status(session, NULL, 0) : GTA_NATIVE_SESSION_FAILED;
  pthread_mutex_unlock(&registry_mutex);
  return result;
}

/* Engine text can contain ordinary UTF-8. Convert bytes through Java's
 * decoder instead of treating them as JNI modified UTF-8. */
static jstring utf8_string(JNIEnv *env, const char *text, size_t size) {
  jclass string_class = (*env)->FindClass(env, "java/lang/String");
  if (!string_class) return NULL;
  jmethodID constructor = (*env)->GetMethodID(env, string_class, "<init>", "([BLjava/lang/String;)V");
  if (!constructor) return NULL;
  jbyteArray bytes = (*env)->NewByteArray(env, (jsize)size);
  if (!bytes) return NULL;
  (*env)->SetByteArrayRegion(env, bytes, 0, (jsize)size, (const jbyte *)text);
  jstring encoding = (*env)->NewStringUTF(env, "UTF-8");
  if (!encoding) return NULL;
  return (jstring)(*env)->NewObject(env, string_class, constructor, bytes, encoding);
}

JNIEXPORT jstring JNICALL JNI_NAME(error)(JNIEnv *env, jclass cls, jlong id) {
  (void)cls;
  char error[2048] = "invalid native session ID";
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  if (session) (void)gta_native_session_status(session, error, sizeof(error));
  pthread_mutex_unlock(&registry_mutex);
  size_t size = 0; while (error[size]) ++size;
  return utf8_string(env, error, size);
}

/* out = { game data bytes loaded, frames presented, world shown (0/1) };
 * returns the engine output lines since the last call. */
JNIEXPORT jstring JNICALL JNI_NAME(loading)(JNIEnv *env, jclass cls, jlong id,
                                          jlongArray out) {
  (void)cls;
  if (!out || (*env)->GetArrayLength(env, out) < 4) return NULL;
  enum { CAPACITY = 64 * 1024 };
  char *lines = malloc(CAPACITY);
  if (!lines) return NULL;
  uint64_t data_bytes = 0;
  uint32_t presents = 0;
  bool world = false, menu = false;
  size_t size = 0;
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  if (session) size = gta_native_session_loading(session, lines, CAPACITY, &data_bytes, &presents, &world, &menu);
  pthread_mutex_unlock(&registry_mutex);
  jlong values[4] = { (jlong)data_bytes, (jlong)presents, world ? 1 : 0, menu ? 1 : 0 };
  (*env)->SetLongArrayRegion(env, out, 0, 4, values);
  jstring result = utf8_string(env, lines, size);
  free(lines);
  return result;
}

JNIEXPORT jint JNICALL JNI_NAME(input)(JNIEnv *env, jclass cls, jlong id,
                                     jint kind, jint code, jfloat value, jfloat value2) {
  (void)env; (void)cls;
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  int result = session ? gta_native_session_input(session, kind, code, value, value2) : -1;
  pthread_mutex_unlock(&registry_mutex);
  return result;
}

JNIEXPORT jint JNICALL JNI_NAME(stop)(JNIEnv *env, jclass cls, jlong id, jint timeout) {
  (void)env; (void)cls;
  if (timeout < 0 || timeout > 60000) return -1;
  pthread_mutex_lock(&registry_mutex);
  GtaNativeSession *session = lookup(id);
  int result = session ? gta_native_session_stop(session, (uint32_t)timeout) : -1;
  pthread_mutex_unlock(&registry_mutex);
  return result;
}

JNIEXPORT void JNICALL JNI_NAME(destroy)(JNIEnv *env, jclass cls, jlong id) {
  (void)env; (void)cls;
  pthread_mutex_lock(&registry_mutex);
  for (size_t i = 0; i < 8; ++i) {
    if (sessions[i].id == id && sessions[i].session) {
      if (gta_native_session_stop(sessions[i].session, 0) == 0) {
        gta_native_session_destroy(sessions[i].session);
        sessions[i].id = 0;
        sessions[i].session = NULL;
      } else {
        __android_log_write(ANDROID_LOG_WARN, "GtaDirectHost", "active session retained pending worker shutdown");
      }
      break;
    }
  }
  pthread_mutex_unlock(&registry_mutex);
}
