//! A minimal RFB (VNC) client, RFC 6143: what a simulator window needs from QEMU's VNC server on
//! localhost, and nothing more.
//!
//! - Version 3.8, security "None" (the server listens on the loopback only).
//! - 32-bit true colour, little-endian `0x00RRGGBB` pixels, "Raw" encoding.
//! - The "DesktopSize" pseudo-encoding, so a mode change resizes the frame.
//! - Pointer events (an absolute pointer: QEMU routes them to the guest's tablet).
//!
//! The connection splits into a [`Reader`], which a thread runs to receive frames, and a
//! [`Writer`], which sends input from wherever it comes.

use std::{
    io::{self, Read, Write},
    net::TcpStream,
};

/// The frame as the server last sent it: `width × height` pixels, B, G, R, X bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub bgrx: Vec<u8>,
}

/// What a received message changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Pixels changed in this rectangle.
    Pixels { x: u16, y: u16, w: u16, h: u16 },
    /// The desktop has a new size (the frame is cleared to it).
    Resized,
    /// A message that changes nothing on screen (a bell, a clipboard text).
    Other,
}

pub struct Reader {
    stream: TcpStream,
    pub frame: Frame,
}

pub struct Writer {
    stream: TcpStream,
    width: u16,
    height: u16,
}

const RAW: i32 = 0;
const DESKTOP_SIZE: i32 = -223;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

/// Connects, negotiates and asks for the whole frame.
pub fn connect(addr: &str) -> io::Result<(Reader, Writer)> {
    let mut s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;

    let mut version = [0u8; 12];
    s.read_exact(&mut version)?;
    if !version.starts_with(b"RFB 003.") {
        return Err(invalid("not an RFB server"));
    }
    s.write_all(b"RFB 003.008\n")?;

    let mut n = [0u8; 1];
    s.read_exact(&mut n)?;
    if n[0] == 0 {
        return Err(invalid("the server refused the connection"));
    }
    let mut types = vec![0u8; n[0] as usize];
    s.read_exact(&mut types)?;
    if !types.contains(&1) {
        return Err(invalid("the server requires authentication"));
    }
    s.write_all(&[1])?;
    let mut result = [0u8; 4];
    s.read_exact(&mut result)?;
    if result != [0, 0, 0, 0] {
        return Err(invalid("security handshake failed"));
    }

    // ClientInit: share the desktop. ServerInit: size, pixel format, name.
    s.write_all(&[1])?;
    let mut init = [0u8; 24];
    s.read_exact(&mut init)?;
    let (width, height) = (u16_at(&init, 0), u16_at(&init, 2));
    let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
    let mut name = vec![0u8; name_len];
    s.read_exact(&mut name)?;

    // SetPixelFormat: 32 bpp, depth 24, little-endian, true colour, 8 bits per channel.
    let mut fmt = vec![0u8, 0, 0, 0, 32, 24, 0, 1];
    fmt.extend_from_slice(&255u16.to_be_bytes());
    fmt.extend_from_slice(&255u16.to_be_bytes());
    fmt.extend_from_slice(&255u16.to_be_bytes());
    fmt.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
    s.write_all(&fmt)?;

    // SetEncodings: Raw, DesktopSize.
    let mut enc = vec![2u8, 0];
    enc.extend_from_slice(&2u16.to_be_bytes());
    enc.extend_from_slice(&RAW.to_be_bytes());
    enc.extend_from_slice(&DESKTOP_SIZE.to_be_bytes());
    s.write_all(&enc)?;

    let mut writer = Writer { stream: s.try_clone()?, width, height };
    writer.request(false)?;
    let frame = Frame { width, height, bgrx: vec![0; width as usize * height as usize * 4] };
    Ok((Reader { stream: s, frame }, writer))
}

impl Reader {
    /// Reads one server message and applies it to `frame`. Blocks until one arrives.
    pub fn next(&mut self) -> io::Result<Vec<Update>> {
        let mut kind = [0u8; 1];
        self.stream.read_exact(&mut kind)?;
        match kind[0] {
            0 => {
                let mut head = [0u8; 3];
                self.stream.read_exact(&mut head)?;
                let rects = u16_at(&head, 1);
                let mut out = Vec::with_capacity(rects as usize);
                for _ in 0..rects {
                    let mut r = [0u8; 12];
                    self.stream.read_exact(&mut r)?;
                    let (x, y, w, h) = (u16_at(&r, 0), u16_at(&r, 2), u16_at(&r, 4), u16_at(&r, 6));
                    let encoding = i32::from_be_bytes([r[8], r[9], r[10], r[11]]);
                    match encoding {
                        RAW => {
                            let mut pixels = vec![0u8; w as usize * h as usize * 4];
                            self.stream.read_exact(&mut pixels)?;
                            self.blit(x, y, w, h, &pixels);
                            out.push(Update::Pixels { x, y, w, h });
                        }
                        DESKTOP_SIZE => {
                            self.frame = Frame { width: w, height: h, bgrx: vec![0; w as usize * h as usize * 4] };
                            out.push(Update::Resized);
                        }
                        _ => return Err(invalid("an encoding that was not asked for")),
                    }
                }
                Ok(out)
            }
            1 => {
                // SetColourMapEntries: not used in true colour; skip it.
                let mut head = [0u8; 5];
                self.stream.read_exact(&mut head)?;
                let n = u16_at(&head, 3) as usize;
                let mut skip = vec![0u8; n * 6];
                self.stream.read_exact(&mut skip)?;
                Ok(vec![Update::Other])
            }
            2 => Ok(vec![Update::Other]),
            3 => {
                let mut head = [0u8; 7];
                self.stream.read_exact(&mut head)?;
                let n = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
                let mut skip = vec![0u8; n];
                self.stream.read_exact(&mut skip)?;
                Ok(vec![Update::Other])
            }
            _ => Err(invalid("an unknown server message")),
        }
    }

    fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, pixels: &[u8]) {
        let fw = self.frame.width as usize;
        for row in 0..h as usize {
            let dy = y as usize + row;
            if dy >= self.frame.height as usize {
                break;
            }
            let src = &pixels[row * w as usize * 4..][..w as usize * 4];
            let start = (dy * fw + x as usize) * 4;
            let len = (w as usize * 4).min(self.frame.bgrx.len().saturating_sub(start));
            self.frame.bgrx[start..start + len].copy_from_slice(&src[..len]);
        }
    }
}

impl Writer {
    /// Asks for the next frame: `incremental` for changes only.
    pub fn request(&mut self, incremental: bool) -> io::Result<()> {
        let mut m = vec![3u8, incremental as u8, 0, 0, 0, 0];
        m.extend_from_slice(&self.width.to_be_bytes());
        m.extend_from_slice(&self.height.to_be_bytes());
        self.stream.write_all(&m)
    }

    /// The desktop changed size: later requests cover the new one.
    pub fn resized(&mut self, width: u16, height: u16) {
        (self.width, self.height) = (width, height);
    }

    /// The pointer at a frame pixel, with the primary button down or up.
    pub fn pointer(&mut self, x: u16, y: u16, pressed: bool) -> io::Result<()> {
        let mut m = vec![5u8, pressed as u8];
        m.extend_from_slice(&x.min(self.width.saturating_sub(1)).to_be_bytes());
        m.extend_from_slice(&y.min(self.height.saturating_sub(1)).to_be_bytes());
        self.stream.write_all(&m)
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread};

    use super::*;

    /// A tiny RFB server that sends a 2 × 1 desktop and one raw update.
    fn serve(listener: TcpListener) {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"RFB 003.008\n").unwrap();
        let mut v = [0u8; 12];
        s.read_exact(&mut v).unwrap();
        s.write_all(&[1, 1]).unwrap();
        let mut choice = [0u8; 1];
        s.read_exact(&mut choice).unwrap();
        s.write_all(&[0, 0, 0, 0]).unwrap();
        let mut shared = [0u8; 1];
        s.read_exact(&mut shared).unwrap();
        let mut init = vec![0, 2, 0, 1];
        init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
        init.extend_from_slice(&3u32.to_be_bytes());
        init.extend_from_slice(b"VT1");
        s.write_all(&init).unwrap();
        // SetPixelFormat (20), SetEncodings (4 + 8), FramebufferUpdateRequest (10).
        let mut client = [0u8; 42];
        s.read_exact(&mut client).unwrap();
        let mut update = vec![0u8, 0, 0, 1, 0, 1, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0];
        update.extend_from_slice(&[0x10, 0x20, 0x30, 0]);
        s.write_all(&update).unwrap();
    }

    #[test]
    fn negotiates_and_applies_a_raw_rectangle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || serve(listener));
        let (mut reader, _writer) = connect(&addr).unwrap();
        assert_eq!((reader.frame.width, reader.frame.height), (2, 1));
        let updates = reader.next().unwrap();
        assert_eq!(updates, vec![Update::Pixels { x: 1, y: 0, w: 1, h: 1 }]);
        assert_eq!(&reader.frame.bgrx[4..8], &[0x10, 0x20, 0x30, 0]);
        server.join().unwrap();
    }
}
