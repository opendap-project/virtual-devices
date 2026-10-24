"""Compose a virtual device from its model: the QEMU arguments of its hardware layers and its device
description (opendap.device-description/v1), which the virtual board hands to the guest.

A model names one layer per kind (soc, board, faceplate, hats); each layer contributes its [qemu]
args and its part of the description. Run-time values fill {run.*} and {host.*} templates.
"""
import pathlib
import re
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
SCHEMA = "opendap.device-description/v1"
# Control fields the runner uses and the description leaves out.
RUNNER_ONLY = {"qcode"}
KINDS = {"soc": "soc", "board": "boards", "case": "cases", "faceplate": "faceplates", "hats": "hats"}
# Layers a model may leave out: a case only shapes the simulator window; HATs are optional.
OPTIONAL = {"case", "hats"}


class CompositionError(Exception):
    pass


def fail(msg):
    raise CompositionError(msg)


def load(path):
    try:
        return tomllib.loads(pathlib.Path(path).read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        fail(f"{path}: {e}")


def model_path(name):
    p = pathlib.Path(name)
    if p.suffix == ".toml" and p.exists():
        return p
    p = ROOT / "models" / f"{name}.toml"
    if not p.exists():
        fail(f"no model {name!r} (see: odap-vdev models)")
    return p


def layers(model):
    """The model's layers, in order: soc, board, case, faceplate, then each HAT."""
    out = []
    for kind, folder in KINDS.items():
        ids = model["layers"].get(kind, [] if kind in OPTIONAL else None)
        if ids is None:
            fail(f"model {model['model']['id']}: no {kind} layer")
        for lid in ids if isinstance(ids, list) else [ids]:
            layer = load(ROOT / "layers" / folder / f"{lid}.toml")
            if layer["layer"]["kind"] != kind:
                fail(f"layer {lid}: kind {layer['layer']['kind']!r}, expected {kind!r}")
            out.append(layer)
    return out


def fill(text, values):
    def sub(m):
        key = m.group(1)
        if key not in values:
            fail(f"no value for {{{key}}} (pass {key}=… or define it in a layer)")
        return str(values[key])
    return re.sub(r"\{([a-z_]+\.[a-z_]+)\}", sub, text)


def flat(layer):
    """Layer tables as dotted keys (soc.cores, display.width_px) for argument templates."""
    return {f"{t}.{k}": v for t, table in layer.items() if isinstance(table, dict) for k, v in table.items()
            if not isinstance(v, (dict, list))}


def qemu_args(model, runtime):
    values = dict(runtime)
    for layer in layers(model):
        values.update(flat(layer))
    return [fill(a, values) for layer in layers(model) for a in layer.get("qemu", {}).get("args", [])]


def toml_value(v):
    if isinstance(v, str):
        return '"' + v.replace("\\", "\\\\").replace('"', '\\"') + '"'
    if isinstance(v, bool):
        return "true" if v else "false"
    return str(v)


def description(model):
    """Identity from the model; display, touch, controls, indicators and sensors from the faceplate
    (and later HATs)."""
    m = model["model"]
    lines = [f'schema = "{SCHEMA}"', "", "[identity]", 'kind = "board"', f'id = {toml_value(m["id"])}',
             f'name = {toml_value(m["name"])}', f'version = {m["version"]}']
    for layer in layers(model):
        if "display" in layer:
            lines += ["", "[display]"] + [f"{k} = {toml_value(v)}" for k, v in layer["display"].items()]
        if "touch" in layer:
            lines += ["", "[touch]"] + [f"{k} = {toml_value(v)}" for k, v in layer["touch"].items()]
        for control in layer.get("controls", []):
            lines += ["", "[[controls]]"] + [f"{k} = {toml_value(v)}" for k, v in control.items() if k not in RUNNER_ONLY]
        for section in ("indicators", "sensors"):
            for item in layer.get(section, []):
                lines += ["", f"[[{section}]]"] + [f"{k} = {toml_value(v)}" for k, v in item.items()]
    return "\n".join(lines) + "\n"


def models():
    """(id, name, stem) of every model."""
    return [(load(p)["model"]["id"], load(p)["model"]["name"], p.stem) for p in sorted((ROOT / "models").glob("*.toml"))]


def controls(model):
    """The model's controls with their roles and QEMU keys, for input injection."""
    return [c for layer in layers(model) for c in layer.get("controls", [])]


def display(model):
    for layer in layers(model):
        if "display" in layer:
            return layer["display"]
    return None


def forced_power_off_ms(model):
    """How long `main` is held before the board's PMIC cuts the power; None without one."""
    for layer in layers(model):
        if "pmic" in layer:
            return layer["pmic"].get("forced_power_off_ms")
    return None


def frame(model):
    """How the simulator window draws the device: the case (body and keys) and the faceplate's
    front (glass, display placement, front details), with the display and the controls. None when
    the model has no case or no front (the window shows the display alone)."""
    case = next((layer["case"] for layer in layers(model) if "case" in layer), None)
    front = next((layer["front"] for layer in layers(model) if "front" in layer), None)
    d = display(model)
    if case is None or front is None or d is None:
        return None
    return {"schema": "opendap.vdev-frame/v2", "case": case, "front": front, "display": d, "controls": controls(model)}


# SDXC's largest card, and the card the simulation uses when no layer sets one.
SD_MAX = 2 * 10**12
SD_DEFAULT = 1 << 30
UNITS = {"KIB": 1 << 10, "MIB": 1 << 20, "GIB": 1 << 30, "TIB": 1 << 40, "KB": 10**3, "MB": 10**6, "GB": 10**9, "TB": 10**12, "B": 1}


def size_bytes(text):
    """`1GiB`, `512 MB`, `2TB` in bytes."""
    m = re.fullmatch(r"\s*([0-9]+(?:\.[0-9]+)?)\s*([A-Za-z]*)\s*", str(text))
    if not m or m.group(2).upper() not in UNITS | {"": 1}:
        fail(f"card_size {text!r}: a number and a unit (KiB, MiB, GiB, TiB, KB, MB, GB, TB)")
    return int(float(m.group(1)) * UNITS.get(m.group(2).upper(), 1))


def microsd_card(model):
    """The size, in bytes, of the card the simulation inserts by default: the layers' `[microsd]
    card_size`, up to 2 TB; 1 GiB when no layer sets it."""
    size = next((size_bytes(layer["microsd"]["card_size"]) for layer in layers(model)
                 if "card_size" in layer.get("microsd", {})), SD_DEFAULT)
    if not 0 < size <= SD_MAX:
        fail(f"microsd card_size: {size} bytes; a card is at most 2 TB")
    return size


def storage_gb(model):
    for layer in layers(model):
        if "board" in layer and "storage_gb" in layer["board"]:
            return layer["board"]["storage_gb"]
    return 32
