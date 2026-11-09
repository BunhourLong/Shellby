//! Minimal RGBA8 PNG writer: zlib stored (uncompressed) deflate blocks, CRC32 + Adler32. No crates.

fn crc32(chunks: &[&[u8]]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *t = c;
    }
    let mut c = !0u32;
    for b in chunks.iter().flat_map(|s| s.iter()) {
        c = table[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[ty, data]).to_be_bytes());
}

/// `rgba` is tightly packed, len w*h*4.
pub fn encode_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (w * h * 4) as usize);
    // Raw scanlines: filter byte 0 (None) + row.
    let mut raw = Vec::with_capacity((h * (w * 4 + 1)) as usize);
    for row in rgba.chunks_exact((w * 4) as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib: CMF/FLG (no compression, check bits valid), stored blocks of <= 65535 bytes, Adler32.
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        z.push(blocks.peek().is_none() as u8); // BFINAL, BTYPE=00
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut s) = (1u32, 0u32);
    for &b in &raw {
        a = (a + b as u32) % 65521;
        s = (s + a) % 65521;
    }
    z.extend_from_slice(&((s << 16) | a).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, adaptive filter, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
