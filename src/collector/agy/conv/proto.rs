use crate::collector::ident::ident;

pub(in crate::collector::agy) fn proto_timestamp_ms(ts: &[u8]) -> Option<i64> {
    let seconds = i64::try_from(varint_field(ts, 1)?).ok()?;
    let nanos = i64::try_from(varint_field(ts, 2).unwrap_or(0)).ok()?;
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

enum Wire<'a> {
    Varint(u64),
    Len(&'a [u8]),
}

struct ProtoReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    fn read_varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn next_field(&mut self) -> Option<(u64, Wire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let wire = match tag & 0x7 {
            0 => Wire::Varint(self.read_varint()?),
            1 => {
                self.pos = self.pos.checked_add(8).filter(|&p| p <= self.buf.len())?;
                return Some((tag >> 3, Wire::Varint(0))); // fixed64, value unused
            }
            2 => {
                let len = usize::try_from(self.read_varint()?).ok()?;
                let end = self.pos.checked_add(len).filter(|&p| p <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Wire::Len(bytes)
            }
            5 => {
                self.pos = self.pos.checked_add(4).filter(|&p| p <= self.buf.len())?;
                return Some((tag >> 3, Wire::Varint(0))); // fixed32, value unused
            }
            _ => return None,
        };
        Some((tag >> 3, wire))
    }
}

pub(in crate::collector::agy) fn message_field(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut reader = ProtoReader { buf, pos: 0 };
    while let Some((found, wire)) = reader.next_field() {
        if found == field
            && let Wire::Len(bytes) = wire
        {
            return Some(bytes);
        }
    }
    None
}

pub(in crate::collector::agy) fn varint_field(buf: &[u8], field: u64) -> Option<u64> {
    let mut reader = ProtoReader { buf, pos: 0 };
    while let Some((found, wire)) = reader.next_field() {
        if found == field
            && let Wire::Varint(value) = wire
        {
            return Some(value);
        }
    }
    None
}

pub(in crate::collector::agy) fn string_field(buf: &[u8], field: u64) -> Option<&str> {
    message_field(buf, field)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(ident)
}
