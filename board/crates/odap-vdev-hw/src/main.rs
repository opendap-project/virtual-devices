//! The virtual board's faceplate hardware, inside the guest: what QEMU does not emulate, made as
//! HID devices through the kernel's uhid, so the guest's own drivers expose them as on real
//! hardware and Blackbird OS reads them through the same interfaces (system hardware-support
//! spec §4):
//!
//! - **The status LED**, an RGB LED: a ThingM blink(1) (USB 27b8:01ed), which `hid-led` drives
//!   as three LED class devices, `thingm0:red`, `thingm0:green`, `thingm0:blue`. Whatever the
//!   guest writes to them is mirrored to `/run/opendap/vdev/led` (`r g b`, 0–255) for the host,
//!   and each change is also written as `led r g b` to the FIFO `/run/opendap/vdev/events`
//!   when a reader holds it open (the simulator window, which so follows it with no polling).
//! - **The ambient light sensor**: a HID sensor hub with an ambient light sensor, which
//!   `hid-sensor-hub` and `hid-sensor-als` expose as an IIO device (`in_illuminance_raw`, lux).
//!   The host sets its reading by writing `als <lux>` to the FIFO `/run/opendap/vdev/hw`; the
//!   current value is in `/run/opendap/vdev/als`.
//!
//! - **The microphone's privacy light**, which the hardware ties to the microphone's power
//!   (BRD-027): on the virtual board, the microphone is live while a capture stream runs. procfs
//!   gives no notice of that, so while a capture device exists its state is read once a second
//!   (the virtual board's own cost, not the product's), published in `/run/opendap/vdev/mic`
//!   (`0` or `1`) and as `mic 0|1` on the events FIFO.
//!
//! One thread, blocked in poll(2) on the two uhid descriptors and the command FIFO: no wakeups
//! while nothing changes, but the microphone's check.

use std::{
    ffi::CString,
    fs,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};

const DIR: &str = "/run/opendap/vdev";

// linux/uhid.h: event types, report types, and the packed event's size and field offsets.
const UHID_START: u32 = 2;
const UHID_OPEN: u32 = 4;
const UHID_CLOSE: u32 = 5;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
const UHID_FEATURE_REPORT: u8 = 0;
const UHID_INPUT_REPORT: u8 = 2;
const EVENT_SIZE: usize = 4376;
const BUS_USB: u16 = 0x03;
const BUS_VIRTUAL: u16 = 0x06;

/// A ThingM blink(1), firmware 1 (one RGB LED): feature report 1, eight bytes.
const LED_DESCRIPTOR: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (vendor)
    0x09, 0x01, // Usage (1)
    0xA1, 0x01, // Collection (Application)
    0x15, 0x00, 0x26, 0xFF, 0x00, // Logical 0..255
    0x75, 0x08, 0x95, 0x08, // 8 × 8 bits
    0x85, 0x01, // Report ID 1
    0x09, 0x00, 0xB2, 0x02, 0x01, // Feature (Data, Var, Abs, Buffered)
    0xC0,
];

/// A sensor hub with one ambient light sensor (HID Usage Tables, Sensors page 0x20): report 1,
/// features reporting state, power state and report interval; input illuminance in lux.
const ALS_DESCRIPTOR: &[u8] = &[
    0x05, 0x20, // Usage Page (Sensors)
    0x09, 0x41, // Usage (Ambient Light)
    0xA1, 0x00, // Collection (Physical)
    0x85, 0x01, // Report ID 1
    0x0A, 0x16, 0x03, 0x15, 0x00, 0x25, 0x05, 0x75, 0x08, 0x95, 0x01, 0xB1, 0x02, // Reporting state
    0x0A, 0x19, 0x03, 0x15, 0x00, 0x25, 0x05, 0x75, 0x08, 0x95, 0x01, 0xB1, 0x02, // Power state
    0x0A, 0x0E, 0x03, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0xFF, 0xFF, 0x75, 0x20, 0x95, 0x01, 0xB1, 0x02, // Report interval
    0x0A, 0xD1, 0x04, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0xFF, 0x7F, 0x75, 0x20, 0x95, 0x01, 0x81, 0x02, // Illuminance
    0xC0,
];

struct Uhid {
    fd: OwnedFd,
    open: bool,
}

impl Uhid {
    fn create(name: &str, bus: u16, vendor: u32, product: u32, descriptor: &[u8]) -> io::Result<Self> {
        let path = CString::new("/dev/uhid").expect("no NUL");
        // SAFETY: a valid C string; the returned descriptor is owned below.
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut ev = [0u8; EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        ev[4..4 + name.len()].copy_from_slice(name.as_bytes());
        ev[260..262].copy_from_slice(&(descriptor.len() as u16).to_ne_bytes());
        ev[262..264].copy_from_slice(&bus.to_ne_bytes());
        ev[264..268].copy_from_slice(&vendor.to_ne_bytes());
        ev[268..272].copy_from_slice(&product.to_ne_bytes());
        ev[280..280 + descriptor.len()].copy_from_slice(descriptor);
        let uhid = Uhid { fd, open: false };
        uhid.write(&ev)?;
        Ok(uhid)
    }

    fn write(&self, ev: &[u8; EVENT_SIZE]) -> io::Result<()> {
        // SAFETY: writes the whole buffer from a valid pointer to an owned descriptor.
        let n = unsafe { libc::write(self.fd.as_raw_fd(), ev.as_ptr().cast(), EVENT_SIZE) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn read(&self) -> io::Result<[u8; EVENT_SIZE]> {
        let mut ev = [0u8; EVENT_SIZE];
        // SAFETY: reads at most the buffer's size into it.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), ev.as_mut_ptr().cast(), EVENT_SIZE) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ev)
    }

    fn reply_get(&self, id: u32, data: &[u8]) -> io::Result<()> {
        let mut ev = [0u8; EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[12..12 + data.len()].copy_from_slice(data);
        self.write(&ev)
    }

    fn reply_set(&self, id: u32) -> io::Result<()> {
        let mut ev = [0u8; EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        self.write(&ev)
    }

    fn input(&self, data: &[u8]) -> io::Result<()> {
        let mut ev = [0u8; EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[6..6 + data.len()].copy_from_slice(data);
        self.write(&ev)
    }
}

/// A file replaced whole, so a reader never sees half of it.
fn publish(name: &str, text: &str) {
    let path = Path::new(DIR).join(name);
    let tmp = path.with_extension("new");
    if fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, &path)).is_err() {
        eprintln!("odap-vdev-hw: cannot write {}", path.display());
    }
}

struct Board {
    led: Uhid,
    als: Uhid,
    rgb: [u8; 3],
    lux: u32,
    // The sensor's features, as the driver last set them.
    reporting: u8,
    power: u8,
    interval: u32,
}

impl Board {
    fn led_event(&mut self) -> io::Result<()> {
        let ev = self.led.read()?;
        let kind = u32::from_ne_bytes(ev[0..4].try_into().expect("4 bytes"));
        let id = u32::from_ne_bytes(ev[4..8].try_into().expect("4 bytes"));
        match kind {
            // hid-led asks the firmware version: '1', one RGB LED.
            UHID_GET_REPORT => self.led.reply_get(id, &[1, b'v', 0, b'1', b'0', 0, 0, 0, 0]),
            UHID_SET_REPORT => {
                let size = u16::from_ne_bytes([ev[10], ev[11]]) as usize;
                let data = &ev[12..12 + size.min(64)];
                // 'c': fade to a colour (r, g, b, fade time, LED); 'n': set it now.
                if data.len() >= 5 && data[0] == 1 && (data[1] == b'c' || data[1] == b'n') {
                    let rgb = [data[2], data[3], data[4]];
                    if rgb != self.rgb {
                        self.rgb = rgb;
                        let line = format!("{} {} {}\n", rgb[0], rgb[1], rgb[2]);
                        publish("led", &line);
                        notify(&format!("led {line}"));
                    }
                }
                self.led.reply_set(id)
            }
            _ => Ok(()),
        }
    }

    fn feature(&self) -> [u8; 7] {
        let i = self.interval.to_le_bytes();
        [1, self.reporting, self.power, i[0], i[1], i[2], i[3]]
    }

    fn reading(&self) -> [u8; 5] {
        let l = self.lux.to_le_bytes();
        [1, l[0], l[1], l[2], l[3]]
    }

    fn als_event(&mut self) -> io::Result<()> {
        let ev = self.als.read()?;
        let kind = u32::from_ne_bytes(ev[0..4].try_into().expect("4 bytes"));
        let id = u32::from_ne_bytes(ev[4..8].try_into().expect("4 bytes"));
        match kind {
            UHID_START => Ok(()),
            UHID_OPEN => {
                self.als.open = true;
                self.als.input(&self.reading())
            }
            UHID_CLOSE => {
                self.als.open = false;
                Ok(())
            }
            UHID_GET_REPORT => match ev[9] {
                UHID_FEATURE_REPORT => self.als.reply_get(id, &self.feature()),
                UHID_INPUT_REPORT => self.als.reply_get(id, &self.reading()),
                _ => self.als.reply_get(id, &[1]),
            },
            UHID_SET_REPORT => {
                let size = u16::from_ne_bytes([ev[10], ev[11]]) as usize;
                let data = &ev[12..12 + size.min(64)];
                if ev[9] == UHID_FEATURE_REPORT && data.len() >= 7 && data[0] == 1 {
                    self.reporting = data[1];
                    self.power = data[2];
                    self.interval = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
                }
                self.als.reply_set(id)
            }
            _ => Ok(()),
        }
    }

    /// A command from the host: `als <lux>`.
    fn command(&mut self, line: &str) -> io::Result<()> {
        let mut words = line.split_whitespace();
        match (words.next(), words.next().and_then(|v| v.parse::<f64>().ok())) {
            (Some("als"), Some(lux)) => {
                self.lux = lux.clamp(0.0, i32::MAX as f64).round() as u32;
                publish("als", &format!("{}\n", self.lux));
                if self.als.open {
                    self.als.input(&self.reading())?;
                }
            }
            _ => eprintln!("odap-vdev-hw: unknown command: {line}"),
        }
        Ok(())
    }
}

/// A change to whoever holds the events FIFO open; nobody reading, nothing written, no wait.
fn notify(line: &str) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    if let Ok(mut f) = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(Path::new(DIR).join("events"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn mkfifo(name: &str, mode: libc::mode_t) -> io::Result<std::path::PathBuf> {
    let path = Path::new(DIR).join(name);
    let _ = fs::remove_file(&path);
    let c = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
    // SAFETY: a valid C string.
    if unsafe { libc::mkfifo(c.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(path)
}

fn fifo() -> io::Result<fs::File> {
    mkfifo("events", 0o640)?;
    let path = Path::new(DIR).join("hw");
    let _ = fs::remove_file(&path);
    let c = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
    // SAFETY: a valid C string.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o620) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Read and write, so it never reports the end when a writer leaves.
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags_nonblock()
        .open(&path)
}

trait NonBlock {
    fn custom_flags_nonblock(&mut self) -> &mut Self;
}

impl NonBlock for fs::OpenOptions {
    fn custom_flags_nonblock(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(libc::O_NONBLOCK)
    }
}

/// Whether a capture stream runs, and whether there is any capture device at all.
fn microphone() -> (bool, bool) {
    let mut any = false;
    for card in fs::read_dir("/proc/asound").into_iter().flatten().flatten() {
        if !card.file_name().to_string_lossy().starts_with("card") {
            continue;
        }
        for pcm in fs::read_dir(card.path()).into_iter().flatten().flatten() {
            if !pcm.file_name().to_string_lossy().ends_with('c') {
                continue;
            }
            any = true;
            for sub in fs::read_dir(pcm.path()).into_iter().flatten().flatten() {
                if fs::read_to_string(sub.path().join("status")).is_ok_and(|t| t.contains("RUNNING")) {
                    return (true, true);
                }
            }
        }
    }
    (false, any)
}

fn main() -> io::Result<()> {
    fs::create_dir_all(DIR)?;
    let mut board = Board {
        led: Uhid::create("ThingM blink(1)", BUS_USB, 0x27b8, 0x01ed, LED_DESCRIPTOR)?,
        als: Uhid::create("OpenDAP virtual ambient light", BUS_VIRTUAL, 0x1209, 0x0da1, ALS_DESCRIPTOR)?,
        rgb: [0; 3],
        lux: 120,
        reporting: 0,
        power: 0,
        interval: 0,
    };
    publish("led", "0 0 0\n");
    publish("als", &format!("{}\n", board.lux));
    let mut commands = fifo()?;
    let mut pending = String::new();
    let mut mic_live = false;
    publish("mic", "0\n");
    loop {
        let (live, capture) = microphone();
        if live != mic_live {
            mic_live = live;
            publish("mic", if live { "1\n" } else { "0\n" });
            notify(if live { "mic 1\n" } else { "mic 0\n" });
        }
        let timeout = if capture { 1000 } else { -1 };
        let mut fds = [
            libc::pollfd { fd: board.led.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: board.als.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: commands.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: a valid array of pollfd for its length.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            board.led_event()?;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            board.als_event()?;
        }
        if fds[2].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 512];
            match commands.read(&mut buf) {
                Ok(n) => pending.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            while let Some(i) = pending.find('\n') {
                let line: String = pending.drain(..=i).collect();
                board.command(line.trim())?;
            }
        }
    }
}
