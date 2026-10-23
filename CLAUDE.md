# Virtual devices

Virtual OpenDAP devices for development on the host: hardware layers, device models composed from them, the QEMU runner and the virtual board's support package for Blackbird OS images.

Part of [OpenDAP](https://github.com/opendap-project). This repository is open source.

## Working here

- Write everything in English: code, docs, commit messages.
- Record only information the maintainers confirmed. Do not infer specs, numbers or wording.
- Project context is in `docs/`: requirements in `docs/requirements/`. Read them before changing anything; update them with the `/opendap:requirement` and `/opendap:adr` skills.
- Licensing follows REUSE (`REUSE.toml`, `LICENSES/`). After adding files or folders, run the `/opendap:license-check` skill.
- A model is a combination of hardware layers; a layer never names a model. Other repositories use this one through `bin/odap-vdev` (its commands and `status --json`) and `export/` (the board support package), never its internals.
