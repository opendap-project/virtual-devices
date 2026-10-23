"""One running virtual device: its state folder, QEMU process, endpoints and management link.

State lives in $XDG_STATE_HOME/opendap/vdev/<model>/ (not in a repository): the disk on top of
the image, firmware variables, the composed description, QMP socket, console and QEMU logs.
"""
import os
import pathlib
import platform
import shutil
import signal
import subprocess
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
    def start(self, window=False):
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
        prefix = brew_prefix("qemu")
        fw = (pathlib.Path(prefix) / "share/qemu/edk2-aarch64-code.fd") if prefix else pathlib.Path("/usr/share/qemu/edk2-aarch64-code.fd")

        qemu = shutil.which("qemu-system-aarch64") or "qemu-system-aarch64"
        display = ["-display", "none"]
        if window:
            zoom = "on" if os.environ.get("ODAP_WINDOW_ZOOM") == "fit" else "off"
            if darwin:
                display = ["-display", f"cocoa,zoom-to-fit={zoom}"]
                qemu = self._app_bundle(qemu)
            else:
                display = ["-display", f"gtk,zoom-to-fit={zoom}"]

        argv, lan = self._lan_prefix(qemu)
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

    def reboot(self, debug=False):
        if debug:
            self.ssh("id=$(bootctl list --json=short | grep -o '\"id\":\"[^\"]*@debug\"' | cut -d'\"' -f4) "
                     "&& [ -n \"$id\" ] && bootctl set-oneshot \"$id\" && systemctl reboot")
        else:
            self.ssh("systemctl", "reboot", check=False)

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
        (app / "Contents/Info.plist").write_text(f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>org.opendap.{self.model_name}</string>
  <key>CFBundleName</key><string>{self.name}</string>
  <key>CFBundleExecutable</key><string>qemu-system-aarch64</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
""")
        return str(link)

    def _lan_prefix(self, qemu):
        """The LAN NIC through socket_vmnet when it runs (toolkit-host odap-vmnet): QEMU is
        started by socket_vmnet_client, which passes the connection as fd 3."""
        if os.environ.get("ODAP_NET") == "user":
            return [qemu], None
        prefix = brew_prefix()
        clients = ["/opt/socket_vmnet/bin/socket_vmnet_client"] + ([f"{prefix}/opt/socket_vmnet/bin/socket_vmnet_client"] if prefix else [])
        client = next((c for c in clients if os.access(c, os.X_OK)), None)
        socks = ([os.environ["ODAP_LAN_SOCKET"]] if os.environ.get("ODAP_LAN_SOCKET") else []) + \
            sorted(str(p) for p in pathlib.Path("/var/run").glob("socket_vmnet.bridged.*")) + \
            ([f"{prefix}/var/run/socket_vmnet"] if prefix else []) + ["/var/run/socket_vmnet"]
        sock = next((s for s in socks if pathlib.Path(s).is_socket()), None)
        if client and sock:
            return [client, sock, qemu, "-netdev", "socket,id=lan,fd=3", "-device", "virtio-net-pci,netdev=lan,mac=52:54:00:0d:a1:01"], sock
        return [qemu], None


def brew_prefix(formula=None):
    try:
        r = subprocess.run(["brew", "--prefix", *([formula] if formula else [])], capture_output=True, text=True)
        return r.stdout.strip() or None
    except OSError:
        return None
