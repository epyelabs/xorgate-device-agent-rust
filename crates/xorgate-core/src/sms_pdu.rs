//! SMS PDU decoding — 1:1 port of `src/hardware/sms-pdu.ts` (429 lines,
//! pure, no deps; device-sms-inbox plan §2.3).
//!
//! Input is what the SIM7600 returns for `AT+CMGRO=<i>` in PDU mode
//! (`AT+CMGF=0`): a `+CMGRO: <stat>,<alpha>,<length>` header, then one hex
//! line that starts with the SMSC address (`07915178…`) followed by the TPDU.
//! `<length>` is the TPDU length in octets, which is how we tell whether the
//! SMSC prefix is there at all.
//!
//! **Read-only contract** (`sms-modem.ts:11-16`, agent-anatomy invariant 4):
//! the sweep reads with `AT+CMGRO` (SIMCom "read only"), which leaves
//! `REC UNREAD` as is. `AT+CMGR` / `AT+CMGL` would flip messages to
//! `REC READ`, and text mode (`AT+CMGF=1`) would do the same on read, so
//! neither is ever sent; nothing is deleted or written to the SIM. This
//! module only decodes what that read-only sweep hands it — the AT session
//! itself is Phase 4's HAL.
//!
//! Only SMS-DELIVER is decoded. Stored SUBMIT PDUs (drafts / sent copies) and
//! status reports come back as [`PduDecodeResult::Skip`] and are counted,
//! never shown.
//!
//! Privacy (contract §6): nothing in here logs. Callers must not log `from`
//! or `text` either.
//!
//! JS string semantics: the TS keeps decoded text as a JS string, i.e. UTF-16
//! code units, and `assembleMessages` relies on that twice — a surrogate pair
//! split across two concatenated UCS2 parts rejoins when the part strings are
//! joined (`sms-pdu.ts:210-212`), and `SMS_TEXT_MAX` / the truncation rule
//! count code units (`:340-346`). [`DeliverPdu::text`] is therefore a
//! `Vec<u16>`; it becomes a Rust `String` only in the wire-facing
//! [`SmsMessage`] (lone surrogates → U+FFFD there, where `JSON.stringify`
//! would print a `\udXXX` escape — the one byte-level divergence, reachable
//! only for an incomplete concatenation that ends on half a pair).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// `SenderType` (`sms-pdu.ts:17`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderType {
    International,
    National,
    Alphanumeric,
    Other,
}

/// `ConcatInfo` (`sms-pdu.ts:19-25`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConcatInfo {
    /// Reference number (8-bit for IEI 0x00, 16-bit for IEI 0x08).
    #[serde(rename = "ref")]
    pub reference: u16,
    pub total: u8,
    /// 1-based sequence number of this part.
    pub seq: u8,
}

/// `DeliverPdu` (`sms-pdu.ts:27-37`) without its `kind` tag (the tag lives
/// on [`PduDecodeResult`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliverPdu {
    pub from: String,
    pub from_type: SenderType,
    /// SCTS as ISO 8601 with its own offset, or null when undecodable.
    pub sent_at: Option<String>,
    /// Decoded text as UTF-16 code units (see the module doc), or the
    /// lowercase hex of the body when `binary`.
    #[serde(serialize_with = "serialize_utf16_lossy")]
    pub text: Vec<u16>,
    pub binary: bool,
    pub concat: Option<ConcatInfo>,
}

impl DeliverPdu {
    /// The text as a Rust string (lone surrogates → U+FFFD).
    pub fn text_lossy(&self) -> String {
        String::from_utf16_lossy(&self.text)
    }
}

fn serialize_utf16_lossy<S: serde::Serializer>(units: &[u16], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&String::from_utf16_lossy(units))
}

/// The `reason` of a `skip` result (`sms-pdu.ts:41`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    #[serde(rename = "not-deliver")]
    NotDeliver,
}

/// `PduDecodeResult` (`sms-pdu.ts:39-42`). Serialises exactly like the TS
/// union: `{"kind":"deliver",…}`, `{"kind":"skip","reason":"not-deliver"}`,
/// `{"kind":"invalid","reason":"…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PduDecodeResult {
    Deliver(DeliverPdu),
    Skip { reason: SkipReason },
    Invalid { reason: String },
}

impl PduDecodeResult {
    fn invalid(reason: &str) -> Self {
        PduDecodeResult::Invalid {
            reason: reason.to_string(),
        }
    }

    pub fn as_deliver(&self) -> Option<&DeliverPdu> {
        match self {
            PduDecodeResult::Deliver(d) => Some(d),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// GSM 03.38 default alphabet + extension table (`sms-pdu.ts:49-71`)
// ---------------------------------------------------------------------------

/// `GSM7_BASIC_TABLE` (`sms-pdu.ts:49-58, 73`). 0x1B (ESC) is a placeholder
/// here; it is handled as the extension escape.
#[rustfmt::skip]
pub const GSM7_BASIC_TABLE: [char; 128] = [
    '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å',
    'Δ', '_', 'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1b}', 'Æ', 'æ', 'ß', 'É',
    ' ', '!', '"', '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/',
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?',
    '¡', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O',
    'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§',
    '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o',
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö', 'ñ', 'ü', 'à',
];

/// `GSM7_EXT_TABLE` (`sms-pdu.ts:60-71, 74`): extension code → char.
pub const GSM7_EXT_TABLE: [(u8, char); 10] = [
    (0x0a, '\u{c}'),
    (0x14, '^'),
    (0x28, '{'),
    (0x29, '}'),
    (0x2f, '\\'),
    (0x3c, '['),
    (0x3d, '~'),
    (0x3e, ']'),
    (0x40, '|'),
    (0x65, '€'),
];

fn gsm7_ext(code: u8) -> Option<char> {
    GSM7_EXT_TABLE
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, ch)| *ch)
}

/// Unpack `count` septets starting at bit 0 of `bytes` (`unpackSeptets`,
/// `sms-pdu.ts:77-89`). Stops early when the bytes run out.
pub fn unpack_septets(bytes: &[u8], count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bit = i * 7;
        let byte = bit >> 3;
        let shift = bit & 7;
        if byte >= bytes.len() {
            break;
        }
        let mut v = u32::from(bytes[byte]) >> shift;
        if shift > 1 && byte + 1 < bytes.len() {
            v |= u32::from(bytes[byte + 1]) << (8 - shift);
        }
        out.push((v & 0x7f) as u8);
    }
    out
}

/// Septet codes → text, honouring the 0x1B extension escape
/// (`gsm7ToString`, `sms-pdu.ts:92-107`). A dangling escape at the end is
/// dropped; an unknown extension code displays as the basic-table char
/// (23.038), and ESC ESC as a space.
pub fn gsm7_to_string(septets: &[u8]) -> String {
    let mut s = String::with_capacity(septets.len());
    let mut i = 0;
    while i < septets.len() {
        let c = septets[i];
        if c == 0x1b {
            let Some(&next) = septets.get(i + 1) else {
                break; // dangling escape at the end: drop it
            };
            i += 1;
            match gsm7_ext(next) {
                Some(ch) => s.push(ch),
                None if next == 0x1b => s.push(' '),
                None => {
                    if let Some(ch) = GSM7_BASIC_TABLE.get(usize::from(next)) {
                        s.push(*ch);
                    }
                }
            }
            i += 1;
            continue;
        }
        if let Some(ch) = GSM7_BASIC_TABLE.get(usize::from(c)) {
            s.push(*ch);
        }
        i += 1;
    }
    s
}

// ---------------------------------------------------------------------------
// Field helpers (`sms-pdu.ts:113-148`)
// ---------------------------------------------------------------------------

/// `hexToBytes` (`sms-pdu.ts:113-119`): trimmed, non-empty, even length,
/// `[0-9a-fA-F]` only — else `None`.
fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let h = hex.trim();
    if h.is_empty() || !h.bytes().all(|b| b.is_ascii_hexdigit()) || !h.len().is_multiple_of(2) {
        return None;
    }
    let bytes = h.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// `bytesToHex` (`sms-pdu.ts:121-125`): lowercase, two digits per byte.
fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Nibbles 0xA..0xE of a BCD address digit (`BCD_EXTRA`, `sms-pdu.ts:127`);
/// 0xF is the filler.
const BCD_EXTRA: [char; 5] = ['*', '#', 'a', 'b', 'c'];

/// Swapped-nibble BCD digits (address values). Stops at the 0xF filler
/// (`semiOctets`, `sms-pdu.ts:130-140`).
fn semi_octets(bytes: &[u8], digits: usize) -> String {
    let mut s = String::with_capacity(digits);
    for i in 0..digits {
        let Some(&b) = bytes.get(i >> 1) else {
            break;
        };
        let nib = if i % 2 == 0 { b & 0x0f } else { b >> 4 };
        if nib == 0x0f {
            break;
        }
        if nib < 10 {
            s.push(char::from(b'0' + nib));
        } else {
            s.push(BCD_EXTRA[usize::from(nib - 10)]);
        }
    }
    s
}

/// One SCTS field octet: low nibble is the tens digit. `None` on a non-BCD
/// nibble (`bcdField`, `sms-pdu.ts:143-148`, where it is `NaN`).
fn bcd_field(b: u8) -> Option<u32> {
    let lo = u32::from(b & 0x0f);
    let hi = u32::from(b >> 4);
    if lo > 9 || hi > 9 {
        return None;
    }
    Some(lo * 10 + hi)
}

/// SCTS (7 octets) → `YYYY-MM-DDThh:mm:ss±hh:mm` (`decodeScts`,
/// `sms-pdu.ts:156-170`). The zone is in quarter hours, with the sign in bit
/// 3 of the low (tens) nibble (23.040 §9.2.3.11). `None` for fewer than 7
/// octets, a non-BCD nibble, a zone units nibble > 9, a month/day/hour/
/// minute/second out of range or a zone beyond ±18 h. A day is checked
/// against 1..=31 only, like the TS (`2026-02-31` passes; see
/// [`parse_sent_at_ms`] for what `Date.parse` then makes of it).
pub fn decode_scts(b: &[u8]) -> Option<String> {
    if b.len() < 7 {
        return None;
    }
    let tz_byte = b[6];
    let tz_tens = u32::from(tz_byte & 0x07);
    let tz_units = u32::from(tz_byte >> 4);
    let negative = (tz_byte & 0x08) != 0;
    let fields: Option<Vec<u32>> = b[..6].iter().map(|&x| bcd_field(x)).collect();
    let fields = fields?;
    if tz_units > 9 {
        return None;
    }
    let (yy, mo, dd, hh, mi, ss) = (
        fields[0], fields[1], fields[2], fields[3], fields[4], fields[5],
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&dd) || hh > 23 || mi > 59 || ss > 59 {
        return None;
    }
    let quarters = tz_tens * 10 + tz_units;
    if quarters > 18 * 4 {
        return None;
    }
    let off_min = quarters * 15;
    let sign = if negative && off_min > 0 { '-' } else { '+' };
    Some(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        2000 + yy,
        mo,
        dd,
        hh,
        mi,
        ss,
        sign,
        off_min / 60,
        off_min % 60
    ))
}

/// `decodeAddress` (`sms-pdu.ts:172-187`).
fn decode_address(digits: u8, toa: u8, body: &[u8]) -> (String, SenderType) {
    let ton = (toa >> 4) & 0x07;
    if ton == 0x05 {
        // Alphanumeric: GSM-7 packed; `digits` counts the semi-octets used.
        let septets = (usize::from(digits) * 4) / 7;
        return (
            gsm7_to_string(&unpack_septets(body, septets)),
            SenderType::Alphanumeric,
        );
    }
    let num = semi_octets(body, usize::from(digits));
    match ton {
        0x01 => (format!("+{num}"), SenderType::International),
        0x02 => (num, SenderType::National),
        _ => (num, SenderType::Other),
    }
}

/// `Alphabet` (`sms-pdu.ts:189`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Alphabet {
    #[serde(rename = "gsm7")]
    Gsm7,
    #[serde(rename = "8bit")]
    Bit8,
    #[serde(rename = "ucs2")]
    Ucs2,
}

/// 23.038 §4 data coding scheme → alphabet (`dcsAlphabet`,
/// `sms-pdu.ts:192-205`). Compressed text is treated as 8-bit.
pub fn dcs_alphabet(dcs: u8) -> Alphabet {
    let group = dcs >> 4;
    if group <= 0x07 {
        // General data coding (00xx) and automatic-deletion (01xx) groups.
        if dcs & 0x20 != 0 {
            return Alphabet::Bit8; // compressed
        }
        return match (dcs >> 2) & 0x03 {
            0x01 => Alphabet::Bit8,
            0x02 => Alphabet::Ucs2,
            _ => Alphabet::Gsm7,
        };
    }
    if group == 0x0f {
        return if dcs & 0x04 != 0 {
            Alphabet::Bit8
        } else {
            Alphabet::Gsm7
        };
    }
    if group == 0x0e {
        return Alphabet::Ucs2; // MWI, store, UCS2
    }
    Alphabet::Gsm7 // 0xC/0xD MWI groups, and the reserved 0x8..0xB
}

/// `decodeUcs2` (`sms-pdu.ts:207-215`): big-endian code units, not code
/// points — a surrogate pair split across two concatenated parts rejoins
/// when the part strings are joined. A trailing odd byte is ignored.
fn decode_ucs2(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| (u16::from(p[0]) << 8) | u16::from(p[1]))
        .collect()
}

/// `parseUdh` (`sms-pdu.ts:217-233`): the LAST well-formed concatenation IE
/// wins; `total`/`seq` must be > 0.
fn parse_udh(udh: &[u8]) -> Option<ConcatInfo> {
    let mut concat = None;
    let mut i = 0;
    while i + 1 < udh.len() {
        let iei = udh[i];
        let len = usize::from(udh[i + 1]);
        let Some(data) = udh.get(i + 2..i + 2 + len) else {
            break;
        };
        if iei == 0x00 && len == 3 && data[1] > 0 && data[2] > 0 {
            concat = Some(ConcatInfo {
                reference: u16::from(data[0]),
                total: data[1],
                seq: data[2],
            });
        } else if iei == 0x08 && len == 4 && data[2] > 0 && data[3] > 0 {
            concat = Some(ConcatInfo {
                reference: (u16::from(data[0]) << 8) | u16::from(data[1]),
                total: data[2],
                seq: data[3],
            });
        }
        i += 2 + len;
    }
    concat
}

/// Decode one stored PDU (`decodePdu`, `sms-pdu.ts:243-299`).
///
/// `tpdu_length` is the `<length>` from the `+CMGRO`/`+CMGR` header (TPDU
/// octets, SMSC excluded). When the line is exactly that long there is no
/// SMSC prefix; otherwise the first octet is the SMSC length. Pass `None` to
/// assume the prefix is present (what the SIM7600 always sends).
///
/// Never panics: every short read is `{"kind":"invalid","reason":"truncated"}`
/// (the TS `"decoder threw"` fallback in `assembleMessages` is unreachable
/// here and kept only as [`DECODER_THREW`] for the record).
pub fn decode_pdu(hex: &str, tpdu_length: Option<usize>) -> PduDecodeResult {
    let Some(all) = hex_to_bytes(hex) else {
        return PduDecodeResult::invalid("not hex");
    };
    let mut p = 0usize;
    if tpdu_length.is_none_or(|l| all.len() != l) {
        p = 1 + usize::from(all[0]);
    }
    let b: &[u8] = all.get(p..).unwrap_or(&[]);
    let Some(&first) = b.first() else {
        return PduDecodeResult::invalid("truncated");
    };
    if first & 0x03 != 0x00 {
        return PduDecodeResult::Skip {
            reason: SkipReason::NotDeliver,
        };
    }
    let udhi = first & 0x40 != 0;

    let mut i = 1;
    let (Some(&oa_digits), Some(&toa)) = (b.get(i), b.get(i + 1)) else {
        return PduDecodeResult::invalid("truncated");
    };
    i += 2;
    let oa_octets = usize::from(oa_digits).div_ceil(2);
    let Some(oa_body) = b.get(i..i + oa_octets) else {
        return PduDecodeResult::invalid("truncated");
    };
    i += oa_octets;
    let (from, from_type) = decode_address(oa_digits, toa, oa_body);

    i += 1; // pid
    let Some(&dcs) = b.get(i) else {
        return PduDecodeResult::invalid("truncated");
    };
    i += 1;
    let Some(scts) = b.get(i..i + 7) else {
        return PduDecodeResult::invalid("truncated");
    };
    i += 7;
    let sent_at = decode_scts(scts);
    let Some(&udl) = b.get(i) else {
        return PduDecodeResult::invalid("truncated");
    };
    i += 1;
    let ud: &[u8] = b.get(i..).unwrap_or(&[]);

    let alphabet = dcs_alphabet(dcs);
    let mut concat = None;
    let mut header_octets = 0usize;
    if udhi && !ud.is_empty() {
        header_octets = 1 + usize::from(ud[0]);
        if header_octets > ud.len() {
            return PduDecodeResult::invalid("bad UDH");
        }
        concat = parse_udh(&ud[1..header_octets]);
    }

    let udl = usize::from(udl);
    let (text, binary) = match alphabet {
        Alphabet::Gsm7 => {
            // UDL counts septets, header included; the header is padded to a
            // septet boundary, so skip ceil(headerBits / 7) septets.
            let skip = (header_octets * 8).div_ceil(7);
            let septets = unpack_septets(ud, udl);
            let septets = septets.get(skip..).unwrap_or(&[]);
            (gsm7_to_string(septets).encode_utf16().collect(), false)
        }
        Alphabet::Ucs2 | Alphabet::Bit8 => {
            // 8-bit and UCS2: UDL counts octets, header included.
            let end = udl.min(ud.len());
            let start = header_octets.min(end);
            let body = &ud[start..end];
            match alphabet {
                Alphabet::Ucs2 => (decode_ucs2(body), false),
                _ => (bytes_to_hex(body).encode_utf16().collect(), true),
            }
        }
    };
    PduDecodeResult::Deliver(DeliverPdu {
        from,
        from_type,
        sent_at,
        text,
        binary,
        concat,
    })
}

/// The `reason` the TS `assembleMessages` substitutes when `decodePdu`
/// throws (`sms-pdu.ts:397`). The Rust decoder cannot throw; kept so the
/// string is defined once.
pub const DECODER_THREW: &str = "decoder threw";

// ---------------------------------------------------------------------------
// Assembly: stored PDUs → contract messages (`sms-pdu.ts:305-429`)
// ---------------------------------------------------------------------------

/// `SmsStorage` (`sms-pdu.ts:305`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SmsStorage {
    SM,
    ME,
}

impl SmsStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            SmsStorage::SM => "SM",
            SmsStorage::ME => "ME",
        }
    }

    /// `"SM"` / `"ME"` → store; anything else (`"SR"`, …) is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "SM" => Some(SmsStorage::SM),
            "ME" => Some(SmsStorage::ME),
            _ => None,
        }
    }
}

/// One `AT+CMGRO` answer, as read off the modem (`StoredPdu`,
/// `sms-pdu.ts:308-316`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPdu {
    pub storage: SmsStorage,
    pub index: i64,
    /// `<stat>` from the header: 0 = REC UNREAD, 1 = REC READ (2/3 = stored MO).
    pub stat: i64,
    /// TPDU length from the header, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
    pub pdu: String,
}

/// Contract §2 message shape (`SmsMessage`, `sms-pdu.ts:319-331`). Reaches
/// the wire inside the `…/sms` payload; key order is the interface order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsMessage {
    pub id: String,
    pub storage: SmsStorage,
    pub indexes: Vec<i64>,
    pub from: String,
    pub from_type: SenderType,
    /// Always present on the wire (`null` when undecodable).
    pub sent_at: Option<String>,
    pub unread: bool,
    pub text: String,
    pub binary: bool,
    pub incomplete: bool,
    pub truncated: bool,
}

/// `SMS_TEXT_MAX` (`sms-pdu.ts:333`): in UTF-16 code units.
pub const SMS_TEXT_MAX: usize = 4000;

struct Part<'a> {
    rec: &'a StoredPdu,
    pdu: DeliverPdu,
}

/// `truncateText` (`sms-pdu.ts:340-346`) over code units: cut at
/// `SMS_TEXT_MAX`, one unit earlier when that would split a surrogate pair.
fn truncate_text(units: &[u16]) -> (String, bool) {
    if units.len() <= SMS_TEXT_MAX {
        return (String::from_utf16_lossy(units), false);
    }
    let mut cut = SMS_TEXT_MAX;
    let c = units[cut - 1];
    if (0xd800..=0xdbff).contains(&c) {
        cut -= 1; // never split a surrogate pair
    }
    (String::from_utf16_lossy(&units[..cut]), true)
}

// --- JS Date arithmetic (proleptic Gregorian, no leap seconds) --------------

/// Days since 1970-01-01 for a civil date (month 1..=12). The day is not
/// range-checked: `day` beyond the month's length rolls into the next month,
/// exactly as V8's `MakeDay` does for `Date.parse("2026-02-31…")`
/// (→ 2026-03-03). Howard Hinnant's `days_from_civil`.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01 (Hinnant's `civil_from_days`).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn two_digits(s: &[u8]) -> Option<u32> {
    if s.len() == 2 && s[0].is_ascii_digit() && s[1].is_ascii_digit() {
        Some(u32::from(s[0] - b'0') * 10 + u32::from(s[1] - b'0'))
    } else {
        None
    }
}

/// `Date.parse` of a `sentAt` string, i.e. of the one shape [`decode_scts`]
/// produces (`YYYY-MM-DDThh:mm:ss±hh:mm`; `Z` is accepted too). Epoch ms, or
/// `None` where `Date.parse` is `NaN` (`sentAtMs`, `sms-pdu.ts:348-352`). A
/// day past the month's end rolls over like V8 (`2026-02-31` → 2026-03-03).
pub fn parse_sent_at_ms(sent_at: &str) -> Option<i64> {
    let b = sent_at.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    if !b[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: i64 = sent_at[..4].parse().ok()?;
    let month = two_digits(&b[5..7])?;
    let day = two_digits(&b[8..10])?;
    let hour = two_digits(&b[11..13])?;
    let minute = two_digits(&b[14..16])?;
    let second = two_digits(&b[17..19])?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 24
        || minute > 59
        || second > 59
    {
        return None;
    }
    let zone = &b[19..];
    let offset_min: i64 = match zone {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest[2] == b':' => {
            let zh = two_digits(&rest[..2])?;
            let zm = two_digits(&rest[3..])?;
            if zh > 23 || zm > 59 {
                return None;
            }
            let m = i64::from(zh) * 60 + i64::from(zm);
            if *sign == b'-' {
                -m
            } else {
                m
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let local_ms = days * 86_400_000
        + i64::from(hour) * 3_600_000
        + i64::from(minute) * 60_000
        + i64::from(second) * 1000;
    Some(local_ms - offset_min * 60_000)
}

/// `new Date(ms).toISOString().slice(0, 19)` — the UTC civil time of an
/// epoch-ms instant without the `.sssZ` tail (used by the sim/replay SMS
/// fixture, `sms-modem.ts:621-624`). Negative instants floor like JS.
pub fn iso_seconds_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// `buildMessage` (`sms-pdu.ts:354-372`).
fn build_message(parts: &[Part<'_>], total: Option<u8>) -> SmsMessage {
    let mut sorted: Vec<&Part<'_>> = parts.iter().collect();
    sorted.sort_by_key(|p| p.pdu.concat.map_or(0, |c| c.seq));
    let head = sorted[0];
    let joined: Vec<u16> = sorted
        .iter()
        .flat_map(|p| p.pdu.text.iter().copied())
        .collect();
    let (text, truncated) = truncate_text(&joined);
    SmsMessage {
        id: sorted
            .iter()
            .map(|p| format!("{}:{}", p.rec.storage.as_str(), p.rec.index))
            .collect::<Vec<_>>()
            .join("+"),
        storage: head.rec.storage,
        indexes: sorted.iter().map(|p| p.rec.index).collect(),
        from: head.pdu.from.clone(),
        from_type: head.pdu.from_type,
        sent_at: head.pdu.sent_at.clone(),
        unread: sorted.iter().any(|p| p.rec.stat == 0),
        text,
        binary: sorted.iter().any(|p| p.pdu.binary),
        incomplete: total.is_some_and(|t| sorted.len() < usize::from(t)),
        truncated,
    }
}

/// What [`assemble_messages`] returns (`sms-pdu.ts:382-385`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembledMessages {
    pub messages: Vec<SmsMessage>,
    pub skipped: usize,
}

/// Decode, join concatenated parts, sort newest first (`assembleMessages`,
/// `sms-pdu.ts:382-429`).
///
/// Parts group by `(from, ref, total)`. An 8-bit reference wraps, so the same
/// key can hold two different messages: a part goes to the first group of
/// its key that does not have its sequence number yet (parts are visited in
/// storage/index order — `"ME"` before `"SM"` by string compare — which is
/// arrival order on this modem).
///
/// Sort (`:423-427`): newest `sentAt` instant first; undecodable timestamps
/// last; ties (including two undecodable ones) by `id` descending. Both
/// sorts are stable, like V8's.
pub fn assemble_messages(records: &[StoredPdu]) -> AssembledMessages {
    let mut skipped = 0usize;
    let mut singles: Vec<Vec<Part<'_>>> = Vec::new();
    let mut groups: IndexMap<String, Vec<Vec<Part<'_>>>> = IndexMap::new();
    let mut ordered: Vec<&StoredPdu> = records.iter().collect();
    ordered.sort_by(|a, b| {
        if a.storage == b.storage {
            a.index.cmp(&b.index)
        } else {
            a.storage.as_str().cmp(b.storage.as_str())
        }
    });
    for rec in ordered {
        let pdu = match decode_pdu(&rec.pdu, rec.length) {
            PduDecodeResult::Deliver(d) => d,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let part = Part { rec, pdu };
        let Some(c) = part.pdu.concat.filter(|c| c.total > 1) else {
            singles.push(vec![part]);
            continue;
        };
        let key = format!("{}\0{}\0{}", part.pdu.from, c.reference, c.total);
        let list = groups.entry(key).or_default();
        let home = list
            .iter()
            .position(|g| !g.iter().any(|p| p.pdu.concat.map(|x| x.seq) == Some(c.seq)));
        match home {
            Some(h) => list[h].push(part),
            None => list.push(vec![part]),
        }
    }
    let mut messages: Vec<SmsMessage> = singles.iter().map(|g| build_message(g, None)).collect();
    for list in groups.values() {
        for g in list {
            let total = g[0].pdu.concat.map(|c| c.total);
            messages.push(build_message(g, total));
        }
    }
    messages.sort_by(|a, b| {
        let ta = a.sent_at.as_deref().and_then(parse_sent_at_ms);
        let tb = b.sent_at.as_deref().and_then(parse_sent_at_ms);
        // JS: d = sentAtMs(b) - sentAtMs(a); if (d !== 0 && !isNaN(d)) return d;
        // with -Infinity for undecodable, so (−∞ − x) < 0 puts `a` first,
        // (x − −∞) > 0 puts `b` first and (−∞ − −∞) = NaN falls to the ids.
        match (tb, ta) {
            (Some(tb), Some(ta)) if tb != ta => tb.cmp(&ta),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            _ => b.id.cmp(&a.id),
        }
    });
    AssembledMessages { messages, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_helpers() {
        assert_eq!(hex_to_bytes(" 00fF \n"), Some(vec![0x00, 0xff]));
        assert_eq!(hex_to_bytes(""), None);
        assert_eq!(hex_to_bytes("0"), None);
        assert_eq!(hex_to_bytes("zz"), None);
        assert_eq!(hex_to_bytes("ü0"), None);
        assert_eq!(bytes_to_hex(&[0x00, 0xff, 0x10, 0xab]), "00ff10ab");
    }

    #[test]
    fn semi_octets_stop_at_filler() {
        assert_eq!(
            semi_octets(&[0x51, 0x55, 0x10, 0x00, 0x00, 0xf1], 11),
            "15550100001"
        );
        assert_eq!(semi_octets(&[0x21, 0x43, 0xf5], 6), "12345");
        assert_eq!(semi_octets(&[0xba, 0xdc], 4), "*#ab");
        assert_eq!(semi_octets(&[0x21], 5), "12");
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(iso_seconds_utc(0), "1970-01-01T00:00:00");
        assert_eq!(iso_seconds_utc(-1), "1969-12-31T23:59:59");
        assert_eq!(iso_seconds_utc(1_790_812_800_000), "2026-10-01T00:00:00");
        assert_eq!(parse_sent_at_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_sent_at_ms("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_sent_at_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_sent_at_ms("garbage"), None);
        assert_eq!(parse_sent_at_ms("2026-13-01T00:00:00+00:00"), None);
    }

    #[test]
    fn gsm7_escape_edge_cases() {
        assert_eq!(gsm7_to_string(&[0x1b]), "");
        assert_eq!(gsm7_to_string(&[0x1b, 0x1b]), " ");
        assert_eq!(gsm7_to_string(&[0x1b, 0x65]), "€");
        assert_eq!(gsm7_to_string(&[0x1b, 0x10]), "Δ");
        assert_eq!(gsm7_to_string(&[0x48, 0x69]), "Hi");
    }

    #[test]
    fn truncate_never_splits_a_pair() {
        let mut units: Vec<u16> = vec![b'a' as u16; SMS_TEXT_MAX - 1];
        units.extend("👍".encode_utf16());
        let (text, truncated) = truncate_text(&units);
        assert!(truncated);
        assert_eq!(text.len(), SMS_TEXT_MAX - 1);
        let (t2, tr2) = truncate_text(&units[..SMS_TEXT_MAX - 1]);
        assert!(!tr2);
        assert_eq!(t2.len(), SMS_TEXT_MAX - 1);
    }

    /// ACCEPTED DIVERGENCE (review-wp5 #1, Phase 2 tolerance table): an
    /// incomplete UCS2 concatenation (part 1 of 2 only) whose body is
    /// `a` + the high half of U+1F44D (`0061 D83D`). The TS keeps code units
    /// (`decodeUcs2`, `sms-pdu.ts:207-215`; `truncateText`, `:340-346`, does
    /// not touch a short string), so `JSON.stringify` emits the 6-byte escape
    /// `"text":"a\ud83d"` (reproduced under tsx). Rust emits valid UTF-8
    /// `"text":"a\u{FFFD}"` (`61 EF BF BD`). Kept on purpose: an unpaired
    /// `\ud83d` escape is rejected by Postgres `jsonb`, so the TS bytes are
    /// the bug, not the contract.
    #[test]
    fn lone_surrogate_in_incomplete_concat_becomes_replacement_char() {
        // SMSC len 00 | DELIVER+UDHI 44 | OA 11 digits intl 0B91 5155100000F1
        // | PID 00 | DCS 08 (UCS2) | SCTS | UDL 0A | UDH 0500032A0201 (ref
        // 0x2A, 2 parts, seq 1) | body 0061 D83D.
        let pdu = "00440B915155100000F1000862900110000000\
                   0A0500032A02010061D83D"
            .to_string();
        let rec = StoredPdu {
            storage: SmsStorage::SM,
            index: 3,
            stat: 0,
            length: None,
            pdu,
        };
        let out = assemble_messages(std::slice::from_ref(&rec));
        assert_eq!(out.skipped, 0);
        let m = &out.messages[0];
        assert!(m.incomplete);
        assert!(!m.truncated);
        assert_eq!(m.text, "a\u{FFFD}");
        let json = serde_json::to_string(m).unwrap();
        assert!(json.contains("\"text\":\"a\u{FFFD}\""), "{json}");
        assert!(!json.contains("\\ud83d"), "{json}");
    }
}
