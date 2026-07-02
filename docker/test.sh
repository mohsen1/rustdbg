#!/usr/bin/env bash
# Linux smoke test: build the example, drive a real debug session.
set -e
echo "== adapter discovery =="
rdbg-probe() { rdbg status >/dev/null 2>&1; }
cd /src/examples/demo
git init -q && git add -A && git -c user.email=t@t -c user.name=t commit -qm x
cargo build 2>&1 | tail -1
rdbg down >/dev/null 2>&1 || true
echo "== launch + vars =="
rdbg launch --bin-path target/debug/demo --break src/main.rs:12 | grep -E "STOP" | head -1
rdbg vars | head -5
echo "== eval/set/step =="
rdbg set sum = 42
rdbg eval sum
rdbg step over | grep STOP
echo "== panic breakpoint =="
rdbg stop >/dev/null 2>&1 || true
rdbg launch --bin-path target/debug/demo --panic -- --panic | grep -E "STOP" | head -1
rdbg bt | head -4
rdbg down >/dev/null 2>&1 || true
sleep 1
echo "leaked: $(ps -eo comm 2>/dev/null | grep -cE 'lldb|debugserver|rdbg' || echo 0)"
echo "LINUX OK"
