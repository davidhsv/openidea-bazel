//! Protobuf text format, leniently: what `proto.encode_text` writes into `.intellij-info.txt` files.
//!
//! [`Reader`] is a pull parser: [`Reader::next_field`] yields one field of the current message at a
//! time, borrowing names and plain strings from the input, and a message field leaves the reader
//! inside that message until `next_field` returns `None` at its closing brace. Values are decoded
//! only when asked for, so skipping an unknown field or message costs a scan and no allocation.
//!
//! Beyond what Starlark writes, it accepts the rest of the text format a hand-written file may
//! use: `#` comments, `name: { … }` and `<…>` messages, `name: [a, b]` lists (read as repeated
//! fields), `;`/`,` field separators, single quotes, adjacent string literals (concatenated),
//! `[extension.name]` field names, and hex/octal integers.

use std::borrow::Cow;
use std::fmt;

/// A syntax error at a byte offset of the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub offset: usize,
    pub message: String,
}

impl Error {
    pub fn new(offset: usize, message: impl Into<String>) -> Self {
        Error {
            offset,
            message: message.into(),
        }
    }

    /// The message with the line and column of the offset in `text`, the input that produced it.
    pub fn describe(&self, text: &str) -> String {
        let before = &text.as_bytes()[..self.offset.min(text.len())];
        let line = 1 + before.iter().filter(|&&c| c == b'\n').count();
        let column = 1 + before.iter().rev().take_while(|&&c| c != b'\n').count();
        format!(
            "line {line}, column {column} (byte {}): {}",
            self.offset, self.message
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for Error {}

/// One field of the message the reader is in.
#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    pub name: &'a str,
    /// Byte offset of the field name (of the element, inside a list).
    pub offset: usize,
    pub value: Value<'a>,
}

#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    /// The reader is now inside this message: read its fields with [`Reader::next_field`] until
    /// `None`, or leave it with [`Reader::skip`].
    Message,
    /// One or more adjacent string literals, still escaped.
    Str(RawStr<'a>),
    /// `true`, an enum value name, `inf`…
    Ident(&'a str),
    /// A numeric literal as written (`-12`, `0x1F`, `1.5e3`).
    Number(&'a str),
}

/// An enum field's value: a name or a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnumValue<'a> {
    Name(&'a str),
    Number(i32),
}

impl<'a> Field<'a> {
    pub fn is_message(&self) -> bool {
        matches!(self.value, Value::Message)
    }

    fn mismatch(&self, expected: &str) -> Error {
        let found = match self.value {
            Value::Message => "a message".to_owned(),
            Value::Str(_) => "a string".to_owned(),
            Value::Ident(s) | Value::Number(s) => format!("`{s}`"),
        };
        Error::new(
            self.offset,
            format!("field `{}`: expected {expected}, found {found}", self.name),
        )
    }

    /// Errors unless the field is a message (which the reader is then inside).
    pub fn expect_message(&self) -> Result<(), Error> {
        if self.is_message() {
            Ok(())
        } else {
            Err(self.mismatch("a message"))
        }
    }

    /// The decoded string, borrowed from the input when it has no escapes.
    pub fn string(&self) -> Result<Cow<'a, str>, Error> {
        match self.value {
            Value::Str(raw) => raw.decode(),
            _ => Err(self.mismatch("a string")),
        }
    }

    /// The spellings protobuf's `TextFormat` accepts for a bool.
    pub fn bool(&self) -> Result<bool, Error> {
        match self.value {
            Value::Ident("true" | "True" | "t") | Value::Number("1") => Ok(true),
            Value::Ident("false" | "False" | "f") | Value::Number("0") => Ok(false),
            _ => Err(self.mismatch("a bool")),
        }
    }

    pub fn int(&self) -> Result<i64, Error> {
        match self.value {
            Value::Number(token) => parse_int(token).ok_or_else(|| self.mismatch("an integer")),
            _ => Err(self.mismatch("an integer")),
        }
    }

    pub fn enum_value(&self) -> Result<EnumValue<'a>, Error> {
        match self.value {
            Value::Ident(name) => Ok(EnumValue::Name(name)),
            Value::Number(token) => parse_int(token)
                .and_then(|n| i32::try_from(n).ok())
                .map(EnumValue::Number)
                .ok_or_else(|| self.mismatch("an enum value")),
            _ => Err(self.mismatch("an enum value")),
        }
    }
}

/// Decimal, `0x` hex or `0` octal, optionally negative, as the text format writes integers.
fn parse_int(token: &str) -> Option<i64> {
    let (negative, unsigned) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let (radix, digits) = if let Some(hex) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        (16, hex)
    } else if unsigned.len() > 1 && unsigned.starts_with('0') {
        (8, &unsigned[1..])
    } else {
        (10, unsigned)
    };
    // from_str_radix would also take a sign here.
    if !digits.bytes().next().is_some_and(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let magnitude = u64::from_str_radix(digits, radix).ok()?;
    if negative {
        if magnitude <= i64::MIN.unsigned_abs() {
            Some((magnitude as i64).wrapping_neg())
        } else {
            None
        }
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// String literals as they appear in the input, decoded by [`RawStr::decode`].
#[derive(Clone, Copy, Debug)]
pub struct RawStr<'a> {
    /// From the first opening quote through the last closing quote.
    span: &'a str,
    offset: usize,
    /// A single literal without escapes: its content is the value.
    plain: bool,
}

impl<'a> RawStr<'a> {
    /// Unescapes and concatenates the literals. Octal and hex escapes yield bytes, so the result is
    /// decoded as UTF-8 with invalid sequences replaced, as protobuf-java's `toStringUtf8` does.
    pub fn decode(&self) -> Result<Cow<'a, str>, Error> {
        if self.plain {
            return Ok(Cow::Borrowed(&self.span[1..self.span.len() - 1]));
        }
        let b = self.span.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                quote @ (b'"' | b'\'') => {
                    i += 1;
                    loop {
                        match b.get(i) {
                            Some(&c) if c == quote => break,
                            Some(b'\\') => i = unescape(b, i + 1, self.offset, &mut out)?,
                            Some(&c) => {
                                out.push(c);
                                i += 1;
                            }
                            None => return Err(Error::new(self.offset, "unterminated string")),
                        }
                    }
                    i += 1;
                }
                b'#' => {
                    i += b[i..]
                        .iter()
                        .position(|&c| c == b'\n')
                        .unwrap_or(b.len() - i)
                }
                _ => i += 1,
            }
        }
        Ok(Cow::Owned(match String::from_utf8(out) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }))
    }
}

/// Decodes the escape whose first byte (after the backslash) is at `i`; returns the next index.
fn unescape(b: &[u8], i: usize, base: usize, out: &mut Vec<u8>) -> Result<usize, Error> {
    let err = |msg: &str| Error::new(base + i - 1, msg);
    let Some(&c) = b.get(i) else {
        return Err(err("unterminated string"));
    };
    let simple = match c {
        b'a' => Some(0x07),
        b'b' => Some(0x08),
        b'f' => Some(0x0c),
        b'n' => Some(b'\n'),
        b'r' => Some(b'\r'),
        b't' => Some(b'\t'),
        b'v' => Some(0x0b),
        b'\\' | b'\'' | b'"' | b'?' => Some(c),
        _ => None,
    };
    if let Some(byte) = simple {
        out.push(byte);
        return Ok(i + 1);
    }
    match c {
        b'0'..=b'7' => {
            let digits = b[i..]
                .iter()
                .take(3)
                .take_while(|d| (b'0'..=b'7').contains(d))
                .count();
            let code = b[i..i + digits]
                .iter()
                .fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
            // protobuf-java narrows `\400`..`\777` to a byte the same way.
            out.push(code as u8);
            Ok(i + digits)
        }
        b'x' | b'X' => {
            let digits = b[i + 1..]
                .iter()
                .take(2)
                .take_while(|d| d.is_ascii_hexdigit())
                .count();
            if digits == 0 {
                return Err(err("`\\x` escape without hex digits"));
            }
            out.push(hex_value(&b[i + 1..i + 1 + digits]) as u8);
            Ok(i + 1 + digits)
        }
        b'u' | b'U' => {
            let len = if c == b'u' { 4 } else { 8 };
            let code = fixed_hex(b, i + 1, len).ok_or_else(|| err("malformed unicode escape"))?;
            let mut next = i + 1 + len;
            let code = if (0xD800..0xDC00).contains(&code) {
                // A high surrogate must pair with a `\uDC00`..`\uDFFF` low one.
                let low = (b.get(next) == Some(&b'\\') && b.get(next + 1) == Some(&b'u'))
                    .then(|| fixed_hex(b, next + 2, 4))
                    .flatten()
                    .filter(|low| (0xDC00..0xE000).contains(low))
                    .ok_or_else(|| err("unpaired surrogate in unicode escape"))?;
                next += 6;
                0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
            } else {
                code
            };
            let ch =
                char::from_u32(code).ok_or_else(|| err("invalid code point in unicode escape"))?;
            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            Ok(next)
        }
        _ => Err(err("invalid escape sequence")),
    }
}

fn hex_value(digits: &[u8]) -> u32 {
    digits
        .iter()
        .fold(0, |n, &d| n * 16 + char::from(d).to_digit(16).unwrap_or(0))
}

fn fixed_hex(b: &[u8], at: usize, len: usize) -> Option<u32> {
    let digits = b.get(at..at + len)?;
    digits
        .iter()
        .all(u8::is_ascii_hexdigit)
        .then(|| hex_value(digits))
}

#[derive(Clone, Copy, Debug)]
enum Frame<'a> {
    Message {
        name: &'a str,
        offset: usize,
        close: u8,
    },
    /// Inside `name: [ … ]`; `need_comma` once an element has been read.
    List {
        name: &'a str,
        offset: usize,
        need_comma: bool,
    },
}

/// Pull parser over a text-format message; the top level is the message itself, without braces.
pub struct Reader<'a> {
    text: &'a str,
    pos: usize,
    frames: Vec<Frame<'a>>,
}

impl<'a> Reader<'a> {
    pub fn new(text: &'a str) -> Self {
        Reader {
            text,
            pos: 0,
            frames: Vec::with_capacity(8),
        }
    }

    /// The next field of the current message, or `None` at its end: the closing brace of a nested
    /// message (consumed) or the end of input at the top level.
    pub fn next_field(&mut self) -> Result<Option<Field<'a>>, Error> {
        let bytes = self.text.as_bytes();
        loop {
            self.skip_whitespace();
            let at = self.pos;
            let next = bytes.get(at).copied();
            match self.frames.last().copied() {
                Some(Frame::List {
                    name,
                    offset,
                    need_comma,
                }) => match next {
                    None => {
                        return Err(Error::new(offset, format!("list `{name}` is not closed")));
                    }
                    Some(b']') => {
                        self.pos += 1;
                        self.frames.pop();
                        self.skip_separator();
                    }
                    Some(b',') if need_comma => {
                        self.pos += 1;
                        self.set_need_comma(false);
                    }
                    Some(_) if need_comma => {
                        return Err(Error::new(
                            at,
                            format!("expected `,` or `]` in list `{name}`"),
                        ));
                    }
                    Some(_) => {
                        self.set_need_comma(true);
                        return self.value(name, at).map(Some);
                    }
                },
                frame => {
                    let close = match frame {
                        Some(Frame::Message { close, .. }) => Some(close),
                        _ => None,
                    };
                    match next {
                        None => {
                            return match frame {
                                Some(Frame::Message { name, offset, .. }) => Err(Error::new(
                                    offset,
                                    format!("message `{name}` is not closed"),
                                )),
                                _ => Ok(None),
                            };
                        }
                        Some(c) if Some(c) == close => {
                            self.pos += 1;
                            self.frames.pop();
                            // Inside a list the comma after an element belongs to the list.
                            if !matches!(self.frames.last(), Some(Frame::List { .. })) {
                                self.skip_separator();
                            }
                            return Ok(None);
                        }
                        Some(c @ (b'}' | b'>' | b']')) => {
                            return Err(Error::new(at, format!("unexpected `{}`", char::from(c))));
                        }
                        Some(_) => {
                            let name = self.field_name()?;
                            self.skip_whitespace();
                            let colon = bytes.get(self.pos) == Some(&b':');
                            if colon {
                                self.pos += 1;
                                self.skip_whitespace();
                            }
                            match bytes.get(self.pos) {
                                Some(b'[') if colon => {
                                    self.pos += 1;
                                    self.frames.push(Frame::List {
                                        name,
                                        offset: at,
                                        need_comma: false,
                                    });
                                }
                                Some(b'{' | b'<') => return self.value(name, at).map(Some),
                                _ if colon => {
                                    let field = self.value(name, at)?;
                                    self.skip_separator();
                                    return Ok(Some(field));
                                }
                                _ => {
                                    return Err(Error::new(
                                        self.pos,
                                        format!("expected `:` or `{{` after field `{name}`"),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Leaves an unknown field: skips the rest of its message if it is one.
    pub fn skip(&mut self, field: &Field<'a>) -> Result<(), Error> {
        if field.is_message() {
            self.skip_message()
        } else {
            Ok(())
        }
    }

    /// Skips the rest of the message the reader is in, through its closing brace.
    pub fn skip_message(&mut self) -> Result<(), Error> {
        let depth = self.frames.len();
        if !matches!(self.frames.last(), Some(Frame::Message { .. })) {
            return Err(Error::new(self.pos, "not inside a message"));
        }
        loop {
            if self.next_field()?.is_none() && self.frames.len() < depth {
                return Ok(());
            }
        }
    }

    /// Current byte offset.
    pub fn offset(&self) -> usize {
        self.pos
    }

    /// The whole input, for callers that scan a hot, fixed-shape message themselves.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Skips whitespace and comments; at the top level, the start of the next field.
    pub fn skip_insignificant(&mut self) {
        self.skip_whitespace();
    }

    /// Moves past a top-level field the caller scanned itself, ending at `end`.
    pub fn jump_to(&mut self, end: usize) {
        debug_assert!(self.frames.is_empty());
        self.pos = end;
        self.skip_separator();
    }

    /// Ends the message just opened by [`Self::next_field`] at `end`, the offset just past its
    /// closing brace, which the caller found by scanning [`Self::text`] itself.
    pub fn close_message_at(&mut self, end: usize) {
        debug_assert!(matches!(self.frames.last(), Some(Frame::Message { .. })));
        self.pos = end;
        self.frames.pop();
        if !matches!(self.frames.last(), Some(Frame::List { .. })) {
            self.skip_separator();
        }
    }

    fn set_need_comma(&mut self, value: bool) {
        if let Some(Frame::List { need_comma, .. }) = self.frames.last_mut() {
            *need_comma = value;
        }
    }

    fn skip_whitespace(&mut self) {
        let bytes = self.text.as_bytes();
        while let Some(&c) = bytes.get(self.pos) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => self.pos += 1,
                b'#' => {
                    self.pos += bytes[self.pos..]
                        .iter()
                        .position(|&c| c == b'\n')
                        .unwrap_or(bytes.len() - self.pos);
                }
                _ => break,
            }
        }
    }

    fn skip_separator(&mut self) {
        self.skip_whitespace();
        if matches!(self.text.as_bytes().get(self.pos), Some(b';' | b',')) {
            self.pos += 1;
        }
    }

    fn field_name(&mut self) -> Result<&'a str, Error> {
        let bytes = self.text.as_bytes();
        let start = self.pos;
        match bytes[start] {
            c if c.is_ascii_alphabetic() || c == b'_' => {
                self.pos += bytes[start..]
                    .iter()
                    .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_'))
                    .unwrap_or(bytes.len() - start);
            }
            b'[' => {
                let len = bytes[start..]
                    .iter()
                    .position(|&c| c == b']' || c == b'\n')
                    .filter(|&i| bytes[start + i] == b']')
                    .ok_or_else(|| Error::new(start, "unterminated `[` field name"))?;
                self.pos += len + 1;
            }
            _ => {
                let found = self.text[start..].chars().next().unwrap_or(' ');
                return Err(Error::new(
                    start,
                    format!("expected a field name, found `{found}`"),
                ));
            }
        }
        Ok(&self.text[start..self.pos])
    }

    /// Reads the value at the current position; a message value pushes its frame.
    fn value(&mut self, name: &'a str, offset: usize) -> Result<Field<'a>, Error> {
        let bytes = self.text.as_bytes();
        let start = self.pos;
        let value = match bytes.get(start) {
            Some(&open @ (b'{' | b'<')) => {
                self.pos += 1;
                let close = if open == b'{' { b'}' } else { b'>' };
                self.frames.push(Frame::Message {
                    name,
                    offset: start,
                    close,
                });
                Value::Message
            }
            Some(b'"' | b'\'') => Value::Str(self.string_literals()?),
            Some(&c) if is_word_byte(c) => {
                self.pos += bytes[start..]
                    .iter()
                    .position(|&c| !is_word_byte(c))
                    .unwrap_or(bytes.len() - start);
                let word = &self.text[start..self.pos];
                if c.is_ascii_alphabetic() || c == b'_' {
                    Value::Ident(word)
                } else {
                    Value::Number(word)
                }
            }
            _ => {
                return Err(Error::new(
                    start,
                    format!("expected a value for field `{name}`"),
                ));
            }
        };
        Ok(Field {
            name,
            offset,
            value,
        })
    }

    fn string_literals(&mut self) -> Result<RawStr<'a>, Error> {
        let bytes = self.text.as_bytes();
        let start = self.pos;
        let mut plain = true;
        let mut literals = 0;
        loop {
            let open = self.pos;
            let quote = bytes[open];
            self.pos += 1;
            loop {
                match bytes.get(self.pos) {
                    Some(&c) if c == quote => break,
                    Some(b'\\') => {
                        plain = false;
                        self.pos += 2;
                    }
                    Some(b'\n') | None => return Err(Error::new(open, "unterminated string")),
                    Some(_) => self.pos += 1,
                }
            }
            self.pos += 1;
            literals += 1;
            let end = self.pos;
            self.skip_whitespace();
            if !matches!(bytes.get(self.pos), Some(b'"' | b'\'')) {
                return Ok(RawStr {
                    span: &self.text[start..end],
                    offset: start,
                    plain: plain && literals == 1,
                });
            }
        }
    }
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'+' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flattens a document into `path=value` lines, descending into every message.
    fn flatten(text: &str) -> Result<Vec<String>, Error> {
        fn walk(r: &mut Reader<'_>, prefix: &str, out: &mut Vec<String>) -> Result<(), Error> {
            while let Some(f) = r.next_field()? {
                let path = if prefix.is_empty() {
                    f.name.to_owned()
                } else {
                    format!("{prefix}.{}", f.name)
                };
                match f.value {
                    Value::Message => walk(r, &path, out)?,
                    Value::Str(_) => out.push(format!("{path}={:?}", f.string()?)),
                    Value::Ident(s) | Value::Number(s) => out.push(format!("{path}={s}")),
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        walk(&mut Reader::new(text), "", &mut out)?;
        Ok(out)
    }

    fn first_string(text: &str) -> Result<String, Error> {
        let mut r = Reader::new(text);
        let f = r.next_field()?.expect("a field");
        Ok(f.string()?.into_owned())
    }

    #[test]
    fn reads_what_encode_text_writes() {
        let text = "deps {\n  dependency_type: 1\n  target {\n    label: \"//a:b\"\n  }\n}\nkind_string: \"java_library\"\ntags: \"x\"\ntags: \"y\"\n";
        assert_eq!(
            flatten(text).unwrap(),
            [
                "deps.dependency_type=1",
                "deps.target.label=\"//a:b\"",
                "kind_string=\"java_library\"",
                "tags=\"x\"",
                "tags=\"y\"",
            ]
        );
    }

    #[test]
    fn plain_strings_are_borrowed() {
        let mut r = Reader::new("label: \"//a:b\"");
        let f = r.next_field().unwrap().unwrap();
        assert!(matches!(f.string().unwrap(), Cow::Borrowed("//a:b")));
    }

    #[test]
    fn decodes_escapes() {
        assert_eq!(
            first_string(r#"s: "a\nb\tc\"d\\e\'f\?""#).unwrap(),
            "a\nb\tc\"d\\e'f?"
        );
        assert_eq!(
            first_string(r#"s: "\a\b\f\v\r""#).unwrap(),
            "\x07\x08\x0c\x0b\r"
        );
        assert_eq!(first_string(r#"s: "\101\x42\x4a\0z""#).unwrap(), "ABJ\0z");
        // UTF-8 bytes of "é" and "€" as octal and hex escapes.
        assert_eq!(
            first_string(r#"s: "caf\303\251 \xe2\x82\xac""#).unwrap(),
            "café €"
        );
        assert_eq!(first_string(r#"s: "é\U0001F600😀""#).unwrap(), "é😀😀");
        assert_eq!(first_string("s: 'it''s' \"!\""), Ok("its!".to_owned()));
        assert_eq!(first_string("s: \"a\" # comment\n \"b\"").unwrap(), "ab");
        assert_eq!(first_string("s: \"raw é\"").unwrap(), "raw é");
    }

    #[test]
    fn invalid_utf8_from_escapes_is_replaced() {
        assert_eq!(
            first_string(r#"s: "a\377b\303""#).unwrap(),
            "a\u{FFFD}b\u{FFFD}"
        );
    }

    #[test]
    fn bad_escapes_are_errors_at_the_backslash() {
        for (text, offset) in [
            (r#"s: "ab\qc""#, 6),
            (r#"s: "\x""#, 4),
            (r#"s: "\u12""#, 4),
            (r#"s: "\ud83d""#, 4),
            (r#"s: "\U00110000""#, 4),
        ] {
            let mut r = Reader::new(text);
            let err = r.next_field().unwrap().unwrap().string().unwrap_err();
            assert_eq!(err.offset, offset, "{text}: {err}");
        }
    }

    #[test]
    fn scalars() {
        let text = "a: -12 b: 0x1F c: 017 d: true e: f f: 1 g: RUNTIME h: 1.5e3 i: -inf j: -9223372036854775808";
        let mut r = Reader::new(text);
        let mut fields = Vec::new();
        while let Some(f) = r.next_field().unwrap() {
            fields.push(f);
        }
        assert_eq!(fields[0].int(), Ok(-12));
        assert_eq!(fields[1].int(), Ok(31));
        assert_eq!(fields[2].int(), Ok(15));
        assert_eq!(fields[3].bool(), Ok(true));
        assert_eq!(fields[4].bool(), Ok(false));
        assert_eq!(fields[5].bool(), Ok(true));
        assert_eq!(fields[5].enum_value(), Ok(EnumValue::Number(1)));
        assert_eq!(fields[6].enum_value(), Ok(EnumValue::Name("RUNTIME")));
        assert!(matches!(fields[7].value, Value::Number("1.5e3")));
        assert!(fields[7].int().is_err());
        assert!(matches!(fields[8].value, Value::Number("-inf")));
        assert_eq!(fields[9].int(), Ok(i64::MIN));
        assert!(fields[3].int().is_err());
        assert!(fields[0].string().is_err());
        assert!(fields[0].bool().is_err());
    }

    #[test]
    fn integer_edge_cases() {
        assert_eq!(parse_int("0"), Some(0));
        assert_eq!(parse_int("-0"), Some(0));
        assert_eq!(parse_int("9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_int("9223372036854775808"), None);
        assert_eq!(parse_int("-9223372036854775809"), None);
        assert_eq!(parse_int("+5"), None);
        assert_eq!(parse_int("0x"), None);
        assert_eq!(parse_int("08"), None);
        assert_eq!(parse_int("-"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn lenient_syntax() {
        let text = "# header\nm: { a: 1; b: \"x\", } n < c: 2 > [ext.Name]: 3\nlist: [1, 2] msgs: [{ d: 4 }, < d: 5 >] empty: []";
        assert_eq!(
            flatten(text).unwrap(),
            [
                "m.a=1",
                "m.b=\"x\"",
                "n.c=2",
                "[ext.Name]=3",
                "list=1",
                "list=2",
                "msgs.d=4",
                "msgs.d=5"
            ]
        );
    }

    #[test]
    fn skips_unknown_messages() {
        let text = "big { x { y: \"}\" z: [1, 2] w: [{ q: 1 }] } v: 1 } after: 2";
        let mut r = Reader::new(text);
        let f = r.next_field().unwrap().unwrap();
        assert_eq!(f.name, "big");
        r.skip(&f).unwrap();
        let f = r.next_field().unwrap().unwrap();
        assert_eq!((f.name, f.int()), ("after", Ok(2)));
        assert!(r.next_field().unwrap().is_none());
    }

    #[test]
    fn skip_message_needs_a_message() {
        let mut r = Reader::new("a: 1");
        assert!(r.skip_message().is_err());
    }

    #[test]
    fn malformed_input_is_an_error_with_offset() {
        for (text, offset) in [
            ("a {\n b: 1\n", 2),
            ("a: 1 }", 5),
            ("a: \"open\n\"", 3),
            ("a: \"open", 3),
            ("a 1", 2),
            ("a:", 2),
            ("a: }", 3),
            ("1: 2", 0),
            ("a: [1 2]", 6),
            ("a: [1, 2", 0),
            ("[a: 1", 0),
            ("a { b: 1 >", 9),
            ("a: \"x\\", 3),
        ] {
            let err = flatten(text).unwrap_err();
            assert_eq!(err.offset, offset, "{text:?}: {err}");
        }
    }

    #[test]
    fn describe_gives_line_and_column() {
        let text = "a {\n  b: 1\n  c: }\n}";
        let err = flatten(text).unwrap_err();
        assert_eq!(
            err.describe(text),
            "line 3, column 6 (byte 16): expected a value for field `c`"
        );
        assert_eq!(err.to_string(), "byte 16: expected a value for field `c`");
    }

    #[test]
    fn empty_input_has_no_fields() {
        assert!(flatten("").unwrap().is_empty());
        assert!(flatten("  # only a comment").unwrap().is_empty());
    }
}
