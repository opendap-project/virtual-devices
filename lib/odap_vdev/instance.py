"""One running virtual device: its state folder, QEMU process, endpoints and management link.

State lives in $XDG_STATE_HOME/opendap/vdev/<model>/ (not in a repository): the disk on top of
the image, firmware variables, the composed description, QMP socket, console and QEMU logs.
"""
import hashlib
import os
import pathlib
import platform
import shutil
import signal
import subprocess
import sys
import time

from . import compose, workspace

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
    def console_log(self): return self.state / "console.log"
    @property
    def qemu_log(self): return self.state / "qemu.log"
    @property
    def disk(self): return self.state / "disk.qcow2"

    def image(self):
        if os.environ.get("ODAP_IMAGE"):
            return pathlib.Path(os.environ["ODAP_IMAGE"]).expanduser()
        return workspace.repo_dir("blackbird-os") / "image/mkosi.output/blackbird-os-qemu.raw"

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
            "image": {"path": image, "present": image_present},
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
    def start(self, window=False, on_top=True):
        if self.pid():
            raise InstanceError(f"{self.name} is already running (pid {self.pid()})")
        image = self.image()
        if not image.is_file():
            raise InstanceError(f"no image at {image}: build it with toolkit-image odap-image build qemu")
        self.state.mkdir(parents=True, exist_ok=True)
        base_file = self.state / "disk.base"
        if not self.disk.exists() or image.stat().st_mtime > self.disk.stat().st_mtime or \
                (base_file.read_text() if base_file.exists() else "") != str(image):
            self.disk.unlink(missing_ok=True)
            size = compose.storage_gb(self.model)
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw", "-b", str(image), str(self.disk), f"{size}G"], check=True)
            base_file.write_text(str(image))
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
        if window:
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
                 "-serial", f"file:{self.console_log}"]
        # Not -daemonize: on macOS it forks after CoreAudio initializes and crashes.
        with open(self.qemu_log, "w") as log:
            p = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        self.pid_file.write_text(str(p.pid))
        time.sleep(1)
        if p.poll() is not None:
            raise InstanceError(f"QEMU exited: {self.qemu_log.read_text().strip()}")
        return lan

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
    def _app_bundle(self, qemu):
        """On macOS the window belongs to an app bundle named after the model."""
        app = self.state / f"{self.name}.app"
        (app / "Contents/MacOS").mkdir(parents=True, exist_ok=True)
        link = app / "Contents/MacOS/qemu-system-aarch64"
        link.unlink(missing_ok=True)
        link.symlink_to(qemu)
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
  <key>CFBundleExecutable</key><string>qemu-system-aarch64</string>
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
        """The LAN NIC through socket_vmnet when it runs (toolkit-host odap-vmnet): QEMU is
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
