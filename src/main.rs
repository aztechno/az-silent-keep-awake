#![windows_subsystem = "windows"]

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIconBuilder,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};
use winreg::enums::*;
use winreg::RegKey;

static ICON_BYTES: &[u8] = include_bytes!("../icon.ico");
static WAV_BYTES: &[u8] = include_bytes!("../silent.wav");

const APP_NAME: &str = "Az Silent Keep Awake";
const CONFIG_FILE: &str = "config.json";
const REG_KEY: &str = "AzSilentKeepAwake";

// ---------- Startup Logic (Registry) ----------
fn is_startup_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
        if let Ok(val) = key.get_value::<String, _>(REG_KEY) {
            let current_exe = std::env::current_exe().unwrap_or_default();
            return val == current_exe.to_string_lossy();
        }
    }
    false
}

fn set_startup_enabled(enable: bool) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_ALL_ACCESS) {
        if enable {
            if let Ok(current_exe) = std::env::current_exe() {
                let _ = key.set_value(REG_KEY, &current_exe.to_string_lossy().to_string());
            }
        } else {
            let _ = key.delete_value(REG_KEY);
        }
    }
}

// ... (Config & Time Logic remain the same) ...
#[derive(Serialize, Deserialize, Clone)]
struct Config {
    master_enabled: bool,
    schedule_enabled: bool,
    audio_enabled: bool,
    awake_start: String,
    awake_end: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            master_enabled: true,
            schedule_enabled: true,
            audio_enabled: true,
            awake_start: "07:00".to_string(),
            awake_end: "22:30".to_string(),
        }
    }
}

fn load_config() -> Config {
    if let Ok(data) = fs::read_to_string(CONFIG_FILE) {
        if let Ok(cfg) = serde_json::from_str(&data) {
            return cfg;
        }
    }
    let default_cfg = Config::default();
    let _ = fs::write(CONFIG_FILE, serde_json::to_string_pretty(&default_cfg).unwrap());
    default_cfg
}

fn save_config(cfg: &Config) {
    if let Ok(data) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(CONFIG_FILE, data);
    }
}

fn parse_time_to_minutes(time_str: &str) -> u32 {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() == 2 {
        let h = parts[0].trim().parse::<u32>().unwrap_or(0);
        let m = parts[1].trim().parse::<u32>().unwrap_or(0);
        return (h * 60 + m) % 1440;
    }
    0
}

fn current_minutes() -> u32 {
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}

fn is_within_awake_window(now: u32, start: u32, end: u32) -> bool {
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn format_ampm(hour: u32, minute: u32) -> String {
    let am_pm = if hour < 12 { "AM" } else { "PM" };
    let display_hour = match hour % 12 { 0 => 12, h => h };
    format!("{:02}:{:02} {}", display_hour, minute, am_pm)
}

// ... (Audio Engine remains the same) ...
struct AudioBuffer {
    samples: Vec<f32>,
    channels: usize,
    position: AtomicUsize,
}

impl AudioBuffer {
    fn from_embedded_wav() -> Self {
        let cursor = Cursor::new(WAV_BYTES);
        if let Ok(mut r) = hound::WavReader::new(cursor) {
            let spec = r.spec();
            if spec.channels >= 1 && spec.sample_rate > 0 {
                let samples: Vec<f32> = match spec.sample_format {
                    hound::SampleFormat::Int => {
                        let max = (1u32 << (spec.bits_per_sample.saturating_sub(1))) as f32;
                        r.samples::<i32>().filter_map(|s| s.ok()).map(|v| v as f32 / max).collect()
                    }
                    hound::SampleFormat::Float => {
                        r.samples::<f32>().filter_map(|s| s.ok()).collect()
                    }
                };
                if !samples.is_empty() {
                    return Self { samples, channels: spec.channels as usize, position: AtomicUsize::new(0) };
                }
            }
        }
        Self { samples: vec![0.0; 44100 * 2], channels: 2, position: AtomicUsize::new(0) }
    }
}

fn render<T: Copy>(buf: &AudioBuffer, data: &mut [T], out_channels: usize, convert: impl Fn(f32) -> T, silence: T) {
    if out_channels == 0 || buf.channels == 0 { return; }
    let src_frames = buf.samples.len() / buf.channels;
    if src_frames == 0 { return; }
    let frames = data.len() / out_channels;
    let mut pos = buf.position.load(Ordering::Relaxed);
    for f in 0..frames {
        let src_f = pos % src_frames;
        for ch in 0..out_channels {
            let src_ch = if ch < buf.channels { ch } else { 0 };
            let s = buf.samples[src_f * buf.channels + src_ch];
            data[f * out_channels + ch] = convert(s);
        }
        pos = pos.wrapping_add(1);
    }
    buf.position.store(pos, Ordering::Relaxed);
}

fn build_stream(buf: Arc<AudioBuffer>) -> Option<Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let cfg_default = device.default_output_config().ok()?;
    let fmt = cfg_default.sample_format();

    let mut cfg: StreamConfig = cfg_default.into();
    cfg.buffer_size = BufferSize::Fixed(4096);

    let out_ch = cfg.channels as usize;
    let err_fn = |_| {};

    let stream = match fmt {
        SampleFormat::F32 => {
            let b = buf.clone();
            device.build_output_stream(&cfg, move |d: &mut [f32], _| render(&b, d, out_ch, |s| s, 0.0), err_fn, None).ok()?
        }
        SampleFormat::I16 => {
            let b = buf.clone();
            device.build_output_stream(&cfg, move |d: &mut [i16], _| render(&b, d, out_ch, |s| (s.clamp(-1.0, 1.0) * 32767.0) as i16, 0), err_fn, None).ok()?
        }
        SampleFormat::U16 => {
            let b = buf.clone();
            device.build_output_stream(&cfg, move |d: &mut [u16], _| render(&b, d, out_ch, |s| ((s.clamp(-1.0, 1.0) * 32767.0) as i32 + 32768) as u16, 32768), err_fn, None).ok()?
        }
        _ => return None,
    };
    stream.play().ok()?;
    Some(stream)
}

fn load_icon_from_memory() -> Icon {
    let cursor = Cursor::new(ICON_BYTES);
    if let Ok(dir) = ico::IconDir::read(cursor) {
        if let Some(entry) = dir.entries().iter().min_by_key(|e| (e.width() as i32 - 32).abs() + (e.height() as i32 - 32).abs()) {
            if let Ok(img) = entry.decode() {
                if let Ok(ic) = Icon::from_rgba(img.rgba_data().to_vec(), img.width(), img.height()) {
                    return ic;
                }
            }
        }
    }
    let s = 16u32;
    let mut rgba = Vec::with_capacity((s * s * 4) as usize);
    for _ in 0..(s * s) { rgba.extend_from_slice(&[200, 30, 30, 255]); }
    Icon::from_rgba(rgba, s, s).expect("fallback icon")
}

fn make_guard() -> Option<keepawake::KeepAwake> {
    keepawake::Builder::default()
        .display(true).idle(true).sleep(true)
        .reason(APP_NAME).app_name("AzSilentKeepAwake").app_reverse_domain("com.az.silentkeepawake")
        .create().ok()
}

// ---------- Main ----------
fn main() {
    let audio_buf = Arc::new(AudioBuffer::from_embedded_wav());
    let mut cfg = load_config();

    let menu = Menu::new();
    let enabled_item  = CheckMenuItem::new("Master Enable Keep Awake", true, cfg.master_enabled, None);
    let schedule_item = CheckMenuItem::new("Use Awake Schedule", true, cfg.schedule_enabled, None);
    let audio_item    = CheckMenuItem::new("Audio Loop Enabled", true, cfg.audio_enabled, None);
    let startup_item  = CheckMenuItem::new("Start with Windows", true, is_startup_enabled(), None); // Fitur Startup
    let sep1          = PredefinedMenuItem::separator();
    let exit_item     = MenuItem::new("Exit", true, None);

    let start_main_sub = Submenu::new("Set Awake Start", true);
    let start_am_sub = Submenu::new("AM", true);
    let start_pm_sub = Submenu::new("PM", true);
    let mut start_items = Vec::new();

    let end_main_sub = Submenu::new("Set Awake End", true);
    let end_am_sub = Submenu::new("AM", true);
    let end_pm_sub = Submenu::new("PM", true);
    let mut end_items = Vec::new();

    for i in 0..48 {
        let h = i / 2;
        let m = (i % 2) * 30;
        let time_24h = format!("{:02}:{:02}", h, m);
        let time_label = format_ampm(h, m);

        let item_start = CheckMenuItem::new(&time_label, true, time_24h == cfg.awake_start, None);
        if h < 12 { start_am_sub.append(&item_start).unwrap(); } else { start_pm_sub.append(&item_start).unwrap(); }
        start_items.push((time_24h.clone(), item_start));

        let item_end = CheckMenuItem::new(&time_label, true, time_24h == cfg.awake_end, None);
        if h < 12 { end_am_sub.append(&item_end).unwrap(); } else { end_pm_sub.append(&item_end).unwrap(); }
        end_items.push((time_24h.clone(), item_end));
    }

    start_main_sub.append(&start_am_sub).unwrap();
    start_main_sub.append(&start_pm_sub).unwrap();
    end_main_sub.append(&end_am_sub).unwrap();
    end_main_sub.append(&end_pm_sub).unwrap();

    menu.append(&enabled_item).unwrap();
    menu.append(&schedule_item).unwrap();
    menu.append(&start_main_sub).unwrap();
    menu.append(&end_main_sub).unwrap();
    menu.append(&audio_item).unwrap();
    menu.append(&startup_item).unwrap(); // Masukkan ke menu tray
    menu.append(&sep1).unwrap();
    menu.append(&exit_item).unwrap();

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(APP_NAME)
        .with_icon(load_icon_from_memory())
        .build()
        .expect("tray icon");

    let mut guard: Option<keepawake::KeepAwake> = None;
    let mut stream: Option<Stream> = None;

    let update_keepawake = |cfg: &Config, guard: &mut Option<keepawake::KeepAwake>| {
        let now = current_minutes();
        let start = parse_time_to_minutes(&cfg.awake_start);
        let end = parse_time_to_minutes(&cfg.awake_end);
        let in_window = is_within_awake_window(now, start, end);
        let should_awake = cfg.master_enabled && (!cfg.schedule_enabled || in_window);
        
        if should_awake {
            if guard.is_none() { *guard = make_guard(); }
        } else {
            *guard = None;
        }
    };

    update_keepawake(&cfg, &mut guard);
    if cfg.audio_enabled {
        stream = build_stream(audio_buf.clone());
    }

    let mut last_check = Instant::now();
    let mut last_audio_retry = Instant::now();

    unsafe {
        let mut msg = std::mem::zeroed::<MSG>();
        loop {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == exit_item.id() {
                    std::process::exit(0);
                } else if ev.id == enabled_item.id() {
                    cfg.master_enabled = enabled_item.is_checked();
                    save_config(&cfg);
                    update_keepawake(&cfg, &mut guard);
                } else if ev.id == schedule_item.id() {
                    cfg.schedule_enabled = schedule_item.is_checked();
                    save_config(&cfg);
                    update_keepawake(&cfg, &mut guard);
                } else if ev.id == audio_item.id() {
                    cfg.audio_enabled = audio_item.is_checked();
                    save_config(&cfg);
                    if cfg.audio_enabled && stream.is_none() {
                        stream = build_stream(audio_buf.clone());
                    } else if !cfg.audio_enabled {
                        stream = None;
                    }
                } else if ev.id == startup_item.id() { // Handle toggle startup
                    set_startup_enabled(startup_item.is_checked());
                } else {
                    let mut changed = false;
                    for (time_str, item) in &start_items {
                        if ev.id == item.id() {
                            cfg.awake_start = time_str.clone();
                            item.set_checked(true);
                            changed = true;
                        } else { item.set_checked(false); }
                    }
                    for (time_str, item) in &end_items {
                        if ev.id == item.id() {
                            cfg.awake_end = time_str.clone();
                            item.set_checked(true);
                            changed = true;
                        } else { item.set_checked(false); }
                    }
                    if changed {
                        save_config(&cfg);
                        update_keepawake(&cfg, &mut guard);
                    }
                }
            }

            if last_check.elapsed() >= Duration::from_secs(30) {
                last_check = Instant::now();
                update_keepawake(&cfg, &mut guard);
            }

            if cfg.audio_enabled && stream.is_none() && last_audio_retry.elapsed() >= Duration::from_secs(5) {
                last_audio_retry = Instant::now();
                stream = build_stream(audio_buf.clone());
            }

            thread::sleep(Duration::from_millis(16));
        }
    }
}