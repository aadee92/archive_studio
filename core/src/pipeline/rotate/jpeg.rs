//! Lossless JPEG transforms in the DCT domain, as `jpegtran` does them.
//!
//! The scans are Huffman-decoded to their quantized coefficients (baseline, extended and
//! progressive files; not arithmetic-coded or lossless ones), each 8×8 block is moved and
//! transposed or mirrored by negating its odd frequencies, and the result is written as one
//! sequential scan with Huffman tables optimized for it. No sample is decoded or quantized
//! again, so the image is exactly the original one turned.
//!
//! Blocks can only move whole: an edge whose partial MCU would have to land at the other
//! side is dropped (`jpegtran -trim`), at most 15 pixels and only when the image's width or
//! height isn't a multiple of its MCU (8 or 16 pixels).
//!
//! Every segment other than the image data's (APPn, COM) is kept as it was and in order,
//! as are the bytes after the end of the image (an MPF preview's offsets aren't updated).
//! Restart markers aren't written again.

use std::io;

use super::Transform;

/// Coefficient index (row-major, `v * 8 + u`) of each position in zigzag order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35,
    42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62,
    63,
];

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DHT: u8 = 0xC4;
const DRI: u8 = 0xDD;
const COM: u8 = 0xFE;

type Block = [i16; 64];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// A decoded JPEG: its kept segments and every block's quantized coefficients.
pub struct Jpeg {
    /// APPn and COM segments as (marker, payload), in file order.
    pub segments: Vec<(u8, Vec<u8>)>,
    pub width: usize,
    pub height: usize,
    /// Quantization tables, in row-major order.
    tables: [Option<[u16; 64]>; 4],
    components: Vec<Component>,
    /// Whatever followed the end of the image (another program's trailer, an MPF image).
    trailer: Vec<u8>,
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    table: usize,
    /// Blocks across and down, covering whole MCUs.
    across: usize,
    down: usize,
    blocks: Vec<Block>,
}

impl Jpeg {
    fn max_h(&self) -> usize {
        self.components.iter().map(|c| c.h).max().unwrap_or(1)
    }

    fn max_v(&self) -> usize {
        self.components.iter().map(|c| c.v).max().unwrap_or(1)
    }

    /// The image `t` makes of this one, as `(width, height)`, after the trim.
    pub fn transformed_size(&self, t: Transform) -> io::Result<(usize, usize)> {
        let (mut width, mut height) = if t.swap { (self.height, self.width) } else { (self.width, self.height) };
        let (mcu_w, mcu_h) = if t.swap { (8 * self.max_v(), 8 * self.max_h()) } else { (8 * self.max_h(), 8 * self.max_v()) };
        if t.flip_x {
            width -= width % mcu_w;
        }
        if t.flip_y {
            height -= height % mcu_h;
        }
        if width == 0 || height == 0 {
            return Err(invalid("image smaller than one MCU"));
        }
        Ok((width, height))
    }

    /// This image with `t` applied to its blocks, trimmed as the module docs say.
    pub fn transform(&mut self, t: Transform) -> io::Result<()> {
        let (width, height) = self.transformed_size(t)?;
        let (max_h, max_v) = if t.swap { (self.max_v(), self.max_h()) } else { (self.max_h(), self.max_v()) };
        let (mcus_x, mcus_y) = (width.div_ceil(8 * max_h), height.div_ceil(8 * max_v));
        for c in &mut self.components {
            let (h, v) = if t.swap { (c.v, c.h) } else { (c.h, c.v) };
            let (across, down) = (mcus_x * h, mcus_y * v);
            let mut blocks = vec![[0; 64]; across * down];
            for y in 0..down {
                for x in 0..across {
                    // Flipped axes are whole MCUs after the trim, so the far block is the last one.
                    let a = if t.flip_x { across - 1 - x } else { x };
                    let b = if t.flip_y { down - 1 - y } else { y };
                    let (sx, sy) = if t.swap { (b, a) } else { (a, b) };
                    if sx < c.across && sy < c.down {
                        blocks[y * across + x] = transform_block(&c.blocks[sy * c.across + sx], t);
                    }
                }
            }
            (c.h, c.v, c.across, c.down, c.blocks) = (h, v, across, down, blocks);
        }
        if t.swap {
            for table in self.tables.iter_mut().flatten() {
                *table = std::array::from_fn(|i| table[(i % 8) * 8 + i / 8]);
            }
        }
        (self.width, self.height) = (width, height);
        Ok(())
    }

    /// Reads a JPEG file's contents.
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.get(..2) != Some(&[0xFF, SOI]) {
            return Err(invalid("not a JPEG file"));
        }
        let mut jpeg = Jpeg { segments: Vec::new(), width: 0, height: 0, tables: [None; 4], components: Vec::new(), trailer: Vec::new() };
        let mut huffman: [[Option<Huffman>; 4]; 2] = Default::default();
        let mut restart_interval = 0;
        let mut progressive = None;
        let mut pos = 2;
        loop {
            // Fill bytes may precede a marker.
            while data.get(pos) == Some(&0xFF) && data.get(pos + 1) == Some(&0xFF) {
                pos += 1;
            }
            let marker = match data.get(pos..pos + 2) {
                Some([0xFF, m]) => *m,
                _ => return Err(invalid("missing marker")),
            };
            pos += 2;
            if marker == EOI {
                jpeg.trailer = data[pos..].to_vec();
                break;
            }
            let length = match data.get(pos..pos + 2) {
                Some(&[a, b]) => usize::from(u16::from_be_bytes([a, b])),
                _ => return Err(invalid("truncated segment")),
            };
            let payload = data.get(pos + 2..pos + length).filter(|_| length >= 2).ok_or_else(|| invalid("truncated segment"))?;
            pos += length;
            match marker {
                0xE0..=0xEF | COM => jpeg.segments.push((marker, payload.to_vec())),
                DQT => read_tables(payload, &mut jpeg.tables)?,
                DHT => read_huffman(payload, &mut huffman)?,
                DRI => match payload {
                    [a, b, ..] => restart_interval = usize::from(u16::from_be_bytes([*a, *b])),
                    _ => return Err(invalid("bad restart interval")),
                },
                0xC0..=0xC2 => {
                    if progressive.is_some() {
                        return Err(invalid("more than one frame"));
                    }
                    progressive = Some(marker == 0xC2);
                    jpeg.read_frame(payload)?;
                }
                0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                    return Err(invalid("lossless, hierarchical and arithmetic-coded JPEGs aren't supported"));
                }
                SOS => {
                    let progressive = progressive.ok_or_else(|| invalid("scan before frame"))?;
                    let scan = Scan::read(payload, &jpeg.components, &huffman, progressive)?;
                    pos = jpeg.read_scan(&scan, data, pos, restart_interval)?;
                }
                // DNL, DHP, EXP, JPGn and reserved markers: nothing to keep.
                _ => {}
            }
        }
        if jpeg.components.is_empty() {
            return Err(invalid("no image"));
        }
        for c in &jpeg.components {
            if jpeg.tables[c.table].is_none() {
                return Err(invalid("missing quantization table"));
            }
        }
        Ok(jpeg)
    }

    fn read_frame(&mut self, p: &[u8]) -> io::Result<()> {
        if p.len() < 6 || p[0] != 8 {
            return Err(invalid("only 8-bit JPEGs are supported"));
        }
        self.height = usize::from(u16::from_be_bytes([p[1], p[2]]));
        self.width = usize::from(u16::from_be_bytes([p[3], p[4]]));
        let count = usize::from(p[5]);
        if self.width == 0 || self.height == 0 || count == 0 || p.len() < 6 + 3 * count {
            return Err(invalid("bad frame header"));
        }
        for c in p[6..6 + 3 * count].chunks(3) {
            let (h, v, table) = (usize::from(c[1] >> 4), usize::from(c[1] & 15), usize::from(c[2]));
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) || table > 3 {
                return Err(invalid("bad frame header"));
            }
            self.components.push(Component { id: c[0], h, v, table, across: 0, down: 0, blocks: Vec::new() });
        }
        let (max_h, max_v) = (self.max_h(), self.max_v());
        let (mcus_x, mcus_y) = (self.width.div_ceil(8 * max_h), self.height.div_ceil(8 * max_v));
        for c in &mut self.components {
            (c.across, c.down) = (mcus_x * c.h, mcus_y * c.v);
            c.blocks = vec![[0; 64]; c.across * c.down];
        }
        Ok(())
    }

    /// Decodes the entropy-coded data of `scan` starting at `pos`; returns where it ends.
    fn read_scan(&mut self, scan: &Scan, data: &[u8], pos: usize, restart_interval: usize) -> io::Result<usize> {
        let (max_h, max_v) = (self.max_h(), self.max_v());
        let mut bits = BitReader::new(data, pos);
        let mut predictions = [0i32; 4];
        let mut eob_run = 0u32;
        // A single-component scan isn't interleaved: its MCU is one block, and it covers only
        // the component's own blocks, not the padding to whole MCUs.
        let units = if let [only] = &scan.components[..] {
            let c = &self.components[only.index];
            let across = (self.width * c.h).div_ceil(max_h).div_ceil(8);
            let down = (self.height * c.v).div_ceil(max_v).div_ceil(8);
            (across, down)
        } else {
            (self.width.div_ceil(8 * max_h), self.height.div_ceil(8 * max_v))
        };
        for unit in 0..units.0 * units.1 {
            if restart_interval > 0 && unit > 0 && unit % restart_interval == 0 {
                bits.restart();
                predictions = [0; 4];
                eob_run = 0;
            }
            let (ux, uy) = (unit % units.0, unit / units.0);
            for (n, sc) in scan.components.iter().enumerate() {
                let c = &mut self.components[sc.index];
                let positions: Vec<(usize, usize)> = if scan.components.len() == 1 {
                    vec![(ux, uy)]
                } else {
                    (0..c.v).flat_map(|y| (0..c.h).map(move |x| (x, y))).map(|(x, y)| (ux * c.h + x, uy * c.v + y)).collect()
                };
                for (x, y) in positions {
                    let block = &mut c.blocks[y * c.across + x];
                    scan.decode_block(&mut bits, sc, block, &mut predictions[n], &mut eob_run)?;
                }
            }
        }
        Ok(bits.end())
    }

    /// Writes the image as a sequential JPEG with optimized Huffman tables.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0xFF, SOI];
        for (marker, payload) in &self.segments {
            write_segment(&mut out, *marker, payload);
        }

        let mut used = [false; 4];
        for c in &self.components {
            used[c.table] = true;
        }
        let mut extended = false;
        for (id, table) in self.tables.iter().enumerate() {
            let Some(table) = table.filter(|_| used[id]) else { continue };
            let wide = table.iter().any(|&q| q > 255);
            extended |= wide;
            let mut p = vec![(u8::from(wide) << 4) | id as u8];
            for &i in &ZIGZAG {
                if wide {
                    p.extend_from_slice(&table[i].to_be_bytes());
                } else {
                    p.push(table[i] as u8);
                }
            }
            write_segment(&mut out, DQT, &p);
        }

        let mut p = vec![8];
        p.extend_from_slice(&(self.height as u16).to_be_bytes());
        p.extend_from_slice(&(self.width as u16).to_be_bytes());
        p.push(self.components.len() as u8);
        for c in &self.components {
            p.extend_from_slice(&[c.id, (c.h << 4 | c.v) as u8, c.table as u8]);
        }
        write_segment(&mut out, if extended { 0xC1 } else { 0xC0 }, &p);

        // One scan with every component when an MCU may hold them all (10 blocks at most),
        // otherwise one scan each. Luma gets its own tables, the other components share.
        let blocks_per_mcu: usize = self.components.iter().map(|c| c.h * c.v).sum();
        let scans: Vec<Vec<usize>> = if self.components.len() > 1 && blocks_per_mcu <= 10 {
            vec![(0..self.components.len()).collect()]
        } else {
            (0..self.components.len()).map(|i| vec![i]).collect()
        };
        let table_of = |i: usize| usize::from(i > 0);

        let mut counts = [[0u32; 256]; 4];
        for scan in &scans {
            self.encode_scan(scan, &mut |i, ac, symbol, _, _| counts[table_of(i) * 2 + usize::from(ac)][usize::from(symbol)] += 1);
        }
        let tables: Vec<Option<HuffmanCode>> = counts.iter().map(|c| c.iter().any(|&n| n > 0).then(|| HuffmanCode::optimal(c))).collect();
        let mut p = Vec::new();
        for (k, table) in tables.iter().enumerate() {
            if let Some(table) = table {
                p.push((((k % 2) << 4) | (k / 2)) as u8);
                p.extend_from_slice(&table.counts);
                p.extend_from_slice(&table.values);
            }
        }
        write_segment(&mut out, DHT, &p);

        for scan in &scans {
            let mut p = vec![scan.len() as u8];
            for &i in scan {
                let t = table_of(i) as u8;
                p.extend_from_slice(&[self.components[i].id, t << 4 | t]);
            }
            p.extend_from_slice(&[0, 63, 0]);
            write_segment(&mut out, SOS, &p);
            let mut bits = BitWriter { out: &mut out, acc: 0, count: 0 };
            self.encode_scan(scan, &mut |i, ac, symbol, extra, extra_len| {
                let code = tables[table_of(i) * 2 + usize::from(ac)].as_ref().expect("counted");
                let (c, len) = code.codes[usize::from(symbol)];
                bits.put(u32::from(c), len);
                bits.put(extra, extra_len);
            });
            bits.flush();
        }
        out.extend_from_slice(&[0xFF, EOI]);
        out.extend_from_slice(&self.trailer);
        out
    }

    /// Calls `emit(component, is_ac, symbol, extra bits, their count)` for every Huffman
    /// symbol of a sequential scan over the `scan` components, in order.
    fn encode_scan(&self, scan: &[usize], emit: &mut impl FnMut(usize, bool, u8, u32, u8)) {
        let (max_h, max_v) = (self.max_h(), self.max_v());
        let mut predictions = [0i32; 4];
        let mut encode_block = |i: usize, block: &Block, prediction: &mut i32| {
            let dc = i32::from(block[0]);
            let (size, extra) = magnitude(dc - *prediction);
            *prediction = dc;
            emit(i, false, size, extra, size);
            let mut run = 0;
            for &k in &ZIGZAG[1..] {
                let value = i32::from(block[k]);
                if value == 0 {
                    run += 1;
                    continue;
                }
                while run > 15 {
                    emit(i, true, 0xF0, 0, 0);
                    run -= 16;
                }
                let (size, extra) = magnitude(value);
                emit(i, true, (run << 4) | size, extra, size);
                run = 0;
            }
            if run > 0 {
                emit(i, true, 0, 0, 0);
            }
        };
        if let [i] = *scan {
            let c = &self.components[i];
            let across = (self.width * c.h).div_ceil(max_h).div_ceil(8);
            let down = (self.height * c.v).div_ceil(max_v).div_ceil(8);
            for y in 0..down {
                for x in 0..across {
                    encode_block(i, &c.blocks[y * c.across + x], &mut predictions[0]);
                }
            }
            return;
        }
        for my in 0..self.height.div_ceil(8 * max_v) {
            for mx in 0..self.width.div_ceil(8 * max_h) {
                for (n, &i) in scan.iter().enumerate() {
                    let c = &self.components[i];
                    for y in my * c.v..(my + 1) * c.v {
                        for x in mx * c.h..(mx + 1) * c.h {
                            encode_block(i, &c.blocks[y * c.across + x], &mut predictions[n]);
                        }
                    }
                }
            }
        }
    }

    /// Every block's coefficients, component by component (for tests).
    #[cfg(test)]
    pub fn coefficients(&self) -> Vec<Vec<Block>> {
        self.components.iter().map(|c| c.blocks.clone()).collect()
    }
}

/// `block` with `t` applied: transposed, and odd frequencies negated along mirrored axes.
fn transform_block(block: &Block, t: Transform) -> Block {
    std::array::from_fn(|i| {
        let (u, v) = (i % 8, i / 8);
        let value = if t.swap { block[u * 8 + v] } else { block[i] };
        let negate = (t.flip_x && u % 2 == 1) != (t.flip_y && v % 2 == 1);
        if negate {
            -value
        } else {
            value
        }
    })
}

/// A value's size category and its extra bits, as JPEG codes coefficients.
fn magnitude(value: i32) -> (u8, u32) {
    let size = 32 - value.unsigned_abs().leading_zeros();
    let extra = (if value < 0 { value - 1 } else { value }) as u32 & ((1 << size) - 1);
    (size as u8, extra)
}

fn write_segment(out: &mut Vec<u8>, marker: u8, payload: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&(payload.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(payload);
}

fn read_tables(mut p: &[u8], tables: &mut [Option<[u16; 64]>; 4]) -> io::Result<()> {
    while let [pq_tq, rest @ ..] = p {
        let (wide, id) = (pq_tq >> 4 != 0, usize::from(pq_tq & 15));
        let size = if wide { 128 } else { 64 };
        if id > 3 || rest.len() < size {
            return Err(invalid("bad quantization table"));
        }
        let mut table = [0; 64];
        for (n, &i) in ZIGZAG.iter().enumerate() {
            table[i] = if wide { u16::from_be_bytes([rest[2 * n], rest[2 * n + 1]]) } else { u16::from(rest[n]) };
        }
        tables[id] = Some(table);
        p = &rest[size..];
    }
    Ok(())
}

fn read_huffman(mut p: &[u8], tables: &mut [[Option<Huffman>; 4]; 2]) -> io::Result<()> {
    while p.len() >= 17 {
        let (class, id) = (usize::from(p[0] >> 4), usize::from(p[0] & 15));
        let counts: [u8; 16] = p[1..17].try_into().expect("16 bytes");
        let total: usize = counts.iter().map(|&n| usize::from(n)).sum();
        let values = p.get(17..17 + total).ok_or_else(|| invalid("bad Huffman table"))?;
        if class > 1 || id > 3 {
            return Err(invalid("bad Huffman table"));
        }
        tables[class][id] = Some(Huffman::new(&counts, values)?);
        p = &p[17 + total..];
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Decoding.

/// A Huffman table for decoding: a lookup of the first 9 bits, then canonical code ranges.
struct Huffman {
    /// For each 9-bit prefix: (code length, value), length 0 when the code is longer.
    fast: Vec<(u8, u8)>,
    /// For each length, the largest code of that length (-1 for none).
    max_code: [i32; 17],
    /// For each length, its first code's index in `values` minus that code.
    offset: [i32; 17],
    values: Vec<u8>,
}

const FAST_BITS: u32 = 9;

impl Huffman {
    fn new(counts: &[u8; 16], values: &[u8]) -> io::Result<Self> {
        let mut table = Huffman { fast: vec![(0, 0); 1 << FAST_BITS], max_code: [-1; 17], offset: [0; 17], values: values.to_vec() };
        let (mut code, mut index) = (0i32, 0usize);
        for len in 1..=16 {
            let count = usize::from(counts[len - 1]);
            if code as usize + count > 1 << len {
                return Err(invalid("bad Huffman table"));
            }
            table.offset[len] = index as i32 - code;
            for _ in 0..count {
                if len <= FAST_BITS as usize {
                    let shift = FAST_BITS as usize - len;
                    for low in 0..1 << shift {
                        table.fast[(code as usize) << shift | low] = (len as u8, values[index]);
                    }
                }
                code += 1;
                index += 1;
            }
            if count > 0 {
                table.max_code[len] = code - 1;
            }
            code <<= 1;
        }
        Ok(table)
    }

    fn decode(&self, bits: &mut BitReader) -> io::Result<u8> {
        bits.fill();
        let (len, value) = self.fast[bits.peek(FAST_BITS) as usize];
        if len > 0 {
            bits.consume(u32::from(len));
            return Ok(value);
        }
        let code = bits.peek(16) as i32;
        for len in FAST_BITS as usize + 1..=16 {
            let c = code >> (16 - len);
            if c <= self.max_code[len] {
                bits.consume(len as u32);
                return self.values.get((self.offset[len] + c) as usize).copied().ok_or_else(|| invalid("bad Huffman code"));
            }
        }
        Err(invalid("bad Huffman code"))
    }
}

/// Reads entropy-coded bits, skipping stuffed zero bytes and stopping at markers (after
/// which it reads zeros, as decoders do with a truncated scan).
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// The next bits, from the top.
    acc: u64,
    count: u32,
    at_marker: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos, acc: 0, count: 0, at_marker: false }
    }

    fn fill(&mut self) {
        while self.count <= 56 {
            let mut byte = 0;
            if !self.at_marker && self.pos < self.data.len() {
                byte = self.data[self.pos];
                if byte != 0xFF {
                    self.pos += 1;
                } else if self.data.get(self.pos + 1) == Some(&0) {
                    self.pos += 2;
                } else {
                    (byte, self.at_marker) = (0, true);
                }
            }
            self.acc |= u64::from(byte) << (56 - self.count);
            self.count += 8;
        }
    }

    fn peek(&self, n: u32) -> u32 {
        (self.acc >> (64 - n)) as u32
    }

    fn consume(&mut self, n: u32) {
        self.acc <<= n;
        self.count -= n;
    }

    fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.fill();
        let value = self.peek(n);
        self.consume(n);
        value
    }

    /// `n` bits as a signed coefficient value (JPEG's EXTEND).
    fn signed(&mut self, n: u32) -> i32 {
        let value = self.bits(n) as i32;
        if n > 0 && value < 1 << (n - 1) {
            value - (1 << n) + 1
        } else {
            value
        }
    }

    /// Skips to just past the next restart marker and starts afresh.
    fn restart(&mut self) {
        (self.acc, self.count, self.at_marker) = (0, 0, false);
        while self.pos + 1 < self.data.len() {
            let (a, b) = (self.data[self.pos], self.data[self.pos + 1]);
            self.pos += 1;
            if a == 0xFF && (0xD0..=0xD7).contains(&b) {
                self.pos += 1;
                return;
            }
        }
    }

    /// Where the next marker after the scan's data is.
    fn end(&self) -> usize {
        let mut pos = self.pos;
        while pos + 1 < self.data.len() && !(self.data[pos] == 0xFF && !matches!(self.data[pos + 1], 0 | 0xD0..=0xD7)) {
            pos += 1;
        }
        pos
    }
}

struct ScanComponent {
    index: usize,
    dc: Option<usize>,
    ac: Option<usize>,
}

struct Scan<'h> {
    components: Vec<ScanComponent>,
    tables: &'h [[Option<Huffman>; 4]; 2],
    progressive: bool,
    /// Spectral selection and successive approximation (high, low).
    start: usize,
    end: usize,
    high: u32,
    low: u32,
}

impl<'h> Scan<'h> {
    fn read(p: &[u8], components: &[Component], tables: &'h [[Option<Huffman>; 4]; 2], progressive: bool) -> io::Result<Self> {
        let count = usize::from(*p.first().ok_or_else(|| invalid("bad scan header"))?);
        if count == 0 || count > 4 || p.len() < 4 + 2 * count {
            return Err(invalid("bad scan header"));
        }
        let mut scan_components = Vec::new();
        for c in p[1..1 + 2 * count].chunks(2) {
            let index = components.iter().position(|x| x.id == c[0]).ok_or_else(|| invalid("scan of an unknown component"))?;
            scan_components.push(ScanComponent { index, dc: Some(usize::from(c[1] >> 4)), ac: Some(usize::from(c[1] & 15)) });
        }
        let rest = &p[1 + 2 * count..];
        let (start, end, high, low) = (usize::from(rest[0]), usize::from(rest[1]), u32::from(rest[2] >> 4), u32::from(rest[2] & 15));
        let mut scan = Scan { components: scan_components, tables, progressive, start, end, high, low };
        if !progressive {
            (scan.start, scan.end, scan.high, scan.low) = (0, 63, 0, 0);
        } else if start > end || end > 63 || (start == 0 && end != 0) || (start > 0 && count != 1) || low > 13 {
            return Err(invalid("bad progressive scan"));
        }
        // Only the tables this scan uses must exist.
        for c in &mut scan.components {
            let dc_used = scan.start == 0 && scan.high == 0;
            let ac_used = scan.end > 0;
            c.dc = c.dc.filter(|_| dc_used);
            c.ac = c.ac.filter(|_| ac_used);
            for (class, id) in [(0, c.dc), (1, c.ac)] {
                if let Some(id) = id {
                    if id > 3 || tables[class][id].is_none() {
                        return Err(invalid("missing Huffman table"));
                    }
                }
            }
        }
        Ok(scan)
    }

    fn table(&self, class: usize, id: Option<usize>) -> &Huffman {
        self.tables[class][id.expect("checked in read")].as_ref().expect("checked in read")
    }

    fn decode_block(&self, bits: &mut BitReader, c: &ScanComponent, block: &mut Block, prediction: &mut i32, eob_run: &mut u32) -> io::Result<()> {
        let set = |block: &mut Block, k: usize, value: i32| -> io::Result<()> {
            block[ZIGZAG[k]] = i16::try_from(value).map_err(|_| invalid("coefficient out of range"))?;
            Ok(())
        };
        if !self.progressive {
            let size = self.table(0, c.dc).decode(bits)?;
            *prediction += bits.signed(u32::from(size));
            set(block, 0, *prediction)?;
            let ac = self.table(1, c.ac);
            let mut k = 1;
            while k < 64 {
                let rs = ac.decode(bits)?;
                let (run, size) = (usize::from(rs >> 4), u32::from(rs & 15));
                if size == 0 {
                    if run != 15 {
                        break;
                    }
                    k += 16;
                    continue;
                }
                k += run;
                if k > 63 {
                    return Err(invalid("bad AC run"));
                }
                set(block, k, bits.signed(size))?;
                k += 1;
            }
            return Ok(());
        }

        if self.start == 0 {
            if self.high == 0 {
                let size = self.table(0, c.dc).decode(bits)?;
                *prediction += bits.signed(u32::from(size));
                set(block, 0, *prediction << self.low)?;
            } else if bits.bits(1) == 1 {
                block[0] |= 1 << self.low;
            }
            return Ok(());
        }

        let ac = self.table(1, c.ac);
        if self.high == 0 {
            if *eob_run > 0 {
                *eob_run -= 1;
                return Ok(());
            }
            let mut k = self.start;
            while k <= self.end {
                let rs = ac.decode(bits)?;
                let (run, size) = (u32::from(rs >> 4), u32::from(rs & 15));
                if size == 0 {
                    if run < 15 {
                        *eob_run = (1 << run) - 1 + bits.bits(run);
                        break;
                    }
                    k += 16;
                    continue;
                }
                k += run as usize;
                if k > self.end {
                    return Err(invalid("bad AC run"));
                }
                set(block, k, bits.signed(size) << self.low)?;
                k += 1;
            }
            return Ok(());
        }

        // AC refinement: one more bit for coefficients already nonzero, new ±1 ones for zeros.
        let (plus, minus) = (1i16 << self.low, -1i16 << self.low);
        let refine = |bits: &mut BitReader, coefficient: &mut i16| {
            if bits.bits(1) == 1 && *coefficient & plus == 0 {
                *coefficient += if *coefficient >= 0 { plus } else { minus };
            }
        };
        let mut k = self.start;
        if *eob_run == 0 {
            while k <= self.end {
                let rs = ac.decode(bits)?;
                let (mut run, size) = (i32::from(rs >> 4), rs & 15);
                let mut value = 0;
                if size != 0 {
                    value = if bits.bits(1) == 1 { plus } else { minus };
                } else if run != 15 {
                    *eob_run = (1u32 << run) + bits.bits(run as u32);
                    break;
                }
                while k <= self.end {
                    let coefficient = &mut block[ZIGZAG[k]];
                    if *coefficient != 0 {
                        refine(bits, coefficient);
                    } else {
                        if run == 0 {
                            break;
                        }
                        run -= 1;
                    }
                    k += 1;
                }
                if value != 0 {
                    if k > self.end {
                        return Err(invalid("bad AC refinement"));
                    }
                    block[ZIGZAG[k]] = value;
                }
                k += 1;
            }
        }
        if *eob_run > 0 {
            while k <= self.end {
                let coefficient = &mut block[ZIGZAG[k]];
                if *coefficient != 0 {
                    refine(bits, coefficient);
                }
                k += 1;
            }
            *eob_run -= 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Encoding.

/// A Huffman table for encoding, with its DHT form.
struct HuffmanCode {
    counts: [u8; 16],
    values: Vec<u8>,
    /// (code, length) by symbol.
    codes: [(u16, u8); 256],
}

impl HuffmanCode {
    /// The optimal code for these symbol frequencies, limited to 16 bits and never all
    /// ones (JPEG's Annex K.2, as libjpeg builds it).
    fn optimal(frequencies: &[u32; 256]) -> Self {
        let mut freq = [0i64; 257];
        for (f, &n) in freq.iter_mut().zip(frequencies) {
            *f = i64::from(n);
        }
        // A reserved symbol keeps any code from being all ones.
        freq[256] = 1;
        let mut size = [0usize; 257];
        let mut others = [usize::MAX; 257];
        loop {
            // The two least frequent remaining (the higher symbol on ties).
            let least = |skip: Option<usize>| {
                let mut best: Option<usize> = None;
                for i in 0..257 {
                    if freq[i] > 0 && Some(i) != skip && best.is_none_or(|b| freq[i] <= freq[b]) {
                        best = Some(i);
                    }
                }
                best
            };
            let Some(mut c1) = least(None) else { break };
            let Some(mut c2) = least(Some(c1)) else { break };
            freq[c1] += freq[c2];
            freq[c2] = 0;
            size[c1] += 1;
            while others[c1] != usize::MAX {
                c1 = others[c1];
                size[c1] += 1;
            }
            others[c1] = c2;
            size[c2] += 1;
            while others[c2] != usize::MAX {
                c2 = others[c2];
                size[c2] += 1;
            }
        }
        let mut bits = [0usize; 33];
        for &s in &size {
            if s > 0 {
                bits[s.min(32)] += 1;
            }
        }
        for i in (17..=32).rev() {
            while bits[i] > 0 {
                let mut j = i - 2;
                while bits[j] == 0 {
                    j -= 1;
                }
                bits[i] -= 2;
                bits[i - 1] += 1;
                bits[j + 1] += 2;
                bits[j] -= 1;
            }
        }
        let mut longest = 16;
        while bits[longest] == 0 {
            longest -= 1;
        }
        bits[longest] -= 1;

        let mut values = Vec::new();
        for len in 1..=32 {
            values.extend((0..256).filter(|&s| size[s] == len).map(|s| s as u8));
        }
        let counts: [u8; 16] = std::array::from_fn(|i| bits[i + 1] as u8);
        values.truncate(counts.iter().map(|&n| usize::from(n)).sum());

        let mut codes = [(0, 0); 256];
        let (mut code, mut next) = (0u16, values.iter());
        for len in 1..=16u8 {
            for _ in 0..counts[usize::from(len) - 1] {
                codes[usize::from(*next.next().expect("counted"))] = (code, len);
                code += 1;
            }
            code <<= 1;
        }
        HuffmanCode { counts, values, codes }
    }
}

/// Writes bits with byte stuffing; [`BitWriter::flush`] pads the last byte with ones.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    count: u32,
}

impl BitWriter<'_> {
    fn put(&mut self, bits: u32, len: u8) {
        if len == 0 {
            return;
        }
        self.acc = self.acc << len | u64::from(bits) & ((1 << len) - 1);
        self.count += u32::from(len);
        while self.count >= 8 {
            self.count -= 8;
            let byte = (self.acc >> self.count) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0);
            }
        }
    }

    fn flush(&mut self) {
        if self.count > 0 {
            let pad = 8 - self.count as u8;
            self.put((1 << pad) - 1, pad);
        }
    }
}
