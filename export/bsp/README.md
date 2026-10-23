# Virtual board support package

What a Blackbird OS image needs to run on the virtual board (system hardware support spec §5: one board support package per board). blackbird-os's `qemu` profile installs this tree as is.

| Path | Purpose |
|---|---|
| `usr/lib/modules-load.d/opendap-virtual-board.conf` | Loads `qemu_fw_cfg`, the channel the virtual board uses to hand its device description to the guest |
| `usr/lib/systemd/system/opendap-virtual-device-description.service` | Copies that description to `/run/opendap/device/description.toml` at boot; enable it in the image |
| `usr/libexec/opendap/virtual-usb-device` | The USB-C port in device mode, on `dummy_hcd` (plug / unplug the cable) |

The image carries no model: the running model's description comes from the virtual board, so one image runs every virtual model.
