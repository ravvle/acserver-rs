use std::fmt;

/// A `string8` kept as the bytes it arrived as. The game writes narrow
/// strings in its code page, not UTF-8 (a non-ASCII team is one byte per
/// character), and vanilla relays those bytes unchanged (docs/01 §1.2).
/// `Display` shows them as UTF-8, with U+FFFD for what is not, as vanilla's
/// JSON and wide-string conversions do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Narrow(Vec<u8>);

impl Narrow {
    pub fn from_bytes(bytes: Vec<u8>) -> Narrow {
        Narrow(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl From<&str> for Narrow {
    fn from(s: &str) -> Narrow {
        Narrow(s.as_bytes().to_vec())
    }
}

impl From<String> for Narrow {
    fn from(s: String) -> Narrow {
        Narrow(s.into_bytes())
    }
}

impl PartialEq<&str> for Narrow {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl fmt::Display for Narrow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        String::from_utf8_lossy(&self.0).fmt(f)
    }
}
