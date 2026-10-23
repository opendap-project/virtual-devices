#!/usr/bin/env python3
"""Compose a virtual device from its model: the QEMU arguments of its hardware layers and its device
description (opendap.device-description/v1), which the virtual board hands to the guest.

  vdev.py models                         the models, one per line: id<TAB>name<TAB>file
  vdev.py description MODEL              the device description, as TOML
  vdev.py qemu-args MODEL [KEY=VALUE…]   QEMU arguments, one per line; KEY=VALUE fills run-time
                                         values (run.disk, run.ssh_port, host.audio)

MODEL is a model file's stem (virtual-t1) or path. A model names one layer per kind (soc, board,
faceplate, hats); each layer contributes its [qemu] args and its part of the description.
Exit status: 0 on success, 2 on a usage or data error (message on stderr).
"""
import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCHEMA = "opendap.device-description/v1"
KINDS = {"soc": "soc", "board": "boards", "faceplate": "faceplates", "hats": "hats"}


def fail(msg):
    print(f"vdev: {msg}", file=sys.stderr)
    sys.exit(2)


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
        fail(f"no model {name!r} (see: vdev.py models)")
    return p


def layers(model):
    """The model's layers, in order: soc, board, faceplate, then each HAT."""
    out = []
    for kind, folder in KINDS.items():
        ids = model["layers"].get(kind, [] if kind == "hats" else None)
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
    """Identity from the model; display and controls from the faceplate (and later HATs)."""
    m = model["model"]
    lines = [f'schema = "{SCHEMA}"', "", "[identity]", 'kind = "board"', f'id = {toml_value(m["id"])}',
             f'name = {toml_value(m["name"])}', f'version = {m["version"]}']
    for layer in layers(model):
        if "display" in layer:
            lines += ["", "[display]"] + [f"{k} = {toml_value(v)}" for k, v in layer["display"].items()]
        for control in layer.get("controls", []):
            lines += ["", "[[controls]]"] + [f"{k} = {toml_value(v)}" for k, v in control.items()]
    return "\n".join(lines) + "\n"


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__.strip())
        return 0
    cmd, rest = argv[0], argv[1:]
    if cmd == "models":
        for p in sorted((ROOT / "models").glob("*.toml")):
            m = load(p)["model"]
            print(f"{m['id']}\t{m['name']}\t{p.relative_to(ROOT)}")
    elif cmd == "description" and len(rest) == 1:
        sys.stdout.write(description(load(model_path(rest[0]))))
    elif cmd == "qemu-args" and rest:
        runtime = {}
        for kv in rest[1:]:
            k, sep, v = kv.partition("=")
            if not sep:
                fail(f"expected KEY=VALUE, got {kv!r}")
            runtime[k] = v
        print("\n".join(qemu_args(load(model_path(rest[0])), runtime)))
    else:
        fail(f"unknown command or arguments: {' '.join(argv)} (see: vdev.py --help)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
