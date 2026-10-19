//! Reuse completed texture-backup extents without overwriting live or pending backups.

use std::ops::Range;

#[derive(Default)]
pub(crate) struct BackingSpace {
    end: u64,
    free: Vec<Range<u64>>,
    largest_free: u64,
}

impl BackingSpace {
    pub(crate) fn high_water(&self) -> u64 { self.end }
    pub(crate) fn reusable_bytes(&self) -> u64 { self.free.iter().map(|range| range.end - range.start).sum() }

    pub(crate) fn can_allocate(&self, size: u64, limit: u64) -> bool {
        // The renderer scans many textures against one fixed cap. Keep that check constant-time.
        let reusable = if self.end <= limit { self.largest_free >= size } else {
            self.free.iter().any(|range| range.start.checked_add(size).is_some_and(|end| end <= range.end && end <= limit))
        };
        size != 0 && (reusable
            || self.end.checked_add(size).is_some_and(|end| end <= limit))
    }

    pub(crate) fn allocate(&mut self, size: u64, limit: u64) -> Option<u64> {
        if size == 0 { return None; }
        let slot = self.free.iter().enumerate().filter(|(_, range)| range.start.checked_add(size).is_some_and(|end| end <= range.end && end <= limit))
            .min_by_key(|(_, range)| range.end - range.start).map(|(index, _)| index);
        if let Some(index) = slot {
            let offset = self.free[index].start;
            self.free[index].start += size;
            if self.free[index].is_empty() { self.free.remove(index); }
            self.refresh_largest_free();
            return Some(offset);
        }
        let end = self.end.checked_add(size).filter(|end| *end <= limit)?;
        let offset = self.end;
        self.end = end;
        Some(offset)
    }

    /// The caller must retain ownership until every write/read of this extent is finished.
    /// Reject invalid or duplicate returns without making live contents allocatable.
    pub(crate) fn release(&mut self, offset: u64, size: u64) -> bool {
        let Some(end) = offset.checked_add(size) else { return false; };
        if size == 0 || end > self.end || self.free.iter().any(|range| offset < range.end && range.start < end) {
            return false;
        }
        if self.free.try_reserve(1).is_err() { return false; }
        let mut index = self.free.partition_point(|range| range.start < offset);
        self.free.insert(index, offset..end);
        if index > 0 && self.free[index - 1].end == offset {
            self.free[index - 1].end = end;
            self.free.remove(index);
            index -= 1;
        }
        if index + 1 < self.free.len() && self.free[index].end == self.free[index + 1].start {
            self.free[index].end = self.free[index + 1].end;
            self.free.remove(index + 1);
        }
        self.refresh_largest_free();
        true
    }

    fn refresh_largest_free(&mut self) {
        self.largest_free = self.free.iter().map(|range| range.end - range.start).max().unwrap_or(0);
    }
}


