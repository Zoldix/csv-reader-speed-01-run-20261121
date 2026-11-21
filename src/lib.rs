// csv-wasm: a fast drop-in for make.fun's csv-reader 1.0.0, written from scratch.
// It parses CSV exactly as the oracle (vendor/csv-reader/index.js) does, but over
// UTF-8 bytes with v128 SIMD scans, and can transduce a whole cases.json buffer
// (JSON in, answers JSON out) without any JS JSON.parse / JSON.stringify.
//
// Semantics (mirroring the oracle, char-for-char on UTF-8 bytes; delimiters are
// ASCII so they can never occur inside a multi-byte UTF-8 sequence):
//  - U+FEFF (EF BB BF) at the very start is dropped.
//  - A record ends at LF or CRLF; a line left with no characters is no record.
//  - Outside quotes a CR not followed by LF is dropped (stray_cr).
//  - A quoted field runs to the next non-doubled quote; "" inside is one quote;
//    CRLF / lone CR inside read as LF; junk after the closing quote is appended
//    as it stands (quote_junk when anything was kept); a quote that never closes
//    is a literal and the rest is re-read as unquoted (unterminated_quote).
//  - A quote inside an unquoted field is literal. Fields stay strings.
//  - columns = first record iff header, else null; rows padded with "" (short_row,
//    with fields: count) or cut (long_row, with extra: the cut fields) to the
//    first record's width. Errors {code, line, row} in encounter order; line is
//    the 1-based line the record starts on, row its index in rows (null: header).

use core::arch::wasm32::*;
use std::vec::Vec;

const COMMA: u8 = 0;
const LB: u8 = 1;
const END: u8 = 2;

// ---------- small helpers ----------

fn hexv(b: u8) -> u32 {
    match b {
        b'0'..=b'9' => (b - b'0') as u32,
        b'a'..=b'f' => (b - b'a' + 10) as u32,
        b'A'..=b'F' => (b - b'A' + 10) as u32,
        _ => 0,
    }
}
fn hexd(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'a' + n - 10 }
}
fn push_u32(out: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 10];
    let mut i = 10;
    if v == 0 {
        out.push(b'0');
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}
fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

/// One byte of a field value, JSON-escaped, appended to `out`.
fn esc_byte(out: &mut Vec<u8>, b: u8) {
    match b {
        b'"' => out.extend_from_slice(b"\\\""),
        b'\\' => out.extend_from_slice(b"\\\\"),
        b'\n' => out.extend_from_slice(b"\\n"),
        b'\r' => out.extend_from_slice(b"\\r"),
        b'\t' => out.extend_from_slice(b"\\t"),
        0x08 => out.extend_from_slice(b"\\b"),
        0x0C => out.extend_from_slice(b"\\f"),
        0x00..=0x1F => {
            out.extend_from_slice(b"\\u00");
            out.push(hexd(b >> 4));
            out.push(hexd(b & 15));
        }
        _ => out.push(b),
    }
}

// ---------- SIMD scans ----------

/// Bitmask of lanes needing JSON escaping: '"' | '\\' | byte < 0x20.
#[inline(always)]
fn esc_mask(v: v128) -> u32 {
    unsafe {
        let q = i8x16_eq(v, i8x16_splat(b'"' as i8));
        let b = i8x16_eq(v, i8x16_splat(b'\\' as i8));
        let c = u8x16_lt(v, u8x16_splat(0x20));
        i8x16_bitmask(v128_or(v128_or(q, b), c)) as u32
    }
}

/// Bitmask of CSV delimiter lanes: ',' | '\n' | '\r'.
#[inline(always)]
fn delim_mask(v: v128) -> u32 {
    unsafe {
        let c = i8x16_eq(v, i8x16_splat(b',' as i8));
        let n = i8x16_eq(v, i8x16_splat(b'\n' as i8));
        let r = i8x16_eq(v, i8x16_splat(b'\r' as i8));
        i8x16_bitmask(v128_or(v128_or(c, n), r)) as u32
    }
}

/// Append `s` to `out` with JSON escaping, bulk-copying clean 16-byte chunks.
/// `s` must come from a buffer with >= 16 readable bytes past its end (our CSV
/// buffer is padded), so a short tail can be checked with one masked v128 load.
#[inline(always)]
fn push_escaped(out: &mut Vec<u8>, s: &[u8]) {
    let mut i = 0;
    while i + 16 <= s.len() {
        let v = unsafe { v128_load(s.as_ptr().add(i) as *const v128) };
        if esc_mask(v) == 0 {
            out.extend_from_slice(&s[i..i + 16]);
            i += 16;
        } else {
            for j in 0..16 {
                esc_byte(out, s[i + j]);
            }
            i += 16;
        }
    }
    let rem = s.len() - i;
    if rem > 0 {
        let v = unsafe { v128_load(s.as_ptr().add(i) as *const v128) };
        let m = esc_mask(v) & ((1u32 << rem) - 1);
        if m == 0 {
            out.extend_from_slice(&s[i..]);
        } else {
            while i < s.len() {
                esc_byte(out, s[i]);
                i += 1;
            }
        }
    }
}

/// First index >= `i` whose byte is ',' | '\n' | '\r', or `n`, plus whether
/// any byte in [i, that index) needs JSON escaping. One v128 load per chunk
/// serves both masks. `d` must have >= 16 readable bytes past `n` (padded).
#[inline(always)]
fn scan_delim(d: &[u8], n: usize, mut i: usize) -> (usize, bool) {
    let mut esc = false;
    while i + 16 <= n {
        let v = unsafe { v128_load(d.as_ptr().add(i) as *const v128) };
        let m = delim_mask(v);
        if m != 0 {
            let k = m.trailing_zeros() as usize;
            esc |= (esc_mask(v) & ((1u32 << k) - 1)) != 0;
            return (i + k, esc);
        }
        esc |= esc_mask(v) != 0;
        i += 16;
    }
    if i < n {
        let v = unsafe { v128_load(d.as_ptr().add(i) as *const v128) };
        let bits = (1u32 << (n - i)) - 1;
        let m = delim_mask(v) & bits;
        if m != 0 {
            let k = m.trailing_zeros() as usize;
            esc |= (esc_mask(v) & ((1u32 << k) - 1)) != 0;
            return (i + k, esc);
        }
        esc |= (esc_mask(v) & bits) != 0;
    }
    (n, esc)
}

// ---------- CSV parser ----------
// Streaming design: each record is emitted straight into `rec` (its JSON array
// under construction) while it is scanned; `offs[k]` is the offset in `rec` of
// field k's opening quote, which lets a long row be split into its row part
// and its `extra` array without a second pass over the fields. Dirty fields
// (doubled quotes, CRs, junk, unterminated) are first built JSON-escaped in
// `scratch`, then copied into `rec`. Clean fields are bulk-copied.

struct P<'a> {
    d: &'a [u8],
    n: usize, // true length; d has 16 zero bytes past n
    pos: usize,
    line: u32,
    header: bool,
    rec_line: u32,
    rec_row: i64, // -1 = null (header record)
    width: i32,
    rows_n: usize,
    cols_set: bool,
    cols: Vec<u8>,
    rows: Vec<u8>,
    rows_first: bool,
    errs: Vec<u8>,
    errs_first: bool,
    scratch: Vec<u8>,
    rec: Vec<u8>, // current record's JSON array, under construction
    split_off: u32, // offset in rec of field #width's opening quote (long rows)
    nfields: usize,
    f0_empty: bool,
    f0_quoted: bool,
}

impl<'a> P<'a> {
    fn new(d: &'a [u8], n: usize, header: bool) -> Self {
        P {
            d,
            n,
            pos: 0,
            line: 1,
            header,
            rec_line: 1,
            rec_row: -1,
            width: -1,
            rows_n: 0,
            cols_set: false,
            cols: Vec::new(),
            rows: Vec::with_capacity(d.len()),
            rows_first: true,
            errs: Vec::with_capacity(4096),
            errs_first: true,
            scratch: Vec::with_capacity(512),
            rec: Vec::with_capacity(1024),
            split_off: 0,
            nfields: 0,
            f0_empty: false,
            f0_quoted: false,
        }
    }

    fn err_simple(&mut self, code: &str) {
        if !self.errs_first {
            self.errs.push(b',');
        } else {
            self.errs_first = false;
        }
        self.errs.extend_from_slice(b"{\"code\":\"");
        self.errs.extend_from_slice(code.as_bytes());
        self.errs.extend_from_slice(b"\",\"line\":");
        push_u32(&mut self.errs, self.rec_line);
        self.errs.extend_from_slice(b",\"row\":");
        if self.rec_row < 0 {
            self.errs.extend_from_slice(b"null");
        } else {
            push_u32(&mut self.errs, self.rec_row as u32);
        }
        self.errs.push(b'}');
    }

    fn err_short(&mut self, nfields: usize) {
        if !self.errs_first {
            self.errs.push(b',');
        } else {
            self.errs_first = false;
        }
        self.errs.extend_from_slice(b"{\"code\":\"short_row\",\"line\":");
        push_u32(&mut self.errs, self.rec_line);
        self.errs.extend_from_slice(b",\"row\":");
        push_u32(&mut self.errs, self.rec_row as u32);
        self.errs.extend_from_slice(b",\"fields\":");
        push_u32(&mut self.errs, nfields as u32);
        self.errs.push(b'}');
    }

    fn err_long(&mut self, extra: &[u8]) {
        if !self.errs_first {
            self.errs.push(b',');
        } else {
            self.errs_first = false;
        }
        self.errs.extend_from_slice(b"{\"code\":\"long_row\",\"line\":");
        push_u32(&mut self.errs, self.rec_line);
        self.errs.extend_from_slice(b",\"row\":");
        push_u32(&mut self.errs, self.rec_row as u32);
        self.errs.extend_from_slice(b",\"extra\":");
        self.errs.extend_from_slice(extra);
        self.errs.push(b'}');
    }

    /// Open a field in `rec`: separator + opening quote. When the field being
    /// opened is #width, remember its offset: a long row is split there.
    #[inline(always)]
    fn field_begin(&mut self, quoted: bool) {
        if self.nfields == 0 {
            self.f0_quoted = quoted;
        }
        if self.nfields > 0 {
            self.rec.push(b',');
        }
        if self.width > 0 && self.nfields as i32 == self.width {
            self.split_off = self.rec.len() as u32;
        }
        self.rec.push(b'"');
    }

    /// Close a clean field whose raw bytes are d[start..start+len].
    #[inline(always)]
    fn field_end_clean(&mut self, start: usize, len: usize, esc: bool) {
        if esc {
            push_escaped(&mut self.rec, &self.d[start..start + len]);
        } else {
            self.rec.extend_from_slice(&self.d[start..start + len]);
        }
        self.rec.push(b'"');
        if self.nfields == 0 {
            self.f0_empty = len == 0;
        }
        self.nfields += 1;
    }

    /// Close a dirty field whose JSON-escaped bytes are scratch[ds..].
    #[inline(always)]
    fn field_end_scratch(&mut self, ds: usize, empty: bool) {
        self.rec.extend_from_slice(&self.scratch[ds..]);
        self.rec.push(b'"');
        if self.nfields == 0 {
            self.f0_empty = empty;
        }
        self.nfields += 1;
    }

    /// Unquoted field, scalar fallback: handles stray CRs (with errors) and any
    /// prefix byte (the literal opening quote of an unterminated quoted field).
    fn unquoted_scalar(&mut self, start: usize, prefix: Option<u8>, quoted: bool) -> u8 {
        self.field_begin(quoted);
        let ds = self.scratch.len();
        if let Some(p) = prefix {
            esc_byte(&mut self.scratch, p);
        }
        let mut i = start;
        let end;
        loop {
            if i >= self.n {
                self.pos = i;
                end = END;
                break;
            }
            let b = self.d[i];
            if b == b',' {
                self.pos = i + 1;
                end = COMMA;
                break;
            }
            if b == b'\n' {
                self.pos = i + 1;
                self.line += 1;
                end = LB;
                break;
            }
            if b == b'\r' {
                if i + 1 < self.n && self.d[i + 1] == b'\n' {
                    self.pos = i + 2;
                    self.line += 1;
                    end = LB;
                    break;
                }
                self.err_simple("stray_cr");
                i += 1;
                continue;
            }
            esc_byte(&mut self.scratch, b);
            i += 1;
        }
        let empty = self.scratch.len() == ds;
        self.field_end_scratch(ds, empty);
        end
    }

    /// Transform quoted content [cs, ce) (all '"' in it are doubled pairs, CRs
    /// become LF) into JSON-escaped bytes appended to scratch; returns start.
    fn quoted_transform(&mut self, cs: usize, ce: usize) -> usize {
        let ds = self.scratch.len();
        let mut i = cs;
        while i < ce {
            let b = self.d[i];
            if b == b'"' {
                // doubled quote: one literal quote
                esc_byte(&mut self.scratch, b'"');
                i += 2;
            } else if b == b'\r' {
                esc_byte(&mut self.scratch, b'\n');
                i += if i + 1 < ce && self.d[i + 1] == b'\n' { 2 } else { 1 };
            } else {
                esc_byte(&mut self.scratch, b);
                i += 1;
            }
        }
        ds
    }

    #[inline(always)]
    fn parse_unquoted(&mut self) -> u8 {
        let start = self.pos;
        let (d1, esc) = scan_delim(self.d, self.n, self.pos);
        if d1 >= self.n {
            self.pos = d1;
            self.field_begin(false);
            self.field_end_clean(start, d1 - start, esc);
            return END;
        }
        let b = self.d[d1];
        if b == b',' {
            self.pos = d1 + 1;
            self.field_begin(false);
            self.field_end_clean(start, d1 - start, esc);
            return COMMA;
        }
        if b == b'\n' {
            self.pos = d1 + 1;
            self.line += 1;
            self.field_begin(false);
            self.field_end_clean(start, d1 - start, esc);
            return LB;
        }
        // '\r'
        if d1 + 1 < self.n && self.d[d1 + 1] == b'\n' {
            self.pos = d1 + 2;
            self.line += 1;
            self.field_begin(false);
            self.field_end_clean(start, d1 - start, esc);
            return LB;
        }
        // stray CR: re-read the whole field the slow way
        self.unquoted_scalar(start, None, false)
    }

    #[inline(always)]
    fn parse_quoted(&mut self) -> u8 {
        let open = self.pos;
        let open_line = self.line;
        let cs = open + 1;
        let n = self.n;
        let mut i = cs;
        let mut dirty = false;
        let mut esc = false;
        let mut closed: Option<usize> = None;
        'scan: while i < n {
            if i + 16 <= n {
                let v = unsafe { v128_load(self.d.as_ptr().add(i) as *const v128) };
                let qm = unsafe { i8x16_bitmask(i8x16_eq(v, i8x16_splat(b'"' as i8))) } as u32;
                let rm = unsafe { i8x16_bitmask(i8x16_eq(v, i8x16_splat(b'\r' as i8))) } as u32;
                let nm = unsafe { i8x16_bitmask(i8x16_eq(v, i8x16_splat(b'\n' as i8))) } as u32;
                if qm == 0 && rm == 0 {
                    self.line += nm.count_ones();
                    esc |= esc_mask(v) != 0;
                    i += 16;
                    continue;
                }
                let stop = (qm | rm).trailing_zeros() as usize;
                // '\n' bytes before `stop` each add a line (no '\r' can hide
                // before `stop`: it would be the stop itself).
                let pre_n = nm & ((1u32 << stop) - 1);
                self.line += pre_n.count_ones();
                esc |= (esc_mask(v) & ((1u32 << stop) - 1)) != 0;
                let idx = i + stop;
                if (qm >> stop) & 1 == 1 {
                    if idx + 1 < n && self.d[idx + 1] == b'"' {
                        dirty = true;
                        i = idx + 2;
                        continue;
                    }
                    closed = Some(idx);
                    break 'scan;
                } else {
                    // '\r' inside quotes -> one LF
                    dirty = true;
                    self.line += 1;
                    i = if idx + 1 < n && self.d[idx + 1] == b'\n' {
                        idx + 2
                    } else {
                        idx + 1
                    };
                    continue 'scan;
                }
            } else {
                // tail: scalar scan
                let mut j = i;
                while j < n {
                    let b = self.d[j];
                    if b == b'"' {
                        if j + 1 < n && self.d[j + 1] == b'"' {
                            dirty = true;
                            j += 2;
                            continue;
                        }
                        closed = Some(j);
                        break 'scan;
                    }
                    if b == b'\r' {
                        dirty = true;
                        self.line += 1;
                        j = if j + 1 < n && self.d[j + 1] == b'\n' {
                            j + 2
                        } else {
                            j + 1
                        };
                        i = j;
                        continue 'scan;
                    }
                    if b == b'\n' {
                        self.line += 1;
                    }
                    if b == b'\\' || b < 0x20 {
                        esc = true;
                    }
                    j += 1;
                }
                i = j;
                break 'scan;
            }
        }
        let qpos = match closed {
            Some(q) => q,
            None => {
                // The quote never closes: literal, re-read as unquoted.
                self.line = open_line;
                self.err_simple("unterminated_quote");
                return self.unquoted_scalar(open + 1, Some(b'"'), true);
            }
        };
        self.pos = qpos + 1;
        // Close the field, unless junk follows the closing quote.
        if self.pos < n {
            let c = self.d[self.pos];
            if c != b',' && c != b'\n' && !(c == b'\r' && self.pos + 1 < n && self.d[self.pos + 1] == b'\n') {
                // Junk after the closing quote: read as unquoted, appended.
                self.field_begin(true);
                let ds = self.quoted_transform(cs, qpos);
                let mut kept = 0usize;
                let mut j = self.pos;
                let end;
                loop {
                    if j >= n {
                        self.pos = j;
                        end = END;
                        break;
                    }
                    let b = self.d[j];
                    if b == b',' {
                        self.pos = j + 1;
                        end = COMMA;
                        break;
                    }
                    if b == b'\n' {
                        self.pos = j + 1;
                        self.line += 1;
                        end = LB;
                        break;
                    }
                    if b == b'\r' {
                        if j + 1 < n && self.d[j + 1] == b'\n' {
                            self.pos = j + 2;
                            self.line += 1;
                            end = LB;
                            break;
                        }
                        self.err_simple("stray_cr");
                        j += 1;
                        continue;
                    }
                    esc_byte(&mut self.scratch, b);
                    kept += 1;
                    j += 1;
                }
                if kept > 0 {
                    self.err_simple("quote_junk");
                }
                let empty = self.scratch.len() == ds;
                self.field_end_scratch(ds, empty);
                return end;
            }
        }
        if self.pos >= n {
            self.field_begin(true);
            if dirty {
                let ds = self.quoted_transform(cs, qpos);
                let empty = self.scratch.len() == ds;
                self.field_end_scratch(ds, empty);
            } else {
                self.field_end_clean(cs, qpos - cs, esc);
            }
            return END;
        }
        let c = self.d[self.pos];
        self.field_begin(true);
        if dirty {
            let ds = self.quoted_transform(cs, qpos);
            let empty = self.scratch.len() == ds;
            self.field_end_scratch(ds, empty);
        } else {
            self.field_end_clean(cs, qpos - cs, esc);
        }
        if c == b',' {
            self.pos += 1;
            COMMA
        } else {
            // '\n' or CRLF
            self.line += 1;
            self.pos += if c == b'\r' { 2 } else { 1 };
            LB
        }
    }

    #[inline(always)]
    fn parse_field(&mut self) -> u8 {
        if self.pos < self.n && self.d[self.pos] == b'"' {
            self.parse_quoted()
        } else {
            self.parse_unquoted()
        }
    }

    fn parse_all(&mut self) {
        if self.n >= 3 && self.d[0] == 0xEF && self.d[1] == 0xBB && self.d[2] == 0xBF {
            self.pos = 3;
        }
        loop {
            // Lines with no characters are no records.
            while self.pos < self.n {
                let b = self.d[self.pos];
                if b == b'\n' {
                    self.pos += 1;
                    self.line += 1;
                } else if b == b'\r'
                    && self.pos + 1 < self.n
                    && self.d[self.pos + 1] == b'\n'
                {
                    self.pos += 2;
                    self.line += 1;
                } else {
                    break;
                }
            }
            if self.pos >= self.n {
                break;
            }
            self.rec_line = self.line;
            self.rec_row = if self.header && !self.cols_set {
                -1
            } else {
                self.rows_n as i64
            };
            self.rec.clear();
            self.rec.push(b'[');
            self.scratch.clear();
            self.nfields = 0;
            self.f0_empty = false;
            self.f0_quoted = false;
            loop {
                let e = self.parse_field();
                if e != COMMA {
                    break;
                }
            }
            let n = self.nfields;
            // A line that held only stray CRs is left with no characters.
            if n == 1 && self.f0_empty && !self.f0_quoted {
                continue;
            }
            if self.width < 0 {
                self.width = n as i32;
            }
            if self.header && !self.cols_set {
                self.rec.push(b']');
                self.cols = std::mem::take(&mut self.rec);
                self.cols_set = true;
                continue;
            }
            let w = self.width as usize;
            if n < w {
                self.err_short(n);
                for _ in n..w {
                    self.rec.push(b',');
                    self.rec.extend_from_slice(b"\"\"");
                }
                self.rec.push(b']');
            } else if n > w {
                let mut extra = Vec::with_capacity(64);
                extra.push(b'[');
                extra.extend_from_slice(&self.rec[self.split_off as usize..]);
                extra.push(b']');
                self.err_long(&extra);
                let cut = self.split_off as usize - 1; // drop the comma before field w
                self.rec.truncate(cut);
                self.rec.push(b']');
            } else {
                self.rec.push(b']');
            }
            if !self.rows_first {
                self.rows.push(b',');
            } else {
                self.rows_first = false;
            }
            self.rows.extend_from_slice(&self.rec);
            self.rows_n += 1;
        }
    }

    fn finish_into(&self, a: &mut Vec<u8>) {
        a.extend_from_slice(b"{\"columns\":");
        if self.header {
            if self.cols_set {
                a.extend_from_slice(&self.cols);
            } else {
                a.extend_from_slice(b"[]");
            }
        } else {
            a.extend_from_slice(b"null");
        }
        a.extend_from_slice(b",\"rows\":[");
        a.extend_from_slice(&self.rows);
        a.extend_from_slice(b"],\"errors\":[");
        a.extend_from_slice(&self.errs);
        a.extend_from_slice(b"]}");
    }
}

/// Parse CSV bytes; `d` must have 16 readable bytes past `n` (padded buffer).
fn parse_csv_into(d: &[u8], n: usize, header: bool, out: &mut Vec<u8>) {
    let mut p = P::new(d, n, header);
    p.parse_all();
    p.finish_into(out);
}

fn parse_csv(d: &[u8], header: bool) -> Vec<u8> {
    // Copy into a buffer with 16 zero bytes of slack past the end, so SIMD
    // loads may safely read up to 15 bytes past any field's end.
    let mut padded = Vec::with_capacity(d.len() + 16);
    padded.extend_from_slice(d);
    padded.extend_from_slice(&[0u8; 16]);
    let n = d.len();
    let mut out = Vec::with_capacity(n + n / 8);
    parse_csv_into(&padded, n, header, &mut out);
    out
}

// ---------- JSON (cases.json) transducer ----------

struct J<'a> {
    b: &'a [u8],
    n: usize, // true length; b has 16 zero bytes past n
    p: usize,
}

impl<'a> J<'a> {
    fn ws(&mut self) {
        while self.p < self.n {
            match self.b[self.p] {
                b' ' | b'\t' | b'\n' | b'\r' => self.p += 1,
                _ => break,
            }
        }
    }

    fn hex4(&mut self) -> u32 {
        let mut v = 0u32;
        for _ in 0..4 {
            if self.p < self.n {
                v = (v << 4) | hexv(self.b[self.p]);
                self.p += 1;
            }
        }
        v
    }

    /// Decode a JSON string (b[p] == '"') into `out` as UTF-8 bytes.
    /// Bulk-copies clean 16-byte runs (no '"' and no '\\') with SIMD.
    fn parse_string(&mut self, out: &mut Vec<u8>) {
        self.p += 1; // opening quote
        loop {
            // Fast run: copy whole clean chunks until '"' or '\\'.
            while self.p + 16 <= self.n {
                let v = unsafe { v128_load(self.b.as_ptr().add(self.p) as *const v128) };
                let q = unsafe { i8x16_bitmask(i8x16_eq(v, i8x16_splat(b'"' as i8))) } as u32;
                let e = unsafe { i8x16_bitmask(i8x16_eq(v, i8x16_splat(b'\\' as i8))) } as u32;
                let m = q | e;
                if m == 0 {
                    out.extend_from_slice(&self.b[self.p..self.p + 16]);
                    self.p += 16;
                } else {
                    let stop = m.trailing_zeros() as usize;
                    out.extend_from_slice(&self.b[self.p..self.p + stop]);
                    self.p += stop;
                    break;
                }
            }
            if self.p >= self.n {
                break;
            }
            let c = self.b[self.p];
            self.p += 1;
            if c == b'"' {
                break;
            }
            if c == b'\\' {
                let e = self.b[self.p];
                self.p += 1;
                match e {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let cp = self.hex4();
                        let cp = if (0xD800..0xDC00).contains(&cp) {
                            if self.p + 1 < self.n
                                && self.b[self.p] == b'\\'
                                && self.b[self.p + 1] == b'u'
                            {
                                let save = self.p;
                                self.p += 2;
                                let lo = self.hex4();
                                if (0xDC00..0xE000).contains(&lo) {
                                    0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    self.p = save;
                                    0xFFFD
                                }
                            } else {
                                0xFFFD
                            }
                        } else if (0xDC00..0xE000).contains(&cp) {
                            0xFFFD
                        } else {
                            cp
                        };
                        push_utf8(out, cp);
                    }
                    _ => out.push(e),
                }
            } else {
                out.push(c);
            }
        }
    }

    /// Skip any JSON value (used for keys we do not need, e.g. "id").
    fn skip_value(&mut self, tmp: &mut Vec<u8>) {
        self.ws();
        match self.b[self.p] {
            b'"' => {
                tmp.clear();
                self.parse_string(tmp);
            }
            b'{' => {
                self.p += 1;
                loop {
                    self.ws();
                    if self.b[self.p] == b'}' {
                        self.p += 1;
                        return;
                    }
                    if self.b[self.p] == b',' {
                        self.p += 1;
                        continue;
                    }
                    tmp.clear();
                    self.parse_string(tmp);
                    self.ws();
                    self.p += 1; // ':'
                    self.skip_value(tmp);
                }
            }
            b'[' => {
                self.p += 1;
                loop {
                    self.ws();
                    if self.b[self.p] == b']' {
                        self.p += 1;
                        return;
                    }
                    if self.b[self.p] == b',' {
                        self.p += 1;
                        continue;
                    }
                    self.skip_value(tmp);
                }
            }
            b't' => self.p += 4,
            b'f' => self.p += 5,
            b'n' => self.p += 4,
            _ => {
                while self.p < self.n {
                    match self.b[self.p] {
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9' => self.p += 1,
                        _ => break,
                    }
                }
            }
        }
    }

    /// Parse one case object {"id":..,"input":{"text":..,"header":..}} (or the
    /// inner "input" object itself): decodes "text" into `csv`, "header" into
    /// `header`. Other keys are skipped.
    fn parse_case_object(&mut self, csv: &mut Vec<u8>, header: &mut bool, tmp: &mut Vec<u8>) {
        self.ws();
        self.p += 1; // '{'
        loop {
            self.ws();
            if self.p >= self.n {
                return;
            }
            if self.b[self.p] == b'}' {
                self.p += 1;
                return;
            }
            if self.b[self.p] == b',' {
                self.p += 1;
                continue;
            }
            tmp.clear();
            self.parse_string(tmp);
            let is_text = tmp.as_slice() == b"text";
            let is_header = tmp.as_slice() == b"header";
            let is_input = tmp.as_slice() == b"input";
            self.ws();
            self.p += 1; // ':'
            self.ws();
            if is_text {
                self.parse_string(csv);
            } else if is_header {
                if self.b[self.p] == b't' {
                    *header = true;
                    self.p += 4;
                } else {
                    *header = false;
                    self.p += 5;
                }
            } else if is_input {
                self.parse_case_object(csv, header, tmp);
            } else {
                self.skip_value(tmp);
            }
        }
    }
}

fn run_cases_impl(input: &[u8], out: &mut Vec<u8>) {
    let n = input.len();
    let mut j = J { b: input, n, p: 0 };
    let mut tmp: Vec<u8> = Vec::with_capacity(64);
    let mut csv: Vec<u8> = Vec::with_capacity(input.len() / 2);
    out.reserve(input.len() + input.len() / 8);
    out.extend_from_slice(b"[");
    j.ws();
    j.p += 1; // '['
    let mut first = true;
    loop {
        j.ws();
        if j.p >= n || j.b[j.p] == b']' {
            break;
        }
        if j.b[j.p] == b',' {
            j.p += 1;
            continue;
        }
        let mut header = false;
        csv.clear();
        j.parse_case_object(&mut csv, &mut header, &mut tmp);
        if !first {
            out.push(b',');
        }
        first = false;
        csv.extend_from_slice(&[0u8; 16]);
        let n = csv.len() - 16;
        parse_csv_into(&csv, n, header, out);
        csv.truncate(n);
    }
    out.extend_from_slice(b"]");
}

// ---------- exports ----------

static mut OUT: Vec<u8> = Vec::new();

#[no_mangle]
pub extern "C" fn alloc(n: u32) -> u32 {
    let mut v = Vec::with_capacity(n as usize);
    let p: *mut u8 = v.as_mut_ptr();
    std::mem::forget(v);
    p as u32
}

#[no_mangle]
pub extern "C" fn out_ptr() -> u32 {
    unsafe { OUT.as_ptr() as u32 }
}

/// Transduce a whole cases.json buffer; output is answers.json bytes.
#[no_mangle]
pub extern "C" fn run_cases(ptr: u32, len: u32) -> u32 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let mut out = Vec::new();
    run_cases_impl(input, &mut out);
    let n = out.len() as u32;
    unsafe {
        OUT = out;
    }
    n
}

/// Parse one CSV text (UTF-8 bytes); output is one answer object as JSON.
#[no_mangle]
pub extern "C" fn run_one(ptr: u32, len: u32, header: u32) -> u32 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = parse_csv(input, header != 0);
    let n = out.len() as u32;
    unsafe {
        OUT = out;
    }
    n
}
