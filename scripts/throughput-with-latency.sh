#!/bin/sh
# Runs the throughput benchmark with network latency, inside a private
# network namespace: only its own loopback gets the delay, never the host's.
# Needs root (ip netns, tc).
#
#   scripts/throughput-with-latency.sh DELAY_MS [MIB] [PIECE_KIB] [DOWNLOADERS]
#
# DELAY_MS is added each way (100 = a 200 ms round trip). Set ONLY to pick
# tests (default "TCP, plain"), BIN to run another build (say, older code
# built in a git worktree), and TCP_BUFFER to raise the kernel's TCP buffer
# limit in the namespace (its default 4 MiB send buffer itself caps one
# connection at high latency).
set -eu
delay=${1:?usage: $0 DELAY_MS [MIB] [PIECE_KIB] [DOWNLOADERS]}
shift
cd "$(dirname "$0")/.."
bin=${BIN:-}
if [ -z "$bin" ]; then
    cargo build -q --release -p empirebt-backend --example throughput
    bin=target/release/examples/throughput
fi
ns=empirebt-bench-$$
ip netns add "$ns"
trap 'ip netns del "$ns"' EXIT
ip netns exec "$ns" ip link set lo up
if [ -n "${TCP_BUFFER:-}" ]; then
    ip netns exec "$ns" sysctl -q -w \
        net.ipv4.tcp_wmem="4096 16384 $TCP_BUFFER" net.ipv4.tcp_rmem="4096 131072 $TCP_BUFFER"
fi
if [ "$delay" != 0 ]; then
    ip netns exec "$ns" tc qdisc add dev lo root netem delay "${delay}ms" limit 100000
fi
ip netns exec "$ns" env ONLY="${ONLY:-TCP, plain}" "$bin" "$@"
