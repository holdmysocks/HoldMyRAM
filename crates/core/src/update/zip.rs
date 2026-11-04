//! A deliberately small ZIP reader for the release archive, with strict validation.
//! Uses `miniz_oxide` (raw deflate) and `crc32fast`, both already in the dependency tree.
//!
//! Accepted: a single-disk archive (no ZIP64, no encryption), methods stored (0) or deflate
//! (8), at most `MAX_ENTRIES` entries and `MAX_TOTAL` bytes uncompressed. Every file must
//! be one of `ALLOWED` (case-insensitive), either at the root or inside exactly one top
//! folder named after the release (`HoldMyRAM-<ver>-win-x64/`); `\` counts as `/` (Windows
//! PowerShell writes it). Refused: absolute paths, drive letters, `:` (alternate streams),
//! `..` or empty components, control characters, any other name, duplicates, and a local
//! header whose name differs from the central directory.

use super::UpdateError;

pub const MAX_ENTRIES: usize = 32;
pub const MAX_TOTAL: u64 = 64 * 1024 * 1024;
/// Names a release may contain (canonical spelling is what gets written).
pub const ALLOWED: &[&str] = &[
    "holdmyram.exe",
    "holdmyram-helper.exe",
    "README.txt",
    "LICENSE.txt",
    "THIRD-PARTY-NOTICES.txt",
    "Start here.txt",
    "CHANGELOG.txt",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Canonical allowed name, no folder.
    pub name: String,
    pub data: Vec<u8>,
}

fn bad(msg: impl Into<String>) -> UpdateError {
    UpdateError::new("bad_archive", msg)
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, UpdateError> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| bad("truncated archive"))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32, UpdateError> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("truncated archive"))
}

/// What an entry name means: Ok(None) for the top folder itself, Ok(Some(canonical)) for
/// an allowed file, Err for anything else.
pub fn classify_name(raw: &str, top: &str) -> Result<Option<&'static str>, UpdateError> {
    if raw.is_empty() || raw.chars().any(|c| c.is_control()) || raw.contains(':') {
        return Err(bad(format!("refusing entry {raw:?}")));
    }
    let n = raw.replace('\\', "/");
    if n.starts_with('/') {
        return Err(bad(format!("refusing absolute entry {raw:?}")));
    }
    let parts: Vec<&str> = n.split('/').collect();
    let (dir, file): (Option<&str>, &str) = match parts.as_slice() {
        [f] => (None, f),
        [d, ""] => return if d.eq_ignore_ascii_case(top) { Ok(None) } else { Err(bad(format!("unexpected folder {raw:?}"))) },
        [d, f] => (Some(d), f),
        _ => return Err(bad(format!("unexpected nested entry {raw:?}"))),
    };
    if let Some(d) = dir {
        if !d.eq_ignore_ascii_case(top) {
            return Err(bad(format!("unexpected folder in {raw:?}")));
        }
    }
    if file.is_empty() || file == "." || file == ".." {
        return Err(bad(format!("refusing entry {raw:?}")));
    }
    ALLOWED
        .iter()
        .copied()
        .find(|a| a.eq_ignore_ascii_case(file))
        .map(Some)
        .ok_or_else(|| bad(format!("unexpected file {raw:?} in the archive")))
}

/// Parse, validate and decompress every allowed file of `zip`.
pub fn read_validated(zip: &[u8], top: &str) -> Result<Vec<Entry>, UpdateError> {
    // End of central directory: the last signature within the final 64 KiB + 22 bytes.
    if zip.len() < 22 {
        return Err(bad("not a zip archive"));
    }
    let min = zip.len().saturating_sub(22 + 0xFFFF);
    let eocd = (min..=zip.len().saturating_sub(22))
        .rev()
        .find(|&i| zip[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| bad("not a zip archive"))?;
    let (disk, cd_disk) = (u16_at(zip, eocd + 4)?, u16_at(zip, eocd + 6)?);
    let (n_disk, n_total) = (u16_at(zip, eocd + 8)?, u16_at(zip, eocd + 10)?);
    let cd_size = u32_at(zip, eocd + 12)? as usize;
    let cd_off = u32_at(zip, eocd + 16)? as usize;
    if disk != 0 || cd_disk != 0 || n_disk != n_total || n_total == 0xFFFF || cd_off == 0xFFFF_FFFF {
        return Err(bad("multi-disk or ZIP64 archives are not accepted"));
    }
    if n_total as usize > MAX_ENTRIES {
        return Err(bad(format!("too many entries ({n_total} > {MAX_ENTRIES})")));
    }
    if cd_off.checked_add(cd_size).is_none_or(|e| e > eocd) {
        return Err(bad("central directory out of range"));
    }

    let mut out: Vec<Entry> = Vec::new();
    let mut total: u64 = 0;
    let mut p = cd_off;
    for _ in 0..n_total {
        if u32_at(zip, p)? != 0x0201_4b50 {
            return Err(bad("bad central directory entry"));
        }
        let flags = u16_at(zip, p + 8)?;
        let method = u16_at(zip, p + 10)?;
        let crc = u32_at(zip, p + 16)?;
        let csize = u32_at(zip, p + 20)? as u64;
        let usize_ = u32_at(zip, p + 24)? as u64;
        let nlen = u16_at(zip, p + 28)? as usize;
        let xlen = u16_at(zip, p + 30)? as usize;
        let clen = u16_at(zip, p + 32)? as usize;
        let local = u32_at(zip, p + 42)? as usize;
        let name_bytes = zip.get(p + 46..p + 46 + nlen).ok_or_else(|| bad("truncated archive"))?;
        let name = std::str::from_utf8(name_bytes).map_err(|_| bad("entry name is not UTF-8"))?.to_string();
        p += 46 + nlen + xlen + clen;

        if flags & 0x1 != 0 {
            return Err(bad(format!("{name:?} is encrypted")));
        }
        if csize == 0xFFFF_FFFF || usize_ == 0xFFFF_FFFF || local == 0xFFFF_FFFF {
            return Err(bad("ZIP64 entries are not accepted"));
        }
        let Some(canonical) = classify_name(&name, top)? else {
            if usize_ != 0 {
                return Err(bad(format!("folder entry {name:?} has data")));
            }
            continue;
        };
        if out.iter().any(|e| e.name == canonical) {
            return Err(bad(format!("duplicate entry {canonical}")));
        }
        total += usize_;
        if total > MAX_TOTAL {
            return Err(bad(format!("archive expands beyond {} MB", MAX_TOTAL / 1024 / 1024)));
        }

        // Local header must agree with the central directory.
        if u32_at(zip, local)? != 0x0403_4b50 {
            return Err(bad(format!("bad local header for {name:?}")));
        }
        let lnlen = u16_at(zip, local + 26)? as usize;
        let lxlen = u16_at(zip, local + 28)? as usize;
        if zip.get(local + 30..local + 30 + lnlen) != Some(name_bytes) {
            return Err(bad(format!("local header name differs for {name:?}")));
        }
        let start = local + 30 + lnlen + lxlen;
        let data = zip.get(start..start + csize as usize).ok_or_else(|| bad(format!("{name:?} data out of range")))?;
        let plain = match method {
            0 if csize == usize_ => data.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, usize_ as usize)
                .map_err(|_| bad(format!("{name:?} could not be decompressed")))?,
            _ => return Err(bad(format!("{name:?} uses an unsupported compression method {method}"))),
        };
        if plain.len() as u64 != usize_ {
            return Err(bad(format!("{name:?} size mismatch")));
        }
        if crc32fast::hash(&plain) != crc {
            return Err(bad(format!("{name:?} CRC mismatch")));
        }
        out.push(Entry { name: canonical.to_string(), data: plain });
    }
    Ok(out)
}
