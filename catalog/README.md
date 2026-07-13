# bindfetto AIDL catalog builder (Track B1)

Turns AIDL into the JSON catalog the offline decoders (CLI, DLT plugin, VS Code
extension) use to resolve raw transaction codes to method names:

```json
{ "android.app.IActivityManager": { "1": "getTasks", "7": "startActivity" } }
```

Pure Python 3 standard library — no dependencies.

## Usage

```sh
# a folder of AIDL (recursed), a single file, or an http(s) URL — mix freely
python3 bindfetto_catalog.py -o catalog.json /path/to/aosp/frameworks/base
python3 bindfetto_catalog.py IActivityManager.aidl
python3 bindfetto_catalog.py https://.../IPowerManager.aidl

# --args: v2 catalog carrying per-method argument types, for decoding parcel payloads
# captured with the runtime's `--parcel`. Entries become
# {"name": "...", "args": [{"name": "...", "type": "..."}]} instead of a bare name;
# the decoder accepts either form.
python3 bindfetto_catalog.py --args -o catalog.json IPowerManager.aidl
```

Then decode with any consumer, e.g.:

```sh
adb logcat -s bindfetto | bindfetto-decode --catalog catalog.json
```

## How codes are assigned

An interface's methods are numbered from `IBinder.FIRST_CALL_TRANSACTION` (1) in
**declaration order** — AIDL's own rule — unless a method fixes its code with a
trailing `= N`:

```aidl
interface IFoo {
    void a();        // 1
    void b() = 10;   // 10 (explicit)
}
```

`const` declarations and nested types (`parcelable`/`enum`/`union`) don't consume
codes. Comments and annotations (including brace args like
`@SuppressWarnings(value={...})`) are stripped before parsing. The interface-agnostic
special transactions (PING/DUMP/INTERFACE/SYSPROPS/SHELL_CMD) are resolved by the
decoder itself, so they are not in the catalog.

**Caveat:** codes are aligned to the AIDL you feed in, so use the AIDL that matches the
device build. Native (non-AIDL) interfaces are out of scope. The parser reads `.aidl`
source directly; it does not need the `aidl` compiler.

## Test

```sh
python3 -m unittest discover -s tests -v
```
