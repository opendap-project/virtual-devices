#!/usr/bin/env python3
"""Drive a running virtual T1 through QEMU's QMP socket (headless automation).

  dap-qmp.py screenshot FILE.png       capture the display
  dap-qmp.py press BUTTON [ms]         action | vol-up | vol-down | power (hold for ms, default 100)
  dap-qmp.py tap X Y [ms]              touch at display pixel (320 x 480 portrait)
  dap-qmp.py swipe X1 Y1 X2 Y2 [ms]    drag from one pixel to another
  dap-qmp.py usb list                  USB devices plugged into the virtual T1
  dap-qmp.py usb plug storage FILE [ID]  plug a USB drive backed by a raw disk image
  dap-qmp.py usb plug dac [ID]         plug a USB audio device (plays on the host's audio output)
  dap-qmp.py usb unplug ID             unplug it again
  dap-qmp.py sd insert FILE           insert a microSD card backed by a raw image (exFAT, as a T1 card)
  dap-qmp.py sd eject                  remove it
  dap-qmp.py raw COMMAND [JSON-ARGS]   any QMP command

Touch goes through the virtual T1's tablet, which dap-shell treats as one finger: a full contact
with position, unlike the multi-touch device, which QEMU 11.1.2 cannot drive from QMP (it passes
BTN_TOUCH only). In the guest, `dap-inject` injects through uinput instead.
"""
import json, os, socket, sys, time

SOCK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "run", "qmp.sock")
WIDTH, HEIGHT = 320, 480  # the virtual T1's display (its device description)
# Virtual T1 button mapping (QEMU key codes → Linux input keys).
BUTTONS = {"action": "audioplay", "vol-up": "volumeup", "vol-down": "volumedown", "power": "power"}

class QMP:
    def __init__(self, path=SOCK):
        self.s = socket.socket(socket.AF_UNIX); self.s.connect(path); self.f = self.s.makefile("rw")
        self._read(); self.cmd("qmp_capabilities")
    def _read(self):
        while True:
            msg = json.loads(self.f.readline())
            if "event" not in msg:
                return msg
    def cmd(self, name, **args):
        self.f.write(json.dumps({"execute": name, "arguments": args}) + "\n"); self.f.flush()
        r = self._read()
        if "error" in r:
            raise SystemExit(f"QMP {name}: {r['error']['desc']}")
        return r.get("return")

def key(q, qcode, down):
    q.cmd("input-send-event", events=[{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}}])

def pointer(q, x, y, down=None):
    # Absolute axes span 0..0x7FFF over the display.
    ev = [{"type": "abs", "data": {"axis": "x", "value": x * 0x7FFF // (WIDTH - 1)}},
          {"type": "abs", "data": {"axis": "y", "value": y * 0x7FFF // (HEIGHT - 1)}}]
    if down is not None:
        ev.append({"type": "btn", "data": {"down": down, "button": "left"}})
    q.cmd("input-send-event", events=ev)

def usb(q, a):
    if a[:1] == ["list"]:
        print(q.cmd("human-monitor-command", **{"command-line": "info usb"}).strip() or "no USB devices")
    elif a[:2] == ["plug", "storage"] and len(a) >= 3:
        path, dev = os.path.abspath(a[2]), (a[3] if len(a) > 3 else "usbdisk")
        q.cmd("blockdev-add", **{"driver": "raw", "node-name": dev, "file": {"driver": "file", "filename": path}})
        q.cmd("device_add", driver="usb-storage", id=dev, drive=dev, bus="xhci.0")
        print(f"plugged {dev}: {path}")
    elif a[:2] == ["plug", "dac"]:
        dev = a[2] if len(a) > 2 else "usbdac"
        q.cmd("device_add", driver="usb-audio", id=dev, audiodev="usbsnd", bus="xhci.0")
        print(f"plugged {dev}: USB audio device")
    elif a[:1] == ["unplug"] and len(a) == 2:
        q.cmd("device_del", id=a[1])
        time.sleep(0.5)
        try:
            q.cmd("blockdev-del", **{"node-name": a[1]})
        except SystemExit:
            pass  # not a drive
        print(f"unplugged {a[1]}")
    else:
        raise SystemExit(__doc__)

def main(a):
    if not a:
        raise SystemExit(__doc__)
    q = QMP()
    if a[0] == "screenshot":
        q.cmd("screendump", filename=os.path.abspath(a[1]), format="png"); print(a[1])
    elif a[0] == "press":
        hold = int(a[2]) if len(a) > 2 else 100
        key(q, BUTTONS[a[1]], True); time.sleep(hold / 1000)
        try:
            key(q, BUTTONS[a[1]], False)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the instance powered off while the button was held (a long press of power)
    elif a[0] == "tap":
        x, y = int(a[1]), int(a[2]); hold = int(a[3]) if len(a) > 3 else 80
        pointer(q, x, y); pointer(q, x, y, True); time.sleep(hold / 1000); pointer(q, x, y, False)
    elif a[0] == "swipe":
        x1, y1, x2, y2 = map(int, a[1:5]); dur = int(a[5]) if len(a) > 5 else 250; steps = 20
        pointer(q, x1, y1); pointer(q, x1, y1, True)
        for i in range(1, steps + 1):
            time.sleep(dur / 1000 / steps)
            pointer(q, x1 + (x2 - x1) * i // steps, y1 + (y2 - y1) * i // steps)
        pointer(q, x2, y2, False)
    elif a[0] == "usb":
        usb(q, a[1:])
    elif a[:2] == ["sd", "insert"] and len(a) == 3:
        q.cmd("blockdev-change-medium", id="sd0", filename=os.path.abspath(a[2]), format="raw")
        print(f"microSD inserted: {a[2]}")
    elif a[:2] == ["sd", "eject"]:
        q.cmd("eject", id="sd0", force=True)
        print("microSD ejected")
    elif a[0] == "raw":
        print(json.dumps(q.cmd(a[1], **(json.loads(a[2]) if len(a) > 2 else {})), indent=2))
    else:
        raise SystemExit(__doc__)

if __name__ == "__main__":
    main(sys.argv[1:])
