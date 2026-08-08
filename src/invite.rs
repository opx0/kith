//! The Invite code — a time-bounded offer to join a Circle, printed as one line
//! of ASCII a Person can paste or read aloud.
//!
//! The trailing CRC-32 is a checksum, not a signature: it catches a mangled
//! paste and proves nothing about who made the code. The gate is a human
//! approving a knock on their own Device.

use crate::engine::{CircleId, DeviceId, InviteTicket};

/// The literal prefix, so a code from a future kith is detectable *before*
/// anything is decoded.
const PREFIX: &str = "KITH";

/// Format version. Single digit by construction — [`encode`] writes one
/// character and [`decode`] reads one.
const VERSION: u8 = 1;

/// Crockford base32: no `I`, `L`, `O` or `U`, so the decoder can undo the
/// substitutions a listener makes.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Hyphen every eight characters. Cosmetic, and stripped before decoding.
const GROUP: usize = 8;

/// Slack against clock skew between two Devices. The bound that matters is
/// checked on the Steward's own Device at approval time.
const CLOCK_SKEW_GRACE_SECS: i64 = 300;

/// Why a code was refused — four outcomes, because the CLI can say something
/// genuinely different about each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    /// Not an invite code at all, or damaged past recognition.
    #[error("that is not a kith invite code")]
    Malformed,
    /// It looks like a code and does not add up.
    #[error("the invite code arrived damaged — check nothing was cut off")]
    Checksum,
    /// A well-formed code whose moment has passed.
    #[error("this invite has expired")]
    Expired,
    /// A code from a kith that speaks a format this one does not.
    #[error("this invite code was made by a newer kith")]
    WrongVersion,
}

/// Render a ticket as the printed code.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 1 | `version` |
/// | 1 | 2 + N | Circle id, u16 big-endian length then UTF-8 |
/// | … | 2 + N | Steward's Device Identity, same shape |
/// | … | 8 | `expires_at`, unix seconds, u64 big-endian |
/// | … | 2 + N | address hint, optional |
/// | … | 4 | CRC-32 (IEEE) of everything above, big-endian |
///
/// Fields are length-prefixed rather than fixed-width because the ids are
/// opaque handles from below the engine seam.
pub fn encode(ticket: &InviteTicket) -> String {
    let mut payload = Vec::with_capacity(96);
    payload.push(VERSION);
    push_field(&mut payload, ticket.circle.0.as_bytes());
    push_field(&mut payload, ticket.steward_device.0.as_bytes());
    payload.extend_from_slice(&ticket.expires_at.to_be_bytes());
    if let Some(address) = &ticket.address {
        push_field(&mut payload, address.as_bytes());
    }

    let crc = crc32(&payload);
    payload.extend_from_slice(&crc.to_be_bytes());

    let body = base32_encode(&payload);
    let mut code = String::with_capacity(PREFIX.len() + 1 + body.len() + body.len() / GROUP + 1);
    code.push_str(PREFIX);
    code.push(char::from(b'0' + VERSION));
    for (i, c) in body.chars().enumerate() {
        if i % GROUP == 0 {
            code.push('-');
        }
        code.push(c);
    }
    code
}

/// Read a printed code back into a ticket, against the system clock.
///
/// Tolerant of lowercase, missing or extra hyphens, wrapped lines and the
/// substitutions a listener makes. Checked entirely locally.
pub fn decode(code: &str) -> Result<InviteTicket, InviteError> {
    decode_at(code, jiff::Timestamp::now().as_second())
}

/// [`decode`] against a supplied clock, in unix seconds.
///
/// Pass `0` to skip the expiry check and compare `expires_at` yourself — the
/// surface that reports how long ago an Invite lapsed needs the expiry, which
/// [`InviteError::Expired`] does not carry.
pub fn decode_at(code: &str, now_unix: i64) -> Result<InviteTicket, InviteError> {
    let normalised: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();

    let rest = normalised
        .strip_prefix(PREFIX)
        .ok_or(InviteError::Malformed)?;

    // The version sits outside the base32 body, so its confusables are undone by
    // hand here — "kith one" is typed back as `KITHL` often enough.
    let mut chars = rest.chars();
    let version = match chars.next().ok_or(InviteError::Malformed)? {
        'I' | 'L' => '1',
        'O' => '0',
        c => c,
    };
    let version = version.to_digit(10).ok_or(InviteError::Malformed)? as u8;
    if version != VERSION {
        return Err(InviteError::WrongVersion);
    }

    let body = chars.as_str();
    if body.is_empty() {
        return Err(InviteError::Malformed);
    }
    let bytes = base32_decode(body)?;

    // Check the CRC before reading a single field: a corrupted length prefix must
    // be reported as damage, never chased into the payload.
    if bytes.len() < 4 {
        return Err(InviteError::Malformed);
    }
    let (payload, tail) = bytes.split_at(bytes.len() - 4);
    let claimed = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
    if claimed != crc32(payload) {
        return Err(InviteError::Checksum);
    }

    let mut reader = Reader::new(payload);
    // The prefix sits outside the checksum, so the version inside it is the
    // authoritative one.
    if reader.u8()? != VERSION {
        return Err(InviteError::WrongVersion);
    }
    let circle = reader.field()?;
    let steward_device = reader.field()?;
    let expires_at = reader.u64()?;
    let address = if reader.done() { None } else { Some(reader.field()?) };
    if !reader.done() {
        return Err(InviteError::Malformed);
    }

    if now_unix > 0 {
        let expires = i64::try_from(expires_at).unwrap_or(i64::MAX);
        if expires.saturating_add(CLOCK_SKEW_GRACE_SECS) < now_unix {
            return Err(InviteError::Expired);
        }
    }

    Ok(InviteTicket {
        circle: CircleId(circle),
        steward_device: DeviceId(steward_device),
        expires_at,
        address,
    })
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    // Truncating past u16::MAX fails safe: the ticket then points at no Device
    // rather than at the wrong one.
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&bytes[..len]);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], InviteError> {
        let end = self.at.checked_add(n).ok_or(InviteError::Malformed)?;
        let slice = self.bytes.get(self.at..end).ok_or(InviteError::Malformed)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, InviteError> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, InviteError> {
        let b: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| InviteError::Malformed)?;
        Ok(u64::from_be_bytes(b))
    }

    fn field(&mut self) -> Result<String, InviteError> {
        let len = self.take(2)?;
        let len = u16::from_be_bytes([len[0], len[1]]) as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| InviteError::Malformed)
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((acc >> bits) & 0x1f) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize]));
    }
    out
}

fn base32_decode(s: &str) -> Result<Vec<u8>, InviteError> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        let v = symbol(c).ok_or(InviteError::Malformed)?;
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // The tail may hold at most four bits of zero padding. Being strict here is
    // what stops tail corruption slipping past the CRC, which never sees it.
    if bits >= 5 || (acc & ((1u32 << bits) - 1)) != 0 {
        return Err(InviteError::Malformed);
    }
    Ok(out)
}

/// One character to its five bits, undoing the substitutions a Person makes
/// reading a code aloud. `U` is absent from the alphabet and stays absent.
fn symbol(c: char) -> Option<u8> {
    match c {
        'O' => Some(0),
        'I' | 'L' => Some(1),
        _ => ALPHABET
            .iter()
            .position(|&a| char::from(a) == c)
            .map(|i| i as u8),
    }
}

/// Bitwise CRC-32, reflected polynomial `0xEDB88320` — the one every checksum
/// tool means by "CRC32".
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    const CIRCLE: &str = "kith-7QM4XKC2";
    /// Shaped like the seam's opaque Device handle, in an alphabet of its own
    /// that includes letters Crockford leaves out.
    const DEVICE: &str = "CVX4DU36QK7WN2ZPB5YHJFA3ETMRS6LXG4VC8DKQ9WNP2ZY7KQ4X";
    const EXPIRES: u64 = 1_786_000_000;
    /// An hour before the Invite lapses.
    const NOW: i64 = EXPIRES as i64 - 3600;

    fn ticket() -> InviteTicket {
        InviteTicket {
            address: None,
            circle: CircleId(CIRCLE.into()),
            steward_device: DeviceId(DEVICE.into()),
            expires_at: EXPIRES,
        }
    }

    #[test]
    fn an_address_hint_survives_the_round_trip() {
        let hinted = InviteTicket {
            address: Some("tcp://192.168.1.5:22000".into()),
            ..ticket()
        };
        let back = decode_at(&encode(&hinted), EXPIRES as i64 - 60).expect("hinted code decodes");
        assert_eq!(back.address.as_deref(), Some("tcp://192.168.1.5:22000"));
        assert_eq!(back.circle.0, CIRCLE);
        assert_eq!(back.steward_device.0, DEVICE);
    }

    #[test]
    fn a_code_without_a_hint_carries_none_and_stays_shorter() {
        let plain = encode(&ticket());
        let hinted = encode(&InviteTicket { address: Some("tcp://10.0.0.2:22000".into()), ..ticket() });
        assert!(plain.len() < hinted.len(), "the hint is what costs the extra length");
        let back = decode_at(&plain, EXPIRES as i64 - 60).expect("plain code decodes");
        assert_eq!(back.address, None);
    }

    /// Replace the `nth` symbol of the body with a different one.
    fn corrupt(code: &str, nth: usize) -> String {
        let (head, body) = code.split_once('-').expect("a grouped code");
        let mut out = String::from(head);
        let mut seen = 0;
        for c in body.chars() {
            if c == '-' {
                out.push('-');
                continue;
            }
            if seen == nth {
                let i = ALPHABET
                    .iter()
                    .position(|&a| char::from(a) == c)
                    .expect("encode emits canonical symbols only");
                out.push(char::from(ALPHABET[(i + 1) % 32]));
            } else {
                out.push(c);
            }
            seen += 1;
        }
        out
    }

    #[test]
    fn a_ticket_survives_the_round_trip() {
        let code = encode(&ticket());
        assert!(code.starts_with("KITH1-"), "{code}");

        let back = decode_at(&code, NOW).expect("round trip");
        assert_eq!(back.circle.0, CIRCLE);
        assert_eq!(back.steward_device.0, DEVICE);
        assert_eq!(back.expires_at, EXPIRES);
    }

    #[test]
    fn the_code_is_grouped_for_reading_aloud() {
        let code = encode(&ticket());
        let (_, body) = code.split_once('-').unwrap();
        for group in body.split('-').take(body.split('-').count() - 1) {
            assert_eq!(
                group.len(),
                GROUP,
                "every group but the last is {GROUP}: {code}"
            );
        }
    }

    #[test]
    fn a_code_read_aloud_and_typed_back_still_decodes() {
        // Lowercase, hyphens lost, a wrapped line, and O/I/L substituted —
        // including in the prefix, which is typed back as `kithl`.
        let code = encode(&ticket());
        let mangled: String = code
            .chars()
            .filter(|c| *c != '-')
            .map(|c| match c {
                '0' => 'o',
                '1' => 'l',
                c => c.to_ascii_lowercase(),
            })
            .collect();
        let mangled = format!("{}\n  {}", &mangled[..20], &mangled[20..]);
        assert!(mangled.starts_with("kithl"));

        let back = decode_at(&mangled, NOW).expect("confusables are undone");
        assert_eq!(back.circle.0, CIRCLE);
        assert_eq!(back.steward_device.0, DEVICE);
    }

    #[test]
    fn a_single_wrong_character_is_caught_by_the_crc() {
        let code = encode(&ticket());
        for nth in [0, 3, 17, 40] {
            let damaged = corrupt(&code, nth);
            assert_ne!(damaged, code);
            assert_eq!(
                decode_at(&damaged, NOW).unwrap_err(),
                InviteError::Checksum,
                "symbol {nth} of {code}"
            );
        }
    }

    #[test]
    fn a_truncated_paste_never_decodes_to_a_ticket() {
        let code = encode(&ticket());
        let cut = &code[..code.len() - 9];
        let err = decode_at(cut, NOW).unwrap_err();
        assert!(
            matches!(err, InviteError::Checksum | InviteError::Malformed),
            "{err:?}"
        );
    }

    #[test]
    fn something_that_is_not_an_invite_code_says_so() {
        for not_a_code in [
            "",
            "hello",
            "https://example.invalid/join",
            "KITH",
            "KITHX-ABCD",
        ] {
            assert_eq!(
                decode_at(not_a_code, NOW).unwrap_err(),
                InviteError::Malformed,
                "{not_a_code}"
            );
        }
    }

    #[test]
    fn a_code_from_a_newer_kith_is_refused_before_it_is_read() {
        let code = encode(&ticket());
        let future = format!("KITH2{}", &code[5..]);
        assert_eq!(
            decode_at(&future, NOW).unwrap_err(),
            InviteError::WrongVersion
        );
    }

    #[test]
    fn an_expired_invite_is_refused_without_contacting_anybody() {
        let code = encode(&ticket());
        assert_eq!(
            decode_at(&code, EXPIRES as i64 + 3600).unwrap_err(),
            InviteError::Expired
        );
    }

    #[test]
    fn expiry_allows_for_two_devices_disagreeing_about_the_time() {
        let code = encode(&ticket());
        // A minute past, on a Device whose clock runs fast: still good.
        assert!(decode_at(&code, EXPIRES as i64 + 60).is_ok());
        // An hour past is not clock skew.
        assert_eq!(
            decode_at(&code, EXPIRES as i64 + CLOCK_SKEW_GRACE_SECS + 1).unwrap_err(),
            InviteError::Expired
        );
    }

    #[test]
    fn crc32_matches_the_check_value_every_implementation_agrees_on() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn base32_round_trips_every_tail_length() {
        for n in 0..12usize {
            let bytes: Vec<u8> = (0..n)
                .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
                .collect();
            let encoded = base32_encode(&bytes);
            assert!(encoded.chars().all(|c| ALPHABET.contains(&(c as u8))));
            assert_eq!(base32_decode(&encoded).unwrap(), bytes, "{n} bytes");
        }
    }

    #[test]
    fn base32_refuses_a_body_that_could_not_have_been_encoded() {
        // One byte plus two bits of zero padding.
        assert_eq!(base32_decode("00"), Ok(vec![0x00]));
        // Fifteen bits: seven left over, which no encoding produces.
        assert_eq!(base32_decode("000"), Err(InviteError::Malformed));
        // Padding bits must be zero — the CRC never sees them.
        assert_eq!(base32_decode("01"), Err(InviteError::Malformed));
        // U is not in the alphabet and is not a confusable for anything.
        assert_eq!(base32_decode("0U"), Err(InviteError::Malformed));
    }
}
