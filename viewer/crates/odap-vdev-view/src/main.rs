//! `odap-vdev-view`: the simulator window of a virtual device (virtual-devices ADR 0001).
//!
//! It shows the guest's display over QEMU's VNC server on the loopback (RFB), framed by the
//! device as `odap-vdev frame` describes it, and sends the device's controls with real presses
//! and releases, so long presses and combinations work as on the hardware. Everything it does to
//! the device goes through the OpenDAP CLI's crates, the use cases `opendap` and OpenDAP Desktop
//! share: the controls over a held QMP session (opendap-transport-vm `LiveKeys`), the rest on a
//! worker thread (src/device.rs).
//!
//! Started by `odap-vdev start --window`; it ends when the device stops.

use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
};

use opendap_core::InputSink;
use opendap_core::hardware::Switch;
use opendap_core::power::Phase;
use opendap_model::{Attachment, BootMode, InputEvent, PowerAction, Role};
use opendap_transport_vm::live::LiveKeys;

use serde::Deserialize;
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

mod device;
mod frame;

slint::include_modules!();

#[derive(Deserialize)]
struct FrameDoc {
    case: Option<frame::Case>,
    front: Option<frame::Front>,
    display: Display,
    controls: Vec<Control>,
}

#[derive(Deserialize)]
struct Display {
    width_px: u32,
    height_px: u32,
    width_mm: f32,
    height_mm: f32,
    #[serde(default)]
    shape: String,
}

#[derive(Deserialize, Clone)]
struct Control {
    role: String,
    qcode: Option<String>,
}

struct Args {
    frame: PathBuf,
    vnc: String,
    qmp: PathBuf,
    checkout: PathBuf,
    name: String,
    framed: bool,
    on_top: bool,
}

fn args() -> Args {
    let mut a = Args {
        frame: PathBuf::new(),
        vnc: "127.0.0.1:5900".into(),
        qmp: PathBuf::new(),
        checkout: PathBuf::new(),
        name: "Virtual device".into(),
        framed: true,
        on_top: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage(&format!("{arg} needs a value")));
        match arg.as_str() {
            "--frame" => a.frame = value().into(),
            "--vnc" => a.vnc = value(),
            "--qmp" => a.qmp = value().into(),
            "--checkout" => a.checkout = value().into(),
            "--name" => a.name = value(),
            "--no-frame" => a.framed = false,
            "--no-on-top" => a.on_top = false,
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    a
}

fn usage(msg: &str) -> ! {
    if !msg.is_empty() {
        eprintln!("odap-vdev-view: {msg}");
    }
    eprintln!(
        "usage: odap-vdev-view --frame FRAME.json --qmp SOCKET --checkout VIRTUAL_DEVICES_DIR [--vnc HOST:PORT] [--name NAME] [--no-frame] [--no-on-top]"
    );
    std::process::exit(if msg.is_empty() { 0 } else { 2 });
}

fn main() -> Result<(), slint::PlatformError> {
    let a = args();
    let text = std::fs::read_to_string(&a.frame).unwrap_or_else(|e| usage(&format!("{}: {e}", a.frame.display())));
    let doc: FrameDoc = serde_json::from_str(&text).unwrap_or_else(|e| usage(&format!("frame: {e}")));
    let px_mm = doc.display.width_px as f32 / doc.display.width_mm;

    // Kept on top from the start: the window is created with that level (winit's attributes).
    if a.on_top {
        slint::BackendSelector::new()
            .with_winit_window_attributes_hook(|attrs| {
                attrs.with_window_level(slint::winit_030::winit::window::WindowLevel::AlwaysOnTop)
            })
            .select()?;
    }
    let ui = View::new()?;
    ui.set_name(a.name.clone().into());
    ui.set_framed(a.framed);
    ui.set_screen_w(doc.display.width_px as f32);
    ui.set_screen_h(doc.display.height_px as f32);
    // The front, drawn at twice the logical density so it stays sharp on a Retina display.
    let framed = a.framed && doc.case.is_some() && doc.front.is_some();
    ui.set_framed(framed);
    let mut key_rects: Vec<(String, frame::Rect)> = Vec::new();
    let mut sprites: Vec<frame::Sprite> = Vec::new();
    if let (true, Some(case), Some(front)) = (framed, doc.case.as_ref(), doc.front.as_ref()) {
        let panel = frame::Panel {
            width_mm: doc.display.width_mm,
            height_mm: doc.display.height_mm,
            round: doc.display.shape == "round",
        };
        let drawn = frame::draw(case, front, &panel, px_mm, 2.0);
        let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&drawn.rgba, drawn.width, drawn.height);
        ui.set_front(slint::Image::from_rgba8_premultiplied(buffer));
        ui.set_front_w(drawn.size.0);
        ui.set_front_h(drawn.size.1);
        ui.set_screen_x(drawn.screen.x);
        ui.set_screen_y(drawn.screen.y);
        ui.set_led_x(drawn.led.0);
        ui.set_led_y(drawn.led.1);
        ui.set_led_d(drawn.led.2);
        key_rects = drawn.keys;
        sprites = drawn.sprites;
    }

    // One key per role, with the shape the concept art gives it (T1-059).
    let mut keys: Vec<Key> = Vec::new();
    let mut qcode_of: Vec<(String, String)> = Vec::new();
    let mut host_keys: Vec<(String, String)> = Vec::new();
    for c in &doc.controls {
        let Some(q) = c.qcode.clone() else { continue };
        if keys.iter().any(|k| k.role == c.role.as_str()) {
            host_keys.push((q, c.role.clone()));
            continue;
        }
        let r = key_rects.iter().find(|(role, _)| *role == c.role).map(|(_, r)| *r).unwrap_or_default();
        let mut key = Key { role: c.role.clone().into(), x: r.x, y: r.y, w: r.w, h: r.h, pressed: false, ..Default::default() };
        if let Some(sp) = sprites.iter().find(|s| s.role == c.role) {
            let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&sp.rgba, sp.width, sp.height);
            key.sprite = slint::Image::from_rgba8_premultiplied(buffer);
            key.has_sprite = true;
            (key.sx, key.sy, key.sw, key.sh, key.travel) = (sp.rect.x, sp.rect.y, sp.rect.w, sp.rect.h, sp.travel);
        }
        keys.push(key);
        qcode_of.push((c.role.clone(), q));
    }
    let keys_model = Rc::new(VecModel::from(keys));
    ui.set_keys(ModelRc::from(keys_model.clone()));

    // The controls, in order, on a thread of their own: the window never waits for QMP.
    let (key_tx, key_rx) = mpsc::channel::<InputEvent>();
    // A keyboard's keys (← →) go the same way, in order with the buttons.
    let keyboard_tx = key_tx.clone();
    {
        let live = LiveKeys::new(&a.qmp, qcode_of.iter().map(|(r, q)| (Role::from(r.as_str()), q.clone())).collect());
        std::thread::spawn(move || {
            // ODAP_VIEW_TRACE=1: every control event, as sent, on stderr (window.log).
            let trace = std::env::var_os("ODAP_VIEW_TRACE").is_some();
            for event in key_rx {
                if trace {
                    eprintln!("odap-vdev-view: {event:?}");
                }
                if let Err(e) = live.send(&event) {
                    eprintln!("odap-vdev-view: {}", e.message);
                }
            }
        });
    }
    let press = {
        let keys_model = keys_model.clone();
        let weak = ui.as_weak();
        move |role: &str, down: bool| {
            // Already so (a key the OS repeats, the mouse and a key on the same control): nothing.
            let same = (0..keys_model.row_count())
                .filter_map(|i| keys_model.row_data(i))
                .any(|k| k.role == role && k.pressed == down);
            if same {
                return;
            }
            let role = Role::from(role);
            let _ = key_tx.send(if down { InputEvent::KeyDown { role: role.clone() } } else { InputEvent::KeyUp { role: role.clone() } });
            let role = role.as_str();
            if let Some(ui) = weak.upgrade() {
                match role {
                    "action" => ui.set_action_down(down),
                    "main" => ui.set_main_down(down),
                    "volume-up" => ui.set_volume_up_down(down),
                    "volume-down" => ui.set_volume_down_down(down),
                    _ => {}
                }
            }
            for i in 0..keys_model.row_count() {
                let mut k = keys_model.row_data(i).unwrap();
                if k.role == role {
                    k.pressed = down;
                    keys_model.set_row_data(i, k);
                }
            }
        }
    };
    let press = Rc::new(press);
    {
        let press = press.clone();
        let trace = std::env::var_os("ODAP_VIEW_TRACE").is_some();
        ui.on_key(move |role, down| {
            if trace {
                eprintln!("odap-vdev-view: key {role} {down}");
            }
            press(role.as_str(), down)
        });
    }
    {
        let press = press.clone();
        let trace = std::env::var_os("ODAP_VIEW_TRACE").is_some();
        ui.on_combo(move |down| {
            if trace {
                eprintln!("odap-vdev-view: combo {down}");
            }
            press("main", down);
            press("volume-down", down);
        });
    }
    {
        // The host keys the faceplate gives (Space, ↑, ↓, P): their role, by their qcode.
        let press = press.clone();
        let trace = std::env::var_os("ODAP_VIEW_TRACE").is_some();
        ui.on_host_key(move |text, down| {
            if trace {
                eprintln!("odap-vdev-view: host key {:?} {down}", text.as_str());
            }
            // ↑ and ↓ are the buttons by their place (Volume + above, Volume − below), as the
            // faceplate's host keys say. ← and → are no button (the buttons stand vertically):
            // they reach the device as a keyboard's arrows, so a focused slider takes them
            // (→ more, ← less). Enter is Action, as Space is: the key that takes the default
            // action or the focused element; which of the two Action does is the device's call.
            match text.as_str() {
                "\n" | "\r" => return press("action", down),
                "\u{F702}" | "\u{F703}" => {
                    let key = if text.as_str() == "\u{F702}" { "left" } else { "right" };
                    let _ = keyboard_tx.send(InputEvent::Keyboard { key: key.into(), down });
                    return;
                }
                _ => {}
            }
            let qcode = match text.as_str() {
                " " => "spc",
                "\u{F700}" => "up",
                "\u{F701}" => "down",
                t => t,
            };
            if let Some((_, role)) = host_keys.iter().find(|(q, _)| q == qcode) {
                press(role, down);
            }
        });
    }

    // The sidebar's other actions, on the worker; the window shows the state it publishes.
    let worker = {
        let weak = ui.as_weak();
        device::Worker::start(a.checkout.clone(), move |st| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    show(&ui, &st);
                }
            });
        })
    };
    let jobs = worker.jobs.clone();
    {
        let jobs = jobs.clone();
        let device_name = a.name.clone();
        let weak = ui.as_weak();
        ui.on_action(move |what| {
            let Some(ui) = weak.upgrade() else { return };
            let job = match what.as_str() {
                // Save PNG: the display as the host sees it, to the Desktop, with no part of the OS involved.
                "screenshot" => {
                    // In flight from the click: a second click never saves a second picture.
                    if ui.get_saving() {
                        return;
                    }
                    ui.set_saving(true);
                    let shots = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Desktop");
                    device::Job::SavePng(shots.join(format!("{device_name} {}.png", stamp())))
                }
                "reboot" => device::Job::Power(PowerAction::Reboot { mode: BootMode::Normal }),
                "reboot-debug" => device::Job::Power(PowerAction::Reboot { mode: BootMode::Debug }),
                "poweroff" => device::Job::Power(PowerAction::Poweroff),
                _ => return,
            };
            let _ = jobs.send(job);
        });
    }
    {
        let jobs = jobs.clone();
        ui.on_battery_set(move |level| {
            let _ = jobs.send(device::Job::Battery(level.round().clamp(0.0, 100.0) as u8));
        });
    }
    {
        let jobs = jobs.clone();
        ui.on_light_set(move |position| {
            let _ = jobs.send(device::Job::Light(slider_to_lux(position)));
        });
    }
    {
        let jobs = jobs.clone();
        let weak = ui.as_weak();
        let debug_jobs = jobs.clone();
        let debug_weak = ui.as_weak();
        ui.on_set_debug_ui(move |on| {
            if let Some(ui) = debug_weak.upgrade() {
                // In flight from the click, as the other switches.
                match (ui.get_debug_ui(), on) {
                    (0, true) => ui.set_debug_ui(1),
                    (2, false) => ui.set_debug_ui(3),
                    _ => return,
                }
            }
            let _ = debug_jobs.send(device::Job::DebugUi(on));
        });
        ui.on_attach(move |what, on| {
            let Some(ui) = weak.upgrade() else { return };
            let (a, get, set): (Attachment, fn(&View) -> i32, fn(&View, i32)) = match what.as_str() {
                "microsd" => (Attachment::Microsd, View::get_sd, View::set_sd),
                "usb-storage" => (Attachment::UsbStorage, View::get_usb_disk, View::set_usb_disk),
                "usb-audio" => (Attachment::UsbAudio, View::get_usb_dac, View::set_usb_dac),
                "charger" => (Attachment::Charger, View::get_charger, View::set_charger),
                _ => return,
            };
            // In flight from the click, even while the worker finishes another change: the
            // worker's next state confirms it, or puts it back if it does not apply.
            match (get(&ui), on) {
                (0, true) => set(&ui, 1),
                (2, false) => set(&ui, 3),
                _ => return,
            }
            let _ = jobs.send(device::Job::Attach(a, on));
        });
    }

    // The display: frames from VNC on a thread, drawn on the event loop.
    let (mut reader, writer) = match opendap_rfb::connect(&a.vnc) {
        Ok(c) => c,
        Err(e) => usage(&format!("VNC {}: {e}", a.vnc)),
    };
    let writer = Arc::new(Mutex::new(writer));
    {
        // The pointer over the display, pressed or not, so the composer's touch indicator follows it.
        let writer = writer.clone();
        let down = std::cell::Cell::new(false);
        ui.on_screen_pointer(move |kind, x, y| {
            match kind.as_str() {
                "down" => down.set(true),
                "up" => down.set(false),
                _ => {}
            }
            let _ = writer.lock().unwrap().pointer(x.max(0.0) as u16, y.max(0.0) as u16, down.get());
        });
    }
    let weak = ui.as_weak();
    let req = writer.clone();
    std::thread::spawn(move || {
        loop {
            match reader.next() {
                Ok(updates) => {
                    if updates.contains(&opendap_rfb::Update::Resized) {
                        req.lock().unwrap().resized(reader.frame.width, reader.frame.height);
                    }
                    let (w, h) = (reader.frame.width as u32, reader.frame.height as u32);
                    let mut rgba = Vec::with_capacity(reader.frame.bgrx.len());
                    for p in reader.frame.bgrx.chunks_exact(4) {
                        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
                    }
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                            ui.set_screen(slint::Image::from_rgba8(buffer));
                        }
                    });
                    let _ = req.lock().unwrap().request(true);
                }
                // The device stopped: the window goes with it.
                Err(_) => {
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                    break;
                }
            }
        }
    });

    // ODAP_VIEW_SNAPSHOT=FILE.png: the window as drawn, saved once the device has drawn, for
    // checks without a person (an agent, a test) and with the host's screen locked.
    if let Some(path) = std::env::var_os("ODAP_VIEW_SNAPSHOT") {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
            let Some(ui) = weak.upgrade() else { return };
            match ui.window().take_snapshot() {
                Ok(shot) => {
                    let saved = save_png(&PathBuf::from(&path), shot.width(), shot.height(), shot.as_bytes());
                    if let Err(e) = saved {
                        eprintln!("odap-vdev-view: snapshot: {e}");
                    }
                }
                Err(e) => eprintln!("odap-vdev-view: snapshot: {e}"),
            }
        });
    }
    ui.run()
}


/// The worker's state, on the window.
fn show(ui: &View, st: &device::State) {
    ui.set_phase(match st.phase {
        Phase::Off => 0,
        Phase::Starting => 1,
        Phase::Ready => 2,
        Phase::Rebooting { .. } => 3,
        Phase::Stopping => 4,
    });
    ui.set_sd(st.code(Attachment::Microsd));
    ui.set_usb_disk(st.code(Attachment::UsbStorage));
    ui.set_usb_dac(st.code(Attachment::UsbAudio));
    ui.set_charger(st.code(Attachment::Charger));
    ui.set_battery_known(st.battery.is_some());
    ui.set_battery_busy(st.battery_busy);
    // The status LED as the device drives it; off, nothing is drawn over the unlit part.
    ui.set_led(match st.status_led {
        Some([0, 0, 0]) | None => slint::Color::from_argb_u8(0, 0, 0, 0),
        Some([r, g, b]) => slint::Color::from_rgb_u8(r, g, b),
    });
    ui.set_light_known(st.light.is_some());
    ui.set_mic_live(st.mic_live);
    // Read the sensor's light once, as the battery: the slider then shows what it sent.
    if let Some(lux) = st.light
        && !ui.get_light_seeded()
    {
        ui.set_light(lux_to_slider(lux));
        ui.set_light_seeded(true);
    }

    // Read the hardware's charge once; after that the slider shows only what it sent.
    if let Some(p) = st.battery
        && !ui.get_battery_seeded()
    {
        ui.set_battery(p as f32);
        ui.set_battery_seeded(true);
    }
    ui.set_saving(st.saving);
    ui.set_debug_ui(match st.debug_ui {
        Switch::Off => 0,
        Switch::Attaching => 1,
        Switch::On => 2,
        Switch::Detaching => 3,
    });
    ui.set_debug_reboot(st.debug_reboot);
    let phase = match st.phase {
        Phase::Off => "Off",
        Phase::Starting => "Starting…",
        Phase::Ready => "Running",
        Phase::Rebooting { .. } => "Restarting…",
        Phase::Stopping => "Powering off…",
    };
    let mut busy: Vec<&str> = Attachment::ALL
        .into_iter()
        .filter(|a| st.switch(*a).busy())
        .map(|a| match a {
            Attachment::Microsd => "microSD",
            Attachment::UsbStorage => "USB disk",
            Attachment::UsbAudio => "USB DAC",
            Attachment::Charger => "charger",
        })
        .collect();
    if st.debug_ui.busy() {
        busy.push("blueprint mode");
    }
    // The line under the name is the device's state only; what is in flight and the notes go to
    // the line at the bottom.
    ui.set_status(phase.into());
    let mut note = String::new();
    if !busy.is_empty() {
        note = busy.join(", ") + "…";
    }
    if !st.note.is_empty() {
        if !note.is_empty() {
            note += " · ";
        }
        note += &st.note;
    }
    ui.set_note(note.into());
}

fn save_png(path: &PathBuf, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgba).map_err(|e| e.to_string())
}

/// `YYYY-MM-DD HH.MM.SS` (UTC) for a saved picture's name, as macOS names screenshots.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}.{:02}.{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

/// The light slider is logarithmic, as the eye is: 0 is darkness, 1 is 10 lx, 5 is 100 000 lx
/// (direct sun); 0.0 to 5.0 maps to `10^p − 1`.
fn slider_to_lux(position: f32) -> u32 {
    (10f64.powf(position.clamp(0.0, 5.0) as f64) - 1.0).round() as u32
}

fn lux_to_slider(lux: u32) -> f32 {
    ((lux as f64) + 1.0).log10() as f32
}

