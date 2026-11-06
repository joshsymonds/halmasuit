#!/usr/bin/env python3
"""Selftest for tests/hw/lab_ec2.py.

The runner calls fake `ec2` and `ssh` executables written into a temp directory. Each fake
records its argv, keeps the box list in a file there, and serves what the test configures; a
command shape it does not know exits 2. No test reaches the real ec2 CLI, ssh, nix, or AWS.
The hold polls on FakeClock, so no test waits out the 300 second deadline. The signal tests
run the runner as a subprocess and signal that process alone.
"""

import contextlib
import io
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from collections.abc import Callable
from pathlib import Path
from unittest import mock

import lab
import lab_ec2

HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "fixtures" / "synthetic-ec2-greeter" / "journal.json"
RUNNER = HERE / "lab_ec2.py"
STAMP = "20260101T000000Z"
BOX = "halmasuit-lab-k3x"
SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
REMOTE_COMMAND = "sudo -n journalctl -b 0 -o json --all --no-pager"
WAIT_SECONDS = 30

EC2_FAKE = r'''
import json
import os
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CONFIG = json.loads((HERE / "config.json").read_text())["ec2"]
STATE = HERE / "ec2-state.json"
HEADER = "NAME CLASS TYPE SPOT AGE ON-DEMAND/HR SPOT/HR"


def boxes():
    return json.loads(STATE.read_text()) if STATE.exists() else []


def store(rows):
    STATE.write_text(json.dumps(rows))


def launch(name):
    store(boxes() + [{"name": name, "class": "halmasuit-lab"}])


argv = sys.argv[1:]
with (HERE / "ec2-calls.jsonl").open("a") as calls:
    calls.write(json.dumps({"argv": argv, "flake": os.environ.get("EC2_FLEET_FLAKE")}) + "\n")
verb = argv[0] if argv else ""

if verb == "ls":
    if CONFIG["ls_garbled_after_down"] and (HERE / "ec2-downed").exists():
        sys.stdout.buffer.write(b"NAME \xff\n")
        sys.stdout.flush()
        sys.exit(0)
    print(HEADER)
    for box in boxes():
        print(f"{box['name']} {box['class']} t3.large no 1m 0.1000 0.0300")
    sys.exit(0)

if verb == "up":
    mode = CONFIG["up"]
    if mode == "fail-before-launch":
        print("ec2: no capacity", file=sys.stderr)
        sys.exit(1)
    name = f"halmasuit-lab-{CONFIG['suffix']}"
    launch(name)
    if mode == "fail-after-launch":
        print("ec2: boot failed", file=sys.stderr)
        sys.exit(1)
    print("requesting capacity")
    if mode == "ready":
        print(f"{name} ready")
        sys.exit(0)
    if mode == "ready-missing":
        sys.exit(0)
    if mode in ("hang", "hang-bad-stderr"):
        if mode == "hang-bad-stderr":
            sys.stderr.buffer.write(b"bad \xff\n")
            sys.stderr.flush()
        sys.stdout.flush()
        (HERE / "ec2-up.pid").write_text(str(os.getpid()))
        while True:
            time.sleep(60)
    sys.exit(2)

if verb == "down":
    time.sleep(CONFIG["down_sleep"])
    if CONFIG["down_fails"]:
        print("ec2: terminate refused", file=sys.stderr)
        sys.exit(1)
    name = argv[1]
    rows = boxes()
    kept = [row for row in rows if row["name"] != name]
    if len(kept) == len(rows):
        print(f"ec2: no fleet box named {name}", file=sys.stderr)
        sys.exit(1)
    store(kept)
    (HERE / "ec2-downed").write_text("")
    print(f"terminating i-0123456789abcdef0 ({name})")
    sys.exit(0)

print(f"fake ec2: unknown command {verb!r}", file=sys.stderr)
sys.exit(2)
'''

SSH_FAKE = r'''
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
CONFIG = json.loads((HERE / "config.json").read_text())["ssh"]
with (HERE / "ssh-calls.jsonl").open("a") as calls:
    calls.write(json.dumps(sys.argv[1:]) + "\n")
if CONFIG["fails"]:
    print("ssh: connect refused", file=sys.stderr)
    sys.exit(255)
if CONFIG["journal"] is not None:
    sys.stdout.write(Path(CONFIG["journal"]).read_text())
'''


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds

    def stamp(self) -> str:
        return STAMP


def install(path: Path, source: str) -> None:
    path.write_text(f"#!{sys.executable}\n{source}")
    path.chmod(0o755)


def read_jsonl(path: Path) -> list:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def make_repo(path: Path) -> str:
    path.mkdir()
    git = ["git", "-C", str(path)]
    subprocess.run([*git, "init", "-q"], check=True)
    subprocess.run(
        [
            *git,
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
        check=True,
    )
    done = subprocess.run([*git, "rev-parse", "HEAD"], check=True, capture_output=True, text=True)
    return done.stdout.strip()


def wait_for(check: Callable[[], object], what: str) -> object:
    deadline = time.monotonic() + WAIT_SECONDS
    while True:
        value = check()
        if value is not None:
            return value
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {what}")
        time.sleep(0.05)


def process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class FakeTools:
    """The fake ec2 and ssh executables in one temp directory, configured per test."""

    def __init__(self, root: Path) -> None:
        self.root = root
        root.mkdir()
        self.ec2 = root / "ec2"
        self.ssh = root / "ssh"
        self.ec2_config = {
            "up": "ready",
            "suffix": "k3x",
            "down_fails": False,
            "down_sleep": 0,
            "ls_garbled_after_down": False,
        }
        self.ssh_config = {"fails": False, "journal": str(FIXTURE)}
        install(self.ec2, EC2_FAKE)
        install(self.ssh, SSH_FAKE)
        self.save()

    def save(self) -> None:
        config = {"ec2": self.ec2_config, "ssh": self.ssh_config}
        (self.root / "config.json").write_text(json.dumps(config))

    def set_ec2(self, **changes: object) -> None:
        self.ec2_config.update(changes)
        self.save()

    def set_ssh(self, **changes: object) -> None:
        self.ssh_config.update(changes)
        self.save()

    def serve_journal(self, data: bytes | None) -> None:
        if data is None:
            self.ssh_config["journal"] = None
        else:
            served = self.root / "served-journal.json"
            served.write_bytes(data)
            self.ssh_config["journal"] = str(served)
        self.save()

    def add_box(self, name: str) -> None:
        (self.root / "ec2-state.json").write_text(json.dumps([{"name": name, "class": "halmasuit-lab"}]))

    def ec2_calls(self, verb: str) -> list[dict]:
        return [call for call in read_jsonl(self.root / "ec2-calls.jsonl") if call["argv"][:1] == [verb]]

    def ssh_calls(self) -> list[list[str]]:
        return read_jsonl(self.root / "ssh-calls.jsonl")

    def up_pid(self) -> int | None:
        pid_file = self.root / "ec2-up.pid"
        return int(pid_file.read_text()) if pid_file.exists() else None

    def listed(self) -> list[str]:
        done = subprocess.run([str(self.ec2), "ls"], capture_output=True, text=True, check=True)
        return [line.split()[0] for line in done.stdout.splitlines()[1:] if line.split()]


class RunnerTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        self.tools = FakeTools(root / "tools")
        self.addCleanup(self.kill_hung_up)
        self.repo_root = root / "halmasuit"
        self.sha = make_repo(self.repo_root)
        self.state_root = root / "runs"
        self.nix_config = root / "nix-config"
        self.clock = FakeClock()

    def kill_hung_up(self) -> None:
        pid = self.tools.up_pid()
        if pid is not None and process_alive(pid):
            os.kill(pid, signal.SIGKILL)

    def args(self) -> list[str]:
        return [
            "--ec2",
            str(self.tools.ec2),
            "--ssh",
            str(self.tools.ssh),
            "--nix-config",
            str(self.nix_config),
            "--repo-root",
            str(self.repo_root),
            "--state-root",
            str(self.state_root),
        ]

    def run_runner(self) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = lab_ec2.main(self.args(), clock=self.clock)
        return code, out.getvalue()

    def run_dir(self) -> Path:
        """The one run directory under state_root; a subprocess run stamps it from the real clock."""
        found = sorted(self.state_root.glob("*-ec2-greeter"))
        self.assertEqual(len(found), 1, found)
        return found[0]

    def run_log(self) -> str:
        return (self.run_dir() / "run.log").read_text()

    def assert_box_ended(self) -> None:
        self.assertEqual([call["argv"] for call in self.tools.ec2_calls("down")], [["down", BOX]])
        self.assertEqual(self.tools.listed(), [])

    def test_pass_launches_checks_and_ends_the_box(self) -> None:
        before = {number: signal.getsignal(number) for number in SIGNALS}
        code, out = self.run_runner()
        self.assertEqual(code, 0, out)
        self.assertEqual(
            self.tools.ec2_calls("up"),
            [
                {
                    "argv": [
                        "up",
                        "halmasuit-lab",
                        "--override-input",
                        "halmasuit",
                        f"git+file://{self.repo_root}?rev={self.sha}",
                    ],
                    "flake": str(self.nix_config),
                }
            ],
        )
        self.assertEqual((self.run_dir() / "journal.json").read_bytes(), FIXTURE.read_bytes())
        self.assertTrue((self.run_dir() / "timeline.json").is_file())
        log = self.run_log()
        self.assertIn("ec2 up: halmasuit-lab-k3x ready", log)
        self.assertIn("PASS ec2-greeter", log)
        self.assert_box_ended()
        self.assertTrue(self.tools.ssh_calls())
        for call in self.tools.ssh_calls():
            self.assertEqual(call[-2], f"joshsymonds@{BOX}")
            self.assertEqual(call[-1], REMOTE_COMMAND)
            self.assertIn("StrictHostKeyChecking=accept-new", call)
        self.assertEqual({number: signal.getsignal(number) for number in SIGNALS}, before)

    def test_profile_failure_fails_the_run_and_ends_the_box(self) -> None:
        journal = FIXTURE.read_bytes()
        self.assertEqual(journal.count(b"NVIDIA Corporation"), 1)
        self.tools.serve_journal(journal.replace(b"NVIDIA Corporation", b"Mesa Project"))
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL ec2-greeter: GL Vendor is not NVIDIA", self.run_log())
        self.assert_box_ended()

    def test_refuses_when_a_lab_box_is_already_listed(self) -> None:
        self.tools.add_box("halmasuit-lab-old")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("refused: halmasuit-lab-old already listed; nothing launched", self.run_log())
        self.assertEqual(self.tools.ec2_calls("up"), [])
        self.assertEqual(self.tools.ec2_calls("down"), [])
        self.assertEqual(self.tools.listed(), ["halmasuit-lab-old"])

    def test_up_failing_after_launch_ends_the_launched_box(self) -> None:
        self.tools.set_ec2(up="fail-after-launch")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("ec2 up failed (rc 1): ec2: boot failed", self.run_log())
        self.assert_box_ended()

    def test_up_failing_before_launch_fails_and_leaves_nothing(self) -> None:
        self.tools.set_ec2(up="fail-before-launch")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("ec2: no capacity", self.run_log())
        self.assertEqual(self.tools.ec2_calls("down"), [])
        self.assertEqual(self.tools.listed(), [])

    def test_ready_line_missing_fails_and_ends_the_launched_box(self) -> None:
        self.tools.set_ec2(up="ready-missing")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("ec2 up exited 0 without a `halmasuit-lab-<x> ready` last line", self.run_log())
        self.assert_box_ended()

    def test_ssh_failure_fails_the_run_and_ends_the_box(self) -> None:
        self.tools.set_ssh(fails=True)
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        log = self.run_log()
        self.assertIn("last read problem: read the boot journal failed (rc 255): ssh: connect refused", log)
        self.assertIn("journal.json not written", log)
        self.assert_box_ended()

    def test_hold_timeout_fails_the_run_and_ends_the_box(self) -> None:
        self.tools.serve_journal(None)
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL ec2-greeter: no halmasuit.service foreground_changed to greeter event", self.run_log())
        self.assertGreaterEqual(self.clock.now, lab.HOLD_DEADLINE_SECONDS)
        self.assertTrue((self.run_dir() / "journal.json").is_file())
        self.assert_box_ended()

    def test_failing_ec2_down_exits_nonzero_naming_the_box(self) -> None:
        self.tools.set_ec2(down_fails=True)
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn(f"could not terminate {BOX}", self.run_log())
        self.assertIn(BOX, out)
        self.assertEqual(self.tools.listed(), [BOX])

    def assert_killed_while_hung(self, signum: int, up: str = "hang") -> None:
        self.tools.set_ec2(up=up)
        runner = subprocess.Popen(
            [sys.executable, "-B", str(RUNNER), *self.args()],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            pid = wait_for(self.tools.up_pid, "the hung ec2 up to start")
            runner.send_signal(signum)
            out, err = runner.communicate(timeout=60)
        finally:
            if runner.poll() is None:
                runner.kill()
                runner.communicate()
        self.assertEqual(runner.returncode, 128 + signum, out + err)
        self.assertFalse(process_alive(pid))
        self.assertIn("stopping ec2 up", self.run_log())
        self.assert_box_ended()

    def test_sigterm_while_up_hangs_ends_the_box(self) -> None:
        self.assert_killed_while_hung(signal.SIGTERM)

    def test_sigint_while_up_hangs_ends_the_box(self) -> None:
        self.assert_killed_while_hung(signal.SIGINT)

    def test_sighup_while_up_hangs_ends_the_box(self) -> None:
        self.assert_killed_while_hung(signal.SIGHUP)

    def test_undecodable_stderr_while_up_hangs_still_ends_the_box(self) -> None:
        self.assert_killed_while_hung(signal.SIGTERM, up="hang-bad-stderr")

    def test_sigint_to_the_group_during_a_slow_ec2_down_still_ends_the_box(self) -> None:
        self.tools.set_ec2(up="hang", down_sleep=3)
        runner = subprocess.Popen(
            [sys.executable, "-B", str(RUNNER), *self.args()],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        try:
            wait_for(self.tools.up_pid, "the hung ec2 up to start")
            runner.send_signal(signal.SIGTERM)
            wait_for(lambda: self.tools.ec2_calls("down") or None, "the cleanup's ec2 down to start")
            os.killpg(runner.pid, signal.SIGINT)
            out, err = runner.communicate(timeout=60)
        finally:
            if runner.poll() is None:
                runner.kill()
                runner.communicate()
        self.assertEqual(runner.returncode, 128 + signal.SIGTERM, out + err)
        self.assertEqual(self.tools.listed(), [])

    def test_second_signal_at_start_of_cleanup_still_ends_the_box(self) -> None:
        self.tools.serve_journal(None)
        real_signal = signal.signal
        armed: list[bool] = []

        def second_signal_at_first_handler_change(signum: int, handler: object) -> object:
            if armed:
                armed.clear()
                os.kill(os.getpid(), signal.SIGHUP)
            return real_signal(signum, handler)

        class SigtermDuringHold(FakeClock):
            def sleep(self, seconds: float) -> None:
                armed.append(True)
                os.kill(os.getpid(), signal.SIGTERM)

        self.clock = SigtermDuringHold()
        with mock.patch.object(signal, "signal", second_signal_at_first_handler_change):
            code, out = self.run_runner()
        self.assertEqual(code, 128 + signal.SIGTERM, out)
        self.assert_box_ended()

    def test_first_signal_as_cleanup_begins_still_ends_the_box(self) -> None:
        real_begin = lab_ec2._Interrupts.begin_cleanup
        armed = [True]

        def signal_before_cleaning_is_set(interrupts: lab_ec2._Interrupts) -> None:
            # The handler runs here as it would at begin_cleanup's first eval-breaker check: after the
            # run body has passed and before the cleaning flag is set.
            if armed:
                armed.clear()
                os.kill(os.getpid(), signal.SIGTERM)
            real_begin(interrupts)

        with mock.patch.object(lab_ec2._Interrupts, "begin_cleanup", signal_before_cleaning_is_set):
            code, out = self.run_runner()
        self.assertEqual(code, 128 + signal.SIGTERM, out)
        self.assert_box_ended()

    def test_undecodable_final_listing_is_recorded_and_the_run_fails(self) -> None:
        self.tools.set_ec2(ls_garbled_after_down=True)
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("could not list boxes after ending the run", self.run_log())
        self.assertEqual([call["argv"] for call in self.tools.ec2_calls("down")], [["down", BOX]])


if __name__ == "__main__":
    unittest.main()
