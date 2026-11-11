#!/bin/sh
# Phase 7c WP1 mutation check for the pure BNO085 codec (plan
# `phase-7c-rust-imu.md` WP1 "Mutations (each must FAIL)").
#
# Run from the repo root of a SCRATCH COPY of the tree (it edits sources and
# restores them afterwards), inside the build image, e.g.
#   CARGO_TARGET_DIR=/target/<tag>-mut sh tools/imu/codec-fixtures/mutations.sh
# Each mutation must apply exactly once and must turn the WP1 tests RED
# (`cargo test -p xorgate-core --no-fail-fast --lib --test imu_codec_fixtures`:
# both the unit pins and the CPython/Adafruit fixture tests run). The tree is
# GREEN before the first mutation and after the last restore.
set -u

TESTS="cargo test -p xorgate-core --offline --no-fail-fast --lib --test imu_codec_fixtures"
fail=0

run_tests() {
  $TESTS >"$1" 2>&1
}

failed_names() {
  grep -E '^test .* FAILED$' "$1" | sed -e 's/^test //' -e 's/ \.\.\. FAILED$//' | tr '\n' ' '
}

# mutate <id> <file> <old> <new> <description>
mutate() {
  id=$1 file=$2 old=$3 new=$4 desc=$5
  cp "$file" "$file.orig"
  if ! OLD="$old" NEW="$new" perl -0pi -e '$n += s/\Q$ENV{OLD}\E/$ENV{NEW}/g; END { exit($n == 1 ? 0 : 3) }' "$file"; then
    echo "$id NOT-APPLIED ($desc)"
    mv "$file.orig" "$file"
    fail=1
    return
  fi
  log="/tmp/mut-$id.log"
  if run_tests "$log"; then
    echo "$id SURVIVED ($desc)"
    fail=1
  elif grep -q '^error\[' "$log"; then
    echo "$id COMPILE-ERROR ($desc)"
    fail=1
  else
    echo "$id RED ($desc): $(failed_names "$log")"
  fi
  mv "$file.orig" "$file"
  touch "$file"
}

if ! run_tests /tmp/mut-baseline.log; then
  echo "BASELINE NOT GREEN"
  exit 2
fi
echo "baseline GREEN: $(grep -c '^test .* ok$' /tmp/mut-baseline.log) tests"

C=crates/xorgate-core/src

# --- the eight named WP1 mutations ---------------------------------------
mutate M1 $C/shtp.rs \
  'u16::from_le_bytes([b[0], b[1]]) & !CONTINUATION_BIT;' \
  'u16::from_le_bytes([b[0], b[1]]);' \
  'continuation mask omitted'
mutate M2 $C/shtp.rs \
  'if header.packet_byte_count == ERROR_COUNT {' \
  'if u16::from_le_bytes([raw[0], raw[1]]) == ERROR_COUNT {' \
  'mask applied AFTER the 0x7FFF check (check on the raw count)'
mutate M3 $C/sh2.rs \
  'while let Some(slice) = pending.pop() {' \
  'while let Some(slice) = (!pending.is_empty()).then(|| pending.remove(0)) {' \
  'FIFO instead of LIFO'
mutate M4 $C/sh2.rs \
  'REPORT_LINEAR_ACCELERATION => (Scalar::Q8, 3, 10),' \
  'REPORT_LINEAR_ACCELERATION => (Scalar::Q12, 3, 10),' \
  'Q8 -> Q12 for linaccel'
mutate M5 $C/py_fmt.rs \
  'write!(out, "e{sign}{:02}", exp.abs());' \
  'write!(out, "e{sign}{}", exp.abs());' \
  'e-05 printed as e-5'
mutate M6 $C/py_fmt.rs \
  'out.push_str(".0");' \
  '' \
  'an integral float printed without .0'
mutate M7 $C/py_fmt.rs \
  'format!("({})", parts.join(", "))' \
  'format!("({})", parts.join(","))' \
  'tuple repr without the space after the comma'
mutate M8 $C/shtp.rs \
  '(i32::from(packet_byte_count) - HEADER_LEN as i32).max(0) as usize' \
  '(i32::from(packet_byte_count) - HEADER_LEN as i32) as usize' \
  'max(0, count-4) without the clamp'

# --- extra mutations on codec-owned quirks (not in the plan's list) -------
mutate X1 $C/sh2.rs \
  '    separate_batch(packet, pending)?;
    while' \
  '    if let Err(e) = separate_batch(packet, pending) {
        pending.clear();
        return Err(e);
    }
    while' \
  'pending slices cleared on a failed batch (F1)'
mutate X2 $C/shtp.rs \
  'if total > DATA_BUFFER_SIZE {' \
  'if total > DATA_BUFFER_SIZE && false {' \
  'buffer not replaced above 512 (F5)'
mutate X3 $C/py_fmt.rs \
  "' '..='~' => out.push(c)," \
  "' '..='\\u{7f}' => out.push(c)," \
  'DEL not escaped by ensure_ascii'
mutate X4 $C/shtp.rs \
  "                if n > 0 {
                    return Err(PyExc::msg(ExcKind::TypeError, NONE_NOT_SUBSCRIPTABLE));" \
  "                if n > 0 {
                    return Err(PyExc::os(11, \"Resource temporarily unavailable\"));" \
  'read EAGAIN mapped to [Errno 11] instead of the TypeError (F2)'
mutate X5 $C/sh2.rs \
  'readings.0.insert(bytes[1], initial_reading(bytes[1]));' \
  'readings.0.entry(bytes[1]).or_insert_with(|| initial_reading(bytes[1]));' \
  '0xFC re-seed only when absent (F7)'

if ! run_tests /tmp/mut-final.log; then
  echo "FINAL NOT GREEN"
  exit 2
fi
echo "final GREEN"
exit $fail
