#!/usr/bin/env python3
"""Run under heavy: real desktop Core plus the disposable pneu-test Android client.

Only desktop OS notification effects are mocked. TLS, HELLO, resume persistence,
READY offsets, Room/spools, battery, ring and notification mirroring are real.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import time
from types import SimpleNamespace
import run_device_tests as device


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serial', default='emulator-5554')
    parser.add_argument('--scratch', type=Path, required=True)
    args = parser.parse_args()
    if not args.serial.startswith('emulator-'):
        parser.error('A disposable emulator is required')
    base = args.scratch.resolve()
    base.mkdir(parents=True, exist_ok=True, mode=0o700)
    ready = base / 'ready.json'
    ready.unlink(missing_ok=True)
    root = Path(__file__).resolve().parents[2]
    subprocess.run([str(root / 'android/gradlew'), '-p', str(root / 'android'), '--no-daemon',
                    ':app:lintDebug', ':app:assembleDebugAndroidTest'], check=True, timeout=600)
    env = dict(os.environ, PNEU_ANDROID_INTEROP_READY=str(ready), XDG_CACHE_HOME=str(base / 'rust-cache'))
    child = None
    with (base / 'desktop.log').open('w') as log:
        try:
            child = subprocess.Popen(['cargo', 'test', '-p', 'pneud', '--test', 'integration',
                                      'android_interop_cases::real_android_nov2_extensions', '--',
                                      '--ignored', '--exact', '--nocapture'], cwd=root / 'desktop', env=env,
                                     stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + 600
            while not ready.exists():
                if child.poll() is not None:
                    raise RuntimeError(f'Desktop interop host exited {child.returncode}; see desktop.log')
                if time.monotonic() > deadline:
                    raise TimeoutError('Desktop interop readiness')
                time.sleep(.1)
            invitation = json.loads(ready.read_text())
            device.args = SimpleNamespace(serial=args.serial)
            device.adb = [str(Path(os.environ['ANDROID_HOME']) / 'platform-tools/adb'), '-s', args.serial]
            device.scratch = base
            device.current_suite = 'nov2-real-interop'
            selector = 'Nov2InteropDeviceTest#resumeBatteryRingAndNotificationsCrossRealImplementations'
            device.current_case = SimpleNamespace(tests=(selector,), executed=set())
            with device.emulator_commands(args.serial):
                avd = device.run([*device.adb, 'emu', 'avd', 'name'], capture_output=True, text=True).stdout
                if avd.splitlines()[0].strip() != 'pneu-test':
                    raise RuntimeError('Only pneu-test is permitted; never use another task AVD')
                device.instrument('android', selector, ['-e', 'pcUri', invitation['uri'],
                                  '-e', 'controlPort', str(invitation['controlPort'])], 210)
            code = child.wait(timeout=30)
            if code:
                raise RuntimeError(f'Desktop interop assertions failed: {code}')
            evidence = json.loads((base / 'evidence.json').read_text())
            print(json.dumps(evidence, indent=2), flush=True)
        except BaseException:
            if ready.exists():
                try:
                    port = json.loads(ready.read_text())["controlPort"]
                    with socket.create_connection(("127.0.0.1", port), timeout=2) as client:
                        client.sendall(b'{"action":"status"}\n')
                        snapshot = json.loads(client.makefile('rb').readline())
                        for field in ('pairing_completion', 'pairing_clipboard'):
                            snapshot.get('snapshot', {}).pop(field, None)
                        (base / 'failure-snapshot.json').write_text(json.dumps(snapshot, indent=2))
                except (OSError, ValueError):
                    pass
            raise
        finally:
            if child is not None and child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=10)
            ready.unlink(missing_ok=True)
            shutil.rmtree(base / 'rust-cache', ignore_errors=True)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
