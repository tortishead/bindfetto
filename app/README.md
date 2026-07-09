# bindfetto control app (Track C)

An Android GUI that drives the bindfetto runtime over its control channel: start/stop
capture, switch sinks, toggle the DLT stream, pick which Binder interfaces to keep, and
(best-effort) deploy the binary.

## Model

An ordinary app can't grant itself root/BPF, so bindfetto runs as a **root daemon**
(started via adb, or attempted from the Deploy tab) and the app controls it over TCP
(default `127.0.0.1:3491`). "Start/stop" toggles capture on that daemon — it doesn't spawn
the process.

```sh
adb shell /data/local/tmp/bindfetto --control 3491 --sink none
```

The app runs on-device and connects to `localhost`. For development from the host the same
protocol is reachable via `adb forward tcp:3491 tcp:3491` and any TCP client.

## Tabs

- **Control** — Connect + a live `STATUS` readout (capturing, sink, DLT, filter size,
  captured/emitted counts); **Start/Stop** capture; a **sink** selector
  (console/logcat/both/none); a **DLT streaming** switch.
- **Filter** (used rarely) — opening the tab enables interface **discovery** (`TRACK on`)
  and loads the observed interfaces as a checkbox list with the active filter pre-checked;
  **Apply** pushes the selection into the in-kernel filter, **Clear** disables it. Leaving
  the tab disables discovery (`TRACK off`) — nothing is tracked until you ask, so the
  runtime carries no discovery overhead otherwise.
- **Deploy** — **Deploy & launch** extracts the bundled binary and tries to place+run it
  via `su`; when the app lacks root/signature privilege it prints the `adb push` fallback
  to run yourself.

Client logic is a thin wrapper over the runtime's line protocol; see
`app/src/main/java/com/bindfetto/control/ControlClient.kt`.

## Build & install

Needs JDK 17+ (Android Studio's bundled JBR works) and the Android SDK. Building the
**runtime** first lets the app bundle the binary for the Deploy tab (a Gradle task copies
`runtime/target/aarch64-linux-android/release/bindfetto` into `jniLibs`); otherwise the
Deploy tab just shows the adb fallback.

```sh
export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
cd app
./gradlew :app:assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Launch **bindfetto control**, tap **Connect**.

## Scope / TODO

Deploy's privileged path can't be exercised on a normal debug build (needs platform
signing or root); it falls back to adb. Still to come: an error-capture toggle (needs the
runtime's `BR_FAILED_REPLY`/`BR_DEAD_REPLY` attach point) and unix-socket + `SO_PEERCRED`
hardening (currently TCP/localhost for testability).
