//! `odap-vdev-view`: the simulator window of a virtual device (virtual-devices ADR 0001).
//!
//! It shows the guest's display over QEMU's VNC server on the loopback (RFB), framed by the
//! device as `odap-vdev frame` describes it, and sends the device's controls over QMP with real
//! presses and releases, so long presses and combinations work as on the hardware. The sidebar's
//! other actions (battery, charger, storage, boot) run `odap-vdev`, which owns them.
//!
//! Started by `odap-vdev start --window`; it ends when the device stops.

use std::{
    io::{BufRead, BufReader, Write},
    rc::Rc,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

#[derive(Deserialize)]
struct FrameDoc {
    frame: Frame,
    display: Display,
    controls: Vec<Control>,
}

#[derive(Deserialize)]
struct Frame {
    body_width_mm: f32,
    body_height_mm: f32,
    body_radius_mm: f32,
    display_x_mm: f32,
    display_y_mm: f32,
    display_radius_mm: f32,
    body_color: String,
    bezel_color: String,
    action_color: String,
    key_color: String,
    key_depth_mm: f32,
}

#[derive(Deserialize)]
struct Display {
    width_px: u32,
    height_px: u32,
    width_mm: f32,
}

#[derive(Deserialize, Clone)]
struct Control {
    role: String,
    kind: String,
    edge: String,
    offset_mm: f32,
    length_mm: f32,
    qcode: Option<String>,
}

struct Args {
    frame: PathBuf,
    vnc: String,
    qmp: PathBuf,
    vdev: PathBuf,
    name: String,
    framed: bool,
    on_top: bool,
}

fn args() -> Args {
    let mut a = Args {
        frame: PathBuf::new(),
        vnc: "127.0.0.1:5900".into(),
        qmp: PathBuf::new(),
        vdev: PathBuf::from("odap-vdev"),
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
            "--vdev" => a.vdev = value().into(),
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
        "usage: odap-vdev-view --frame FRAME.json --qmp SOCKET [--vnc HOST:PORT] [--vdev PATH] [--name NAME] [--no-frame] [--no-on-top]"
    );
    std::process::exit(if msg.is_empty() { 0 } else { 2 });
}

/// QMP, for the controls: key down and up as separate events.
struct Qmp(Mutex<Option<(UnixStream, BufReader<UnixStream>)>>);

impl Qmp {
    fn connect(path: &PathBuf) -> Self {
        let conn = (|| {
            let s = UnixStream::connect(path)?;
            let mut r = BufReader::new(s.try_clone()?);
            let mut line = String::new();
            r.read_line(&mut line)?; // the greeting
            let mut w = s.try_clone()?;
            writeln!(w, r#"{{"execute":"qmp_capabilities"}}"#)?;
            line.clear();
            r.read_line(&mut line)?;
            Ok::<_, std::io::Error>((w, r))
        })();
        if let Err(err) = &conn {
            eprintln!("odap-vdev-view: QMP: {err}");
        }
        Qmp(Mutex::new(conn.ok()))
    }

    fn key(&self, qcode: &str, down: bool) {
        let mut guard = self.0.lock().unwrap();
        let Some((w, r)) = guard.as_mut() else { return };
        let cmd = format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"key","data":{{"down":{down},"key":{{"type":"qcode","data":"{qcode}"}}}}}}]}}}}"#
        );
        let mut line = String::new();
        // Replies and events come as lines; read up to the reply to this command.
        if writeln!(w, "{cmd}").is_ok() {
            while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                if line.contains("\"return\"") || line.contains("\"error\"") {
                    break;
                }
                line.clear();
            }
        }
    }
}

fn color(hex: &str) -> slint::Color {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0x808080);
    slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn main() -> Result<(), slint::PlatformError> {
    let a = args();
    let text = std::fs::read_to_string(&a.frame).unwrap_or_else(|e| usage(&format!("{}: {e}", a.frame.display())));
    let doc: FrameDoc = serde_json::from_str(&text).unwrap_or_else(|e| usage(&format!("frame: {e}")));
    let px_mm = doc.display.width_px as f32 / doc.display.width_mm;
    let f = &doc.frame;

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
    ui.set_body_w(f.body_width_mm * px_mm);
    ui.set_body_h(f.body_height_mm * px_mm);
    ui.set_body_radius(f.body_radius_mm * px_mm);
    ui.set_display_x(f.display_x_mm * px_mm);
    ui.set_display_y(f.display_y_mm * px_mm);
    ui.set_display_radius(f.display_radius_mm * px_mm);
    ui.set_key_depth(f.key_depth_mm * px_mm);
    let palette = ui.global::<Palette>();
    palette.set_body(color(&f.body_color));
    palette.set_bezel(color(&f.bezel_color));
    palette.set_action(color(&f.action_color));
    palette.set_key(color(&f.key_color));

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
        let shape = match (c.kind.as_str(), c.role.as_str()) {
            (_, "action") => "disc",
            ("rocker-half", "volume-up") => "half-top",
            ("rocker-half", "volume-down") => "half-bottom",
            _ => "capsule",
        };
        keys.push(Key {
            role: c.role.clone().into(),
            edge: c.edge.clone().into(),
            centre: c.offset_mm * px_mm,
            span: c.length_mm * px_mm,
            shape: shape.into(),
            pressed: false,
        });
        qcode_of.push((c.role.clone(), q));
    }
    let keys_model = Rc::new(VecModel::from(keys));
    ui.set_keys(ModelRc::from(keys_model.clone()));

    let qmp = Arc::new(Qmp::connect(&a.qmp));
    let press = {
        let qmp = qmp.clone();
        let keys_model = keys_model.clone();
        let qcode_of = qcode_of.clone();
        move |role: &str, down: bool| {
            if let Some((_, q)) = qcode_of.iter().find(|(r, _)| r == role) {
                qmp.key(q, down);
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
        ui.on_key(move |role, down| press(role.as_str(), down));
    }
    {
        let press = press.clone();
        ui.on_combo(move |down| {
            press("main", down);
            press("volume-down", down);
        });
    }
    {
        // The host keys the faceplate gives (Space, ↑, ↓, P): their role, by their qcode.
        let press = press.clone();
        ui.on_host_key(move |text, down| {
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

    // The sidebar's other actions belong to odap-vdev.
    let vdev = a.vdev.clone();
    let run = move |args: &[&str]| {
        let out = Command::new(&vdev).args(args).output();
        match out {
            Ok(o) => String::from_utf8_lossy(if o.status.success() { &o.stdout } else { &o.stderr }).trim().to_string(),
            Err(e) => e.to_string(),
        }
    };
    let run = Rc::new(run);
    {
        let run = run.clone();
        let weak = ui.as_weak();
        let state = a.frame.parent().map(PathBuf::from).unwrap_or_default();
        ui.on_action(move |what| {
            let shots = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Desktop");
            // A blank card or disk, in the instance's own state directory (beside frame.json),
            // never in a shared temporary directory; created only if it is not there.
            let blank = state.join("blank.img");
            if !blank.exists() {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&blank)
                    .and_then(|f| f.set_len(64 << 20));
            }
            let blank = blank.display().to_string();
            let shot = shots.join(format!("{}.png", what.as_str())).display().to_string();
            let result = match what.as_str() {
                "sd-insert" => run(&["sd", "insert", &blank]),
                "sd-eject" => run(&["sd", "eject"]),
                "usb-disk" => run(&["usb", "plug", "storage", &blank]),
                "usb-disk-out" => run(&["usb", "unplug", "usbdisk"]),
                "usb-audio" => run(&["usb", "plug", "audio"]),
                "usb-audio-out" => run(&["usb", "unplug", "usbaudio"]),
                "screenshot" => run(&["screen", "capture", &shot]),
                "reboot" => run(&["reboot"]),
                "reboot-debug" => run(&["reboot", "--mode", "debug"]),
                "poweroff" => run(&["stop"]),
                _ => String::new(),
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_status(result.into());
            }
        });
    }
    {
        let run = run.clone();
        let weak = ui.as_weak();
        ui.on_battery_set(move |level| {
            let r = run(&["power", "battery", &format!("{}", level.round() as i32)]);
            if let Some(ui) = weak.upgrade() {
                ui.set_status(r.into());
            }
        });
    }
    {
        let run = run.clone();
        let weak = ui.as_weak();
        ui.on_charger_set(move |on| {
            let r = run(&["power", "charger", if on { "on" } else { "off" }]);
            if let Some(ui) = weak.upgrade() {
                ui.set_status(r.into());
            }
        });
    }

    // The display: frames from VNC on a thread, drawn on the event loop.
    let (mut reader, writer) = match opendap_rfb::connect(&a.vnc) {
        Ok(c) => c,
        Err(e) => usage(&format!("VNC {}: {e}", a.vnc)),
    };
    let writer = Arc::new(Mutex::new(writer));
    {
        // The pointer over the display, pressed or not, so the shell's touch indicator follows it.
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
                    if updates.iter().any(|u| *u == opendap_rfb::Update::Resized) {
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


fn save_png(path: &PathBuf, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgba).map_err(|e| e.to_string())
}
