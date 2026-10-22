"""Field table (D4): name -> PIDs (preference order), unit, decoder per PID.

Every decoder returns a number (int/float) or raises ValueError/IndexError on
short data; the caller treats that as a miss. Nothing here is ever a string
or None, because telemetry leaves must be numeric.
"""
from collections import OrderedDict


def _b(h, i):
    """Byte i of a hex payload; short data raises ValueError (a miss, never a guess)."""
    if len(h) < 2 * i + 2:
        raise ValueError("payload too short for byte %d: %r" % (i, h))
    return int(h[2 * i:2 * i + 2], 16)


def _pct(h):
    return round(_b(h, 0) * 100 / 255, 2)


def _temp(h):
    return _b(h, 0) - 40


def _u16(h):
    return _b(h, 0) * 256 + _b(h, 1)


def _fuel_rate_9d(h):
    # SAE J1979 PID 9D: A,B engine fuel rate x0.02 g/s; -> L/h at 0.745 kg/L
    g_s = _u16(h) * 0.02
    return round(g_s * 3600 / 745.0, 3)


def _fuel_rate_5e(h):
    return round(_u16(h) / 20, 2)


def _intake_temp_68(h):
    # PID 68: A = sensor support bits, B = intake air temperature sensor 1 (-40)
    return _b(h, 1) - 40


def _gear_a4(h):
    # PID A4 (J1979-DA): A = support bits (bit 0: gear status in B, bit 1: ratio
    # in C,D x0.001), B bits 7-4 = actual gear (0 = park/neutral, 1..n). The 2022
    # Taos answers A=01 and only fills B (01 00 00 00 in Park, 01 30 00 00 in 3rd
    # at 32 km/h; C,D stay 0000 for the whole drive), so the ratio is never the
    # gear here. A ratio-only ECU (A bit 0 clear) is a miss, not a gear.
    if not _b(h, 0) & 0x01:
        raise ValueError("A4 gear status not supported: %r" % h)
    return _b(h, 1) >> 4


def _odometer_a6(h):
    if len(h) < 8:
        raise ValueError("odometer payload too short: %r" % h)
    return round(int(h[:8], 16) / 10, 1)


def _torque_pct_62(h):
    # PID 62 (J1979): actual engine - percent torque, A - 125 (-125..130 %);
    # negative while the engine is driven (engine braking). Taos idle: 84 -> 7.
    return _b(h, 0) - 125


def _reference_torque_63(h):
    # PID 63: engine reference torque, A*256 + B Nm. Taos: 01 18 -> 280.
    return _u16(h)


def _engine_torque(pct, reference):
    """Derived engine torque, Nm = pct / 100 * reference (0.1 Nm)."""
    return round(pct * reference / 100.0, 1)


def _mil(h):
    return 1 if _b(h, 0) & 0x80 else 0


def _dtc_count(h):
    return _b(h, 0) & 0x7F


class FieldSpec:
    __slots__ = ("name", "unit", "pids", "decoders", "source", "inputs", "derive")

    def __init__(self, name, unit, pids=(), decoders=None, source="pid", inputs=(), derive=None):
        self.name, self.unit, self.pids = name, unit, tuple(pids)
        self.decoders = decoders or {}
        self.source = source   # "pid" | "atrv" | "link" | "derived"
        # "derived": computed from other PID fields of the SAME poll cycle
        self.inputs, self.derive = tuple(inputs), derive


FIELDS = OrderedDict()
for _spec in (
    FieldSpec("rpm", "rpm", ["0C"], {"0C": lambda h: _u16(h) / 4}),
    FieldSpec("speed", "km/h", ["0D"], {"0D": lambda h: _b(h, 0)}),
    FieldSpec("coolant_temp", "C", ["05"], {"05": _temp}),
    FieldSpec("engine_load", "%", ["04"], {"04": _pct}),
    FieldSpec("throttle", "%", ["11"], {"11": _pct}),
    FieldSpec("pedal", "%", ["49"], {"49": _pct}),
    FieldSpec("intake_temp", "C", ["68", "0F"], {"68": _intake_temp_68, "0F": _temp}),
    FieldSpec("ambient_temp", "C", ["46"], {"46": _temp}),
    FieldSpec("intake_map", "kPa", ["0B"], {"0B": lambda h: _b(h, 0)}),
    FieldSpec("baro", "kPa", ["33"], {"33": lambda h: _b(h, 0)}),
    FieldSpec("fuel_level", "%", ["2F"], {"2F": _pct}),
    FieldSpec("fuel_rate", "L/h", ["9D", "5E"], {"9D": _fuel_rate_9d, "5E": _fuel_rate_5e}),
    FieldSpec("module_voltage", "V", ["42"], {"42": lambda h: round(_u16(h) / 1000, 3)}),
    FieldSpec("adapter_voltage", "V", source="atrv"),
    FieldSpec("run_time", "s", ["1F"], {"1F": _u16}),
    FieldSpec("odometer", "km", ["A6"], {"A6": _odometer_a6}),
    FieldSpec("gear", "", ["A4"], {"A4": _gear_a4}),
    FieldSpec("distance_since_clear", "km", ["31"], {"31": _u16}),
    FieldSpec("mil", "", ["01"], {"01": _mil}),
    FieldSpec("dtc_count", "", ["01"], {"01": _dtc_count}),
    FieldSpec("link", "", source="link"),
    # torque (POST_INTEGRATION 2026-10-16), after link like the cloud vocabulary
    FieldSpec("engine_torque_pct", "%", ["62"], {"62": _torque_pct_62}),
    FieldSpec("reference_torque", "Nm", ["63"], {"63": _reference_torque_63}),
    FieldSpec("engine_torque", "Nm", source="derived",
              inputs=("engine_torque_pct", "reference_torque"), derive=_engine_torque),
):
    FIELDS[_spec.name] = _spec

DEFAULT_FIELDS = tuple(FIELDS.keys())
POLL_FIELDS = tuple(n for n, s in FIELDS.items() if s.source == "pid")
DERIVED_FIELDS = tuple(n for n, s in FIELDS.items() if s.source == "derived")


def decode_field(name, pid, data_hex):
    """Decode `data_hex` (payload after 41 PP) for field `name` via `pid`."""
    fn = FIELDS[name].decoders[pid]
    value = fn(data_hex)
    if isinstance(value, float) and value.is_integer():
        value = int(value)
    return value


def _pid_for(spec, supported_pids):
    return next((p for p in spec.pids if p in supported_pids), None)


def plan_fields(requested, supported_pids):
    """Split requested field names into supported / unsupported and build the
    poll plan: OrderedDict pid -> [field names decoded from that PID].

    `supported_pids` is the set of PID strings ("0C") any ECU advertised.
    Unknown names are ignored by the caller (they are not fields). A derived
    field is supported when every input's PID is; its inputs are then polled
    even when not requested themselves (`apply_derived` strips them again).
    """
    supported, unsupported = [], []
    plan = OrderedDict()
    for name in requested:
        spec = FIELDS.get(name)
        if spec is None:
            continue
        if spec.source in ("atrv", "link"):
            supported.append(name)
            continue
        if spec.source == "derived":
            pids = [(i, _pid_for(FIELDS[i], supported_pids)) for i in spec.inputs]
            if any(p is None for _, p in pids):
                unsupported.append(name)
                continue
            supported.append(name)
            for inp, pid in pids:
                names = plan.setdefault(pid, [])
                if inp not in names:
                    names.append(inp)
            continue
        pid = _pid_for(spec, supported_pids)
        if pid is None:
            unsupported.append(name)
            continue
        supported.append(name)
        names = plan.setdefault(pid, [])
        if name not in names:
            names.append(name)
    return supported, unsupported, plan


def apply_derived(fields, requested):
    """Add every requested derived field whose inputs all decoded in THIS
    cycle's `fields`, then drop inputs polled only for a derivation (not
    requested). Mutates and returns `fields`. A missing input is a miss for
    the derived field (never a stale or guessed value)."""
    for name in requested:
        spec = FIELDS.get(name)
        if spec is None or spec.source != "derived":
            continue
        if all(i in fields for i in spec.inputs):
            try:
                value = spec.derive(*(fields[i] for i in spec.inputs))
            except (ValueError, ArithmeticError):
                continue
            if isinstance(value, float) and value.is_integer():
                value = int(value)
            fields[name] = value
    for name in list(fields):
        if name not in requested and FIELDS.get(name) is not None and FIELDS[name].source == "pid":
            del fields[name]
    return fields
