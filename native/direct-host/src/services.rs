//! VM-free C ABI for the direct engine's platform imports.
//!
//! `Services` owns persistent host state. The C engine owns the native mapping
//! and must call destroy before it unmaps the heap.

use crate::{audio_host::AudioWorker, io_host::{Heap, IoWorker}, memory::NativeMmapMemory, syscalls::{DispatchResult, DispatchValue, FsConfig, GuestMemory as SyscallMemory, SyscallService}};
use gta5_native_input::{offsets as input_offsets, SHARED_BLOCK_SIZE};
use gta5_native_platform::{AssetStore, GuestMemory as PlatformMemory, Manifest, SaveStore};
use std::{ffi::{c_char, c_void, CStr, CString}, path::PathBuf, ptr, sync::{atomic::{AtomicU64, Ordering}, Mutex}, time::{Instant, SystemTime, UNIX_EPOCH}};

#[cfg(target_os="android")]
#[link(name="log")]
unsafe extern "C" {
    fn __android_log_write(priority:i32,tag:*const c_char,text:*const c_char)->i32;
}

const ENVIRONMENT:&[&str]=&["USER=android","LOGNAME=android","PATH=/","PWD=/","HOME=/userdata","LANG=en_US.UTF-8","_=gta5-native"];
const INPUT_TEXT_CAPACITY:u32=64;
const APP_LINES:usize=512;
const ROOT_OWNED:&[&str]=&["exit","emscripten_exit_with_live_runtime","_abort_js","_emscripten_throw_longjmp","emscripten_resize_heap","emscripten_get_heap_max","emscripten_stack_snapshot","emscripten_stack_unwind_buffer","wgpu_start_worker","__pthread_create_js","_emscripten_thread_mailbox_await","_emscripten_notify_mailbox_postmessage","_emscripten_receive_on_main_thread_js","_emscripten_thread_cleanup","_emscripten_init_main_thread_js","emscripten_asm_const_int","emscripten_asm_const_int_sync_on_main_thread"];

struct Host {
    heap:NativeMmapMemory,
    assets_root:PathBuf,
    saves_root:PathBuf,
    started:Instant,
    manifest:Option<Vec<u8>>,
    fs:Option<SyscallService>,
    io:Option<IoWorker>,
    audio:Option<AudioWorker>,
    input_address:Option<u64>,
    viewport:(u32,u32),
    focused:bool,
    locked:bool,
    stdout_line:Vec<u8>,
    stderr_line:Vec<u8>,
    /// The title screen's start choice: 0 = ask, 1 = Story Mode, 2/3 = Sandbox Mode maps.
    start_mode:u32,
    /// Engine output lines for the app's title screen (the web page's progress channel).
    app_lines:std::collections::VecDeque<String>,
}
impl Host {
    fn heap(&self)->Heap {Heap(self.heap.clone())}
    fn fs(&mut self)->Result<&mut SyscallService,String> {
        if self.fs.is_none() {
            let mut config=FsConfig::new(self.saves_root.join("userdata"));config.game_root=Some(self.assets_root.clone());
            // Sandbox Mode savegames live in the browser build's in-memory MEMFS, so each launch starts with an empty /sandbox.
            let sandbox=self.saves_root.join("sandbox");
            match std::fs::remove_dir_all(&sandbox){Err(error) if error.kind()!=std::io::ErrorKind::NotFound=>return Err(format!("reset sandbox saves: {error}")),_=>{}}
            config.sandbox_root=Some(sandbox);
            self.fs=Some(SyscallService::new(config).map_err(|error|error.message)?);
        }
        self.fs.as_mut().ok_or_else(||"filesystem service unavailable".into())
    }
    fn manifest(&mut self)->Result<&[u8],String> {
        if self.manifest.is_none() {
            let source=std::fs::read(self.assets_root.join("manifest.json")).map_err(|error|format!("read manifest: {error}"))?;
            let parsed=Manifest::from_json(&source).map_err(|error|error.to_string())?;
            self.manifest=Some(parsed.to_json_bytes().map_err(|error|error.to_string())?);
        }
        self.manifest.as_deref().ok_or_else(||"manifest unavailable".into())
    }
    fn check_workers(&self)->Result<(),String> {
        if let Some(io)=&self.io {io.check().map_err(|error|format!("{error:#}"))?;}
        if let Some(audio)=&self.audio {audio.check().map_err(|error|format!("{error:#}"))?;}
        Ok(())
    }
    fn read_string(&self,address:u64)->Result<String,String> {self.heap().read_c_string(address,4096).map_err(|error|error.message)}
    fn log(&mut self,address:u64,hang:bool)->Result<(),String> {
        // Engine log text may carry non-UTF-8 bytes (localized strings): never fatal.
        let text=self.heap().read_c_text(address,4096).map_err(|error|error.message)?;
        self.line(&text,hang);
        Ok(())
    }
    /// Emscripten's out()/err(): wasm_print_line_js, wasm_hang_line_js and stdio all print lines.
    fn line(&mut self,text:&str,hang:bool) {
        if self.app_lines.len()==APP_LINES {self.app_lines.pop_front();}
        self.app_lines.push_back(text.to_owned());
        if gta5_native_renderer::logging() {android_log_line(text,hang);}
    }
    fn flush_stdio(&mut self)->Result<(),String> {
        let fs=self.fs()?;let (stdout,stderr)=(fs.take_stdout(),fs.take_stderr());
        for line in stdio_lines(&mut self.stdout_line,&stdout) {self.line(&line,false);}
        for line in stdio_lines(&mut self.stderr_line,&stderr) {self.line(&line,true);}
        Ok(())
    }
    fn publish_input(&mut self,address:u64)->Result<(),String> {
        let heap=self.heap();
        initialize_input(&heap.0,address,self.viewport.0,self.viewport.1,self.focused,self.locked,self.start_mode)?;
        self.input_address=Some(address);
        Ok(())
    }
    fn set_viewport(&mut self,width:u32,height:u32)->Result<(),String> {
        self.viewport=normalize_viewport(width,height)?;
        if let Some(address)=self.input_address {
            let heap=self.heap();
            publish_viewport(&heap.0,address,self.viewport.0,self.viewport.1)?;
        }
        Ok(())
    }
    fn input_event(&mut self,kind:i32,code:i32,value:f32,value2:f32)->Result<(),String> {
        match kind {
            5=>{
                let focused=focus_state(code,value,value2)?;
                if let Some(address)=self.input_address {
                    let heap=self.heap();
                    apply_input_event(&heap.0,address,self.viewport,kind,code,value,value2)?;
                }
                self.focused=focused;
                self.locked=focused;
                Ok(())
            }
            6=>{
                if let Some(address)=self.input_address {
                    let heap=self.heap();
                    apply_input_event(&heap.0,address,self.viewport,kind,code,value,value2)?;
                }
                self.locked=false;
                Ok(())
            }
            9=>{
                self.start_mode=start_mode(code,value,value2)?;
                if let Some(address)=self.input_address {
                    let heap=self.heap();
                    apply_input_event(&heap.0,address,self.viewport,kind,code,value,value2)?;
                }
                Ok(())
            }
            _=>{
                let address=self.input_address.ok_or_else(||"input block is not bound".to_string())?;
                let heap=self.heap();
                apply_input_event(&heap.0,address,self.viewport,kind,code,value,value2)
            }
        }
    }
    fn dispatch(&mut self,name:&str,args:&[u64])->Result<u64,String> {
        self.check_workers()?;
        let bare=name.strip_prefix("env.").or_else(||name.strip_prefix("wasi_snapshot_preview1.")).unwrap_or(name);
        if ROOT_OWNED.contains(&bare) || bare.starts_with("invoke_") {return Err(format!("{name} belongs to the direct C root"));}
        if bare.starts_with("__syscall_") || matches!(bare,"fd_read"|"fd_write"|"fd_seek"|"fd_pread"|"fd_pwrite"|"fd_close"|"clock_time_get"|"_gmtime_js"|"_localtime_js"|"_mktime_js"|"_tzset_js") {
            let heap=self.heap();let result=self.fs()?.dispatch(name,args,&heap);
            if bare=="fd_write" {self.flush_stdio()?;}
            return match result {DispatchResult::Returned(DispatchValue::I32(value))=>Ok(value as u32 as u64),DispatchResult::Returned(DispatchValue::I64(value))=>Ok(value as u64),DispatchResult::Returned(DispatchValue::F64(value))=>Ok(value.to_bits()),DispatchResult::Trap(error)=>Err(error.message)};
        }
        match bare {
            "wasm_now_ms"|"emscripten_date_now"=>Ok(now_ms().to_bits()),
            "emscripten_get_now"=>Ok((now_ms()).to_bits()),
            "emscripten_num_logical_cores"=>Ok(std::thread::available_parallelism().map(|count|count.get() as u64).unwrap_or(1)),
            "wasm_module_int_js"=>{need(args,2,name)?;let _=self.heap().read_c_text(args[0],4096).map_err(|error|error.message)?;Ok(args[1] as u32 as u64)}
            "wasm_print_line_js"=>{need(args,1,name)?;self.log(args[0],false)?;Ok(0)}
            "wasm_hang_line_js"=>{need(args,1,name)?;self.log(args[0],true)?;Ok(0)}
            "emscripten_check_blocking_allowed"=>Ok(0),
            "wasm_has_page_js"=>Ok(1),
            "wasm_input_publish_js"=>{need(args,1,name)?;self.publish_input(args[0])?;Ok(0)}
            "wasm_audio_publish_js"=>{need(args,2,name)?;if self.audio.is_some(){return Err("audio ring already bound".into());}let capacity=u32::try_from(args[1]).map_err(|_|"audio capacity overflow")?;self.audio=Some(AudioWorker::start(self.heap.clone(),args[0],capacity).map_err(|error|format!("{error:#}"))?);Ok(0)}
            "wasm_httpfs_manifest_js"=>{need(args,2,name)?;let bytes=self.manifest()?.to_vec();let length=u64::try_from(bytes.len()).map_err(|_|"manifest too large")?;if args[1]>length {let heap=self.heap();PlatformMemory::write_bytes(&heap,args[0],&bytes).map_err(|error|error.to_string())?;PlatformMemory::write_bytes(&heap,args[0].checked_add(length).ok_or("manifest pointer overflow")?,&[0]).map_err(|error|error.to_string())?;}Ok(length)}
            "wasm_httpfs_io_start_js"=>{need(args,1,name)?;if self.io.is_some(){return Err("I/O table already bound".into());}let manifest=self.manifest()?.to_vec();let assets=AssetStore::from_manifest_json(&self.assets_root,&manifest).map_err(|error|error.to_string())?;let saves=SaveStore::new(&self.saves_root).map_err(|error|error.to_string())?;self.io=Some(IoWorker::start(self.heap.clone(),args[0],assets,saves).map_err(|error|format!("{error:#}"))?);Ok(0)}
            // 0: no browser page to mirror saves to, so the engine never posts wasm_userdata_put_js/delete_js. /userdata
            // is already the persistent save folder; answering 1 made the engine re-post every settled file each 2 s.
            "wasm_userdata_page_js"=>Ok(0),
            "environ_sizes_get"=>{need(args,2,name)?;let heap=self.heap();heap.write_u64(args[0],ENVIRONMENT.len() as u64).map_err(|error|error.message)?;heap.write_u64(args[1],ENVIRONMENT.iter().map(|item|item.len() as u64+1).sum()).map_err(|error|error.message)?;Ok(0)}
            "environ_get"=>{need(args,2,name)?;let heap=self.heap();let mut target=args[1];for(index,item) in ENVIRONMENT.iter().enumerate(){heap.write_u64(args[0]+index as u64*8,target).map_err(|error|error.message)?;SyscallMemory::write_bytes(&heap,target,item.as_bytes()).map_err(|error|error.message)?;target=target.checked_add(item.len() as u64).ok_or("environment pointer overflow")?;heap.write_u8(target,0).map_err(|error|error.message)?;target+=1;}Ok(0)}
            _=>Err(format!("native direct service not implemented: {name}")),
        }
    }
}

fn take_lines(lines:&mut std::collections::VecDeque<String>,capacity:usize)->String {
    let mut text=String::new();
    while let Some(line)=lines.front() {
        let mut take=line.len().min(capacity.saturating_sub(1));
        while !line.is_char_boundary(take) {take-=1;}
        if text.len()+take+1>capacity && !text.is_empty() {break;}
        if !text.is_empty() {text.push('\n');}
        text.push_str(&line[..take]);
        lines.pop_front();
    }
    text
}

/// Emscripten's printChar: bytes up to a newline or NUL make one line; the rest waits.
fn stdio_lines(pending:&mut Vec<u8>,bytes:&[u8])->Vec<String> {
    let mut lines=Vec::new();
    for &byte in bytes {
        if byte==b'\n'||byte==0 {lines.push(String::from_utf8_lossy(pending).into_owned());pending.clear();} else {pending.push(byte);}
    }
    lines
}

fn normalize_viewport(width:u32,height:u32)->Result<(u32,u32),String> {
    if width==0||height==0{return Err("viewport dimensions must be non-zero".into());}
    Ok((width.min(i32::MAX as u32),height.min(i32::MAX as u32)))
}

fn input_address(base:u64,offset:usize)->Result<usize,String> {
    let address=base.checked_add(offset as u64).ok_or_else(||"input address overflow".to_string())?;
    usize::try_from(address).map_err(|_|"input address does not fit usize".into())
}

fn initialize_input(heap:&NativeMmapMemory,base:u64,width:u32,height:u32,focused:bool,locked:bool,start_mode:u32)->Result<(),String> {
    heap.checked(base,SHARED_BLOCK_SIZE,4).map_err(|error|error.to_string())?;
    let (width,height)=normalize_viewport(width,height)?;
    publish_viewport(heap,base,width,height)?;
    heap.atomic_write_u32(input_address(base,input_offsets::FOCUS)?,focused as u32,Ordering::Release).map_err(|error|error.to_string())?;
    heap.atomic_write_u32(input_address(base,input_offsets::LOCKED)?,locked as u32,Ordering::Release).map_err(|error|error.to_string())?;
    heap.atomic_write_u32(input_address(base,input_offsets::START_MODE)?,start_mode,Ordering::Release).map_err(|error|error.to_string())?;
    Ok(())
}

fn start_mode(code:i32,value:f32,value2:f32)->Result<u32,String> {
    if value!=0.0||value2!=0.0{return Err("start mode event values must be zero".into());}
    match code {1..=3=>Ok(code as u32),_=>Err("start mode must be 1 (Story Mode), 2 (Sandbox Mode) or 3 (Sandbox Mode on env_test)".into())}
}

fn focus_state(code:i32,value:f32,value2:f32)->Result<bool,String> {
    if value!=0.0||value2!=0.0{return Err("focus event values must be zero".into());}
    match code {0=>Ok(false),1=>Ok(true),_=>Err("focus code must be 0 or 1".into())}
}

#[cfg(target_os="android")]
pub(crate) fn android_log_line(text:&str,hang:bool) {
    let Ok(text)=CString::new(text) else {return;};
    const TAG:&[u8]=b"GtaDirectEngine\0";
    unsafe {let _=__android_log_write(if hang {6} else {4},TAG.as_ptr().cast(),text.as_ptr());}
}

#[cfg(not(target_os="android"))]
pub(crate) fn android_log_line(_text:&str,_hang:bool) {}

fn publish_viewport(heap:&NativeMmapMemory,base:u64,width:u32,height:u32)->Result<(),String> {
    let (width,height)=normalize_viewport(width,height)?;
    heap.atomic_write_u32(input_address(base,input_offsets::WINDOW_WIDTH)?,width,Ordering::Release).map_err(|error|error.to_string())?;
    heap.atomic_write_u32(input_address(base,input_offsets::WINDOW_HEIGHT)?,height,Ordering::Release).map_err(|error|error.to_string())?;
    Ok(())
}

fn finite_i32(value:f32,name:&str)->Result<i32,String> {
    if !value.is_finite()||value<i32::MIN as f32||value>i32::MAX as f32{return Err(format!("{name} is outside finite i32 range"));}
    Ok(value.trunc() as i32)
}

fn clear_active_input(heap:&NativeMmapMemory,base:u64)->Result<(),String> {
    for offset in input_offsets::KEYS..input_offsets::KEYS+gta5_native_input::KEY_COUNT {
        heap.atomic_write_u8(input_address(base,offset)?,0,Ordering::Release).map_err(|error|error.to_string())?;
    }
    for offset in [input_offsets::MOUSE_BUTTONS,input_offsets::MOUSE_DX,input_offsets::MOUSE_DY,input_offsets::MOUSE_DWHEEL] {
        heap.atomic_write_u32(input_address(base,offset)?,0,Ordering::Release).map_err(|error|error.to_string())?;
    }
    heap.atomic_write_u32(input_address(base,input_offsets::LOCKED)?,0,Ordering::Release).map_err(|error|error.to_string())?;
    Ok(())
}

fn set_button(heap:&NativeMmapMemory,base:u64,mask:u32,down:bool)->Result<(),String> {
    let address=input_address(base,input_offsets::MOUSE_BUTTONS)?;
    for _ in 0..32 {
        let previous=heap.atomic_read_u32(address,Ordering::Acquire).map_err(|error|error.to_string())?;
        let next=if down {previous|mask} else {previous&!mask};
        let observed=heap.compare_exchange_u32(address,previous,next,Ordering::AcqRel,Ordering::Acquire).map_err(|error|error.to_string())?;
        if observed==previous{return Ok(());}
    }
    Err("mouse button update did not converge".into())
}

fn add_delta(heap:&NativeMmapMemory,base:u64,offset:usize,value:f32,name:&str)->Result<(),String> {
    let value=finite_i32(value,name)?;
    if value!=0 {heap.atomic_fetch_add_u32(input_address(base,offset)?,value as u32,Ordering::AcqRel).map_err(|error|error.to_string())?;}
    Ok(())
}

fn push_text_unit(heap:&NativeMmapMemory,base:u64,unit:u16)->Result<(),String> {
    let head_address=input_address(base,input_offsets::TEXT_HEAD)?;
    let tail_address=input_address(base,input_offsets::TEXT_TAIL)?;
    let head=heap.atomic_read_u32(head_address,Ordering::Acquire).map_err(|error|error.to_string())?;
    let tail=heap.atomic_read_u32(tail_address,Ordering::Acquire).map_err(|error|error.to_string())?;
    let queued=head.wrapping_sub(tail);
    if queued>INPUT_TEXT_CAPACITY {return Err("input text ring state is invalid".into());}
    if queued==INPUT_TEXT_CAPACITY {return Err("input text ring is full".into());}
    let slot=(head&(INPUT_TEXT_CAPACITY-1)) as usize;
    let address=input_address(base,input_offsets::TEXT+slot*2)?;
    let bytes=unit.to_le_bytes();
    heap.atomic_write_u8(address,bytes[0],Ordering::Relaxed).map_err(|error|error.to_string())?;
    heap.atomic_write_u8(address+1,bytes[1],Ordering::Relaxed).map_err(|error|error.to_string())?;
    heap.atomic_write_u32(head_address,head.wrapping_add(1),Ordering::Release).map_err(|error|error.to_string())?;
    Ok(())
}

/// Apply one UI event to the live 444-byte input block.
///
/// The UI owns per-source key/button reference counts and sends the resulting
/// 0/1 state.  Motion and wheel are additive because the engine consumes them
/// with an atomic exchange between host events.
fn apply_input_event(heap:&NativeMmapMemory,base:u64,viewport:(u32,u32),kind:i32,code:i32,value:f32,value2:f32)->Result<(),String> {
    heap.checked(base,SHARED_BLOCK_SIZE,4).map_err(|error|error.to_string())?;
    match kind {
        1=>{
            if !(value==0.0||value==1.0){return Err("key value must be 0 or 1".into());}
            let code=u8::try_from(code).map_err(|_|"virtual key must be in 0..255".to_string())?;
            heap.atomic_write_u8(input_address(base,input_offsets::KEYS+code as usize)?,if value==1.0{0x80}else{0},Ordering::Release).map_err(|error|error.to_string())
        }
        2=>{add_delta(heap,base,input_offsets::MOUSE_DX,value,"look dx")?;add_delta(heap,base,input_offsets::MOUSE_DY,value2,"look dy")}
        3=>{
            if !(value==0.0||value==1.0){return Err("mouse button value must be 0 or 1".into());}
            let mask=match code{0=>1,1=>2,2=>4,_=>return Err("mouse button must be left(0), right(1), or middle(2)".into())};
            set_button(heap,base,mask,value==1.0)
        }
        4=>add_delta(heap,base,input_offsets::MOUSE_DWHEEL,value,"wheel"),
        5=>{
            let focused=focus_state(code,value,value2)?;
            if !focused {
                clear_active_input(heap,base)?;
                heap.atomic_write_u32(input_address(base,input_offsets::FOCUS)?,0,Ordering::Release).map_err(|error|error.to_string())
            } else {
                heap.atomic_write_u32(input_address(base,input_offsets::FOCUS)?,1,Ordering::Release).map_err(|error|error.to_string())?;
                heap.atomic_write_u32(input_address(base,input_offsets::LOCKED)?,1,Ordering::Release).map_err(|error|error.to_string())
            }
        }
        6=>clear_active_input(heap,base),
        7=>{
            let unit=u16::try_from(code).map_err(|_|"text code unit must be in 0..65535".to_string())?;
            push_text_unit(heap,base,unit)
        }
        9=>heap.atomic_write_u32(input_address(base,input_offsets::START_MODE)?,start_mode(code,value,value2)?,Ordering::Release).map_err(|error|error.to_string()),
        8=>{
            let x=finite_i32(value,"pointer x")?.clamp(0,viewport.0.saturating_sub(1) as i32);
            let y=finite_i32(value2,"pointer y")?.clamp(0,viewport.1.saturating_sub(1) as i32);
            heap.atomic_write_u32(input_address(base,input_offsets::MOUSE_X)?,x as u32,Ordering::Release).map_err(|error|error.to_string())?;
            heap.atomic_write_u32(input_address(base,input_offsets::MOUSE_Y)?,y as u32,Ordering::Release).map_err(|error|error.to_string())
        }
        _=>Err(format!("unsupported native input event kind {kind}")),
    }
}

/// Opaque C-owned handle. Destroy it before C unmaps `base`.
pub struct Services {host:Mutex<Host>,error:Mutex<Vec<u8>>}
fn now_ms()->f64 {SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()*1000.0}
fn need(args:&[u64],count:usize,name:&str)->Result<(),String> {if args.len()<count{Err(format!("{name} needs {count} arguments"))}else{Ok(())}}
fn set_error(services:&Services,message:String) {if let Ok(mut error)=services.error.lock(){*error=message.into_bytes();}}
fn clear_error(services:&Services) {if let Ok(mut error)=services.error.lock(){error.clear();}}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_create(base:*mut c_void,reserved:usize,logical_size:*const AtomicU64,assets:*const c_char,saves:*const c_char)->*mut Services {
    let result=(||->Result<Services,String>{
        if assets.is_null()||saves.is_null(){return Err("asset and save roots are required".into());}
        let assets=unsafe{CStr::from_ptr(assets)}.to_str().map_err(|_|"asset root is not UTF-8")?;
        let saves=unsafe{CStr::from_ptr(saves)}.to_str().map_err(|_|"save root is not UTF-8")?;
        let heap=unsafe{NativeMmapMemory::from_raw_with_size(base.cast(),reserved,logical_size)}.map_err(|error|error.to_string())?;
        let saves_root=PathBuf::from(saves);
        Ok(Services{host:Mutex::new(Host{heap,assets_root:PathBuf::from(assets),saves_root,started:Instant::now(),manifest:None,fs:None,io:None,audio:None,input_address:None,viewport:(1,1),focused:true,locked:true,stdout_line:Vec::new(),stderr_line:Vec::new(),start_mode:0,app_lines:std::collections::VecDeque::new()}),error:Mutex::new(Vec::new())})
    })();
    match result {Ok(services)=>Box::into_raw(Box::new(services)),Err(_)=>ptr::null_mut()}
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_call(services:*mut Services,name:*const c_char,args:*const u64,argc:usize,result:*mut u64)->i32 {
    let Some(services)=(unsafe{services.as_ref()}) else{return -1;};
    let call=(||->Result<u64,String>{if name.is_null()||result.is_null()||(argc>0&&args.is_null()){return Err("invalid C service call pointers".into());}let name=unsafe{CStr::from_ptr(name)}.to_str().map_err(|_|"import name is not UTF-8")?;let args=if argc==0{&[][..]}else{unsafe{std::slice::from_raw_parts(args,argc)}};services.host.lock().map_err(|_|"service host mutex poisoned")?.dispatch(name,args)})();
    // The error is read only after a failure, which overwrites it: no lock on success.
    match call {Ok(value)=>{unsafe{*result=value};0}Err(error)=>{set_error(services,error);-1}}
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_last_error(services:*const Services,out:*mut c_char,capacity:usize)->usize {
    let Some(services)=(unsafe{services.as_ref()}) else{return 0;};let Ok(error)=services.error.lock() else{return 0;};
    if !out.is_null()&&capacity>0 {let count=error.len().min(capacity-1);unsafe{ptr::copy_nonoverlapping(error.as_ptr(),out.cast::<u8>(),count);*out.add(count)=0;}}
    error.len()
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_input_address(services:*const Services)->u64 {unsafe{services.as_ref()}.and_then(|services|services.host.lock().ok().and_then(|host|host.input_address)).unwrap_or(0)}
/// Publish one native UI event into the engine-owned input block.
///
/// `kind`: 1 key, 2 relative look, 3 mouse button, 4 wheel, 5 focus,
/// 6 release-all, 7 one UTF-16 code unit, 8 absolute pointer position.
/// Key and button `value` arguments are exactly 0 or 1.  Focus uses `code` 0/1
/// and requires both value arguments to be zero.  The UI is responsible for
/// source reference counts before sending those state changes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_input(services:*mut Services,kind:i32,code:i32,value:f32,value2:f32)->i32 {
    let Some(services)=(unsafe{services.as_ref()}) else{return -1;};
    let result=services.host.lock().map_err(|_|"service host mutex poisoned".to_string()).and_then(|mut host|host.input_event(kind,code,value,value2));
    match result {Ok(())=>{clear_error(services);0}Err(error)=>{let code=if error=="input text ring is full"{-2}else{-1};set_error(services,error);code}}
}
/// Store the render/input viewport.  The cached size is used when the engine
/// later calls `wasm_input_publish_js`; an already-bound block is updated now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_viewport(services:*mut Services,width:u32,height:u32)->i32 {
    let Some(services)=(unsafe{services.as_ref()}) else{return -1;};
    let result=services.host.lock().map_err(|_|"service host mutex poisoned".to_string()).and_then(|mut host|host.set_viewport(width,height));
    match result {Ok(())=>{clear_error(services);0}Err(error)=>{set_error(services,error);-1}}
}
/// Move whole engine output lines, newline-separated and NUL-terminated, into
/// `out`; lines that do not fit stay queued. Returns the bytes written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_take_lines(services:*mut Services,out:*mut c_char,capacity:usize)->usize {
    let Some(services)=(unsafe{services.as_ref()}) else{return 0;};
    if out.is_null()||capacity==0 {return 0;}
    let Ok(mut host)=services.host.lock() else{return 0;};
    let text=take_lines(&mut host.app_lines,capacity-1);
    unsafe{ptr::copy_nonoverlapping(text.as_ptr(),out.cast::<u8>(),text.len());*out.add(text.len())=0;}
    text.len()
}
/// Game data bytes the I/O worker has read from storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_data_bytes(services:*mut Services)->u64 {
    let Some(services)=(unsafe{services.as_ref()}) else{return 0;};
    services.host.lock().ok().and_then(|host|host.io.as_ref().map(|io|io.report().data_bytes)).unwrap_or(0)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gta_native_services_destroy(services:*mut Services) {if !services.is_null(){unsafe{drop(Box::from_raw(services));}}}


