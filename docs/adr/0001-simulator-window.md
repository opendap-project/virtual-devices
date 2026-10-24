# ADR 0001: The simulator window

- **Status:** Accepted
- **Date:** 2026-10-23
- **Decided by:** Claude, under the maintainers' delegation, from their request for a framed mode "like the Android emulator, with a collapsible sidebar of options such as sending physical key events, through UI"

## Context

The window of a virtual device was QEMU's own (cocoa on macOS). It can show only the display: it cannot draw the device around it, add controls beside it, or stay above other windows. Staying on top needed a library injected into QEMU. The maintainers asked for a framed mode that shows the case and the faceplate around the display, as Android's emulator does, with a collapsible sidebar of hardware actions.

## Decision

- **Our own window.** `viewer/` holds `odap-vdev-view`, a host app in Rust and Slint (the ecosystem's UI stack, system ADR 0006). `odap-vdev start --window` starts QEMU without a window of its own (`-display none`) and opens it, inside the model's app bundle (name and icon). `odap-vdev window` opens it for a running device. QEMU's own window remains as a fallback (`ODAP_WINDOW=qemu`).
- **The display over VNC.** QEMU's VNC server on the loopback provides the display, through `opendap-rfb`, a minimal RFB client (RFC 6143: raw 32-bit frames, desktop resizes, pointer events). The pointer over the display is the guest's absolute pointer, the composer's finger.
- **The OpenDAP CLI's crates, not a copy of them** (amended 2026-10-23). What the window does to the device goes through the use cases `opendap` and OpenDAP Desktop share (`opendap-core` over `opendap-transport-vm`), consumed from the cli repository by revision:
  - the controls, pressed and released as the mouse button or the host key is, so long presses and combinations work as on the hardware, over a QMP session of the window's own (`LiveKeys`);
  - the rest on a worker thread: attaching by hand and the battery (`hardware`, through `odap-vdev`, which owns the hardware), the display (`screen`), restarts and power-off (`power`, the device's own `odap-reboot`).
- **State from the hardware, locked while in flight** (amended 2026-10-23). The sidebar shows the virtual hardware's state (a card in its socket), never the system's (a card mounted). A change in flight locks its control until the hardware reports it (`opendap_core::hardware::Switch`); a reboot lasts until the device went down and answers again (`opendap_core::power::Phase`). Nothing is asked twice; what cannot be taken now (the battery model while the device boots) is disabled and says why.
- **The keyboard drives everything.** The faceplate's host keys hold the controls; ⌘ shortcuts run the sidebar's actions under the same rules, folded or not; ⌘K lists them.
- **The frame is data.** `odap-vdev frame` gives the body, where the display sits in it, the colours and the controls, from the faceplate layer (`[frame]`, runner-only). It is drawn at the display's own density, so the device has its physical proportions.
  - It is schematic until the case and the faceplate export their geometry: the body is T1-032's ideal outline, the display is centred, the radii are guesses (UNVERIFIED).
  - Controls follow the concept art (T1-059).
- **On top** by default (`--no-on-top`), set when the window is created. **Framed** by default (`--no-frame` shows the display alone).

## Consequences

- The window, its frame, its sidebar and its level are OpenDAP's; QEMU needs no patch or injected library for them.
- Building the window needs Rust on the host; without it, `odap-vdev` falls back to QEMU's window and says so.
- When the case exports its geometry, the frame comes from that export instead of the layer's schematic values.
