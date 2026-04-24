pub const NEGATIVE_TTL: u32 = 60;

pub fn rewrite_id(msg: &mut [u8], id: u16) {
    if msg.len() >= 2 {
        msg[0..2].copy_from_slice(&id.to_be_bytes());
    }
}

pub fn read_id(msg: &[u8]) -> u16 {
    u16::from_be_bytes([msg[0], msg[1]])
}

pub fn parse_question(msg: &[u8]) -> Option<(String, u16)> {
    if msg.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    if qdcount == 0 {
        return None;
    }
    let mut pos = 12;
    let mut name = String::new();
    loop {
        if pos >= msg.len() {
            return None;
        }
        let len = msg[pos] as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xc0 != 0 {
            return None; // question names aren't compressed
        }
        pos += 1;
        if pos + len > msg.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        for &b in &msg[pos..pos + len] {
            name.push(b.to_ascii_lowercase() as char);
        }
        pos += len;
    }
    if pos + 4 > msg.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([msg[pos], msg[pos + 1]]);
    Some((name, qtype))
}

/// Walk the answer section, returning `(min_ttl, ttl_field_offsets)`. The offsets
/// point at the first byte of each 4-byte TTL field in `msg`, so callers can
/// rewrite them in place on cache hits (RFC 1035 §4.1.3 TTL decrement).
pub fn answer_ttl_info(msg: &[u8]) -> (u32, Vec<usize>) {
    if msg.len() < 12 {
        return (NEGATIVE_TTL, Vec::new());
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let ancount = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    if ancount == 0 {
        return (NEGATIVE_TTL, Vec::new());
    }
    let mut pos = 12;
    for _ in 0..qdcount {
        if !skip_name(msg, &mut pos) {
            return (NEGATIVE_TTL, Vec::new());
        }
        pos += 4;
    }
    let mut offsets = Vec::with_capacity(ancount);
    let mut min: Option<u32> = None;
    for _ in 0..ancount {
        if !skip_name(msg, &mut pos) {
            break;
        }
        if pos + 10 > msg.len() {
            break;
        }
        let ttl_off = pos + 4;
        let ttl = u32::from_be_bytes([msg[ttl_off], msg[ttl_off + 1], msg[ttl_off + 2], msg[ttl_off + 3]]);
        let rdlen = u16::from_be_bytes([msg[pos + 8], msg[pos + 9]]) as usize;
        offsets.push(ttl_off);
        pos += 10 + rdlen;
        min = Some(min.map_or(ttl, |m| m.min(ttl)));
    }
    (min.unwrap_or(NEGATIVE_TTL), offsets)
}

pub fn decrement_ttls(msg: &mut [u8], offsets: &[usize], elapsed_secs: u32) {
    for &off in offsets {
        if off + 4 > msg.len() {
            continue;
        }
        let ttl = u32::from_be_bytes([msg[off], msg[off + 1], msg[off + 2], msg[off + 3]]);
        let new = ttl.saturating_sub(elapsed_secs);
        msg[off..off + 4].copy_from_slice(&new.to_be_bytes());
    }
}

/// Combined cache-hit transform: decrement answer TTLs by `elapsed_secs` and
/// rewrite the transaction ID to `client_id`.
pub fn apply_cache_hit(msg: &mut [u8], client_id: u16, elapsed_secs: u32) {
    let (_, offsets) = answer_ttl_info(msg);
    decrement_ttls(msg, &offsets, elapsed_secs);
    rewrite_id(msg, client_id);
}

fn skip_name(msg: &[u8], pos: &mut usize) -> bool {
    loop {
        if *pos >= msg.len() {
            return false;
        }
        let b = msg[*pos];
        if b == 0 {
            *pos += 1;
            return true;
        }
        if b & 0xc0 == 0xc0 {
            *pos += 2; // pointer terminates the name
            return true;
        }
        let len = b as usize;
        *pos += 1 + len;
    }
}
