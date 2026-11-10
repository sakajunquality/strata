//! pkt-line framing used by every git transport.

use crate::{corrupt, Result};

const MAX_PKT: usize = 65520;
/// Largest payload of a sideband packet: 4 length bytes and 1 band byte are overhead.
const MAX_SIDEBAND_DATA: usize = MAX_PKT - 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    Flush,
    Delim,
    ResponseEnd,
    Data(&'a [u8]),
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Returns `None` at end of input.
    pub fn read(&mut self) -> Result<Option<Pkt<'a>>> {
        let rest = &self.buf[self.pos..];
        if rest.is_empty() {
            return Ok(None);
        }
        let len = rest
            .get(..4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| corrupt("bad pkt-line length"))?;
        let pkt = match len {
            0 => Pkt::Flush,
            1 => Pkt::Delim,
            2 => Pkt::ResponseEnd,
            3 => return Err(corrupt("bad pkt-line length")),
            n => {
                if n > MAX_PKT || rest.len() < n {
                    return Err(corrupt("truncated pkt-line"));
                }
                self.pos += n;
                return Ok(Some(Pkt::Data(&rest[4..n])));
            }
        };
        self.pos += 4;
        Ok(Some(pkt))
    }

    /// Number of bytes consumed so far; what follows is not pkt-line framed (e.g. a packfile).
    pub fn position(&self) -> usize {
        self.pos
    }
}

/// Strips the optional trailing newline of a text pkt-line.
pub fn text(data: &[u8]) -> &[u8] {
    data.strip_suffix(b"\n").unwrap_or(data)
}

pub fn write(out: &mut Vec<u8>, data: impl AsRef<[u8]>) {
    let data = data.as_ref();
    assert!(data.len() + 4 <= MAX_PKT, "pkt-line payload too long");
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub fn delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

/// Writes `data` on a sideband channel (1 = pack data, 2 = progress, 3 = error).
pub fn write_sideband(out: &mut Vec<u8>, band: u8, data: &[u8]) {
    for chunk in data.chunks(MAX_SIDEBAND_DATA) {
        out.extend_from_slice(format!("{:04x}", chunk.len() + 5).as_bytes());
        out.push(band);
        out.extend_from_slice(chunk);
    }
}

/// Frames everything written to it as sideband packets on one band.
pub struct SidebandWriter<W: std::io::Write> {
    out: W,
    band: u8,
    buf: Vec<u8>,
}

impl<W: std::io::Write> SidebandWriter<W> {
    pub fn new(out: W, band: u8) -> Self {
        SidebandWriter { out, band, buf: Vec::with_capacity(MAX_SIDEBAND_DATA) }
    }

    fn emit(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let mut header = [0u8; 5];
            header[..4].copy_from_slice(format!("{:04x}", self.buf.len() + 5).as_bytes());
            header[4] = self.band;
            self.out.write_all(&header)?;
            self.out.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Sends any buffered data and returns the underlying writer.
    pub fn finish(mut self) -> std::io::Result<W> {
        self.emit()?;
        Ok(self.out)
    }
}

impl<W: std::io::Write> std::io::Write for SidebandWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let take = data.len().min(MAX_SIDEBAND_DATA - self.buf.len());
        self.buf.extend_from_slice(&data[..take]);
        if self.buf.len() == MAX_SIDEBAND_DATA {
            self.emit()?;
        }
        Ok(take)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.emit()?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        write(&mut buf, "command=ls-refs\n");
        delim(&mut buf);
        write(&mut buf, "peel\n");
        flush(&mut buf);
        buf.extend_from_slice(b"PACK");

        let mut r = Reader::new(&buf);
        assert_eq!(r.read().unwrap(), Some(Pkt::Data(b"command=ls-refs\n")));
        assert_eq!(r.read().unwrap(), Some(Pkt::Delim));
        assert_eq!(r.read().unwrap(), Some(Pkt::Data(b"peel\n")));
        assert_eq!(r.read().unwrap(), Some(Pkt::Flush));
        assert_eq!(&buf[r.position()..], b"PACK");
    }

    #[test]
    fn sideband_writer_frames_like_write_sideband() {
        use std::io::Write;
        let data: Vec<u8> = (0..150_000u32).map(|i| i as u8).collect();
        let mut expected = Vec::new();
        write_sideband(&mut expected, 1, &data);

        let mut w = SidebandWriter::new(Vec::new(), 1);
        for piece in data.chunks(7001) {
            w.write_all(piece).unwrap();
        }
        assert_eq!(w.finish().unwrap(), expected);
    }

    #[test]
    fn rejects_truncated() {
        assert!(Reader::new(b"0009ab").read().is_err());
    }
}
