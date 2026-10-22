#!/usr/bin/env python3
"""Generate an ELM327-emulator scenario module from a survey.json.

    build_scenario.py SURVEY.json --name taos-2022 --out taos_2022.py

The survey is the output of the P0 experiment (`30_survey.py`): raw CAN
frames per Mode 01 PID (ATH1/ATS0 format, one line per frame, several ECUs),
supported-PID lists per ECU, Mode 09 bitmaps, cal IDs, CVN, ECU names, DTC
raws, Mode 02/06 bitmaps. The VIN is NEVER taken from the survey; a
placeholder (default WVGTESTVIN0000001) is used so the scenario can be
committed. The generated module is self-contained (needs only the emulator).
"""
import argparse
import json
import re

PLACEHOLDER_VIN = "WVGTESTVIN0000001"
NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")


def split_frame(raw):
    s = raw.replace(" ", "").upper()
    hlen = 3 if len(s) % 2 == 1 else 8
    return s[:hlen], s[hlen:]


def frames_expr(raws):
    """Raw ATH1/ATS0 lines -> python expression building HD/SZ/DT tags."""
    parts = []
    for raw in raws:
        hdr, data = split_frame(raw)
        size, body = data[:2], data[2:]
        spaced = " ".join(body[i:i + 2] for i in range(0, len(body), 2))
        parts.append("HD(%r) + SZ(%r) + DT(%r)" % (hdr, size, spaced))
    return " + ".join(parts) if parts else "ST('NO DATA')"


def bitmap_bytes(pids, base):
    bits = 0
    for p in pids:
        if base < p <= base + 32:
            bits |= 1 << (31 - (p - base - 1))
    if any(p > base + 32 for p in pids):
        bits |= 1  # "next range supported" bit (PID base+0x20)
    return "%08X" % bits


def bitmap_frames(service_resp, by_ecu, base):
    raws = []
    for ecu, pids in by_ecu.items():
        if base and not any(p > base for p in pids):
            continue
        raws.append("%s06%s%02X%s" % (ecu, service_resp, base, bitmap_bytes(pids, base)))
    return raws


def isotp_payload_frames(ecu, payload_bytes):
    """Frame a payload (list of hex bytes) as ISO-TP single/multi frames."""
    n = len(payload_bytes)
    if n <= 7:
        return ["%s0%X%s" % (ecu, n, "".join(payload_bytes))]
    frames = ["%s1%03X%s" % (ecu, n, "".join(payload_bytes[:6]))]
    rest = payload_bytes[6:]
    seq = 1
    for i in range(0, len(rest), 7):
        chunk = rest[i:i + 7]
        chunk += ["AA"] * (7 - len(chunk))   # VW pads with AA, as in the survey
        frames.append("%s2%X%s" % (ecu, seq & 0xF, "".join(chunk)))
        seq += 1
    return frames


def ascii_bytes(text, width):
    b = text.encode("ascii", "replace")[:width].ljust(width, b"\x00")
    return ["%02X" % c for c in b]


def entry(key, request, descr, response_expr, priority=1, extra=""):
    return ("    %r: {\n        'Request': %r,\n        'Descr': %r,\n"
            "%s        'Response': %s,\n        'Priority': %d,\n    },\n"
            % (key, request, descr, extra, response_expr, priority))


def build(survey, name, vin):
    m01 = {ecu: sorted(int(p) for p in pids) for ecu, pids in survey["mode01_supported"].items()}
    m09 = {ecu: sorted(int(p) for p in pids) for ecu, pids in survey.get("mode09_supported", {}).items()}
    ecus = list(m01.keys())
    ecm = ecus[0]
    out = []
    out.append(entry("AT_RESET", "^ATZ$", "AT reset", "ST(%r)" % survey["identity"]["ATI"],
                     extra="        'Exec': 'self.reset(0.5)',\n"))
    out.append(entry("AT_I", "^ATI$", "AT version", "ST(%r)" % survey["identity"]["ATI"]))
    out.append(entry("AT_DESCR", "^AT@1$", "AT description", "ST(%r)" % survey["identity"]["AT@1"]))
    volts = survey.get("voltage_end") or 13.2
    out.append(entry("AT_R_VOLT", "^ATRV$", "AT read voltage", "ST(%r)" % ("%.1fV" % volts)))
    out.append(entry("AT_DESCRIBE_PROTO_N", "^ATDPN$", "AT protocol number",
                     "ST(%r)" % survey["protocol"]["ATDPN"]))
    out.append(entry("AT_DESCRIBE_PROTO", "^ATDP$", "AT describe protocol",
                     "ST(%r)" % survey["protocol"]["ATDP"]))
    # Mode 01 bitmaps (consistent with the supported lists), 0100 with SEARCHING
    for base in range(0x00, 0xE0 + 1, 0x20):
        raws = bitmap_frames("41", m01, base)
        if not raws:
            continue
        key = "ELM_PIDS_%s" % "ABCDEFGH"[base // 0x20]
        extra = ""
        if base == 0:
            extra = ("        'ResponseHeader': lambda self, cmd, pid, uc_val: "
                     "('<string>SEARCHING...</string><exec>time.sleep(1.5)</exec>' + ST(''))"
                     " if self.counters[pid] == 1 else '',\n")
        out.append(entry(key, "^01%02X" % base, "Supported PIDs [%02X-%02X]" % (base + 1, base + 0x20),
                         frames_expr(raws), extra=extra))
    # Mode 01 readings from the raw frames
    for pid, rec in sorted(survey["mode01_readings"].items()):
        if rec.get("no_data") or not rec.get("raw"):
            continue
        out.append(entry("PID_%s" % pid, "^01%s" % pid, rec.get("name", "pid_%s" % pid),
                         frames_expr(rec["raw"])))
    # Mode 09 bitmap, VIN (placeholder), cal IDs, CVN, ECU names
    out.append(entry("ELM_INFO_A", "^0900", "Mode 09 supported", frames_expr(bitmap_frames("49", m09, 0))))
    vin_frames = isotp_payload_frames(ecm, ["49", "02", "01"] + ["%02X" % c for c in vin.encode("ascii")])
    out.append(entry("VIN", "^0902", "VIN (placeholder, not the real vehicle)", frames_expr(vin_frames)))
    cal_raws, cvn_raws, name_raws = [], [], []
    for ecu, text in survey.get("cal_ids", {}).items():
        cal_raws += isotp_payload_frames(ecu, ["49", "04", "01"] + ascii_bytes(text, 16))
    for ecu, hexs in survey.get("cvn", {}).items():
        cvn_raws += isotp_payload_frames(ecu, ["49", "06", "01"] + [hexs[i:i + 2] for i in range(0, 8, 2)])
    for ecu, text in survey.get("ecu_name", {}).items():
        name_raws += isotp_payload_frames(ecu, ["49", "0A", "01"] + ascii_bytes(text.replace("\x00", " ").strip(), 20))
    if cal_raws:
        out.append(entry("CAL_IDS", "^0904", "Calibration IDs", frames_expr(cal_raws)))
    if cvn_raws:
        out.append(entry("CVN", "^0906", "Calibration verification numbers", frames_expr(cvn_raws)))
    if name_raws:
        out.append(entry("ECU_NAME", "^090A", "ECU names", frames_expr(name_raws)))
    # DTC modes, Mode 02 / 06 bitmaps
    for key, req, field in (("DTC_STORED", "^03", "dtc_stored_raw"), ("DTC_PENDING", "^07", "dtc_pending_raw"),
                            ("DTC_PERMANENT", "^0A", "dtc_permanent_raw")):
        if survey.get(field):
            out.append(entry(key, req, key.lower(), frames_expr(survey[field])))
    if survey.get("mode02_00_raw"):
        out.append(entry("FF_PIDS_A", "^020000", "Freeze frame supported PIDs", frames_expr(survey["mode02_00_raw"])))
    if survey.get("mode06_00_raw"):
        out.append(entry("ELM_MIDS_A", "^0600", "Mode 06 supported MIDs", frames_expr(survey["mode06_00_raw"])))
    out.append(entry("NO_DATA", "^0[1-9A][0-9A-F]*$", "anything else the car does not answer",
                     "ST('NO DATA')", priority=6))
    body = "".join(out)
    # every request regex gets the ELM footer (optional response-count hint digit)
    body = re.sub(r"'Request': '(\^0[0-9A-F]+)',", r"'Request': '\1' + ELM_FOOTER,", body)
    return body


HEADER = '''"""ELM327-emulator scenario %(name)r generated by build_scenario.py from a
vehicle survey (real raw frames; VIN replaced by the placeholder %(vin)s).

Launch (from this directory, see README.md):
    PYTHONPATH=. elm -b /tmp/elm.out <<< $'merge %(module)s\\nscenario %(name)s'
Variants for fault-injection tests: %(name)s-badproto (ATDPN A3, must be
refused with exit 4) and %(name)s-nodata2f (PID 2F advertised but answers
NO DATA: must be dropped after one request).
"""
import copy
import time  # noqa: F401  (used by the SEARCHING... exec)

from elm.obd_message import HD, SZ, DT, ST, ELM_FOOTER

_BASE = {
%(body)s}

_BADPROTO = copy.copy(_BASE)
_BADPROTO['AT_DESCRIBE_PROTO_N'] = {'Request': '^ATDPN$', 'Descr': 'AT protocol number',
                                    'Response': ST('A3'), 'Priority': 1}
_BADPROTO['AT_DESCRIBE_PROTO'] = {'Request': '^ATDP$', 'Descr': 'AT describe protocol',
                                  'Response': ST('AUTO, ISO 9141-2'), 'Priority': 1}

_NODATA2F = copy.copy(_BASE)
_NODATA2F['PID_2F'] = {'Request': '^012F' + ELM_FOOTER, 'Descr': 'fuel level: advertised, silent',
                       'Response': ST('NO DATA'), 'Priority': 1}

ObdMessage = {
    %(name)r: _BASE,
    '%(name)s-badproto': _BADPROTO,
    '%(name)s-nodata2f': _NODATA2F,
}
'''


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("survey")
    ap.add_argument("--name", default="taos-2022")
    ap.add_argument("--out", default=None)
    ap.add_argument("--vin", default=PLACEHOLDER_VIN, help="placeholder VIN (never the real one)")
    a = ap.parse_args()
    if not NAME_RE.match(a.name):
        raise SystemExit("scenario name must be lower-case [a-z0-9-]")
    if len(a.vin) != 17:
        raise SystemExit("VIN placeholder must be 17 characters")
    with open(a.survey) as f:
        survey = json.load(f)
    module = a.name.replace("-", "_")
    out = a.out or module + ".py"
    body = build(survey, a.name, a.vin)
    with open(out, "w") as f:
        f.write(HEADER % {"name": a.name, "vin": a.vin, "module": module, "body": body})
    print("wrote %s (%d scenario entries)" % (out, body.count("'Request'")))


if __name__ == "__main__":
    main()
