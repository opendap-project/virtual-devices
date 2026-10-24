"""Finding the other repositories a virtual device needs, and the host-wide state and keys.

A repository is found through ODAP_<NAME>_DIR (for example ODAP_BLACKBIRD_OS_DIR), else through the
workspace's repos.tsv (the folder above this checkout that lists every repository), never by a
guessed relative path (RFC 0008).
"""
import os
import pathlib

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent.parent


class NotFound(Exception):
    pass


def workspace():
    for d in [REPO, *REPO.parents]:
        if (d / "repos.tsv").is_file():
            return d
    return None


def repo_dir(name):
    """The checkout of the GitHub repository `name` (for example "blackbird-os")."""
    var = "ODAP_" + name.upper().replace("-", "_") + "_DIR"
    if os.environ.get(var):
        p = pathlib.Path(os.environ[var]).expanduser()
        if not p.is_dir():
            raise NotFound(f"{var}={p} is not a directory")
        return p
    ws = workspace()
    if ws:
        for line in (ws / "repos.tsv").read_text().splitlines()[1:]:
            path, _, slug = line.partition("\t")
            if slug.strip().split("/")[-1] == name and (ws / path).is_dir():
                return ws / path
    raise NotFound(f"no {name} checkout: set {var}, or run scripts/bootstrap.sh in the workspace")


def state_dir(model):
    base = pathlib.Path(os.environ.get("XDG_STATE_HOME", pathlib.Path.home() / ".local/state"))
    return base / "opendap" / "vdev" / model


def dev_key():
    """The user's SSH key for virtual devices: ODAP_DEV_KEY, else ~/.local/share/opendap/dev-ssh.
    Maintainers' images trust it (odapkit dev-key creates it); device pairing will add it to others."""
    if os.environ.get("ODAP_DEV_KEY"):
        return pathlib.Path(os.environ["ODAP_DEV_KEY"]).expanduser()
    base = pathlib.Path(os.environ.get("XDG_DATA_HOME", pathlib.Path.home() / ".local/share"))
    return base / "opendap" / "dev-ssh" / "id_ed25519"
