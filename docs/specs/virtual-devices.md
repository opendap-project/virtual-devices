# Virtual devices

- **Status:** Draft, 2026-10-23.
- **Requirements:** VD-001 to VD-007; blackbird-os OS-016, OS-019 and OS-035; cli TK-018.
- **Builds on:** the [hardware support spec](https://github.com/opendap-project/system/blob/main/docs/specs/hardware-support.md) (layers H0–H5, device descriptions, adapters); the [development workflow](https://github.com/opendap-project/toolkit-host/blob/main/docs/specs/development-workflow.md).

## 1. Summary

1. **A virtual device is a model over hardware layers.** Each layer describes one part of the hardware: SoC, board, faceplate, HATs. A model names one layer of each kind, and the device is what they make together (VD-005).
2. **Each layer brings two things:**
   - the QEMU devices that stand in for its hardware;
   - its part of the device description, for example the faceplate's display and controls.
3. **The board hands the description to the guest.** It goes through QEMU's fw_cfg, as a real board is identified by its hardware. The Blackbird OS image carries no model, so one image runs every model.
4. **What QEMU cannot provide is shown by the toolkit** (TK-018).

## 2. Layout

| Path | Contents |
|---|---|
| `layers/soc/` | H0: `qemu-virt-aarch64`, with cores and memory |
| `layers/boards/` | H1: `virtual-t1-board`, with storage, management link, USB-C host, microSD, audio output and named connectors |
| `layers/faceplates/` | H3: `virtual-touch`, with the panel, touch, the controls with roles and places, and the host's keys and pointer |
| `layers/hats/` | H4: none yet |
| `models/` | `virtual-t1`: its identity, and one layer per kind |
| `bin/odap-vdev` | The tool, one domain per subcommand: `models`, `describe`, `start [--window] [--no-on-top] [--wait]`, `stop`, `reboot [--mode MODE] [--list]`, `status [--json]`, `ssh`, `console`, `screen capture`, `input press ROLE [ROLE…] / tap / swipe`, `usb list / plug / unplug`, `sd insert / eject`, `power battery / charger` |
| `lib/odap_vdev/` | Its modules: `compose` (model and layers to description and QEMU arguments), `instance` (one running device), `qmp`, `workspace` (other repositories through `repos.tsv`), `cli` |
| `export/bsp/` | The virtual board's support package for Blackbird OS images (a mkosi config directory): the description from fw_cfg, the network links, the USB device port, the initrd's drivers |

**Rules:**

- A layer never names a model.
- A model names layers by id, never QEMU devices.
- Run-time values come from the tool: `run.disk`, `run.ssh_port`, `host.audio`.
- Controls are pressed by role; a control's QEMU key (`qcode`) is a runner-only field the description leaves out.
- Run state lives in `~/.local/state/opendap/vdev/<model>/`, never in a repository. The image is blackbird-os's `image/mkosi.output/`, found through `repos.tsv` (or `ODAP_IMAGE`); the development key is toolkit-image's (`~/.local/share/opendap/dev-ssh`).
- A new model is a new file in `models/`, or new layers combined differently, such as the no-display and round-display fixtures of the hardware support spec §8.

## 3. Boot

- **Product boot.**
  - The firmware does not wait and draws nothing.
  - systemd-boot's menu is hidden.
  - The kernel and systemd are quiet, and print only to the serial port.
  - No console runs, on the screen or on the serial port.
  - The screen goes straight to the interface (OS-035).
- **Debug boot mode.**
  - It is the image's "debug" boot entry: a UKI profile with `blackbird.debug`, full messages, and a login on the screen and on the serial port.
  - Reach it with `odap-vdev reboot --mode debug` (or `opendap device reboot --mode debug`), which asks the device's `odap-reboot` to boot it once.
- **Boot image.** On the T1 the boot image is U-Boot's logo, and the shell's first frame draws the same image before the interface (system shell spec §9). Not built yet: on the virtual board the screen is dark until the interface.

## 4. Window

`odap-vdev start --window` opens the display at the panel's pixels, 1:1. The GPU starts at the panel's size and the firmware never resizes it, so the window opens at the right size and keeps it (VD-007). `ODAP_WINDOW_ZOOM=fit` scales the picture with the window instead.

On macOS the window belongs to an app bundle named after the model ("Virtual T1"), with the OpenDAP app icon, which gives it a name in the Dock and lets automation address it.

The window stays above other windows by default, so it stays in view while you work in an editor or a terminal; `--no-on-top` turns that off. QEMU's cocoa display has no option for it (QEMU 11.1), so odap-vdev builds a small library with the system's clang (`host/macos/window-on-top.m`, kept in `~/.cache/opendap/vdev/`) and loads it into QEMU only. It sets the level of QEMU's own windows and changes nothing else. An `always-on-top` option for QEMU's cocoa display, proposed upstream, would replace it. The host's pointer acts as a finger and its keys as the controls.

## 5. Fidelity

Each part of the T1 reference board (board BOM v1.7) and what the `virtual-t1` model offers in its place.

| T1 | Virtual T1 | Level |
|---|---|---|
| RK3566, 4 × Cortex-A55 | 4 cores of the host CPU (HVF on macOS, KVM on Linux); aarch64 | Same ISA and core count. Timing, power and thermal behaviour are not those of the A55, so measure them on the T1 |
| LPDDR4X 4 GB | 4 GB | Same |
| eMMC 32 GB | virtio-blk, 32 GB | Same size; a different bus |
| microSD socket | `sdhci-pci` with an `sd-card`, empty at boot; `odap-vdev sd insert FILE` and `sd eject` | Same class (SD host, `mmcblk`), with insertion and removal |
| 320 × 480 panel (ILI9488 on DSI) | virtio-gpu over MMIO, starting at and preferring 320 × 480 | Same pixels; DRM/KMS without hardware planes, and no 3D (Mesa renders in software). The firmware does not drive it, so boot shows no text and the window keeps its size |
| Touch (faceplate HID) | virtio-multitouch; toolkit-verify `odap-inject` (uinput) for full contacts in tests | Same class. QEMU 11.1.2 passes no contacts from QMP to the multi-touch device |
| — (simulator window) | virtio-tablet, used as one finger by `odap-shell`; `odap-vdev input tap / swipe` | Host mouse and VNC act as a finger |
| Buttons: action, volume + and −, main | virtio-keyboard: `KEY_PLAYPAUSE`, `KEY_VOLUMEUP`, `KEY_VOLUMEDOWN`, `KEY_POWER`, plus the host keys Space, ↑, ↓ and P with the same roles (the faceplate layer) | Same roles; `odap-vdev input press ROLE [ROLE…]` (several roles: a combination, pressed and released together), or the keyboard in the window. Holding `main` 8 s cuts the power, as the board's PMIC (`[pmic]` in the board layer) |
| CS43198 DACs, 3.5 mm and 4.4 mm outputs | virtio-sound to the host's audio output | One generic output; no DAC filters, gain stages, DSD or jack detection |
| USB-C in host mode (USB DAC, drives) | `qemu-xhci`; `odap-vdev usb plug storage FILE`, `usb plug audio`, `usb unplug` | Same classes (`usb-storage`, UAC through `snd-usb-audio`), plugged at run time |
| USB-C in device mode (ADR 0013: NCM, ACM, mass storage, UAC2) | `dummy_hcd` and the configfs gadget, with this instance as the computer; `virtual-usb-device plug / unplug` | NCM, ACM and mass storage enumerate (checked 2026-10-23). UAC2 cannot bind, because `dummy_hcd` has no isochronous transfers, so it is tested on the T1. The host computer itself does not see the gadget |
| Wi-Fi | A second NIC on the host's network through socket_vmnet: bridged to the host's interface (the LAN), or vmnet's shared network | IP on the local network and the internet, mDNS; no radio, scanning or roaming |
| — (cable to a computer) | QEMU's user-mode network, SSH on `127.0.0.1:2222` | The management link the CLI uses (`--target vm`) |
| Bluetooth | None (`hci_vhci` is not built in the image's kernel) | Not emulated |
| Battery and charger (BQ28Z610, BQ25798) | `test_power` (`test_battery`, `test_ac`, `test_usb`); `odap-vdev power battery LEVEL` and `power charger on / off` | Same power-supply class, with level and charging state |
| Status LED (LP5521, RGB) | The virtual board's LED adapter | Shown by the CLI's debug mode (TK-018) |
| Haptics (DRV2605L) | The virtual board's haptics adapter | Shown by the CLI's debug mode (TK-018) |
| NFC tag, TPM 2.0, RP2354A coprocessor, HAT connector | None yet | To add as virtual adapters, shown by the toolkit |

## 6. Networking

- **Management link.** QEMU's user-mode network gives SSH on `127.0.0.1:2222`. It is the link `opendap` uses for `--target vm:<name>` today, and it needs no setup.
- **LAN.** When socket_vmnet runs on the host, the runner adds a second NIC on the host's network.
  - On the bridged socket (`/var/run/socket_vmnet.bridged.en0`), the instance takes an address from the LAN's DHCP, like a T1 on Wi-Fi, so other devices see it.
  - On the shared socket (Homebrew's service), the instance is behind vmnet's NAT, and only the host sees it.
  - Either way it has the internet and mDNS, so the host's CLI and Desktop can reach it by name. DNS-SD of the OpenDAP Protocol (`_odap._tcp`) comes with the protocol services.
  - Setup is toolkit-host `odapkit vmnet install` (one-time; run as yourself, it asks for administrator rights only for the daemon). `ODAP_NET=user` turns the LAN off.

## 7. What the toolkit shows

The parts a VM cannot emulate still have their adapters on the virtual board, implementing the same `org.opendap.hw.*` interfaces (hardware support spec §4). They publish what software asks of them:

- the LED's colour and pattern;
- haptic effects;
- later, the NFC tag's contents and the coprocessor's state.

The CLI's debug mode shows that state as it changes (TK-018), so the LED and haptic design can be developed and tested without a T1. Not built yet: these adapters come with the services that use them.
