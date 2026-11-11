#!/usr/bin/env python3
"""Phase 7c WP2 mutation runner (plan §WP2 "Mutations (each must FAIL)").

    python3 tools/imu/oracle/mutations.py SCRATCH_TREE [--only ID]

SCRATCH_TREE is a COPY of the repo (never the worktree): each mutation is ONE
set of exact-once text replacements, then `cargo test --locked -p xorgate-core
--test imu_oracle_c4 -- --nocapture` must FAIL (non-zero) with at least one
C4 scenario FAIL; the file is restored before the next mutation. A GREEN
baseline runs first and a GREEN final run last. Prints a markdown table.
Runs inside the build image (CARGO_TARGET_DIR set by the caller).
"""
import argparse
import re
import subprocess
import sys

CLIENT = "crates/xorgate-core/src/bno08x_client.rs"
SH2 = "crates/xorgate-core/src/sh2.rs"

MUTATIONS = [
    ("M1", "plan", "drop the second header read (`_read_packet` reuses `_data_ready`'s header)", [
        (CLIENT, "        self.readinto(io, HEADER_LEN)?;\n        let h = self.buffer.header();\n",
         "        let h = self.buffer.header();\n"),
    ]),
    ("M2", "plan", "build the Packet from the 2nd header (not the full read's)", [
        (CLIENT, "        let p = self.buffer.packet();\n",
         "        let p = {\n            let b = self.buffer.as_bytes();\n"
         "            let end = (h.data_length + HEADER_LEN).min(b.len());\n"
         "            Packet { header: h, data: b[HEADER_LEN.min(end)..end].to_vec() }\n        };\n"),
    ]),
    ("M3", "plan", "clear the pending-slice list on error", [
        (CLIENT, "        let clock = &mut *io.clock;\n        sh2::handle_packet_with(",
         "        let clock = &mut *io.clock;\n        let r = sh2::handle_packet_with("),
        (CLIENT, "                clock.monotonic();\n            }\n        })\n",
         "                clock.monotonic();\n            }\n        });\n"
         "        if r.is_err() {\n            self.pending.clear();\n        }\n        r\n"),
    ]),
    ("M4", "plan", "the probe through `write_all(&[])` (no zero-length write syscall)", [
        (CLIENT, "    if bus.write(addr7, &[]).is_ok() {", "    if true {"),
    ]),
    ("M5", "plan", "a sequence byte that ignores the received value (headers not stored)", [
        (CLIENT, "        self.seq.store_received(h.channel, h.sequence)?;",
         "        if usize::from(h.channel) >= 6 {\n"
         "            self.seq.store_received(h.channel, h.sequence)?;\n        }"),
        (CLIENT, "        self.seq\n            .store_received(p.header.channel, p.header.sequence)?;",
         "        if usize::from(p.header.channel) >= 6 {\n"
         "            self.seq.store_received(p.header.channel, p.header.sequence)?;\n        }"),
    ]),
    ("M6", "plan", "soft reset inside the retry `try`", [
        (CLIENT, "            self.soft_reset(io)?;\n            match self.check_id(io) {",
         "            let r = self.soft_reset(io).and_then(|()| self.check_id(io));\n            match r {"),
    ]),
    ("M7", "plan", "reset the error counter only on a NEW report (a changed sample)", [
        (CLIENT, "    let mut consecutive: u32 = 0;\n",
         "    let mut consecutive: u32 = 0;\n    let mut last: Option<([f64; 4], [f64; 3])> = None;\n"),
        (CLIENT, "            Ok((quat, linaccel)) => {\n                consecutive = 0;\n",
         "            Ok((quat, linaccel)) => {\n                if last != Some((quat, linaccel)) {\n"
         "                    consecutive = 0;\n                }\n                last = Some((quat, linaccel));\n"),
    ]),
    ("M8", "plan", "41 instead of 40 consecutive errors", [
        (CLIENT, "                if consecutive >= max_errors {", "                if consecutive > max_errors {"),
    ]),
    ("M9", "plan", "skip the cached-sample emission (emit only a changed sample)", [
        (CLIENT, "    let mut consecutive: u32 = 0;\n",
         "    let mut consecutive: u32 = 0;\n    let mut prev: Option<([f64; 4], [f64; 3])> = None;\n"),
        (CLIENT, "                if io.out.emit(&sample_line(quat, linaccel)).is_err() {\n",
         "                let fresh = prev != Some((quat, linaccel));\n                prev = Some((quat, linaccel));\n"
         "                if fresh && io.out.emit(&sample_line(quat, linaccel)).is_err() {\n"),
    ]),
    ("M10", "plan", "`max_packets` off by one (10 packets per call, not 11)", [
        (CLIENT, "                if max != 0 && processed > max {",
         "                if max != 0 && processed >= max {"),
    ]),
    ("M11", "plan+brief", "map a read EAGAIN to `[Errno 11]` (an OSError) instead of the TypeError", [
        (CLIENT, "            ReadOutcome::WouldBlock => self.buffer.fill(end, ReadBytes::WouldBlock),",
         "            ReadOutcome::WouldBlock => Err(PyExc::os(11, \"Resource temporarily unavailable\")),"),
        (CLIENT, "        Ok(ReadOutcome::WouldBlock) => result.fill(1, ReadBytes::WouldBlock),",
         "        Ok(ReadOutcome::WouldBlock) => Err(PyExc::msg(ExcKind::ValueError, &no_i2c_device_text(address))),"),
    ]),
    ("M12", "brief", "FIFO instead of LIFO over the pending list", [
        (SH2, "    while let Some(slice) = pending.pop() {\n"
              "        process_report(slice.id, &slice.bytes, readings)?;\n        processed(&slice);",
         "    while !pending.is_empty() {\n        let slice = pending.remove(0);\n"
         "        process_report(slice.id, &slice.bytes, readings)?;\n        processed(&slice);"),
    ]),
    ("M13", "brief", "sends through `write_all` (a short write count is retried)", [
        (CLIENT, "        io.bus.write(self.addr7, &bytes).map_err(os_exc)?;\n",
         "        let mut off = 0;\n        while off < bytes.len() {\n"
         "            match io.bus.write(self.addr7, &bytes[off..]).map_err(os_exc)? {\n"
         "                crate::imu_io::WriteOutcome::Written(n) if n > 0 => off += n,\n"
         "                _ => break,\n            }\n        }\n"),
    ]),
    ("X1", "extra", "quaternion captured after the SECOND drain", [
        (CLIENT, "        let quat = self.readings.quaternion()?;\n"
                 "        self.process_available_packets(io, None)?;\n",
         "        self.process_available_packets(io, None)?;\n        let quat = self.readings.quaternion()?;\n"),
    ]),
    ("X2", "extra", "the `_data_ready` lines written at once (no CPython block buffering)", [
        (CLIENT, "        for line in r.stdout {\n            io.out.print(line);\n        }",
         "        for line in r.stdout {\n"
         "            let _ = io.out.sink.write_chunk(&format!(\"{line}\\n\"));\n        }"),
    ]),
    ("X3", "extra", "ENOENT not special (no bus-missing constant)", [
        (CLIENT, "        if e.errno == ENOENT {", "        if e.errno == -ENOENT {"),
    ]),
    ("X4", "extra", "SIGINT at the interval wait ignored", [
        (CLIENT, "        if io.clock.interval_wait(args.interval) == IntervalWait::Interrupted {",
         "        if io.clock.interval_wait(args.interval) == IntervalWait::Interrupted && args.interval < 0.0 {"),
    ]),
    ("X5", "extra", "a successful command response does not read the clock", [
        (CLIENT, "                clock.monotonic();\n", "                let _ = &clock;\n"),
    ]),
    ("X6", "extra", "an unsolicited `0xFC` re-seeds only an absent reading", [
        (SH2, "                readings.0.insert(bytes[1], initial_reading(bytes[1]));",
         "                readings.0.entry(bytes[1]).or_insert_with(|| initial_reading(bytes[1]));"),
    ]),
    ("X7", "extra", "`_wait_for_packet_type` also handles channel 0/1 packets", [
        (CLIENT, "            if packet.header.channel != CHANNEL_EXE"
                 " && packet.header.channel != CHANNEL_SHTP_COMMAND\n            {",
         "            if packet.header.channel < 255 {"),
    ]),
    ("M14", "review F8", "the 2 s `_wait_for_packet` budget compared with `<=`", [
        (CLIENT, "        while io.clock.monotonic() - start < timeout {",
         "        while io.clock.monotonic() - start <= timeout {"),
    ]),
    ("R1", "c5.md 2a", "the 3rd-header sequence stored only when its channel differs from the 2nd header's", [
        (CLIENT, "        self.seq\n            .store_received(p.header.channel, p.header.sequence)?;",
         "        if p.header.channel != h.channel {\n"
         "            self.seq.store_received(p.header.channel, p.header.sequence)?;\n        }"),
    ]),
    ("R2", "c5.md 2a", "the 3rd-header sequence stored only when its count differs from the 2nd header's", [
        (CLIENT, "        self.seq\n            .store_received(p.header.channel, p.header.sequence)?;",
         "        if p.header.packet_byte_count != h.packet_byte_count {\n"
         "            self.seq.store_received(p.header.channel, p.header.sequence)?;\n        }"),
    ]),
    # The reviewer's 12 (review-wp2.md, scratchpad wp2rv/rv_mutations.py), verbatim.

    ("N1", "review", "the error path skips the interval wait (no sleep after a failed iteration)", [
        (CLIENT, "                }\n            }\n            Ok((quat, linaccel)) => {",
            "                }\n                continue;\n            }\n            Ok((quat, linaccel)) => {"),
    ]),
    ("N2", "review", "quaternion read BEFORE the first drain (stale)", [
        (CLIENT, "        self.process_available_packets(io, None)?;\n        let quat = self.readings.quaternion()?;\n",  # noqa: E501 (the reviewer's text, verbatim)
            "        let quat = self.readings.quaternion()?;\n        self.process_available_packets(io, None)?;\n"),
    ]),
    ("N3", "review", "an error in the SECOND drain is swallowed (stale linaccel used)", [
        (CLIENT, "        self.process_available_packets(io, None)?;\n        let linaccel",
            "        let _ = self.process_available_packets(io, None);\n        let linaccel"),
    ]),
    ("N4", "review", "probe: a failed zero-length write raises at once (no 1-byte fallback read)", [
        (CLIENT, "    let mut result = ShtpBuffer::from_bytes(vec![0; 1]);\n",
            "    if address >= 0 {\n        return Err(PyExc::msg(ExcKind::ValueError, &no_i2c_device_text(address)));\n    }\n"  # noqa: E501 (the reviewer's text, verbatim)
            "    let mut result = ShtpBuffer::from_bytes(vec![0; 1]);\n"),
    ]),
    ("N5", "review", "a write EAGAIN is 'sent' but the sequence byte does NOT advance", [
        (CLIENT, "        io.bus.write(self.addr7, &bytes).map_err(os_exc)?;\n        self.seq.advance_after_send(channel);",  # noqa: E501 (the reviewer's text, verbatim)
            "        if matches!(io.bus.write(self.addr7, &bytes).map_err(os_exc)?, crate::imu_io::WriteOutcome::WouldBlock) {\n"  # noqa: E501 (the reviewer's text, verbatim)
            "            return Ok(());\n        }\n        self.seq.advance_after_send(channel);"),
    ]),
    ("N6", "review", "the pending-slice list is cleared at the start of every loop iteration", [
        (CLIENT, "    ) -> Result<([f64; 4], [f64; 3]), PyExc> {\n",
            "    ) -> Result<([f64; 4], [f64; 3]), PyExc> {\n        self.pending.clear();\n"),
    ]),
    ("N7", "review", "enable budget compared with <= (boundary)", [
        (CLIENT, "        while io.clock.monotonic() - start < FEATURE_ENABLE_TIMEOUT {",
            "        while io.clock.monotonic() - start <= FEATURE_ENABLE_TIMEOUT {"),
    ]),
    ("N8", "review", "_wait_for_packet_type budget 5.5 s instead of 5.0 s", [
        (CLIENT, "pub const WAIT_FOR_PACKET_TYPE_TIMEOUT: f64 = 5.0;", "pub const WAIT_FOR_PACKET_TYPE_TIMEOUT: f64 = 5.5;"),  # noqa: E501 (the reviewer's text, verbatim)
    ]),
    ("N9", "review", "the 40x counter counts only consecutive errors with the SAME text", [
        (CLIENT, "    let mut consecutive: u32 = 0;\n",
            "    let mut consecutive: u32 = 0;\n    let mut last_err = String::new();\n"),
        (CLIENT, "            Err(exc) => {\n                consecutive += 1;\n",
            "            Err(exc) => {\n                if exc.py_str() != last_err {\n                    consecutive = 0;\n"  # noqa: E501 (the reviewer's text, verbatim)
            "                    last_err = exc.py_str();\n                }\n                consecutive += 1;\n"),
    ]),
    ("N10", "review", "loop/init read EAGAIN -> [Errno 11] (the readinto half of M11 ONLY; probe untouched)", [
        (CLIENT, "            ReadOutcome::WouldBlock => self.buffer.fill(end, ReadBytes::WouldBlock),",
            "            ReadOutcome::WouldBlock => Err(PyExc::os(11, \"Resource temporarily unavailable\")),"),
    ]),
    ("N11", "review", "readinto short read -> a different IndexError text (8192 clamp / short read)", [
        (CLIENT, "            ReadOutcome::Data(b) => self.buffer.fill(end, ReadBytes::Data(&b)),",
            "            ReadOutcome::Data(b) => self.buffer.fill(end, ReadBytes::Data(&b))"
            ".map_err(|_| PyExc::msg(ExcKind::IndexError, \"bytearray index out of range\")),"),
    ]),
    ("N12", "review", "the 0xFC zero seed for the rotation vector has 3 zeros, not 4", [
        (SH2, "        | REPORT_GEOMAGNETIC_ROTATION_VECTOR => Reading::Floats(vec![0.0; 4]),",
            "        | REPORT_GEOMAGNETIC_ROTATION_VECTOR => Reading::Floats(vec![0.0; 3]),"),
    ]),
    ("X9", "extra", "a raising `str(packet)` does not replace the batch error (review-wp1 F2)", [
        (SH2, "        Err(original) => Err(packet_str_error(packet).unwrap_or(original)),",
         "        Err(original) => Err(original),"),
    ]),
    ("X8", "extra", "the 0.5 s back-off after a failed id attempt dropped", [
        (CLIENT, "                Err(_) => io.clock.sleep(INIT_SLEEP_S),", "                Err(_) => {}"),
    ]),
]


def run_c4(tree):
    p = subprocess.run(
        ["cargo", "test", "--locked", "-p", "xorgate-core", "--test", "imu_oracle_c4", "--", "--nocapture"],
        cwd=tree, capture_output=True, text=True, check=False)
    out = p.stdout + p.stderr
    fails = re.findall(r"^C4 (\S+)\.imutx\s+row\s+\d+ FAIL", out, re.M)
    other = sorted(set(re.findall(r"^test (c4_\w+) \.\.\. FAILED", out, re.M)) - {
        "c4_the_rust_client_equals_the_python_oracle_on_every_scenario"})
    compiled = "could not compile" not in out
    return p.returncode, fails, other, compiled, out


def apply(tree, edits):
    saved = {}
    for path, old, new in edits:
        full = f"{tree}/{path}"
        text = saved.get(full) or open(full).read()
        saved.setdefault(full, text)
        cur = open(full).read()
        n = cur.count(old)
        if n != 1:
            raise SystemExit(f"mutation text found {n}x in {path}: {old[:60]!r}")
        open(full, "w").write(cur.replace(old, new, 1))
    return saved


def restore(saved):
    for full, text in saved.items():
        open(full, "w").write(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tree")
    ap.add_argument("--only")
    a = ap.parse_args()
    rc, fails, other, ok, out = run_c4(a.tree)
    if rc != 0:
        print(out[-4000:])
        raise SystemExit("baseline is not GREEN")
    print("baseline GREEN")
    rows = []
    for mid, src, desc, edits in MUTATIONS:
        if a.only and mid != a.only:
            continue
        saved = apply(a.tree, edits)
        try:
            rc, fails, other, ok, out = run_c4(a.tree)
        finally:
            restore(saved)
        red = rc != 0 and ok and bool(fails or other)
        rows.append((mid, src, desc, red, fails, other, ok))
        print(f"{mid}: {'RED' if red else 'SURVIVED' if ok else 'DID NOT COMPILE'} {fails} {other}", flush=True)
        if not ok:
            print(out[-3000:])
    rc, *_ = run_c4(a.tree)
    print("final GREEN" if rc == 0 else "final NOT GREEN")
    print("\n| ID | Source | Mutation | Verdict | C4 scenarios that FAIL |")
    print("|---|---|---|---|---|")
    for mid, src, desc, red, fails, other, ok in rows:
        v = "**RED**" if red else ("SURVIVED" if ok else "did not compile")
        names = ", ".join(f.split("_")[0] + ("_rs" if f.endswith("_rs") else "") for f in fails)
        names += (" + " + ", ".join(other)) if other else ""
        if fails and all(f.endswith("_rs") for f in fails):
            names += " (**restamp-only**)"
        print(f"| {mid} | {src} | {desc} | {v} | {len(fails)}: {names} |")
    return 0 if rc == 0 and all(r[3] for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
