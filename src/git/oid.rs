//! Object ids, as git prints them.

use std::fmt;

use anyhow::{Result, bail};

/// The id of a git object. Repositories that use SHA-1 have 20-byte ids,
/// and ones that use SHA-256 have 32-byte ids.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId {
    len: u8,
    bytes: [u8; 32],
}

impl ObjectId {
    /// The id of a SHA-1 repository's empty tree. Git knows this tree
    /// even when the repository doesn't hold it.
    const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const EMPTY_TREE_SHA256: &str =
        "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

    /// Read an id from 40 or 64 hex digits.
    pub fn from_hex(hex: &[u8]) -> Result<ObjectId> {
        if hex.len() != 40 && hex.len() != 64 {
            bail!("not an object id: {:?}", String::from_utf8_lossy(hex));
        }
        let mut bytes = [0u8; 32];
        for (i, pair) in hex.chunks(2).enumerate() {
            match (digit(pair[0]), digit(pair[1])) {
                (Some(hi), Some(lo)) => bytes[i] = hi << 4 | lo,
                _ => bail!("not an object id: {:?}", String::from_utf8_lossy(hex)),
            }
        }
        Ok(ObjectId {
            len: (hex.len() / 2) as u8,
            bytes,
        })
    }

    /// Make an id from its 20 or 32 bytes, if it has that many.
    pub fn from_bytes(bytes: &[u8]) -> Option<ObjectId> {
        (bytes.len() == 20 || bytes.len() == 32).then(|| ObjectId::from_bytes_or_panic(bytes))
    }

    /// Make an id from its 20 or 32 bytes.
    ///
    /// # Panics
    ///
    /// If `bytes` has another length.
    pub fn from_bytes_or_panic(bytes: &[u8]) -> ObjectId {
        assert!(bytes.len() == 20 || bytes.len() == 32, "bad id length");
        let mut b = [0u8; 32];
        b[..bytes.len()].copy_from_slice(bytes);
        ObjectId {
            len: bytes.len() as u8,
            bytes: b,
        }
    }

    /// The SHA-1 id made of zeros, which git uses for "no object".
    pub fn null() -> ObjectId {
        ObjectId {
            len: 20,
            bytes: [0; 32],
        }
    }

    pub fn is_null(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// The empty tree of a repository whose ids look like this one.
    pub fn empty_tree(&self) -> ObjectId {
        let hex = if self.len == 20 {
            Self::EMPTY_TREE_SHA1
        } else {
            Self::EMPTY_TREE_SHA256
        };
        let id = Self::from_hex(hex.as_bytes());
        id.unwrap_or_else(|_| unreachable!("the empty tree ids are valid"))
    }

    /// The first seven hex digits, as the history shows them.
    pub fn short(&self) -> String {
        let mut s = self.to_string();
        s.truncate(7);
        s
    }
}

fn digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.as_bytes() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        for hex in [
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321",
        ] {
            let id = ObjectId::from_hex(hex.as_bytes()).expect("id");
            assert_eq!(id.to_string(), hex);
            assert_eq!(id.as_bytes().len(), hex.len() / 2);
            assert_eq!(id.empty_tree(), id);
        }
        let upper = ObjectId::from_hex(b"4B825DC642CB6EB9A060E54BF8D69288FBEE4904");
        assert_eq!(upper.expect("id").short(), "4b825dc");
    }

    #[test]
    fn rejects_other_text() {
        for bad in [
            &b""[..],
            b"4b825dc",
            b"4b825dc642cb6eb9a060e54bf8d69288fbee490g",
            b"4b825dc642cb6eb9a060e54bf8d69288fbee49041",
        ] {
            assert!(ObjectId::from_hex(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn null_is_zeros() {
        assert!(ObjectId::null().is_null());
        assert_eq!(ObjectId::null().to_string(), "0".repeat(40));
        assert!(!ObjectId::from_bytes_or_panic(&[1; 20]).is_null());
    }
}
