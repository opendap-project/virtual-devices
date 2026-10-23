# Virtual devices requirements

Requirements for the virtual OpenDAP devices that run on a developer host.

| ID | Requirement | Priority | Status |
|---|---|---|---|
| VD-001 | A virtual device shall mimic its hardware as closely as possible: display, controls, touch, audio output, battery and charging, storage, microSD, USB in host and device mode, networking, and the coprocessor and HAT presence where they can be emulated. | must | Confirmed |
| VD-002 | A virtual device shall work with the `opendap` CLI and OpenDAP Desktop running on the host, as the hardware would. | must | Confirmed |
| VD-003 | A virtual device shall have network access, to the local network and to the internet. | must | Confirmed |
| VD-004 | Virtual devices shall live in their own repository, separate from the toolkit and from the operating system. | must | Confirmed |
| VD-005 | A virtual device shall be organised by its hardware: a combination of hardware layers (SoC, board, components, faceplate, HATs), with the device model defined on top of that combination. | must | Confirmed |
| VD-006 | Everything that lets a device be emulated on the host during development shall be provided, for every requirement, as far as possible; what cannot be emulated is shown by the toolkit (cli TK-018). | must | Confirmed |
| VD-007 | The window of a virtual device shall always open at the right size, and keep it while the device boots. | must | Confirmed |

## Context

- VD-001 to VD-003 were blackbird-os OS-018, OS-033 and OS-034 (moved on 2026-10-23 with VD-004).
- VD-004 to VD-007 (maintainers, 2026-10-23). VD-005 applies the system hardware support spec's layers (H0 to H5) to virtual hardware: one image runs every virtual model, because the model's description comes from the virtual board (fw_cfg), as a real board is identified by its hardware.
- VD-007: "the right size" is the panel's pixels at 1:1, so the picture stays crisp; on a 34-inch 3440 × 1440 monitor that is already larger than the T1's panel (about 74 mm against 49 mm wide).
- Specification: [virtual devices](../specs/virtual-devices.md).
