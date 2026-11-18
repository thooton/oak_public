# Oak MCP: the Blender side of Oak's Blender MCP server (Oak's `src/mcp/blender.rs`).
#
# A small socket server inside Blender, on localhost:9876 by default. Each
# command is one JSON object, {"type": ..., "params": {...}}, answered with
# {"status": "success", "result": ...} or {"status": "error", "message": ...},
# the protocol of the community Blender MCP add-on (github.com/ahujasid/mcp-for-blender),
# so Oak's server works with either add-on. Commands run on Blender's main
# thread (bpy isn't thread-safe), from a timer.
#
# Installed by Oak (Home > Protocols > Blender > Install add-on) into Blender's
# user add-ons folder; the server starts with Blender (not in background mode)
# and the 3D viewport's sidebar (N) has an "Oak" tab to stop or start it.
#
# A Blender Oak runs on its hidden desktop (Oak's `src/mcp/blender_run.rs`)
# loads this file itself and calls `start_on` (OAK_MCP_RUN is set there, so an
# installed copy doesn't start too). Oak shows that Blender in its own window
# and passes the user's input to it: `window_frame` takes a picture of the
# window, `simulate` feeds it events (Blender runs with
# --enable-event-simulate, as it reads keys only from the real keyboard).
#
# Anything the model writes runs here with Blender's (and your user's) rights.

bl_info = {
    "name": "Oak MCP",
    "author": "Oak",
    "version": (1, 2, 0),
    "blender": (3, 0, 0),
    "location": "View3D > Sidebar > Oak",
    "description": "Lets Oak's agents work in this Blender through MCP (a local socket server)",
    "category": "Interface",
}

import io
import json
import os
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
# When the last command came (the pump runs often while they flow).
_last = 0.0
# Python property edits need not set bpy.data.is_dirty. Agent code is
# conservatively dirty until a real save/load, even after a failed script.
_script_dirty = False


@bpy.app.handlers.persistent
def _saved_or_loaded(_):
    global _script_dirty
    _script_dirty = False


def _track_saves():
    for handlers in (bpy.app.handlers.save_post, bpy.app.handlers.load_post):
        if _saved_or_loaded not in handlers:
            handlers.append(_saved_or_loaded)


class _Reply:
    def __init__(self):
        self.done = threading.Event()
        self.value = None


def _run(command):
    global _script_dirty
    kind = command.get("type")
    params = command.get("params") or {}
    if kind == "session_state":
        return {"status": "success", "result": {"dirty": bool(bpy.data.is_dirty or _script_dirty), "filepath": bpy.data.filepath}}
    if kind == "execute_code":
        if params.get("track_changes"):
            _script_dirty = True
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
    if kind == "window_frame":
        win = bpy.context.window_manager.windows[0]
        area = win.screen.areas[0]
        with bpy.context.temp_override(window=win, screen=win.screen, area=area):
            bpy.ops.screen.screenshot(filepath=params["path"])
        return {"status": "success", "result": {"width": win.width, "height": win.height}}
    if kind == "simulate":
        if not getattr(bpy.app, "use_event_simulate", False):
            return {"status": "error", "message": "This Blender wasn't started with --enable-event-simulate."}
        win = bpy.context.window_manager.windows[0]
        done = 0
        for e in params.get("events", []):
            # Oak's y runs down from the top; Blender's up from the bottom.
            x = int(e.get("x", -1))
            y = int(e.get("y", -1))
            kw = {}
            if x >= 0 and y >= 0:
                kw["x"] = x
                kw["y"] = win.height - 1 - y
            for m in ("shift", "ctrl", "alt"):
                if e.get(m):
                    kw[m] = True
            if e.get("unicode"):
                kw["unicode"] = e["unicode"]
            try:
                win.event_simulate(e["type"], e.get("value", "PRESS"), **kw)
                done += 1
            except (TypeError, ValueError, RuntimeError) as err:
                return {"status": "error", "message": "Event %d (%s): %s" % (done, e.get("type"), err)}
        return {"status": "success", "result": {"events": done}}
    if kind == "get_addon_info":
        return {"status": "success", "result": {"addon": "oak_mcp", "version": list(bl_info["version"]), "blender": bpy.app.version_string}}
    return {"status": "error", "message": "Unknown command type: %s (the Oak MCP add-on runs execute_code)" % kind}


def _pump():
    """Runs the waiting commands on the main thread; a timer calls it."""
    global _last
    import time
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
        _last = time.monotonic()
    if _server is None:
        return None
    return 0.01 if time.monotonic() - _last < 2.0 else 0.05


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


def start_on(port):
    """Starts the server on `port` (Oak's own Blenders); an error or None."""
    global _server
    if _server is not None and _server.port == port:
        return None
    stop()
    _track_saves()
    try:
        _server = _Server(port)
    except OSError as e:
        return "Port %d is in use: %s" % (port, e)
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
    for handlers in (bpy.app.handlers.save_post, bpy.app.handlers.load_post):
        if _saved_or_loaded in handlers:
            handlers.remove(_saved_or_loaded)


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
    if os.environ.get("OAK_MCP_RUN"):
        return None  # Oak runs this Blender and serves it itself
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
