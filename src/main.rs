#![allow(non_snake_case)]

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
use xiaomi_displayfeature_mock::{INTERFACE_HASH, INTERFACE_VERSION, SERVICE_NAME};

include!(concat!(env!("OUT_DIR"), "/displayfeature.rs"));

use crate::vendor::xiaomi::hardware::displayfeature_aidl::IDisplayFeature::{BnDisplayFeature, IDisplayFeature};
use crate::vendor::xiaomi::hardware::displayfeature_aidl::IDisplayFeatureCallback::IDisplayFeatureCallback;

type BinderResult<T> = rsbinder::status::Result<T>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RateCommand {
    UserSelected(i32),
    Boost(i32, i32), // value, cookie
}

/// Screen-effect commands routed through IColorDisplayManager (service `color_display`).
///
/// AIDL transaction codes verified on-device (IColorDisplayManager.aidl order):
///   2 = setSaturationLevel(int)         [0 = grayscale, 100 = normal]
///   7 = setNightDisplayActivated(bool)
///   9 = setNightDisplayColorTemperature(int, Kelvin)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreenCommand {
    /// MIUI color-scheme / unlimited-color value.
    /// Presets 1=warm, 2=nature, 3=cool; custom values are packed ARGB
    /// (ScreenColorAlgorithm.encode of the picked bitmap pixel).
    ColorPacked(i32),
    /// mode=3: reading/paper mode CT level (0 = off, >0 = warm tint strength).
    ReadingCt(i32),
    /// mode=31: paper color type — 2 = grayscale (hitam-putih), 0 = off.
    PaperColorType(i32),
}
const COLOR_DISPLAY_SERVICE: &str = "color_display";
const TX_SET_SATURATION: u32 = 2;
const TX_SET_NIGHT_ACTIVE: u32 = 7;
const TX_SET_NIGHT_TEMP: u32 = 9;

/// MIUI's `screen_optimize_mode` 1/2/3/4 reaches the HAL as setFeature
/// mode 0/1/2/23. Each maps to a different native SurfaceFlinger colour mode.
fn native_color_mode(mode: i32) -> i32 {
    match mode {
        0 => 1, // Vivid / adapt
        1 => 2, // Enhanced
        2 => 0, // Original colour PRO / standard
        _ => 0, // custom colour level
    }
}

/// Mirror DisplayFeatureManagerService.notifySFColorMode(): the framework
/// writes display_color_mode, sets the native-mode property, then sends
/// transaction 31023 to SurfaceFlinger. Doing it here keeps Vivid /
/// Original working even when no framework callback is registered.
fn notify_sf_color_mode(mode: i32) {
    let script = format!(
        "/system/bin/settings put system display_color_mode {m} ; \
         /system/bin/setprop persist.sys.sf.native_mode {m} ; \
         /system/bin/service call SurfaceFlinger 31023 i32 {m}",
        m = mode
    );
    if let Err(e) = std::process::Command::new("/system/bin/sh")
        .args(["-c", &script])
        .status()
    {
        tracing::warn!("[DisplayFeature] notifySFColorMode({}) failed: {}", mode, e);
    }
}

static WORKER_TX: RwLock<Option<Sender<RateCommand>>> = RwLock::new(None);
static SCREEN_TX: RwLock<Option<Sender<ScreenCommand>>> = RwLock::new(None);
static USER_TARGET_FPS: AtomicI32 = AtomicI32::new(120);
/// Debounce window for coalescing the framework's animated setFeature ramp
/// (~200 calls per toggle) into a single IColorDisplayManager transaction.
const SCREEN_DEBOUNCE_MS: u64 = 150;

fn init_logging() {
    let _ = tracing_log::LogTracer::init();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init();
}

fn is_smart_fps_mode() -> bool {
    if let Ok(output) = std::process::Command::new("/system/bin/settings")
        .args(["get", "system", "is_smart_fps"])
        .output()
    {
        if let Ok(text) = String::from_utf8(output.stdout) {
            return text.trim() == "1";
        }
    }
    false
}

/// Only pin the minimum to the requested rate when the user explicitly wants
/// 60Hz: that is what stops PowerKeeper boosts from reverting to 120Hz.
/// For any higher rate the minimum must stay at 60 so the panel can idle at
/// 60Hz; locking it at 120 keeps the GPU/CPU hot and causes thermal lag.
fn min_refresh_rate(hz: i32, is_smart: bool) -> i32 {
    if is_smart || hz > 60 {
        60
    } else {
        hz
    }
}

fn apply_settings_batch(hz: i32, is_user_selection: bool) {
    let is_smart = is_smart_fps_mode();
    let min_hz = min_refresh_rate(hz, is_smart);

    let peak_str = format!("{}.0", hz);
    let min_str = format!("{}.0", min_hz);
    let hz_str = hz.to_string();

    tracing::info!(
        "[DisplayFeature-Worker] Applying display config: peak={}, min={}, smart_fps={}, user_selection={}",
        peak_str, min_str, is_smart, is_user_selection
    );

    if is_user_selection {
        // Atomic batch update in a single shell session to eliminate process spawn overhead and races
        let script = format!(
            "/system/bin/settings put system peak_refresh_rate {peak} ; \
             /system/bin/settings put system min_refresh_rate {min} ; \
             /system/bin/settings put secure user_refresh_rate {hz} ; \
             /system/bin/settings put system user_refresh_rate {hz} ; \
             /system/bin/settings put secure miui_refresh_rate {hz} ; \
             /system/bin/setprop persist.vendor.dfps.level {hz}",
            peak = peak_str,
            min = min_str,
            hz = hz_str
        );
        let _ = std::process::Command::new("/system/bin/sh")
            .args(["-c", &script])
            .status();
    } else {
        // Gesture / dynamic boost only modulates hardware rates
        let script = format!(
            "/system/bin/settings put system peak_refresh_rate {peak} ; \
             /system/bin/settings put system min_refresh_rate {min}",
            peak = peak_str,
            min = min_str
        );
        let _ = std::process::Command::new("/system/bin/sh")
            .args(["-c", &script])
            .status();
    }
}

fn init_rate_worker() {
    let (tx, rx) = channel::<RateCommand>();
    *WORKER_TX.write() = Some(tx);

    std::thread::spawn(move || {
        let mut last_applied_peak = -1;

        while let Ok(mut cmd) = rx.recv() {
            // Drain queue so rapid consecutive toggles (e.g. 60 -> 120) apply only the final state
            while let Ok(next) = rx.try_recv() {
                cmd = next;
            }

            match cmd {
                RateCommand::UserSelected(hz) => {
                    if hz <= 0 {
                        continue;
                    }
                    USER_TARGET_FPS.store(hz, Ordering::SeqCst);
                    apply_settings_batch(hz, true);
                    last_applied_peak = hz;
                }
                RateCommand::Boost(hz, cookie) => {
                    if hz <= 0 {
                        continue;
                    }
                    let is_smart = is_smart_fps_mode();
                    let user_target = USER_TARGET_FPS.load(Ordering::SeqCst);

                    // When locked at 60Hz in Custom mode, gate/block boost to prevent reverting to 120Hz
                    if !is_smart && user_target == 60 && hz > 60 {
                        tracing::debug!(
                            "[DisplayFeature-Worker] Gated boost ({}Hz, cookie={}) - user locked at 60Hz",
                            hz, cookie
                        );
                        continue;
                    }

                    if hz != last_applied_peak {
                        apply_settings_batch(hz, false);
                        last_applied_peak = hz;
                    }
                }
            }
        }
    });
}

fn request_refresh_rate(cmd: RateCommand) {
    if let Some(ref tx) = *WORKER_TX.read() {
        let _ = tx.send(cmd);
    }
}

/// Run `/system/bin/service call color_display <tx> <args...>` as the HAL's root uid.
fn color_display_call(args: &[&str]) {
    let mut argv = vec![
        "call".to_string(),
        COLOR_DISPLAY_SERVICE.to_string(),
    ];
    argv.extend(args.iter().map(|s| s.to_string()));
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    match std::process::Command::new("/system/bin/service").args(&argv_refs).output() {
        Ok(out) => {
            tracing::info!(
                "[DisplayFeature-Color] service call {} {:?} -> {}",
                COLOR_DISPLAY_SERVICE,
                args,
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        Err(e) => tracing::warn!("[DisplayFeature-Color] service call failed: {}", e),
    }
}

/// Map MIUI reading-mode CT level (0..255) to Kelvin: 6500K (neutral) .. 2500K (warm paper).
fn ct_level_to_kelvin(level: i32) -> i32 {
    let level = level.clamp(0, 255);
    6500 - ((level as i64 * 4000) / 255) as i32
}

// Shared saturation state: grayscale (mode=31) overrides color mode.
static GRAYSCALE_ON: AtomicI32 = AtomicI32::new(0);

fn push_saturation(normal: i32) {
    let sat = if GRAYSCALE_ON.load(Ordering::SeqCst) > 0 {
        0
    } else {
        normal.clamp(0, 100)
    };
    let sat_str = sat.to_string();
    color_display_call(&[&TX_SET_SATURATION.to_string(), "i32", &sat_str]);
}

fn srgb_to_linear(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn rgb_to_lab(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (rl, gl, bl) = (srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b));
    let x = (0.4124564 * rl + 0.3575761 * gl + 0.1804375 * bl) / 0.950456;
    let y = 0.2126729 * rl + 0.7151522 * gl + 0.072175 * bl;
    let z = (0.0193339 * rl + 0.119192 * gl + 0.9503041 * bl) / 1.088754;
    let f = |t: f64| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            t * 7.787 + 0.137931
        }
    };
    (116.0 * f(y) - 16.0, 500.0 * (f(x) - f(y)), 200.0 * (f(y) - f(z)))
}

fn lab_to_rgb(l: f64, a: f64, b: f64) -> (u8, u8, u8) {
    let fy = (l + 16.0) / 116.0;
    let fx = a / 500.0 + fy;
    let fz = fy - b / 200.0;
    let f = |t: f64| {
        if t > 0.206896552 {
            t * t * t
        } else {
            (t - 0.137931) / 7.787
        }
    };
    let x = f(fx) * 0.950456;
    let y = f(fy);
    let z = f(fz) * 1.088754;
    let r = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z;
    let g = -0.969266 * x + 1.8760108 * y + 0.041556 * z;
    let b = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z;
    let enc = |c: f64| {
        let c = c.max(0.0).min(1.0);
        let c = if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (c * 255.0).round() as u8
    };
    (enc(r), enc(g), enc(b))
}

/// Inverse of MIUI's ScreenColorAlgorithm.encode(): recover the picked pixel
/// from the encoded `screen_color_level`.
fn decode_color(value: i32) -> (u8, u8, u8) {
    let argb = value as u32;
    let r = ((argb >> 16) & 0xff) as u8;
    let g = ((argb >> 8) & 0xff) as u8;
    let b = (argb & 0xff) as u8;
    let (l, a, bb) = rgb_to_lab(r, g, b);
    let angle = bb.atan2(a);
    let magnitude = (a * a + bb * bb).sqrt();
    let expanded = (magnitude / 7.0).exp() - 1.0;
    let a2 = (angle.cos() * expanded).clamp(-128.0, 128.0);
    let b2 = (angle.sin() * expanded).clamp(-128.0, 128.0);
    lab_to_rgb(l, a2, b2)
}

fn color_saturation(green_axis: f64) -> i32 {
    if green_axis < -8.0 {
        (100.0 - ((-green_axis) / 6.0).min(20.0)).round() as i32
    } else {
        100
    }
}

/// MIUI's custom color picker stores a packed Lab value in
/// `screen_color_level`. Only two controls exist on this port: NightDisplay
/// (2596..4082K, yellow/blue axis) and saturation (0..100). Use the decoded
/// blue-yellow axis for temperature and the red-green axis for saturation,
/// so more of the wheel produces a visible change than r-b alone did.
fn apply_color_packed(value: i32) {
    let (delta, green_axis) = if (0..=3).contains(&value) {
        match value {
            1 => (60.0, 0.0),  // warm preset
            3 => (-60.0, 0.0), // cool preset
            _ => (0.0, 0.0),   // natural / default
        }
    } else {
        let (r, g, b) = decode_color(value);
        let (_, a, bb) = rgb_to_lab(r, g, b);
        (bb as f64, a as f64)
    };

    // Green/cyan side: reduce saturation so that half of the wheel is not
    // identical to neutral. Clamped so it never goes grayscale here.
    let normal_sat = color_saturation(green_axis);
    push_saturation(normal_sat);

    let active_tx = TX_SET_NIGHT_ACTIVE.to_string();
    if delta.abs() <= 3.0 {
        tracing::info!(
            "[DisplayFeature-Color] ColorPacked({}) -> natural, NightDisplay OFF (sat={})",
            value,
            normal_sat
        );
        color_display_call(&[&active_tx, "i32", "0"]);
        return;
    }

    let kelvin = (6500i32 - (delta.round() as i32).saturating_mul(100)).clamp(2500, 9000);
    tracing::info!(
        "[DisplayFeature-Color] ColorPacked({}) -> NightDisplay {}K (b*={:.0}, a*={:.0}, sat={})",
        value, kelvin, delta, green_axis, normal_sat
    );
    let temp = kelvin.to_string();
    // Set temperature before activation so the tint appears in one frame.
    color_display_call(&[&TX_SET_NIGHT_TEMP.to_string(), "i32", &temp]);
    color_display_call(&[&active_tx, "i32", "1"]);
}

fn apply_paper_color_type(value: i32) {
    GRAYSCALE_ON.store(if value > 0 { 1 } else { 0 }, Ordering::SeqCst);
    push_saturation(100);
}

fn apply_reading_ct(level: i32) {
    if level <= 0 {
        tracing::info!("[DisplayFeature-Color] ReadingCt {} -> NightDisplay OFF", level);
        color_display_call(&[&TX_SET_NIGHT_ACTIVE.to_string(), "i32", "0"]);
    } else {
        let temp = ct_level_to_kelvin(level);
        tracing::info!(
            "[DisplayFeature-Color] ReadingCt {} -> NightDisplay ON @ {}K",
            level, temp
        );
        let temp_str = temp.to_string();
        // Set temperature first, then activate, so the tint appears in one frame.
        color_display_call(&[&TX_SET_NIGHT_TEMP.to_string(), "i32", &temp_str]);
        color_display_call(&[&TX_SET_NIGHT_ACTIVE.to_string(), "i32", "1"]);
    }
}

fn init_screen_worker() {
    let (tx, rx) = channel::<ScreenCommand>();
    *SCREEN_TX.write() = Some(tx);

    std::thread::spawn(move || {
        let mut pending: Option<ScreenCommand> = None;
        let mut last_applied: Option<ScreenCommand> = None;

        loop {
            // Wait for the first command, then keep draining inside the debounce window
            // so the framework's animated setFeature ramp (0..114..0, ~200 calls) only
            // triggers a single IColorDisplayManager transaction.
            let cmd = match pending.take() {
                Some(c) => c,
                None => {
                    let mut first = match rx.recv() {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    // Drain everything already queued
                    while let Ok(next) = rx.try_recv() {
                        first = next;
                    }
                    first
                }
            };

            // Coalesce window: collect any further commands within SCREEN_DEBOUNCE_MS
            let mut final_cmd = cmd;
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(SCREEN_DEBOUNCE_MS)) {
                    Ok(next) => final_cmd = next,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }

            if Some(&final_cmd) != last_applied.as_ref() {
                match final_cmd {
                    ScreenCommand::ColorPacked(v) => apply_color_packed(v),
                    ScreenCommand::ReadingCt(v) => apply_reading_ct(v),
                    ScreenCommand::PaperColorType(v) => apply_paper_color_type(v),
                }
                last_applied = Some(final_cmd);
            } else {
                tracing::debug!(
                    "[DisplayFeature-Color] Skipping duplicate state {:?}",
                    final_cmd
                );
            }
        }
    });
}

fn request_screen_effect(cmd: ScreenCommand) {
    if let Some(ref tx) = *SCREEN_TX.read() {
        let _ = tx.send(cmd);
    }
}

fn read_initial_user_fps() -> i32 {
    if let Ok(output) = std::process::Command::new("/system/bin/settings")
        .args(["get", "secure", "user_refresh_rate"])
        .output()
    {
        if let Ok(text) = String::from_utf8(output.stdout) {
            if let Ok(val) = text.trim().parse::<i32>() {
                if val == 60 || val == 90 || val == 120 || val == 144 {
                    tracing::info!("[DisplayFeature] Restored user_refresh_rate from Settings: {}Hz", val);
                    return val;
                }
            }
        }
    }
    120
}

struct DisplayFeatureService {
    callbacks: Arc<Mutex<Vec<rsbinder::Strong<dyn IDisplayFeatureCallback>>>>,
}

impl DisplayFeatureService {
    fn new() -> Self {
        Self {
            callbacks: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl rsbinder::Interface for DisplayFeatureService {}

impl IDisplayFeature for DisplayFeatureService {
    // 1: notifyBrightness
    fn r#notifyBrightness(&self, brightness: i32) -> BinderResult<()> {
        tracing::debug!("[DisplayFeature] notifyBrightness: {}", brightness);
        Ok(())
    }

    // 2: registerCallback
    fn r#registerCallback(&self, displayId: i32, callback: &rsbinder::Strong<dyn IDisplayFeatureCallback>) -> BinderResult<()> {
        tracing::info!("[DisplayFeature] registerCallback for displayId: {}", displayId);
        self.callbacks.lock().push(callback.clone());
        Ok(())
    }

    // 3: sendMessage
    fn r#sendMessage(&self, displayId: i32, mode: i32, message: &str) -> BinderResult<()> {
        tracing::info!("[DisplayFeature] sendMessage: displayId={}, mode={}, message={}", displayId, mode, message);
        Ok(())
    }

    // 4: sendPanelCommand
    fn r#sendPanelCommand(&self, command: &str) -> BinderResult<()> {
        tracing::info!("[DisplayFeature] sendPanelCommand: {}", command);
        Ok(())
    }

    // 5: sendPostProcCommand
    fn r#sendPostProcCommand(&self, displayId: i32, command: i32) -> BinderResult<()> {
        tracing::debug!("[DisplayFeature] sendPostProcCommand: displayId={}, command={}", displayId, command);
        Ok(())
    }

    // 6: sendRefreshCommand
    fn r#sendRefreshCommand(&self) -> BinderResult<()> {
        tracing::debug!("[DisplayFeature] sendRefreshCommand");
        Ok(())
    }

    // 7: setFeature
    fn r#setFeature(&self, displayId: i32, mode: i32, value: i32, cookie: i32) -> BinderResult<()> {
        tracing::info!(
            "[DisplayFeature] setFeature: displayId={}, mode={}, value={}, cookie={}",
            displayId, mode, value, cookie
        );

        if mode == 24 {
            // mode 24 = DFPS / Refresh Rate
            if cookie == 255 {
                // Explicit user selection in MiSettings (60, 90, 120, etc.)
                tracing::info!("[DisplayFeature] User selected refresh rate: {}Hz (cookie=255)", value);
                request_refresh_rate(RateCommand::UserSelected(value));
            } else {
                // Boost or automated framework switch (PowerKeeper=244, etc.)
                request_refresh_rate(RateCommand::Boost(value, cookie));
            }

            // Notify registered callbacks so UI & framework know mode switch succeeded
            let callbacks = self.callbacks.lock().clone();
            for cb in callbacks {
                let _ = cb.r#displayfeatureInfoChanged(displayId, mode, value as f32, 0.0, 0.0);
            }
        } else if mode == 0 || mode == 1 || mode == 2 || mode == 23 {
            // MIUI colour-scheme modes. `value` is the colour-temperature
            // level (screen_color_level); the mode itself selects Vivid /
            // Enhanced / Original and must be forwarded to SurfaceFlinger.
            request_screen_effect(ScreenCommand::ColorPacked(value));

            notify_sf_color_mode(native_color_mode(mode));
            let callbacks = self.callbacks.lock().clone();
            for cb in callbacks {
                let _ = cb.r#displayfeatureInfoChanged(displayId, mode, value as f32, 0.0, 0.0);
            }
        } else if mode == 31 {
            // mode 31 = paper color type (2 = grayscale/hitam-putih, 0 = off)
            request_screen_effect(ScreenCommand::PaperColorType(value));
            let callbacks = self.callbacks.lock().clone();
            for cb in callbacks {
                let _ = cb.r#displayfeatureInfoChanged(displayId, mode, value as f32, 0.0, 0.0);
            }
        } else if mode == 3 {
            // mode 3 = reading / paper mode CT level (0 = off, ramp 0..255 animated)
            request_screen_effect(ScreenCommand::ReadingCt(value));
            let callbacks = self.callbacks.lock().clone();
            for cb in callbacks {
                let _ = cb.r#displayfeatureInfoChanged(displayId, mode, value as f32, 0.0, 0.0);
            }
        }

        Ok(())
    }

    // 8: setFunction
    fn r#setFunction(&self, displayId: i32, mode: i32, value: i32, cookie: i32) -> BinderResult<()> {
        tracing::debug!(
            "[DisplayFeature] setFunction: displayId={}, mode={}, value={}, cookie={}",
            displayId, mode, value, cookie
        );
        Ok(())
    }

    // Metadata
    fn r#getInterfaceVersion(&self) -> BinderResult<i32> {
        Ok(INTERFACE_VERSION)
    }

    fn r#getInterfaceHash(&self) -> BinderResult<String> {
        Ok(INTERFACE_HASH.to_string())
    }
}

fn main() -> anyhow::Result<()> {
    init_logging();
    tracing::info!("Starting vendor.xiaomi.hardware.displayfeature mock service...");

    init_rate_worker();
    init_screen_worker();

    let initial_fps = read_initial_user_fps();
    request_refresh_rate(RateCommand::UserSelected(initial_fps));

    rsbinder::ProcessState::init_default().map_err(|error| {
        anyhow::anyhow!("failed to initialize Binder process state: {error}")
    })?;
    rsbinder::ProcessState::start_thread_pool();

    let service = DisplayFeatureService::new();
    let binder = BnDisplayFeature::new_binder(service);
    rsbinder::hub::add_service(SERVICE_NAME, binder.as_binder())?;

    tracing::info!("Registered {} successfully. Joining thread pool...", SERVICE_NAME);
    rsbinder::ProcessState::join_thread_pool()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_scheme_modes_map_to_native_modes() {
        // DisplayFeatureManagerService.handleScreenSchemeChange():
        // screen_optimize_mode 1/2/3/4 -> setFeature mode 0/1/2/23.
        assert_eq!(native_color_mode(0), 1); // Vivid
        assert_eq!(native_color_mode(1), 2); // Enhanced
        assert_eq!(native_color_mode(2), 0); // Original colour PRO
        assert_eq!(native_color_mode(23), 0); // custom
        assert_eq!(native_color_mode(99), 0); // unknown stays neutral
    }

    #[test]
    fn green_axis_changes_saturation() {
        assert_eq!(color_saturation(50.0), 100);
        assert_eq!(color_saturation(-2.0), 100);
        assert_eq!(color_saturation(-60.0), 90);
        assert_eq!(color_saturation(-200.0), 80);
    }

    #[test]
    fn decode_neutral_is_gray() {
        let (r, g, b) = decode_color(0xff808080u32 as i32);
        assert!(r.abs_diff(g) <= 2 && g.abs_diff(b) <= 2, "{r},{g},{b}");
    }

    #[test]
    fn higher_rates_keep_min_at_60() {
        assert_eq!(min_refresh_rate(60, false), 60);
        assert_eq!(min_refresh_rate(90, false), 60);
        assert_eq!(min_refresh_rate(120, false), 60);
        assert_eq!(min_refresh_rate(144, false), 60);
        assert_eq!(min_refresh_rate(120, true), 60);
    }
}
