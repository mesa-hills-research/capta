//! PulseAudio's "tagstruct" encoding. Each value is a type byte and its body, integers big
//! endian, so the reader can skip any value without knowing what it means.

/// The type byte of each kind of value.
mod tag {
    pub const STRING: u8 = b't';
    pub const STRING_NULL: u8 = b'N';
    pub const U32: u8 = b'L';
    pub const U8: u8 = b'B';
    pub const U64: u8 = b'R';
    pub const S64: u8 = b'r';
    pub const SAMPLE_SPEC: u8 = b'a';
    pub const ARBITRARY: u8 = b'x';
    pub const TRUE: u8 = b'1';
    pub const FALSE: u8 = b'0';
    pub const TIMEVAL: u8 = b'T';
    pub const USEC: u8 = b'U';
    pub const CHANNEL_MAP: u8 = b'm';
    pub const CVOLUME: u8 = b'v';
    pub const PROPLIST: u8 = b'P';
    pub const VOLUME: u8 = b'V';
    pub const FORMAT_INFO: u8 = b'f';
}

/// A sample format, rate and channel count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleSpec {
    pub format: u8,
    pub channels: u8,
    pub rate: u32,
}

/// Builds the body of a packet.
#[derive(Default)]
pub struct Writer {
    pub bytes: Vec<u8>,
}

impl Writer {
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes.push(tag::U32);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.bytes.extend_from_slice(&[tag::U8, value]);
        self
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.bytes.push(if value { tag::TRUE } else { tag::FALSE });
        self
    }

    /// A string, or the null string for `None`. It ends at a NUL inside, as on the server.
    pub fn string(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(text) => {
                let text = text.split('\0').next().unwrap_or_default();
                self.bytes.push(tag::STRING);
                self.bytes.extend_from_slice(text.as_bytes());
                self.bytes.push(0);
            }
            None => self.bytes.push(tag::STRING_NULL),
        }
        self
    }

    pub fn arbitrary(&mut self, value: &[u8]) -> &mut Self {
        self.bytes.push(tag::ARBITRARY);
        self.bytes
            .extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.bytes.extend_from_slice(value);
        self
    }

    pub fn sample_spec(&mut self, spec: SampleSpec) -> &mut Self {
        self.bytes
            .extend_from_slice(&[tag::SAMPLE_SPEC, spec.format, spec.channels]);
        self.bytes.extend_from_slice(&spec.rate.to_be_bytes());
        self
    }

    pub fn channel_map(&mut self, positions: &[u8]) -> &mut Self {
        self.bytes
            .extend_from_slice(&[tag::CHANNEL_MAP, positions.len() as u8]);
        self.bytes.extend_from_slice(positions);
        self
    }

    pub fn cvolume(&mut self, volumes: &[u32]) -> &mut Self {
        self.bytes
            .extend_from_slice(&[tag::CVOLUME, volumes.len() as u8]);
        for volume in volumes {
            self.bytes.extend_from_slice(&volume.to_be_bytes());
        }
        self
    }

    /// A property list of string values, each stored with its NUL as the protocol wants.
    pub fn proplist(&mut self, properties: &[(&str, &str)]) -> &mut Self {
        self.bytes.push(tag::PROPLIST);
        for (key, value) in properties {
            let mut stored = value.as_bytes().to_vec();
            stored.push(0);
            self.string(Some(key))
                .u32(stored.len() as u32)
                .arbitrary(&stored);
        }
        self.bytes.push(tag::STRING_NULL);
        self
    }
}

/// A value that isn't what the reader expected, or a body that ends early.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed(pub String);

impl Malformed {
    fn expected(what: &str, found: Option<u8>) -> Self {
        match found {
            Some(byte) => Malformed(format!("expected {what}, found type {:?}", byte as char)),
            None => Malformed(format!("expected {what}, found the end")),
        }
    }
}

/// Reads a body. Readers of single values check the type byte, and [`Reader::skip`] passes
/// over any value, so replies keep working when a server adds fields.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, at: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// The bytes not read yet.
    pub fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at.min(self.bytes.len())..]
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Malformed> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| Malformed(format!("a value runs past the end at byte {}", self.at)))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn peek_tag(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn expect(&mut self, wanted: u8, what: &str) -> Result<(), Malformed> {
        match self.peek_tag() {
            Some(found) if found == wanted => {
                self.at += 1;
                Ok(())
            }
            found => Err(Malformed::expected(what, found)),
        }
    }

    fn be_u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, Malformed> {
        self.expect(tag::U32, "a u32")?;
        self.be_u32()
    }

    pub fn u8(&mut self) -> Result<u8, Malformed> {
        self.expect(tag::U8, "a u8")?;
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, Malformed> {
        match self.peek_tag() {
            Some(tag::TRUE) => {
                self.at += 1;
                Ok(true)
            }
            Some(tag::FALSE) => {
                self.at += 1;
                Ok(false)
            }
            found => Err(Malformed::expected("a boolean", found)),
        }
    }

    /// A string, `None` for the null string. Bytes that aren't UTF-8 are replaced.
    pub fn string(&mut self) -> Result<Option<String>, Malformed> {
        match self.peek_tag() {
            Some(tag::STRING_NULL) => {
                self.at += 1;
                Ok(None)
            }
            Some(tag::STRING) => {
                self.at += 1;
                Ok(Some(String::from_utf8_lossy(self.c_string()?).into_owned()))
            }
            found => Err(Malformed::expected("a string", found)),
        }
    }

    fn c_string(&mut self) -> Result<&'a [u8], Malformed> {
        let rest = &self.bytes[self.at..];
        let length = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| Malformed("a string has no end".into()))?;
        self.at += length + 1;
        Ok(&rest[..length])
    }

    pub fn sample_spec(&mut self) -> Result<SampleSpec, Malformed> {
        self.expect(tag::SAMPLE_SPEC, "a sample spec")?;
        let head = self.take(2)?;
        let (format, channels) = (head[0], head[1]);
        Ok(SampleSpec {
            format,
            channels,
            rate: self.be_u32()?,
        })
    }

    pub fn channel_map(&mut self) -> Result<Vec<u8>, Malformed> {
        self.expect(tag::CHANNEL_MAP, "a channel map")?;
        let count = self.take(1)?[0] as usize;
        Ok(self.take(count)?.to_vec())
    }

    /// A property list, its string values without their NUL.
    pub fn proplist(&mut self) -> Result<Vec<(String, String)>, Malformed> {
        self.expect(tag::PROPLIST, "a property list")?;
        let mut properties = Vec::new();
        while let Some(key) = self.string()? {
            let length = self.u32()?;
            self.expect(tag::ARBITRARY, "a property value")?;
            if self.be_u32()? != length {
                return Err(Malformed(format!("property {key} has two lengths")));
            }
            let value = self.take(length as usize)?;
            let value = value.strip_suffix(&[0]).unwrap_or(value);
            properties.push((key, String::from_utf8_lossy(value).into_owned()));
        }
        Ok(properties)
    }

    /// Passes over one value of any type.
    pub fn skip(&mut self) -> Result<(), Malformed> {
        let Some(found) = self.peek_tag() else {
            return Err(Malformed::expected("a value", None));
        };
        match found {
            tag::STRING | tag::STRING_NULL => drop(self.string()?),
            tag::PROPLIST => drop(self.proplist()?),
            tag::CHANNEL_MAP => drop(self.channel_map()?),
            tag::FORMAT_INFO => {
                self.at += 1;
                self.u8()?;
                self.proplist()?;
            }
            _ => {
                self.at += 1;
                let size = match found {
                    tag::U32 | tag::VOLUME => 4,
                    tag::U8 => 1,
                    tag::U64 | tag::S64 | tag::USEC | tag::TIMEVAL => 8,
                    tag::SAMPLE_SPEC => 6,
                    tag::TRUE | tag::FALSE => 0,
                    tag::ARBITRARY => self.be_u32()? as usize,
                    tag::CVOLUME => 4 * self.take(1)?[0] as usize,
                    other => return Err(Malformed(format!("unknown type {:?}", other as char))),
                };
                self.take(size)?;
            }
        }
        Ok(())
    }

    pub fn skip_n(&mut self, count: usize) -> Result<(), Malformed> {
        (0..count).try_for_each(|_| self.skip())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_back_as_written() {
        let mut w = Writer::default();
        w.u32(7)
            .u8(3)
            .bool(true)
            .bool(false)
            .string(Some("name"))
            .string(None)
            .arbitrary(&[1, 2, 3])
            .sample_spec(SampleSpec {
                format: 5,
                channels: 1,
                rate: 16_000,
            })
            .channel_map(&[1, 2])
            .cvolume(&[0x10000, 0x10000])
            .proplist(&[("application.name", "Capta")]);
        let mut r = Reader::new(&w.bytes);
        assert_eq!(r.u32(), Ok(7));
        assert_eq!(r.u8(), Ok(3));
        assert_eq!(r.bool(), Ok(true));
        assert_eq!(r.bool(), Ok(false));
        assert_eq!(r.string(), Ok(Some("name".into())));
        assert_eq!(r.string(), Ok(None));
        r.skip().unwrap();
        assert_eq!(
            r.sample_spec(),
            Ok(SampleSpec {
                format: 5,
                channels: 1,
                rate: 16_000
            })
        );
        assert_eq!(r.channel_map(), Ok(vec![1, 2]));
        r.skip().unwrap();
        assert_eq!(
            r.proplist(),
            Ok(vec![("application.name".into(), "Capta".into())])
        );
        assert!(r.is_empty());
    }

    #[test]
    fn skip_passes_over_every_type() {
        let mut w = Writer::default();
        w.u32(1)
            .u8(2)
            .bool(true)
            .string(Some("x"))
            .string(None)
            .arbitrary(&[9; 5]);
        w.sample_spec(SampleSpec {
            format: 3,
            channels: 2,
            rate: 48_000,
        });
        w.channel_map(&[1, 2])
            .cvolume(&[1, 2, 3])
            .proplist(&[("a", "b")]);
        w.bytes.push(b'U');
        w.bytes.extend_from_slice(&5u64.to_be_bytes());
        w.bytes.push(b'V');
        w.bytes.extend_from_slice(&5u32.to_be_bytes());
        // A format info, its encoding and an empty property list.
        w.bytes.extend_from_slice(&[b'f', b'B', 1]);
        w.proplist(&[]);
        let mut r = Reader::new(&w.bytes);
        r.skip_n(13).unwrap();
        assert!(r.is_empty(), "left {:?}", &w.bytes[r.at..]);
    }

    #[test]
    fn truncated_and_mistyped_values_are_errors() {
        assert!(Reader::new(&[b'L', 0, 0]).u32().is_err());
        assert!(Reader::new(b"ta").string().is_err());
        assert!(Reader::new(&[b'B', 1]).u32().is_err());
        assert!(Reader::new(&[b'x', 0xff, 0xff, 0xff, 0xff]).skip().is_err());
        assert!(Reader::new(b"?").skip().is_err());
        assert!(Reader::new(&[]).skip().is_err());
    }
}
