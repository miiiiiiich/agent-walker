use std::io::{self, Write};

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter, PrettyFormatter, Serializer};

/// `serde_json::to_writer_pretty`, except that `must_escape` code points
/// inside strings are written as `\uXXXX`.
pub(in crate::format::json) fn to_writer_pretty<W: Write, T: ?Sized + Serialize>(
    writer: W,
    value: &T,
) -> serde_json::Result<()> {
    let mut serializer =
        Serializer::with_formatter(writer, EscapeInvisible(PrettyFormatter::new()));
    value.serialize(&mut serializer)
}

// Skill, subagent, project and model names come from files a cloned repo
// controls, and `--json` is often printed straight to a terminal. serde_json
// escapes only C0 controls, so DEL, C1 (raw U+009B is CSI on xterm/VTE), bidi
// overrides and zero-width characters would reach the terminal raw. The
// export is also fed to LLMs, where tag characters (U+E0000 block) carry text
// a human reviewer cannot see, and U+2028/U+2029 break JavaScript consumers.
// Escaping rather than stripping keeps every parsed value identical.
fn must_escape(ch: char) -> bool {
    matches!(
        ch,
        '\u{7f}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

struct EscapeInvisible<F>(F);

macro_rules! delegate {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {$(
        fn $method<W: ?Sized + Write>(&mut self, writer: &mut W $(, $arg: $ty)*) -> io::Result<()> {
            self.0.$method(writer $(, $arg)*)
        }
    )*};
}

// Every other method forwards, so the output stays byte-identical to the
// wrapped formatter wherever no escape is needed.
impl<F: Formatter> Formatter for EscapeInvisible<F> {
    delegate! {
        write_null();
        write_bool(value: bool);
        write_i8(value: i8);
        write_i16(value: i16);
        write_i32(value: i32);
        write_i64(value: i64);
        write_i128(value: i128);
        write_u8(value: u8);
        write_u16(value: u16);
        write_u32(value: u32);
        write_u64(value: u64);
        write_u128(value: u128);
        write_f32(value: f32);
        write_f64(value: f64);
        write_number_str(value: &str);
        begin_string();
        end_string();
        write_char_escape(char_escape: CharEscape);
        write_byte_array(value: &[u8]);
        begin_array();
        end_array();
        begin_array_value(first: bool);
        end_array_value();
        begin_object();
        end_object();
        begin_object_key(first: bool);
        end_object_key();
        begin_object_value();
        end_object_value();
        write_raw_fragment(fragment: &str);
    }

    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if must_escape(ch) {
                self.0
                    .write_string_fragment(writer, &fragment[start..index])?;
                for unit in ch.encode_utf16(&mut [0; 2]) {
                    write!(writer, "\\u{unit:04x}")?;
                }
                start = index + ch.len_utf8();
            }
        }
        self.0.write_string_fragment(writer, &fragment[start..])
    }
}
