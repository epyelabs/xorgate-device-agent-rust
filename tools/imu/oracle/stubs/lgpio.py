"""Stub of `lgpio` 0.2.2 for the phase 7c WP2 oracle (plan WP2, `plan-notes.md` §5).

Blinka 9.1.0's `generic_linux/lgpio_pin.py` (the BCM2712 `Pin`) imports
`lgpio` and runs `CHIP = _get_gpiochip()` -> `lgpio.gpiochip_open(N)` at IMPORT
time, and sets `lgpio.exceptions = True` in the `Pin` class body. The bench has
lgpio 0.2.2 (pip 0.2.2.0, deb python3-lgpio 0.2.2-1~rpt1, `wp0.md`). The I2C path
never calls lgpio, so the stub only has to let `import board` succeed and to
record what the import did (D81: the gpiochip handle the helper holds).

It mirrors the names Blinka 9.1.0 uses (`gpiochip_open`, `gpio_claim_input`,
`gpio_claim_output`, `gpio_read`, `gpio_write`, `gpio_free`, `error`,
`error_text`, `exceptions`, the pull/mode flags); any other attribute raises
AttributeError, as the real module would for a missing name. Calls are kept in
`CALLS` (reported by the oracle shim as a `note`, never compared).
"""

exceptions = True
CALLS = []

SET_ACTIVE_LOW = 4
SET_OPEN_DRAIN = 8
SET_OPEN_SOURCE = 16
SET_PULL_UP = 32
SET_PULL_DOWN = 64
SET_PULL_NONE = 128


class error(Exception):  # noqa: N801 - the real class name
    pass


def error_text(errnum):
    return f"lgpio error {errnum}"


def gpiochip_open(gpiochip):
    CALLS.append(["gpiochip_open", gpiochip])
    return 0


def gpio_claim_input(handle, gpio, lFlags=0):  # noqa: N803 - the real signature
    CALLS.append(["gpio_claim_input", handle, gpio, lFlags])
    return 0


def gpio_claim_output(handle, gpio, level=0, lFlags=0):  # noqa: N803
    CALLS.append(["gpio_claim_output", handle, gpio, level, lFlags])
    return 0


def gpio_read(handle, gpio):
    CALLS.append(["gpio_read", handle, gpio])
    return 0


def gpio_write(handle, gpio, level):
    CALLS.append(["gpio_write", handle, gpio, level])
    return 0


def gpio_free(handle, gpio):
    CALLS.append(["gpio_free", handle, gpio])
    return 0
