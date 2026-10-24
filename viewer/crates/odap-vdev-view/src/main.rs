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
        keys.push(Key { role: c.role.clone().into(), x: r.x, y: r.y, w: r.w, h: r.h, pressed: false });
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
            // Save PNG: the display as the host sees it, to the Desktop, with no part of the OS involved.
            let shot = shots.join(format!("{} {}.png", "Virtual T1", stamp())).display().to_string();
            let result = match what.as_str() {
                "sd-insert" => run(&["sd", "insert"]),
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
