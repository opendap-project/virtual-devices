"""A minimal client for QEMU's QMP socket: line-delimited JSON."""
import json
import socket


class QmpError(Exception):
    pass


class Qmp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        try:
            self.sock.connect(str(path))
        except OSError as e:
            raise QmpError(f"cannot reach QMP at {path}: {e}") from e
        self.f = self.sock.makefile("rw")
        self._read()
        self.cmd("qmp_capabilities")

    def _read(self):
        while True:
            line = self.f.readline()
            if not line:
                raise QmpError("QMP closed the connection")
            msg = json.loads(line)
            if "event" not in msg:
                return msg

    def cmd(self, name, **args):
        try:
            self.f.write(json.dumps({"execute": name, "arguments": args}) + "\n")
            self.f.flush()
        except (BrokenPipeError, ConnectionResetError) as e:
            raise QmpError(f"QMP {name}: the instance went away") from e
        reply = self._read()
        if "error" in reply:
            raise QmpError(f"QMP {name}: {reply['error']['desc']}")
        return reply.get("return")

    def key(self, qcode, down):
        self.cmd("input-send-event", events=[{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}}])

    def pointer(self, x, y, width, height, down=None):
        """Absolute pointer at a display pixel (axes span 0..0x7FFF); `down` presses or releases."""
        events = [{"type": "abs", "data": {"axis": "x", "value": x * 0x7FFF // max(1, width - 1)}},
                  {"type": "abs", "data": {"axis": "y", "value": y * 0x7FFF // max(1, height - 1)}}]
        if down is not None:
            events.append({"type": "btn", "data": {"down": down, "button": "left"}})
        self.cmd("input-send-event", events=events)
