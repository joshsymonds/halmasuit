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
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import zlib
from collections.abc import Callable
from decimal import Decimal
from pathlib import Path
from typing import Any
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
OUTPUT = "DP-1"
FRAME_COUNT = 64
FRAME_SIZE = 4
FIRST_MONO_US = 1_599_000
FRAME_STEP_US = 500_000
GREETER_FRAME = FRAME_COUNT - 1
PARTIAL_LINE_BYTES = 25
DURING_COPY_FRAMES = 80

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
import io
import json
import os
import shlex
import sys
import tarfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CONFIG = json.loads((HERE / "config.json").read_text())["ssh"]
CHUNK = 1024
with (HERE / "ssh-calls.jsonl").open("a") as calls:
    calls.write(json.dumps(sys.argv[1:]) + "\n")
if CONFIG["fails"]:
    print("ssh: connect refused", file=sys.stderr)
    sys.exit(255)


class Recorder:
    """The live recorder on the box: lands one scripted step inside each member's read.

    A step writes its PNG, then appends its index bytes. The first step completes the index's
    partial last line, so the box is mid-write when the copy starts.
    """

    def __init__(self, source, append):
        self.source = source
        self.append = append
        self.next = 0

    def land(self):
        if self.append is None or self.next >= len(self.append["steps"]):
            return
        step = self.append["steps"][self.next]
        self.next += 1
        target = self.source / step["png"]
        target.parent.mkdir(exist_ok=True)
        target.write_bytes((Path(self.append["pending"]) / step["png"]).read_bytes())
        with (self.source / "index.jsonl").open("ab") as index:
            index.write(step["index"].encode())


def stamp(path):
    status = path.stat()
    return (status.st_size, status.st_mtime_ns)


def read_member(path, recorder):
    with path.open("rb") as handle:
        head = handle.read(CHUNK)
        recorder.land()
        return head + handle.read()


def archive_members(source, names, recorder, mode):
    """Writes the members to stdout as GNU tar would: exit 2 for a name it cannot stat, else 1 for a change."""
    status = 0
    with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as archive:
        for name in names:
            path = source / name
            try:
                before = stamp(path)
            except FileNotFoundError:
                print(f"tar: {name}: Cannot stat: No such file or directory", file=sys.stderr)
                status = 2
                continue
            data = read_member(path, recorder)
            if stamp(path) != before:
                print(f"tar: {name}: file changed as we read it", file=sys.stderr)
                status = status or 1
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        if mode == "escape":
            payload = b"outside the frames directory"
            member = tarfile.TarInfo("../escape.txt")
            member.size = len(payload)
            archive.addfile(member, io.BytesIO(payload))
    return status


remote = sys.argv[-1]
words = shlex.split(remote)
if remote.startswith("sudo -n cat "):
    frames = CONFIG["frames"]
    index = Path(frames["source"]) / "index.jsonl"
    if words[3] != f"{frames['dir']}/index.jsonl":
        print(f"cat: {words[3]}: Cannot open: No such file or directory", file=sys.stderr)
        sys.exit(2)
    if not index.exists():
        print(f"cat: {words[3]}: No such file or directory", file=sys.stderr)
        sys.exit(1)
    sys.stdout.buffer.write(index.read_bytes())
    sys.exit(0)
if remote.startswith("sudo -n tar "):
    frames = CONFIG["frames"]
    if words[4] != frames["dir"]:
        print(f"tar: {words[4]}: Cannot open: No such file or directory", file=sys.stderr)
        sys.exit(2)
    if frames["mode"] == "fail":
        print("ssh: connection reset during frames copy", file=sys.stderr)
        sys.exit(255)
    if frames["mode"] == "hang":
        (HERE / "ssh-copy.pid").write_text(str(os.getpid()))
        while True:
            time.sleep(60)
    source = Path(frames["source"])
    names = [raw.decode() for raw in sys.stdin.buffer.read().split(b"\0") if raw]
    sys.exit(archive_members(source, names, Recorder(source, frames["append"]), frames["mode"]))
if CONFIG["journal"] is not None:
    sys.stdout.write(Path(CONFIG["journal"]).read_text())
'''

DEFAULT_FRAMES_DIR = "/run/halmasuit-lab/frames"


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


def tree(root: Path) -> dict[str, bytes]:
    """Every regular file under root, keyed by its path relative to root."""
    return {str(path.relative_to(root)): path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}


def tar_command(frames_dir: str) -> str:
    return f"sudo -n tar -C {frames_dir} -cf - --null -T -"


def index_command(frames_dir: str) -> str:
    return f"sudo -n cat {frames_dir}/index.jsonl"


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def frame_pixels(seq: int) -> bytes:
    """The rgb8 pixels of one frame; each seq has its own colour."""
    red, green, blue = (seq * 4) % 256, 255 - (seq * 4) % 256, (seq * 7) % 256
    return bytes((red, green, blue)) * (FRAME_SIZE * FRAME_SIZE)


def png_chunk(kind: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def frame_png(seq: int) -> bytes:
    """One frame as an 8-bit RGB PNG, encoded with zlib and struct alone."""
    row = FRAME_SIZE * 3
    pixels = frame_pixels(seq)
    raw = b"".join(b"\x00" + pixels[y * row : (y + 1) * row] for y in range(FRAME_SIZE))
    header = struct.pack(">IIBBBBB", FRAME_SIZE, FRAME_SIZE, 8, 2, 0, 0, 0)
    return (
        PNG_SIGNATURE
        + png_chunk(b"IHDR", header)
        + png_chunk(b"IDAT", zlib.compress(raw))
        + png_chunk(b"IEND", b"")
    )


def png_pixels(data: bytes) -> bytes:
    """The rgb8 pixels of a PNG as frame_png writes it: one IHDR, one IDAT, filter 0 on each row."""
    assert data.startswith(PNG_SIGNATURE), "not a PNG"
    chunks: dict[bytes, bytes] = {}
    offset = len(PNG_SIGNATURE)
    while offset < len(data):
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        chunks[data[offset + 4 : offset + 8]] = data[offset + 8 : offset + 8 + length]
        offset += 12 + length
    width, height, depth, color = struct.unpack(">IIBB", chunks[b"IHDR"][:10])
    assert (width, height, depth, color) == (FRAME_SIZE, FRAME_SIZE, 8, 2), (width, height, depth, color)
    raw = zlib.decompress(chunks[b"IDAT"])
    stride = 1 + width * 3
    rows = []
    for y in range(height):
        assert raw[y * stride] == 0, f"row {y} filter {raw[y * stride]}"
        rows.append(raw[y * stride + 1 : (y + 1) * stride])
    return b"".join(rows)


def index_line(seq: int, *, mono_us: int | None = None, dropped: int = 0) -> dict[str, Any]:
    if mono_us is None:
        mono_us = FIRST_MONO_US + FRAME_STEP_US * seq
    return {
        "seq": seq,
        "output": OUTPUT,
        "mono_us": mono_us,
        "width": FRAME_SIZE,
        "height": FRAME_SIZE,
        "format": "rgb8",
        "dropped": dropped,
    }


def write_index(path: Path, lines: list[dict[str, Any]]) -> None:
    path.write_text("".join(json.dumps(line) + "\n" for line in lines), encoding="utf-8")


def write_recorder(directory: Path) -> None:
    """A recorder directory as the box holds it: one output, a PNG and an index line per seq."""
    (directory / OUTPUT).mkdir(parents=True)
    for seq in range(FRAME_COUNT):
        (directory / OUTPUT / f"{seq:08d}.png").write_bytes(frame_png(seq))
    write_index(directory / "index.jsonl", [index_line(seq) for seq in range(FRAME_COUNT)])


def decode_video(path: Path) -> bytes:
    done = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"],
        capture_output=True,
        check=True,
    )
    return done.stdout


def video_times_us(path: Path) -> list[Decimal]:
    done = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "frame=pts_time", "-of", "csv=p=0", str(path)],
        capture_output=True,
        text=True,
        check=True,
    )
    return [Decimal(text) * 1_000_000 for text in done.stdout.split()]


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
        served = root / "served-frames"
        write_recorder(served)
        self.frames_source = served
        self.ssh_config = {
            "fails": False,
            "journal": str(FIXTURE),
            "frames": {"dir": DEFAULT_FRAMES_DIR, "source": str(served), "mode": "serve", "append": None},
        }
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

    def set_frames(self, **changes: object) -> None:
        self.ssh_config["frames"].update(changes)
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

    def pid_of(self, name: str) -> int | None:
        pid_file = self.root / name
        return int(pid_file.read_text()) if pid_file.exists() else None

    def up_pid(self) -> int | None:
        return self.pid_of("ec2-up.pid")

    def copy_pid(self) -> int | None:
        return self.pid_of("ssh-copy.pid")

    def listed(self) -> list[str]:
        done = subprocess.run([str(self.ec2), "ls"], capture_output=True, text=True, check=True)
        return [line.split()[0] for line in done.stdout.splitlines()[1:] if line.split()]


class RunnerTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        self.tools = FakeTools(root / "tools")
        self.addCleanup(self.kill_hung_children)
        self.repo_root = root / "halmasuit"
        self.sha = make_repo(self.repo_root)
        self.state_root = root / "runs"
        self.nix_config = root / "nix-config"
        self.clock = FakeClock()

    def kill_hung_children(self) -> None:
        for pid in (self.tools.up_pid(), self.tools.copy_pid()):
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

    def run_runner(self, *options: str) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = lab_ec2.main([*self.args(), *options], clock=self.clock)
        return code, out.getvalue()

    def frames_calls(self) -> list[list[str]]:
        return [call for call in self.tools.ssh_calls() if call[-1].startswith("sudo -n tar ")]

    def assert_frames_copied(self, frames_dir: str) -> None:
        self.assertEqual([call[-1] for call in self.frames_calls()], [tar_command(frames_dir)])
        self.assertEqual(tree(self.run_dir() / "frames"), tree(self.tools.frames_source))

    def set_served_index(self, lines: list[dict[str, Any]]) -> None:
        write_index(self.tools.frames_source / "index.jsonl", lines)

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
            self.assertIn(
                call[-1],
                (REMOTE_COMMAND, index_command(DEFAULT_FRAMES_DIR), tar_command(DEFAULT_FRAMES_DIR)),
            )
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

    def test_pass_copies_the_default_frames_dir_into_the_run(self) -> None:
        code, out = self.run_runner()
        self.assertEqual(code, 0, out)
        self.assert_frames_copied(DEFAULT_FRAMES_DIR)
        self.assertEqual(
            sorted(path.name for path in self.run_dir().iterdir()),
            ["DP-1-greeter.png", "DP-1.nut", "frames", "journal.json", "run.log", "timeline.json"],
        )
        self.assert_box_ended()

    def test_pass_writes_each_video_and_the_greeter_png(self) -> None:
        code, out = self.run_runner()
        self.assertEqual(code, 0, out)
        run_dir = self.run_dir()
        video = run_dir / f"{OUTPUT}.nut"
        self.assertEqual(decode_video(video), b"".join(png_pixels(frame_png(seq)) for seq in range(FRAME_COUNT)))
        self.assertEqual(video_times_us(video), [Decimal(FRAME_STEP_US * seq) for seq in range(FRAME_COUNT)])
        self.assertEqual((run_dir / f"{OUTPUT}-greeter.png").read_bytes(), frame_png(GREETER_FRAME))
        self.assert_box_ended()

    def test_journal_is_read_after_the_index_snapshot(self) -> None:
        code, out = self.run_runner()
        self.assertEqual(code, 0, out)
        commands = [call[-1] for call in self.tools.ssh_calls()]
        snapshot = commands.index(index_command(DEFAULT_FRAMES_DIR))
        journal = len(commands) - 1 - commands[::-1].index(REMOTE_COMMAND)
        self.assertGreater(journal, snapshot, commands)

    def test_index_starting_after_the_first_frame_fails_the_run(self) -> None:
        self.set_served_index([index_line(seq) for seq in range(1, FRAME_COUNT)])
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn(
            "FAIL ec2-greeter: recorder index starts at 2099000 us, after the first frame_rendered from pid 1184",
            self.run_log(),
        )
        self.assert_box_ended()

    def test_seq_gap_in_the_index_fails_the_run(self) -> None:
        self.set_served_index([index_line(seq) for seq in range(FRAME_COUNT) if seq != 10])
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL ec2-greeter: boot-to-greeter frames: seq 10 missing", self.run_log())
        self.assert_box_ended()

    def test_dropped_frame_in_the_index_fails_the_run(self) -> None:
        self.set_served_index([index_line(seq, dropped=1 if seq == 20 else 0) for seq in range(FRAME_COUNT)])
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn(
            "FAIL ec2-greeter: dropped frames in boot-to-greeter window: DP-1 (1 line(s))",
            self.run_log(),
        )
        self.assert_box_ended()

    def test_malformed_index_fails_the_run_naming_the_line(self) -> None:
        lines = [json.dumps(index_line(seq)) for seq in range(FRAME_COUNT)]
        lines[2] = "not json"
        (self.tools.frames_source / "index.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL ec2-greeter: recorder index unreadable: index.jsonl line 3: not JSON", self.run_log())
        self.assert_box_ended()

    def test_missing_index_fails_the_run(self) -> None:
        (self.tools.frames_source / "index.jsonl").unlink()
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn(
            f"FAIL ec2-greeter: copy of {DEFAULT_FRAMES_DIR} failed (rc 1): "
            f"cat: {DEFAULT_FRAMES_DIR}/index.jsonl: No such file or directory",
            self.run_log(),
        )
        self.assertFalse((self.run_dir() / "frames").exists())
        self.assert_box_ended()

    def test_missing_frame_file_fails_the_run(self) -> None:
        (self.tools.frames_source / OUTPUT / f"{5:08d}.png").unlink()
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn(
            f"FAIL ec2-greeter: copy of {DEFAULT_FRAMES_DIR} failed (rc 2): "
            f"tar: {OUTPUT}/{5:08d}.png: Cannot stat: No such file or directory",
            self.run_log(),
        )
        self.assertFalse((self.run_dir() / "frames").exists())
        self.assert_box_ended()

    def test_frames_recorded_during_the_copy_stay_out_of_the_run(self) -> None:
        served = self.tools.frames_source / "index.jsonl"
        complete = json.dumps(index_line(FRAME_COUNT))
        with served.open("ab") as index:
            index.write(complete[:PARTIAL_LINE_BYTES].encode())
        pending = self.tools.root / "pending"
        steps = []
        for seq in range(FRAME_COUNT, FRAME_COUNT + DURING_COPY_FRAMES):
            png = f"{OUTPUT}/{seq:08d}.png"
            (pending / png).parent.mkdir(parents=True, exist_ok=True)
            (pending / png).write_bytes(frame_png(seq))
            line = json.dumps(index_line(seq)) + "\n"
            if seq == FRAME_COUNT:
                line = complete[PARTIAL_LINE_BYTES:] + "\n"
            steps.append({"png": png, "index": line})
        self.tools.set_frames(append={"pending": str(pending), "steps": steps})

        code, out = self.run_runner()

        self.assertEqual(code, 0, out)
        snapshot = "".join(json.dumps(index_line(seq)) + "\n" for seq in range(FRAME_COUNT)).encode()
        expected = {f"{OUTPUT}/{seq:08d}.png": frame_png(seq) for seq in range(FRAME_COUNT)}
        expected["index.jsonl"] = snapshot
        self.assertEqual(tree(self.run_dir() / "frames"), expected)
        self.assertGreater(served.read_bytes().count(b"\n"), FRAME_COUNT)
        self.assertEqual([call[-1] for call in self.frames_calls()], [tar_command(DEFAULT_FRAMES_DIR)])
        self.assert_box_ended()

    def test_failing_ffmpeg_fails_the_run(self) -> None:
        code, out = self.run_runner("--ffmpeg", "false")
        self.assertEqual(code, 1, out)
        self.assertIn(f"FAIL ec2-greeter: encode {OUTPUT} failed (rc 1)", self.run_log())
        self.assert_box_ended()

    def test_missing_ffmpeg_fails_the_run(self) -> None:
        code, out = self.run_runner("--ffmpeg", str(self.tools.root / "no-such-ffmpeg"))
        self.assertEqual(code, 1, out)
        self.assertIn(f"FAIL ec2-greeter: encode {OUTPUT} failed (rc 127)", self.run_log())
        self.assert_box_ended()

    def test_ffprobe_count_different_from_the_index_fails_the_run(self) -> None:
        install(self.tools.root / "ffprobe-63", "print(63)\n")
        code, out = self.run_runner("--ffprobe", str(self.tools.root / "ffprobe-63"))
        self.assertEqual(code, 1, out)
        self.assertIn(
            f"FAIL ec2-greeter: video frame count differs from index: {OUTPUT} has 63 video frame(s) "
            f"for {FRAME_COUNT} index line(s)",
            self.run_log(),
        )
        self.assert_box_ended()

    def test_frames_dir_option_names_the_copied_directory(self) -> None:
        self.tools.set_frames(dir="/srv/recorder/frames")
        code, out = self.run_runner("--frames-dir", "/srv/recorder/frames")
        self.assertEqual(code, 0, out)
        self.assert_frames_copied("/srv/recorder/frames")
        self.assert_box_ended()

    def test_failing_frames_copy_fails_the_run_and_ends_the_box(self) -> None:
        self.tools.set_frames(mode="fail")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn(
            f"copy of {DEFAULT_FRAMES_DIR} failed (rc 255): ssh: connection reset during frames copy",
            self.run_log(),
        )
        self.assertFalse((self.run_dir() / "frames").exists())
        self.assert_box_ended()

    def test_frames_member_outside_the_directory_fails_the_run(self) -> None:
        self.tools.set_frames(mode="escape")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        self.assertIn("extract of /run/halmasuit-lab/frames failed", self.run_log())
        self.assertFalse((self.run_dir() / "escape.txt").exists())
        self.assert_box_ended()

    def test_frames_copy_outlasting_the_timeout_fails_and_ends_the_box(self) -> None:
        self.tools.set_frames(mode="hang")
        code, out = self.run_runner("--copy-timeout", "2")
        self.assertEqual(code, 1, out)
        self.assertIn(f"copy of {DEFAULT_FRAMES_DIR} timed out after 2 seconds", self.run_log())
        pid = self.tools.copy_pid()
        self.assertIsNotNone(pid)
        self.assertFalse(process_alive(pid))
        self.assert_box_ended()

    def test_failing_frames_copy_after_a_hold_timeout_keeps_the_hold_reason(self) -> None:
        self.tools.serve_journal(None)
        self.tools.set_frames(mode="fail")
        code, out = self.run_runner()
        self.assertEqual(code, 1, out)
        log = self.run_log()
        self.assertIn("FAIL ec2-greeter: no halmasuit.service foreground_changed to greeter event", log)
        self.assertIn("frames not copied", log)
        self.assert_box_ended()

    def test_sigterm_during_the_frames_copy_ends_the_box(self) -> None:
        self.tools.set_frames(mode="hang")
        runner = subprocess.Popen(
            [sys.executable, "-B", str(RUNNER), *self.args()],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            pid = wait_for(self.tools.copy_pid, "the hung frames copy to start")
            runner.send_signal(signal.SIGTERM)
            out, err = runner.communicate(timeout=60)
        finally:
            if runner.poll() is None:
                runner.kill()
                runner.communicate()
        self.assertEqual(runner.returncode, 128 + signal.SIGTERM, out + err)
        self.assertFalse(process_alive(pid))
        self.assert_box_ended()

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
