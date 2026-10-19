//! Android AAudio worker for the direct C engine's shared audio ring.
//!
//! The worker reads only through the checked `NativeMmapMemory` view. On a
//! non-Android build startup fails explicitly; it never reports fake audio.

use crate::{io_host::Heap, memory::NativeMmapMemory};
use anyhow::{anyhow, Result};
use gta5_native_platform::GuestMemory;
use std::{sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, thread::JoinHandle, time::{Duration, Instant}};

#[derive(Clone,Debug,Default)]
pub struct AudioReport {
    /// `true` while the worker is negotiating and starting the AAudio stream.
    /// Stream startup can take several seconds on a cold Android audio path.
    pub initializing:bool,
    pub frames_written:u64,
    pub underrun_bursts:u64,
    pub xruns:i32,
    /// Streams reopened after AAudio dropped one (route change, device taken).
    pub reconnects:u32,
    pub failure:Option<String>,
}

#[cfg(target_os="android")]
#[link(name="aaudio")]
unsafe extern "C" {
    fn AAudio_createStreamBuilder(builder:*mut *mut std::ffi::c_void)->i32;
    fn AAudioStreamBuilder_delete(builder:*mut std::ffi::c_void)->i32;
    fn AAudioStreamBuilder_setDirection(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_setFormat(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_setChannelCount(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_setSampleRate(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_setSharingMode(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_setPerformanceMode(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_setUsage(builder:*mut std::ffi::c_void,value:i32);
    fn AAudioStreamBuilder_openStream(builder:*mut std::ffi::c_void,stream:*mut *mut std::ffi::c_void)->i32;
    fn AAudioStream_getSampleRate(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_getChannelCount(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_getFormat(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_getFramesPerBurst(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_getXRunCount(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_requestStart(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_requestStop(stream:*mut std::ffi::c_void)->i32;
    fn AAudioStream_write(stream:*mut std::ffi::c_void,buffer:*const std::ffi::c_void,frames:i32,timeout_ns:i64)->i32;
    fn AAudioStream_close(stream:*mut std::ffi::c_void)->i32;
}

#[cfg(target_os="android")]
struct Stream(*mut std::ffi::c_void);
#[cfg(target_os="android")]
impl Stream {
    fn open()->Result<Self> {
        unsafe {
            let mut builder=std::ptr::null_mut();
            anyhow::ensure!(AAudio_createStreamBuilder(&mut builder)==0 && !builder.is_null(),"AAudio builder creation failed");
            AAudioStreamBuilder_setDirection(builder,0);AAudioStreamBuilder_setFormat(builder,2);
            AAudioStreamBuilder_setChannelCount(builder,2);AAudioStreamBuilder_setSampleRate(builder,48_000);
            AAudioStreamBuilder_setSharingMode(builder,1);AAudioStreamBuilder_setPerformanceMode(builder,12);
            AAudioStreamBuilder_setUsage(builder,14);
            let mut pointer=std::ptr::null_mut();let status=AAudioStreamBuilder_openStream(builder,&mut pointer);
            AAudioStreamBuilder_delete(builder);
            anyhow::ensure!(status==0 && !pointer.is_null(),"AAudio open failed: {status}");
            let stream=Self(pointer);
            anyhow::ensure!(AAudioStream_getSampleRate(pointer)==48_000 && AAudioStream_getChannelCount(pointer)==2 && AAudioStream_getFormat(pointer)==2,"AAudio negotiated incompatible mixer format");
            let status=AAudioStream_requestStart(pointer);anyhow::ensure!(status==0,"AAudio start failed: {status}");
            Ok(stream)
        }
    }
}
#[cfg(target_os="android")]
impl Drop for Stream {fn drop(&mut self){unsafe{AAudioStream_requestStop(self.0);AAudioStream_close(self.0);}}}

pub struct AudioWorker {
    stop:Arc<AtomicBool>,
    failed:Arc<AtomicBool>,
    report:Arc<Mutex<AudioReport>>,
    worker:Option<JoinHandle<()>>,
}
impl AudioWorker {
    #[cfg(target_os="android")]
    pub fn start(memory:NativeMmapMemory,ring:u64,capacity:u32)->Result<Self> {
        anyhow::ensure!(capacity.is_power_of_two() && capacity<=1_048_576,"Invalid native audio ring capacity");
        let heap=Heap(memory);heap.checked(ring,64+capacity as usize*8,4)?;
        anyhow::ensure!(heap.load_i32(ring+8)? as u32==capacity,"Audio ring capacity header mismatch");
        let stop=Arc::new(AtomicBool::new(false));
        let report=Arc::new(Mutex::new(AudioReport{initializing:true,..AudioReport::default()}));
        let failed=Arc::new(AtomicBool::new(false));let worker_failed=failed.clone();
        let s=stop.clone();let r=report.clone();
        let worker=std::thread::Builder::new().name("gta-native-audio".into()).spawn(move || {
            let result=(||->Result<()> {
                // AAudio disconnects a stream on route changes (headphones, Bluetooth, USB) or when
                // the exclusive device is taken: reopen it; meanwhile the ring keeps draining in real time.
                let mut stream:Option<Stream>=None;
                let mut retry_at=Instant::now();
                let (mut burst,mut played,mut underruns,mut reconnects,mut opened_once)=(256usize,0u64,0u64,0u32,false);
                let mut samples=vec![0f32;burst*2];
                while !s.load(Ordering::Acquire) {
                    if stream.is_none() && Instant::now()>=retry_at {
                        match Stream::open() {
                            Ok(opened)=>{
                                burst=unsafe{AAudioStream_getFramesPerBurst(opened.0)}.clamp(64,1024) as usize;
                                samples.resize(burst*2,0.0);
                                if opened_once {reconnects+=1;if gta5_native_renderer::logging() {crate::services::android_log_line(&format!("[audio] AAudio stream reopened ({reconnects})"),false);}}
                                opened_once=true;
                                stream=Some(opened);
                            }
                            Err(error)=>{crate::services::android_log_line(&format!("[audio] {error:#}; retrying"),true);retry_at=Instant::now()+Duration::from_millis(500);}
                        }
                        r.lock().map_err(|_|anyhow!("Audio report poisoned"))?.initializing=false;
                    }
                    let read=heap.load_i32(ring+4)? as u32;let write=heap.load_i32(ring)? as u32;
                    let available=write.wrapping_sub(read);anyhow::ensure!(available<=capacity,"Audio ring producer overrun");
                    let take=(available as usize).min(burst);samples.fill(0.0);
                    let Some(open)=stream.as_ref() else {
                        // No device: consume one burst of frames per burst of real time.
                        heap.store_i32(ring+4,read.wrapping_add(take as u32) as i32)?;
                        std::thread::sleep(Duration::from_micros(burst as u64*1_000_000/48_000));
                        continue;
                    };
                    for i in 0..take {
                        let at=ring+64+u64::from((read.wrapping_add(i as u32))&(capacity-1))*8;
                        samples[i*2]=f32::from_bits(heap.load_i32(at)? as u32);
                        samples[i*2+1]=f32::from_bits(heap.load_i32(at+4)? as u32);
                    }
                    let written=unsafe{AAudioStream_write(open.0,samples.as_ptr().cast(),burst as i32,20_000_000)};
                    if written<0 {
                        crate::services::android_log_line(&format!("[audio] AAudio output failed: {written}; reopening"),true);
                        stream=None;
                        retry_at=Instant::now();
                        continue;
                    }
                    if written==0 {std::thread::sleep(Duration::from_millis(1));continue;}
                    let consumed=take.min(written as usize);
                    heap.store_i32(ring+4,read.wrapping_add(consumed as u32) as i32)?;
                    let heartbeat=heap.load_i32(ring+12)?;heap.store_i32(ring+12,heartbeat.wrapping_add(1))?;
                    let peak=samples[..consumed*2].iter().fold(0f32,|n,v|n.max(v.abs()));
                    if peak>0.0 {let old=heap.load_i32(ring+16)?;heap.store_i32(ring+16,old.max((peak*1e6) as i32))?;}
                    played+=written as u64;if consumed<written as usize {underruns+=1;}
                    if played%48_000 < written as u64 {
                        *r.lock().map_err(|_|anyhow!("Audio report poisoned"))?=AudioReport{initializing:false,frames_written:played,underrun_bursts:underruns,xruns:unsafe{AAudioStream_getXRunCount(open.0)},reconnects,failure:None};
                    }
                }
                Ok(())
            })();
            if let Err(error)=result {
                let message=format!("{error:#}");
                if let Ok(mut report)=r.lock(){report.initializing=false;report.failure=Some(message);}
                worker_failed.store(true,Ordering::Release);
            }
        })?;
        Ok(Self{stop,failed,report,worker:Some(worker)})
    }
    #[cfg(not(target_os="android"))]
    pub fn start(_memory:NativeMmapMemory,_ring:u64,_capacity:u32)->Result<Self> {Err(anyhow!("AAudio requires Android"))}
    pub fn report(&self)->AudioReport {self.report.lock().map(|value|value.clone()).unwrap_or_else(|_|AudioReport{failure:Some("Audio report mutex poisoned".into()),..AudioReport::default()})}
    pub fn check(&self)->Result<()> {if self.failed.load(Ordering::Acquire){return Err(anyhow!("Native audio worker failed: {:?}",self.report().failure));}Ok(())}
}
impl Drop for AudioWorker {fn drop(&mut self){self.stop.store(true,Ordering::Release);if let Some(worker)=self.worker.take(){let _=worker.join();}}}
