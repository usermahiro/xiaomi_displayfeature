# Xiaomi DisplayFeature AIDL HAL Mock (HyperOS / Transsion MT6789)

A standalone Rust-based AIDL HAL mock service implementing `vendor.xiaomi.hardware.displayfeature_aidl.IDisplayFeature/default` for non-Xiaomi devices (such as TECNO / Infinix / Transsion MT6789 / Helio G99) running ported Xiaomi HyperOS 1.0 (Android 14/15).

## Overview
When running a Xiaomi HyperOS / MIUI ROM on non-Xiaomi hardware, framework components (`DisplayFeatureManager`, `MiuiRefreshRatePolicy`, `com.miui.powerkeeper`, `com.xiaomi.misettings`) constantly poll and invoke the proprietary `vendor.xiaomi.hardware.displayfeature` HAL. On unsupported hardware:
1. Framework queries flood `logcat` with retry loops and binder failures.
2. The refresh rate menu in Settings locks up or crashes.
3. Refresh rate changes selected in `MiSettings` fail to bind to SurfaceFlinger / MediaTek HWC.
4. Gesture navigation animations from `PowerKeeper` (`cookie=244`) forcibly revert 60Hz selections back to 120Hz.

This mock service solves these issues natively by registering with Android `servicemanager` and implementing the exact AIDL interface hash and transactions.

---

## Technical Specifications
- **Service Name**: `vendor.xiaomi.hardware.displayfeature_aidl.IDisplayFeature/default`
- **AIDL Interface Version**: `2`
- **Interface Hash**: `aff8a2e5c8a611565115fd1a30bb902f64dd336f`
- **Architecture**: `aarch64-linux-android` (API 33+)
- **Runtime**: Native Rust with `rsbinder` (Binder driver IPC)

### AIDL Transaction Mapping
| Method | Code | Parameters | Description |
|---|---|---|---|
| `notifyBrightness` | 1 | `(int brightness)` | Brightness change notifications |
| `registerCallback` | 2 | `(int displayId, IDisplayFeatureCallback callback)` | Framework callback registration |
| `sendMessage` | 3 | `(int displayId, int mode, String message)` | One-way display commands |
| `sendPanelCommand` | 4 | `(String command)` | Panel OEM commands |
| `sendPostProcCommand` | 5 | `(int displayId, int command)` | Post-processing effects |
| `sendRefreshCommand`| 6 | `()` | Refresh command trigger |
| `setFeature` | 7 | `(int displayId, int mode, int value, int cookie)` | Primary mode switch (mode 24 = DFPS / refresh rate) |
| `setFunction` | 8 | `(int displayId, int mode, int value, int cookie)` | Display function configuration |

---

## Key Features & Refresh Rate Bridging
1. **Dynamic vs. Locked Refresh Rate**:
   - **Auto Mode** (`is_smart_fps == 1`): Sets `peak=120.0`, `min=60.0`, allowing PowerKeeper animation boosts (`cookie=244`) for smooth adaptive refresh rates.
   - **Custom 60Hz Mode**: Sets both `peak=60.0` and `min=60.0`, gating PowerKeeper boosts to prevent gesture animations from overriding 60Hz.
   - **Custom 120Hz Mode**: Sets both `peak=120.0` and `min=120.0`, preventing MediaTek HWC from dropping frames while idle.
   - **144Hz panels**: user selection up to 144Hz is persisted and restored at boot.
2. **Sequential Worker Queue**:
   - Rapid consecutive taps (e.g. 60 $\leftrightarrow$ 120) drain intermediate backlog events to apply only the final target state, completely preventing thread race conditions.
3. **Atomic Multi-Database Sync**:
   - Applies `Settings.System.peak_refresh_rate`, `Settings.System.min_refresh_rate`, `Settings.Secure.user_refresh_rate`, `Settings.System.user_refresh_rate`, `Settings.Secure.miui_refresh_rate`, and `persist.vendor.dfps.level` in a single execution.
4. **AIDL Callbacks**:
   - Dispatches `displayfeatureInfoChanged` on every switch so `MiSettings` UI and framework stay acknowledged.

---

## Screen Effects (mode 2 / 3 / 31) — ColorDisplayManager Bridge

The framework's `DisplayFeatureManagerService` sends animated `setFeature` ramps (~200 calls per toggle). The mock coalesces them with a 150 ms debounce window and translates the final state into `IColorDisplayManager` (`color_display` service) binder calls — executed as root uid 0 via `/system/bin/service`, which satisfies the `CONTROL_DISPLAY_COLOR_TRANSFORMS` signature permission.

| `setFeature` mode | MIUI feature | Value semantics | Panel effect |
|---|---|---|---|
| `24` | DFPS / refresh rate | Hz (cookie 255 = user selection) | peak/min refresh rate batch write |
| `3` | Reading / paper mode | CT level 0 = off, 0..255 = strength | `setNightDisplayActivated` + `setNightDisplayColorTemperature` (6500K → 2500K warm paper tint) |
| `2` | Color scheme level | 0 / 1 / 2 | `setSaturationLevel(100/85/70)` |
| `31` | Paper color type (hitam-putih / grayscale) | 2 = grayscale, 0 = off | `setSaturationLevel(0)` |

Verified `IColorDisplayManager` transaction codes (AOSP `IColorDisplayManager.aidl` order):
`2`=setSaturationLevel, `7`=setNightDisplayActivated, `9`=setNightDisplayColorTemperature.

`setColorMode` (code 18) is rejected by MIUI's `ColorDisplayService` ("Invalid colorMode") and therefore not used.

---

## Build Instructions

### GitHub Actions (recommended)
Push to GitHub — `.github/workflows/build.yml` builds automatically and uploads the stripped aarch64 binary as an artifact. Tag `v*` triggers a GitHub Release.

### Manual (Linux)
Requirements:
- Android NDK r26b+ (r27c tested)
- Rust `aarch64-linux-android` target (`rustup target add aarch64-linux-android`)

```sh
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android33-clang
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar
cargo build --target aarch64-linux-android --release
```

### Manual (Windows)
Requirements:
- MinGW / w64devkit in PATH

Run the build script:
```powershell
powershell -ExecutionPolicy Bypass -File build.ps1
```
The output flashable Magisk/KernelSU module will be generated at:
`out/` and packaged as `HyperOS_DisplayFeature_Mock.zip`.

---

## Installation
### Via KernelSU / Magisk:
1. Flash `HyperOS_DisplayFeature_Mock.zip` via the KernelSU or Magisk Manager app.
2. Reboot the device.
3. Verify the service is registered:
   ```sh
   su -c 'service check vendor.xiaomi.hardware.displayfeature_aidl.IDisplayFeature/default'
   ```
