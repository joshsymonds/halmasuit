#!/usr/bin/env python3
"""Launches one halmasuit-lab EC2 box from this checkout's HEAD and checks its greeter boot.

`ec2 up` builds the checkout's HEAD into a new box and prints `<name> ready` as its last stdout
line. The runner holds the box's boot journal until halmasuit.service's greeter event is 30
seconds old in it, copies the journal into the run directory as journal.json, copies the box's
recorder directory into the run directory's frames/ over ssh, and checks the journal against the
ec2-greeter profile in timeline.py. The frames copy is bounded by --copy-timeout seconds. The
recorder's index is checked with timeline.recorder_failures against each output's video, which
--ffmpeg encodes losslessly as `<output>.nut` at the frames' mono_us timing and --ffprobe counts.
Each output's greeter frame is copied to `<output>-greeter.png`. Each run writes its record to a
directory under --state-root named `<stamp>-ec2-greeter`: run.log holds the ec2 calls with their
results, the `ec2 up` output, and PASS or each `FAIL ec2-greeter: <message>` line.

A run refuses, launching nothing, while a halmasuit-lab box is already listed. Every box a run
launches is ended before the run returns, on pass, fail, an exception, or SIGINT, SIGTERM, or
SIGHUP. The cleanup stops a still-running `ec2 up`, runs `ec2 down` on the named box (or, when
`ec2 up` printed no name, on each halmasuit-lab box listed now, since the refusal left none
listed before the launch), then lists again and fails, naming each box, if one is still listed.
Signals that arrive during the cleanup are ignored. Exit 0 on PASS, 1 on any failure or refusal,
128 plus the signal number after a signal.

Every ec2 and ssh call goes through the programs named by --ec2 and --ssh, and the hold polls on
the clock handed to main, so tests run the whole sequence against fake executables.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Sequence
from pathlib import Path
from typing import IO, Any

import lab
import timeline
from lab import LabError

PROFILE = "ec2-greeter"
CLASS = "halmasuit-lab"
BOX_PREFIX = f"{CLASS}-"
READY = re.compile(r"(halmasuit-lab-\S+) ready")
BOX_USER = "joshsymonds"
CONNECT_TIMEOUT_SECONDS = 10
JOURNAL_COMMAND = ("sudo", "-n", "journalctl", "-b", "0", "-o", "json", "--all", "--no-pager")
DEFAULT_FRAMES_DIR = "/run/halmasuit-lab/frames"
FRAMES_NAME = "frames"
COPY_TIMEOUT_SECONDS = 600
VIDEO_TIMEOUT_SECONDS = 600
VIDEO_SUFFIX = ".nut"
GREETER_SUFFIX = "-greeter.png"
CONCAT_FRAMERATE = 1_000_000
SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


class _Signalled(Exception):
    """A SIGINT, SIGTERM, or SIGHUP arrived; raised into the run so its cleanup runs."""

    def __init__(self, signum: int) -> None:
        super().__init__(signum)
        self.signum = signum


class _Interrupts:
    """Turns the first SIGINT, SIGTERM, or SIGHUP into one _Signalled and ignores every later one.

    The flag is set before any disposition changes, so a signal that lands while the handler or
    the cleanup is changing dispositions is ignored too. An ignored disposition survives exec, so
    the cleanup's ec2 and ssh children ignore these signals as well.
    """

    def __init__(self) -> None:
        self.cleaning = False

    def handle(self, signum: int, _frame: object) -> None:
        if self.cleaning:
            return
        self.begin_cleanup()
        raise _Signalled(signum)

    def begin_cleanup(self) -> None:
        self.cleaning = True
        for signum in SIGNALS:
            signal.signal(signum, signal.SIG_IGN)


class _Launch:
    """What the run has started, so the cleanup knows which processes and boxes are its own."""

    def __init__(self) -> None:
        self.attempted = False
        self.name: str | None = None
        self.process: subprocess.Popen[str] | None = None
        self.stderr: IO[str] | None = None


def _program(argv: Sequence[str], timeout: float) -> lab.Result:
    """Runs a program; one that cannot start is a failed call, not an exception."""
    try:
        return lab._capture(list(argv), timeout)
    except OSError as err:
        return lab.Result(127, "", str(err))


def _hold_reason(greeter: int | None, newest: int | None, problem: str) -> str:
    if greeter is None:
        reason = (
            f"no halmasuit.service foreground_changed to greeter event in the boot journal "
            f"within {lab.HOLD_DEADLINE_SECONDS} seconds"
        )
    elif newest is None:
        reason = "the boot journal's newest entry was not readable after the greeter event"
    else:
        past = (newest - greeter) / lab.MICROSECONDS_PER_SECOND
        reason = (
            f"the boot journal's newest entry was {past:.1f} s past the greeter event at the "
            f"{lab.HOLD_DEADLINE_SECONDS} second deadline; {lab.HOLD_AFTER_GREETER_SECONDS} s needed"
        )
    if problem:
        reason += f"; last read problem: {problem}"
    return reason


class Ec2Run:
    def __init__(
        self,
        *,
        ec2: str,
        ssh: str,
        clock: lab.Clock,
        repo_root: Path,
        nix_config: Path,
        state_root: Path,
        known_hosts: Path,
        frames_dir: str,
        copy_timeout: int,
        ffmpeg: str,
        ffprobe: str,
        interrupts: _Interrupts,
    ) -> None:
        self.ec2 = ec2
        self.ssh = ssh
        self.clock = clock
        self.repo_root = repo_root
        self.nix_config = nix_config
        self.state_root = state_root
        self.known_hosts = known_hosts
        self.frames_dir = frames_dir
        self.copy_timeout = copy_timeout
        self.ffmpeg = ffmpeg
        self.ffprobe = ffprobe
        self.interrupts = interrupts

    def run(self) -> int:
        run = lab.RunLog(self.state_root, self.clock.stamp(), PROFILE, print)
        launch = _Launch()
        code = 1
        try:
            code = self._launch_and_check(run, launch)
        except _Signalled as received:
            code = 128 + received.signum
            run.say(f"{signal.Signals(received.signum).name} received; ending the launched boxes")
        except LabError as err:
            run.say(f"FAIL {PROFILE}: {err}")
        finally:
            # A first signal can still land as the cleanup begins, before the cleaning flag is set;
            # its handler raises here, and the cleanup goes on under the signal's exit code. Nothing
            # but the code is set before _end, so no failing log write can skip the cleanup.
            late: int | None = None
            try:
                self.interrupts.begin_cleanup()
            except _Signalled as received:
                late = received.signum
                code = 128 + late
            problems = self._end(run, launch)
            if late is not None:
                run.say(f"{signal.Signals(late).name} received as the cleanup began")
            for problem in problems:
                run.say(f"FAIL {PROFILE}: {problem}")
            if problems and code == 0:
                code = 1
        return code

    def _launch_and_check(self, run: lab.RunLog, launch: _Launch) -> int:
        head = self._head()
        run.say(f"ec2-greeter run from halmasuit {head}")
        listed = self._listed(run)
        if listed:
            raise LabError(f"refused: {', '.join(listed)} already listed; nothing launched")
        name = self._up(run, launch, head)
        reason = self._hold(name)
        try:
            self._collect_journal(run, name)
        except LabError as err:
            if reason is None:
                raise
            run.say(f"journal: {err}; journal.json not written")
        try:
            self._collect_frames(run, name)
        except LabError as err:
            if reason is None:
                raise
            run.say(f"frames: {err}; frames not copied")
        if reason is not None:
            raise LabError(reason)
        return self._check(run)

    def _head(self) -> str:
        result = _program(["git", "-C", str(self.repo_root), "rev-parse", "HEAD"], lab.REMOTE_TIMEOUT_SECONDS)
        return lab._ok(result, "resolve HEAD").strip()

    def _record(self, run: lab.RunLog, label: str, result: lab.Result) -> None:
        run.say(f"{label}: rc {result.returncode}")
        for stream in (result.stdout, result.stderr):
            for line in stream.splitlines():
                run.say(f"  {line}")

    def _listed(self, run: lab.RunLog) -> list[str]:
        result = _program([self.ec2, "ls"], lab.REMOTE_TIMEOUT_SECONDS)
        self._record(run, "ec2 ls", result)
        if result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip() or "no output"
            raise LabError(f"ec2 ls failed (rc {result.returncode}): {detail}")
        lines = result.stdout.splitlines()
        if not lines or lines[0].split()[:1] != ["NAME"]:
            raise LabError("ec2 ls printed no NAME header")
        names = []
        for line in lines[1:]:
            fields = line.split()
            if fields and fields[0].startswith(BOX_PREFIX):
                names.append(fields[0])
        return names

    def _up(self, run: lab.RunLog, launch: _Launch, head: str) -> str:
        argv = [self.ec2, "up", CLASS, "--override-input", "halmasuit", f"git+file://{self.repo_root}?rev={head}"]
        env = {**os.environ, "EC2_FLEET_FLAKE": str(self.nix_config)}
        run.say(f"launching: {shlex.join(argv)}")
        stderr = tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace")
        launch.attempted = True
        launch.stderr = stderr
        # The mask keeps a signal from landing between fork and the process being recorded, where
        # the cleanup could not see the child. The child restores the mask before exec.
        blocked = signal.pthread_sigmask(signal.SIG_BLOCK, SIGNALS)
        try:
            launch.process = subprocess.Popen(
                argv,
                stdout=subprocess.PIPE,
                stderr=stderr,
                text=True,
                env=env,
                preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, blocked),
            )
        except OSError as err:
            stderr.close()
            launch.stderr = None
            raise LabError(f"cannot start ec2 up: {err}") from err
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, blocked)
        process = launch.process
        last: str | None = None
        for raw in process.stdout:
            text = raw.rstrip("\r\n")
            run.say(f"ec2 up: {text}")
            last = text
        rc, detail = self._reap_up(run, launch)
        if rc != 0:
            raise LabError(f"ec2 up failed (rc {rc}): {detail or 'no output'}")
        match = READY.fullmatch(last or "")
        if match is None:
            raise LabError("ec2 up exited 0 without a `halmasuit-lab-<x> ready` last line")
        launch.name = match.group(1)
        return launch.name

    def _reap_up(self, run: lab.RunLog, launch: _Launch) -> tuple[int, str]:
        """Waits for the ec2 up process, logs its stderr, and returns its code and stderr text."""
        process = launch.process
        if process is None or launch.stderr is None:
            raise LabError("internal error: no ec2 up process to reap")
        rc = process.wait()
        launch.process = None
        process.stdout.close()
        launch.stderr.seek(0)
        text = launch.stderr.read()
        launch.stderr.close()
        launch.stderr = None
        for line in text.splitlines():
            run.say(f"ec2 up stderr: {line}")
        return rc, text.strip()

    def _stop_up(self, run: lab.RunLog, launch: _Launch) -> None:
        process = launch.process
        if process is None:
            return
        if process.poll() is None:
            run.say("stopping ec2 up with SIGTERM")
            process.terminate()
        self._reap_up(run, launch)

    def _ssh_argv(self, name: str, remote: Sequence[str]) -> list[str]:
        return [
            self.ssh,
            "-o",
            "BatchMode=yes",
            "-o",
            f"ConnectTimeout={CONNECT_TIMEOUT_SECONDS}",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            f"UserKnownHostsFile={self.known_hosts}",
            f"{BOX_USER}@{name}",
            shlex.join(remote),
        ]

    def _ssh(self, name: str, remote: Sequence[str]) -> lab.Result:
        return _program(self._ssh_argv(name, remote), lab.REMOTE_TIMEOUT_SECONDS)

    def _journal(self, name: str, what: str) -> str:
        return lab._ok(self._ssh(name, JOURNAL_COMMAND), what)

    def _hold(self, name: str) -> str | None:
        """Hold until the greeter event is 30 s old in the boot journal; None once held, else why not.

        Each query that fails or prints unreadable output counts as not yet, and the last such
        problem is part of the reason.
        """
        greeter: int | None = None
        newest: int | None = None
        problem = ""

        def held() -> bool:
            nonlocal greeter, newest, problem
            newest = None
            try:
                records = lab._journal_records(self._journal(name, "read the boot journal"))
                if greeter is None:
                    greeter = next((lab._monotonic_us(r) for r in records if lab._is_greeter_event(r)), None)
                if records:
                    newest = lab._monotonic_us(records[-1])
            except LabError as err:
                problem = str(err)
                return False
            return (
                greeter is not None
                and newest is not None
                and newest - greeter >= lab.HOLD_AFTER_GREETER_SECONDS * lab.MICROSECONDS_PER_SECOND
            )

        started = self.clock.monotonic()
        while not held():
            if self.clock.monotonic() - started >= lab.HOLD_DEADLINE_SECONDS:
                return _hold_reason(greeter, newest, problem)
            self.clock.sleep(lab.POLL_SECONDS)
        return None

    def _collect_journal(self, run: lab.RunLog, name: str) -> None:
        text = self._journal(name, "collect the boot journal")
        (run.directory / timeline.JOURNAL_NAME).write_text(text, encoding="utf-8")

    def _collect_frames(self, run: lab.RunLog, name: str) -> None:
        """Extracts the box's recorder directory into the run directory's frames/, or raises.

        The tar stream is binary, so it goes to an unnamed temporary file in the run directory,
        which is gone once closed, even when the run is signalled mid-copy. subprocess.run kills
        the ssh child on the timeout and on any exception escaping it.
        """
        remote = ("sudo", "-n", "tar", "-C", self.frames_dir, "-cf", "-", ".")
        run.say(f"frames copy: {shlex.join(remote)}")
        with tempfile.TemporaryFile(dir=run.directory) as archive:
            try:
                done = subprocess.run(
                    self._ssh_argv(name, remote),
                    stdout=archive,
                    stderr=subprocess.PIPE,
                    timeout=self.copy_timeout,
                    check=False,
                )
            except subprocess.TimeoutExpired as err:
                raise LabError(f"copy of {self.frames_dir} timed out after {self.copy_timeout} seconds") from err
            except OSError as err:
                raise LabError(f"cannot start ssh for the frames copy: {err}") from err
            stderr = done.stderr.decode("utf-8", errors="replace")
            self._record(run, "frames copy", lab.Result(done.returncode, "", stderr))
            if done.returncode != 0:
                detail = stderr.strip() or "no output"
                raise LabError(f"copy of {self.frames_dir} failed (rc {done.returncode}): {detail}")
            archive.seek(0)
            try:
                (run.directory / FRAMES_NAME).mkdir()
                with tarfile.open(fileobj=archive, mode="r:") as tar:
                    tar.extractall(run.directory / FRAMES_NAME, filter="data")
            except (tarfile.TarError, OSError) as err:
                raise LabError(f"extract of {self.frames_dir} failed: {err}") from err

    def _check(self, run: lab.RunLog) -> int:
        try:
            document = timeline.build_timeline(run.directory)
        except (KeyError, OSError, ValueError) as exc:
            raise LabError(f"malformed run: {exc!r}") from exc
        frames = self._recorder_index(run)
        video_frames = self._encode(run, frames)
        self._greeter_pngs(run, document, frames)
        (run.directory / timeline.TIMELINE_NAME).write_text(json.dumps(document, indent=2) + "\n")
        failures = timeline.recorder_failures(document, frames, video_frames)
        failures += timeline.PROFILES[PROFILE](document)
        for failure in failures:
            run.say(f"FAIL {PROFILE}: {failure}")
        if failures:
            return 1
        run.say(f"PASS {PROFILE}")
        return 0

    def _recorder_index(self, run: lab.RunLog) -> list[dict[str, Any]]:
        try:
            return timeline.read_recorder_index(run.directory / FRAMES_NAME)
        except (OSError, ValueError) as err:
            raise LabError(f"recorder index unreadable: {err}") from err

    def _encode(self, run: lab.RunLog, frames: list[dict[str, Any]]) -> dict[str, int]:
        """Writes one lossless video per output; returns each video's frame count as ffprobe reads it."""
        outputs: dict[str, list[dict[str, Any]]] = {}
        for frame in frames:
            outputs.setdefault(frame["output"], []).append(frame)
        counts: dict[str, int] = {}
        for output, lines in sorted(outputs.items()):
            lines.sort(key=lambda frame: frame["mono_us"])
            video = run.directory / f"{output}{VIDEO_SUFFIX}"
            self._write_video(run, output, lines, video)
            counts[output] = self._count_frames(run, video)
        return counts

    def _write_video(self, run: lab.RunLog, output: str, lines: list[dict[str, Any]], video: Path) -> None:
        frames_dir = (run.directory / FRAMES_NAME).resolve()
        paths = [_frame_path(frames_dir, frame) for frame in lines]
        for path in paths:
            if not path.is_file():
                raise LabError(f"frame file missing for {output}: {path}")
        with tempfile.TemporaryDirectory(dir=run.directory) as scratch:
            listing = Path(scratch) / "frames.ffconcat"
            listing.write_text(_concat_list(lines, paths), encoding="utf-8")
            result = _program(
                [
                    self.ffmpeg,
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-n",
                    "-f",
                    "concat",
                    "-safe",
                    "0",
                    "-i",
                    str(listing),
                    "-fps_mode",
                    "passthrough",
                    "-c:v",
                    "ffv1",
                    str(video),
                ],
                VIDEO_TIMEOUT_SECONDS,
            )
        self._record(run, f"encode {output}", result)
        lab._ok(result, f"encode {output}")

    def _count_frames(self, run: lab.RunLog, video: Path) -> int:
        result = _program(
            [
                self.ffprobe,
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "default=nw=1:nk=1",
                str(video),
            ],
            VIDEO_TIMEOUT_SECONDS,
        )
        self._record(run, f"count {video.name}", result)
        text = lab._ok(result, f"count frames of {video.name}").strip()
        try:
            return int(text)
        except ValueError as err:
            raise LabError(f"count frames of {video.name} printed {text!r}, not a number") from err

    def _greeter_pngs(self, run: lab.RunLog, document: timeline.Timeline, frames: list[dict[str, Any]]) -> None:
        frames_dir = (run.directory / FRAMES_NAME).resolve()
        for output, frame in sorted(timeline.greeter_frames(document, frames).items()):
            target = run.directory / f"{output}{GREETER_SUFFIX}"
            try:
                shutil.copyfile(_frame_path(frames_dir, frame), target)
            except OSError as err:
                raise LabError(f"copy greeter frame of {output} failed: {err}") from err

    def _end(self, run: lab.RunLog, launch: _Launch) -> list[str]:
        """Ends the boxes this run launched; returns one problem per box or check that failed.

        Each step is isolated: a failure in one is recorded and the remaining steps still run.
        """
        if not launch.attempted:
            return []
        problems: list[str] = []
        try:
            self._stop_up(run, launch)
        except Exception as err:
            problems.append(f"could not stop ec2 up: {err}; terminate any {CLASS} box by hand")
        if launch.name is not None:
            targets = [launch.name]
        else:
            try:
                targets = self._listed(run)
            except Exception as err:
                problems.append(f"could not list boxes to end: {err}; terminate any {CLASS} box by hand")
                targets = []
        for name in targets:
            problem = self._down(run, name)
            if problem is not None:
                problems.append(problem)
        try:
            remaining = self._listed(run)
        except Exception as err:
            problems.append(f"could not list boxes after ending the run: {err}; check ec2 ls by hand")
        else:
            problems.extend(f"{name} is still listed; terminate it by hand" for name in remaining)
        return problems

    def _down(self, run: lab.RunLog, name: str) -> str | None:
        """Runs ec2 down on one box; returns the problem to record, or None when it succeeded."""
        try:
            result = _program([self.ec2, "down", name], lab.REMOTE_TIMEOUT_SECONDS)
            self._record(run, f"ec2 down {name}", result)
        except Exception as err:
            return f"could not terminate {name}: {err}; terminate it by hand"
        if result.returncode == 0:
            return None
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        return f"could not terminate {name} (rc {result.returncode}): {detail}; terminate it by hand"


def _frame_path(frames_dir: Path, frame: dict[str, Any]) -> Path:
    return frames_dir / frame["output"] / f"{frame['seq']:08d}.png"


def _ffconcat_quote(path: str) -> str:
    return "'" + path.replace("'", "'\\''") + "'"


def _concat_list(lines: list[dict[str, Any]], paths: list[Path]) -> str:
    """An ffconcat list that shows each frame for the microseconds until the next frame's mono_us."""
    text = ["ffconcat version 1.0"]
    for index, (frame, path) in enumerate(zip(lines, paths, strict=True)):
        text.append(f"file {_ffconcat_quote(str(path))}")
        text.append(f"option framerate {CONCAT_FRAMERATE}")
        if index + 1 < len(lines):
            text.append(f"duration {lines[index + 1]['mono_us'] - frame['mono_us']}us")
    return "\n".join(text) + "\n"


def _known_hosts() -> Path:
    state = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
    return Path(state) / "ec2-fleet" / "known_hosts"


def main(argv: Sequence[str] | None = None, *, clock: lab.Clock | None = None) -> int:
    parser = argparse.ArgumentParser(description=f"Launch one {CLASS} EC2 box, check its greeter boot, end the box.")
    parser.add_argument("--ec2", default="ec2", help="ec2 CLI to run")
    parser.add_argument("--ssh", default="ssh", help="ssh client to run")
    parser.add_argument(
        "--nix-config",
        type=Path,
        default=Path.home() / "nix-config",
        help="nix-config checkout whose flake ec2 up builds",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=lab.REPO_ROOT,
        help="halmasuit checkout whose HEAD is built into the box",
    )
    parser.add_argument(
        "--state-root",
        type=Path,
        default=Path.home() / ".local" / "state" / "halmasuit-lab" / "runs",
        help="directory for per-run logs",
    )
    parser.add_argument(
        "--frames-dir",
        default=DEFAULT_FRAMES_DIR,
        help="recorder directory on the box",
    )
    parser.add_argument(
        "--copy-timeout",
        type=int,
        default=COPY_TIMEOUT_SECONDS,
        help="seconds the frames copy from the box may take",
    )
    parser.add_argument("--ffmpeg", default="ffmpeg", help="ffmpeg to encode each output's video")
    parser.add_argument("--ffprobe", default="ffprobe", help="ffprobe to count each video's frames")
    options = parser.parse_args(argv)
    interrupts = _Interrupts()
    runner = Ec2Run(
        ec2=options.ec2,
        ssh=options.ssh,
        clock=clock or lab.SystemClock(),
        repo_root=options.repo_root,
        nix_config=options.nix_config,
        state_root=options.state_root,
        known_hosts=_known_hosts(),
        frames_dir=options.frames_dir,
        copy_timeout=options.copy_timeout,
        ffmpeg=options.ffmpeg,
        ffprobe=options.ffprobe,
        interrupts=interrupts,
    )
    previous: dict[int, object] = {}
    try:
        for signum in SIGNALS:
            previous[signum] = signal.signal(signum, interrupts.handle)
        return runner.run()
    except _Signalled as received:
        return 128 + received.signum
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, signal.SIG_DFL if handler is None else handler)


if __name__ == "__main__":
    sys.exit(main())
