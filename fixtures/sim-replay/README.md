# Replay fixture pack (Phase 7e)

A tiny asset pack in the `build-scenario.mjs` on-disk shape, for the replay
unit tests (`crates/xorgate-agent/src/replay/tests.rs`) and the sim image's
offline lap check. Everything is synthetic: `cam0/lap.h264` and
`cam0/seg/00000.mp4` are a 3 s 1280x720 15 fps `videotestsrc pattern=ball`
clip (x264, constrained baseline), and the trace carries made-up values with
the real pack's leaf shape classes (`{value}` / `{value, unit}`, int and
float, groups imu/gps/lte/system/obd). No real location or vehicle data.
