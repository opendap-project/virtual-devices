"""odap-vdev: run and drive virtual OpenDAP devices.

Every command acts on one model (--model, or ODAP_MODEL; default virtual-t1). Output is text for
people; --json prints the machine-readable form where a command has one. Exit status: 0 success,
1 the operation failed, 2 a usage or data error.
"""
import argparse
import json
import os
import sys
import time

from . import compose, workspace
from .instance import Instance, InstanceError
from .qmp import Qmp, QmpError


def parser():
    p = argparse.ArgumentParser(prog="odap-vdev", description="Run and drive virtual OpenDAP devices.")
    p.add_argument("--model", default=os.environ.get("ODAP_MODEL", "virtual-t1"), help="model to act on (default: virtual-t1)")
    sub = p.add_subparsers(dest="command", required=True, metavar="COMMAND")

    s = sub.add_parser("models", help="the models this checkout defines")
    s.add_argument("--json", action="store_true")
    sub.add_parser("describe", help="the model's device description (TOML)")
    sub.add_parser("frame", help="how a window draws the device around its screen (JSON)")
    s = sub.add_parser("qemu-args", help="the model's QEMU arguments, one per line (for inspection)")
    s.add_argument("values", nargs="*", metavar="KEY=VALUE")

    s = sub.add_parser("start", help="boot the model in the background")
    s.add_argument("--window", action="store_true", help="also show its display in a window, at the panel's pixels")
    s.add_argument("--on-top", action=argparse.BooleanOptionalAction, default=True,
                   help="keep the window above other windows (default; --no-on-top to turn it off)")
    s.add_argument("--frame", action=argparse.BooleanOptionalAction, default=True,
                   help="draw the device around its display, with the sidebar of hardware actions (default)")
    s.add_argument("--wait", action="store_true", help="return once the management link answers")
    s = sub.add_parser("window", help="open the simulator window of the running device")
    s.add_argument("--on-top", action=argparse.BooleanOptionalAction, default=True)
    s.add_argument("--frame", action=argparse.BooleanOptionalAction, default=True)
    sub.add_parser("stop", help="orderly power-off")
    s = sub.add_parser("reboot", help="reboot, optionally into another boot mode")
    s.add_argument("--mode", default="normal", help="normal, debug, or a mode of the board (reboot --list)")
    s.add_argument("--debug", action="store_const", const="debug", dest="mode", help="the same as --mode debug")
    s.add_argument("--list", action="store_true", help="the boot modes the device has")
    s = sub.add_parser("hw", help="the virtual hardware's physical state: card, USB, charger, battery")
    s.add_argument("--json", action="store_true")
    s = sub.add_parser("status", help="state and endpoints")
    s.add_argument("--json", action="store_true")
    s = sub.add_parser("ssh", help="a shell, or a command, on the device")
    s.add_argument("argv", nargs=argparse.REMAINDER)
    sub.add_parser("console", help="follow the serial console")

    screen = sub.add_parser("screen", help="the display").add_subparsers(dest="action", required=True)
    s = screen.add_parser("capture", help="save the display as PNG")
    s.add_argument("file")

    inp = sub.add_parser("input", help="controls and touch").add_subparsers(dest="action", required=True)
    s = inp.add_parser("press", help="press a control by its role, or several at once (a combination)")
    s.add_argument("roles", nargs="+", metavar="role",
                   help="roles of the model's controls: action, volume-up, volume-down, main…; "
                        "several are pressed together, in order, and released together")
    s.add_argument("--hold", type=int, default=100, metavar="MS",
                   help="how long; main held as long as the board's PMIC time cuts the power")
    s = inp.add_parser("key", help="press or release a control by its role, as a finger does")
    s.add_argument("role")
    s.add_argument("state", choices=["down", "up"])
    s = inp.add_parser("keyboard", help="a key of a keyboard attached to the device, by its Linux name (left, enter…): not a control")
    s.add_argument("key")
    s.add_argument("state", choices=["down", "up"])
    s = inp.add_parser("tap", help="touch a display pixel")
    s.add_argument("x", type=int); s.add_argument("y", type=int)
    s.add_argument("--hold", type=int, default=80, metavar="MS")
    s = inp.add_parser("swipe", help="drag from one pixel to another")
    for a in ("x1", "y1", "x2", "y2"):
        s.add_argument(a, type=int)
    s.add_argument("--duration", type=int, default=250, metavar="MS")

    usb = sub.add_parser("usb", help="USB devices on the USB-C port (host mode)").add_subparsers(dest="action", required=True)
    usb.add_parser("list")
    s = usb.add_parser("plug", help="plug a device")
    s.add_argument("kind", choices=["storage", "audio"])
    s.add_argument("file", nargs="?", help="raw disk image (storage; default: the simulator's own 1 GiB stick, in the instance's state)")
    s.add_argument("--id")
    s = usb.add_parser("unplug")
    s.add_argument("id")

    sd = sub.add_parser("sd", help="the microSD slot").add_subparsers(dest="action", required=True)
    s = sd.add_parser("insert", help="insert a card: FILE, or the hardware's default card ([microsd] card_size, 1 GiB)")
    s.add_argument("file", nargs="?", help="raw image of the card (default: the hardware's own, in the instance's state)")
    sd.add_parser("eject")

    power = sub.add_parser("power", help="the simulated battery and charger").add_subparsers(dest="action", required=True)
    s = power.add_parser("battery", help="the level in percent, or set it")
    s.add_argument("level", nargs="?", type=int)
    s = power.add_parser("charger", help="whether a charger is connected, or connect or disconnect it")
    s.add_argument("state", nargs="?", choices=["on", "off"])
    return p


def display_of(inst):
    d = compose.display(inst.model)
    if not d:
        raise InstanceError(f"{inst.name} has no display")
    return d["width_px"], d["height_px"]


def in_display(inst, *points):
    w, h = display_of(inst)
    for x, y in points:
        if not (0 <= x < w and 0 <= y < h):
            raise ValueError(f"({x}, {y}) is outside the {w} x {h} display")
    return w, h


def qmp(inst):
    if not inst.pid():
        raise InstanceError(f"{inst.name} is not running (odap-vdev start)")
    return Qmp(inst.qmp_socket)


def run(args):
    if args.command == "models":
        rows = compose.models()
        if args.json:
            print(json.dumps([{"id": i, "name": n, "model": s} for i, n, s in rows]))
        else:
            for i, n, s in rows:
                print(f"{s}\t{n}\t{i}")
        return 0
    if args.command == "describe":
        sys.stdout.write(compose.description(compose.load(compose.model_path(args.model))))
        return 0
    if args.command == "frame":
        f = compose.frame(compose.load(compose.model_path(args.model)))
        if f is None:
            raise ValueError(f"{args.model} describes no frame")
        print(json.dumps(f))
        return 0
    if args.command == "qemu-args":
        values = dict(v.split("=", 1) for v in args.values)
        print("\n".join(compose.qemu_args(compose.load(compose.model_path(args.model)), values)))
        return 0

    inst = Instance(args.model)
    c = args.command
    if c == "start":
        lan = inst.start(window=args.window, on_top=args.on_top, framed=args.frame)
        if lan:
            net = lan
        elif os.environ.get("ODAP_NET") == "user":
            net = "off (ODAP_NET=user; management link only)"
        else:
            net = "none (management link only; toolkit-host odapkit-vmnet install)"
        print(f"{inst.name} started; LAN: {net}")
        if args.wait and not inst.wait_ssh():
            raise InstanceError("the management link did not answer")
    elif c == "window":
        if not inst.pid():
            raise InstanceError(f"{inst.name} is not running (odap-vdev start)")
        inst.open_window(on_top=args.on_top, framed=args.frame)
        print("window opened")
    elif c == "stop":
        inst.stop()
        print("stopped")
    elif c == "reboot":
        if args.list:
            print("\n".join(inst.boot_modes()))
            return 0
        inst.reboot(args.mode)
        print("rebooting" + ("" if args.mode == "normal" else f" into the {args.mode} boot mode, once"))
    elif c == "status":
        st = inst.status()
        if args.json:
            print(json.dumps(st))
        else:
            print(f"{st['name']} ({st['model']}): {st['state']}" + (f", pid {st['pid']}" if st["pid"] else ""))
            for k, v in st["endpoints"].items():
                print(f"  {k}: {v}")
            print(f"  image: {st['image']['path']} ({'present' if st['image']['present'] else 'missing'})")
    elif c == "hw":
        hw = inst.hardware()
        if args.json:
            print(json.dumps(hw))
        else:
            print(f"microSD: {'in' if hw['sd'] else 'out'}; USB: {', '.join(hw['usb']) or 'none'}; "
                  f"charger: {'on' if hw['charger'] else 'off'}; battery: {hw['battery'] if hw['battery'] is not None else '?'} %")
    elif c == "ssh":
        argv = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
        os.execvp("ssh", inst.ssh_argv(argv))
    elif c == "console":
        os.execvp("tail", ["tail", "-f", str(inst.console_log)])
    elif c == "screen":
        q = qmp(inst)
        q.cmd("screendump", filename=os.path.abspath(args.file), format="png")
        print(args.file)
    elif c == "input":
        q = qmp(inst)
        if args.action == "keyboard":
            # QEMU's key code for a Linux key name: the same word, except where QEMU names it otherwise.
            qcode = {"enter": "ret", "escape": "esc", "space": "spc", "leftshift": "shift",
                     "leftctrl": "ctrl", "leftalt": "alt"}.get(args.key, args.key)
            q.key(qcode, args.state == "down")
            print(f"keyboard {args.key} {args.state}")
        elif args.action == "key":
            qcodes = [k["qcode"] for k in compose.controls(inst.model) if k["role"] == args.role and k.get("qcode")]
            if not qcodes:
                roles = sorted({k["role"] for k in compose.controls(inst.model)})
                raise ValueError(f"no control with role {args.role!r} (roles: {', '.join(roles)})")
            q.key(qcodes[0], args.state == "down")
            print(f"{args.role} {args.state}")
        elif args.action == "press":
            controls = compose.controls(inst.model)
            keys = []
            for role in args.roles:
                qcodes = [k["qcode"] for k in controls if k["role"] == role and k.get("qcode")]
                if not qcodes:
                    roles = sorted({k["role"] for k in controls})
                    raise ValueError(f"no control with role {role!r} (roles: {', '.join(roles)})")
                keys.append(qcodes[0])
            # Held past the PMIC's time, `main` cuts the power, as the board's PMIC does in hardware:
            # no orderly shutdown, like pulling the battery.
            cut = compose.forced_power_off_ms(inst.model) if "main" in args.roles else None
            for k in keys:
                q.key(k, True)
            if cut is not None and args.hold >= cut:
                time.sleep(cut / 1000)
                q.cmd("quit")
                print(f"held {' + '.join(args.roles)} {cut} ms: the PMIC cut the power")
                return 0
            time.sleep(args.hold / 1000)
            try:
                for k in keys:
                    q.key(k, False)
            except QmpError:
                pass  # the instance powered off while the controls were held
            print(f"pressed {' + '.join(args.roles)} for {args.hold} ms")
        elif args.action == "tap":
            w, h = in_display(inst, (args.x, args.y))
            q.pointer(args.x, args.y, w, h)
            q.pointer(args.x, args.y, w, h, True)
            time.sleep(args.hold / 1000)
            q.pointer(args.x, args.y, w, h, False)
            print(f"tapped {args.x},{args.y}")
        else:
            w, h = in_display(inst, (args.x1, args.y1), (args.x2, args.y2))
            steps = 20
            q.pointer(args.x1, args.y1, w, h)
            q.pointer(args.x1, args.y1, w, h, True)
            for i in range(1, steps + 1):
                time.sleep(args.duration / 1000 / steps)
                q.pointer(args.x1 + (args.x2 - args.x1) * i // steps, args.y1 + (args.y2 - args.y1) * i // steps, w, h)
            q.pointer(args.x2, args.y2, w, h, False)
            print(f"swiped {args.x1},{args.y1} to {args.x2},{args.y2}")
    elif c == "usb":
        q = qmp(inst)
        if args.action == "list":
            print(q.cmd("human-monitor-command", **{"command-line": "info usb"}).strip() or "no USB devices")
        elif args.action == "plug":
            if args.kind == "storage":
                image = args.file
                if not image:
                    # The simulator's own stick: sparse, kept in the instance's state.
                    path = inst.state / "usb-disk.img"
                    if not path.exists():
                        with open(path, "wb") as f:
                            f.truncate(compose.SD_DEFAULT)
                    image = str(path)
                dev = args.id or "usbdisk"
                q.cmd("blockdev-add", **{"driver": "raw", "node-name": dev, "file": {"driver": "file", "filename": os.path.abspath(image)}})
                q.cmd("device_add", driver="usb-storage", id=dev, drive=dev, bus="xhci.0")
            else:
                dev = args.id or "usbaudio"
                q.cmd("device_add", driver="usb-audio", id=dev, audiodev="usbsnd", bus="xhci.0")
            print(f"plugged {dev}")
        else:
            q.cmd("device_del", id=args.id)
            time.sleep(0.5)
            try:
                q.cmd("blockdev-del", **{"node-name": args.id})
            except QmpError:
                pass  # not a drive
            print(f"unplugged {args.id}")
    elif c == "sd":
        q = qmp(inst)
        if args.action == "insert":
            card = args.file
            if not card:
                # The hardware's card: sparse, so it takes space only as the device writes it.
                size = compose.microsd_card(inst.model)
                path = inst.state / "microsd.img"
                if not path.exists():
                    with open(path, "wb") as f:
                        f.truncate(size)
                elif path.stat().st_size != size:
                    # The card holds what the device wrote: it is never recreated to resize it.
                    print(f"note: {path} keeps its {path.stat().st_size} bytes; remove it for a {size}-byte card", file=sys.stderr)
                card = str(path)
            q.cmd("blockdev-change-medium", id="sd0", filename=os.path.abspath(card), format="raw")
            print(f"microSD inserted: {card}")
        else:
            q.cmd("eject", id="sd0", force=True)
            print("microSD ejected")
    elif c == "power":
        p = "/sys/module/test_power/parameters"
        if args.action == "battery":
            if args.level is not None:
                if not 0 <= args.level <= 100:
                    raise ValueError("battery level: 0 to 100")
                inst.ssh(f"echo {args.level} > {p}/battery_capacity")
            print(inst.ssh("cat", "/sys/class/power_supply/test_battery/capacity").stdout.strip() + "%")
        else:
            if args.state:
                status = "charging" if args.state == "on" else "discharging"
                inst.ssh(f"echo {args.state} > {p}/usb_online && echo {status} > {p}/battery_status")
            online = inst.ssh("cat", "/sys/class/power_supply/test_usb/online").stdout.strip() == "1"
            status = inst.ssh("cat", "/sys/class/power_supply/test_battery/status").stdout.strip()
            print(f"charger {'on' if online else 'off'}, battery {status.lower()}")
    return 0


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        return run(args)
    except (ValueError, compose.CompositionError, workspace.NotFound) as e:
        print(f"odap-vdev: {e}", file=sys.stderr)
        return 2
    except (InstanceError, QmpError, OSError) as e:
        print(f"odap-vdev: {e}", file=sys.stderr)
        return 1
