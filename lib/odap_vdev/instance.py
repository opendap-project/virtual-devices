"""One running virtual device: its state folder, QEMU process, endpoints and management link.

State lives in $XDG_STATE_HOME/opendap/vdev/<model>/ (not in a repository): the disk on top of
the image, firmware variables, the composed description, QMP socket, console and QEMU logs.
"""
import hashlib
import json
import os
import pathlib
import platform
import re
import shutil
import signal
import subprocess
import sys
import time

from . import compose, workspace
from .qmp import Qmp

SSH_OPTS = ["-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]


class InstanceError(Exception):
    pass


def env_int(name, default):
    try:
        return int(os.environ.get(name, default))
    except ValueError:
        raise InstanceError(f"{name} must be a number")


class Instance:
    def __init__(self, model_name):
        self.model_name = model_name
        self.model = compose.load(compose.model_path(model_name))
        self.name = self.model["model"]["name"]
        self.state = workspace.state_dir(model_name)
        self.ssh_port = env_int("ODAP_SSH_PORT", 2222)
        self.vnc_display = env_int("ODAP_VNC_DISPLAY", 0)

    # Paths
    @property
    def pid_file(self): return self.state / "qemu.pid"
    @property
    def qmp_socket(self): return self.state / "qmp.sock"

    @property
    def window_qmp_socket(self): return self.state / "qmp-window.sock"
    @property
    def console_log(self): return self.state / "console.log"
    @property
    def qemu_log(self): return self.state / "qemu.log"
    @property
    def disk(self): return self.state / "disk.qcow2"

    def image(self):
        """The Blackbird OS qemu image (rfcs 0009): ODAP_IMAGE; else a blackbird-os checkout's build
        output, when there is one (a maintainer's latest build); else the user's images, where
        released images go. Users need no toolkit and no checkout."""
        if os.environ.get("ODAP_IMAGE"):
            return pathlib.Path(os.environ["ODAP_IMAGE"]).expanduser()
        try:
            built = workspace.repo_dir("blackbird-os") / "image/mkosi.output/blackbird-os-qemu.raw"
            if built.is_file() or built.with_name(built.name + ".building").exists():
                return built
        except workspace.NotFound:
            pass
        return USER_IMAGES / "blackbird-os-qemu.raw"

    def pid(self):
        try:
            pid = int(self.pid_file.read_text())
            os.kill(pid, 0)
            return pid
        except (OSError, ValueError):
            return None

    def status(self):
        pid = self.pid()
        try:
            image = str(self.image())
            image_present = pathlib.Path(image).is_file()
        except workspace.NotFound as e:
            image, image_present = str(e), False
        return {
            "schema": "opendap.vdev-status/v1",
            "model": self.model["model"]["id"],
            "name": self.name,
            "state": "running" if pid else "stopped",
            "pid": pid,
            "endpoints": {
                "ssh": f"127.0.0.1:{self.ssh_port}",
                "vnc": f"127.0.0.1:{5900 + self.vnc_display}",
                "qmp": str(self.qmp_socket),
            },
            # building: a toolkit is rebuilding it (its <image>.building marker, rfcs 0009).
            "image": {"path": image, "present": image_present,
                      "building": pathlib.Path(image + ".building").exists()},
            "dev_key": str(workspace.dev_key()),
            "console_log": str(self.console_log),
            "qemu_log": str(self.qemu_log),
            "state_dir": str(self.state),
        }

    # Management link
    def ssh_argv(self, command=()):
        return ["ssh", "-i", str(workspace.dev_key()), "-p", str(self.ssh_port), *SSH_OPTS, "root@127.0.0.1", "--", *command]

    def ssh(self, *command, check=True, capture=True):
        """Run on the device. ssh joins its arguments into one line for the remote shell, so pass
        a command line as one string when it has redirections or operators."""
        r = subprocess.run(self.ssh_argv(command), capture_output=capture, text=True)
        if check and r.returncode != 0:
            raise InstanceError(f"on the device: {' '.join(command)}: {(r.stderr or '').strip() or r.returncode}")
        return r

    # Lifecycle
    def start(self, window=False, on_top=True, framed=True):
        if self.pid():
            raise InstanceError(f"{self.name} is already running (pid {self.pid()})")
        image = self.image()
        if not image.is_file():
            raise InstanceError(f"no image at {image}: build it with toolkit-image odapkit-image build qemu")
        self.state.mkdir(parents=True, exist_ok=True)
        # The disk is an overlay of one image: recreated when the image is another one. Its identity
        # is the image's path, size and modification time when the overlay was made; the overlay's
        # own time says nothing (it changes with every write while the device runs), and an overlay
        # applied to a different base corrupts the file system.
        base_file = self.state / "disk.base"
        st = image.stat()
        identity = f"{image}\n{st.st_size}\n{st.st_mtime_ns}"
        if not self.disk.exists() or (base_file.read_text() if base_file.exists() else "") != identity:
            self.disk.unlink(missing_ok=True)
            size = compose.storage_gb(self.model)
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw", "-b", str(image), str(self.disk), f"{size}G"], check=True)
            base_file.write_text(identity)
        vars_fd = self.state / "vars.fd"
        if not vars_fd.exists():
            vars_fd.write_bytes(b"\0" * (64 << 20))
        description = self.state / "description.toml"
        description.write_text(compose.description(self.model))

        darwin = platform.system() == "Darwin"
        accel, audio = ("hvf", "coreaudio") if darwin else ("kvm", "pa")
        qemu = shutil.which("qemu-system-aarch64") or "qemu-system-aarch64"
        fw = firmware(qemu)
        display = ["-display", "none"]
        launcher = []
        # The window is the simulator window (viewer/, ADR 0001), drawn over VNC; QEMU's own
        # window (ODAP_WINDOW=qemu) is the fallback, without the frame and the sidebar.
        viewer = window and os.environ.get("ODAP_WINDOW") != "qemu" and build_viewer()
        if window and not viewer:
            zoom = "on" if os.environ.get("ODAP_WINDOW_ZOOM") == "fit" else "off"
            if darwin:
                display = ["-display", f"cocoa,zoom-to-fit={zoom}"]
                qemu = self._app_bundle(qemu)
                if on_top:
                    lib = window_on_top_library()
                    if lib:
                        # Only QEMU loads it: env sets the variable for the program it runs.
                        launcher = ["/usr/bin/env", f"DYLD_INSERT_LIBRARIES={lib}"]
            else:
                display = ["-display", f"gtk,zoom-to-fit={zoom}"]

        argv, lan = self._lan_prefix(launcher + [qemu])
        argv += compose.qemu_args(self.model, {"run.disk": str(self.disk), "run.ssh_port": self.ssh_port, "host.audio": audio})
        argv += ["-name", self.name, "-accel", accel,
                 "-drive", f"if=pflash,format=raw,readonly=on,file={fw}",
                 "-drive", f"if=pflash,format=raw,file={vars_fd}",
                 "-fw_cfg", f"name=opt/org.opendap/device-description,file={description}",
                 *display, "-vnc", f"127.0.0.1:{self.vnc_display}",
                 "-qmp", f"unix:{self.qmp_socket},server=on,wait=off",
                 # A monitor of its own for the simulator window, which keeps its connection open:
                 # a QMP socket serves one client at a time, and the tools use the first.
                 "-qmp", f"unix:{self.window_qmp_socket},server=on,wait=off",
                 "-serial", f"file:{self.console_log}"]
        # Not -daemonize: on macOS it forks after CoreAudio initializes and crashes.
        with open(self.qemu_log, "w") as log:
            p = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        self.pid_file.write_text(str(p.pid))
        time.sleep(1)
        if p.poll() is not None:
            raise InstanceError(f"QEMU exited: {self.qemu_log.read_text().strip()}")
        if viewer:
            self.open_window(viewer, on_top=on_top, framed=framed)
        return lan

    def open_window(self, viewer=None, on_top=True, framed=True):
        """The simulator window for this running instance: its display over VNC, framed by the
        device, with the sidebar. It closes when the instance stops."""
        viewer = viewer or build_viewer()
        if not viewer:
            raise InstanceError("the simulator window is not built (cargo, viewer/)")
        f = compose.frame(self.model)
        if f is None:
            framed = False
            d = compose.display(self.model)
            f = {"frame": None, "display": d, "controls": compose.controls(self.model)}
        frame = self.state / "frame.json"
        frame.write_text(json.dumps(f))
        program = self._app_bundle(viewer) if platform.system() == "Darwin" else viewer
        argv = [program, "--frame", str(frame), "--vnc", f"127.0.0.1:{5900 + self.vnc_display}",
                "--qmp", str(self.window_qmp_socket), "--checkout", str(ROOT), "--name", self.name]
        argv += [] if framed else ["--no-frame"]
        argv += [] if on_top else ["--no-on-top"]
        with open(self.state / "window.log", "w") as log:
            subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)

    def stop(self, timeout=30):
        """An orderly power-off over the management link; QEMU is stopped if the guest does not answer."""
        pid = self.pid()
        if pid:
            self.ssh("systemctl", "poweroff", check=False)
            for _ in range(timeout):
                if not self.pid():
                    break
                time.sleep(1)
            if self.pid():
                os.kill(pid, signal.SIGTERM)
        self.pid_file.unlink(missing_ok=True)

    def reboot(self, mode="normal"):
        """Restart into a boot mode, through the device's own odap-reboot (blackbird-os)."""
        r = self.ssh("odap-reboot", mode, check=False)
        if r.returncode == 2:
            raise ValueError((r.stderr or "").strip().removeprefix("odap-reboot: ") or f"no mode {mode}")
        if r.returncode not in (0, 255):  # 255: the connection closed as the device went down
            raise InstanceError((r.stderr or "").strip() or "the reboot failed")

    def hardware(self):
        """The virtual hardware's physical state (`opendap.vdev-hardware/v1`), not what the
        device's system makes of it: the card in the socket and the USB devices on the port
        (QEMU), the charger and the battery (the board's battery model, test_power)."""
        if not self.pid():
            raise InstanceError(f"{self.name} is not running (odap-vdev start)")
        q = Qmp(self.qmp_socket)
        sd = any(b.get("qdev") == "sd0" and "inserted" in b for b in q.cmd("query-block"))
        usb = re.findall(r"ID: (\S+)", q.cmd("human-monitor-command", **{"command-line": "info usb"}) or "")
        charger, battery = False, None
        r = self.ssh("cat /sys/module/test_power/parameters/usb_online /sys/module/test_power/parameters/battery_capacity", check=False)
        if r.returncode == 0:
            lines = r.stdout.split()
            charger = bool(lines) and lines[0].strip() in ("on", "1")
            battery = int(lines[1]) if len(lines) > 1 and lines[1].isdigit() else None
        return {"schema": "opendap.vdev-hardware/v1", "sd": sd, "usb": usb, "charger": charger, "battery": battery}

    def boot_modes(self):
        return self.ssh("odap-reboot", "--list").stdout.split()

    def wait_ssh(self, timeout=120):
        end = time.time() + timeout
        while time.time() < end:
            if self.ssh("true", check=False).returncode == 0:
                return True
            time.sleep(2)
        return False

    # Helpers
    def _app_bundle(self, program):
        """On macOS the window belongs to an app bundle named after the model."""
        app = self.state / f"{self.name}.app"
        (app / "Contents/MacOS").mkdir(parents=True, exist_ok=True)
        exe = pathlib.Path(program).name
        for old in (app / "Contents/MacOS").iterdir():
            old.unlink()
        link = app / "Contents/MacOS" / exe
        link.symlink_to(program)
        # The OpenDAP app icon (Blackbird UI's brand export, copied in assets/), not QEMU's generic one.
        icon = pathlib.Path(__file__).resolve().parents[2] / "assets/opendap-app.icns"
        if icon.is_file():
            (app / "Contents/Resources").mkdir(parents=True, exist_ok=True)
            shutil.copyfile(icon, app / "Contents/Resources/AppIcon.icns")
        (app / "Contents/Info.plist").write_text(f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>org.opendap.{self.model_name}</string>
  <key>CFBundleName</key><string>{self.name}</string>
  <key>CFBundleExecutable</key><string>{exe}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
""")
        # Tell Launch Services the bundle changed, so the Dock shows the current icon.
        os.utime(app, None)
        return str(link)

    def _lan_prefix(self, qemu):
        """`qemu` is the command that runs QEMU (its launcher words, then the program)."""
        """The LAN NIC through socket_vmnet when it runs (toolkit-host odapkit-vmnet): QEMU is
        started by socket_vmnet_client, which passes the connection as fd 3."""
        if os.environ.get("ODAP_NET") == "user":
            return list(qemu), None
        client = "/opt/socket_vmnet/bin/socket_vmnet_client"
        client = client if os.access(client, os.X_OK) else None
        socks = ([os.environ["ODAP_LAN_SOCKET"]] if os.environ.get("ODAP_LAN_SOCKET") else []) + \
            sorted(str(p) for p in pathlib.Path("/var/run").glob("socket_vmnet.bridged.*")) + ["/var/run/socket_vmnet"]
        sock = next((s for s in socks if pathlib.Path(s).is_socket()), None)
        if client and sock:
            return [client, sock, *qemu, "-netdev", "socket,id=lan,fd=3", "-device", "virtio-net-pci,netdev=lan,mac=52:54:00:0d:a1:01"], sock
        return list(qemu), None


# Where a user's images are, without a toolkit or a blackbird-os checkout.
USER_IMAGES = pathlib.Path(os.environ.get("XDG_DATA_HOME", pathlib.Path.home() / ".local/share")) / "opendap/images"


ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOL = ROOT / "bin/odap-vdev"


def build_viewer():
    """The simulator window (viewer/), built with cargo when its sources changed (cargo decides);
    None, with a note, if it cannot be built."""
    manifest = ROOT / "viewer/Cargo.toml"
    if not shutil.which("cargo"):
        print("odap-vdev: no cargo: using QEMU's own window (install Rust for the simulator window)", file=sys.stderr)
        return None
    r = subprocess.run(["cargo", "build", "--release", "-q", "--manifest-path", str(manifest)],
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        print(f"odap-vdev: the simulator window did not build; using QEMU's own window:\n{r.stderr.strip()[-800:]}", file=sys.stderr)
        return None
    return str(ROOT / "viewer/target/release/odap-vdev-view")


def window_on_top_library():
    """The library that keeps QEMU's window on top (host/macos/window-on-top.m), built with the
    system's clang once per source and kept in the cache; None, with a note, if it cannot be."""
    src = pathlib.Path(__file__).resolve().parents[2] / "host/macos/window-on-top.m"
    digest = hashlib.sha256(src.read_bytes()).hexdigest()[:16]
    out = pathlib.Path(os.environ.get("XDG_CACHE_HOME", pathlib.Path.home() / ".cache")) / f"opendap/vdev/window-on-top-{digest}.dylib"
    if out.is_file():
        return str(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    r = subprocess.run(["clang", "-dynamiclib", "-fobjc-arc", "-framework", "AppKit", "-O2", "-o", str(out), str(src)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print(f"odap-vdev: the window cannot stay on top (clang: {r.stderr.strip() or r.returncode}); "
              "install the Xcode command-line tools", file=sys.stderr)
        return None
    return str(out)


def firmware(qemu):
    """QEMU's own UEFI firmware for aarch64, next to the binary that runs: <prefix>/share/qemu,
    whichever way QEMU was installed; then the distribution paths."""
    real = pathlib.Path(shutil.which(qemu) or qemu).resolve()
    for d in (real.parent.parent / "share/qemu", pathlib.Path("/usr/share/qemu"), pathlib.Path("/usr/share/AAVMF")):
        for name in ("edk2-aarch64-code.fd", "AAVMF_CODE.fd"):
            if (d / name).is_file():
                return d / name
    return real.parent.parent / "share/qemu/edk2-aarch64-code.fd"
