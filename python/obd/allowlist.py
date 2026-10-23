"""The product allowlist: the only command shapes that may ever reach the vehicle.

Any change here needs a one-line justification, a test vector and the owner's
sign-off recorded in the integration plan (section 7.7). The default answer to
"can we also send X" is no.

AT: reset, warm start, identity strings, echo/linefeed/space/header toggles,
adaptive timing, timeout, "auto" protocol only (SP0), describe protocol,
read voltage. OBD: Mode 01 PID, Mode 02 PID+frame, Mode 03/07/0A DTC lists,
Mode 06 MID, Mode 09 info type, optional single response-count hint digit.
Deliberately absent: Mode 04/08, every UDS service, ATSH/ATCRA/ATCF/ATCM/
ATCEA/ATFC*, ATSP1-C, ATTP, ATPP/ATSD/ATWM/ATBRD/ATBRT, ATLP, ATMA, raw CAN.
"""
import hashlib
import random
import re

AT_PATTERN = r"^AT(Z|WS|I|@1|E[01]|L[01]|S[01]|H[01]|AT[012]|ST[0-9A-F]{2}|SP0|DP|DPN|RV)$"
OBD_PATTERN = r"^((01|09)[0-9A-F]{2}|02[0-9A-F]{4}|06[0-9A-F]{2}|03|07|0A)[1-9]?$"

_AT = re.compile(AT_PATTERN)
_OBD = re.compile(OBD_PATTERN)

# Version stamp of the allowlist, reported in the "ready" line and the audit log.
ALLOWLIST_SHA256 = hashlib.sha256(
    (AT_PATTERN + "\n" + OBD_PATTERN + "\n").encode("ascii")).hexdigest()

_WS = re.compile(r"\s+")


def normalize(cmd):
    """Upper-case and strip every whitespace character (the ELM ignores spaces)."""
    return _WS.sub("", str(cmd).upper())


def is_allowed(cmd):
    c = normalize(cmd)
    return bool(_AT.match(c) or _OBD.match(c))


# ---------------------------------------------------------------- corpora
# Commands that must be refused. Every entry is a real command somebody could
# plausibly want; several are what python-OBD sends out of the box.
BANNED_CORPUS = (
    # write/clear services
    "04", "0400", "08", "0800", "04 ",
    # UDS services (ISO 14229)
    "10", "1001", "1003", "11", "1101", "14", "14FFFFFF", "22", "22F190",
    "27", "2701", "28", "2800", "2E", "2EF190", "2F", "2F1234", "31",
    "3101", "34", "35", "36", "37", "3E", "3E00", "3E80", "85", "8502",
    # UDS with hint digit, mixed case, spaces
    "10031", "22 F1 90", "2e f1 90",
    # header / filter / addressing / flow control
    "ATSH7E0", "AT SH 7E0", "ATSH 7DF", "ATCRA7E8", "ATCRA 7E8", "ATCF7E8",
    "ATCM7FF", "ATCEA", "ATCEA10", "ATFCSH7E0", "ATFCSD300000", "ATFCSM1",
    "ATAR", "ATAL", "ATNL", "ATCAF0", "ATCAF1", "ATCFC0", "ATR0", "ATV1",
    "ATD0", "ATD",
    # protocol forcing (only SP0 is allowed)
    "ATSP1", "ATSP2", "ATSP3", "ATSP4", "ATSP5", "ATSP6", "ATSP7", "ATSP8",
    "ATSP9", "ATSPA", "ATSPB", "ATSPC", "ATSPA6", "ATTP6", "ATTPA6", "ATTP0",
    "ATSP", "ATPC", "ATBI", "ATSI", "ATFI",
    # programmable parameters, storage, wake-up, baud rate
    "ATPP0CSV23", "ATPP0CON", "ATPPFFOFF", "ATPPS", "ATSD00", "ATWM", "ATWM7E0013E",
    "ATSW00", "ATBRD23", "ATBRT0F", "ATBD", "ATSS", "ATMT",
    # low power, monitoring, raw CAN
    "ATLP", "ATMA", "ATMR7E8", "ATMT7E0", "ATRTR", "ATRA", "7E0020C", "7DF 02 01 0C",
    "7E00201 0C", "ATCSM1", "ATCS", "ATJE", "ATJS", "ATKW", "ATKW0", "ATIB10",
    "ATIFR2", "ATIIA33", "ATSR", "ATSTOP",
    # malformed allowlisted shapes
    "", "AT", "ATZ1", "ATE2", "ATL2", "ATS2", "ATH2", "ATAT3", "ATST0", "ATST000",
    "ATSTGG", "ATDPX", "ATRV1", "AT@2", "AT@3", "ATI1", "ATWS1",
    "0", "01", "010", "010C0", "010C10", "010CG", "0100 0", "01 0C 0D", "010C0D",
    "0200", "02000", "020000 00", "06", "060", "0600001", "09", "0902 1 1",
    "030", "0A0", "070", "03 1 1", "0B", "0B00", "0C", "0D", "0E", "0F", "05", "0500",
    "ATZ\x00", "ATZ;ATLP", "ATZ\rATLP", "ATZ\nATSH7E0",
)

# Commands that must pass (every command the helper or the experiment sent).
ALLOWED_CORPUS = (
    "ATZ", "ATWS", "ATI", "AT@1", "ATE0", "ATE1", "ATL0", "ATL1", "ATS0", "ATS1",
    "ATH0", "ATH1", "ATAT0", "ATAT1", "ATAT2", "ATST0A", "ATSTFF", "ATSP0", "ATDP",
    "ATDPN", "ATRV", "at z", "ATE 0", "atrv", "at st 0a",
    "0100", "0101", "010C", "010C1", "010C2", "010D1", "0120", "0140", "0160",
    "0180", "01A0", "01C0", "01E0", "0142", "0168", "019D", "01A4", "01A6", "01A61",
    "020000", "020C00", "0200001", "03", "07", "0A", "031", "071", "0A1",
    "0600", "0601", "0620", "06011", "0900", "0902", "0904", "0906", "090A", "09021",
    "01 0C 1", "01 0c", "09 02",
)

_FORBIDDEN_OBD_SERVICES = {
    "04", "08", "10", "11", "14", "22", "27", "28", "2E", "2F", "31", "34", "35",
    "36", "37", "3E", "85",
}
_FORBIDDEN_AT_PREFIXES = ("SH", "CRA", "CF", "CM", "CEA", "FC", "TP", "PP", "SD",
                          "WM", "BRD", "BRT", "LP", "MA", "SW", "PC", "BI", "AL",
                          "CAF", "CFC", "R0", "R1", "AR", "MR", "MT", "RA", "RTR")
_RANDOM_ALPHABET = "0123456789ABCDEFabcdef ATZSHPLMWCRFDNI@ \r\n"


def _property_ok(c):
    """Structural property every allowed (normalized) command must satisfy."""
    if c.startswith("AT"):
        tail = c[2:]
        if any(tail.startswith(p) for p in _FORBIDDEN_AT_PREFIXES):
            return False
        if re.match(r"^SP[1-9A-C]", tail):
            return False
        return bool(_AT.match(c))
    if c[:2] in _FORBIDDEN_OBD_SERVICES:
        return False
    if c[:2] not in ("01", "02", "03", "06", "07", "09", "0A"):
        return False
    if len(c) > 7:
        return False
    return True


def self_test(random_count=2000, seed=0):
    """Return a list of failure descriptions; empty means the gate is sound.

    Run at startup before the socket is opened and as a unit test. Checks the
    banned corpus is refused, the allowed corpus passes, and that random
    strings never pass unless they satisfy the structural property.
    """
    failures = []
    for cmd in BANNED_CORPUS:
        if is_allowed(cmd):
            failures.append("banned command passed: %r" % cmd)
    for cmd in ALLOWED_CORPUS:
        if not is_allowed(cmd):
            failures.append("allowed command refused: %r" % cmd)
    rng = random.Random(seed)
    for _ in range(random_count):
        n = rng.randint(0, 9)
        s = "".join(rng.choice(_RANDOM_ALPHABET) for _ in range(n))
        if is_allowed(s) and not _property_ok(normalize(s)):
            failures.append("random string passed the allowlist: %r" % s)
    if is_allowed("04") or is_allowed("ATLP") or is_allowed("ATSH7E0"):
        failures.append("sanity: a known banned command passed")
    return failures
