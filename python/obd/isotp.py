"""CAN line parsing (ATH1 + ATS0 format) and ISO-TP (ISO 15765-2) reassembly."""
import re

_HEX = re.compile(r"^[0-9A-F]+$")


def can_frames(lines):
    """Split adapter lines into (header, data_hex). Non-frame lines are skipped.

    With headers on and spaces off, an 11-bit frame is ``7E8`` + data (odd
    total length) and a 29-bit frame is ``18DAF110`` + data (even length);
    the data part is always whole bytes, which is what disambiguates them.
    """
    out = []
    for ln in lines:
        s = ln.replace(" ", "").upper()
        if len(s) < 5 or not _HEX.match(s):
            continue
        hlen = 3 if len(s) % 2 == 1 else 8
        if len(s) <= hlen:
            continue
        out.append((s[:hlen], s[hlen:]))
    return out


def isotp_reassemble(lines):
    """Return {header: payload_hex} with multi-frame messages joined per ECU."""
    per = {}
    expected = {}
    next_seq = {}
    for hdr, data in can_frames(lines):
        if not data:
            continue
        pci = int(data[0], 16)
        if pci == 0:                      # single frame 0L
            n = int(data[1], 16)
            per[hdr] = data[2:2 + 2 * n]
            expected.pop(hdr, None)
        elif pci == 1:                    # first frame 1LLL
            if len(data) < 4:
                continue
            expected[hdr] = int(data[1:4], 16)
            per[hdr] = data[4:]
            next_seq[hdr] = 1
        elif pci == 2:                    # consecutive frame 2N
            seq = int(data[1], 16)
            if hdr in next_seq and seq != next_seq[hdr]:
                # out-of-order or repeated frame: keep what we have, mark short
                continue
            per[hdr] = per.get(hdr, "") + data[2:]
            next_seq[hdr] = (seq + 1) & 0xF
        else:                             # flow control or unknown: keep raw
            per[hdr] = per.get(hdr, "") + data
    for hdr, n in expected.items():
        per[hdr] = per[hdr][:2 * n]
    return per


def is_no_data(lines):
    j = "".join(lines).upper().replace(" ", "")
    return (not lines) or "NODATA" in j or "UNABLETOCONNECT" in j


def supported_bitmap(payload_hex, base_pid):
    """``41 00 BE 3F A8 13`` style payload (hex, no spaces) -> set of PIDs."""
    if len(payload_hex) < 12:
        return set()
    bits = int(payload_hex[4:12], 16)
    return {base_pid + 1 + i for i in range(32) if bits & (1 << (31 - i))}


def decode_dtcs(payload_hex):
    """43/47/4A NN then 2-byte codes (CAN format) -> ["P0420", ...]."""
    if len(payload_hex) < 4:
        return []
    codes = []
    body = payload_hex[4:]
    for i in range(0, len(body) - 3, 4):
        raw = int(body[i:i + 4], 16)
        if raw == 0:
            continue
        kind = "PCBU"[raw >> 14]
        codes.append("%s%d%X%02X" % (kind, (raw >> 12) & 3, (raw >> 8) & 0xF, raw & 0xFF))
    return codes


def find_payload(per, prefix):
    """First (header, payload) whose payload starts with `prefix` (e.g. '410C')."""
    prefix = prefix.upper()
    for hdr, payload in per.items():
        if payload.upper().startswith(prefix):
            return hdr, payload
    return None, None
