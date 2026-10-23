# ELM327 emulator scenarios for `obd_read.py`

`taos_2022.py` is an [ELM327-emulator](https://github.com/Ircama/ELM327-emulator)
4.0.0 scenario generated from the P0 survey of the owner's 2022 VW Taos
(real raw CAN frames for every supported Mode 01 PID, both ECUs `7E8`/`7E9`,
Mode 09 bitmaps, cal IDs, CVN, ECU names, empty DTC lists, Mode 02/06 bitmaps).
The VIN is the placeholder `WVGTESTVIN0000001`; the real VIN never leaves the
bench Pi. Unsupported PIDs answer `NO DATA` like the real adapter.

Scenarios in the module:

| name | purpose |
|---|---|
| `taos-2022` | the car at idle, ISO 15765-4 CAN 11/500 (`ATDPN` `A6`) |
| `taos-2022-badproto` | same car but `ATDPN` `A3`: the helper must refuse (exit 4) |
| `taos-2022-nodata2f` | PID `2F` advertised in the bitmap but answers `NO DATA`: polled once, then dropped |
| `taos-2022-crank` | start-stop: rpm 0 for four reads, the fourth PID 42 read is a 9.82 V crank dip, then 928 rpm at 14.40 V; the helper must not stop (either power source) |
| `taos-2022-ignoff` | ignition off on a CAN-only car: AT answers, `0100` is `SEARCHING...` / `UNABLE TO CONNECT`, every PID `NO DATA` (deterministic, unlike the built-in `engineoff`); the helper probes without tearing the link down |
| `taos-2022-lowbatt` | engine off, 11.50 V for eight PID 42 reads then 13.40 V; `constant` power stops after the sustained window and resumes from one re-check, `switched` never stops |

## Install (never system-wide)

```sh
python3 -m venv .venv-obd            # repo root; .venv-obd/ is gitignored
.venv-obd/bin/pip install "ELM327-emulator==4.0.0"
```

## Launch

The emulator only loads custom scenarios through its interpreter `merge`
command, which imports a `*.py` module from the current directory, so run it
from this directory with `PYTHONPATH=.`; `-s` on the command line runs before
`merge`, so the scenario is selected with a second interpreter command:

```sh
cd python/obd/emulator
PYTHONPATH=. ../../../.venv-obd/bin/elm -b /tmp/elm.out <<< $'merge taos_2022\nscenario taos-2022'
head -1 /tmp/elm.out        # first line = the pty, e.g. /dev/ttys003 or /dev/pts/4
```

The emulator keeps running after the batch commands end; stop it with
`pkill -f "elm -b /tmp/elm.out"`. Then, from the repo root:

```sh
python3 python/obd_read.py --serial "$(head -1 /tmp/elm.out)" --poll-hz 1 --audit-dir /tmp/obd-audit
```

`python/tests/emulator_harness.py [scenario] [seconds]` does all of the above
in one go and prints the helper's JSON lines. Fault injection at runtime: the
emulator's stdin accepts interpreter commands (`scenario engineoff` makes
every PID answer `NO DATA`, `scenario taos-2022` brings the car back).

## Regenerate from a survey

```sh
python3 build_scenario.py /path/to/survey.json --name taos-2022
```

`survey.json` is the output of the P0 `30_survey.py`; the generator ignores
its VIN and always writes the placeholder (`--vin` to choose another fake).
