# Virtual devices

Virtual OpenDAP devices that run on a developer host with QEMU, as close to the hardware as QEMU allows. Early open development: one model today, the virtual T1.

A virtual device is a **model** over **hardware layers**: a SoC, a board, a faceplate and HATs. Each layer brings the QEMU devices that stand in for its hardware and its part of the device description. The board hands that description to the guest, so one Blackbird OS image runs every model. [Specification](docs/specs/virtual-devices.md).

```sh
bin/odap-vdev models                   # the models
bin/odap-vdev start --window --wait    # run the virtual T1 in a window, kept on top (--no-on-top; --model picks another)
bin/odap-vdev input tap 160 428        # touch; also input press ROLE [ROLE…] (combinations), screen capture, usb, sd
bin/odap-vdev power battery 30         # the simulated battery and charger
bin/odap-vdev reboot --mode debug      # a boot mode, once (reboot --list: the device's modes)
bin/odap-vdev status --json            # state and endpoints, for other tools
bin/odap-vdev stop
```

It needs a Blackbird OS `qemu` image (toolkit-image `odapkit image build qemu`) and the host tools of toolkit-host. For a LAN address, install socket_vmnet once with toolkit-host's `odapkit vmnet install`.

## Layout

- `layers/` — hardware layers by kind (`soc/`, `boards/`, `faceplates/`)
- `models/` — device models
- `bin/odap-vdev`, `lib/odap_vdev/` — the tool and its modules
- `export/bsp/` — the virtual board's support package, installed into Blackbird OS `qemu` images
- `docs/` — requirements and specification

## License

This repository follows the [REUSE](https://reuse.software) layout: license texts live in `LICENSES/` and `REUSE.toml` maps each path to its license.

- Default: **GNU GPL v3.0 or later** (`GPL-3.0-or-later`)
- `layers/` and `models/`: **Mozilla Public License 2.0** (`MPL-2.0`), so others can compose their own devices from them
- Markdown files and `docs/`: **Creative Commons Attribution-ShareAlike 4.0** (`CC-BY-SA-4.0`)

Copyright © 2026 The OpenDAP Project.
