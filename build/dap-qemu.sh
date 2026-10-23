#!/bin/sh
# Run a virtual OpenDAP device on the host with QEMU (HVF on macOS, KVM on Linux).
#
# Usage: build/dap-qemu.sh start [--window] | stop | reboot [--debug] | ssh [command…] | console | models
#                          | battery [LEVEL] | charger [on|off]
#   start      boot the model in the background; --window also shows its display in a window
#   stop       orderly power-off (falls back to stopping QEMU after 30 s)
#   reboot     reboot; --debug boots once into the debug boot mode (consoles, full messages)
#   ssh        a shell or a command on the device, over the management link
#   console    follow the serial console
#   models     the models this checkout defines
#   battery    the battery level in percent, or set it (the board's simulated battery)
#   charger    whether a charger is connected, or connect / disconnect it (USB-C power)
#
# Environment (good defaults for the workspace layout):
#   DAP_MODEL         model to run (default: virtual-t1; see models/)
#   DAP_IMAGE         Blackbird OS qemu image (default: blackbird-os's latest qemu build)
#   BLACKBIRD_OS      blackbird-os checkout (default: next to this one); its build/dev-ssh key is used
#   DAP_SSH_PORT      management link SSH port on 127.0.0.1 (default: 2222)
#   DAP_VNC_DISPLAY   VNC display on 127.0.0.1 (default: 0, port 5900)
#   DAP_NET=user      no LAN NIC, even when socket_vmnet runs
#   DAP_WINDOW_ZOOM=fit  scale the window's picture with its size (default: 1:1 panel pixels)
#
# The hardware comes from the model's layers (build/vdev.py qemu-args); its device description is
# handed to the guest through fw_cfg (opt/org.opendap/device-description), so the image carries no
# model. Run state is in build/run/: disk, firmware variables, QMP socket, console and QEMU logs.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
os="${BLACKBIRD_OS:-$repo/../blackbird-os}"
model="${DAP_MODEL:-virtual-t1}"
run="$here/run"; port="${DAP_SSH_PORT:-2222}"; vnc="${DAP_VNC_DISPLAY:-0}"
image="${DAP_IMAGE:-$os/image/mkosi.output/blackbird-os-qemu.raw}"
disk="$run/disk.qcow2"
key="$os/build/dev-ssh/id_ed25519"
fw_code="$(brew --prefix qemu 2>/dev/null)/share/qemu/edk2-aarch64-code.fd"
case "$(uname -s)" in Darwin) accel=hvf; audio=coreaudio;; *) accel=kvm; audio=pa;; esac

sshq() {
  ssh -i "$key" -p "$port" -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR root@127.0.0.1 -- "$@"
}

case "${1:-start}" in
  start)
    [ -f "$image" ] || { echo "no image at $image: build it with blackbird-os build/dap-image.sh qemu" >&2; exit 1; }
    name="$("$here/vdev.py" models | awk -F'\t' -v m="models/$model.toml" '$3 == m {print $2}')"
    [ -n "$name" ] || { echo "no model $model (see: $0 models)" >&2; exit 2; }
    mkdir -p "$run"
    # The disk: the model's storage size on top of the latest image, which stays untouched.
    if [ ! -f "$disk" ] || [ "$image" -nt "$disk" ] || [ "$(cat "$run/disk.base" 2>/dev/null)" != "$image" ]; then
      rm -f "$disk"; qemu-img create -q -f qcow2 -F raw -b "$image" "$disk" 32G; echo "$image" > "$run/disk.base"
    fi
    [ -f "$run/vars.fd" ] || dd if=/dev/zero of="$run/vars.fd" bs=1m count=64 2>/dev/null
    "$here/vdev.py" description "$model" > "$run/description.toml"

    display="-display none"
    qemu=qemu-system-aarch64
    if [ "${2:-}" = --window ]; then
      zoom=off; [ "${DAP_WINDOW_ZOOM:-}" = fit ] && zoom=on
      case "$(uname -s)" in
        Darwin)
          display="-display cocoa,zoom-to-fit=$zoom"
          # The window belongs to an app bundle named after the model, so it has a name in the Dock
          # and can be addressed by automation; the bundle only points at Homebrew's QEMU.
          app="$run/$name.app"
          mkdir -p "$app/Contents/MacOS"
          ln -sf "$(command -v qemu-system-aarch64)" "$app/Contents/MacOS/qemu-system-aarch64"
          cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>org.opendap.$model</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleExecutable</key><string>qemu-system-aarch64</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
          qemu="$app/Contents/MacOS/qemu-system-aarch64";;
        *) display="-display gtk,zoom-to-fit=$zoom";;
      esac
    fi

    # The LAN, like a T1 on Wi-Fi: a NIC on the host's network through socket_vmnet (bridged when
    # that daemon runs, else vmnet's shared network), with DHCP, internet and mDNS. The model's
    # management link (user-mode network, SSH on 127.0.0.1) is always there.
    lan="" client=""
    if [ "${DAP_NET:-auto}" != user ]; then
      for c in /opt/socket_vmnet/bin/socket_vmnet_client "$(brew --prefix 2>/dev/null)/opt/socket_vmnet/bin/socket_vmnet_client"; do
        [ -x "$c" ] && { client="$c"; break; }
      done
      for sock in ${DAP_LAN_SOCKET:-} /var/run/socket_vmnet.bridged.* "$(brew --prefix 2>/dev/null)/var/run/socket_vmnet" /var/run/socket_vmnet; do
        [ -n "$client" ] && [ -S "$sock" ] && { lan="$sock"; break; }
      done
    fi
    if [ -n "$lan" ]; then
      set -- "$client" "$lan" "$qemu" -netdev socket,id=lan,fd=3 -device virtio-net-pci,netdev=lan,mac=52:54:00:0d:a1:01
      echo "LAN: $lan"
    else
      set -- "$qemu"
      [ "${DAP_NET:-auto}" = user ] || echo "LAN: none (socket_vmnet not running: toolkit-host socket-vmnet.sh install); management link only"
    fi

    # The model's hardware, one argument per line.
    old_ifs="$IFS"; IFS='
'
    for a in $("$here/vdev.py" qemu-args "$model" run.disk="$disk" run.ssh_port="$port" host.audio="$audio"); do
      set -- "$@" "$a"
    done
    IFS="$old_ifs"

    # Not -daemonize: on macOS it forks after CoreAudio initializes and crashes.
    # shellcheck disable=SC2086
    nohup "$@" -name "$name" -accel "$accel" \
      -drive if=pflash,format=raw,readonly=on,file="$fw_code" \
      -drive if=pflash,format=raw,file="$run/vars.fd" \
      -fw_cfg name=opt/org.opendap/device-description,file="$run/description.toml" \
      $display -vnc 127.0.0.1:"$vnc" -qmp unix:"$run/qmp.sock",server=on,wait=off \
      -serial file:"$run/console.log" \
      >"$run/qemu.log" 2>&1 &
    echo $! > "$run/qemu.pid"; sleep 1
    kill -0 "$(cat "$run/qemu.pid")" 2>/dev/null || { cat "$run/qemu.log" >&2; exit 1; }
    echo "$name started (pid $(cat "$run/qemu.pid")); console: $run/console.log; ssh: $0 ssh";;
  stop)
    # An orderly power-off, so the guest's file systems are clean (a kill loses recent writes).
    # Power is the shell's button (a short press does nothing), so ask systemd over SSH.
    if [ -f "$run/qemu.pid" ] && kill -0 "$(cat "$run/qemu.pid")" 2>/dev/null; then
      pid="$(cat "$run/qemu.pid")"
      sshq systemctl poweroff >/dev/null 2>&1 || true
      n=0; while kill -0 "$pid" 2>/dev/null && [ $n -lt 30 ]; do sleep 1; n=$((n + 1)); done
      kill "$pid" 2>/dev/null || true
    fi
    rm -f "$run/qemu.pid"; echo stopped;;
  reboot)
    if [ "${2:-}" = --debug ]; then
      sshq 'id=$(bootctl list --json=short | grep -o "\"id\":\"[^\"]*@debug\"" | cut -d\" -f4) && [ -n "$id" ] && bootctl set-oneshot "$id" && systemctl reboot'
    else
      sshq systemctl reboot
    fi
    echo rebooting;;
  ssh)
    shift
    exec ssh -i "$key" -p "$port" -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@127.0.0.1 -- "$@";;
  console)
    exec tail -f "$run/console.log";;
  models)
    exec "$here/vdev.py" models;;
  battery)
    p=/sys/module/test_power/parameters
    if [ -n "${2:-}" ]; then
      case "$2" in *[!0-9]*) echo "battery LEVEL: 0 to 100" >&2; exit 2;; esac
      [ "$2" -le 100 ] || { echo "battery LEVEL: 0 to 100" >&2; exit 2; }
      sshq "echo $2 > $p/battery_capacity"
    fi
    sshq "cat /sys/class/power_supply/test_battery/capacity";;
  charger)
    p=/sys/module/test_power/parameters
    case "${2:-}" in
      on) sshq "echo on > $p/usb_online && echo charging > $p/battery_status";;
      off) sshq "echo off > $p/usb_online && echo discharging > $p/battery_status";;
      "") ;;
      *) echo "charger [on|off]" >&2; exit 2;;
    esac
    sshq "printf 'charger %s, battery %s\\n' \"\$(cat /sys/class/power_supply/test_usb/online | sed 's/1/on/;s/0/off/')\" \"\$(cat /sys/class/power_supply/test_battery/status)\"";;
  *) echo "usage: $0 start [--window] | stop | reboot [--debug] | ssh [command…] | console | models | battery [LEVEL] | charger [on|off]" >&2; exit 2;;
esac
