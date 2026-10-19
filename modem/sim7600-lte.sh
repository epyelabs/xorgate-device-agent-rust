#!/bin/bash
# =============================================================================
# sim7600-lte.sh — Resilient LTE connection manager for SIM7600G-H-M.2
# =============================================================================
# Designed for IoT deployments (e.g. xorgate edge device, fleet tracking).
#
# Features:
#   - Waits for stable signal (AT+CSQ) before connecting
#   - Configures + enables GNSS so NMEA streams on the dedicated GPS port:
#       * all constellations (AT+CGNSSMODE), DPO off for max sensitivity
#       * XTRA predicted-ephemeris assistance (AT+CGPSXE/AT+CGPSXD) with
#         periodic re-download — the big TTFF win in urban canyons
#       * SUPL UE-based AGPS (AT+CGPSURL + AT+CGPS=1,2), auto-falls back to
#         standalone when the server is unreachable
#   - Re-asserts the GNSS engine while connected (survives modem resets) and
#     self-heals a wedged engine: on + fixless too long -> clean engine
#     restart, capped per hour (never a modem reset), and suppressed while the
#     agent reports a no-RF sky (a garage is not a wedge — see SKY_STATE_FILE)
#   - Adaptive CSQ polling: faster when signal is changing rapidly
#   - Signal-aware reconnect: distinguishes dead zone vs network failure
#   - Registration-aware: good RSSI alone never triggers a connect attempt;
#     the modem must also be attached to the network (AT+CEREG)
#   - Escalating recovery ladder for failures a plain retry can't fix,
#     ordered by blast radius (GNSS-free tools first): PS re-attach
#     (AT+CGATT/AT+COPS) -> PDP bounce (AT+CGACT) -> radio cycle (AT+CFUN,
#     GNSS-safe) -> full modem reset (AT+CRESET — GNSS-destructive, so it is
#     gated: cooldown + earned radio cycles + GNSS-fix policy + daily budget)
#   - Wedge detection in dead-zone wait: no service for too long nudges the
#     radio on an exponential backoff (firmware can stop cell-searching after
#     a prolonged outage); a reset only after repeated radio cycles provably
#     didn't help, and only through the same gate
#   - Atomic JSON telemetry output for consumption by other processes
#   - Automatic reconnect on link failure
#
# NOTE (RNDIS mode, USB PID 9011): the LTE data call is dialed by the modem
# firmware itself; usb0's DHCP lease comes from the modem's INTERNAL NAT
# (192.168.225.1), so a successful dhclient bind proves nothing about the WAN.
# Connectivity must be verified end-to-end (ping) and recovery must happen at
# the modem level — AT+NETOPEN/NETCLOSE only drive the internal socket stack.
# usb0 must NOT be managed by NetworkManager (see 99-sim7600-unmanaged.conf).
#
# The xorgate-device-agent reads two things this daemon produces:
#   - LTE signal: the telemetry JSON at TELEMETRY_FILE (lte collector)
#   - GPS: NMEA on /dev/ttyUSB1, enabled here via AT+CGPS (gps collector)
# This daemon OWNS the AT port; the agent never opens it.
# =============================================================================

# --- Configuration -----------------------------------------------------------
SERIAL="${SIM7600_SERIAL:-/dev/ttyUSB2}"
IFACE="${SIM7600_IFACE:-usb0}"
# USB composition the whole stack assumes: 9011 = RNDIS ($IFACE + 5 tty ports).
# A factory-fresh SIM7600 ships at 9001 (QMI/RMNET — the kernel binds
# qmi_wwan/wwan0 and usb0 can NEVER appear), so every dial loops on
# "Interface usb0 never appeared". Stored in modem NV; corrected once at
# startup by ensure_usb_composition.
USB_PID_WANT="${SIM7600_USB_PID:-9011}"
PING_HOST="${SIM7600_PING_HOST:-8.8.8.8}"
# Second opinion for the end-to-end WAN verdict: "down" requires BOTH hosts to
# fail, so a one-target anomaly (anycast outage, route filter) can't walk the
# recovery ladder toward a modem reset for nothing. Empty disables it.
# NOTE for fault injection: blackholing PING_HOST alone no longer fails the
# health check — override BOTH hosts (e.g. 192.0.2.1 / 192.0.2.2).
PING_HOST2="${SIM7600_PING_HOST2:-1.1.1.1}"

# Signal thresholds (AT+CSQ returns RSSI 0–31; 99 = unknown)
# 10 = ~-93dBm (marginal), 15 = ~-83dBm (good)
MIN_CSQ="${SIM7600_MIN_CSQ:-10}"
GOOD_CSQ="${SIM7600_GOOD_CSQ:-15}"

# Timing (seconds)
SIGNAL_STABLE_COUNT=3        # consecutive good readings before connecting
HEALTH_CHECK_INTERVAL=15     # how often to ping while connected
HEALTH_FAIL_THRESHOLD=3      # consecutive ping failures before reconnect
RECONNECT_DELAY=5            # base wait before attempting reconnect (backs off)
AT_CMD_DELAY=1               # pause between AT commands
MODEM_BOOT_WAIT=8            # initial wait for modem USB enumeration
# Config-read retries (at_setting_confirmed): AT reads of persisted settings
# intermittently return nothing at daemon start even on a healthy modem (the
# F7 flake). A lost read must never be mistaken for config drift.
AT_SETTING_ATTEMPTS=3        # read attempts before giving up ("unknown")
AT_SETTING_RETRY_DELAY=1     # pause between attempts
# For ~10 s after a modem (re)boot, GNSS config queries answer well-formed
# boot-DEFAULT values before NV state is loaded (measured 3/3 reboots:
# +CGPSXE reads 0 until ~11 s post-enumeration, then the true 1) — landing
# exactly where the daemon's startup reads fall. Apparent drift must
# therefore hold across a re-read this far apart before it may be repaired.
DRIFT_CONFIRM_DELAY=15       # secs between the two reads that confirm drift
# GNSS engine start sequencing (F4, verified on-device 2026-07-25): the
# SUPL-assisted start AT+CGPS=1,2 is rejected ("explicit mode not accepted")
# when issued too soon after AT+CGPS=0 or a modem reboot; the same command is
# accepted after a ~4-5 s settle. Sequencing bug, not a firmware limitation.
# ROOT CAUSE, isolated on the bench 2026-07-30 (Phase 7C): the rejection is not
# about elapsed time at all — the module rejects AT+CGPS=<mode> whenever the
# engine is still RUNNING, including the very mode requested (AT+CGPS=1,2 ->
# ERROR while +CGPS? reads 1,2; AT+CGPS=1 -> ERROR too). The settle "worked"
# only because AT+CGPS=0 needs those seconds to take effect. At cold boot the
# module restores GNSS from NV by itself, so the engine is already up when the
# daemon starts, and a single lost AT+CGPS? read is enough to mistake that for
# "engine off" and issue a doomed start. See gps_stop (bounded wait for the
# engine to be really off) and gps_start (confirm the state before claiming a
# standalone fallback).
GNSS_START_SETTLE_SECS=5     # min settle before an explicit-mode engine start
# Bounded wait for the engine to actually stop after AT+CGPS=0 (Phase 7C). The
# module answers OK immediately and keeps running for several more seconds; any
# AT+CGPS=<mode> inside that window is rejected.
GNSS_STOP_SETTLE_MAX_SECS=20
# Post-reset quiet period (F10/F11): for ~10 s after re-enumeration the modem
# answers config queries with well-formed boot DEFAULTS (NV not yet loaded)
# and auto-restarts the GNSS engine itself as plain "1,1". Post-reset recovery
# must sit out this window before trusting or touching GNSS state.
GNSS_RESET_SETTLE_SECS=15    # wait after modem reboot before GNSS recovery acts

# Recovery ladder / wedge detection (seconds unless noted)
# After consecutive failed connect cycles the ladder escalates in blast-radius
# order (F1/F2: re-attach, PDP bounce and radio cycle are GNSS-free; F3: only
# AT+CRESET destroys GNSS acquisition):
#   fail 2: L1 re-attach, fail 3: L1 + PDP bounce, fails 4-5: L2 radio cycle,
#   fail 6: L3 modem reset (gated, below) — then the cycle repeats from L2
#   (this deep, re-attaches have proven useless).
RECONNECT_DELAY_MAX=60       # cap for backed-off reconnect delay
# Dead-zone nudge pacing comes from the lteRecovery.deadZoneProfile policy
# preset in settings.json (see deadzone_profile_params for the bundles). These
# env knobs, when set, override the active profile's raw values — dev tuning
# only, never the user surface (R8: presets are policy, numbers are not).
DEADZONE_NUDGE_BASE_OVERRIDE="${SIM7600_DEADZONE_NUDGE_SECS:-}"        # first nudge after this long without service
DEADZONE_NUDGE_CAP_OVERRIDE="${SIM7600_DEADZONE_NUDGE_CAP_SECS:-}"     # backoff ceiling
DEADZONE_CYCLES_OVERRIDE="${SIM7600_DEADZONE_CYCLES_BEFORE_RESET:-}"   # failed radio cycles before a reset may be tried; -1 = never
# Global CRESET gate (R3): beyond the cooldown, a reset must be *earned* — at
# least this many radio cycles must have actually fired since the last
# verified-healthy connection. The GNSS-free tools go first, always.
CRESET_MIN_RADIO_CYCLES="${SIM7600_CRESET_MIN_RADIO_CYCLES:-2}"
# GNSS-fix gate (policy lteRecovery.gpsFixResetPolicy, default "defer"): with
# a live fix, a reset is allowed only once LTE has been unusable this long —
# position continuity outranks upload immediacy for a dashcam; uploads buffer.
CRESET_FIX_DEFER_SECS="${SIM7600_CRESET_FIX_DEFER_SECS:-1800}"
# Rolling reset budget: each CRESET is a modem boot + full re-attach — the
# worst ~2A TX burst profile the modem has (field-crash power theory says be
# stingy). A modem that needs a 7th daily reset has a problem resets don't
# fix. Window state is in-memory: a daemon restart forgets earlier spend
# (accepted — restarts are rare and the cooldown still bounds the burst rate).
CRESET_BUDGET_MAX="${SIM7600_CRESET_BUDGET_MAX:-6}"
CRESET_BUDGET_WINDOW_SECS="${SIM7600_CRESET_BUDGET_WINDOW_SECS:-86400}"
# Power-aware cooldowns: a radio cycle and especially a modem reset force a
# full re-registration, during which the SIM7600 pulls TX bursts up to ~2A.
# On a marginal vehicle supply, clustering those events risks browning out the
# host. These floors bound how often each rung may fire; on cooldown the rung
# is skipped (logged) and the cheaper retries continue.
RADIO_CYCLE_COOLDOWN="${SIM7600_RADIO_CYCLE_COOLDOWN:-300}"   # min secs between AT+CFUN cycles
MODEM_RESET_COOLDOWN="${SIM7600_MODEM_RESET_COOLDOWN:-1800}"  # min secs between AT+CRESET

# Default-route metric for the LTE uplink. Must stay ABOVE the WiFi metric
# (50, see 10-uplink-priority.conf) so WiFi wins while associated; dhclient
# alone would install metric 0 and silently steal the whole uplink.
LTE_ROUTE_METRIC="${SIM7600_ROUTE_METRIC:-100}"

# Adaptive CSQ polling intervals (seconds)
CSQ_INTERVAL_FAST=3          # signal changing rapidly (delta >= CSQ_RAPID_DELTA)
CSQ_INTERVAL_WEAK=5          # signal weak / recovering (CSQ < MIN_CSQ)
CSQ_INTERVAL_STABLE=30       # signal stable and connected
CSQ_RAPID_DELTA=3            # CSQ change threshold to trigger fast polling

# Modem temperature poll (AT+CPMUTEMP) — temperature moves slowly, so this
# rides its own fixed interval instead of the adaptive CSQ cadence.
MODEM_TEMP_INTERVAL="${SIM7600_MODEM_TEMP_INTERVAL:-30}"

# SIM presence poll (AT+CPIN?) — one command per interval, no more. The card is
# physically fixed while the device is powered, so the IDENTITY fields behind it
# (ICCID, MSISDN) are read once and cached; only presence is re-checked, and
# only this slowly. See maybe_update_sim.
#
# Why 10 minutes and not faster: the block's only consumer is the agent's config
# REPORT, which reaches the cloud on connect, on a config change, and otherwise
# on an HOURLY heartbeat (DEFAULT_HEARTBEAT_MS). Nothing reads it faster — the
# monitor TUI does not display SIM state. A 60 s poll therefore produced ~60
# samples an hour that nobody ever saw. This is still 6x finer than the
# consumer, and the cases that actually matter do not wait for the timer: the
# first probe fires immediately at start-up, and a modem reset invalidates the
# cache and re-arms this throttle (gnss_recover_after_reset).
#
# Bench note: a SIM-pull test waits up to this long for the state to flip.
# Override with SIM7600_SIM_CHECK_INTERVAL=30 in a systemd drop-in to speed it
# up, and remove the drop-in afterwards.
SIM_CHECK_INTERVAL="${SIM7600_SIM_CHECK_INTERVAL:-600}"

# APN apply-and-confirm (manage_sim_card_info plan, Phase 2). A wrong APN on an
# LTE-only device is a PERMANENT strand: the data context fails, the device
# leaves the network, and the correction can only travel over the link the
# mistake just broke. So a change is never just written: it is applied, proven
# with real traffic bound to $IFACE inside APN_CONFIRM_WINDOW, and otherwise
# the previous APN (read from the modem, never from settings.json) is restored
# automatically. See apn_maintain / apn_apply.
#
# ABSENT MEANS DON'T TOUCH: no `cellular` namespace in settings.json (or an
# empty apn) issues no AT+CGDCONT write at all — every fielded device has a
# working APN in modem NV and must keep it.
APN_CHECK_INTERVAL="${SIM7600_APN_CHECK_INTERVAL:-30}"     # settings re-read cadence (s)
APN_CONFIRM_WINDOW="${SIM7600_APN_CONFIRM_WINDOW:-300}"    # prove-it window before rollback (s)
APN_MAX_FAILS="${SIM7600_APN_MAX_FAILS:-3}"                # failed applies of the SAME value -> rejected
# Persisted apply/rollback state. Deliberately NOT under /run (dies with a
# reboot): a power cut mid-apply must not lose the way back to the old APN.
APN_STATE_FILE="${SIM7600_APN_STATE_FILE:-/var/lib/xorgate/apn-apply.json}"
# TCP fallback targets (port 443) for the confirm step — some carriers/APNs
# drop ICMP, so a failed ping alone is not proof of a dead bearer.
APN_TCP_HOSTS="${SIM7600_APN_TCP_HOSTS:-1.1.1.1 8.8.8.8}"

# On-demand refresh (Phase 2 §4d): the agent touches this file when the cloud
# asks for a state refresh; we re-arm the SIM/APN/bias probe throttles so the
# next loop tick re-reads everything, instead of waiting out the intervals.
# The agent owns the directory; this daemon (root) reads and deletes the flag.
REFRESH_FLAG_FILE="${SIM7600_REFRESH_FLAG_FILE:-/var/lib/xorgate/refresh-modem.flag}"

# GPS / GNSS
ENABLE_GPS="${SIM7600_ENABLE_GPS:-1}"   # set 0 to skip enabling GNSS
# Constellations on top of GPS (AT+CGNSSMODE bitmask): 1 GLONASS, 2 BeiDou,
# 4 Galileo, 8 QZSS. 15 = everything; more satellites = better geometry when
# only a slot of sky is visible (urban canyon). Needs LE20-era firmware.
GNSS_MODE="${SIM7600_GNSS_MODE:-15}"
GNSS_DPO="${SIM7600_GNSS_DPO:-0}"       # DPO duty-cycling: 0 = off (max sensitivity)
GNSS_XTRA="${SIM7600_GNSS_XTRA:-1}"     # XTRA predicted-ephemeris assistance
# SUPL server for UE-based AGPS (AT+CGPS=1,2). Empty = plain standalone start.
# Port 7276 = plain TCP; 7275 would need a carrier TLS cert (AT+CGPSSSL=1).
SUPL_URL="${SIM7600_SUPL_URL:-supl.google.com:7276}"
# Horizontal accuracy threshold in meters (AT+CGPSHOR, factory default 50).
# Looser lets the engine report an early coarse fix in bad geometry; it then
# refines. For a tracker an early rough position beats a late precise one.
GNSS_ACCURACY="${SIM7600_GNSS_ACCURACY:-100}"
# NMEA sentence bitmask (AT+CGPSNMEA). 197119 = GGA+RMC+GSV+GSA+VTG+ the
# GLONASS/Galileo/BeiDou GSA/GSV variants — the agent parses GSA (HDOP) and
# GSV (satellites in view) for fix confidence.
NMEA_MASK="${SIM7600_NMEA_MASK:-197119}"
# Aux LDO powering an ACTIVE GNSS antenna's LNA (AT+CVAUXS/AT+CVAUXV). Driven
# by `gnssAntBias` in the agent's settings.json (set from the web device
# settings page or `xorgate set gps-antenna`); missing/unreadable file or key
# means passive (bias off), the hardware-safe default. Asserted at boot and
# re-checked live in the health loop — no daemon restart needed to change it.
# The env var only relocates the file for dev setups; it is not a value
# override. 3050 mV is the LDO maximum (hardcoded; set BEFORE output on).
SETTINGS_FILE="${SIM7600_SETTINGS_FILE:-/var/lib/xorgate/settings.json}"
GNSS_ANT_BIAS_MV=3050
GNSS_BIAS_CHECK_INTERVAL=30          # settings.json re-read cadence (seconds)
# XTRA maintenance: re-download when validity drops under a day (file is good
# for ~7 days; SIMCom suggests daily refresh), checking every 6 h while up.
XTRA_MIN_VALID_MIN=1440
XTRA_CHECK_INTERVAL=21600
XTRA_RETRY_DELAY=600                 # re-attempt sooner after a failed download
HTP_HOST="${SIM7600_HTP_HOST:-www.google.com}"  # HTP time source (XTRA needs accurate clock)
GNSS_REASSERT_INTERVAL=600           # re-check the engine every 10 min while connected
# Wedge self-healing (R5, Q1 option (a) — blind but capped): an engine that is
# ON yet fixless this long gets one clean engine restart (stop -> settled SUPL
# start via gps_start). Engine-only, NEVER AT+CRESET — a wedge is an engine
# problem, and the modem reset is what wedged it in the first place. Blind
# cost, accepted: a healthy-but-indoors device loses a few minutes of
# acquisition progress it wasn't converting anyway, at most the capped number
# of times per hour; XTRA persists in module flash, so TTFF once sky returns
# is barely affected.
GNSS_WEDGE_RESTART_SECS="${SIM7600_GNSS_WEDGE_RESTART_SECS:-1200}"
GNSS_WEDGE_RESTART_MAX_PER_HOUR="${SIM7600_GNSS_WEDGE_RESTART_MAX_PER_HOUR:-1}"  # 0 = detect + log only, never restart
# Sky-visibility veto for that detector (optimize-system-process-cycle plan,
# Phase 7). The detector above is blind by construction — the daemon's only fix
# view is AT+CGNSSINFO, so it cannot tell a garage from a wedged engine and
# restarts the engine either way. The agent CAN tell: it parses per-satellite
# C/N0 out of GSV and publishes a hysteretic verdict to this small file
# (device src/lib/sky-state.ts). A sustained "no-rf" means nothing is being
# heard on any constellation — nothing is wedged, the sky is simply missing —
# so the restart is suppressed. Fail-open by design: a missing, unparseable or
# STALE file (dead agent, pre-Phase-7 build) means no opinion, and the detector
# behaves exactly as it did before. Crucially "no NMEA at all" is published as
# `unknown`, never `no-rf`: a silenced NMEA stream is the brownout/wedge
# signature (2026-07-28), the one case the restart must still fire for.
SKY_STATE_FILE="${SIM7600_SKY_STATE_FILE:-/var/lib/xorgate/gnss-sky.json}"
SKY_STATE_FRESH_SECS=60              # older than this = no opinion

# Telemetry
TELEMETRY_FILE="${SIM7600_TELEMETRY_FILE:-/run/sim7600-lte/telemetry.json}"

# --- Logging -----------------------------------------------------------------
LOG_TAG="sim7600-lte"

log()  { echo "[$(date '+%Y-%m-%d %H:%M:%S')] [INFO]  $*" | tee -a /var/log/sim7600-lte.log; logger -t "$LOG_TAG" "$*"; }
warn() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] [WARN]  $*" | tee -a /var/log/sim7600-lte.log; logger -t "$LOG_TAG" -p user.warning "$*"; }
err()  { echo "[$(date '+%Y-%m-%d %H:%M:%S')] [ERROR] $*" | tee -a /var/log/sim7600-lte.log; logger -t "$LOG_TAG" -p user.err "$*"; }

# --- Telemetry ---------------------------------------------------------------
# Writes an atomic JSON snapshot to TELEMETRY_FILE.
# Other processes (GPS, IMU, cloud uploader) can read this at any time
# without risk of reading a half-written file.

# Observability state (R6/R9). Recorded where the decisions already happen
# (gps_start, gnss_fix_present, the CRESET gate, gnss_health) — telemetry-only,
# never an input to recovery logic. All "last knowledge" semantics: the daemon
# probes GNSS only on healthy-LTE health ticks and at the CRESET gate, so while
# LTE is down these hold the most recent verdict, not live truth — readers must
# judge freshness by the paired timestamps, never assume "no change = healthy".
GNSS_ENGINE_ON=""            # 0|1 last definite AT+CGPS? parse ("" = never seen)
GNSS_LAST_FIX=""             # 0|1 verdict of the last AT+CGNSSINFO probe ("" = never probed)
GNSS_FIX_CHECKED_AT=0        # epoch of that probe
GNSS_LAST_ENGINE_START=0     # epoch of the last engine start THIS daemon issued
GNSS_TTFF_S=""               # coarse TTFF (Q6): engine start -> first fix-probe success.
                             # Null until the first post-start fix probe lands; only
                             # measurable within one daemon lifetime (a warm-boot engine
                             # that was already running never arms the clock).
GNSS_STANDALONE_FALLBACK=0   # 1 while the session runs the plain AT+CGPS=1 fallback
GNSS_LAST_RESTART_AT=0       # epoch of the last engine restart
GNSS_LAST_RESTART_REASON=""  # off | wedge | post-reset
CRESET_LAST_PATH=""          # origin of the last fired CRESET: ladder | deadzone | config-drift
LAST_GATE_SKIP_AT=0          # last CRESET gate refusal (mirrors the R7 verdict log line)
LAST_GATE_SKIP_CONTEXT=""    # ladder | deadzone
LAST_GATE_SKIP_REASON=""     # cooldown | radio-cycles | fix-veto | fix-defer | budget

# ISO-8601 UTC JSON value for an epoch: quoted string, or null for 0/empty.
iso_or_null() {
    local epoch="$1"
    if [[ -n "$epoch" && "$epoch" -gt 0 ]]; then
        echo "\"$(date -u -d "@$epoch" '+%Y-%m-%dT%H:%M:%SZ')\""
    else
        echo "null"
    fi
}

# Derived coarse GNSS health for the UI badge (R9) — prints a JSON value.
# Taxonomy (final, recorded in PROGRESS.md Cross-phase state):
#   "ok"         engine on, last fix probe saw a fix
#   "acquiring"  engine on, last probe fixless, wedge window not yet expired
#   "degraded"   engine on but running the standalone fallback (SUPL start
#                failed twice — assist unavailable this session)
#   "unhealthy"  engine off at last knowledge, or fixless past the wedge
#                window (restart pending or cap-suppressed), or a wedge
#                restart fired <1 h ago with no fix seen since
#   null         nothing known yet (engine never read / fix never probed /
#                GPS disabled)
gnss_health_state() {
    if [[ "$ENABLE_GPS" != "1" || -z "$GNSS_ENGINE_ON" ]]; then
        echo "null"
        return
    fi
    if [[ "$GNSS_ENGINE_ON" == "0" ]]; then
        echo '"unhealthy"'
        return
    fi
    local now
    now=$(date +%s)
    if (( NO_FIX_SINCE > 0 && now - NO_FIX_SINCE >= GNSS_WEDGE_RESTART_SECS )); then
        echo '"unhealthy"'
        return
    fi
    if [[ "$GNSS_LAST_RESTART_REASON" == "wedge" ]] && (( now - GNSS_LAST_RESTART_AT < 3600 )); then
        # A wedge restart is "recent" for the cap window; a fix probe success
        # AFTER the restart clears the suspicion (the restart worked).
        if ! { [[ "$GNSS_LAST_FIX" == "1" ]] && (( GNSS_FIX_CHECKED_AT > GNSS_LAST_RESTART_AT )); }; then
            echo '"unhealthy"'
            return
        fi
    fi
    if (( GNSS_STANDALONE_FALLBACK )); then
        echo '"degraded"'
        return
    fi
    case "$GNSS_LAST_FIX" in
        1) echo '"ok"' ;;
        0) echo '"acquiring"' ;;
        *) echo "null" ;;
    esac
}

# Schema (top-level keys pre-date this plan; `gnss` + `recovery` are the
# Phase 5 additions — ADDITIVE ONLY, existing readers must keep working;
# every sub-field is null before first knowledge, the modem_temp pattern):
# {
#   "timestamp":    ISO-8601 UTC string
#   "csq":          integer 0-31, or 99 if unknown
#   "rssi_dbm":     integer dBm, or null if unknown
#   "csq_interval": current adaptive poll interval in seconds
#   "status":       "connected" | "connecting" | "disconnected" | "dead_zone"
#   "reg":          last seen registration <stat> (1 home, 5 roaming, 2
#                   searching, 3 denied, 0 none, 99 unknown)
#   "iface":        interface name
#   "ip":           current IP string, or null
#   "xtra_valid_min": minutes of GNSS XTRA assistance validity left, or null
#   "gnss_ant_bias": 0|1 last-applied antenna bias state, or null before first apply
#   "modem_temp":   modem PMU temperature in °C (AT+CPMUTEMP), or null if
#                   never read / unsupported by this firmware
#   "modem_clock":  {"cclk": "yy/MM/dd,hh:mm:ss±zz", "read_at": epoch_secs,
#                    "reg": <stat>}
#                   raw AT+CCLK? value (NITZ-disciplined local time, zone in
#                   quarter hours) + our clock at the read instant, refreshed
#                   ~60 s; null before the first successful read. Consumed by
#                   the agent's offline clock stepper (Phase 4). `reg` is the
#                   registration <stat> at the READ instant (Phase 8) — the
#                   stepper trusts the value only while it is 1/5, because an
#                   unregistered modem has never been NITZ'd and serves its
#                   factory default (80/01/06).
#   "gnss": {      GNSS engine observability. LAST-KNOWLEDGE values: the fix
#                  probe runs only on healthy-LTE health ticks (10 min) and at
#                  the CRESET gate — while LTE is down these go stale rather
#                  than null; check fix_checked_at before trusting has_fix.
#     "engine_on":       0|1 last definite AT+CGPS? parse, null before first
#     "has_fix":         0|1 last AT+CGNSSINFO verdict (lost read counts as 0,
#                        matching the gate's posture), null before first probe
#     "fix_checked_at":  ISO ts of that verdict, or null
#     "ttff_s":          coarse TTFF secs (engine start -> first fix-probe
#                        success; ceiling ~= the 10-min probe cadence). Null
#                        until measured; resets on every engine start; only
#                        measurable within one daemon lifetime. The accurate
#                        TTFF is the agent's gps.ttff_s (NMEA transition).
#     "last_engine_start":   ISO ts of the last start this daemon issued, or null
#     "last_engine_restart": {"ts": ISO, "reason": "off"|"wedge"|"post-reset"}
#                            or null (reason names the restart path: engine
#                            found off / wedge detector / post-CRESET recovery)
#     "standalone_fallback": 0|1 — 1 while the engine session runs plain
#                            AT+CGPS=1 (SUPL start failed twice, R9 "degraded")
#     "no_fix_since":    ISO ts the current fixless streak started (Phase 4
#                        clock; advances only while LTE-healthy ticks run), or null
#     "wedge_restarts_1h": wedge restarts in the rolling hour (cap window)
#     "health":          "ok"|"acquiring"|"degraded"|"unhealthy"|null — the
#                        derived coarse state (taxonomy at gnss_health_state)
#   }
#   "recovery": {  LTE recovery-machinery observability (R6).
#     "connect_fails":         current consecutive failed connect cycles
#     "radio_cycles_since_ok": radio cycles fired since last verified-healthy link
#     "resets_24h":            CRESETs fired in the rolling budget window
#                              (in-memory: a daemon restart forgets prior spend)
#     "last_reset":     {"ts": ISO, "path": "ladder"|"deadzone"|"config-drift"}
#                       or null — the last FIRED CRESET and which path fired it
#     "last_gate_skip": {"ts": ISO, "context": "ladder"|"deadzone", "reason":
#                       "cooldown"|"radio-cycles"|"fix-veto"|"fix-defer"|
#                       "budget"} or null — last CRESET gate refusal (mirrors
#                       the R7 verdict log line)
#     "last_lte_ok":    ISO ts of the last verified-healthy end-to-end link
#                       (seeded at daemon start — the fix-gate outage clock)
#   }
#   "sim": {       SIM identity + registration (manage_sim_card_info Phase 1).
#                  READ-ONLY. The whole block is ABSENT until the first probe
#                  reaches the modem — and an absent block means "this daemon
#                  has nothing to say", which readers must render as UNKNOWN,
#                  never as "no SIM". A daemon predating this plan writes none.
#     "present":      1 present (ready/locked), 0 the modem says no card is
#                     inserted, null we could not tell (error/unknown state)
#     "state":        "ready" | "locked" | "not-detected" | "error" | "unknown"
#     "iccid":        card serial, key OMITTED until read
#     "msisdn":       phone number, key OMITTED when the carrier never wrote
#                     EF_MSISDN (common) — never emitted as ""
#     "carrier":      operator name, key omitted until known
#     "act":          "2g"|"3g"|"lte"|"5g", key omitted when unknown
#     "registration": "home"|"roaming"|"searching"|"denied"|"not-searching"|
#                     "unknown" — from the same <stat> as the top-level `reg`,
#                     and therefore LAST-KNOWN, not live: LAST_REG is refreshed
#                     by the signal-wait loop and by the health loop's failure
#                     path, so on a healthy link it holds the pre-dial value.
#                     Forced to "unknown" unless `state` is "ready", so a pulled
#                     card can never report a stale "home".
#     "checked_at":   ISO ts of the last probe that reached the modem
#   }
#   "uplink": {    Which interface carries the default route (Phase 1; Phase 2
#                  scales its confirm-before-apply friction off modem_only).
#     "primary":    interface name from the kernel's own route lookup, or null
#     "modem_only": 1 when $IFACE is the ONLY default route — losing LTE loses
#                   the device — else 0
#   }
#   "cellular": {  APN apply machinery (Phase 2). ABSENT on a daemon predating
#                  it — which readers MUST treat as "no rollback safety net on
#                  this device" and refuse to write the cellular namespace.
#     "state":         "in-force" | "applying" | "rolled-back" | "rejected"
#     "apn_applied":   the APN in the modem's data context (AT+CGDCONT? ctx 1,
#                      NV truth), key omitted while never read / NV holds none
#     "apn_requested": last non-empty desired value seen, key omitted when none
#     "last_error":    why the last apply rolled back / was rejected; omitted
#                      when the last transition was clean
#     "fails":         failed applies of apn_requested (cap APN_MAX_FAILS,
#                      keyed on the VALUE — a new value resets it)
#     "changed_at":    ISO ts of the last state transition, or null
#   }
# }
write_telemetry() {
    local csq="$1"
    local status="$2"
    local csq_interval="$3"

    mkdir -p "$(dirname "$TELEMETRY_FILE")"

    local rssi_dbm="null"
    if [[ "$csq" -ne 99 ]]; then
        rssi_dbm=$(( -113 + csq * 2 ))
    fi

    local ip="null"
    local raw_ip
    # Take only the first inet address: an interface can carry several (the modem
    # may hand out more than one), and embedding a multi-line value would produce
    # invalid JSON (a raw newline inside a JSON string). head -n1 keeps it valid.
    raw_ip=$(ip addr show "$IFACE" 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1 | head -n1)
    if [[ -n "$raw_ip" ]]; then
        ip="\"$raw_ip\""
    fi

    local timestamp
    timestamp=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

    # Rolling counters pruned at write time so readers see current values.
    # creset_budget_recount mutates globals — direct call, never substituted.
    creset_budget_recount
    local now_epoch t wedge_restarts_1h=0
    now_epoch=$(date +%s)
    for t in "${GNSS_WEDGE_RESTARTS[@]}"; do
        (( t > now_epoch - 3600 )) && (( wedge_restarts_1h++ ))
    done

    local last_engine_restart="null"
    if [[ -n "$GNSS_LAST_RESTART_REASON" ]]; then
        last_engine_restart="{\"ts\": $(iso_or_null "$GNSS_LAST_RESTART_AT"), \"reason\": \"$GNSS_LAST_RESTART_REASON\"}"
    fi
    local last_reset="null"
    if [[ -n "$CRESET_LAST_PATH" ]]; then
        last_reset="{\"ts\": $(iso_or_null "$LAST_MODEM_RESET"), \"path\": \"$CRESET_LAST_PATH\"}"
    fi
    local last_gate_skip="null"
    if [[ -n "$LAST_GATE_SKIP_REASON" ]]; then
        last_gate_skip="{\"ts\": $(iso_or_null "$LAST_GATE_SKIP_AT"), \"context\": \"$LAST_GATE_SKIP_CONTEXT\", \"reason\": \"$LAST_GATE_SKIP_REASON\"}"
    fi

    # Raw AT+CCLK? pair for the agent's offline clock stepper (Phase 4).
    local modem_clock="null"
    if [[ -n "$MODEM_CCLK" && -n "$MODEM_CCLK_READ_AT" ]]; then
        modem_clock="{\"cclk\": \"$MODEM_CCLK\", \"read_at\": $MODEM_CCLK_READ_AT, \"reg\": ${MODEM_CCLK_REG:-99}}"
    fi

    # SIM block — omitted entirely until a probe has actually reached the modem.
    # Fail open: never synthesise "no SIM" out of a read that did not happen.
    local sim_block="null"
    if (( SIM_CHECKED_AT > 0 )); then
        local sim_present="null"
        case "$SIM_STATE" in
            ready|locked) sim_present=1 ;;
            not-detected) sim_present=0 ;;
        esac
        sim_block="{\"present\": $sim_present, \"state\": \"$SIM_STATE\""
        [[ -n "$SIM_ICCID" ]]   && sim_block+=", \"iccid\": \"$SIM_ICCID\""
        [[ -n "$SIM_MSISDN" ]]  && sim_block+=", \"msisdn\": \"$SIM_MSISDN\""
        [[ -n "$SIM_CARRIER" ]] && sim_block+=", \"carrier\": \"$SIM_CARRIER\""
        [[ -n "$SIM_ACT" ]]     && sim_block+=", \"act\": \"$SIM_ACT\""
        # Registration is only attributable to a card the modem can actually
        # use. LAST_REG is refreshed by the signal-wait loop and by the health
        # loop's FAILURE path, so on a healthy link it holds the value from
        # before the dial — pull the SIM and it would still read 1 (home) for a
        # cycle or two. Emitting "not-detected" next to "home" is incoherent, so
        # anything but a ready card reports "unknown" rather than a stale claim.
        # ("denied" is a READY card the network refused, so it survives this.)
        local sim_reg="unknown"
        [[ "$SIM_STATE" == "ready" ]] && sim_reg=$(sim_registration_label "${LAST_REG:-99}")
        sim_block+=", \"registration\": \"$sim_reg\""
        sim_block+=", \"checked_at\": $(iso_or_null "$SIM_CHECKED_AT")}"
    fi

    # APN machinery block (Phase 2) — omitted until the applied APN has been
    # read or a request seen; its ABSENCE is how readers spot a pre-Phase-2
    # daemon with no rollback support.
    local cellular_block="null"
    if [[ -n "$APN_STATE" ]]; then
        cellular_block="{\"state\": \"$APN_STATE\""
        [[ -n "$APN_APPLIED" ]]    && cellular_block+=", \"apn_applied\": \"$APN_APPLIED\""
        [[ -n "$APN_REQUESTED" ]]  && cellular_block+=", \"apn_requested\": \"$APN_REQUESTED\""
        [[ -n "$APN_LAST_ERROR" ]] && cellular_block+=", \"last_error\": \"$APN_LAST_ERROR\""
        cellular_block+=", \"fails\": ${APN_FAILS:-0}"
        cellular_block+=", \"changed_at\": $(iso_or_null "$APN_CHANGED_AT")}"
    fi

    local up_devs up_primary up_modem_only=0
    up_devs=$(uplink_default_devs)
    up_primary=$(uplink_primary)
    [[ -n "$up_devs" && "$up_devs" == "$IFACE" ]] && up_modem_only=1
    local uplink_primary_json="null"
    [[ -n "$up_primary" ]] && uplink_primary_json="\"$up_primary\""

    # Write to temp file first, then atomically move into place
    # so readers never see a partial file
    local tmp="${TELEMETRY_FILE}.tmp"

    cat > "$tmp" <<EOF
{
  "timestamp": "$timestamp",
  "csq": $csq,
  "rssi_dbm": $rssi_dbm,
  "csq_interval": $csq_interval,
  "status": "$status",
  "reg": ${LAST_REG:-99},
  "iface": "$IFACE",
  "ip": $ip,
  "xtra_valid_min": ${XTRA_VALID_MIN:-null},
  "gnss_ant_bias": ${GNSS_BIAS_APPLIED:-null},
  "modem_temp": ${MODEM_TEMP:-null},
  "modem_clock": $modem_clock,
  "gnss": {
    "engine_on": ${GNSS_ENGINE_ON:-null},
    "has_fix": ${GNSS_LAST_FIX:-null},
    "fix_checked_at": $(iso_or_null "$GNSS_FIX_CHECKED_AT"),
    "ttff_s": ${GNSS_TTFF_S:-null},
    "last_engine_start": $(iso_or_null "$GNSS_LAST_ENGINE_START"),
    "last_engine_restart": $last_engine_restart,
    "standalone_fallback": $GNSS_STANDALONE_FALLBACK,
    "no_fix_since": $(iso_or_null "$NO_FIX_SINCE"),
    "wedge_restarts_1h": $wedge_restarts_1h,
    "health": $(gnss_health_state)
  },
  "recovery": {
    "connect_fails": ${CONNECT_FAILS:-0},
    "radio_cycles_since_ok": ${RADIO_CYCLES_SINCE_OK:-0},
    "resets_24h": ${CRESET_BUDGET_USED:-0},
    "last_reset": $last_reset,
    "last_gate_skip": $last_gate_skip,
    "last_lte_ok": $(iso_or_null "$LAST_LTE_OK")
  },
  "sim": $sim_block,
  "cellular": $cellular_block,
  "uplink": {
    "primary": $uplink_primary_json,
    "modem_only": $up_modem_only
  }
}
EOF

    mv "$tmp" "$TELEMETRY_FILE"
}

# --- AT command helpers -------------------------------------------------------
# CRITICAL: every open of $SERIAL must be guarded by [[ -c ]]. When the modem
# drops off USB (AT+CRESET, power blip) the device node vanishes, and a bare
# `>` or `<>` redirect would CREATE A REGULAR FILE at that path — which then
# blocks the kernel from recreating the real node on re-enumeration. That
# wedges the AT port until someone deletes the file (i.e. "until a reboot").
at_cmd() {
    local cmd="$1"
    local wait="${2:-$AT_CMD_DELAY}"
    [[ -c "$SERIAL" ]] || return 1
    echo -e "${cmd}\r" > "$SERIAL"
    sleep "$wait"
}

# Send a command and return its full response. Opens the port on an fd, drains
# any stale bytes first (leftover URCs / echoes from fire-and-forget at_cmd),
# then reads line-by-line until the final result code or timeout. This avoids
# the write-then-open-reader race of the old `echo; timeout cat` approach,
# which routinely lost responses and produced spurious CSQ=99 readings.
at_query() {
    local cmd="$1" secs="${2:-4}" line resp=""
    [[ -c "$SERIAL" ]] || return 1
    exec 3<>"$SERIAL" 2>/dev/null || return 1
    while IFS= read -r -t 0.1 -u 3 line; do :; done
    printf '%s\r' "$cmd" >&3
    local deadline=$(( SECONDS + secs ))
    while (( SECONDS < deadline )); do
        if IFS= read -r -t 1 -u 3 line; then
            line="${line%$'\r'}"
            [[ -z "$line" ]] && continue
            resp+="$line"$'\n'
            case "$line" in
                OK|ERROR|*"+CME ERROR"*|*"+CMS ERROR"*|*"+IP ERROR"*) break ;;
            esac
        fi
    done
    exec 3<&- 3>&-
    printf '%s' "$resp"
}

# Send a command and wait (up to $2 seconds) for a line matching $3 — for
# commands that answer with a delayed URC (XTRA download, HTP time sync).
# grep -m1 returns as soon as the URC lands, so the happy path doesn't burn
# the full timeout. Prints the matched line, or nothing on timeout.
at_wait_urc() {
    local cmd="$1" secs="$2" pattern="$3"
    [[ -c "$SERIAL" ]] || return 1
    echo -e "${cmd}\r" > "$SERIAL"
    timeout "$secs" grep -m1 -aE "$pattern" "$SERIAL" 2>/dev/null | tr -d '\r'
}

# --- Signal quality ----------------------------------------------------------
get_csq() {
    # One retry on 99/empty: a lost AT response must not masquerade as a dead
    # zone — that misclassification drives the wrong reconnect path.
    local raw val attempt
    for attempt in 1 2; do
        raw=$(at_query "AT+CSQ" | grep "+CSQ:" | head -1)
        val=$(echo "$raw" | sed -n 's/.*+CSQ: \([0-9]*\),.*/\1/p')
        if [[ -n "$val" && "$val" -ne 99 ]]; then
            echo "$val"
            return
        fi
        sleep 1
    done
    echo "${val:-99}"
}

# --- Network registration -----------------------------------------------------
# CS/PS registration status from AT+CEREG (LTE), falling back to AT+CGREG.
# Prints the <stat> digit: 1 home, 5 roaming, 2 searching, 3 denied, 0 none,
# 99 unknown/no-response. Good RSSI without registration is exactly the state
# a long outage leaves behind — it must never be treated as "ready to connect".
LAST_REG=99
# Updates LAST_REG in the CURRENT shell (no command substitution — a subshell
# would throw the value away and telemetry/logs would show a stale 99).
update_reg() {
    local stat
    stat=$(at_query "AT+CEREG?" | sed -n 's/.*+CEREG: [0-9],\([0-9]\).*/\1/p' | head -1)
    if [[ -z "$stat" ]]; then
        stat=$(at_query "AT+CGREG?" | sed -n 's/.*+CGREG: [0-9],\([0-9]\).*/\1/p' | head -1)
    fi
    LAST_REG="${stat:-99}"
}

is_registered() {
    update_reg
    [[ "$LAST_REG" == "1" || "$LAST_REG" == "5" ]]
}

# True when PDP context 1 holds a real WAN address (the RNDIS data call is up
# modem-side). dhclient success can NOT stand in for this — the lease comes
# from the modem's internal NAT and is granted even with the WAN dead.
wan_ip_up() {
    local addr
    addr=$(at_query "AT+CGPADDR=1" | sed -n 's/.*+CGPADDR: 1,"\{0,1\}\([0-9.]*\).*/\1/p' | head -1)
    [[ -n "$addr" && "$addr" != "0.0.0.0" ]]
}

csq_to_dbm() {
    local csq="$1"
    if [[ "$csq" -eq 99 ]]; then echo "unknown"; return; fi
    echo "$(( -113 + csq * 2 )) dBm"
}

# Computes adaptive poll interval based on signal state and rate of change.
# Args: $1=current_csq $2=previous_csq
# Prints interval in seconds.
adaptive_interval() {
    local csq="$1"
    local prev_csq="$2"

    if [[ "$csq" -eq 99 || "$csq" -lt "$MIN_CSQ" ]]; then
        # Weak or unknown — poll more often, reconnect decisions depend on this
        echo "$CSQ_INTERVAL_WEAK"
        return
    fi

    local delta=$(( csq - prev_csq ))
    delta="${delta#-}"   # absolute value

    if [[ "$delta" -ge "$CSQ_RAPID_DELTA" ]]; then
        # Signal changing rapidly — increase resolution
        echo "$CSQ_INTERVAL_FAST"
    else
        # Signal stable — back off to conserve AT bus bandwidth
        echo "$CSQ_INTERVAL_STABLE"
    fi
}

# --- Modem temperature --------------------------------------------------------
# PMU temperature via AT+CPMUTEMP, published in telemetry as "modem_temp".
# Empty until the first successful read; an ERROR/empty response leaves the
# last good value in place (a lost AT response is not a temperature change).
MODEM_TEMP=""
MODEM_TEMP_AT=-9999   # negative so the first maybe_update fires immediately

get_modem_temp() {
    # Updates MODEM_TEMP in the CURRENT shell (same subshell pitfall update_reg
    # documents — command substitution would discard the value).
    local val
    val=$(at_query "AT+CPMUTEMP" | sed -n 's/.*+CPMUTEMP:[[:space:]]*\(-\{0,1\}[0-9]\{1,\}\).*/\1/p' | head -1)
    if [[ -n "$val" ]]; then
        MODEM_TEMP="$val"
    fi
}

# Self-throttled wrapper — call next to get_csq in the poll loops; the daemon
# is single-threaded and sole owner of the AT port, so sequencing is safe.
maybe_update_modem_temp() {
    (( SECONDS - MODEM_TEMP_AT >= MODEM_TEMP_INTERVAL )) || return 0
    MODEM_TEMP_AT=$SECONDS
    get_modem_temp
}

# --- Modem clock export (optimize-system-process-cycle plan, Phase 4) --------
# The module clock is NITZ-disciplined by the carrier at registration (no data
# path needed) and free-runs on the module's own RTC after that. Export the
# raw AT+CCLK? value plus OUR epoch at the read instant into telemetry.json,
# so the agent's offline clock stepper can compute the system-clock offset as
# parse(cclk) - read_at (both captured at the same instant — the device
# clock's own error cancels). The agent owns all parsing/validation (TZ
# quarter-hours, never-NITZ'd bogus years); we just ship the raw pair.
#
# Phase 8 addition: also ship the registration <stat> AT THE READ INSTANT.
# NITZ is network-originated, so an unregistered modem's CCLK is just its own
# RTC — which reads the factory default 80/01/06 after a cold power cut and
# once cost a drive a +53-year clock step. The agent refuses a reading whose
# `reg` is not 1/5; the top-level `reg` is a different instant (telemetry
# write time), so the pair carries its own.
MODEM_CCLK=""
MODEM_CCLK_READ_AT=""
MODEM_CCLK_REG=""
MODEM_CLOCK_AT=-9999      # negative so the first maybe_update fires immediately
MODEM_CLOCK_INTERVAL="${SIM7600_MODEM_CLOCK_INTERVAL:-60}"

maybe_update_modem_clock() {
    (( SECONDS - MODEM_CLOCK_AT >= MODEM_CLOCK_INTERVAL )) || return 0
    MODEM_CLOCK_AT=$SECONDS
    local val
    val=$(at_setting "AT+CCLK?" "CCLK" | tr -d '"' | tr -d '\r')
    if [[ -n "$val" ]]; then
        MODEM_CCLK="$val"
        MODEM_CCLK_READ_AT=$(date +%s)
        MODEM_CCLK_REG="${LAST_REG:-99}"
    fi
}

# --- SIM identity (manage_sim_card_info_in_settings, Phase 1) -----------------
# READ-ONLY. Nothing here writes to the modem or touches how the device dials;
# the worst outcome of a bug is a stale card in the cloud UI.
#
# Why it exists: on 2026-08-08 an intermittent SIM socket answered
# "+CME ERROR: SIM not inserted" for ~24 h while the only thing visible was
# "Good signal (CSQ=26) but NOT registered" — which points at coverage. AT+CSQ
# measures raw RF and returns a number WITH NO SIM PRESENT, so it proved
# nothing. A day of investigation for one AT command nobody was running.
#
# IMSI is deliberately not read (README §9): ICCID + carrier + number answer
# every operational question, and IMSI is the SIM-swap identifier.
SIM_STATE=""          # ready | locked | not-detected | error | unknown ("" = never probed)
SIM_ICCID=""          # card serial (AT+CICCID) — fixed for the life of the card
SIM_MSISDN=""         # phone number (AT+CNUM); often empty, see below
SIM_CARRIER=""        # operator name (AT+COPS?)
SIM_ACT=""            # access technology label derived from AT+COPS? <AcT>
SIM_CHECKED_AT=0      # epoch of the last probe that reached the modem
SIM_COPS_REG=""       # LAST_REG at the last successful AT+COPS? read
SIM_CHECK_AT=-9999    # negative so the first maybe_update fires immediately

# Pure: classify an AT+CPIN? response. Every form below was observed on the
# bench (README §8) or is in the 27.007 CME table for a SIM fault.
sim_state_from_cpin() {
    local resp="$1"
    if [[ "$resp" == *"+CPIN: READY"* ]]; then echo "ready"; return; fi
    # 10 = SIM not inserted. THE case this whole phase exists for.
    if [[ "$resp" == *"SIM not inserted"* ]] || [[ "$resp" =~ \+CME\ ERROR:\ 10([^0-9]|$) ]]; then
        echo "not-detected"; return
    fi
    # 11/12 = SIM PIN / SIM PUK required. Unlocking is out of scope.
    if [[ "$resp" == *"+CPIN: SIM PIN"* || "$resp" == *"+CPIN: SIM PUK"* ]] \
       || [[ "$resp" == *"SIM PIN required"* || "$resp" == *"SIM PUK required"* ]] \
       || [[ "$resp" =~ \+CME\ ERROR:\ 1[12]([^0-9]|$) ]]; then
        echo "locked"; return
    fi
    # 13 SIM failure / 15 SIM wrong: a card is in the slot and the modem cannot
    # use it. Distinct from "not inserted" — reporting it as absent would send
    # someone to the wrong place, which is the mistake this plan is fixing.
    if [[ "$resp" == *"SIM failure"* || "$resp" == *"SIM wrong"* ]] \
       || [[ "$resp" =~ \+CME\ ERROR:\ 1[35]([^0-9]|$) ]]; then
        echo "error"; return
    fi
    # Any other +CPIN state (PH-NET PIN, SIM PIN2, …) means a card IS present
    # but is not usable — closer to locked than to anything else.
    if [[ "$resp" == *"+CPIN:"* ]]; then echo "locked"; return; fi
    # Bare ERROR, "+CME ERROR: unknown", a truncated read: say so.
    echo "unknown"
}

# Pure: the card serial out of AT+CICCID (some firmware answers +CCID:).
sim_iccid_from_ciccid() {
    sed -n 's/.*+I\{0,1\}CCID:[[:space:]]*\([0-9]\{10,22\}\).*/\1/p' <<< "$1" | head -1
}

# Pure: the subscriber number out of AT+CNUM.
#   +CNUM: "","+14379073824",145
# EF_MSISDN is unwritten on many carriers, so EMPTY IS NORMAL — never a fault,
# and the key is omitted rather than emitted blank (README §9, Q4).
sim_msisdn_from_cnum() {
    sed -n 's/.*+CNUM:[^,]*,[[:space:]]*"\{0,1\}\(+\{0,1\}[0-9]\{3,20\}\)"\{0,1\},.*/\1/p' <<< "$1" | head -1
}

# Pure: operator name out of AT+COPS? — +COPS: 0,0,"Public Mobile",7
# Quotes/backslashes/control chars are stripped and the name is capped: it is
# network-supplied free text interpolated straight into the telemetry JSON, and
# an unbalanced quote there would take out every reader of this file.
sim_carrier_from_cops() {
    sed -n 's/.*+COPS:[[:space:]]*[0-9]\{1,\},[[:space:]]*[0-9]\{1,\},[[:space:]]*"\([^"]*\)".*/\1/p' <<< "$1" \
        | head -1 | tr -d '"\\' | tr -d '[:cntrl:]' | cut -c1-32
}

# Pure: the <AcT> digit out of the same response (4th field).
sim_act_from_cops() {
    sed -n 's/.*+COPS:[[:space:]]*[0-9]\{1,\},[[:space:]]*[0-9]\{1,\},[[:space:]]*"[^"]*",[[:space:]]*\([0-9]\{1,\}\).*/\1/p' <<< "$1" | head -1
}

# Pure: 27.007 <AcT> -> a generation label an operator can read.
sim_act_label() {
    case "$1" in
        0|1|3|8)     echo "2g" ;;
        2|4|5|6)     echo "3g" ;;
        7|9)         echo "lte" ;;
        10|11|12|13) echo "5g" ;;
        *)           echo "" ;;
    esac
}

# Pure: 3GPP registration <stat> -> label. "denied" earns its own value: the
# network actively REJECTED this SIM (unpaid / deprovisioned account), which is
# a completely different problem from "searching", and nothing today tells the
# two apart.
sim_registration_label() {
    case "${1:-99}" in
        0) echo "not-searching" ;;
        1) echo "home" ;;
        2) echo "searching" ;;
        3) echo "denied" ;;
        5) echo "roaming" ;;
        *) echo "unknown" ;;
    esac
}

# Self-throttled probe — call next to get_csq in the poll loops. Costs one
# AT+CPIN? per SIM_CHECK_INTERVAL; the identity reads happen ONCE per card.
maybe_update_sim() {
    (( SECONDS - SIM_CHECK_AT >= SIM_CHECK_INTERVAL )) || return 0
    SIM_CHECK_AT=$SECONDS

    local resp state
    resp=$(at_query "AT+CPIN?" 4)
    # A lost read is not a missing SIM. "Absence needs evidence": an empty
    # response means the port went away or the command timed out, so keep the
    # previous verdict and do not even re-stamp checked_at.
    [[ -n "$resp" ]] || return 0
    state=$(sim_state_from_cpin "$resp")

    if [[ "$state" != "$SIM_STATE" ]]; then
        [[ -n "$SIM_STATE" ]] && log "SIM state: $SIM_STATE -> $state"
        SIM_STATE="$state"
        # Identity belongs to the card that was in the slot. A state change may
        # mean a different card (or none), so it is re-read from scratch.
        SIM_ICCID=""; SIM_MSISDN=""; SIM_CARRIER=""; SIM_ACT=""; SIM_COPS_REG=""
    fi
    SIM_CHECKED_AT=$(date +%s)

    [[ "$SIM_STATE" == "ready" ]] || return 0

    # Identity is fixed for the life of the card — read once, then never again.
    # A 60 s ICCID poll would be pure AT-port traffic for a value that cannot
    # change while the device is powered.
    if [[ -z "$SIM_ICCID" ]]; then
        SIM_ICCID=$(sim_iccid_from_ciccid "$(at_query "AT+CICCID" 4)")
        SIM_MSISDN=$(sim_msisdn_from_cnum "$(at_query "AT+CNUM" 4)")
        [[ -n "$SIM_ICCID" ]] && log "SIM ready: ICCID $SIM_ICCID${SIM_MSISDN:+, number $SIM_MSISDN}"
    fi

    # The operator CAN change (roaming, a re-attach on another network), so this
    # one is re-read whenever registration has moved since the last good read —
    # not on every tick, and never while unregistered would answer "+COPS: 0".
    if [[ -z "$SIM_CARRIER" || "$SIM_COPS_REG" != "${LAST_REG:-99}" ]]; then
        local cops carrier
        cops=$(at_query "AT+COPS?" 4)
        carrier=$(sim_carrier_from_cops "$cops")
        if [[ -n "$carrier" ]]; then
            SIM_CARRIER="$carrier"
            SIM_ACT=$(sim_act_label "$(sim_act_from_cops "$cops")")
            SIM_COPS_REG="${LAST_REG:-99}"
        fi
    fi
}

# --- Uplink (which interface actually carries the default route) --------------
# Cheap netlink reads, no AT traffic. `primary` comes from the KERNEL'S OWN
# lookup, not from "lowest metric wins": verified on the bench 2026-08-09, where
# usb0 held a metric-0 default route and beat wlan0's metric 50, the opposite of
# what the route table read at a glance.
#
# `modem_only` is 1 only when the modem interface is the ONLY default route —
# i.e. losing LTE loses the device. A bench box on WiFi reads 0.
uplink_primary() {
    local dev
    dev=$(ip -4 route get "$PING_HOST" 2>/dev/null \
          | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')
    if [[ -z "$dev" ]]; then
        dev=$(uplink_default_devs | head -1)
    fi
    echo "$dev"
}

uplink_default_devs() {
    ip -4 route show default 2>/dev/null \
        | awk '{for(i=1;i<=NF;i++) if($i=="dev") print $(i+1)}' | sort -u
}

# --- APN apply-and-confirm (manage_sim_card_info_in_settings, Phase 2) --------
# The one WRITABLE modem setting this daemon owns, and the one that can strand
# a fielded device (README §5). Design rules, all load-bearing:
#
#   1. ABSENT MEANS DON'T TOUCH. No `cellular` namespace / empty apn -> no
#      AT+CGDCONT write, ever. Every fielded device has a working APN in NV.
#   2. Compare against the APPLIED value (AT+CGDCONT?, the modem's own NV),
#      never the last requested one — a daemon restart must not re-apply an
#      APN already in force, and settings.json is not the truth about the modem.
#   3. Apply-and-confirm: a change must prove REAL traffic over $IFACE inside
#      APN_CONFIRM_WINDOW (ping, then a TCP fallback for ICMP-dropping
#      carriers) or the previous APN is written back and re-dialed.
#   4. The way back is persisted to APN_STATE_FILE BEFORE the modem is touched,
#      and the file lives outside /run — a crash or power cut mid-apply rolls
#      back on the next daemon start (apn_startup_recover).
#   5. The retry cap keys on the VALUE: APN_MAX_FAILS failed applies of the
#      same string -> "rejected", no further attempts until the value changes.
#      A tech trying three different APNs gets three real attempts.
#   6. NOTHING here restarts this service, and the value is charset-checked in
#      THIS script before interpolation (settings.json is not trusted): a quote
#      or CR/LF could terminate the AT command and inject a second one.
APN_APPLIED=""        # what AT+CGDCONT? ctx 1 last read — modem NV truth
APN_REQUESTED=""      # last non-empty desired value seen (persisted across restarts)
APN_STATE=""          # "" (nothing known) | in-force | applying | rolled-back | rejected
APN_LAST_ERROR=""     # static strings only — interpolated into telemetry JSON
APN_CHANGED_AT=0      # epoch of the last state transition
APN_CHECK_AT=-9999    # negative so the first maintain tick fires immediately
APN_FAILS=0           # failed applies of APN_REQUESTED (the CURRENT value)

# Pure: context 1's APN out of an AT+CGDCONT? response.
#   +CGDCONT: 1,"IP","sp.mb.com","0.0.0.0",0,0
apn_from_cgdcont() {
    sed -n 's/.*+CGDCONT: 1,"[^"]*","\([^"]*\)".*/\1/p' <<< "$1" | head -1
}

# Pure: the ONLY charset this script will ever interpolate into an AT command.
apn_valid() {
    [[ "$1" =~ ^[A-Za-z0-9.-]{1,100}$ ]]
}

# Desired APN from settings.json: prints "absent", "empty", "invalid", or
# "ok <apn>". python3, not jq (jq is not guaranteed on the device); any read
# failure resolves to "absent" — the answer that touches nothing.
apn_desired() {
    python3 - "$SETTINGS_FILE" 2>/dev/null <<'PYEOF' || echo "absent"
import json, re, sys
try:
    with open(sys.argv[1]) as f:
        cell = json.load(f).get("cellular")
    apn = cell.get("apn") if isinstance(cell, dict) else None
    if apn is None:
        print("absent")
    elif not isinstance(apn, str):
        print("invalid")
    elif apn == "":
        print("empty")
    elif re.fullmatch(r"[A-Za-z0-9.-]{1,100}", apn):
        print("ok " + apn)
    else:
        print("invalid")
except Exception:
    print("absent")
PYEOF
}

# Persist the apply/rollback state (atomic: tmp + mv). phase is "applying"
# while the modem may hold a change that has not been confirmed — recovery
# rolls back unconditionally whenever it finds that phase.
apn_state_save() {
    local phase="$1" requested="$2" previous="$3" fails="$4"
    mkdir -p "$(dirname "$APN_STATE_FILE")" 2>/dev/null || true
    local tmp="${APN_STATE_FILE}.tmp"
    printf '{"phase": "%s", "requested": "%s", "previous": "%s", "fails": %d}\n' \
        "$phase" "$requested" "$previous" "$fails" > "$tmp" 2>/dev/null || return 1
    mv "$tmp" "$APN_STATE_FILE" 2>/dev/null || return 1
}

# Read the persisted state: prints "phase requested previous fails" with "-"
# for empty fields (values are charset-limited, so space-splitting is safe).
# Any unreadable/garbled file reads as nothing-in-flight.
apn_state_read() {
    python3 - "$APN_STATE_FILE" 2>/dev/null <<'PYEOF' || echo "- - - 0"
import json, re, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
    ok = re.compile(r"[A-Za-z0-9.-]{0,100}$")
    phase = d.get("phase") if d.get("phase") in ("applying", "done") else ""
    req = d.get("requested"); prev = d.get("previous"); fails = d.get("fails")
    if not (isinstance(req, str) and ok.fullmatch(req)): req = ""
    if not (isinstance(prev, str) and ok.fullmatch(prev)): prev = ""
    if not isinstance(fails, int) or fails < 0 or fails > 99: fails = 0
    print("%s %s %s %d" % (phase or "-", req or "-", prev or "-", fails))
except Exception:
    print("- - - 0")
PYEOF
}

# Read the APPLIED APN off the modem into APN_APPLIED. Ctx 1 can legitimately
# hold no APN, so an "OK" with no +CGDCONT line is still a confirmed (empty)
# read; only a lost response leaves the cache alone. Returns 0 on a confirmed
# read.
apn_read_applied() {
    local raw
    raw=$(at_query "AT+CGDCONT?" 6)
    if grep -q "+CGDCONT:\|OK" <<< "$raw"; then
        # NV-sourced text still goes through the charset filter before it can
        # reach telemetry JSON or a future AT interpolation.
        APN_APPLIED=$(apn_from_cgdcont "$raw" | tr -cd 'A-Za-z0-9.-' | cut -c1-100)
        [[ -z "$APN_STATE" ]] && APN_STATE="in-force"
        return 0
    fi
    return 1
}

# One AT+CGDCONT write, verdict observed (at_query, never fire-and-forget).
# EVERY write is logged at the port boundary: this is the single place a
# CGDCONT set can be issued, so "no such log line" IS the evidence that a
# device was never touched — the DoD's absence proof, not an inference.
apn_write_cgdcont() {
    local apn="$1"
    apn_valid "$apn" || return 1
    log "AT port: writing AT+CGDCONT=1,\"IP\",\"$apn\""
    grep -q "OK" <<< "$(at_query "AT+CGDCONT=1,\"IP\",\"$apn\"" 6)"
}

# Drop the data bearer so the NEXT activation attaches with the APN just
# written. Load-bearing, and every line of it bench-verified (2026-08-09):
#   - AT+CGDCONT only takes effect on the next activation, and AT+NETCLOSE
#     does NOT deactivate the context — without a bounce a bogus APN
#     "confirmed end-to-end" over the still-attached old bearer, the exact
#     false success the confirm step exists to prevent.
#   - AT+CGACT=0,1 alone CANNOT do it either: on this firmware it returns
#     ERROR with context 1 left active — including after a clean NETCLOSE —
#     because ctx 1 is the initial default EPS bearer. It is tried first
#     anyway (harmless, and the right tool on firmwares where it works), but
#     the reliable mechanism is a PS detach/re-attach, the same tool recovery
#     L1 already uses. Verified: after CGATT 0/1 the network genuinely
#     re-evaluates ctx 1's APN (a bogus one is left unactivated with no
#     address, which is what makes the confirm fail honestly).
# bring_up_interface re-activates via its wan_ip_up check (AT+CGACT=1,1).
apn_pdp_bounce() {
    at_query "AT+CGACT=0,1" 10 >/dev/null
    at_query "AT+CGATT=0" 15 >/dev/null
    sleep 2
    at_query "AT+CGATT=1" 20 >/dev/null
    sleep 2
}

# End-to-end confirm bound to $IFACE. ICMP first (wan_ping_ok is already
# interface-bound and dual-target); some carriers drop ICMP, so a bounded TCP
# connect (SO_BINDTODEVICE — this daemon runs as root) gets the second opinion
# before anything is rolled back. Prints which check passed; rc 1 = both failed.
apn_confirm_traffic() {
    if wan_ping_ok 2; then echo "ping"; return 0; fi
    local host
    for host in $APN_TCP_HOSTS; do
        if python3 - "$IFACE" "$host" 2>/dev/null <<'PYEOF'
import socket, sys
try:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, 25, (sys.argv[1] + "\0").encode())  # SO_BINDTODEVICE
    s.settimeout(8)
    s.connect((sys.argv[2], 443))
    s.close()
except Exception:
    sys.exit(1)
PYEOF
        then echo "tcp"; return 0; fi
    done
    return 1
}

# Finish anything a previous boot (or a failed rollback write) left half done.
# Called at daemon start and after every modem reset, in both cases BEFORE any
# dialing. Unconditional: phase "applying" means the modem may hold an APN that
# was never confirmed, and the only safe exit is back to `previous`.
apn_startup_recover() {
    local phase req prev fails
    read -r phase req prev fails <<< "$(apn_state_read)"
    [[ "$req" == "-" ]] && req=""
    [[ "$prev" == "-" ]] && prev=""
    APN_FAILS="$fails"
    [[ -n "$req" ]] && APN_REQUESTED="$req"
    if [[ "$phase" != "applying" ]]; then
        return 0
    fi
    APN_FAILS=$(( APN_FAILS + 1 ))
    warn "APN apply of \"${req:-?}\" was interrupted (restart/reset mid-apply) — restoring previous APN \"${prev}\" unconditionally (failure $APN_FAILS/$APN_MAX_FAILS for this value)."
    if [[ -n "$prev" ]] && ! apn_valid "$prev"; then
        err "persisted previous APN failed validation — refusing to write it; modem NV left as-is."
        apn_state_save "done" "$req" "" "$APN_FAILS"
        return 1
    fi
    if [[ -z "$prev" || "$APN_APPLIED" == "$prev" ]] || { { apn_write_cgdcont "$prev" || apn_write_cgdcont "$prev"; } && apn_pdp_bounce; }; then
        [[ -n "$prev" ]] && APN_APPLIED="$prev"
        APN_STATE="rolled-back"
        APN_LAST_ERROR="apply interrupted by a restart; previous APN restored"
        APN_CHANGED_AT=$(date +%s)
        apn_state_save "done" "$req" "$prev" "$APN_FAILS"
        if (( APN_FAILS >= APN_MAX_FAILS )); then
            APN_STATE="rejected"
            err "APN \"$req\" has now failed $APN_FAILS times — rejected until the value changes."
        fi
        return 0
    fi
    # Both rollback writes lost: leave phase=applying so the NEXT recovery
    # (reset exit, daemon restart) retries — the way back is never discarded.
    err "rollback CGDCONT write failed twice — leaving recovery armed for the next modem contact."
    APN_STATE="rolled-back"
    APN_LAST_ERROR="rollback write failed; retrying on next modem contact"
    APN_CHANGED_AT=$(date +%s)
    apn_state_save applying "$req" "$prev" "$APN_FAILS"
    return 1
}

# The apply itself: commit-confirm with automatic rollback (network gear's
# `commit confirm`, borrowed deliberately). Synchronous — this daemon is
# single-threaded and owns the AT port, so nothing else can interleave.
# Telemetry keeps being written inside the confirm wait so readers (boot
# policy, the agent's report plane) never see a stale file during the window.
apn_apply() {
    local new="$1"
    local previous="$APN_APPLIED"
    warn "APN change: applying \"$new\" (previous \"$previous\"; confirm window ${APN_CONFIRM_WINDOW}s; attempt $(( APN_FAILS + 1 ))/$APN_MAX_FAILS for this value)."
    APN_STATE="applying"
    APN_REQUESTED="$new"
    APN_LAST_ERROR=""
    APN_CHANGED_AT=$(date +%s)
    # Rule 4: persist the way back BEFORE the modem is touched.
    apn_state_save applying "$new" "$previous" "$APN_FAILS"
    write_telemetry "$(get_csq)" "connecting" "$APN_CHECK_INTERVAL"

    teardown_interface
    if ! apn_write_cgdcont "$new"; then
        # The modem refused the write outright — NV unchanged, redial as before.
        APN_FAILS=$(( APN_FAILS + 1 ))
        APN_STATE="rolled-back"
        APN_LAST_ERROR="modem rejected the CGDCONT write"
        APN_CHANGED_AT=$(date +%s)
        apn_state_save "done" "$new" "$previous" "$APN_FAILS"
        (( APN_FAILS >= APN_MAX_FAILS )) && APN_STATE="rejected"
        warn "AT+CGDCONT write for \"$new\" was rejected by the modem (failure $APN_FAILS/$APN_MAX_FAILS)."
        bring_up_interface || true
        return 1
    fi

    # The new APN only binds on the next PDP activation — force one.
    apn_pdp_bounce

    # Re-dial and confirm until the window closes. bring_up_interface verifies
    # end-to-end ICMP itself; when only ICMP is blocked the bearer still comes
    # up, and the TCP fallback catches that before rollback fires.
    local deadline=$(( SECONDS + APN_CONFIRM_WINDOW )) via=""
    while true; do
        if bring_up_interface; then
            via="ping"
            break
        fi
        if via=$(apn_confirm_traffic); then
            break
        fi
        via=""
        (( SECONDS >= deadline )) && break
        write_telemetry "$(get_csq)" "connecting" "$APN_CHECK_INTERVAL"
        sleep 5
    done

    if [[ -n "$via" ]]; then
        APN_APPLIED="$new"
        APN_STATE="in-force"
        APN_LAST_ERROR=""
        APN_FAILS=0
        APN_CHANGED_AT=$(date +%s)
        apn_state_save "done" "$new" "$previous" 0
        log "APN \"$new\" confirmed end-to-end (via $via) — in force."
        return 0
    fi

    # Confirm failed: restore the previous APN and prove the way back worked.
    APN_FAILS=$(( APN_FAILS + 1 ))
    warn "APN \"$new\" passed no traffic within ${APN_CONFIRM_WINDOW}s (ping AND tcp failed) — rolling back to \"$previous\" (failure $APN_FAILS/$APN_MAX_FAILS for this value)."
    teardown_interface
    if [[ -z "$previous" ]] || apn_write_cgdcont "$previous" || apn_write_cgdcont "$previous"; then
        [[ -n "$previous" ]] && apn_pdp_bounce
        APN_APPLIED="$previous"
        APN_STATE="rolled-back"
        APN_LAST_ERROR="no data session within ${APN_CONFIRM_WINDOW}s (ping and tcp both failed)"
        APN_CHANGED_AT=$(date +%s)
        apn_state_save "done" "$new" "$previous" "$APN_FAILS"
    else
        # The way back is NOT given up: phase stays "applying" so the startup /
        # post-reset recovery retries the restore on the next modem contact.
        err "rollback CGDCONT write failed twice — recovery stays armed."
        APN_STATE="rolled-back"
        APN_LAST_ERROR="rollback write failed; retrying on next modem contact"
        APN_CHANGED_AT=$(date +%s)
        apn_state_save applying "$new" "$previous" "$APN_FAILS"
    fi
    if (( APN_FAILS >= APN_MAX_FAILS )); then
        APN_STATE="rejected"
        err "APN \"$new\" has failed $APN_FAILS times — rejected; the value must change before another attempt."
    fi
    bring_up_interface || true
    return 1
}

# Self-throttled maintain tick — the ONLY entry point the loops call. Ordered
# so every no-op exits before any AT traffic; the DoD "a device with no
# cellular namespace never sees a CGDCONT write" hangs on these early returns.
apn_maintain() {
    (( SECONDS - APN_CHECK_AT >= APN_CHECK_INTERVAL )) || return 0
    APN_CHECK_AT=$SECONDS

    local verdict desired
    verdict=$(apn_desired)
    case "$verdict" in
        absent|empty)
            # Rule 1. Also clears a stale verdict for a WITHDRAWN request, so a
            # rolled-back badge does not outlive the operator removing the ask.
            if [[ -n "$APN_REQUESTED" && "$APN_REQUESTED" != "$APN_APPLIED" ]]; then
                log "APN request \"$APN_REQUESTED\" withdrawn (settings: $verdict) — modem keeps \"$APN_APPLIED\"."
                APN_REQUESTED=""
                [[ -n "$APN_STATE" ]] && APN_STATE="in-force"
                APN_LAST_ERROR=""
                APN_FAILS=0
                APN_CHANGED_AT=$(date +%s)
                apn_state_save "done" "" "$APN_APPLIED" 0
            fi
            return 0 ;;
        invalid)
            # Rule 6: never let a bad value NEAR the AT port. The agent's own
            # allowlist should make this unreachable; belt and braces.
            if [[ "$APN_STATE" != "rejected" || "$APN_LAST_ERROR" != "invalid APN in settings.json" ]]; then
                warn "cellular.apn in settings.json is invalid (bad characters or too long) — refusing to touch the modem."
                APN_STATE="rejected"
                APN_LAST_ERROR="invalid APN in settings.json"
                APN_CHANGED_AT=$(date +%s)
            fi
            return 0 ;;
        ok\ *)
            desired="${verdict#ok }" ;;
        *)
            return 0 ;;
    esac
    apn_valid "$desired" || return 0   # unreachable after "ok"; defence in depth

    # Rule 2: compare against the modem's APPLIED value.
    if [[ "$desired" == "$APN_APPLIED" ]]; then
        if [[ "$APN_STATE" != "in-force" || "$APN_REQUESTED" != "$desired" ]]; then
            APN_REQUESTED="$desired"
            APN_STATE="in-force"
            APN_LAST_ERROR=""
            APN_FAILS=0
            APN_CHANGED_AT=$(date +%s)
            apn_state_save "done" "$desired" "$APN_APPLIED" 0
        fi
        return 0
    fi

    # Rule 5: the cap keys on the value. A NEW value earns a fresh budget.
    if [[ "$desired" != "$APN_REQUESTED" ]]; then
        APN_FAILS=0
    elif (( APN_FAILS >= APN_MAX_FAILS )); then
        if [[ "$APN_STATE" != "rejected" ]]; then
            APN_STATE="rejected"
            APN_CHANGED_AT=$(date +%s)
        fi
        return 0
    fi

    # Never burn an attempt on coverage: unregistered means the confirm can
    # only fail for reasons that say nothing about the APN. Stays pending.
    if ! is_registered; then
        log "APN change to \"$desired\" deferred — not registered (stat=$LAST_REG)."
        return 0
    fi

    # The rollback target must be current before anything is written.
    apn_read_applied || { warn "APN change deferred — could not read the applied APN off the modem."; return 0; }
    if [[ "$desired" == "$APN_APPLIED" ]]; then
        return 0   # the fresh read says it is already in force
    fi

    apn_apply "$desired" || true
}

# On-demand refresh (Phase 2 §4d): one flag-file stat per loop tick. The agent
# touches the file when the cloud asks; we re-arm every probe throttle so the
# next tick re-reads SIM identity, applied APN and antenna bias immediately.
maybe_refresh_flag() {
    [[ -f "$REFRESH_FLAG_FILE" ]] || return 0
    rm -f "$REFRESH_FLAG_FILE" 2>/dev/null || true
    log "State refresh requested — re-arming SIM/APN/bias probes."
    SIM_CHECK_AT=-9999
    # A re-probe must also re-read the card identity: a refresh is exactly the
    # "I just swapped the SIM, show me" path.
    SIM_ICCID=""; SIM_MSISDN=""; SIM_CARRIER=""; SIM_ACT=""; SIM_COPS_REG=""
    APN_CHECK_AT=-9999
    apn_read_applied || true
    LAST_BIAS_CHECK=0
}

# --- GPS / GNSS --------------------------------------------------------------
# The GNSS engine streams NMEA on the dedicated GPS port (/dev/ttyUSB1), where
# the agent's gps collector reads it; everything here talks only to the AT port
# so there is no contention with that stream.
#
# TTFF strategy (why this is more than the old one-shot AT+CGPS=1):
#   - A bare standalone start must demodulate ephemeris from the satellites at
#     50 bit/s, which needs ~-144 dBm — exactly what an urban canyon denies.
#   - XTRA sideloads predicted ephemeris over LTE instead (valid ~7 days,
#     survives reboots in module flash), so the engine only needs to *track*
#     satellites, not decode them. Requires an accurate module clock, hence
#     the HTP time sync before every download.
#   - SUPL UE-based AGPS (AT+CGPS=1,2) adds server-computed assistance when
#     the data link is up, and the modem auto-falls back to standalone.

# Minutes of XTRA validity left (telemetry), and maintenance timestamps.
XTRA_VALID_MIN=null
LAST_XTRA_CHECK=0
LAST_GNSS_CHECK=0
# Last-applied antenna bias (0/1; empty until configure_gnss asserts it).
GNSS_BIAS_APPLIED=""
LAST_BIAS_CHECK=0

# Desired antenna bias from the agent's settings.json: prints 1 (active) or 0
# (passive). Any failure — missing/unreadable file, bad JSON, missing key, no
# python3 — resolves to 0, the hardware-safe default (jq is not guaranteed on
# the device; python3 is).
gnss_bias_desired() {
    local out
    out=$(python3 - "$SETTINGS_FILE" 2>/dev/null <<'PYEOF'
import json, sys
try:
    with open(sys.argv[1]) as f:
        v = json.load(f).get("gnssAntBias")
    print("1" if v is True else "0")
except Exception:
    print("0")
PYEOF
    )
    if [[ "$out" == "1" ]]; then echo 1; else echo 0; fi
}

# Assert the antenna bias state on the modem (both directions). Safe to apply
# live — it never touches the GNSS engine or the LTE link.
gnss_bias_apply() {
    local desired="$1"
    if [[ "$desired" == "1" ]]; then
        # Order matters: program the LDO voltage BEFORE switching the output
        # on — the register can sit at 1700 mV, which browns out most LNAs.
        log "GNSS antenna bias -> on (AT+CVAUXV=$GNSS_ANT_BIAS_MV then AT+CVAUXS=1) (source: settings)"
        at_cmd "AT+CVAUXV=$GNSS_ANT_BIAS_MV" 1
        at_cmd "AT+CVAUXS=1" 1
    else
        log "GNSS antenna bias -> off (AT+CVAUXS=0) (source: settings)"
        at_cmd "AT+CVAUXS=0" 1
    fi
    GNSS_BIAS_APPLIED="$desired"
}

# Sky visibility as published by the agent: prints "clear", "obstructed",
# "no-rf", or "" for no opinion (missing/unreadable/bad JSON/stale/no python3).
# Same failure posture as gnss_bias_desired — every error path resolves to the
# answer that changes nothing.
gnss_sky_state() {
    python3 - "$SKY_STATE_FILE" "$SKY_STATE_FRESH_SECS" 2>/dev/null <<'PYEOF'
import json, sys, time, calendar
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
    # The file's own ISO stamp is the freshness contract; an agent that died
    # (or never got past boot) must not keep vetoing forever.
    ts = calendar.timegm(time.strptime(d["timestamp"][:19], "%Y-%m-%dT%H:%M:%S"))
    if abs(time.time() - ts) > float(sys.argv[2]):
        print("")
    else:
        s = d.get("state")
        print(s if s in ("clear", "obstructed", "no-rf") else "")
except Exception:
    print("")
PYEOF
}

# --- Recovery policy from settings.json (R8) ---------------------------------
# Two cloud-delivered *policy* knobs ride the same conduit as gnssAntBias
# (web device settings -> retained MQTT -> settings.json -> live re-read here).
# Shape: {"lteRecovery": {"gpsFixResetPolicy": "...", "deadZoneProfile": "..."}}
# Failure posture matches gnss_bias_desired: missing/unreadable file, bad
# JSON, missing key, unknown value, no python3 — all resolve to the safe
# default. Read at decision time (gate check / nudge), so a cloud change
# applies without a daemon restart; both decisions are minutes apart at their
# fastest, so the read is not hot.
lte_recovery_setting() {
    local key="$1"
    python3 - "$SETTINGS_FILE" "$key" 2>/dev/null <<'PYEOF'
import json, sys
try:
    with open(sys.argv[1]) as f:
        v = json.load(f).get("lteRecovery")
    v = (v or {}).get(sys.argv[2]) if isinstance(v, dict) else None
    print(v if isinstance(v, str) else "")
except Exception:
    print("")
PYEOF
}

# lteRecovery.gpsFixResetPolicy — what a live GNSS fix means for CRESET:
#   "defer" (default): reset allowed only once LTE has been unusable
#     >= CRESET_FIX_DEFER_SECS (both ladder and dead-zone paths).
#   "veto-deadzone": a fix fully vetoes dead-zone-path resets; ladder-path
#     resets still follow the defer timing.
gps_fix_reset_policy() {
    case "$(lte_recovery_setting gpsFixResetPolicy)" in
        veto-deadzone) echo "veto-deadzone" ;;
        *)             echo "defer" ;;
    esac
}

# lteRecovery.deadZoneProfile — dead-zone nudge preset bundle. Prints
# "name base_secs cap_secs cycles_before_reset" (cycles -1 = never reset in a
# dead zone). The nudge interval doubles from base to cap. Bundles (the
# Phase 5 settings UI describes these to the user):
#   conservative: 900 -> 1800 -> 3600 -> cap 7200; never auto-resets
#   balanced:     600 -> 1200 -> 2400 -> cap 3600; reset after 3 failed cycles
#   aggressive:   600 -> cap 1200; reset after 1 failed cycle (~legacy cadence
#                 but radio-cycle-first; the global CRESET gate still applies)
deadzone_profile_params() {
    local name base cap cycles
    case "$(lte_recovery_setting deadZoneProfile)" in
        conservative) name=conservative base=900 cap=7200 cycles=-1 ;;
        aggressive)   name=aggressive   base=600 cap=1200 cycles=1 ;;
        *)            name=balanced     base=600 cap=3600 cycles=3 ;;
    esac
    [[ -n "$DEADZONE_NUDGE_BASE_OVERRIDE" ]] && base="$DEADZONE_NUDGE_BASE_OVERRIDE"
    [[ -n "$DEADZONE_NUDGE_CAP_OVERRIDE" ]] && cap="$DEADZONE_NUDGE_CAP_OVERRIDE"
    [[ -n "$DEADZONE_CYCLES_OVERRIDE" ]] && cycles="$DEADZONE_CYCLES_OVERRIDE"
    echo "$name $base $cap $cycles"
}

# Live re-apply: re-read settings.json (self-throttled) and only touch the AT
# port when the desired state differs from what was last applied.
gnss_bias_maintain() {
    [[ "$ENABLE_GPS" == "1" ]] || return 0
    local now
    now=$(date +%s)
    (( now - LAST_BIAS_CHECK < GNSS_BIAS_CHECK_INTERVAL )) && return 0
    LAST_BIAS_CHECK=$now
    local desired
    desired=$(gnss_bias_desired)
    if [[ "$desired" != "$GNSS_BIAS_APPLIED" ]]; then
        gnss_bias_apply "$desired"
    fi
}

gps_engine_on() {
    local state
    state=$(at_query "AT+CGPS?" | sed -n 's/.*+CGPS: \([01]\).*/\1/p')
    # Telemetry: record definite knowledge only — a lost read (empty state)
    # returns "not on" below but must not overwrite the last-known value.
    # Callers invoke this directly (never command-substituted), so the global
    # sticks (same subshell pitfall update_reg documents).
    [[ "$state" == "0" || "$state" == "1" ]] && GNSS_ENGINE_ON="$state"
    [[ "$state" == "1" ]]
}

# Live-fix probe on the AT port (F6): AT+CGNSSINFO answers all-empty fields
# ("+CGNSSINFO: ,,,,,...") without a fix and leads with the fix-mode digit
# with one. The daemon cannot read the NMEA port (the agent owns it), so this
# is its only view of fix state. Failure posture: a lost/failed read counts
# as NO fix — an unreadable probe must not block LTE recovery (the fix gate
# exists to protect a fix we can SEE, not to wedge the ladder).
gnss_fix_present() {
    [[ "$ENABLE_GPS" == "1" ]] || return 1
    local info
    info=$(at_query "AT+CGNSSINFO" 6 | sed -n 's/.*+CGNSSINFO: *//p' | head -1)
    # Telemetry: every probe verdict is recorded (callers invoke this directly,
    # never command-substituted). The lost-read = no-fix posture applies to the
    # recorded verdict too — has_fix mirrors what the daemon ACTS on.
    GNSS_FIX_CHECKED_AT=$(date +%s)
    if [[ "$info" == [0-9]* ]]; then
        GNSS_LAST_FIX=1
        # Coarse daemon-side TTFF (Q6): first fix-probe success after an
        # engine start observed by this daemon. Probe cadence bounds the
        # precision; the agent's NMEA-transition ttff_s is the accurate one.
        if [[ -z "$GNSS_TTFF_S" && "$GNSS_LAST_ENGINE_START" -gt 0 ]]; then
            GNSS_TTFF_S=$(( GNSS_FIX_CHECKED_AT - GNSS_LAST_ENGINE_START ))
        fi
        return 0
    fi
    GNSS_LAST_FIX=0
    return 1
}

# Issue AT+CGPS=0 and wait (bounded) for the engine to REALLY be off.
#
# AT+CGPS=0 answers OK immediately but the engine keeps running for a few more
# seconds (the module emits an async "+CGPS: 0" when it is really down;
# bench-measured ~6 s on 2026-07-30). Any AT+CGPS=<mode> issued inside that
# window is REJECTED — which is what the historical "explicit-mode start failed"
# fallback was really hitting.
#
# Verification uses the RETRYING confirmed read, never the single-shot
# gps_engine_on: this whole code path exists because that read is unreliable at
# cold boot, so depending on it here would reintroduce the very bug (a lost read
# made the stop a no-op, and the re-issued start was rejected again — caught on
# the bench 2026-07-30). Returns 0 only when the engine is confirmed off.
gps_stop_confirmed() {
    log "Stopping GNSS engine for reconfiguration (AT+CGPS=0)..."
    at_cmd "AT+CGPS=0" 3
    GNSS_ENGINE_ON=0
    local deadline=$(( SECONDS + GNSS_STOP_SETTLE_MAX_SECS )) state rc
    while (( SECONDS < deadline )); do
        sleep 1
        state=$(at_setting_confirmed "AT+CGPS?" "CGPS"); rc=$?
        if (( rc == 0 )) && [[ "$state" == 0* ]]; then
            log "GNSS engine confirmed off after $(( GNSS_STOP_SETTLE_MAX_SECS - (deadline - SECONDS) ))s."
            return 0
        fi
    done
    warn "GNSS engine still reads on ${GNSS_STOP_SETTLE_MAX_SECS}s after AT+CGPS=0."
    return 1
}

gps_stop() {
    if gps_engine_on; then
        gps_stop_confirmed || true
    fi
}

# Start the GNSS engine, SUPL-assisted when a SUPL server is configured.
# Callers reach here only with the engine off ("found off", post-reset via
# gnss_recover_after_reset, or a gnss_health wedge restart — the latter two
# stop it first) — never to re-start a running healthy engine (F5 non-goal).
# F5: mode 2 is non-sticky — when the SUPL session fails the engine
# auto-reverts and AT+CGPS? later reads 1,1. That is the documented fallback,
# NOT a failed start; nothing may flap the engine to force mode 2 back
# (gnss_health's "any 1,x = on" check depends on this).
#
# ONE exception to that non-goal, added 2026-07-30 (Phase 7C) on bench evidence:
# after a modem reboot the MODULE restores GNSS by itself as plain 1,1, and a
# mode change is refused while the engine runs — so a device that power-cycles
# (every car start) spent its whole session in standalone mode with the daemon
# reporting a "fallback" that never executed. Correcting THAT — once per daemon
# session, and only when the engine has just come up under the module's own
# restore — is worth the restart: there is no acquisition progress to lose yet,
# and assisted mode is the entire point of configuring a SUPL server.
GNSS_MODE_FIX_DONE=0         # a mode-correcting engine restart COMPLETED this modem boot
# The once-per-modem-boot guard used to latch on the ATTEMPT, so a corrective
# stop that merely ran out of GNSS_STOP_SETTLE_MAX_SECS burned the single
# chance and the leg ran standalone (1 of 5 cold boots, 2026-07-30 field
# drive). "Stopped, corrected" and "could not stop" are different outcomes:
# only the first is done, the second gets one bounded retry.
GNSS_MODE_FIX_STOP_FAILS=0
GNSS_MODE_FIX_MAX_STOP_FAILS=1

# Stop an engine that is running in the WRONG mode so a start can be re-issued.
# Returns 0 when the engine is confirmed off (caller should start it), 1 when it
# must be left alone (already corrected this modem boot, or it refused to stop).
#
# Reached from BOTH detection points, because the cold boot hits them
# unpredictably: when the early-boot AT+CGPS? read succeeds we notice the wrong
# mode up front, and when that read is LOST (the flaky window that also loses
# the CGNSSMODE/CGPSXE/CGPSURL/CGPSNMEA reads — 2026-07-30 15:00 boot) we only
# find out because the start itself is rejected. Correcting in one place and
# giving up in the other is how that boot silently ran standalone all session.
gps_mode_correct() {
    local running="$1" mode="$2" desc="$3"
    (( GNSS_MODE_FIX_DONE )) && return 1
    if (( GNSS_MODE_FIX_STOP_FAILS > GNSS_MODE_FIX_MAX_STOP_FAILS )); then
        return 1
    fi
    log "GNSS engine is running as +CGPS: $running, but $desc ($mode) is configured — the module restores GNSS from NV on its own after a reboot and refuses a mode change while running. Stopping it once so the assisted start is accepted."
    if gps_stop_confirmed; then
        # Latch only on a CONFIRMED stop: the engine is now ours to restart.
        GNSS_MODE_FIX_DONE=1
        return 0
    fi
    GNSS_MODE_FIX_STOP_FAILS=$(( GNSS_MODE_FIX_STOP_FAILS + 1 ))
    if (( GNSS_MODE_FIX_STOP_FAILS > GNSS_MODE_FIX_MAX_STOP_FAILS )); then
        warn "GNSS engine did not stop after $GNSS_MODE_FIX_STOP_FAILS corrective attempts — leaving it in +CGPS: $running for this session."
    else
        warn "GNSS engine did not stop within ${GNSS_STOP_SETTLE_MAX_SECS}s — leaving it in +CGPS: $running for now; one more corrective attempt is allowed this modem boot."
    fi
    return 1
}

gps_start() {
    local mode="1,2" desc="UE-based AGPS (SUPL; auto-falls back to standalone)"
    if [[ -z "$SUPL_URL" ]]; then
        mode="1,1" desc="standalone"
    fi
    if gps_engine_on; then
        # Engine already up. Is it up in the mode we want?
        local running rc
        running=$(at_setting_confirmed "AT+CGPS?" "CGPS"); rc=$?
        if (( rc != 0 )) || [[ "$running" == "$mode"* ]]; then
            return 0
        fi
        gps_mode_correct "$running" "$mode" "$desc" || return 0
    fi
    # F4: the explicit-mode start is rejected when it lands too close to the
    # last AT+CGPS=0 / modem reboot (root cause: the engine is still running —
    # see the GNSS_STOP_SETTLE_MAX_SECS note at the top).
    sleep "$GNSS_START_SETTLE_SECS"
    log "Starting GNSS engine in $desc mode (AT+CGPS=$mode)..."
    # Telemetry: every start attempt re-arms the coarse TTFF clock.
    GNSS_LAST_ENGINE_START=$(date +%s)
    GNSS_TTFF_S=""
    local resp
    resp=$(at_query "AT+CGPS=$mode" 6)
    if ! grep -q "OK" <<< "$resp"; then
        warn "AT+CGPS=$mode not accepted — retrying once after another ${GNSS_START_SETTLE_SECS}s settle."
        sleep "$GNSS_START_SETTLE_SECS"
        resp=$(at_query "AT+CGPS=$mode" 6)
    fi
    if grep -q "OK" <<< "$resp"; then
        GNSS_ENGINE_ON=1
        GNSS_STANDALONE_FALLBACK=0
        return 0
    fi
    # Before believing the start failed, ask what the engine is actually doing.
    # BENCH-VERIFIED 2026-07-30 (optimize-system-process-cycle plan, Phase 7C):
    # the module rejects AT+CGPS=<anything> while the engine is still running —
    # INCLUDING the exact mode requested — so a single lost AT+CGPS? read in the
    # flaky early-boot window (the same one that loses the CGNSSMODE/CGPSXE/
    # CGPSURL/CGPSNMEA reads) is enough to send an already-assisted engine down
    # this path. Every "SUPL assist unavailable" line in the field logs so far
    # was this: +CGPS read back 1,2 the whole time. Claiming a standalone
    # fallback that never happened is worse than the non-event it describes —
    # it sends the next investigation after a phantom.
    local actual rc
    actual=$(at_setting_confirmed "AT+CGPS?" "CGPS"); rc=$?
    if (( rc == 0 )) && [[ "$actual" == "$mode"* ]]; then
        log "AT+CGPS=$mode was rejected because the engine is ALREADY running in that mode (+CGPS: $actual) — the module restores GNSS from NV on its own after a reboot. Nothing to do; $desc mode is active."
        GNSS_ENGINE_ON=1
        GNSS_STANDALONE_FALLBACK=0
        return 0
    fi
    if (( rc == 0 )) && [[ "$actual" == 1,* ]]; then
        # Running, but in the wrong mode — the rejection IS the symptom. This is
        # the cold-boot case where the up-front AT+CGPS? read was lost, so the
        # correction has to happen here or the session stays standalone.
        if gps_mode_correct "$actual" "$mode" "$desc"; then
            sleep "$GNSS_START_SETTLE_SECS"
            log "Re-issuing the $desc start after the corrective stop (AT+CGPS=$mode)..."
            GNSS_LAST_ENGINE_START=$(date +%s)
            resp=$(at_query "AT+CGPS=$mode" 6)
            if grep -q "OK" <<< "$resp"; then
                GNSS_ENGINE_ON=1
                GNSS_STANDALONE_FALLBACK=0
                return 0
            fi
            warn "The re-issued AT+CGPS=$mode was rejected too — restarting the engine plain so GNSS runs at all."
            resp=$(at_query "AT+CGPS=1" 6)
            if grep -q "OK" <<< "$resp"; then
                GNSS_ENGINE_ON=1
                GNSS_STANDALONE_FALLBACK=1
                return 0
            fi
            warn "Plain AT+CGPS=1 was rejected as well — GNSS engine state unknown; the health loop will re-check."
            return 1
        fi
        warn "AT+CGPS=$mode was rejected and the engine is already running as +CGPS: $actual — leaving it alone (mode already corrected once this modem boot, or the engine refused to stop; the wedge detector still governs this session)."
        GNSS_ENGINE_ON=1
        GNSS_STANDALONE_FALLBACK=1
        return 0
    fi
    warn "GNSS explicit-mode start failed twice — falling back to plain AT+CGPS=1 (running standalone: SUPL assist unavailable this session)."
    resp=$(at_query "AT+CGPS=1" 6)
    if grep -q "OK" <<< "$resp"; then
        GNSS_ENGINE_ON=1
        GNSS_STANDALONE_FALLBACK=1
        return 0
    fi
    # Both starts rejected AND the state read is unusable: record NOTHING as
    # definite (GNSS_ENGINE_ON keeps last-knowledge semantics) and let the
    # health loop re-check on its own cadence.
    warn "Plain AT+CGPS=1 was rejected too and the engine state could not be read — leaving GNSS state unknown; the health loop will re-check."
    return 1
}

# Read a "+PREFIX: value" style current setting; prints the raw value part.
at_setting() {
    local cmd="$1" prefix="$2"
    at_query "$cmd" | sed -n "s/.*+${prefix}: *//p" | head -1
}

# Confirmed read of a "+PREFIX: value" setting, for decisions that must not
# act on a lost response. Retries up to AT_SETTING_ATTEMPTS times and prints
# the value as soon as a response line carrying the expected prefix arrives.
# The value may legitimately be empty-looking (an unset AT+CGPSURL answers
# +CGPSURL: "") — presence of the prefix line, not the value, is what marks
# the read confirmed, so callers must branch on the return code, not on -z.
# Returns: 0 = confirmed read (output is authoritative), 2 = firmware
# rejected the command (ERROR seen, no value on any attempt), 1 = no usable
# response at all — the setting is UNKNOWN and must be left alone.
at_setting_confirmed() {
    local cmd="$1" prefix="$2" attempt raw saw_error=0
    for (( attempt = 1; attempt <= AT_SETTING_ATTEMPTS; attempt++ )); do
        (( attempt > 1 )) && sleep "$AT_SETTING_RETRY_DELAY"
        raw=$(at_query "$cmd")
        if grep -qF -- "+${prefix}:" <<< "$raw"; then
            # F7 visibility: callers capture stdout, so log via stderr only.
            (( attempt > 1 )) && log "AT read of +${prefix} needed attempt $attempt/$AT_SETTING_ATTEMPTS (flaky AT port)." >&2
            sed -n "s/.*+${prefix}: *//p" <<< "$raw" | head -1
            return 0
        fi
        grep -q "ERROR" <<< "$raw" && saw_error=1
    done
    (( saw_error )) && return 2
    return 1
}

# Second half of the drift invariant: a confirmed read that differs from the
# target may still be a modem-boot transient (see DRIFT_CONFIRM_DELAY). Waits,
# re-reads, and returns 0 only when the same non-target value is read again —
# only then is the drift real and repair (write/reset) allowed. Logs its
# verdict; callers keep their first read's value on success.
drift_is_stable() {
    local cmd="$1" prefix="$2" first="$3" again rc
    log "Apparent GNSS config drift (+${prefix}: '$first') — re-reading in ${DRIFT_CONFIRM_DELAY}s to rule out a modem-boot transient..."
    sleep "$DRIFT_CONFIRM_DELAY"
    again=$(at_setting_confirmed "$cmd" "$prefix"); rc=$?
    again=${again//\"/}; again=${again// /}
    if (( rc != 0 )); then
        warn "+${prefix} re-read failed — drift unconfirmed, leaving as-is (no write, no reset)."
        return 1
    elif [[ "$again" != "$first" ]]; then
        log "+${prefix} settled to '$again' on re-read — boot transient, not drift; leaving as-is."
        return 1
    fi
    return 0
}

# One-time (idempotent) GNSS configuration. Compares each persisted setting
# and only writes on drift, so a normal boot makes no config writes and no
# modem reset. Settings that need the engine stopped stop it first; settings
# that only apply after a module restart set need_reset, and we AT+CRESET once
# at the end (values persist in NV) — exiting through gnss_recover_after_reset
# like every other reset path.
configure_gnss() {
    if [[ "$ENABLE_GPS" != "1" ]]; then
        log "GNSS setup skipped (SIM7600_ENABLE_GPS=$ENABLE_GPS)."
        return
    fi

    # INVARIANT: need_reset=1 (and any config write) requires CONFIRMED drift:
    # an at_setting_confirmed rc-0 read that differs from the target AND holds
    # on a drift_is_stable re-read. A failed read (rc 1/2) means UNKNOWN: warn
    # and leave the setting alone. Acting on lost reads — or on the well-formed
    # boot-default answers the modem serves for ~10 s after (re)boot — is what
    # fired spurious AT+CRESETs on healthy modems and wedged the GNSS engine
    # (the 2026-07 TTFF field regression).
    local need_reset=0 cur rc

    # Constellations + DPO (persists in NV; module restart to take effect).
    cur=$(at_setting_confirmed "AT+CGNSSMODE?" "CGNSSMODE"); rc=$?
    cur=${cur// /}
    if (( rc == 2 )); then
        warn "AT+CGNSSMODE rejected by firmware (old firmware?) — staying on firmware default constellations."
    elif (( rc != 0 )); then
        warn "AT+CGNSSMODE read failed after $AT_SETTING_ATTEMPTS attempts — constellation config unknown, leaving as-is (no write, no reset)."
    elif [[ "$cur" != "$GNSS_MODE,$GNSS_DPO" ]] && drift_is_stable "AT+CGNSSMODE?" "CGNSSMODE" "$cur"; then
        log "GNSS constellations: $cur -> $GNSS_MODE,$GNSS_DPO (AT+CGNSSMODE)."
        gps_stop
        at_cmd "AT+CGNSSMODE=$GNSS_MODE,$GNSS_DPO" 2
        need_reset=1
    fi

    # XTRA enable (module restart to take effect).
    cur=$(at_setting_confirmed "AT+CGPSXE?" "CGPSXE"); rc=$?
    cur=${cur// /}
    if (( rc != 0 )); then
        warn "AT+CGPSXE read failed after $AT_SETTING_ATTEMPTS attempts — XTRA-enable state unknown, leaving as-is (no write, no reset)."
    elif [[ "$cur" != "$GNSS_XTRA" ]] && drift_is_stable "AT+CGPSXE?" "CGPSXE" "$cur"; then
        log "GNSS XTRA: $cur -> $GNSS_XTRA (AT+CGPSXE)."
        gps_stop
        at_cmd "AT+CGPSXE=$GNSS_XTRA" 2
        need_reset=1
    fi

    # SUPL server for UE-based AGPS (module restart to take effect). A truly
    # unset URL still reads confirmed (+CGPSURL: "" -> rc 0, empty value) and
    # is repaired; only a LOST read skips.
    if [[ -n "$SUPL_URL" ]]; then
        cur=$(at_setting_confirmed "AT+CGPSURL?" "CGPSURL"); rc=$?
        cur=${cur//\"/}
        if (( rc != 0 )); then
            warn "AT+CGPSURL read failed after $AT_SETTING_ATTEMPTS attempts — SUPL server unknown, leaving as-is (no write, no reset)."
        elif [[ "$cur" != "$SUPL_URL" ]] && drift_is_stable "AT+CGPSURL?" "CGPSURL" "$cur"; then
            log "AGPS SUPL server: '${cur:-unset}' -> $SUPL_URL (AT+CGPSURL)."
            gps_stop
            at_cmd "AT+CGPSURL=\"$SUPL_URL\"" 1
            at_cmd "AT+CGPSSSL=0" 1
            need_reset=1
        fi
    fi

    # NMEA sentence mask (needs the engine stopped, no restart).
    cur=$(at_setting_confirmed "AT+CGPSNMEA?" "CGPSNMEA"); rc=$?
    cur=${cur// /}
    if (( rc != 0 )); then
        warn "AT+CGPSNMEA read failed after $AT_SETTING_ATTEMPTS attempts — NMEA mask unknown, leaving as-is (no write)."
    elif [[ "$cur" != "$NMEA_MASK" ]] && drift_is_stable "AT+CGPSNMEA?" "CGPSNMEA" "$cur"; then
        log "NMEA sentence mask: $cur -> $NMEA_MASK (AT+CGPSNMEA)."
        gps_stop
        at_cmd "AT+CGPSNMEA=$NMEA_MASK" 1
    fi

    # Horizontal accuracy threshold: plain write, idempotent.
    at_cmd "AT+CGPSHOR=$GNSS_ACCURACY" 1

    # Active-antenna LNA bias from settings.json. Asserted unconditionally in
    # BOTH directions at boot so a replaced/reflashed module always matches the
    # setting; the health loop (gnss_bias_maintain) re-applies live changes.
    gnss_bias_apply "$(gnss_bias_desired)"

    if [[ "$need_reset" -eq 1 ]]; then
        log "GNSS settings changed — resetting modem (AT+CRESET) to apply..."
        # Config-apply resets are ungated (Q5: confirmed drift must be
        # repaired) but still spend the shared cooldown + rolling budget —
        # a recovery reset shortly after one of these is exactly the power
        # clustering the gate exists to prevent.
        creset_note_fired config-drift
        at_cmd "AT+CRESET" 2
        # The shared routine owns everything after a reset (engine start,
        # XTRA verify, bias re-assert) — nothing more to do here.
        gnss_recover_after_reset || return 1
        return 0
    fi

    # No-reset path only. XTRA auto-inject at GPS start: the command is
    # rejected with "+CME ERROR: XTRA Disabled" until CGPSXE=1 is in effect.
    if [[ "$GNSS_XTRA" == "1" ]]; then
        at_cmd "AT+CGPSXDAUTO=1" 1
    fi

    gps_start
}

# Keep XTRA assistance fresh. Call whenever the data link is up; self-throttles
# to every XTRA_CHECK_INTERVAL (retries sooner after a failure). Syncs the
# module clock over HTP first — XTRA injection silently underperforms with a
# wrong clock. The download runs over the modem's own PS data connection.
xtra_maintain() {
    [[ "$ENABLE_GPS" == "1" && "$GNSS_XTRA" == "1" ]] || return 0

    local now
    now=$(date +%s)
    (( now - LAST_XTRA_CHECK < XTRA_CHECK_INTERVAL )) && return 0
    LAST_XTRA_CHECK=$now

    local valid
    valid=$(at_setting_confirmed "AT+CGPSXTRADATA?" "CGPSXTRADATA" | cut -d, -f1 | tr -d ' ')
    if [[ -n "$valid" && "$valid" -ge "$XTRA_MIN_VALID_MIN" ]]; then
        XTRA_VALID_MIN=$valid
        log "XTRA assistance valid for ${valid} min — no download needed."
        return 0
    fi

    log "XTRA assistance stale/missing (${valid:-unknown} min) — downloading..."

    # XTRA injection needs an accurate module clock. NITZ normally provides it;
    # only fall back to HTP time sync when the clock is obviously bogus (some
    # carriers/SIMs just ERROR on CHTPUPDATE, so it can't be the primary path).
    local year
    year=$(at_setting "AT+CCLK?" "CCLK" | tr -d '"' | cut -d/ -f1)
    if [[ -z "$year" || "10#$year" -lt 24 ]]; then
        warn "Module clock looks unset (year '20${year:-??}') — trying HTP time sync..."
        at_cmd "AT+CHTPSERV=\"ADD\",\"$HTP_HOST\",80,1" 1
        at_wait_urc "AT+CHTPUPDATE" 15 '\+CHTPUPDATE:|ERROR' >/dev/null
    fi

    # Trigger the download, then trust a re-query of the stored file over the
    # URC — the +CGPSXD result can slip between two reads of the AT port.
    at_wait_urc "AT+CGPSXD=0" 60 '\+CGPSXD:' >/dev/null
    valid=$(at_setting_confirmed "AT+CGPSXTRADATA?" "CGPSXTRADATA" | cut -d, -f1 | tr -d ' ')
    if [[ -n "$valid" && "$valid" -ge "$XTRA_MIN_VALID_MIN" ]]; then
        XTRA_VALID_MIN=$valid
        log "XTRA assistance downloaded OK (valid ${valid} min)."
    else
        warn "XTRA download failed (validity ${valid:-unknown} min); will retry in ${XTRA_RETRY_DELAY}s."
        LAST_XTRA_CHECK=$(( now - XTRA_CHECK_INTERVAL + XTRA_RETRY_DELAY ))
    fi
}

# GNSS health check: engine liveness AND engine sanity (R5, principle 4).
# Self-throttles to GNSS_REASSERT_INTERVAL while the link is healthy.
# Liveness (pre-existing): an engine found off (modem reset underneath us,
# AT+CGPSAUTO=0) is restarted. Sanity (the wedge detector): liveness is not
# health — the 2026-07-24 wedge streamed NMEA and "tracked" phantom satellites
# for days without ever fixing, and nothing ever restarted it. The daemon's
# only fix view is AT+CGNSSINFO (F6, via gnss_fix_present; its lost-read =
# no-fix posture is fine here — a flaky probe at worst advances the no-fix
# clock, bounded by the cap), so the detector is blind to the GSV wedge
# signature (Q1 option (b), not built) and works on time alone: engine on but
# fixless for GNSS_WEDGE_RESTART_SECS -> one clean engine restart, hard-capped
# per rolling hour. The restart is justified by the no-fix clock ONLY — never
# by mode (F5: +CGPS: 1,1 after a SUPL auto-fallback is normal, not a failed
# start) — and is engine-only, NEVER escalating to AT+CRESET.
NO_FIX_SINCE=0            # epoch of the current fixless streak's first probe; 0 = no streak
GNSS_WEDGE_RESTARTS=()    # epochs of wedge restarts inside the rolling 1 h cap window
gnss_health() {
    [[ "$ENABLE_GPS" == "1" ]] || return 0
    local now
    now=$(date +%s)
    (( now - LAST_GNSS_CHECK < GNSS_REASSERT_INTERVAL )) && return 0
    LAST_GNSS_CHECK=$now
    if ! gps_engine_on; then
        warn "GNSS engine found off (modem reset?) — restarting it."
        GNSS_LAST_RESTART_AT=$now
        GNSS_LAST_RESTART_REASON="off"
        gps_start
        NO_FIX_SINCE=0
        # An engine found off usually means the modem reset underneath us, which
        # also reverts AT+CVAUXS (antenna bias) to off while our last-applied
        # tracker still says on. Clear the tracker + throttle so the very next
        # gnss_bias_maintain tick re-asserts the bias from settings.json.
        GNSS_BIAS_APPLIED=""
        LAST_BIAS_CHECK=0
        return 0
    fi
    if gnss_fix_present; then
        NO_FIX_SINCE=0
        return 0
    fi
    if (( NO_FIX_SINCE == 0 )); then
        NO_FIX_SINCE=$now
        log "GNSS health: engine on, no fix — no-fix clock armed (engine restart if still fixless after ${GNSS_WEDGE_RESTART_SECS}s)."
        return 0
    fi
    if (( LAST_MODEM_RESET > NO_FIX_SINCE )); then
        # A CRESET path already restarted the engine (gnss_recover_after_reset)
        # after this streak began; the fresh session earns a full window before
        # the blind detector may judge it.
        NO_FIX_SINCE=$now
        log "GNSS health: no-fix clock re-armed — post-reset recovery restarted the engine $(( now - LAST_MODEM_RESET ))s ago; granting the fresh session a full ${GNSS_WEDGE_RESTART_SECS}s window."
        return 0
    fi
    local fixless=$(( now - NO_FIX_SINCE ))
    if (( fixless < GNSS_WEDGE_RESTART_SECS )); then
        log "GNSS health: engine on, no fix for ${fixless}s/${GNSS_WEDGE_RESTART_SECS}s — watching."
        return 0
    fi
    # Sky-visibility veto (Phase 7): the time-only detector is about to call
    # this a wedge. Ask the agent whether there is any sky first — a garage or a
    # bagged antenna is not a wedge, and restarting the engine there throws away
    # acquisition state for nothing. Re-arm the clock so a returning sky earns a
    # full window before the detector may judge the session again.
    local sky
    sky=$(gnss_sky_state)
    if [[ "$sky" == "no-rf" ]]; then
        NO_FIX_SINCE=$now
        log "GNSS health: no fix for ${fixless}s but the agent reports a no-RF sky (no satellite heard on any constellation) — engine restart suppressed; no-fix clock re-armed, so a returning sky gets a full ${GNSS_WEDGE_RESTART_SECS}s window."
        return 0
    fi
    # Rolling per-hour cap (same prune idiom as creset_budget_recount).
    local t kept=()
    for t in "${GNSS_WEDGE_RESTARTS[@]}"; do
        (( t > now - 3600 )) && kept+=("$t")
    done
    GNSS_WEDGE_RESTARTS=("${kept[@]}")
    if (( ${#GNSS_WEDGE_RESTARTS[@]} >= GNSS_WEDGE_RESTART_MAX_PER_HOUR )); then
        warn "GNSS health: no fix for ${fixless}s but engine-restart cap reached (${#GNSS_WEDGE_RESTARTS[@]}/$GNSS_WEDGE_RESTART_MAX_PER_HOUR in the last hour) — restart suppressed (the blind detector must not flap a merely-indoors device)."
        return 0
    fi
    GNSS_WEDGE_RESTARTS+=("$now")
    warn "GNSS health: engine on but fixless for ${fixless}s (>= ${GNSS_WEDGE_RESTART_SECS}s) — wedge suspected; clean engine restart (${#GNSS_WEDGE_RESTARTS[@]}/$GNSS_WEDGE_RESTART_MAX_PER_HOUR this hour; engine-only, never CRESET)."
    GNSS_LAST_RESTART_AT=$now
    GNSS_LAST_RESTART_REASON="wedge"
    gps_stop
    gps_start
    NO_FIX_SINCE=0
}

# --- Modem / interface helpers -----------------------------------------------
wait_for_serial() {
    log "Waiting for modem serial port $SERIAL..."
    local attempts=0
    while [[ ! -c "$SERIAL" ]]; do
        # A REGULAR file at the serial path (left by an unguarded write while
        # the modem was off USB) blocks the kernel from recreating the real
        # node — delete it or the AT port never comes back without a reboot.
        if [[ -e "$SERIAL" ]]; then
            warn "$SERIAL exists but is not a character device — removing impostor file."
            rm -f "$SERIAL"
        fi
        sleep 2
        (( attempts++ ))
        if (( attempts > 30 )); then
            err "Serial port $SERIAL never appeared. Is the modem enumerated?"
            return 1
        fi
    done
    # Best-effort sane line discipline (raw, no echo) — a freshly enumerated
    # tty comes up with defaults that can mangle AT responses.
    stty -F "$SERIAL" 115200 raw -echo 2>/dev/null || true
    log "Serial port $SERIAL is available."
}

modem_at_ok() {
    local response
    response=$(at_query "AT")
    echo "$response" | grep -q "OK"
}

wait_for_modem_ready() {
    log "Waiting for modem AT interface to respond..."
    local attempts=0
    while ! modem_at_ok; do
        sleep 2
        (( attempts++ ))
        if (( attempts > 20 )); then
            err "Modem not responding to AT commands."
            return 1
        fi
    done
    log "Modem is responsive."
}

# Pure: the stored USB PID out of an AT+CUSBPIDSWITCH? response.
#   +CUSBPIDSWITCH: 9001
# Prints the PID digits, or nothing when the response has no such line (the
# caller treats that as "unknown — touch nothing").
usb_pid_from_cusbpidswitch() {
    sed -n 's/.*+CUSBPIDSWITCH: *\([0-9A-Fa-f]\{4,5\}\).*/\1/p' <<< "$1" | head -1
}

# One-time NV correction for a factory-fresh module (hardware-verified
# 2026-08-12: a never-onboarded SIM7600G at PID 9001 registers and applies its
# APN fine, but usb0 cannot exist, so bring_up_interface fails forever). The
# switch re-enumerates the modem, so it must run BEFORE anything else is
# applied to the AT port; on an already-correct module this costs one AT
# round-trip. Failure is not fatal to the daemon — the dial loop keeps
# retrying and logging, same as any other unusable-link state.
ensure_usb_composition() {
    local resp pid
    resp=$(at_query "AT+CUSBPIDSWITCH?" 5)
    pid=$(usb_pid_from_cusbpidswitch "$resp")
    if [[ -z "$pid" ]]; then
        warn "USB composition unreadable (AT+CUSBPIDSWITCH? gave no parsable answer) — leaving it alone."
        return 0
    fi
    if [[ "$pid" == "$USB_PID_WANT" ]]; then
        return 0
    fi
    warn "USB composition is PID $pid, not $USB_PID_WANT (RNDIS) — $IFACE can never enumerate. Switching (persists in modem NV; module re-enumerates now)..."
    at_query "AT+CUSBPIDSWITCH=$USB_PID_WANT,1,1" 10 >/dev/null
    if ! wait_for_modem_reboot; then
        err "Modem never re-enumerated after the USB PID switch."
        return 1
    fi
    resp=$(at_query "AT+CUSBPIDSWITCH?" 5)
    pid=$(usb_pid_from_cusbpidswitch "$resp")
    if [[ "$pid" != "$USB_PID_WANT" ]]; then
        err "USB PID still reads '${pid:-unknown}' after the switch — dials will keep failing."
        return 1
    fi
    log "USB composition switched to PID $USB_PID_WANT (RNDIS)."
}

# After AT+CRESET the modem takes a few seconds to actually drop off USB.
# Checking for the serial port too early sees the OLD node still present and
# then races the disappearance — so first wait for the port to GO AWAY (up to
# 30s; if it never does, proceed anyway), then wait for it to come back.
wait_for_modem_reboot() {
    local waited=0
    while [[ -c "$SERIAL" ]] && (( waited < 30 )); do
        sleep 1
        (( waited++ ))
    done
    if [[ ! -c "$SERIAL" ]]; then
        log "Modem dropped off USB (expected after reset)."
    else
        warn "Modem never left the USB bus after reset; continuing anyway."
    fi
    wait_for_serial || return 1
    wait_for_modem_ready || return 1
}

# --- Unified post-reset GNSS recovery ----------------------------------------
# The ONLY way back from an AT+CRESET. Every reset site (configure_gnss's
# apply-reset tail, recover_modem_reset — which the dead-zone nudge reuses —
# and any future rung) must exit through here so no path can forget a step.
# Contract: call immediately after issuing AT+CRESET. Returns 0 with the GNSS
# engine started (SUPL mode attempted per F4), XTRA validity verified (stale ->
# xtra_maintain throttle cleared), antenna bias re-asserted from settings, and
# the engine/bias check throttles zeroed. Returns 1 only if the modem never
# came back on USB — the caller decides what that means for its path.
gnss_recover_after_reset() {
    # A CRESET re-initialises the SIM interface, and this is the one moment a
    # swapped card could go unnoticed: the identity cache otherwise invalidates
    # only on a CPIN transition, and a 60 s poll can miss the reset's not-ready
    # window entirely. Drop the cache and re-arm the throttle so the next loop
    # tick re-probes (phase-1 §1 rule 1: re-probe identity after a modem reset
    # OR a CPIN transition). Deliberately BEFORE the reboot wait and the
    # ENABLE_GPS gate below — a reset makes the cache suspect whether or not
    # the modem comes back cleanly, and whether or not GNSS is in use.
    # SIM_STATE / SIM_CHECKED_AT are kept: the file's contract is last-knowledge
    # plus a timestamp, so a reader can age it rather than see the block vanish.
    SIM_ICCID=""; SIM_MSISDN=""; SIM_CARRIER=""; SIM_ACT=""; SIM_COPS_REG=""
    SIM_CHECK_AT=-9999
    wait_for_modem_reboot || return 1
    log "Modem back after reset — running unified GNSS recovery."
    # APN (Phase 2): the modem is back — refresh the applied-APN cache off NV
    # (a reset must not leave us trusting our own memory) and retry any
    # rollback that is still armed (a failed rollback write leaves the state
    # file in phase=applying precisely so this path finishes the job). ABOVE
    # the ENABLE_GPS early-return, deliberately: the way back to a working APN
    # must not depend on GPS being enabled — Phase 1 shipped a bug on exactly
    # this line, and the rollback state has the same requirement.
    apn_read_applied || true
    apn_startup_recover || true
    if [[ "$ENABLE_GPS" != "1" ]]; then
        log "GNSS recovery skipped (SIM7600_ENABLE_GPS=$ENABLE_GPS)."
        return 0
    fi
    # F10/F11: right after re-enumeration the modem serves boot-default config
    # answers and auto-restarts the GNSS engine on its own (~10 s in). Sit that
    # window out so we act on the engine's true state instead of racing it.
    log "Settling ${GNSS_RESET_SETTLE_SECS}s for NV load + GNSS auto-restart (F10/F11)..."
    sleep "$GNSS_RESET_SETTLE_SECS"
    # XTRA auto-inject re-assert. Must follow the settle: the command is
    # rejected with "+CME ERROR: XTRA Disabled" until CGPSXE=1 is in effect.
    if [[ "$GNSS_XTRA" == "1" ]]; then
        at_cmd "AT+CGPSXDAUTO=1" 1
    fi
    # F11: the auto-restarted engine runs plain (1,1) — the reset already cost
    # us its acquisition, and no SUPL start was ever issued. One stop followed
    # by a settled gps_start is the sanctioned post-reset SUPL-mode start.
    # (NOT the F5 flapping non-goal: that forbids restarting a healthy running
    # session because AT+CGPS? read 1,1 after a SUPL auto-fallback.)
    GNSS_LAST_RESTART_AT=$(date +%s)
    GNSS_LAST_RESTART_REASON="post-reset"
    gps_stop
    gps_start
    # Verify XTRA assistance survived the reset (file lives in module flash).
    local valid
    valid=$(at_setting_confirmed "AT+CGPSXTRADATA?" "CGPSXTRADATA" | cut -d, -f1 | tr -d ' ')
    [[ -n "$valid" ]] && XTRA_VALID_MIN=$valid
    if [[ -n "$valid" && "$valid" -ge "$XTRA_MIN_VALID_MIN" ]]; then
        log "XTRA assistance intact after reset (valid ${valid} min)."
    else
        warn "XTRA validity ${valid:-unknown} min after reset — clearing throttle so xtra_maintain re-downloads on the next healthy tick."
        LAST_XTRA_CHECK=0
    fi
    # The reset reverted the antenna bias LDO; re-assert from settings NOW
    # rather than waiting for a healthy link (dead-zone resets may not see one
    # for a long time). Zero the throttles so the health loop re-checks soon.
    gnss_bias_apply "$(gnss_bias_desired)"
    LAST_BIAS_CHECK=0
    LAST_GNSS_CHECK=0
    log "Unified GNSS recovery complete."
}

# --- Recovery ladder ----------------------------------------------------------
# Escalating interventions for connect cycles that keep failing. A plain
# NETOPEN+dhclient retry cannot fix a wedged data call: the DHCP lease comes
# from the modem's internal NAT and succeeds regardless of WAN state. These
# operate on the thing that is actually broken — the modem's network attach.

# L1 — PS re-attach + operator reselect. Cheapest; fixes a stale PDP/attach
# left behind when the network detached us during a long outage.
recover_reattach() {
    warn "Recovery L1: PS re-attach (AT+CGATT 0/1) + operator reselect (AT+COPS 2/0)..."
    at_query "AT+CGATT=0" 15 >/dev/null
    sleep 2
    at_query "AT+CGATT=1" 15 >/dev/null
    at_query "AT+COPS=2" 15 >/dev/null
    sleep 2
    at_query "AT+COPS=0" 20 >/dev/null
}

# L1+ — PDP context bounce. Deactivate/reactivate context 1: still cheap,
# WAN-level and GNSS-free; clears a wedged data call that survives a plain
# re-attach (the network re-grants the context from scratch).
recover_pdp_bounce() {
    warn "Recovery L1+: PDP context bounce (AT+CGACT=0,1 -> AT+CGACT=1,1)..."
    at_query "AT+CGACT=0,1" 15 >/dev/null
    sleep 2
    at_query "AT+CGACT=1,1" 30 >/dev/null
}

# L2 — radio cycle (airplane-mode bounce). Forces a full cell reselection and
# fresh registration; recovers a radio that stopped cell-searching. Verified
# GNSS-safe (F1): the engine keeps streaming NMEA through the whole cycle, so
# the ladder and the dead-zone nudge lean on this rung hard.
# Returns 0 when the cycle fired, 2 when skipped on cooldown.
LAST_RADIO_CYCLE=0
# Radio cycles actually fired since the last verified-healthy link (either
# path — ladder or dead-zone). Gates L3 (R3): a reset must be earned by
# cheaper tools having provably failed. Healed in monitor_connection.
RADIO_CYCLES_SINCE_OK=0
recover_radio_cycle() {
    local now
    now=$(date +%s)
    if (( now - LAST_RADIO_CYCLE < RADIO_CYCLE_COOLDOWN )); then
        warn "Recovery L2 skipped — radio cycle on cooldown ($(( RADIO_CYCLE_COOLDOWN - (now - LAST_RADIO_CYCLE) ))s left; re-registration TX bursts are power-expensive)."
        return 2
    fi
    LAST_RADIO_CYCLE=$now
    (( RADIO_CYCLES_SINCE_OK += 1 ))
    warn "Recovery L2: radio cycle (AT+CFUN=0 -> AT+CFUN=1) (#$RADIO_CYCLES_SINCE_OK since last healthy link)..."
    at_query "AT+CFUN=0" 15 >/dev/null
    sleep 5
    at_query "AT+CFUN=1" 20 >/dev/null
    sleep 5
}

# --- The global CRESET gate ---------------------------------------------------
# AT+CRESET is the one GNSS-destructive tool (F3): it destroys acquisition at
# exactly the moment a fast fix matters, and its boot + full re-attach is the
# modem's worst ~2A power profile. Both recovery reset sites (ladder L3 and
# the dead-zone nudge) go through recover_modem_reset below, where four gates
# must ALL clear; every verdict — fired, or skipped and why — is one log
# line (R7). configure_gnss's config-apply reset is deliberately ungated (Q5:
# confirmed NV drift must be repaired), but it reports here so its reset
# still spends cooldown + budget.

# Epochs of fired CRESETs inside the rolling budget window, plus the pruned
# count. In-memory only (daemon restart forgets prior spend — accepted).
CRESET_TIMES=()
CRESET_BUDGET_USED=0

# Recount CRESET_BUDGET_USED, pruning entries older than the window. Mutates
# globals, so callers must NOT command-substitute it (subshell would lose the
# prune — same pitfall update_reg documents).
creset_budget_recount() {
    local now cutoff t kept=()
    now=$(date +%s)
    cutoff=$(( now - CRESET_BUDGET_WINDOW_SECS ))
    for t in "${CRESET_TIMES[@]}"; do
        (( t > cutoff )) && kept+=("$t")
    done
    CRESET_TIMES=("${kept[@]}")
    CRESET_BUDGET_USED=${#CRESET_TIMES[@]}
}

# Record a fired CRESET (any site) into the cooldown clock + budget window.
# $1 = originating path for telemetry: ladder | deadzone | config-drift.
creset_note_fired() {
    LAST_MODEM_RESET=$(date +%s)
    CRESET_TIMES+=("$LAST_MODEM_RESET")
    CRESET_LAST_PATH="${1:-unknown}"
    # A modem reboot re-arms the one-shot GNSS mode correction: the module will
    # restore the engine as plain 1,1 again on the way back up (F11), and this
    # session's earlier correction says nothing about the new one.
    GNSS_MODE_FIX_DONE=0
    GNSS_MODE_FIX_STOP_FAILS=0
}

# Record a CRESET gate refusal for telemetry (the R7 log line is the primary
# record; this mirrors it into recovery.last_gate_skip).
gate_skip_note() {
    LAST_GATE_SKIP_AT=$(date +%s)
    LAST_GATE_SKIP_CONTEXT="$1"
    LAST_GATE_SKIP_REASON="$2"
}

# L3 — full modem reboot, gated. Equivalent to power-cycling the module; the
# modem drops off USB and re-enumerates, so the Phase-2 shared routine
# re-waits for the port and re-establishes GNSS afterwards.
# $1 = calling path, "ladder" (default) or "deadzone" — the fix policy
# distinguishes them. Returns: 0 = reset fired and GNSS recovery completed,
# 1 = reset fired but the modem never re-enumerated (caller decides),
# 2 = gate said no (verdict logged; caller should keep radio-cycling).
LAST_MODEM_RESET=0
LAST_LTE_OK=$(date +%s)   # last verified-healthy end-to-end link (seeded at daemon start)
recover_modem_reset() {
    local context="${1:-ladder}" now
    now=$(date +%s)
    if (( now - LAST_MODEM_RESET < MODEM_RESET_COOLDOWN )); then
        warn "CRESET gate [$context]: skipped — cooldown ($(( MODEM_RESET_COOLDOWN - (now - LAST_MODEM_RESET) ))s left; a reset's boot + attach draws ~2A bursts)."
        gate_skip_note "$context" cooldown
        return 2
    fi
    if (( RADIO_CYCLES_SINCE_OK < CRESET_MIN_RADIO_CYCLES )); then
        warn "CRESET gate [$context]: skipped — only $RADIO_CYCLES_SINCE_OK/$CRESET_MIN_RADIO_CYCLES radio cycles since last healthy link; the GNSS-free rungs must prove useless first (R3)."
        gate_skip_note "$context" radio-cycles
        return 2
    fi
    local fix_state="no-fix"
    if gnss_fix_present; then
        fix_state="fix"
        local policy down_secs
        policy=$(gps_fix_reset_policy)
        down_secs=$(( now - LAST_LTE_OK ))
        if [[ "$policy" == "veto-deadzone" && "$context" == "deadzone" ]]; then
            warn "CRESET gate [$context]: skipped — live GNSS fix vetoes dead-zone resets (policy=veto-deadzone; a dead zone with a fix is an environment, not a modem fault)."
            gate_skip_note "$context" fix-veto
            return 2
        fi
        if (( down_secs < CRESET_FIX_DEFER_SECS )); then
            warn "CRESET gate [$context]: skipped — live GNSS fix and LTE unusable only ${down_secs}s < ${CRESET_FIX_DEFER_SECS}s (policy=$policy; position outranks upload immediacy — uploads buffer)."
            gate_skip_note "$context" fix-defer
            return 2
        fi
    fi
    creset_budget_recount
    if (( CRESET_BUDGET_USED >= CRESET_BUDGET_MAX )); then
        err "CRESET gate [$context]: skipped — reset budget exhausted ($CRESET_BUDGET_USED/$CRESET_BUDGET_MAX in the last $(( CRESET_BUDGET_WINDOW_SECS / 3600 ))h; a modem needing more resets has a problem resets don't fix). Continuing with radio cycles only."
        gate_skip_note "$context" budget
        return 2
    fi
    err "Recovery L3 [$context]: full modem reset (AT+CRESET) — gate passed (cooldown clear, radio cycles since healthy: $RADIO_CYCLES_SINCE_OK, gnss=$fix_state, budget $(( CRESET_BUDGET_USED + 1 ))/$CRESET_BUDGET_MAX with this one)."
    creset_note_fired "$context"
    at_query "AT+CRESET" 5 >/dev/null
    # The shared routine owns the reset exit: engine restart (SUPL mode), XTRA
    # verify, bias re-assert. GNSS NV config survives the reset, so a full
    # configure_gnss re-read pass is not needed (and would land its reads in
    # the F10 boot-default window for nothing).
    gnss_recover_after_reset || return 1
}

# Consecutive failed connect cycles (healed by the health monitor on the first
# verified-healthy ping). Radio-cycle-first ladder in blast-radius order:
#   fail 2: L1, fail 3: L1 + PDP bounce, fails 4-5: L2, fail 6: L3 (gated) —
# then the cycle repeats from L2 (re-attaches have proven useless this deep).
# A gate-skipped L3 radio-cycles instead, so recovery pressure never stops
# and the earned-cycles precondition eventually satisfies itself.
CONNECT_FAILS=0
escalate_recovery() {
    (( CONNECT_FAILS >= 2 )) || return 0
    local rung=$CONNECT_FAILS
    (( rung > 6 )) && rung=$(( 4 + (rung - 7) % 3 ))
    case "$rung" in
        2) recover_reattach ;;
        3) recover_reattach
           recover_pdp_bounce ;;
        4|5) recover_radio_cycle ;;
        6) recover_modem_reset ladder
           case $? in
               1) warn "Modem reset recovery failed (modem never re-enumerated); ladder will retry." ;;
               2) recover_radio_cycle ;;
           esac ;;
    esac
}

teardown_interface() {
    if ip link show "$IFACE" &>/dev/null; then
        log "Tearing down $IFACE..."
        dhclient -r "$IFACE" 2>/dev/null || true
        # dhclient -r only releases the newest lease; stale addresses from
        # earlier cycles (or a meddling NetworkManager) linger and can become
        # the ping source address, which the modem's NAT silently drops.
        pkill -f "dhclient.*$IFACE" 2>/dev/null || true
        ip addr flush dev "$IFACE" 2>/dev/null || true
        ip link set "$IFACE" down 2>/dev/null || true
    fi
    at_cmd "AT+NETCLOSE" 2
}

# IPv6 hard-off for the modem interface, re-asserted on every connect cycle
# (a re-enumerated netdev is born with the kernel default, i.e. v6 back on).
# WHY: the LTE path is IPv4-by-design (PDP type "IP", v4 health probes,
# IPv4-only KVS), but some carriers push a global IPv6 RA through the RNDIS
# tether anyway (Lucky/Bell does; Public Mobile does not). The host then
# holds its ONLY global v6 address + default route on $IFACE, RFC 6724 sorts
# AAAA first for every dual-stack client, and the agent's MQTT dials a
# half-dead v6 path: TCP connects, TLS stalls, connack-timeout loop
# (hardware-verified 2026-08-12 after a Lucky Mobile SIM swap). Setting
# disable_ipv6 also flushes any already-learned RA address/route, so this
# heals late as well as prevents. Per-interface on purpose: wlan0/eth0 IPv6
# stays untouched (MQTT has ridden wlan0's IPv6 through an LTE outage).
ipv6_off_iface() {
    # /proc write, not `sysctl`: no PATH dependence (the fake-hwclock cron
    # taught that lesson), and the node's existence doubles as the iface check.
    local node="/proc/sys/net/ipv6/conf/${IFACE}/disable_ipv6"
    [[ -e "$node" ]] || return 0
    echo 1 > "$node" 2>/dev/null \
        || warn "could not disable IPv6 on $IFACE — a carrier v6 RA may black-hole dual-stack clients (MQTT connack timeouts)."
}

# End-to-end WAN probe ($1 = pings per target, default 2). Success on either
# target; "down" therefore means BOTH failed. A single host's outage must not
# read as a dead WAN and walk the recovery ladder — this hardens the gauge
# while the ladder reshapes the response.
wan_ping_ok() {
    local count="${1:-2}"
    ping -c "$count" -W 4 -I "$IFACE" "$PING_HOST" &>/dev/null && return 0
    [[ -n "$PING_HOST2" ]] || return 1
    ping -c "$count" -W 4 -I "$IFACE" "$PING_HOST2" &>/dev/null
}

bring_up_interface() {
    # The RNDIS data call is dialed by the modem firmware; make sure it is
    # actually up modem-side before touching the host interface at all.
    if ! wan_ip_up; then
        log "PDP context 1 has no WAN address — activating (AT+CGACT=1,1)..."
        at_query "AT+CGACT=1,1" 30 >/dev/null
        if ! wan_ip_up; then
            warn "PDP context still has no WAN address; data call is down modem-side."
            return 1
        fi
    fi

    log "Opening modem network context (AT+NETOPEN)..."
    at_cmd "AT+NETOPEN" 3

    # The netdev vanishes across modem resets/re-enumeration — wait for it.
    local tries=0
    while ! ip link show "$IFACE" &>/dev/null; do
        (( tries++ ))
        if (( tries > 15 )); then
            err "Interface $IFACE never appeared."
            return 1
        fi
        sleep 2
    done

    # Own the interface exclusively: no leftover dhclient, no stale addresses,
    # and no carrier IPv6 (see ipv6_off_iface — the netdev may be freshly
    # re-enumerated here, which resets disable_ipv6 to the kernel default).
    ipv6_off_iface
    pkill -f "dhclient.*$IFACE" 2>/dev/null || true
    ip addr flush dev "$IFACE" 2>/dev/null || true
    ip link set "$IFACE" up 2>/dev/null || true

    log "Running dhclient on $IFACE..."
    timeout 45 dhclient -1 -v "$IFACE" 2>&1 | tee -a /var/log/sim7600-lte.log
    local dh_status="${PIPESTATUS[0]}"
    if [[ "$dh_status" -ne 0 ]]; then
        err "dhclient failed on $IFACE (exit $dh_status)."
        return 1
    fi
    log "Interface $IFACE is up."
    ip addr show "$IFACE" | grep "inet " | tee -a /var/log/sim7600-lte.log

    # dhclient installs its default route at metric 0, which would out-rank
    # WiFi (metric 50) and steal the whole uplink. Re-install it at
    # LTE_ROUTE_METRIC so LTE stays the fallback path.
    local gw
    gw=$(ip route show dev "$IFACE" default 2>/dev/null | awk '{print $3; exit}')
    if [[ -n "$gw" ]]; then
        ip route del default via "$gw" dev "$IFACE" 2>/dev/null || true
        ip route replace default via "$gw" dev "$IFACE" metric "$LTE_ROUTE_METRIC" 2>/dev/null || true
    fi

    # Verify END-TO-END connectivity before declaring victory: the DHCP lease
    # is granted by the modem's own NAT and proves nothing about the WAN.
    local i
    for i in 1 2 3; do
        if wan_ping_ok 1; then
            return 0
        fi
        sleep 3
    done
    err "No end-to-end connectivity through $IFACE after DHCP — WAN side is dead."
    return 1
}

# --- Signal wait loop --------------------------------------------------------
wait_for_stable_signal() {
    log "Waiting for stable LTE signal (min CSQ=$MIN_CSQ, need $SIGNAL_STABLE_COUNT consecutive reads >= $GOOD_CSQ, registered)..."
    local stable_count=0
    local prev_csq=99
    local interval=$CSQ_INTERVAL_WEAK
    # Wedge detection with backoff: no usable service (signal + registration)
    # for the nudge interval may mean the firmware stopped cell-searching —
    # nudge with a radio cycle (GNSS-free, F1). But a dead zone is an
    # environment, not a modem fault: the interval backs off exponentially
    # (profile base -> cap) so overnight garage parking is not a nudge
    # treadmill, and a reset is tried only after the profile's quota of radio
    # cycles provably restored nothing — and then only through the global
    # CRESET gate. Profile params re-read from settings.json at each nudge,
    # so a cloud policy change applies mid-wait without a restart (R8).
    local no_service_since nudge_level=0 dz_cycles=0
    local dz_profile dz_base dz_cap dz_cycles_max nudge_interval
    read -r dz_profile dz_base dz_cap dz_cycles_max <<< "$(deadzone_profile_params)"
    nudge_interval=$dz_base
    no_service_since=$(date +%s)

    while true; do
        local csq dbm
        csq=$(get_csq)
        maybe_refresh_flag
        maybe_update_modem_temp
        maybe_update_modem_clock
        maybe_update_sim
        # A pending APN change is picked up here too: a device that cannot
        # register on the CURRENT APN's network still re-reads settings, so a
        # local `xorgate set apn` (or a cloud change over a second link) is not
        # trapped behind a connection that will never come up. apn_maintain
        # itself defers the actual apply until registered.
        apn_maintain
        dbm=$(csq_to_dbm "$csq")
        interval=$(adaptive_interval "$csq" "$prev_csq")

        if [[ "$csq" -eq 99 ]]; then
            warn "Signal unknown (CSQ=99) — modem may still be registering. [poll=${interval}s]"
            stable_count=0
            write_telemetry "$csq" "disconnected" "$interval"

        elif [[ "$csq" -lt "$MIN_CSQ" ]]; then
            warn "Signal too weak: CSQ=$csq ($dbm). Waiting... [poll=${interval}s]"
            stable_count=0
            write_telemetry "$csq" "dead_zone" "$interval"

        elif [[ "$csq" -ge "$GOOD_CSQ" ]]; then
            if is_registered; then
                no_service_since=$(date +%s)
                nudge_level=0
                dz_cycles=0
                nudge_interval=$dz_base
                (( stable_count++ ))
                # We are actively trying to connect — poll fast regardless of
                # what the adaptive backoff says, so reconnects aren't delayed
                # by the 30s "stable" interval.
                interval=$CSQ_INTERVAL_FAST
                log "Good signal: CSQ=$csq ($dbm), registered (stat=$LAST_REG) [$stable_count/$SIGNAL_STABLE_COUNT stable] [poll=${interval}s]"
                write_telemetry "$csq" "connecting" "$interval"
                if [[ "$stable_count" -ge "$SIGNAL_STABLE_COUNT" ]]; then
                    log "Signal stable and registered. Proceeding with connection."
                    return 0
                fi
            else
                # RF is fine but the network doesn't have us — the classic
                # post-outage wedge. Never connect in this state; the nudge
                # timer below is what gets us out if it persists.
                warn "Good signal (CSQ=$csq, $dbm) but NOT registered (stat=$LAST_REG). Waiting for network attach... [poll=${interval}s]"
                stable_count=0
                write_telemetry "$csq" "disconnected" "$interval"
            fi

        else
            log "Marginal signal: CSQ=$csq ($dbm). Waiting for improvement... [poll=${interval}s]"
            stable_count=0
            write_telemetry "$csq" "disconnected" "$interval"
        fi

        local now
        now=$(date +%s)
        if (( now - no_service_since > nudge_interval )); then
            (( nudge_level++ ))
            read -r dz_profile dz_base dz_cap dz_cycles_max <<< "$(deadzone_profile_params)"
            warn "No usable service for ${nudge_interval}s (nudge #$nudge_level, profile=$dz_profile, cycles since last reset: $dz_cycles) — modem may be wedged."
            local did_reset=0
            if (( dz_cycles_max >= 0 && dz_cycles >= dz_cycles_max )); then
                recover_modem_reset deadzone
                case $? in
                    0) did_reset=1
                       dz_cycles=0 ;;
                    1) did_reset=1
                       dz_cycles=0
                       warn "Modem reset during dead-zone wait failed; will retry." ;;
                    2) : ;;   # gate said no — fall through to a radio cycle
                esac
            fi
            if (( ! did_reset )); then
                recover_radio_cycle && (( dz_cycles += 1 ))
            fi
            nudge_interval=$(( nudge_interval * 2 ))
            (( nudge_interval > dz_cap )) && nudge_interval=$dz_cap
            no_service_since=$(date +%s)
            stable_count=0
        fi

        prev_csq="$csq"
        sleep "$interval"
    done
}

# --- Health monitor ----------------------------------------------------------
# Exit codes:
#   1 — reconnect needed (ping failed, signal OK → network/carrier issue)
#   2 — dead zone (ping failed, signal weak → wait for signal recovery)
monitor_connection() {
    log "Connection monitor started. Pinging $PING_HOST${PING_HOST2:+ (2nd opinion: $PING_HOST2)} every ${HEALTH_CHECK_INTERVAL}s."
    local fail_count=0
    local prev_csq=99

    while true; do
        sleep "$HEALTH_CHECK_INTERVAL"

        local csq dbm interval
        csq=$(get_csq)
        maybe_refresh_flag
        maybe_update_modem_temp
        maybe_update_modem_clock
        maybe_update_sim
        # APN maintain runs on the healthy loop too — an in-force link is
        # exactly where a deliberate APN change arrives from the cloud. The
        # apply is synchronous (commit-confirm-rollback) and the monitor simply
        # resumes pinging whatever link state it left behind.
        apn_maintain
        dbm=$(csq_to_dbm "$csq")
        interval=$(adaptive_interval "$csq" "$prev_csq")
        prev_csq="$csq"

        if wan_ping_ok 2; then
            fail_count=0
            # A verified-healthy link heals the recovery ladder: the failure
            # streak, the earned-radio-cycles gate counter (R3: "since the
            # last healthy connection"), and the fix-gate's outage clock.
            CONNECT_FAILS=0
            RADIO_CYCLES_SINCE_OK=0
            LAST_LTE_OK=$(date +%s)
            write_telemetry "$csq" "connected" "$interval"
            log "Health OK — CSQ=$csq ($dbm) [poll=${interval}s]"
            # GNSS upkeep while the link is healthy (all self-throttle).
            gnss_health
            xtra_maintain
            gnss_bias_maintain
        else
            (( fail_count++ ))
            warn "Ping failed ($fail_count/$HEALTH_FAIL_THRESHOLD) — CSQ=$csq ($dbm)"
            write_telemetry "$csq" "disconnected" "$interval"

            if [[ "$fail_count" -ge "$HEALTH_FAIL_THRESHOLD" ]]; then
                if [[ "$csq" -eq 99 || "$csq" -lt "$MIN_CSQ" ]]; then
                    err "Connection lost — signal too weak (CSQ=$csq). Entering dead zone wait."
                    return 2
                else
                    update_reg
                    err "Connection lost — signal OK (CSQ=$csq, reg=$LAST_REG) but ping failing. Network issue, reconnecting."
                    return 1
                fi
            fi
        fi
    done
}

# --- Main loop ---------------------------------------------------------------
main() {
    log "============================================"
    log " sim7600-lte connection manager starting"
    log " Serial:    $SERIAL"
    log " Interface: $IFACE"
    log " Telemetry: $TELEMETRY_FILE"
    log "============================================"

    log "Waiting ${MODEM_BOOT_WAIT}s for modem USB enumeration..."
    sleep "$MODEM_BOOT_WAIT"

    wait_for_serial || exit 1
    wait_for_modem_ready || exit 1

    # USB composition FIRST: a PID switch re-enumerates the module, which
    # would clobber anything applied to the AT port before it.
    ensure_usb_composition \
        || warn "USB composition fix failed; dials cannot succeed until the PID is $USB_PID_WANT."

    # Kill carrier IPv6 on the modem netdev before the first dial: an RA can
    # land the moment the interface enumerates, and the boot window between
    # enumeration and the first bring_up_interface is long enough for MQTT to
    # wedge on the resulting v6 default route. Re-asserted per connect cycle.
    ipv6_off_iface

    # APN (Phase 2): read what the modem actually holds (context 1 NV) and
    # finish any apply a previous boot left half-done — BEFORE anything dials.
    # The applied value is the rollback anchor and the comparison target for
    # apn_maintain; settings.json is never trusted to know it.
    apn_read_applied || warn "could not read the applied APN at startup — will retry before any change."
    apn_startup_recover || true

    # Configure + start GNSS once the AT interface is up so NMEA flows on the
    # GPS port. May AT+CRESET once on first boot (or config drift) to apply
    # NV settings; a failure here must not block the LTE uplink.
    configure_gnss || warn "GNSS configuration failed; continuing with LTE only."

    while true; do
        # 1. Wait for stable signal AND network registration
        wait_for_stable_signal

        # 2. Bring up interface (verified end-to-end before it returns 0)
        if bring_up_interface; then
            log "LTE connection established (verified end-to-end)."

            # Data is up: fetch/refresh XTRA assistance now (self-throttled).
            xtra_maintain

            # 3. Monitor — exit code tells us why it failed
            monitor_connection
            local monitor_exit=$?

            # 4. Teardown
            warn "Tearing down connection..."
            teardown_interface

            if [[ "$monitor_exit" -eq 2 ]]; then
                # Dead zone — go straight back to signal wait, no reconnect
                # delay, and no ladder escalation: losing signal is expected
                # in the field, not a fault to punish the modem for.
                log "Dead zone detected. Waiting for signal recovery..."
                continue
            fi
            (( CONNECT_FAILS++ ))
        else
            err "Failed to bring up interface."
            teardown_interface
            (( CONNECT_FAILS++ ))
        fi

        # 5. Escalate when plain retries aren't working
        escalate_recovery

        # Backed-off retry delay: 5s, 10s, ... capped at RECONNECT_DELAY_MAX.
        local delay=$(( RECONNECT_DELAY * CONNECT_FAILS ))
        (( delay > RECONNECT_DELAY_MAX )) && delay=$RECONNECT_DELAY_MAX
        (( delay < RECONNECT_DELAY )) && delay=$RECONNECT_DELAY
        log "Retrying in ${delay}s (consecutive failed cycles: $CONNECT_FAILS)..."
        sleep "$delay"
        log "--- Reconnect attempt ---"
    done
}

# Executed (systemd's ExecStart, or by hand) -> run the daemon. SOURCED -> only
# define the functions, so the pure AT-response parsers can be unit-tested
# without opening the serial port or starting a connection manager.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
