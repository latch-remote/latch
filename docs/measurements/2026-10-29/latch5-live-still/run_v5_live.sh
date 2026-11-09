#!/bin/zsh
# Latch 5 engine on arm-vps's real :1 desktop (read-only: no input sent, no resize),
# 1280x720 at 60 fps, 10 Mbps, with audio; CPU sampled on both ends.
OUT=${1:-/tmp/v5-live}
mkdir -p $OUT
ssh arm-vps 'pkill -x latch-serve; cd /tmp && DISPLAY=:1 XAUTHORITY=/run/user/$(id -u)/latch.Xauthority setsid nohup /tmp/latch-serve --bind 100.64.0.31 --port 47860 > /tmp/latch-serve.log 2>&1 &'
sleep 1
ssh arm-vps 'python3 /tmp/proc_cpu.py latch-serve 45' > $OUT/arm-vps-cpu.txt 2>&1 &
SP=$!
sleep 1
/Users/dev/Desktop/dev/latch-int/target/release/examples/view 100.64.0.31 --port 47860 --width 1280 --height 720 --fps 60 --kbps 10000 --seconds 30 --audio > $OUT/view.json 2> $OUT/view.log
echo "view exit $?"
wait $SP
ssh arm-vps 'cat /tmp/latch-serve.log; pkill -x latch-serve' > $OUT/serve.log 2>&1
