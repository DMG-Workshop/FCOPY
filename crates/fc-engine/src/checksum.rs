//! Streaming checksums: MD5, SHA-256 and XXH3-128.
//!
//! [`Hasher`] is fed chunk by chunk from the copy loop, so the source digest
//! costs no extra I/O. Pick the algorithm by purpose:
//!
//! | Algorithm | Speed (typ., 1 core) | Use it for |
//! |-----------|----------------------|------------|
//! | `Xxh3`    | 10+ GB/s             | Detecting accidental corruption (default) |
//! | `Sha256`  | 0.5–2 GB/s (SHA-NI)  | Cryptographic integrity, published checksums |
//! | `Md5`     | ~0.6 GB/s            | Compatibility with legacy `.md5` files only (broken for security) |

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::str::FromStr;

use md5::{Digest as _, Md5};
use sha2::Sha256;
use xxhash_rust::xxh3::Xxh3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Algorithm {
    Md5,
    Sha256,
    /// XXH3 with 128-bit output. Non-cryptographic, extremely fast.
    #[default]
    Xxh3,
}

impl Algorithm {
    pub const ALL: [Algorithm; 3] = [Algorithm::Md5, Algorithm::Sha256, Algorithm::Xxh3];

    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Md5 => "MD5",
            Algorithm::Sha256 => "SHA-256",
            Algorithm::Xxh3 => "XXH3-128",
        }
    }

    /// Digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Algorithm::Md5 | Algorithm::Xxh3 => 16,
            Algorithm::Sha256 => 32,
        }
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Algorithm {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "md5" => Ok(Algorithm::Md5),
            "sha256" => Ok(Algorithm::Sha256),
            "xxh3" | "xxhash" | "xxh3128" => Ok(Algorithm::Xxh3),
            _ => Err(format!("unknown algorithm '{s}' (expected md5, sha256 or xxh3)")),
        }
    }
}

/// A finished checksum.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    algorithm: Algorithm,
    bytes: Vec<u8>,
}

impl Digest {
    /// Wrap raw digest bytes (e.g. received from a remote peer).
    pub fn new(algorithm: Algorithm, bytes: Vec<u8>) -> Self {
        Self { algorithm, bytes }
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Lower-case hex, the format used by `md5sum` / `sha256sum` files.
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(self.bytes.len() * 2);
        for &b in &self.bytes {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    /// Parse a hex string (e.g. from a checksum manifest) for comparison.
    pub fn from_hex(algorithm: Algorithm, hex: &str) -> Option<Digest> {
        let hex = hex.trim();
        if hex.len() != algorithm.digest_len() * 2 || !hex.is_ascii() {
            return None;
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        Some(Digest { algorithm, bytes })
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.name(), self.to_hex())
    }
}

enum Inner {
    Md5(Md5),
    Sha256(Sha256),
    // XXH3 state is ~600 bytes; box it so `Hasher` stays small to move around.
    Xxh3(Box<Xxh3>),
}

/// Incremental hasher over any supported [`Algorithm`].
pub struct Hasher {
    inner: Inner,
}

impl Hasher {
    pub fn new(algorithm: Algorithm) -> Self {
        let inner = match algorithm {
            Algorithm::Md5 => Inner::Md5(Md5::new()),
            Algorithm::Sha256 => Inner::Sha256(Sha256::new()),
            Algorithm::Xxh3 => Inner::Xxh3(Box::new(Xxh3::new())),
        };
        Self { inner }
    }

    pub fn algorithm(&self) -> Algorithm {
        match self.inner {
            Inner::Md5(_) => Algorithm::Md5,
            Inner::Sha256(_) => Algorithm::Sha256,
            Inner::Xxh3(_) => Algorithm::Xxh3,
        }
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        match &mut self.inner {
            Inner::Md5(h) => h.update(data),
            Inner::Sha256(h) => h.update(data),
            Inner::Xxh3(h) => h.update(data),
        }
    }

    pub fn finalize(self) -> Digest {
        let algorithm = self.algorithm();
        let bytes = match self.inner {
            Inner::Md5(h) => h.finalize().to_vec(),
            Inner::Sha256(h) => h.finalize().to_vec(),
            Inner::Xxh3(h) => h.digest128().to_be_bytes().to_vec(),
        };
        Digest { algorithm, bytes }
    }
}

/// Hash a whole file with plain buffered reads.
///
/// Convenience for standalone "verify this earlier copy" use. It has no
/// progress, pause or error-recovery hooks; the copy engine's own read-back
/// pass is the fully instrumented version.
pub fn hash_file(path: &Path, algorithm: Algorithm) -> io::Result<Digest> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new(algorithm);
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(algo: Algorithm, data: &[u8]) -> String {
        let mut h = Hasher::new(algo);
        h.update(data);
        h.finalize().to_hex()
    }

    #[test]
    fn md5_known_vector() {
        assert_eq!(hex_of(Algorithm::Md5, b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex_of(Algorithm::Sha256, b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn xxh3_matches_reference_and_is_chunk_independent() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let whole = hex_of(Algorithm::Xxh3, &data);
        let reference = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&data));
        assert_eq!(whole, reference);

        for algo in Algorithm::ALL {
            let mut h = Hasher::new(algo);
            for chunk in data.chunks(777) {
                h.update(chunk);
            }
            assert_eq!(h.finalize().to_hex(), hex_of(algo, &data), "{algo}");
        }
    }

    #[test]
    fn hex_round_trip() {
        let d = Hasher::new(Algorithm::Sha256).finalize();
        assert_eq!(Digest::from_hex(Algorithm::Sha256, &d.to_hex()), Some(d));
        assert!(Digest::from_hex(Algorithm::Md5, "xyz").is_none());
        assert!(Digest::from_hex(Algorithm::Md5, &"g".repeat(32)).is_none());
    }

    #[test]
    fn parse_algorithm_names() {
        assert_eq!("SHA-256".parse::<Algorithm>(), Ok(Algorithm::Sha256));
        assert_eq!("xxhash".parse::<Algorithm>(), Ok(Algorithm::Xxh3));
        assert!("crc32".parse::<Algorithm>().is_err());
    }
}
