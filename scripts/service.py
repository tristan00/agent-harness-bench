#!/usr/bin/env python3
"""Build, start and stop the benchmark service: setup | start | stop."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
WORKSPACE = ROOT.parent
EXE = ".exe" if os.name == "nt" else ""
DEFAULT_DATA = WORKSPACE / "runtime" / "benchmark-service"
BUILD = WORKSPACE / "runtime" / "build"


def executable_path(pid):
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes
        handle = ctypes.windll.kernel32.OpenProcess(0x1000, False, pid)
        if not handle:
            return None
        try:
            size = wintypes.DWORD(32768)
            buffer = ctypes.create_unicode_buffer(size.value)
            ok = ctypes.windll.kernel32.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size))
            return buffer.value if ok else None
        finally:
            ctypes.windll.kernel32.CloseHandle(handle)
    try:
        return os.readlink(f"/proc/{pid}/exe")
    except OSError:
        return None


def same(a, b):
    return a is not None and os.path.normcase(os.path.realpath(a)) == os.path.normcase(os.path.realpath(b))


def running(data):
    pid_file, binary_file = data / "service.pid", data / "service-binary"
    if not pid_file.exists() or not binary_file.exists():
        return None
    pid = int(pid_file.read_text().strip())
    return pid if same(executable_path(pid), binary_file.read_text().strip()) else None


def cargo(*args, target, cwd=ROOT):
    command = [shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / f"cargo{EXE}"), *args]
    subprocess.run(command, cwd=cwd, env={**os.environ, "CARGO_TARGET_DIR": str(target)}, check=True)


def start(data, bind, binary):
    data.mkdir(parents=True, exist_ok=True)
    if running(data):
        raise SystemExit("Benchmark service is already running. Leave it running until the active batch finishes.")
    marker = data / "next-service-binary"
    staged = marker.read_text().strip() if marker.exists() else ""
    binary = Path(binary or staged or ROOT / "target" / "release" / f"harness-bench{EXE}").resolve()
    if not binary.is_file():
        raise SystemExit(f"Missing executable: {binary}")
    flags = {"creationflags": 0x00000008 | 0x00000200 | 0x08000000} if os.name == "nt" else {"start_new_session": True}
    with (data / "stdout.log").open("ab") as out, (data / "stderr.log").open("ab") as err:
        process = subprocess.Popen([str(binary), "serve", "--bind", bind, "--data", str(data)], cwd=ROOT, stdin=subprocess.DEVNULL, stdout=out, stderr=err, **flags)
    (data / "service.pid").write_text(str(process.pid))
    (data / "service-binary").write_text(str(binary))
    deadline = time.monotonic() + 15
    while True:
        if process.poll() is not None:
            raise SystemExit((data / "stderr.log").read_text(errors="replace")[-4000:])
        try:
            with urllib.request.urlopen(f"http://{bind}/health", timeout=1) as response:
                json.load(response)
            break
        except OSError:
            if time.monotonic() > deadline:
                raise SystemExit("Benchmark service did not become healthy")
            time.sleep(0.2)
    if staged and same(str(binary), staged):
        marker.unlink()
    print(f"Evaluator running at http://{bind} (PID {process.pid})")
    print(f"Admin token: {data / 'admin-token'}")
    print(f"Judge token: {data / 'judge-token'}")


def stop(data):
    pid_file = data / "service.pid"
    if not pid_file.exists():
        raise SystemExit(f"No service PID file at {pid_file}")
    pid = int(pid_file.read_text().strip())
    if executable_path(pid) is not None:
        if not running(data):
            raise SystemExit("PID belongs to another executable; refusing to stop it.")
        if os.name == "nt":
            subprocess.run(["taskkill", "/PID", str(pid), "/F"], check=True, capture_output=True)
        else:
            os.kill(pid, 15)
        while executable_path(pid) is not None:
            time.sleep(0.1)
    pid_file.unlink()
    print("Evaluator stopped. Database and submissions are retained.")


def setup(data, bind, coordinator_source, skip_build):
    benchmark = BUILD / "benchmark" / "release" / f"harness-bench{EXE}"
    if not skip_build:
        cargo("build", "--release", "--locked", "--manifest-path", str(coordinator_source / "Cargo.toml"), target=BUILD / "coordinator", cwd=coordinator_source)
        cargo("build", "--release", "--locked", target=BUILD / "benchmark")
    data.mkdir(parents=True, exist_ok=True)
    profiles = data / "coordinator-profiles.json"
    if not profiles.exists():
        profile = {"coordinator_url": "http://127.0.0.1:4242", "workspace": {},
                   "session_command": [sys.executable, str(ROOT / "scripts" / "template_session.py")],
                   "session_planner_command": [sys.executable, str(ROOT / "scripts" / "template_planner.py")]}
        profiles.write_text(json.dumps({"default": profile}, indent=2), encoding="utf-8")
    start(data, bind, benchmark)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["setup", "start", "stop"])
    parser.add_argument("--data", type=Path, default=DEFAULT_DATA)
    parser.add_argument("--bind", default="127.0.0.1:8741")
    parser.add_argument("--binary", help="start: executable to run instead of the staged or default build")
    parser.add_argument("--coordinator-source", type=Path, default=WORKSPACE / "agent_coordinator")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()
    data = args.data.resolve()
    if args.action == "setup":
        source = args.coordinator_source.resolve()
        if data == source or source in data.parents:
            raise SystemExit("Benchmark data must be outside coordinator source")
        setup(data, args.bind, source, args.skip_build)
    elif args.action == "start":
        start(data, args.bind, args.binary)
    else:
        stop(data)


if __name__ == "__main__":
    main()
