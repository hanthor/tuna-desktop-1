#!/usr/bin/env python3
"""Time the moment a showing accessible stops showing (G-CAMERA-INDICATOR).

Usage: tuna-a11y-hide-watch.py APP_NAME NODE_NAME OUT.json [TIMEOUT_S]

Finds the first showing node named NODE_NAME in APP_NAME's AT-SPI tree and
prints "ready" on stdout. From then on it asks the application for that
node's parent's children every 20 ms, dropping libatspi's client-side cache
first, so every answer is the application's own. GTK 4 answers SHOWING for
any object it still knows, hidden or not; a hidden widget leaves its
parent's children instead, as it leaves a tuna-a11y-dump.py tree. Once the
parent no longer lists a showing child named NODE_NAME, or the application
no longer knows the parent, it writes found_ns, hidden_ns (CLOCK_REALTIME
nanoseconds, comparable with `date +%s%N`) and the number of polls to
OUT.json and exits 0. Exit 1 when the node never shows within TIMEOUT_S,
2 when it never hides within TIMEOUT_S after that.
"""
import json
import sys
import time

import pyatspi

POLL_S = 0.02


def find(acc, name):
    try:
        if acc.name == name and acc.getState().contains(pyatspi.STATE_SHOWING):
            return acc
        for i in range(acc.childCount):
            child = acc.getChildAtIndex(i)
            found = find(child, name) if child is not None else None
            if found is not None:
                return found
    except Exception:  # a node can die mid-walk
        pass
    return None


def locate(app_name, name, deadline):
    while time.monotonic() < deadline:
        desktop = pyatspi.Registry.getDesktop(0)
        for i in range(desktop.childCount):
            app = desktop.getChildAtIndex(i)
            if app is not None and app.name == app_name:
                node = find(app, name)
                if node is not None:
                    return node
        time.sleep(0.2)
    return None


def listed(parent, name):
    try:
        parent.clear_cache()
        for i in range(parent.childCount):
            child = parent.getChildAtIndex(i)
            if child is not None and child.name == name:
                child.clear_cache()
                return child.getState().contains(pyatspi.STATE_SHOWING)
    except Exception:  # the application dropped the parent: not shown
        return False
    return False


def main():
    if len(sys.argv) not in (4, 5):
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 64
    app_name, name, path = sys.argv[1:4]
    timeout = float(sys.argv[4]) if len(sys.argv) == 5 else 20
    node = locate(app_name, name, time.monotonic() + timeout)
    parent = node.parent if node is not None else None
    if parent is None or not listed(parent, name):
        print(f"tuna-a11y-hide-watch: {name} never showed in {app_name}", file=sys.stderr)
        return 1
    found_ns = time.time_ns()
    print("ready", flush=True)
    polls = 0
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        polls += 1
        if not listed(parent, name):
            with open(path, "w") as stream:
                json.dump({"found_ns": found_ns, "hidden_ns": time.time_ns(), "polls": polls}, stream)
            return 0
        time.sleep(POLL_S)
    print(f"tuna-a11y-hide-watch: {name} still showing after {timeout} s", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
