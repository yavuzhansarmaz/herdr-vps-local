#!/bin/bash
# Measures Mutagen sync latency between a local dir and an SSH endpoint.
# Requires an existing Mutagen session already syncing the two dirs.
#
# Usage: latency.sh <local-dir> <ssh-target> <remote-dir> [samples]
#   remote-dir may reference $HOME (expanded on the REMOTE side).
#
# Example:
#   ./tests/latency.sh /tmp/sync-test pizero '$HOME/sync-test' 5
#
# Methodology:
#   - SSH baseline RTT measured first (5x `ssh <target> true`).
#   - local->remote: write locally at T0, one blocking SSH that returns when
#     the file appears remotely at T1. Adjusted latency = (T1-T0) - RTT.
#     (Steady-state Mutagen uses a persistent agent connection, so real
#     propagation does not pay SSH setup per change; the adjustment
#     approximates that.)
#   - remote->local: write via SSH, T0 taken right after it returns (bytes
#     are on remote disk), then poll locally (no SSH in the loop).
#     Raw result ~= sync latency.
set -u

if [ $# -lt 3 ]; then
  echo "usage: latency.sh <local-dir> <ssh-target> <remote-dir> [samples]" >&2
  exit 2
fi
LOCAL_DIR=$1
TARGET=$2
REMOTE_DIR=$3
N=${4:-5}

ms_now() { echo $(( $(date +%s%N) / 1000000 )); }

echo "--- SSH baseline RTT (5 samples) ---"
rtts=()
for i in $(seq 1 5); do
  t0=$(ms_now); ssh -o BatchMode=yes "$TARGET" true; t1=$(ms_now)
  rtts+=($((t1-t0)))
done
echo "RTT_MS: ${rtts[*]}"
RTT=$(printf "%s\n" "${rtts[@]}" | awk '{s+=$1} END {print int(s/NR)}')
echo "RTT_AVG_MS: $RTT"

echo "--- local->remote (small file, $N samples, raw ms) ---"
for i in $(seq 1 $N); do
  f="l2r_$i.txt"; content="payload-$i-$RANDOM"
  t0=$(ms_now)
  echo "$content" > "$LOCAL_DIR/$f"
  ssh -o BatchMode=yes "$TARGET" "for n in \$(seq 1 400); do [ \"\$(cat $REMOTE_DIR/$f 2>/dev/null)\" = \"$content\" ] && break; sleep 0.05; done"
  t1=$(ms_now)
  echo "L2R_$i: raw=$((t1-t0)) adjusted=$((t1-t0-RTT))"
done

echo "--- remote->local (small file, $N samples, ms) ---"
for i in $(seq 1 $N); do
  f="r2l_$i.txt"; content="payload-$i-$RANDOM"
  ssh -o BatchMode=yes "$TARGET" "echo '$content' > $REMOTE_DIR/$f"
  t0=$(ms_now)
  for n in $(seq 1 400); do
    [ "$(cat $LOCAL_DIR/$f 2>/dev/null)" = "$content" ] && break
    sleep 0.05
  done
  t1=$(ms_now)
  echo "R2L_$i: $((t1-t0))"
done

echo "--- 1MB file (1 sample per direction) ---"
head -c 1048576 /dev/urandom | base64 | head -c 1048576 > "$LOCAL_DIR/big_l2r.bin"
t0=$(ms_now)
ssh -o BatchMode=yes "$TARGET" "for n in \$(seq 1 400); do [ \"\$(stat -c%s $REMOTE_DIR/big_l2r.bin 2>/dev/null || echo 0)\" -eq 1048576 ] && break; sleep 0.05; done"
t1=$(ms_now)
echo "L2R_BIG: raw=$((t1-t0)) adjusted=$((t1-t0-RTT))"
ssh -o BatchMode=yes "$TARGET" "head -c 1048576 /dev/urandom | base64 | head -c 1048576 > $REMOTE_DIR/big_r2l.bin"
t0=$(ms_now)
for n in $(seq 1 400); do
  [ -f "$LOCAL_DIR/big_r2l.bin" ] && [ "$(stat -c%s $LOCAL_DIR/big_r2l.bin 2>/dev/null || echo 0)" -eq 1048576 ] && break
  sleep 0.05
done
t1=$(ms_now)
echo "R2L_BIG: $((t1-t0))"
echo DONE
