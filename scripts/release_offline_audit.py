#!/usr/bin/env python3
"""Audit release-mode WikiSyncer offline behavior without live services.

The audit injects a small native library into the release CLI, daemon, and, when a
native graphical session is available, the packaged Iced GUI executable.  The
library records and denies C-library IPv4/IPv6 connects, addressed datagrams, and
hostname resolution.  Unix-domain daemon IPC and inbound requests to the loopback
reader remain available.  Any recorded attempt fails the audit.
"""

from __future__ import annotations

import argparse
import html.parser
import http.client
import os
import pathlib
import platform
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse


ROOT = pathlib.Path(__file__).resolve().parents[1]
INTERPOSER_SOURCE = ROOT / "scripts" / "offline_audit_interpose.c"
RELEASE_BINARIES = ("wikisync", "wikisyncd", "wikisync-gui")
GUI_OBSERVATION_SECONDS = 3.0


class AuditError(RuntimeError):
    """The release-mode audit could not run or observed unsafe behavior."""


class PageLinks(html.parser.HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.navigation: set[str] = set()
        self.resources: set[str] = set()

    def handle_starttag(self, tag: str, attributes: list[tuple[str, str | None]]) -> None:
        values = dict(attributes)
        source = values.get("src")
        if source:
            self.resources.add(source)
        href = values.get("href")
        if not href:
            return
        if tag == "link" and "stylesheet" in (values.get("rel") or "").split():
            self.resources.add(href)
        elif tag == "a":
            self.navigation.add(href)


def compile_interposer(output_directory: pathlib.Path, compiler: str | None = None) -> pathlib.Path:
    system = platform.system()
    compiler_path = shutil.which(compiler or os.environ.get("CC", "cc"))
    if compiler_path is None:
        raise AuditError("a C compiler is required to build the offline audit interposer")
    if system == "Darwin":
        output = output_directory / "libwikisync_offline_audit.dylib"
        command = [compiler_path, "-std=c11", "-Wall", "-Wextra", "-Werror", "-dynamiclib",
                   str(INTERPOSER_SOURCE), "-o", str(output)]
    elif system == "Linux":
        output = output_directory / "libwikisync_offline_audit.so"
        command = [compiler_path, "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                   str(INTERPOSER_SOURCE), "-ldl", "-o", str(output)]
    else:
        raise AuditError(f"offline release auditing is supported on macOS and Linux, not {system}")
    completed = subprocess.run(command, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        raise AuditError(f"failed to compile offline audit interposer:\n{completed.stderr}")
    return output


def audited_environment(interposer: pathlib.Path, log_path: pathlib.Path) -> dict[str, str]:
    environment = os.environ.copy()
    environment["WIKISYNC_OFFLINE_AUDIT_LOG"] = str(log_path)
    if platform.system() == "Darwin":
        environment["DYLD_INSERT_LIBRARIES"] = str(interposer)
        environment["DYLD_FORCE_FLAT_NAMESPACE"] = "1"
    else:
        environment["LD_PRELOAD"] = str(interposer)
    # An accidental proxy lookup is itself unwanted, but clearing these variables also
    # keeps the release audit independent of runner-specific proxy configuration.
    for name in tuple(environment):
        if name.lower() in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}:
            environment.pop(name)
    return environment


def run_checked(command: list[str], environment: dict[str, str], timeout: float = 20.0) -> str:
    completed = subprocess.run(
        command,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )
    if completed.returncode != 0:
        rendered = " ".join(command)
        raise AuditError(
            f"audited command failed ({completed.returncode}): {rendered}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed.stdout


def unused_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def wait_for_reader(port: int, process: subprocess.Popen[str]) -> None:
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            raise AuditError(f"reader exited before becoming ready:\n{stdout}\n{stderr}")
        try:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=0.2)
            connection.request("GET", "/")
            response = connection.getresponse()
            response.read()
            connection.close()
            if response.status == 200:
                return
        except OSError:
            time.sleep(0.05)
    raise AuditError("reader did not become ready within 10 seconds")


def fetch(port: int, path: str) -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2.0)
    connection.request("GET", path, headers={"Connection": "close"})
    response = connection.getresponse()
    body = response.read()
    headers = {name.lower(): value for name, value in response.getheaders()}
    status = response.status
    connection.close()
    return status, headers, body


def local_path(value: str) -> str | None:
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme == "data":
        return None
    if parsed.scheme or parsed.netloc or not value.startswith("/"):
        raise AuditError(f"reader loads an outbound resource in default mode: {value!r}")
    return urllib.parse.urlunsplit(("", "", parsed.path or "/", parsed.query, ""))


def crawl_reader(port: int) -> set[str]:
    pending = ["/"]
    visited: set[str] = set()
    while pending:
        path = pending.pop()
        if path in visited or len(visited) >= 64:
            continue
        visited.add(path)
        status, headers, body = fetch(port, path)
        if status not in {200, 404}:
            raise AuditError(f"reader returned HTTP {status} for {path}")
        content_type = headers.get("content-type", "")
        if "text/html" not in content_type:
            if path.endswith(".css"):
                css = body.decode("utf-8")
                if "url(" in css.lower():
                    raise AuditError("bundled reader CSS contains a url() resource reference")
            continue
        parser = PageLinks()
        parser.feed(body.decode("utf-8"))
        for resource in parser.resources:
            resolved = local_path(resource)
            if resolved is not None:
                pending.append(resolved)
        for navigation in parser.navigation:
            parsed = urllib.parse.urlsplit(navigation)
            if not parsed.scheme and not parsed.netloc and navigation.startswith("/"):
                pending.append(urllib.parse.urlunsplit(("", "", parsed.path, parsed.query, "")))
    if "/assets/reader.css" not in visited:
        raise AuditError("reader crawl did not exercise the bundled stylesheet")
    return visited


def stop_process(process: subprocess.Popen[str], label: str) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired as error:
        process.kill()
        process.wait(timeout=2)
        raise AuditError(f"{label} did not stop after SIGTERM") from error


def gui_launch_limitation(
    environment: dict[str, str], system: str | None = None
) -> str | None:
    """Return why a GUI launch cannot provide honest evidence on this runner."""
    native_system = platform.system() if system is None else system
    if native_system == "Linux":
        if environment.get("DISPLAY") or environment.get("WAYLAND_DISPLAY"):
            return None
        return "no DISPLAY or WAYLAND_DISPLAY graphical session"
    if native_system == "Darwin":
        launchctl = pathlib.Path("/bin/launchctl")
        if not launchctl.is_file():
            return "launchctl is unavailable, so an Aqua session cannot be established"
        try:
            completed = subprocess.run(
                [str(launchctl), "managername"],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=2.0,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            return f"Aqua session detection failed: {error}"
        if completed.returncode != 0 or completed.stdout.strip() != "Aqua":
            return "the process is not running in a macOS Aqua session"
        return None
    return f"native GUI auditing is unsupported on {native_system}"


def observe_gui_launch(
    binary: pathlib.Path,
    library: pathlib.Path,
    environment: dict[str, str],
    observation_seconds: float = GUI_OBSERVATION_SECONDS,
) -> None:
    """Launch the GUI without actions and require it to stay alive for a bounded window."""
    gui_environment = environment.copy()
    gui_environment["WIKISYNC_LIBRARY"] = str(library)
    process = subprocess.Popen(
        [str(binary)],
        env=gui_environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        deadline = time.monotonic() + observation_seconds
        while time.monotonic() < deadline:
            if process.poll() is not None:
                stdout, stderr = process.communicate()
                raise AuditError(
                    f"GUI exited during its {observation_seconds:g}-second default launch "
                    f"observation ({process.returncode}):\n{stdout}\n{stderr}"
                )
            time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
    finally:
        stop_process(process, "GUI")


def ensure_binaries(binary_directory: pathlib.Path) -> dict[str, pathlib.Path]:
    binaries = {name: binary_directory / name for name in RELEASE_BINARIES}
    missing = [str(path) for path in binaries.values() if not path.is_file() or not os.access(path, os.X_OK)]
    if missing:
        raise AuditError("missing executable release binaries: " + ", ".join(missing))
    return binaries


def audit(binary_directory: pathlib.Path, build: bool) -> tuple[int, list[str], str]:
    if build:
        run_checked(
            ["cargo", "build", "--workspace", "--bins", "--release", "--locked"],
            os.environ.copy(),
            timeout=1200,
        )
    binaries = ensure_binaries(binary_directory)
    # Keep the spelling of this path short enough for the macOS sockaddr_un limit.
    with tempfile.TemporaryDirectory(prefix="ws-audit-", dir="/tmp") as temporary:
        temporary_path = pathlib.Path(temporary)
        interposer = compile_interposer(temporary_path)
        log_path = temporary_path / "network-attempts.log"
        log_path.touch(mode=0o600)
        environment = audited_environment(interposer, log_path)
        library = temporary_path / "library"

        run_checked([str(binaries["wikisync"]), "--library", str(library), "init"], environment)
        run_checked([str(binaries["wikisync"]), "--library", str(library), "status", "--json"], environment)
        run_checked([str(binaries["wikisync"]), "--library", str(library), "doctor", "--json"], environment)
        run_checked([str(binaries["wikisync"]), "--library", str(library), "search", "--json", "offline"], environment)
        run_checked([str(binaries["wikisync"]), "--library", str(library), "verify", "--full"], environment)

        port = unused_loopback_port()
        reader = subprocess.Popen(
            [str(binaries["wikisync"]), "--library", str(library), "serve", "--port", str(port)],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            wait_for_reader(port, reader)
            visited = crawl_reader(port)
        finally:
            stop_process(reader, "reader")

        daemon = subprocess.Popen(
            [str(binaries["wikisyncd"]), "--library", str(library), "run"],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 10.0
            while True:
                if daemon.poll() is not None:
                    stdout, stderr = daemon.communicate()
                    raise AuditError(f"daemon exited before becoming ready:\n{stdout}\n{stderr}")
                probe = subprocess.run(
                    [str(binaries["wikisyncd"]), "--library", str(library), "health"],
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                if probe.returncode == 0:
                    break
                if time.monotonic() >= deadline:
                    raise AuditError(f"daemon did not become healthy:\n{probe.stderr}")
                time.sleep(0.05)
            run_checked([str(binaries["wikisyncd"]), "--library", str(library), "status"], environment)
            run_checked([str(binaries["wikisyncd"]), "--library", str(library), "shutdown"], environment)
            daemon.wait(timeout=5)
            if daemon.returncode != 0:
                stdout, stderr = daemon.communicate()
                raise AuditError(f"daemon exited unsuccessfully ({daemon.returncode}):\n{stdout}\n{stderr}")
        finally:
            stop_process(daemon, "daemon")

        gui_limitation = gui_launch_limitation(environment)
        if gui_limitation is None:
            observe_gui_launch(binaries["wikisync-gui"], library, environment)
            gui_evidence = (
                f"packaged Iced GUI default launch observed for {GUI_OBSERVATION_SECONDS:g} seconds"
            )
        else:
            gui_evidence = f"GUI launch not audited: {gui_limitation}"

        attempts = log_path.read_text(encoding="utf-8").splitlines()
        if attempts:
            raise AuditError("release binaries attempted outbound networking:\n" + "\n".join(attempts))
        return len(visited), attempts, gui_evidence


def parse_arguments(arguments: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary-dir",
        type=pathlib.Path,
        default=ROOT / "target" / "release",
        help=(
            "directory containing the native release wikisync, wikisyncd, and "
            "wikisync-gui binaries"
        ),
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="audit existing release binaries instead of building --workspace --bins --release --locked",
    )
    return parser.parse_args(arguments)


def main(arguments: list[str] | None = None) -> int:
    options = parse_arguments(sys.argv[1:] if arguments is None else arguments)
    try:
        routes, attempts, gui_evidence = audit(
            options.binary_dir.resolve(), not options.skip_build
        )
    except (AuditError, OSError, subprocess.TimeoutExpired) as error:
        print(f"release offline audit: FAILED: {error}", file=sys.stderr)
        return 1
    print(
        "release offline audit: PASS: "
        f"CLI offline commands, daemon idle/IPC, and {routes} reader routes; "
        f"{gui_evidence}; {len(attempts)} outbound attempts"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
