#!/bin/sh
# A fake adb for the Logcat e2e test: one device, a few log lines, a process table.
# Every call is recorded in $FAKE_ADB_LOG so the test can check what was asked.
[ -n "$FAKE_ADB_LOG" ] && echo "$*" >> "$FAKE_ADB_LOG"
case "$*" in
  "devices -l")
    printf 'List of devices attached\nemulator-5554          device product:sdk model:Pixel_8 device:x transport_id:1\n' ;;
  *"shell ps -A -o PID,NAME")
    printf 'PID NAME\n1 init\n4242 com.demo.app\n777 system_server\n' ;;
  *"shell pm list packages "*)
    printf 'package:com.demo.app.debug\npackage:com.demo.app.debug.test\n' ;;
  *"resolve-activity"*)
    printf 'priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\ncom.demo.app.debug/com.demo.MainActivity\n' ;;
  *"am start"*"PreviewActivity"*)
    if [ -n "$FAKE_NO_PREVIEW" ]; then
      printf 'Error type 3\nError: Activity class {com.demo.app.debug/androidx.compose.ui.tooling.PreviewActivity} does not exist.\n'
    else
      printf 'Starting: Intent { cmp=com.demo.app.debug/androidx.compose.ui.tooling.PreviewActivity }\n'
    fi ;;
  *"am start"*)
    printf 'Starting: Intent { cmp=com.demo.app.debug/com.demo.MainActivity }\n' ;;
  *"exec-out screencap -p")
    # a 1x1 PNG
    printf '%s' 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==' | base64 --decode ;;
  *"exec screenrecord"*)
    # like the real one: print the pid, then record until SIGINT
    echo $$
    exec sleep 30 ;;
  *"shell kill -INT "*)
    kill -INT "${6}" ;;
  *" pull "*)
    printf 'fake mp4' > "${5}" ;;
  *"shell rm -f "*) ;;
  *"logcat"*)
    printf '%s\n' \
      '--------- beginning of main' \
      '11-12 10:00:00.001  4242  4242 D DemoNet : fast call' \
      '11-12 10:00:00.002  4242  4243 W DemoNet : slow call' \
      '11-12 10:00:00.003   777   800 E ActivityManager: ANR in com.other' \
      '11-12 10:00:00.004  4242  4242 E AndroidRuntime: FATAL EXCEPTION: main' \
      '11-12 10:00:00.005  4242  4242 E AndroidRuntime: 	at com.demo.ui.UserCardKt.UserCard(UserCard.kt:7)'
    exec sleep 30 ;;
esac
