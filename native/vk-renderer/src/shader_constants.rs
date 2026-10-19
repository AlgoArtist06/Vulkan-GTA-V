//! The preserved shader ABI stores one scalar word per 16-byte uniform slot.
pub(crate) fn pack(words: &[u32]) -> Result<Vec<u8>, String> {
    let bytes = words.len().max(1).checked_mul(16).ok_or("constant table size overflow")?;
    let mut result = Vec::new();
    result.try_reserve_exact(bytes.max(16)).map_err(|_| "constant table allocation failed")?;
    result.resize(bytes.max(16), 0);
    for (i, word) in words.iter().enumerate() { result[i*16..i*16+4].copy_from_slice(&word.to_le_bytes()); }
    Ok(result)
}

