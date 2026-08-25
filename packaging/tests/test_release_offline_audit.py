import importlib.util
import pathlib
import platform
import shutil
import signal
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "release_offline_audit.py"


def load_audit_module():
    specification = importlib.util.spec_from_file_location("release_offline_audit", SCRIPT)
    module = importlib.util.module_from_spec(specification)
    assert specification.loader is not None
    specification.loader.exec_module(module)
    return module


@unittest.skipUnless(platform.system() in {"Darwin", "Linux"}, "native audit is macOS/Linux only")
class ReleaseOfflineAuditTests(unittest.TestCase):
    def test_interposer_records_and_denies_outbound_calls(self):
        audit = load_audit_module()
        compiler = shutil.which("cc")
        if compiler is None:
            self.skipTest("C compiler unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            interposer = audit.compile_interposer(root, compiler)
            probe_source = root / "probe.c"
            probe = root / "probe"
            probe_source.write_text(
                """
#include <arpa/inet.h>
#include <netdb.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>
int main(void) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int datagram;
    struct sockaddr_in address = {0};
    struct addrinfo hints = {0};
    struct addrinfo *result = 0;
    struct iovec vector = {0};
    struct msghdr message = {0};
    address.sin_family = AF_INET;
    address.sin_port = htons(9);
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (fd < 0) return 2;
    if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) == 0) return 3;
    close(fd);
    datagram = socket(AF_INET, SOCK_DGRAM, 0);
    if (datagram < 0) return 4;
    if (sendto(datagram, "x", 1, 0, (const struct sockaddr *)&address, sizeof(address)) >= 0)
        return 5;
    vector.iov_base = (void *)"x";
    vector.iov_len = 1;
    message.msg_name = &address;
    message.msg_namelen = sizeof(address);
    message.msg_iov = &vector;
    message.msg_iovlen = 1;
    if (sendmsg(datagram, &message, 0) >= 0) return 6;
    close(datagram);
    hints.ai_family = AF_INET;
    if (getaddrinfo("offline-audit.invalid", "443", &hints, &result) != EAI_AGAIN) return 7;
    return 0;
}
""",
                encoding="utf-8",
            )
            subprocess.run([compiler, "-std=c11", "-Wall", "-Wextra", "-Werror",
                            str(probe_source), "-o", str(probe)], check=True)
            log = root / "attempts.log"
            log.touch(mode=0o600)
            completed = subprocess.run(
                [str(probe)],
                env=audit.audited_environment(interposer, log),
                check=False,
            )
            self.assertEqual(completed.returncode, 0)
            self.assertEqual(
                log.read_text(encoding="utf-8"),
                "connect AF_INET\nsendto AF_INET\nsendmsg AF_INET\ngetaddrinfo AF_INET\n",
            )

    def test_reader_resource_policy_rejects_remote_urls(self):
        audit = load_audit_module()
        self.assertEqual(audit.local_path("/assets/reader.css"), "/assets/reader.css")
        self.assertIsNone(audit.local_path("data:image/png;base64,AA=="))
        with self.assertRaises(audit.AuditError):
            audit.local_path("https://example.invalid/remote.js")

    def test_gui_capability_reports_linux_display_limit_honestly(self):
        audit = load_audit_module()
        self.assertEqual(
            audit.gui_launch_limitation({}, system="Linux"),
            "no DISPLAY or WAYLAND_DISPLAY graphical session",
        )
        self.assertIsNone(
            audit.gui_launch_limitation({"DISPLAY": ":99"}, system="Linux")
        )
        self.assertIsNone(
            audit.gui_launch_limitation(
                {"WAYLAND_DISPLAY": "wayland-0"}, system="Linux"
            )
        )

    def test_release_binary_set_requires_the_packaged_gui(self):
        audit = load_audit_module()
        with tempfile.TemporaryDirectory() as temporary:
            binary_directory = pathlib.Path(temporary)
            for name in ("wikisync", "wikisyncd"):
                binary = binary_directory / name
                binary.touch(mode=0o755)
            with self.assertRaisesRegex(audit.AuditError, "wikisync-gui"):
                audit.ensure_binaries(binary_directory)

    def test_gui_observation_uses_default_launch_and_is_bounded(self):
        audit = load_audit_module()

        class RunningProcess:
            returncode = None

            def __init__(self):
                self.signal = None

            def poll(self):
                return self.returncode

            def send_signal(self, sent_signal):
                self.signal = sent_signal
                self.returncode = -sent_signal

            def wait(self, timeout):
                del timeout
                return self.returncode

            def kill(self):
                self.returncode = -signal.SIGKILL

        process = RunningProcess()
        with mock.patch.object(audit.subprocess, "Popen", return_value=process) as popen:
            audit.observe_gui_launch(
                pathlib.Path("/candidate/bin/wikisync-gui"),
                pathlib.Path("/tmp/offline-library"),
                {"AUDIT_SENTINEL": "yes"},
                observation_seconds=0.0,
            )

        self.assertEqual(process.signal, signal.SIGTERM)
        arguments, keywords = popen.call_args
        self.assertEqual(arguments[0], ["/candidate/bin/wikisync-gui"])
        self.assertEqual(keywords["env"]["WIKISYNC_LIBRARY"], "/tmp/offline-library")
        self.assertEqual(keywords["env"]["AUDIT_SENTINEL"], "yes")

    def test_gui_observation_rejects_an_early_exit(self):
        audit = load_audit_module()
        process = mock.Mock()
        process.poll.return_value = 7
        process.returncode = 7
        process.communicate.return_value = ("", "display initialization failed")
        with mock.patch.object(audit.subprocess, "Popen", return_value=process):
            with self.assertRaisesRegex(audit.AuditError, "GUI exited during"):
                audit.observe_gui_launch(
                    pathlib.Path("/candidate/bin/wikisync-gui"),
                    pathlib.Path("/tmp/offline-library"),
                    {},
                    observation_seconds=1.0,
                )
        process.send_signal.assert_not_called()


if __name__ == "__main__":
    unittest.main()
