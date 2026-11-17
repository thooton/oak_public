# Oak MCP: the Blender side of Oak's Blender MCP server (Oak's `src/mcp/blender.rs`).
#
# A small socket server inside Blender, on localhost:9876 by default. Each
# command is one JSON object, {"type": ..., "params": {...}}, answered with
# {"status": "success", "result": ...} or {"status": "error", "message": ...},
# the protocol of the community Blender MCP add-on (github.com/ahujasid/mcp-for-blender),
# so Oak's server works with either add-on. Commands run on Blender's main
# thread (bpy isn't thread-safe), from a timer.
#
# Installed by Oak (Home > MCP > Blender > Install add-on) into Blender's user
# add-ons folder; the server starts with Blender (not in background mode) and
# the 3D viewport's sidebar (N) has an "Oak" tab to stop or start it.
#
# Anything the model writes runs here with Blender's (and your user's) rights.

bl_info = {
    "name": "Oak MCP",
    "author": "Oak",
    "version": (1, 0, 0),
    "blender": (3, 0, 0),
    "location": "View3D > Sidebar > Oak",
    "description": "Lets Oak's agents work in this Blender through MCP (a local socket server)",
    "category": "Interface",
}

import io
import json
import queue
import socket
import threading
import traceback
from contextlib import redirect_stdout

import bpy

DEFAULT_PORT = 9876

# Commands waiting for the main thread: (command, reply box).
_todo = queue.Queue()
_server = None


class _Reply:
    def __init__(self):
        self.done = threading.Event()
        self.value = None


def _run(command):
    kind = command.get("type")
    params = command.get("params") or {}
    if kind == "execute_code":
        namespace = {"bpy": bpy, "__name__": "__oak_mcp__"}
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                exec(params.get("code", ""), namespace)
        except BaseException as e:  # SystemExit too: never let a script end Blender
            text = out.getvalue()
            tb = traceback.format_exc()
            message = "Code execution error: %s: %s" % (type(e).__name__, e)
            if text:
                message += "\n\nPrinted before the error:\n" + text
            return {"status": "error", "message": message + "\n\n" + tb}
        return {"status": "success", "result": {"executed": True, "result": out.getvalue()}}
    if kind == "get_addon_info":
        return {"status": "success", "result": {"addon": "oak_mcp", "version": list(bl_info["version"]), "blender": bpy.app.version_string}}
    return {"status": "error", "message": "Unknown command type: %s (the Oak MCP add-on runs execute_code)" % kind}


def _pump():
    """Runs the waiting commands on the main thread; a timer calls it."""
    while True:
        try:
            command, reply = _todo.get_nowait()
        except queue.Empty:
            break
        try:
            reply.value = _run(command)
        except BaseException as e:
            reply.value = {"status": "error", "message": "%s: %s" % (type(e).__name__, e)}
        reply.done.set()
    return 0.05 if _server is not None else None


class _Server:
    def __init__(self, port):
        self.port = port
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.bind(("127.0.0.1", port))
        self.sock.listen(4)
        self.sock.settimeout(1.0)
        self.running = True
        threading.Thread(target=self._accept, daemon=True).start()

    def stop(self):
        self.running = False
        try:
            self.sock.close()
        except OSError:
            pass

    def _accept(self):
        while self.running:
            try:
                client, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=self._client, args=(client,), daemon=True).start()

    def _client(self, client):
        decoder = json.JSONDecoder()
        raw = b""
        client.settimeout(None)
        try:
            while self.running:
                data = client.recv(65536)
                if not data:
                    break
                raw += data
                try:
                    text = raw.decode("utf-8")
                except UnicodeDecodeError:
                    continue  # a character split between reads
                text = text.lstrip()
                try:
                    command, end = decoder.raw_decode(text)
                except ValueError:
                    continue  # not all of it yet
                raw = text[end:].encode("utf-8")
                reply = _Reply()
                _todo.put((command, reply))
                # Long scripts (renders, simulations) may take a while.
                while not reply.done.wait(0.5):
                    if not self.running:
                        return
                client.sendall(json.dumps(reply.value).encode("utf-8"))
        except OSError:
            pass
        finally:
            try:
                client.close()
            except OSError:
                pass


def _prefs():
    addon = bpy.context.preferences.addons.get(__name__)
    return addon.preferences if addon else None


def start():
    """Starts the server; returns an error message or None."""
    global _server
    if _server is not None:
        return None
    prefs = _prefs()
    port = prefs.port if prefs else DEFAULT_PORT
    try:
        _server = _Server(port)
    except OSError as e:
        return "Port %d is in use (another Blender, or another MCP add-on): %s" % (port, e)
    if not bpy.app.timers.is_registered(_pump):
        bpy.app.timers.register(_pump, first_interval=0.05, persistent=True)
    print("Oak MCP: listening on 127.0.0.1:%d" % port)
    return None


def stop():
    global _server
    if _server is not None:
        _server.stop()
        _server = None
    if bpy.app.timers.is_registered(_pump):
        bpy.app.timers.unregister(_pump)


_last_error = ""


class OAKMCP_Preferences(bpy.types.AddonPreferences):
    bl_idname = __name__

    port: bpy.props.IntProperty(name="Port", default=DEFAULT_PORT, min=1024, max=65535,
                                description="Oak's Blender MCP server connects here (BLENDER_PORT in Oak's environment changes its side)")
    auto_start: bpy.props.BoolProperty(name="Start with Blender", default=True)

    def draw(self, context):
        row = self.layout.row()
        row.prop(self, "port")
        row.prop(self, "auto_start")


class OAKMCP_OT_start(bpy.types.Operator):
    bl_idname = "oak_mcp.start"
    bl_label = "Start"
    bl_description = "Let Oak's agents connect to this Blender"

    def execute(self, context):
        global _last_error
        _last_error = start() or ""
        if _last_error:
            self.report({"ERROR"}, _last_error)
            return {"CANCELLED"}
        return {"FINISHED"}


class OAKMCP_OT_stop(bpy.types.Operator):
    bl_idname = "oak_mcp.stop"
    bl_label = "Stop"
    bl_description = "Close the socket; Oak's agents can't reach this Blender until it starts again"

    def execute(self, context):
        stop()
        return {"FINISHED"}


class OAKMCP_PT_panel(bpy.types.Panel):
    bl_label = "Oak MCP"
    bl_idname = "OAKMCP_PT_panel"
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"
    bl_category = "Oak"

    def draw(self, context):
        layout = self.layout
        if _server is not None:
            layout.label(text="Listening on port %d" % _server.port, icon="CHECKMARK")
            layout.operator("oak_mcp.stop")
        else:
            layout.label(text="Stopped", icon="CANCEL")
            if _last_error:
                layout.label(text=_last_error[:60], icon="ERROR")
            layout.operator("oak_mcp.start")
        prefs = _prefs()
        if prefs:
            layout.prop(prefs, "port")


_classes = (OAKMCP_Preferences, OAKMCP_OT_start, OAKMCP_OT_stop, OAKMCP_PT_panel)


def _auto_start():
    global _last_error
    prefs = _prefs()
    if prefs is None or prefs.auto_start:
        _last_error = start() or ""
    return None


def register():
    for c in _classes:
        bpy.utils.register_class(c)
    # Not while Blender runs a script without its window (`-b`): Oak installs
    # the add-on that way, and timers wouldn't run there anyway.
    if not bpy.app.background:
        bpy.app.timers.register(_auto_start, first_interval=0.5)


def unregister():
    stop()
    for c in reversed(_classes):
        bpy.utils.unregister_class(c)
