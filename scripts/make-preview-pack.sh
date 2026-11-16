#!/bin/sh
# Builds Droid Lite's preview base pack in ~/Library/Caches/droid-lite/previews/base (or
# $DROID_LITE_PREVIEW_DIR/base) as real files. One Gradle run fetches the libraries; previews never
# run Gradle. Needs a Gradle 9 install ($GRADLE, else the newest in ~/.gradle/wrapper/dists) and the
# Android SDK's android-35 platform.
set -eu
cd "$(dirname "$0")/preview-pack"
G=${GRADLE:-$(ls -d "$HOME"/.gradle/wrapper/dists/gradle-9*-bin/*/gradle-*/bin/gradle 2>/dev/null | sort -V | tail -1)}
SDK=${ANDROID_HOME:-$HOME/Library/Android/sdk}
[ -x "$G" ] || { echo "No Gradle 9 found: set GRADLE"; exit 1; }
[ -f "$SDK/platforms/android-35/android.jar" ] || { echo "No android-35 platform in $SDK"; exit 1; }
echo "sdk.dir=$SDK" > local.properties
"$G" -p . --no-daemon -q :app:packLists :worker:packRuntime :worker:packLayoutlibRuntime \
    :worker:packLayoutlibResources :worker:packComposePlugin
BASE=${DROID_LITE_PREVIEW_DIR:-$HOME/Library/Caches/droid-lite/previews}/base
NEW="$BASE.new"
rm -rf "$NEW"
mkdir -p "$NEW/compose" "$NEW/compile" "$NEW/layoutlib"
cp -R worker/build/pack/runtime "$NEW/runtime"
cp -R worker/build/pack/layoutlib/runtime worker/build/pack/layoutlib/resources "$NEW/layoutlib/"
cp worker/build/pack/compose-plugin.jar "$NEW/"
cp "$SDK/platforms/android-35/android.jar" "$NEW/android.jar"
# files compile against it too; named so it sorts last (a hard link: no second copy on disk)
ln "$NEW/android.jar" "$NEW/compile/zz-android.jar"
# AGP's unpacked AAR jars are all named classes.jar: number them
n=0; while read -r j; do n=$((n + 1)); cp "$j" "$NEW/compose/rt$(printf %03d $n)-$(basename "$j")"; done < app/build/pack/compose.txt
n=0; while read -r j; do n=$((n + 1)); cp "$j" "$NEW/compile/ct$(printf %03d $n)-$(basename "$j")"; done < app/build/pack/compile.txt
R=$(find app/build/intermediates -name R.jar | head -1)
cp "$R" "$NEW/compose/r-libraries-R.jar"
cp "$R" "$NEW/compile/r-libraries-R.jar"
# swap in the new pack only when it is complete; the worker's own source is written at start
rm -rf "$BASE.old"
if [ -e "$BASE" ]; then mv "$BASE" "$BASE.old"; fi
mv "$NEW" "$BASE"
rm -rf "$BASE.old"
echo "base pack: $(ls "$BASE/runtime" | wc -l | tr -d ' ') runtime, $(ls "$BASE/compose" | wc -l | tr -d ' ') compose, $(ls "$BASE/compile" | wc -l | tr -d ' ') compile jars, $(du -sh "$BASE" | cut -f1)"
