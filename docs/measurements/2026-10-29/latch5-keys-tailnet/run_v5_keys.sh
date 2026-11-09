#!/bin/zsh
# Latch 5 engine on arm-vps streaming the still test pattern (touches no display):
# the Mac presses a key 30 times and times each to the decoded picture, over the tailnet.
OUT=${1:-/tmp/v5-keys}
mkdir -p $OUT
ssh arm-vps 'pkill -x latch-serve; cd /tmp && LATCH_TEST_PATTERN=still setsid nohup /tmp/latch-serve --bind 100.64.0.31 --port 47860 > /tmp/latch-serve.log 2>&1 &'
sleep 1
ssh arm-vps 'python3 /tmp/proc_cpu.py latch-serve 25' > $OUT/arm-vps-cpu.txt 2>&1 &
SP=$!
/Users/dev/Desktop/dev/latch-int/target/release/examples/view 100.64.0.31 --port 47860 --width 1280 --height 720 --fps 60 --kbps 10000 --seconds 10 --keys 30 > $OUT/view.json 2> $OUT/view.log
echo "view exit $?"
wait $SP
ssh arm-vps 'cat /tmp/latch-serve.log; pkill -x latch-serve' > $OUT/serve.log 2>&1
