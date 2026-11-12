//! Losing the GPU device (a driver reset or update, a GPU switch, waking
//! from sleep, a VM taking the GPU): wgpu then fails every call, and
//! egui-wgpu, which can't make a new device, panics drawing the next frame
//! ("Failed to create staging buffer"). Oak can't go on drawing, so it
//! restarts: `main` catches that panic, and `recover` ultrapauses the
//! conversations at work (offered for resuming at the next start), keeps
//! the background tasks running (the next Oak takes them up again), and
//! has Oak start again once it has closed. Why the device was lost goes to
//! crash.log (`watch`).

use crate::agent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The device was lost (wgpu said so).
static LOST: AtomicBool = AtomicBool::new(false);
/// Oak is to start again once it has closed.
static RESTART: AtomicBool = AtomicBool::new(false);
/// Set for the Oak started again: when it was.
const RESTARTED_VAR: &str = "OAK_GPU_RESTART";
/// An Oak started again that loses the device again this soon doesn't
/// start again (the GPU may be unusable).
const TOO_SOON: Duration = Duration::from_secs(60);
/// How long the ultrapaused conversations get to record what they stopped.
const GRACE: Duration = Duration::from_secs(3);

/// How Oak sets up wgpu: on Windows it prefers Direct3D 12 (Windows' own
/// path, which rides out driver resets and a GPU shared with VMs best),
/// then Vulkan; a discrete GPU before an integrated one. `WGPU_BACKEND`
/// still picks the backend when set.
pub(crate) fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    let mut options = eframe::egui_wgpu::WgpuConfiguration::default();
    if cfg!(windows)
        && std::env::var_os("WGPU_BACKEND").is_none()
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut options.wgpu_setup
    {
        new.native_adapter_selector = Some(std::sync::Arc::new(|adapters, surface| {
            use eframe::wgpu::{Backend, DeviceType};
            adapters
                .iter()
                .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
                .min_by_key(|a| {
                    let info = a.get_info();
                    let backend = match info.backend {
                        Backend::Dx12 => 0,
                        Backend::Vulkan => 1,
                        _ => 2,
                    };
                    let kind = match info.device_type {
                        DeviceType::DiscreteGpu => 0,
                        DeviceType::IntegratedGpu => 1,
                        _ => 2,
                    };
                    // A software renderer only when there is nothing else.
                    (info.device_type == DeviceType::Cpu, backend, kind)
                })
                .cloned()
                .ok_or_else(|| "no graphics adapter can draw to the window".to_string())
        }));
    }
    options
}

/// The device, and the adapter it is on, as `watch` found them.
static DEVICE: std::sync::Mutex<Option<(eframe::wgpu::Device, String)>> = std::sync::Mutex::new(None);

/// Logs to crash.log why the device of `cc` is lost, should it be (wgpu
/// says so when the device is next polled: `recover` polls it).
pub(crate) fn watch(cc: &eframe::CreationContext) {
    let Some(rs) = &cc.wgpu_render_state else { return };
    let info = rs.adapter.get_info();
    let adapter = format!("{} on {:?} (driver {})", info.name, info.backend, format!("{} {}", info.driver, info.driver_info).trim());
    *DEVICE.lock().unwrap() = Some((rs.device.clone(), adapter));
    rs.device.set_device_lost_callback(move |reason, message| {
        LOST.store(true, Ordering::SeqCst);
        crate::log_crash(&format!("GPU device lost ({reason:?}): {message}"));
    });
    // A way to try it: `OAK_DIR=<a scratch folder> OAK_TEST_GPU_LOSS=5 oak`
    // loses the device after 5 seconds.
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("OAK_TEST_GPU_LOSS").ok().and_then(|s| s.parse().ok()) {
        let device = rs.device.clone();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            device.destroy();
            ctx.request_repaint();
        });
    }
}

/// Whether a panic that ended the event loop was the device's loss.
pub(crate) fn is_loss(payload: &(dyn std::any::Any + Send)) -> bool {
    let message = payload.downcast_ref::<String>().map(String::as_str).or(payload.downcast_ref::<&str>().copied()).unwrap_or("");
    LOST.load(Ordering::SeqCst) || message.starts_with("Failed to create staging buffer")
}

/// This Oak was started again after losing the device.
static RESTARTED: AtomicBool = AtomicBool::new(false);

/// At start (before any thread): whether this Oak was started again after
/// losing the device (the variable then goes, so that the commands Oak runs,
/// and an Oak started again to update, don't have it).
pub(crate) fn init() {
    if std::env::var_os(RESTARTED_VAR).is_some() {
        RESTARTED.store(true, Ordering::SeqCst);
        // SAFETY: no other thread runs yet.
        unsafe { std::env::remove_var(RESTARTED_VAR) };
    }
}

/// This Oak was started again after losing the device.
pub(crate) fn restarted() -> bool {
    RESTARTED.load(Ordering::SeqCst)
}

/// After the device was lost (the window is gone): the conversations at
/// work stop and are noted for resuming, their background tasks are kept
/// running, and Oak is to start again (`relaunch`), unless it was started
/// again `TOO_SOON` before.
pub(crate) fn recover(started: Instant) {
    // Why it was lost (the callback above logs it), and on what.
    if let Some((device, adapter)) = DEVICE.lock().unwrap().take() {
        let _ = device.poll(eframe::wgpu::PollType::Poll);
        crate::log_crash(&format!("The GPU device was on {adapter}."));
    }
    let ids = agent::ultrapause_all_keeping_tasks();
    crate::tasks::keep_on_close(true);
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline && !agent::all_settled(&ids) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !ids.is_empty() {
        let mut settings = crate::store::load_settings();
        for id in ids {
            if !settings.paused_on_close.contains(&id) {
                settings.paused_on_close.push(id);
            }
        }
        crate::store::save_settings(&settings);
    }
    let again = !(restarted() && started.elapsed() < TOO_SOON);
    crate::log_crash(if again { "Oak restarts after losing the GPU device." } else { "Oak lost the GPU device again right after restarting: not restarting." });
    RESTART.store(again, Ordering::SeqCst);
}

/// Starts Oak again, if `recover` said so (once this one has closed).
pub(crate) fn relaunch() {
    if !RESTART.load(Ordering::SeqCst) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).env(RESTARTED_VAR, crate::api::now_ms().to_string());
    let _ = cmd.spawn();
}
