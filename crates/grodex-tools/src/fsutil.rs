//! Filesystem helpers shared by read/write/edit/patch tools:
//!   - `FileVersion`: a content+hash fingerprint used as an edit fence
//!     (expected_file_version). Lets a caller refuse to write if the file
//!     changed since they last read it (lost-update protection).
//!   - `canonicalize`: resolves a path to its canonical absolute form,
//!     normalizing `..`, `.`, and symlinks.
//!   - `assert_no_symlink_escape`: refuses paths that resolve through a
//!     symlink outside the allowed root set — the symlink-escape defence
//!     the audit flagged as missing (§9/§10 canonical-path + symlink check).
//!
//! Kept dependency-free (only std) so it can be reused by all tools without
//! pulling extra crates.

use std::path::{Path, PathBuf};

/// A content fingerprint for a file, used as an edit/version fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    /// SHA-256 of the file contents at the time of the read.
    pub content_hash: String,
    /// Byte length at read time.
    pub size: u64,
    /// mtime (seconds since UNIX epoch) at read time, if available.
    pub mtime_secs: Option<i64>,
}

impl FileVersion {
    /// Compute a `FileVersion` for the file at `path`. Returns `None` if
    /// the file does not exist (callers treat None as "no fence").
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let content = std::fs::read(path).ok()?;
        let hash = sha256_hex(&content);
        let mtime_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        Some(Self {
            content_hash: hash,
            size: content.len() as u64,
            mtime_secs,
        })
    }

    /// True if `current` is consistent with this version (same hash). Used
    /// to detect a lost update: if the file changed since the caller read
    /// it, an edit must be refused rather than blindly applied.
    pub fn matches(&self, current: &FileVersion) -> bool {
        self.content_hash == current.content_hash
    }
}

/// SHA-256 of `data` as a lowercase hex string. Pure std (no sha2 dep here —
/// the algorithm is small enough inline for a fingerprint; for security-
/// critical hashing the loop crate uses the `sha2` crate).
pub fn sha256_hex(data: &[u8]) -> String {
    // Minimal SHA-256 (FIPS 180-4). Compiled once; not perf-critical for
    // fingerprinting tool args / file versions.
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    // Pad.
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e; e = d.wrapping_add(temp1);
            d = c; c = b; b = a; a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a); h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c); h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e); h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g); h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// Canonicalize a path: resolve to an absolute, normalized form, following
/// symlinks when they resolve (via `std::fs::canonicalize`). Falls back to
/// a lexical normalization (no symlink resolution) if the path doesn't
/// exist yet (e.g. a file we're about to create).
pub fn canonicalize(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    // Lexical normalization for not-yet-existing paths.
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Atomic write: create a temp file in the same directory as `target`,
/// write + fsync, then atomic rename over `target`. A crash leaves the
/// target either fully old or fully new — never half-written. Shared by
/// the edit and patch tools.
pub fn atomic_write(target: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;

    let tmp = dir.join(format!(".grodex-tmp-{}", uuid::Uuid::new_v4()));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("temp create: {e}"))?;
        f.write_all(content).map_err(|e| format!("temp write: {e}"))?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename: {e}")
    })?;
    Ok(())
}

/// Refuse a path whose canonical form escapes `allowed_root`.
///
/// This is the symlink-escape defence: a tool asked to write under
/// `/workspace` must not follow a symlink to `/etc`. Returns the canonical
/// path on success, or an error message describing the escape.
pub fn assert_within_root(path: &Path, allowed_root: &Path) -> Result<PathBuf, String> {
    let can = canonicalize(path);
    let root_can = canonicalize(allowed_root);
    if !can.starts_with(&root_can) {
        return Err(format!(
            "path {} resolves to {} which is outside the allowed root {}",
            path.display(),
            can.display(),
            root_can.display()
        ));
    }
    Ok(can)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn file_version_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "hello").unwrap();
        let v1 = FileVersion::of(&p).unwrap();
        std::fs::write(&p, "world").unwrap();
        let v2 = FileVersion::of(&p).unwrap();
        assert!(!v1.matches(&v2), "FileVersion must detect a content change");
    }

    #[test]
    fn assert_within_root_blocks_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        // Symlink inside "workspace" pointing outside.
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, ws.join("escape")).unwrap();
            let res = assert_within_root(&ws.join("escape"), &ws);
            assert!(res.is_err(), "symlink escaping the root must be rejected");
        }
        // A legit path inside resolves fine.
        let legit = ws.join("ok.txt");
        std::fs::write(&legit, "x").unwrap();
        assert!(assert_within_root(&legit, &ws).is_ok());
    }
}
