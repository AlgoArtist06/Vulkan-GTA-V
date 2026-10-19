/* Generated direct native ABI; no module loader or VM. */
#include "native_host.h"
#include <string.h>
u32 w2c_env_0x5F_pthread_create_js(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__pthread_create_js", values, 4);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_accept4(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2, u32 arg3, u32 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_accept4", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_connect(struct w2c_env* context, u32 arg0, u64 arg1, u32 arg2, u32 arg3, u32 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_connect", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_fcntl64(struct w2c_env* context, u32 arg0, u32 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_fcntl64", values, 3);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_fstat64(struct w2c_env* context, u32 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_fstat64", values, 2);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_ftruncate64(struct w2c_env* context, u32 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_ftruncate64", values, 2);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_getcwd(struct w2c_env* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_getcwd", values, 2);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_getdents64(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_getdents64", values, 3);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_getsockname(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2, u32 arg3, u32 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_getsockname", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_getsockopt(struct w2c_env* context, u32 arg0, u32 arg1, u32 arg2, u64 arg3, u64 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_getsockopt", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_ioctl(struct w2c_env* context, u32 arg0, u32 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_ioctl", values, 3);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_lstat64(struct w2c_env* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_lstat64", values, 2);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_mkdirat(struct w2c_env* context, u32 arg0, u64 arg1, u32 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_mkdirat", values, 3);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_newfstatat(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2, u32 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_newfstatat", values, 4);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_openat(struct w2c_env* context, u32 arg0, u64 arg1, u32 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_openat", values, 4);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_poll(struct w2c_env* context, u64 arg0, u32 arg1, u32 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_poll", values, 3);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_recvfrom(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2, u32 arg3, u64 arg4, u64 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_recvfrom", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_renameat(struct w2c_env* context, u32 arg0, u64 arg1, u32 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_renameat", values, 4);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_rmdir(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_rmdir", values, 1);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_sendto(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2, u32 arg3, u64 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_sendto", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_setsockopt(struct w2c_env* context, u32 arg0, u32 arg1, u32 arg2, u64 arg3, u32 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_setsockopt", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_shutdown(struct w2c_env* context, u32 arg0, u32 arg1, u32 arg2, u32 arg3, u32 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_shutdown", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_socket(struct w2c_env* context, u32 arg0, u32 arg1, u32 arg2, u32 arg3, u32 arg4, u32 arg5) {
  uint64_t values[6];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_socket", values, 6);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_stat64(struct w2c_env* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_stat64", values, 2);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_statfs64(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_statfs64", values, 3);
  return (u32)returned;
}
u32 w2c_env_0x5F_syscall_unlinkat(struct w2c_env* context, u32 arg0, u64 arg1, u32 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.__syscall_unlinkat", values, 3);
  return (u32)returned;
}
void w2c_env_0x5Fabort_js(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env._abort_js", values, 0);
  (void)returned;
}
void w2c_env_0x5Femscripten_init_main_thread_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_init_main_thread_js", values, 1);
  (void)returned;
}
void w2c_env_0x5Femscripten_notify_mailbox_postmessage(struct w2c_env* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_notify_mailbox_postmessage", values, 2);
  (void)returned;
}
f64 w2c_env_0x5Femscripten_receive_on_main_thread_js(struct w2c_env* context, u32 arg0, u64 arg1, u64 arg2, u32 arg3, u64 arg4, u64 arg5, u64 arg6) {
  uint64_t values[7];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  values[5] = arg5;
  values[6] = arg6;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_receive_on_main_thread_js", values, 7);
  f64 value; memcpy(&value, &returned, 8); return value;
}
u32 w2c_env_0x5Femscripten_system(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_system", values, 1);
  return (u32)returned;
}
void w2c_env_0x5Femscripten_thread_cleanup(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_thread_cleanup", values, 1);
  (void)returned;
}
void w2c_env_0x5Femscripten_thread_mailbox_await(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_thread_mailbox_await", values, 1);
  (void)returned;
}
void w2c_env_0x5Femscripten_thread_set_strongref(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env._emscripten_thread_set_strongref", values, 1);
  (void)returned;
}
void w2c_env_0x5Femscripten_throw_longjmp(struct w2c_env* context) {
  if (!gta_native_current_jump) gta_native_fail(context->thread, "longjmp outside invoke");
  longjmp(gta_native_current_jump->destination, 1);
}
u32 w2c_env_0x5Fgmtime_js(struct w2c_env* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env._gmtime_js", values, 2);
  return (u32)returned;
}
u32 w2c_env_0x5Flocaltime_js(struct w2c_env* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env._localtime_js", values, 2);
  return (u32)returned;
}
u64 w2c_env_0x5Fmktime_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env._mktime_js", values, 1);
  return (u64)returned;
}
void w2c_env_0x5Ftzset_js(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "env._tzset_js", values, 4);
  (void)returned;
}
u32 w2c_env_emscripten_asm_const_int(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_asm_const_int", values, 3);
  return (u32)returned;
}
u32 w2c_env_emscripten_asm_const_int_sync_on_main_thread(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_asm_const_int_sync_on_main_thread", values, 3);
  return (u32)returned;
}
void w2c_env_emscripten_check_blocking_allowed(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_check_blocking_allowed", values, 0);
  (void)returned;
}
f64 w2c_env_emscripten_date_now(struct w2c_env* context) {
  return gta_native_now_ms(context->thread);
}
void w2c_env_emscripten_exit_with_live_runtime(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_exit_with_live_runtime", values, 0);
  (void)returned;
}
u64 w2c_env_emscripten_get_heap_max(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_get_heap_max", values, 0);
  return (u64)returned;
}
f64 w2c_env_emscripten_get_now(struct w2c_env* context) {
  return gta_native_now_ms(context->thread);
}
u32 w2c_env_emscripten_num_logical_cores(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_num_logical_cores", values, 0);
  return (u32)returned;
}
u32 w2c_env_emscripten_resize_heap(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_resize_heap", values, 1);
  return (u32)returned;
}
u64 w2c_env_emscripten_stack_snapshot(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_stack_snapshot", values, 0);
  return (u64)returned;
}
u32 w2c_env_emscripten_stack_unwind_buffer(struct w2c_env* context, u64 arg0, u64 arg1, u32 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.emscripten_stack_unwind_buffer", values, 3);
  return (u32)returned;
}
void w2c_env_exit(struct w2c_env* context, u32 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.exit", values, 1);
  (void)returned;
}
u32 w2c_env_invoke_ij(struct w2c_env* context, u64 arg0, u64 arg1) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(1, 1, WASM_RT_I64, WASM_RT_I32);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    u32 returned = ((u32 (*)(void *, u64))entry.func)(instance, arg1);
    gta_native_current_jump = frame.previous;
    return returned;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return 0;
}
u32 w2c_env_invoke_iji(struct w2c_env* context, u64 arg0, u64 arg1, u32 arg2) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(2, 1, WASM_RT_I64, WASM_RT_I32, WASM_RT_I32);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    u32 returned = ((u32 (*)(void *, u64, u32))entry.func)(instance, arg1, arg2);
    gta_native_current_jump = frame.previous;
    return returned;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return 0;
}
u32 w2c_env_invoke_ijji(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2, u32 arg3) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(3, 1, WASM_RT_I64, WASM_RT_I64, WASM_RT_I32, WASM_RT_I32);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    u32 returned = ((u32 (*)(void *, u64, u64, u32))entry.func)(instance, arg1, arg2, arg3);
    gta_native_current_jump = frame.previous;
    return returned;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return 0;
}
u32 w2c_env_invoke_ijjiii(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2, u32 arg3, u32 arg4, u32 arg5) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(5, 1, WASM_RT_I64, WASM_RT_I64, WASM_RT_I32, WASM_RT_I32, WASM_RT_I32, WASM_RT_I32);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    u32 returned = ((u32 (*)(void *, u64, u64, u32, u32, u32))entry.func)(instance, arg1, arg2, arg3, arg4, arg5);
    gta_native_current_jump = frame.previous;
    return returned;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return 0;
}
u64 w2c_env_invoke_jjiiijij(struct w2c_env* context, u64 arg0, u64 arg1, u32 arg2, u32 arg3, u32 arg4, u64 arg5, u32 arg6, u64 arg7) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(7, 1, WASM_RT_I64, WASM_RT_I32, WASM_RT_I32, WASM_RT_I32, WASM_RT_I64, WASM_RT_I32, WASM_RT_I64, WASM_RT_I64);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    u64 returned = ((u64 (*)(void *, u64, u32, u32, u32, u64, u32, u64))entry.func)(instance, arg1, arg2, arg3, arg4, arg5, arg6, arg7);
    gta_native_current_jump = frame.previous;
    return returned;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return 0;
}
u64 w2c_env_invoke_jjij(struct w2c_env* context, u64 arg0, u64 arg1, u32 arg2, u64 arg3) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(3, 1, WASM_RT_I64, WASM_RT_I32, WASM_RT_I64, WASM_RT_I64);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    u64 returned = ((u64 (*)(void *, u64, u32, u64))entry.func)(instance, arg1, arg2, arg3);
    gta_native_current_jump = frame.previous;
    return returned;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return 0;
}
void w2c_env_invoke_vj(struct w2c_env* context, u64 arg0, u64 arg1) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(1, 0, WASM_RT_I64);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    ((void (*)(void *, u64))entry.func)(instance, arg1);
    gta_native_current_jump = frame.previous;
    return;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return;
}
void w2c_env_invoke_vjii(struct w2c_env* context, u64 arg0, u64 arg1, u32 arg2, u32 arg3) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(3, 0, WASM_RT_I64, WASM_RT_I32, WASM_RT_I32);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    ((void (*)(void *, u64, u32, u32))entry.func)(instance, arg1, arg2, arg3);
    gta_native_current_jump = frame.previous;
    return;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return;
}
void w2c_env_invoke_vjij(struct w2c_env* context, u64 arg0, u64 arg1, u32 arg2, u64 arg3) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(3, 0, WASM_RT_I64, WASM_RT_I32, WASM_RT_I64);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    ((void (*)(void *, u64, u32, u64))entry.func)(instance, arg1, arg2, arg3);
    gta_native_current_jump = frame.previous;
    return;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return;
}
void w2c_env_invoke_vjj(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(2, 0, WASM_RT_I64, WASM_RT_I64);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    ((void (*)(void *, u64, u64))entry.func)(instance, arg1, arg2);
    gta_native_current_jump = frame.previous;
    return;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return;
}
void w2c_env_invoke_vjjii(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2, u32 arg3, u32 arg4) {
  w2c_gta5__native *instance = gta_native_instance(context->thread);
  wasm_rt_funcref_table_t *table = w2c_gta5__native_0x5F_indirect_function_table(instance);
  if (arg0 >= table->size) gta_native_fail(context->thread, "indirect index out of bounds");
  wasm_rt_funcref_t entry = table->data[arg0];
  wasm_rt_func_type_t type = wasm2c_gta5__native_get_func_type(4, 0, WASM_RT_I64, WASM_RT_I64, WASM_RT_I32, WASM_RT_I32);
  if (!type || !entry.func || !entry.func_type || memcmp(type, entry.func_type, 32))
    gta_native_fail(context->thread, "indirect function signature mismatch");
  GtaNativeJumpFrame frame;
  frame.previous = gta_native_current_jump;
  frame.stack = w2c_gta5__native_emscripten_stack_get_current(instance);
#if WASM_RT_STACK_DEPTH_COUNT
  frame.call_stack_depth = wasm_rt_call_stack_depth;
#endif
  gta_native_current_jump = &frame;
  if (setjmp(frame.destination) == 0) {
    ((void (*)(void *, u64, u64, u32, u32))entry.func)(instance, arg1, arg2, arg3, arg4);
    gta_native_current_jump = frame.previous;
    return;
  }
  gta_native_current_jump = frame.previous;
#if WASM_RT_STACK_DEPTH_COUNT
  wasm_rt_call_stack_depth = frame.call_stack_depth;
#endif
  w2c_gta5__native_0x5Femscripten_stack_restore(instance, frame.stack);
  w2c_gta5__native_setThrew(instance, 1, 0);
  return;
}
void w2c_env_wasm_audio_publish_js(struct w2c_env* context, u64 arg0, u32 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_audio_publish_js", values, 2);
  (void)returned;
}
void w2c_env_wasm_hang_line_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_hang_line_js", values, 1);
  (void)returned;
}
u32 w2c_env_wasm_has_page_js(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_has_page_js", values, 0);
  return (u32)returned;
}
void w2c_env_wasm_httpfs_io_start_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_httpfs_io_start_js", values, 1);
  (void)returned;
}
u32 w2c_env_wasm_httpfs_manifest_js(struct w2c_env* context, u64 arg0, u32 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_httpfs_manifest_js", values, 2);
  return (u32)returned;
}
void w2c_env_wasm_input_publish_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_input_publish_js", values, 1);
  (void)returned;
}
u32 w2c_env_wasm_module_int_js(struct w2c_env* context, u64 arg0, u32 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_module_int_js", values, 2);
  return (u32)returned;
}
f64 w2c_env_wasm_now_ms(struct w2c_env* context) {
  return gta_native_now_ms(context->thread);
}
void w2c_env_wasm_print_line_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_print_line_js", values, 1);
  (void)returned;
}
void w2c_env_wasm_userdata_delete_js(struct w2c_env* context, u64 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_userdata_delete_js", values, 1);
  (void)returned;
}
u32 w2c_env_wasm_userdata_page_js(struct w2c_env* context) {
  const uint64_t *values = NULL;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_userdata_page_js", values, 0);
  return (u32)returned;
}
void w2c_env_wasm_userdata_put_js(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2, f64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  memcpy(&values[3], &arg3, 8);
  uint64_t returned = gta_native_dispatch(context->thread, "env.wasm_userdata_put_js", values, 4);
  (void)returned;
}
void w2c_env_wgpu_start_worker(struct w2c_env* context, u64 arg0, u64 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "env.wgpu_start_worker", values, 3);
  (void)returned;
}
u32 w2c_wasi__snapshot__preview1_clock_time_get(struct w2c_wasi__snapshot__preview1* context, u32 arg0, u64 arg1, u64 arg2) {
  uint64_t values[3];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.clock_time_get", values, 3);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_environ_get(struct w2c_wasi__snapshot__preview1* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.environ_get", values, 2);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_environ_sizes_get(struct w2c_wasi__snapshot__preview1* context, u64 arg0, u64 arg1) {
  uint64_t values[2];
  values[0] = arg0;
  values[1] = arg1;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.environ_sizes_get", values, 2);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_fd_close(struct w2c_wasi__snapshot__preview1* context, u32 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.fd_close", values, 1);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_fd_pread(struct w2c_wasi__snapshot__preview1* context, u32 arg0, u64 arg1, u64 arg2, u64 arg3, u64 arg4) {
  uint64_t values[5];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.fd_pread", values, 5);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_fd_pwrite(struct w2c_wasi__snapshot__preview1* context, u32 arg0, u64 arg1, u64 arg2, u64 arg3, u64 arg4) {
  uint64_t values[5];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  values[4] = arg4;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.fd_pwrite", values, 5);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_fd_read(struct w2c_wasi__snapshot__preview1* context, u32 arg0, u64 arg1, u64 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.fd_read", values, 4);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_fd_seek(struct w2c_wasi__snapshot__preview1* context, u32 arg0, u64 arg1, u32 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.fd_seek", values, 4);
  return (u32)returned;
}
u32 w2c_wasi__snapshot__preview1_fd_write(struct w2c_wasi__snapshot__preview1* context, u32 arg0, u64 arg1, u64 arg2, u64 arg3) {
  uint64_t values[4];
  values[0] = arg0;
  values[1] = arg1;
  values[2] = arg2;
  values[3] = arg3;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.fd_write", values, 4);
  return (u32)returned;
}
void w2c_wasi__snapshot__preview1_proc_exit(struct w2c_wasi__snapshot__preview1* context, u32 arg0) {
  uint64_t values[1];
  values[0] = arg0;
  uint64_t returned = gta_native_dispatch(context->thread, "wasi_snapshot_preview1.proc_exit", values, 1);
  (void)returned;
}
