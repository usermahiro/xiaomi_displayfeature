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

fn apply_settings_batch(hz: i32, is_user_selection: bool) {
    let is_smart = is_smart_fps_mode();
    let min_hz = if is_smart {
        60
    } else {
        hz // in custom mode, lock minimum to requested rate (e.g. 60Hz -> min 60; 120Hz -> min 120)
    };

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

fn push_saturation() {
    let sat = if GRAYSCALE_ON.load(Ordering::SeqCst) > 0 { 0 } else { 100 };
    let sat_str = sat.to_string();
    color_display_call(&[&TX_SET_SATURATION.to_string(), "i32", &sat_str]);
}

/// MIUI's custom color picker stores a packed ARGB pixel in
/// `screen_color_level` (ScreenColorAlgorithm.encode). Presets are the small
/// values 1=warm, 2=natural, 3=cool. Translate both to a night-display
/// colour temperature so the mode is actually visible on a ported ROM.
fn apply_color_packed(value: i32) {
    let delta = if (0..=3).contains(&value) {
        match value {
            1 => 60,   // warm preset
            3 => -60,  // cool preset
            _ => 0,    // natural / default
        }
    } else {
        let argb = value as u32;
        let r = (argb >> 16) & 0xff;
        let b = argb & 0xff;
        r as i32 - b as i32
    };

    // Grayscale must stay grayscale regardless of the picked colour.
    push_saturation();

    let active_tx = TX_SET_NIGHT_ACTIVE.to_string();
    if delta.abs() <= 3 {
        tracing::info!("[DisplayFeature-Color] ColorPacked({}) -> natural, NightDisplay OFF", value);
        color_display_call(&[&active_tx, "i32", "0"]);
        return;
    }

    let kelvin = (6500i32 - delta.saturating_mul(100)).clamp(2500, 9000);
    tracing::info!(
        "[DisplayFeature-Color] ColorPacked({}) -> NightDisplay ON @ {}K (r-b={})",
        value, kelvin, delta
    );
    let temp_tx = TX_SET_NIGHT_TEMP.to_string();
    let temp = kelvin.to_string();
    // Set temperature before activation so the tint appears in one frame.
    color_display_call(&[&temp_tx, "i32", &temp]);
    color_display_call(&[&active_tx, "i32", "1"]);
}

fn apply_paper_color_type(value: i32) {
    GRAYSCALE_ON.store(if value > 0 { 1 } else { 0 }, Ordering::SeqCst);
    push_saturation();
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
            // MIUI colour-scheme modes: 0/1/2 = adapt/enhance/standard,
            // 23 = unlimited colour level (packed ARGB from the colour picker).
            request_screen_effect(ScreenCommand::ColorPacked(value));
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
