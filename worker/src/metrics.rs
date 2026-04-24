pub const DATASET_BINDING: &str = "QUERIES";

pub fn rcode_of(msg: &[u8]) -> u8 {
    if msg.len() < 4 {
        return 0;
    }
    msg[3] & 0x0f
}

pub fn rcode_name(code: u8) -> &'static str {
    match code {
        0 => "NOERROR",
        1 => "FORMERR",
        2 => "SERVFAIL",
        3 => "NXDOMAIN",
        4 => "NOTIMP",
        5 => "REFUSED",
        _ => "OTHER",
    }
}

pub fn qtype_name(qtype: u16) -> &'static str {
    match qtype {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        6 => "SOA",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        33 => "SRV",
        35 => "NAPTR",
        43 => "DS",
        46 => "RRSIG",
        47 => "NSEC",
        48 => "DNSKEY",
        50 => "NSEC3",
        52 => "TLSA",
        64 => "SVCB",
        65 => "HTTPS",
        257 => "CAA",
        _ => "OTHER",
    }
}
