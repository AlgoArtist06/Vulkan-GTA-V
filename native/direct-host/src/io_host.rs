//! Direct-native asset and save I/O worker.
//!
//! The C engine owns the mapped guest heap. This module uses checked atomic
//! accesses through `NativeMmapMemory`; it has no VM or browser dependency.

use crate::memory::NativeMmapMemory;
use crate::syscalls::{GuestMemory as SyscallMemory, SyscallError};
use anyhow::{anyhow, Result};
use gta5_native_platform::{AssetStore, GuestMemory, NativeIoService, PlatformError, SaveStore};
use gta5_native_renderer::SharedMemory;
use std::{sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, thread::JoinHandle, time::Duration};

#[derive(Clone)]
pub struct Heap(pub NativeMmapMemory);

impl Heap {
    pub fn checked(&self, offset:u64, len:usize, align:usize)->Result<usize,PlatformError> {
        let index=usize::try_from(offset).map_err(|_|PlatformError::OffsetOverflow)?;
        let end=index.checked_add(len).ok_or(PlatformError::OffsetOverflow)?;
        if end>self.0.len() || index%align!=0 {return Err(PlatformError::InvalidIoTable("guest memory range or alignment".into()));}
        Ok(index)
    }
    pub fn read_bytes(&self, offset:u64, out:&mut [u8])->Result<(),PlatformError> {
        let index=self.checked(offset,out.len(),1)?;
        self.0.read(index,out).map_err(|error|PlatformError::Io(error.to_string()))
    }
    fn memory_error(&self,error:impl std::fmt::Display)->PlatformError {PlatformError::Io(error.to_string())}
}

impl SyscallMemory for Heap {
    fn len_bytes(&self)->u64 {self.0.len() as u64}
    fn read_u8(&self,offset:u64)->Result<u8,SyscallError> {
        let index=usize::try_from(offset).map_err(|_|SyscallError::memory("guest offset overflow"))?;
        let mut byte=[0];self.0.read(index,&mut byte).map_err(|error|SyscallError::memory(error.to_string()))?;Ok(byte[0])
    }
    fn write_u8(&self,offset:u64,value:u8)->Result<(),SyscallError> {
        let index=usize::try_from(offset).map_err(|_|SyscallError::memory("guest offset overflow"))?;
        self.0.write(index,&[value]).map_err(|error|SyscallError::memory(error.to_string()))
    }
}

impl GuestMemory for Heap {
    fn len_bytes(&self)->u64 {self.0.len() as u64}
    fn load_i32(&self,offset:u64)->Result<i32,PlatformError> {
        let index=self.checked(offset,4,4)?;
        self.0.load_u32(index,Ordering::Acquire).map(|value|value as i32).map_err(|error|self.memory_error(error))
    }
    fn compare_exchange_i32(&self,offset:u64,expected:i32,replacement:i32)->Result<i32,PlatformError> {
        let index=self.checked(offset,4,4)?;
        self.0.compare_exchange_u32(index,expected as u32,replacement as u32,Ordering::AcqRel,Ordering::Acquire)
            .map(|value|value as i32).map_err(|error|self.memory_error(error))
    }
    fn store_i32(&self,offset:u64,value:i32)->Result<(),PlatformError> {
        let index=self.checked(offset,4,4)?;
        self.0.store_u32(index,value as u32,Ordering::Release).map_err(|error|self.memory_error(error))
    }
    fn write_bytes(&self,offset:u64,bytes:&[u8])->Result<(),PlatformError> {
        let index=self.checked(offset,bytes.len(),1)?;
        self.0.write(index,bytes).map_err(|error|self.memory_error(error))
    }
    fn notify_i32(&self,offset:u64,waiters:u32)->Result<(),PlatformError> {
        let index=self.checked(offset,4,4)?;
        self.0.notify(index,waiters).map_err(|error|self.memory_error(error))
    }
}

#[derive(Clone,Debug,Default)]
pub struct IoReport {
    pub data_bytes:u64,
    pub failure:Option<String>,
}

pub struct IoWorker {
    heap:Heap,
    table:u64,
    stop:Arc<AtomicBool>,
    failed:Arc<AtomicBool>,
    report:Arc<Mutex<IoReport>>,
    worker:Option<JoinHandle<()>>,
}
impl IoWorker {
    pub fn start(memory:NativeMmapMemory,table:u64,assets:AssetStore,saves:SaveStore)->Result<Self> {
        let heap=Heap(memory);let mut service=NativeIoService::new(table,assets,saves)?;
        service.ready(&heap)?;
        let stop=Arc::new(AtomicBool::new(false));let report=Arc::new(Mutex::new(IoReport::default()));
        let failed=Arc::new(AtomicBool::new(false));let worker_failed=failed.clone();
        let h=heap.clone();let s=stop.clone();let r=report.clone();
        let worker=std::thread::Builder::new().name("gta-native-io".into()).spawn(move || {
            while !s.load(Ordering::Acquire) {
                let outcome=(||->Result<()> {
                    let seen=h.load_i32(table)?;
                    service.process_available(&h)?;
                    let mut report=r.lock().map_err(|_|anyhow!("I/O report lock poisoned"))?;
                    *report=IoReport{data_bytes:service.assets().disk_bytes(),failure:None};
                    drop(report);
                    let _=h.0.wait_u32(usize::try_from(table).map_err(|_|anyhow!("I/O table offset overflow"))?,seen as u32,Duration::from_millis(100))?;
                    Ok(())
                })();
                if let Err(error)=outcome {
                    let message=format!("{error:#}");
                    if let Ok(mut report)=r.lock(){report.failure=Some(message);}
                    worker_failed.store(true,Ordering::Release);
                    break;
                }
            }
        })?;
        Ok(Self{heap,table,stop,failed,report,worker:Some(worker)})
    }
    pub fn report(&self)->IoReport {self.report.lock().map(|value|value.clone()).unwrap_or_else(|_|IoReport{failure:Some("I/O report mutex poisoned".into()),..IoReport::default()})}
    pub fn check(&self)->Result<()> {
        if self.failed.load(Ordering::Acquire) {return Err(anyhow!("Native I/O worker failed: {:?}",self.report().failure));}
        Ok(())
    }
}
impl Drop for IoWorker {
    fn drop(&mut self){
        self.stop.store(true,Ordering::Release);
        if let Ok(table)=usize::try_from(self.table){let _=self.heap.0.notify(table,u32::MAX);}
        if let Some(worker)=self.worker.take(){let _=worker.join();}
    }
}
