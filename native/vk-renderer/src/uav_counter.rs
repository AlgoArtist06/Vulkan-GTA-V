//! Storage binding numbers are part of the preserved shader ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageSlot { Srv(usize), Uav(usize), Counter(usize) }
pub(crate) fn slot(binding: u32) -> Result<StorageSlot, String> {
    match binding {
        32..=159 => Ok(StorageSlot::Srv((binding-32) as usize)),
        160..=167 => Ok(StorageSlot::Uav((binding-160) as usize)),
        176..=183 => Ok(StorageSlot::Counter((binding-176) as usize)),
        _ => Err(format!("storage binding {binding} is outside the D3D resource ranges")),
    }
}
pub(crate) fn reset(value: u32) -> Option<u32> { (value != u32::MAX).then_some(value) }
pub(crate) fn copy_range(bytes: u64, offset: u64) -> Result<(), String> {
    if offset % 4 != 0 || offset.checked_add(4).is_none_or(|end| end > bytes) {
        Err("counter copy requires four aligned bytes within the destination".into())
    } else { Ok(()) }
}

