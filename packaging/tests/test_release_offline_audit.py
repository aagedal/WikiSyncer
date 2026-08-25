import importlib.util
import pathlib
import platform
import shutil
import subprocess
import tempfile
import unittest


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


if __name__ == "__main__":
    unittest.main()
