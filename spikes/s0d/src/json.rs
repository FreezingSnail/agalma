//! Minimal std-only JSON reader.
//!
//! Only what the S0d spike needs: parse a document and read fields. There is no
//! serializer because every transcript and evidence file is plain text. No
//! third-party crates; the whole point of the spike is a zero-dependency build.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser { bytes: text.as_bytes(), pos: 0 };
        p.skip_ws();
        let value = p.value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(format!("trailing data at byte {}", p.pos));
        }
        Ok(value)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|n| n.max(0.0) as u64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// Follow a chain of object keys, e.g. `path(&["data", "id"])`.
    pub fn path(&self, keys: &[&str]) -> Option<&Json> {
        let mut cur = self;
        for key in keys {
            cur = cur.get(key)?;
        }
        Some(cur)
    }

    pub fn str_at(&self, keys: &[&str]) -> Option<&str> {
        self.path(keys).and_then(Json::as_str)
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn expect(&mut self, want: u8) -> Result<(), String> {
        match self.bump() {
            Some(b) if b == want => Ok(()),
            other => Err(format!("expected '{}' at byte {}, got {:?}", want as char, self.pos, other)),
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some(_) => self.number(),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        for expected in word.bytes() {
            self.expect(expected)?;
        }
        Ok(())
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                other => return Err(format!("expected ',' or '}}' at byte {}, got {:?}", self.pos, other)),
            }
        }
        Ok(Json::Obj(fields))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            let value = self.value()?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                other => return Err(format!("expected ',' or ']' at byte {}, got {:?}", self.pos, other)),
            }
        }
        Ok(Json::Arr(items))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_string()),
                Some(b'"') => break,
                Some(b'\\') => match self.bump() {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'b') => out.push('\u{0008}'),
                    Some(b'f') => out.push('\u{000c}'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        let cp = self.hex4()?;
                        if (0xD800..0xDC00).contains(&cp) {
                            // high surrogate: expect a following low surrogate
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            let lo = self.hex4()?;
                            let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                        } else {
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                    }
                    other => return Err(format!("bad escape {:?}", other)),
                },
                Some(b) => out.push(b as char),
            }
        }
        Ok(out)
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self.bump().ok_or_else(|| "short unicode escape".to_string())?;
            let digit = (b as char).to_digit(16).ok_or_else(|| "bad hex digit".to_string())?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => self.pos += 1,
                _ => break,
            }
        }
        let slice = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "non-utf8 number".to_string())?;
        slice
            .parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("bad number {:?} at byte {}", slice, start))
    }
}
