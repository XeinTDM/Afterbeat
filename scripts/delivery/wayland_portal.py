"""Serve a private D-Bus portal fixture while the Rust Wayland delivery test runs."""

import subprocess
import sys

from gi.repository import Gio, GLib


BUS = "org.freedesktop.portal.Desktop"
PATH = "/org/freedesktop/portal/desktop"
GLOBAL = "org.freedesktop.portal.GlobalShortcuts"
REMOTE = "org.freedesktop.portal.RemoteDesktop"
SCREEN = "org.freedesktop.portal.ScreenCast"
TEST = "io.github.xeintdm.Afterbeat.TestPortal"
OPTIONS = "a{sv}"
SHORTCUTS = "a(sa{sv})"


def register(connection, name, methods, method_call, properties=None):
    xml = [f'<node><interface name="{name}">']
    for method, inputs, outputs in methods:
        xml.append(f'<method name="{method}">')
        for direction, types in (("in", inputs), ("out", outputs)):
            xml.extend(f'<arg type="{kind}" direction="{direction}"/>' for kind in types)
        xml.append("</method>")
    for key, value in (properties or {}).items():
        xml.append(f'<property name="{key}" type="{value.get_type_string()}" access="read"/>')
    xml.append("</interface></node>")
    interface = Gio.DBusNodeInfo.new_for_xml("".join(xml)).interfaces[0]
    connection.register_object(
        PATH, interface, method_call,
        lambda _conn, _sender, _path, _iface, name: properties[name], None,
    )


class Portal:
    def __init__(self, connection):
        self.trace = []
        self.sessions = {}
        self.stop_enabled = True
        self.bound = []
        self.share = False
        self.configuration = None
        self.connection = connection
        register(connection, GLOBAL, [
            ("CreateSession", [OPTIONS], ["o"]),
            ("BindShortcuts", ["o", SHORTCUTS, "s", OPTIONS], ["o"]),
            ("ListShortcuts", ["o", OPTIONS], ["o"]),
            ("ConfigureShortcuts", ["o", "s", OPTIONS], []),
        ], self.call, {"version": GLib.Variant("u", 2)})
        register(connection, REMOTE, [
            ("CreateSession", [OPTIONS], ["o"]),
            ("SelectDevices", ["o", OPTIONS], ["o"]),
            ("Start", ["o", "s", OPTIONS], ["o"]),
            ("NotifyPointerMotionAbsolute", ["o", OPTIONS, "u", "d", "d"], []),
            ("NotifyPointerButton", ["o", OPTIONS, "i", "u"], []),
            ("NotifyKeyboardKeysym", ["o", OPTIONS, "i", "u"], []),
        ], self.call, {"AvailableDeviceTypes": GLib.Variant("u", 3)})
        register(connection, SCREEN, [("SelectSources", ["o", OPTIONS], ["o"])], self.call)
        register(connection, TEST, [
            ("EmitShortcut", ["s", "b"], []),
            ("Revoke", [], []),
            ("EnableStop", ["b"], []),
            ("Trace", [], ["as"]),
            ("FinishConfiguration", [], []),
        ], self.call)

    def current_shortcuts(self):
        return [(key, props) for key, props in self.bound if key != "stop" or self.stop_enabled]

    def call(self, _connection, sender, _path, iface, method, parameters, invocation):
        args = parameters.unpack()
        try:
            if iface == TEST:
                if method == "Trace":
                    invocation.return_value(GLib.Variant("(as)", (self.trace,)))
                    return
                if method == "EmitShortcut":
                    key, pressed = args
                    signal = "Activated" if pressed else "Deactivated"
                    body = GLib.Variant("(osta{sv})", (self.sessions[GLOBAL], key, 0, {}))
                    self.connection.emit_signal(None, PATH, GLOBAL, signal, body)
                elif method == "Revoke":
                    self.connection.emit_signal(
                        None, self.sessions[REMOTE], "org.freedesktop.portal.Session",
                        "Closed", GLib.Variant("(a{sv})", ({},)),
                    )
                elif method == "EnableStop":
                    self.stop_enabled = args[0]
                    if GLOBAL in self.sessions:
                        self.connection.emit_signal(
                            None, PATH, GLOBAL, "ShortcutsChanged",
                            GLib.Variant("(oa(sa{sv}))", (self.sessions[GLOBAL], self.current_shortcuts())),
                        )
                elif method == "FinishConfiguration":
                    self.configuration.return_value(None)
                    self.configuration = None
                invocation.return_value(None)
                return
            if method.startswith("Notify"):
                assert args[0] == self.sessions[REMOTE]
                if method == "NotifyPointerMotionAbsolute":
                    self.trace.append(f"move:{args[2]}:{args[3]:g}:{args[4]:g}")
                else:
                    name = "button" if method == "NotifyPointerButton" else "key"
                    self.trace.append(f"{name}:{args[2]}:{args[3]}")
                invocation.return_value(None)
                return
            if method == "ConfigureShortcuts":
                self.trace.append("configure")
                self.configuration = invocation
                return

            response = {}
            sender = sender.removeprefix(":").replace(".", "_")
            options = args[-1]
            if method == "CreateSession":
                session = f"{PATH}/session/{sender}/{options['session_handle_token']}"
                self.sessions[iface] = session
                self.share = self.share and iface != REMOTE
                response["session_handle"] = GLib.Variant("s" if iface == GLOBAL else "o", session)
            elif method in ("BindShortcuts", "ListShortcuts"):
                if method == "BindShortcuts":
                    self.bound = [(key, {"trigger_description": GLib.Variant("s", props["preferred_trigger"])})
                                  for key, props in args[1]]
                response["shortcuts"] = GLib.Variant(SHORTCUTS, self.current_shortcuts())
            elif method == "SelectSources":
                assert options["types"] == 1
                self.share = True
            elif method == "Start":
                response["devices"] = GLib.Variant("u", 3)
                if self.share:
                    response["streams"] = GLib.Variant("a(ua{sv})", [(7, {
                        "position": GLib.Variant("(ii)", (-800, 0)),
                        "size": GLib.Variant("(ii)", (800, 600)),
                        "source_type": GLib.Variant("u", 1),
                    })])
            handle = f"{PATH}/request/{sender}/{options['handle_token']}"
            # A real portal may emit the response before returning the request handle.
            self.connection.emit_signal(None, handle, "org.freedesktop.portal.Request",
                                        "Response", GLib.Variant("(ua{sv})", (0, response)))
            invocation.return_value(GLib.Variant("(o)", (handle,)))
        except Exception as error:
            invocation.return_dbus_error(TEST + ".InvalidRequest", str(error))


def main():
    connection = Gio.bus_get_sync(Gio.BusType.SESSION)
    connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                         "RequestName", GLib.Variant("(su)", (BUS, 4)), None,
                         Gio.DBusCallFlags.NONE, 2000, None)
    portal = Portal(connection)
    with subprocess.Popen(sys.argv[1:]) as child:
        loop = GLib.MainLoop()

        def finished():
            if child.poll() is None:
                return True
            loop.quit()
            return False

        def timeout():
            child.kill()
            return False

        GLib.timeout_add(25, finished)
        GLib.timeout_add_seconds(45, timeout)
        loop.run()
        assert portal.trace, "The test did not deliver input through the mock portal."
        return child.returncode


if __name__ == "__main__":
    sys.exit(main())
