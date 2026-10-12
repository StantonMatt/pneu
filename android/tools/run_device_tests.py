#!/usr/bin/env python3
"""Run named Android suites under one outer heavy admission (build APKs first).

Usage: heavy python3 tools/run_device_tests.py [suite ...]
No arguments runs all suites, including the real daemon. Fixtures, invitations,
installs and finite device deadlines all start inside the admitted process.
Evidence stays in scratch; private keys, listeners and child processes do not.
"""
import argparse
import base64
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import socket
import sqlite3
import subprocess
import threading
import time
import traceback
import urllib.parse
import xml.etree.ElementTree as ET

root = Path(__file__).resolve().parents[1]
package = 'io.github.stantonmatt.pneu'
runner = package + '.test/androidx.test.runner.AndroidJUnitRunner'
permissions = ['ACCESS_LOCAL_NETWORK', 'CAMERA', 'POST_NOTIFICATIONS', 'READ_MEDIA_IMAGES', 'READ_MEDIA_VIDEO']
children, logs, keys, sinks = [], [], [], []
results = []
identity_hashes = set()
current_case = None
IDENTITY_EXPORT = 'TlsDeviceTest#exportPublicClientLeaf'


class DeviceCase:
    """One independently recoverable fixture with the selectors it actually runs."""
    def __init__(self, name, tests, execute, identity=False):
        self.name = name
        self.primary_tests = (tests,) if isinstance(tests, str) else tuple(tests)
        self.tests = self.primary_tests + ((IDENTITY_EXPORT,) if identity else ())
        self.execute = execute

    def run(self):
        self.execute(*self.primary_tests)



def run(command, **kwargs):
    return subprocess.run(command, check=True, timeout=kwargs.pop('timeout', 60), **kwargs)


def install():
    for path in ['apk/debug/app-debug.apk', 'apk/androidTest/debug/app-debug-androidTest.apk']:
        run([*adb, 'install', '-r', str(root / 'app/build/outputs' / path)], stdout=subprocess.DEVNULL)


def instrument(name, test, extras=(), seconds=150):
    # Selectors come from the executable case table used by the coverage test.
    assert current_case is not None and test in current_case.tests, (name, test)
    # The entire Python process already holds the slot. Never nest scheduler jobs.
    for attempt in range(2):
        install()
        command = shlex.join(['timeout', str(seconds), 'am', 'instrument', '-w',
                              '-e', 'class', package + '.' + test, *extras, runner])
        result = subprocess.run([*adb, 'shell', command], timeout=seconds + 30,
                                capture_output=True, text=True)
        output = result.stdout + result.stderr
        (scratch / (name + '.log')).write_text(output)
        print(output, flush=True)
        match = re.search(r'OK \((\d+) tests?\)', output)
        count = int(match[1]) if match else 0
        expected = 1 if '#' in test else None
        if result.returncode == 0 and match and not any(s in output for s in ['FAILURES', 'INSTRUMENTATION_FAILED']):
            if count <= 0 or (expected is not None and count != expected):
                raise AssertionError(f'{name}: executed {count} tests; expected {expected or "a positive count"} ({test})')
            current_case.executed.add(test)
            results.append(dict(suite=current_suite, scenario=name, tests=count))
            return
        # Retry only explicit automation interference, never app crashes/assertions.
        interference = any(s in output for s in ['UiAutomation not connected', 'already registered!'])
        if not interference or attempt:
            raise AssertionError(name)
        (scratch / (name + '-interference.log')).write_text(output)
        time.sleep(2)


def port():
    with socket.socket() as reserve:
        reserve.bind(('0.0.0.0', 0))
        return reserve.getsockname()[1]


def start(command, name, **kwargs):
    log = (scratch / (name + '.log')).open('w')
    logs.append(log)
    child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, **kwargs)
    children.append(child)
    return child


def stop(child):
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def wait_ready(child, name, needle='READY'):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if needle in (scratch / (name + '.log')).read_text():
            return
        assert child.poll() is None, name + ' exited'
        time.sleep(.05)
    raise TimeoutError(name + ' readiness')


def restore():
    for permission in permissions:
        for command in [['clear-permission-flags', package, 'android.permission.' + permission, 'user-set', 'user-fixed'],
                        ['grant', package, 'android.permission.' + permission]]:
            subprocess.run([*adb, 'shell', 'pm', *command], timeout=15, check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def screenshots(remote, destination):
    target = scratch / destination
    target.mkdir(exist_ok=True)
    listing = subprocess.run([*adb, 'exec-out', 'run-as', package, 'ls', '-1', remote],
                             timeout=15, capture_output=True, text=True)
    if listing.returncode:
        return
    for name in listing.stdout.splitlines():
        if name.endswith(('.png', '.xml')):
            capture = subprocess.run([*adb, 'exec-out', 'run-as', package, 'cat', remote + '/' + name],
                                     timeout=15, capture_output=True)
            if capture.returncode == 0:
                (target / name).write_bytes(capture.stdout)


def cert(stem):
    key, pem = scratch / (stem + '.key'), scratch / (stem + '.pem')
    keys.append(key)
    run(['openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256',
         '-nodes', '-keyout', str(key), '-out', str(pem), '-days', '2', '-subj', '/CN=' + stem],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    der = run(['openssl', 'x509', '-in', str(pem), '-outform', 'DER'], capture_output=True).stdout
    return pem, key, hashlib.sha256(der).hexdigest()


def identity(name):
    instrument('export-' + name, IDENTITY_EXPORT, ['-e', 'prepareIdentity', 'true'])
    leaf = run([*adb, 'exec-out', 'run-as', package, 'cat', 'files/client.der'], capture_output=True).stdout
    digest = hashlib.sha256(leaf).hexdigest()
    assert digest not in identity_hashes, 'Phone identity did not rotate'
    identity_hashes.add(digest)
    der, pem = scratch / (name + '-client.der'), scratch / (name + '-client.pem')
    der.write_bytes(leaf)
    run(['openssl', 'x509', '-inform', 'DER', '-in', str(der), '-out', str(pem)])
    return pem, digest


@contextmanager
def peer(name, server, client_ca, flags=(), witness=False):
    peer_port = port()
    witness_port = port() if witness else None
    events = scratch / (name + '-events.jsonl')
    events.write_text('')
    child = start(['python3', str(root / 'tools/protocol_peer.py'),
                   '--cert', str(server[0]), '--key', str(server[1]), '--client-ca', str(client_ca),
                   '--port', str(peer_port), '--events', str(events),
                   *(['--witness-port', str(witness_port)] if witness else []), *flags],
                  name + '-peer', stdin=subprocess.PIPE, text=True)
    try:
        wait_ready(child, name + '-peer', 'READY ' + str(peer_port))
        extras = ['-e', 'pcPin', server[2], '-e', 'pcPort', str(peer_port), '-e', 'identityPrepared', 'true']
        if witness:
            extras += ['-e', 'witnessPort', str(witness_port)]
        yield extras, events, peer_port
    finally:
        stop(child)


def records(events):
    return [json.loads(line) for line in events.read_text().splitlines()]


def count(rows, event):
    return sum(row['event'] == event for row in rows)


def authenticated(rows, digest):
    assert all(row['client_fp'] == digest for row in rows if row['event'] == 'HELLO'), rows


LOCAL_TESTS = (
    'DeviceFixtureDeviceTest', 'MainActivityTest', 'CoreDeviceTest',
    'FullReviewDeviceTest', 'SecondReviewDeviceTest', 'AdvisoryDeviceTest',
    'FourthReviewStorageDeviceTest', 'FifthReviewStorageDeviceTest',
    'FifthReviewUiDeviceTest', 'SixthReviewStorageDeviceTest', 'SeventhReviewStorageDeviceTest',
    'RoundTwoAdvisoryDeviceTest', 'TenthReviewDeviceTest', 'TenthReviewUiDeviceTest',
    'TwelfthReviewDeviceTest', 'NinthReviewDeviceTest', 'EighthReviewDeviceTest',
    'ThirteenthReviewDeviceTest#failedInitialCaptureShowsSettingsFailureWithoutAdmitting',
    'FourteenthReviewDeviceTest', 'FifteenthReviewDeviceTest',
    'SixteenthReviewDeviceTest', 'SixteenthRecentDeviceTest', 'EighteenthReviewUiDeviceTest',
    'TwentiethReviewDeviceTest', 'TwentyFirstPolicyDeviceTest', 'TwentySecondReviewDeviceTest',
)


def local_case(test):
    restore()
    try:
        instrument(test, test, seconds=600)
    finally:
        screenshots('files/fifth-review-screens', 'screens')
        if test == 'SixteenthRecentDeviceTest':
            hierarchy = subprocess.run([*adb, 'exec-out', 'run-as', package, 'cat', 'cache/sixteenth-recent.xml'],
                                       capture_output=True, timeout=15)
            if hierarchy.returncode == 0:
                (scratch / 'active-recent-ui.xml').write_bytes(hierarchy.stdout)


def permission_denials(test):
    # Revocation must be outside instrumentation: Android kills the target app.
    install()
    run([*adb, 'shell', 'am', 'force-stop', package])
    for permission in permissions:
        run([*adb, 'shell', 'pm', 'revoke', package, 'android.permission.' + permission])
        run([*adb, 'shell', 'pm', 'set-permission-flags', package, 'android.permission.' + permission, 'user-set', 'user-fixed'])
    try:
        instrument('permission-denials', test, seconds=240)
    finally:
        restore()


def snapshot_rows():
    # Read copies only. Never open or mutate the live phone DB from the host.
    db = scratch / 'cold-outgoing.db'
    for suffix in ('', '-wal'):
        result = subprocess.run([*adb, 'exec-out', 'run-as', package, 'cat', 'databases/outgoing.db' + suffix], capture_output=True, timeout=15)
        path = Path(str(db) + suffix)
        valid = result.stdout.startswith(b'SQLite format 3\x00') if not suffix else result.stdout[:4] in (b'\x37\x7f\x06\x82', b'\x37\x7f\x06\x83')
        if result.returncode == 0 and valid:
            path.write_bytes(result.stdout)
        else:
            path.unlink(missing_ok=True)
    try:
        with sqlite3.connect(db) as connection:
            return connection.execute("SELECT kind,state,size,hash FROM outgoing WHERE state IN ('waiting','failed','sending')").fetchall()
    except sqlite3.Error:
        return []  # A writer may advance between the separate DB/WAL copies.
    finally:
        for suffix in ('', '-wal', '-shm'):
            Path(str(db) + suffix).unlink(missing_ok=True)

def cold_share(mode, prepare, verify):
    restore()
    instrument('cold-' + mode + '-prepare', prepare, ['-e', 'mode', mode])
    data = json.loads(run([*adb, 'exec-out', 'run-as', package, 'cat', 'files/cold-share.json'], capture_output=True).stdout)
    for name in (package, package + '.test'):
        run([*adb, 'shell', 'am', 'force-stop', name])
    pids = subprocess.run([*adb, 'shell', 'pidof', package], capture_output=True, text=True, timeout=15)
    assert not pids.stdout.strip(), 'Target must be cold before SEND'
    command = ['am', 'start', '-a', 'android.intent.action.SEND', '-n', package + '/.features.ShareActivity']
    if mode == 'single':
        command += ['-t', 'application/octet-stream', '-d', data['uri'], '--eu', 'android.intent.extra.STREAM', data['uri'], '--grant-read-uri-permission']
    elif mode == 'text':
        command += ['-t', 'text/plain', '--es', 'android.intent.extra.TEXT', data['text']]
    else:
        command = ['am', 'start', '-n', package + '.test/' + package + '.FixtureCallerActivity', '--esal', 'uris',
                   'content://' + package + '.test.sources/image,content://' + package + '.test.sources/third-image', '--ez', 'newTask', 'true']
    run([*adb, 'shell', shlex.join(command)], stdout=subprocess.DEVNULL)
    deadline = time.monotonic() + 12
    expected = 2 if mode == 'multiple' else 1
    while time.monotonic() < deadline:
        rows = snapshot_rows()
        if len(rows) == expected:
            break
        time.sleep(.1)
    else:
        raise AssertionError('No queue row after cold ' + mode + ' SEND')
    evidence = dict(mode=mode, no_target_pid_before_send=True, witnessed_rows_before_instrumentation=rows)
    (scratch / ('cold-' + mode + '-evidence.json')).write_text(json.dumps(evidence, indent=2) + '\n')
    instrument('cold-' + mode + '-verify', verify)

def fresh_pairing(test):
    # Already holds a heavy slot. No warm-up instrumentation is allowed here.
    install()
    run([*adb, 'shell', 'pm', 'clear', package], stdout=subprocess.DEVNULL)
    # Permission UI can obscure the confirmation; this does not start the app.
    run([*adb, 'shell', 'pm', 'grant', package, 'android.permission.CAMERA'])
    assert not subprocess.run([*adb, 'shell', 'pidof', package], capture_output=True, text=True, timeout=15).stdout.strip()
    link = f'pneu://pair?v=1&name=Fresh%20PC&fp={"a"*64}&addr=127.0.0.1%3A1&token={"b"*64}&exp={int(time.time()*1000)+300000}'
    run([*adb, 'shell', shlex.join(['am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', link, '-n', package + '/.MainActivity'])], stdout=subprocess.DEVNULL)
    remote = '/data/local/tmp/pneu16-fresh-' + str(os.getpid()) + '.xml'
    try:
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            run([*adb, 'shell', 'timeout', '10', 'uiautomator', 'dump', remote], stdout=subprocess.DEVNULL)
            xml = run([*adb, 'exec-out', 'cat', remote], capture_output=True).stdout
            (scratch / 'fresh-link-ui.xml').write_bytes(xml)
            if any(node.attrib.get('text') == 'Pair with Fresh PC?' for node in ET.fromstring(xml).iter('node')):
                break
        else:
            raise AssertionError('Fresh deep link never showed its pairing confirmation')
        pid = run([*adb, 'shell', 'pidof', package], capture_output=True, text=True).stdout.strip()
        assert pid, 'Fresh activity crashed before instrumentation'
        (scratch / 'fresh-link-evidence.json').write_text(json.dumps(dict(data_cleared=True, no_pid_before_launch=True, confirmation_before_instrumentation=True, pid=pid), indent=2) + '\n')
        instrument('fresh-pairing', test)
    finally:
        run([*adb, 'shell', 'rm', '-f', remote])


class SilentTls:
    """Accept TCP but leave TLS unanswered until the phone's handshake deadline."""
    def __init__(self):
        self.socket = socket.create_server(('0.0.0.0', 0))
        self.socket.settimeout(.2)
        self.port = self.socket.getsockname()[1]
        self.stopped = threading.Event()
        self.connections, self.durations, self.readers = [], [], []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopped.is_set():
            try:
                connection, _ = self.socket.accept()
                self.connections.append(connection)
                reader = threading.Thread(target=self.observe,
                                          args=(connection, time.monotonic()), daemon=True)
                self.readers.append(reader)
                reader.start()
            except socket.timeout:
                pass
            except OSError:
                return

    def observe(self, connection, started):
        try:
            connection.settimeout(.2)
            while not self.stopped.is_set():
                try:
                    if not connection.recv(4096):
                        break
                except socket.timeout:
                    pass
        except OSError:
            pass
        finally:
            self.durations.append(time.monotonic() - started)

    def close(self):
        self.stopped.set()
        self.socket.close()
        self.thread.join(timeout=2)
        for connection in self.connections:
            connection.close()
        for reader in self.readers:
            reader.join(timeout=2)


def feature_suite(name, test):
    ca, digest = identity(name)
    server = cert(name + '-pc')
    flags = ['--drop-first-pair-result', '--early-clipboard'] if name == 'features' else []
    current_sinks = []
    with peer(name, server, ca, flags, witness=True) as (extras, events, _):
        if name == 'features':
            current_sinks = [SilentTls(), SilentTls()]
            sinks.extend(current_sinks)
            extras += ['-e', 'slowPorts', ','.join(str(s.port) for s in current_sinks)]
        try:
            instrument(name, test, extras, 360)
            rows = records(events)
            authenticated(rows, digest)
            if name == 'features':
                assert count(rows, 'PAIR') == 2, rows
                assert all(len(s.connections) == 2 for s in current_sinks), 'UI reset candidate traversal'
                durations = [s.durations for s in current_sinks]
                (scratch / 'slow-candidates.json').write_text(json.dumps(durations))
                assert all(len(times) == 2 and all(t >= 9.5 for t in times) for times in durations), durations
            else:
                for ident in ['f', 'c', '9']:
                    assert any(r['event'] == 'PHONE_ACK' and r['transfer_id'] == ident * 32 for r in rows), rows
            screenshots('files/feature-screens', 'screens')
        finally:
            for sink in current_sinks:
                sink.close()


CONNECTION_CASES = [
    ('pairing-retry', 'TwentyFirstPairingDeviceTest', ['--reject-first-pair-result']),
    ('fourth', 'FourthReviewConnectionDeviceTest', []),
    ('fifth', 'FifthReviewConnectionDeviceTest', ['--drop-first-text-receipt', '--text-ack-delay', '2']),
    ('sixth', 'SixthReviewConnectionDeviceTest', []),
]


def connection_case(name, test, flags):
    ca, digest = identity(name)
    server = cert(name + '-pc')
    with peer(name, server, ca, flags, witness=True) as (extras, events, _):
        instrument(name, test, extras, 240)
        rows = records(events)
        authenticated(rows, digest)
        if name == 'pairing-retry':
            assert count(rows, 'HELLO') == 2 and count(rows, 'PAIR') == count(rows, 'PAIR_REJECTED') == 1, rows
            assert any(r['event'] == 'PAIR_REJECTED' and r['code'] == 'storage_failed' for r in rows), rows
        elif name == 'fourth':
            assert count(rows, 'HELLO') == 2 and count(rows, 'FILE') == 7, rows
            rejected = [r for r in rows if r['event'] == 'FILE_REJECTED']
            assert len(rejected) == 1 and rejected[0]['code'] == 'size_mismatch' and rejected[0]['size'] == 262144, rows
        elif name == 'fifth':
            assert count(rows, 'HELLO') == 3 and count(rows, 'DROPPED_TEXT') == 1, rows
            texts = [r for r in rows if r['event'] == 'TEXT']
            assert len(texts) == 4 and len({r['transfer_id'] for r in texts}) == 4, rows
        else:
            assert count(rows, 'HELLO') == 3 and count(rows, 'PAIR') == count(rows, 'FILE') == 1, rows
            assert count(rows, 'TEXT') == 0, rows


FAULT_CASES = [
    ('completion-write-recovery', 'failedCompletionAndFallbackWritesRecoverBothLanesWithoutSpinning', (), None),
    ('ineligible-backoff', 'repeatedlyIneligibleSelectionBacksOffAndDoesNotStarveSibling', (), None),
    ('discard-writer-waiters', 'discardedWriterWaitersNeverEmitAndDoNotResetEitherLane', (), None),
    ('discard-emitted-text', 'discardedEmittedTextDrainsReceiptWithoutResettingFileLane', ('--text-ack-delay','2'), None),
    ('pause-hash', 'pauseDuringHashHoldsAutomaticButNotManual', (), None),
    ('pause-encoding', 'pauseDuringEncodingKeepsPublishedSnapshotUnattemptedUntilResume', (), None),
    ('text-file-lanes', 'supersessionAndManualRetryPreserveLargeFileConnection', ('--text-ack-delay','2','--chunk-delay','0.005'), None),
    ('ready-before-retry', 'readyFileDrainsBeforeEarlierRetryDelay', (), None),
    ('durable-unpair', 'localUnpairDurablyRemovesBindingBeforeCapturedNotification', (), None),
    ('large', 'progressingLargeFileReachesVerifiedPeerAck', (), None),
    ('remote-unpair', 'remoteUnpairClearsSuccessAndBindingDerivedState', ('--unpair-after-pair-result',), None),
    ('binding-changes', 'bindingChangesInvalidateTransientState', (), None),
    ('tls-fault', 'pinnedHandshakeFaultPreservesBinding', (), 'wrong-ca'),
    ('unauthorized', 'threePinnedClientRejectionsForgetBindingAndRetainLostName', ('--unauthorized-hello',), None),
    ('generic-timeout', 'helloDripCannotExtendAbsoluteDeadline', ('--hello-drip',), None),
    ('stalled-unpair', 'unpairClosesStalledWritePromptly', ('--stall-file-after-begin',), None),
    ('pin-fallback', 'pinFailureFallsBackWithoutLosingTrust', (), None),
    ('current-policy', 'currentTransportPolicyStopsSessionAndEveryRetry', (), None),
    ('no-files', 'missingFileCapabilityIsVisibleAndDoesNotBlockText', ('--no-files',), None),
    ('reject-kind', 'unsupportedKindIsNotRetriedOnSameCapabilities', ('--reject-file-kind',), None),
    ('source-revoked', 'revokedSourceStillSendsAdmittedSnapshot', (), None),
    ('pair-drip', 'pairResultDripCannotExtendAbsoluteDeadline', ('--pair-drip',), None),
    ('ready-backoff', 'healthySessionsResetBackoffOnReadyEvenAfterEof', ('--ready-close',), None),
    ('hello-pause', 'pauseDuringHelloIsReconciledAtReady', ('--hello-delay','3'), None),
]


def connection_fault_case(name, test, flags, ca_override):
    ca, digest = identity(name)
    server = cert(name + '-pc')
    wrong = cert(name + '-wrong') if ca_override or name == 'pin-fallback' else None
    ca_override = wrong[0] if ca_override else None
    with peer(name, server, ca_override or ca, flags, witness=name in ('completion-write-recovery', 'ineligible-backoff', 'discard-emitted-text', 'pause-hash', 'pause-encoding')) as (extras, events, _):
        if name == 'pin-fallback':
            with peer(name + '-wrong', wrong, ca) as (_, _, wrong_port):
                instrument(name, test,
                           extras + ['-e', 'rejectPort', str(wrong_port)], 240)
        else:
            instrument(name, test, extras, 240)
        rows = records(events)
        authenticated(rows, digest)
        if name == 'completion-write-recovery':
            assert count(rows, 'HELLO') == 1 and count(rows, 'FILE_BEGIN') == 3 and count(rows, 'TEXT') == 2, rows
            files = [r for r in rows if r['event'] == 'FILE']
            assert [r['size'] for r in files] == [64, 8, 64], rows
            assert files[0]['transfer_id'] == files[2]['transfer_id'] != files[1]['transfer_id'], rows
            assert files[0]['sha256'] == files[2]['sha256'], rows
        elif name == 'ineligible-backoff':
            assert count(rows, 'HELLO') == 1 and count(rows, 'FILE_BEGIN') == 2, rows
            assert [r['size'] for r in rows if r['event'] == 'FILE'] == [8, 64], rows
        elif name in ('discard-writer-waiters', 'discard-emitted-text', 'pause-hash', 'pause-encoding'):
            assert count(rows, 'HELLO') == 1, rows
            files = [r for r in rows if r['event'] == 'FILE_BEGIN']
            if name == 'discard-writer-waiters':
                assert len(files) == count(rows, 'FILE') == count(rows, 'TEXT') == 1, rows
                assert not any(r.get('sha256') == hashlib.sha256(b'Discarded writer waiter').hexdigest() for r in rows), rows
            elif name == 'discard-emitted-text':
                assert len(files) == count(rows, 'FILE') == 1 and count(rows, 'TEXT') == 2, rows
            else:
                assert len(files) == count(rows, 'FILE') == 2, rows
                statuses = [index for index, r in enumerate(rows) if r['event'] == 'STATUS' and r['paused']]
                resumes = [index for index, r in enumerate(rows) if r['event'] == 'STATUS' and not r['paused']]
                begins = [index for index, r in enumerate(rows) if r['event'] == 'FILE_BEGIN']
                assert statuses and resumes and statuses[0] < begins[0] < begins[1], rows
                completed = [r for r in rows if r['event'] == 'FILE']
                if name == 'pause-encoding':
                    assert completed[0]['kind'] == 'file' and completed[1]['mime'] == 'image/jpeg', rows
                else:
                    assert [r['size'] for r in completed] == [8, 64], rows
        elif name == 'text-file-lanes':
            assert count(rows, 'HELLO') == 1 and count(rows, 'FILE_BEGIN') == 2, rows
            assert sorted(r['size'] for r in rows if r['event'] == 'FILE') == [8, 209715200], rows
        elif name == 'ready-before-retry':
            assert count(rows, 'FILE') == 1, rows
        elif name == 'durable-unpair':
            assert count(rows, 'UNPAIR') == 1, rows
        elif name == 'large':
            files = [r for r in rows if r['event'] == 'FILE']
            assert len(files) == 1 and files[0]['size'] == 209715200, rows
            assert files[0]['sha256'] == '72abf2ca8f36943ebe2e49ca3a51d409ca5f0bfcffab6c9d25643c17c32889da', rows
        elif name == 'unauthorized':
            assert count(rows, 'UNAUTHORIZED') == 3, rows
        elif name == 'tls-fault':
            assert (scratch / (name + '-peer.log')).read_text().count('CERTIFICATE_VERIFY_FAILED') >= 3
        elif name == 'source-revoked':
            assert count(rows, 'HELLO') == 1 and count(rows, 'FILE') == 2, rows
        elif name == 'hello-pause':
            assert any(r['event'] == 'STATUS' and r['paused'] for r in rows), rows
    screenshots('files/e2e-fix-screens', 'screens')


def tls_case(name, test):
    ca, digest = identity(name)
    server = cert(name + '-pc')
    if name == 'mutual':
        with peer(name, server, ca) as (extras, events, _):
            instrument(name, test, extras)
            assert count(records(events), 'HELLO') == 1
            authenticated(records(events), digest)
    elif name == 'address-fallback':
        wrong = cert('wrong-pc')
        with peer('wrong-pin', server, ca, ['--first-connection-mode', 'wrong-pin', '--wrong-cert', str(wrong[0]), '--wrong-key', str(wrong[1])]) as (_, wrong_events, wrong_port):
            with peer('plaintext', server, ca, ['--first-connection-mode', 'plaintext']) as (_, plain_events, plain_port):
                instrument(name, test, ['-e', 'pcPin', server[2], '-e', 'wrongPort', str(wrong_port),
                                       '-e', 'plainPort', str(plain_port), '-e', 'identityPrepared', 'true'])
                for events, event in [(wrong_events, 'WRONG_PIN'), (plain_events, 'BAD_TLS')]:
                    rows = records(events)
                    assert count(rows, 'HELLO') == count(rows, event) == 1, rows
                    authenticated(rows, digest)
    else:
        der = run(['openssl', 'x509', '-in', str(server[0]), '-outform', 'DER'], capture_output=True).stdout
        rejection = ['-e', 'pcPin', server[2], '-e', 'pcCert', base64.b64encode(der).decode(), '-e', 'identityPrepared', 'true']
        if name == 'tls12':
            with peer(name, server, ca, ['--tls12-only']) as (_, _, reject_port):
                instrument(name, test, rejection + ['-e', 'rejectPort', str(reject_port)])
        else:
            reject_port = port()
            child = start(['python3', str(root / 'tools/invalid_certificate_verify_peer.py'),
                           '--cert', str(server[0]), '--port', str(reject_port)],
                          'invalid-proof-peer', stdin=subprocess.PIPE, text=True)
            try:
                wait_ready(child, 'invalid-proof-peer')
                instrument(name, test, rejection + ['-e', 'rejectPort', str(reject_port)])
                assert 'INVALID_CERTIFICATE_VERIFY_SENT' in (scratch / 'invalid-proof-peer.log').read_text()
            finally:
                stop(child)


def upload(test):
    ca, digest = identity('upload')
    server = cert('upload-pc')
    with peer('upload', server, ca, ['--chunk-delay', '0.31']) as (extras, events, _):
        instrument('long-upload', test, extras, 360)
        rows = records(events)
        authenticated(rows, digest)
        assert count(rows, 'HELLO') == 1, rows
        begins = [e for e in rows if e['event'] == 'FILE_BEGIN']
        files = [e for e in rows if e['event'] == 'FILE']
        assert len(begins) == len(files) == 1 and files[0]['size'] == 167772160, rows
        expected = hashlib.sha256()
        for _ in range(640):
            expected.update(bytes(262144))
        assert files[0]['sha256'] == expected.hexdigest(), rows
        elapsed = files[0]['at'] - begins[0]['at']
        assert elapsed > 180000, rows
        assert 'PING/PONG' not in (scratch / 'upload-peer.log').read_text()
        print(f'One verified ACK after {elapsed / 1000:.1f} s; no PONG traffic.', flush=True)


def daemon(test):
    env = dict(os.environ, CARGO_TARGET_DIR=str(args.daemon.parent.parent))
    run(['cargo', 'build', '-p', 'pneud'], cwd=root.parent / 'desktop', env=env, timeout=900)
    assert args.daemon.is_file(), 'Real pneud build missing'
    instrument('daemon-identity', IDENTITY_EXPORT, ['-e', 'prepareIdentity', 'true'])
    base = scratch / 'daemon-xdg'
    env = os.environ.copy()
    for variable, subdir in [('XDG_RUNTIME_DIR', 'runtime'), ('XDG_CONFIG_HOME', 'config'),
                             ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')]:
        directory = base / subdir
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        env[variable] = str(directory)
    env.pop('DISPLAY', None)
    env.pop('WAYLAND_DISPLAY', None)
    env['DBUS_SESSION_BUS_ADDRESS'] = 'unix:path=' + str(base / 'absent-bus')
    daemon_port = port()
    config = base / 'config/pneu'
    config.mkdir(mode=0o700, exist_ok=True)
    (config / 'config.toml').write_text(f'name = "Tenth real daemon"\nport = {daemon_port}\nsave_to_folder = false\nnotification_preview = false\ncopy_image_to_clipboard = false\nclipboard_sync_to_phone = false\n')
    child = start([str(args.daemon.resolve()), 'run'], 'real-daemon', env=env)
    ipc_path = base / 'runtime/pneud.sock'

    def ipc(command, **fields):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5)
            client.connect(str(ipc_path))
            client.sendall(json.dumps(dict(command=command, **fields)).encode() + b'\n')
            reply = json.loads(client.makefile('rb').readline(65536))
            assert reply['ok'], reply
            return reply['value']

    control = socket.create_server(('0.0.0.0', 0))
    control.settimeout(.2)
    stopping = threading.Event()
    failures = []
    evidence = {}

    def forget_offline():
        try:
            deadline = time.monotonic() + 100
            while not stopping.is_set():
                try:
                    connection, _ = control.accept()
                    break
                except socket.timeout:
                    if time.monotonic() >= deadline:
                        raise TimeoutError('Offline-unpair control request')
            else:
                return
            with connection:
                connection.settimeout(5)
                request = json.loads(connection.makefile('rb').readline(4096))
                deadline = time.monotonic() + 10
                while not stopping.is_set() and time.monotonic() < deadline:
                    status = ipc('status')
                    if not status['connected_devices']:
                        break
                    time.sleep(.05)
                if stopping.is_set():
                    return
                assert not status['connected_devices'], status
                evidence['offline_before_unpair'] = True
                # Same CLI command used by the user, with only these temp XDG dirs.
                run([str(args.daemon.resolve()), 'unpair', request['device_id']], env=env, capture_output=True, timeout=10)
                ipc('close_pairing')
                assert not ipc('status')['paired_devices']
                evidence['daemon_forgot_phone'] = True
                connection.sendall(b'{"ok":true}\n')
        except Exception as error:
            failures.append(error)

    thread = threading.Thread(target=forget_offline, daemon=True)
    try:
        for _ in range(100):
            assert child.poll() is None, 'Daemon exited'
            try:
                ipc('status')
                break
            except (OSError, AssertionError):
                time.sleep(.1)
        else:
            raise TimeoutError('Daemon readiness')
        invitation = ipc('pair')['uri']
        parsed = urllib.parse.urlsplit(invitation)
        fields = [(k, v) for k, v in urllib.parse.parse_qsl(parsed.query) if k != 'addr']
        fields.append(('addr', f'10.0.2.2:{daemon_port}'))
        raw = 'pneu://pair?' + urllib.parse.urlencode(fields, quote_via=urllib.parse.quote)
        thread.start()
        instrument('real-daemon-revocation', test,
                   ['-e', 'pcUri', raw, '-e', 'controlPort', str(control.getsockname()[1])], 100)
        evidence['phone_received_unauthorized_frames'] = 3  # Device asserts exact decoded frame count.
        evidence['phone_pairing_lost'] = True
        thread.join(timeout=5)
        assert not thread.is_alive() and not failures, failures
        (scratch / 'real-daemon-evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')
    finally:
        stopping.set()
        control.close()
        stop(child)
        if thread.ident is not None:
            thread.join(timeout=20)
        shutil.rmtree(base)  # Includes only this test's private identity/keys.

def cold_cleanup():
    result = subprocess.run([*adb, 'exec-out', 'run-as', package, 'cat', 'files/cold-share.json'],
                            capture_output=True, timeout=15)
    try:
        data = json.loads(result.stdout) if result.returncode == 0 else None
    except (ValueError, UnicodeDecodeError):
        data = None
    if data is not None:
        if data.get('uri'):
            subprocess.run([*adb, 'shell', 'content', 'delete', '--uri', data['uri']], timeout=15, check=False)
        subprocess.run([*adb, 'shell', 'run-as', package, 'rm', 'files/cold-share.json'], timeout=15, check=False)

def cleanup():
    errors = []

    def attempt(action):
        try:
            action()
        except Exception as error:
            errors.append(str(error))

    for sink in sinks:
        attempt(sink.close)
    for child in children:
        attempt(lambda child=child: stop(child))
    for log in logs:
        attempt(log.close)
    for key in keys:
        attempt(lambda key=key: key.unlink(missing_ok=True))
    sinks.clear()
    children[:] = [child for child in children if child.poll() is None]
    logs.clear()
    keys[:] = [key for key in keys if key.exists()]
    attempt(lambda: shutil.rmtree(scratch / 'daemon-xdg', ignore_errors=True))
    attempt(restore)
    for name in [package, package + '.test']:
        attempt(lambda name=name: subprocess.run([*adb, 'shell', 'am', 'force-stop', name], timeout=15, check=False))
    # Only delete the cold fixture's recorded source if a failed phase left it.
    attempt(cold_cleanup)
    if errors:
        raise RuntimeError('; '.join(errors))


SUITES = {
    'local': [DeviceCase(test, test, local_case) for test in LOCAL_TESTS] +
             [DeviceCase('permission-denials', 'FourthReviewUiDeviceTest', permission_denials)],
    'cold': [DeviceCase('fresh-pairing', 'SixteenthFreshPairingDeviceTest', fresh_pairing)] +
            [DeviceCase('cold-' + mode,
                        ('ThirteenthReviewDeviceTest#prepareColdShare', 'ThirteenthReviewDeviceTest#verifyColdShareAndCleanUp'),
                        lambda prepare, verify, mode=mode: cold_share(mode, prepare, verify))
             for mode in ('single', 'multiple', 'text')],
    'connection': [DeviceCase(name, test, lambda test, name=name, flags=flags: connection_case(name, test, flags), identity=True)
                   for name, test, flags in CONNECTION_CASES],
    'faults': [DeviceCase(name, 'ConnectionFaultDeviceTest#' + method,
                        lambda test, name=name, flags=flags, ca=ca: connection_fault_case(name, test, flags, ca), identity=True)
               for name, method, flags, ca in FAULT_CASES],
    'tls': [DeviceCase(name, test, lambda test, name=name: tls_case(name, test), identity=True)
            for name, test in [('mutual', 'TlsDeviceTest#keystoreMutualTlsHandshake'),
                               ('address-fallback', 'RoundTwoTlsDeviceTest'),
                               ('tls12', 'TlsDeviceTest#rejectedHandshake'),
                               ('invalid-proof', 'TlsDeviceTest#rejectedHandshake')]],
    'features': [DeviceCase('features', 'FeaturesDeviceTest', lambda test: feature_suite('features', test), identity=True)],
    'policy': [DeviceCase('policy', 'PolicyDeviceTest', lambda test: feature_suite('policy', test), identity=True)],
    'upload': [DeviceCase('long-upload', 'ConnectionFaultDeviceTest#progressingUploadBeyondThreeMinutesReachesVerifiedPeerAck', upload, identity=True)],
    'daemon': [DeviceCase('real-daemon-revocation', 'TenthReviewDaemonDeviceTest', daemon, identity=True)],
}


def selected_device_tests():
    return {test for cases in SUITES.values() for case in cases for test in case.tests}


def case_resources():
    return tuple(len(items) for items in (children, logs, keys, sinks))


def cleanup_case(markers, failed):
    """Release only this case's resources; preserve any suite-owned fixtures."""
    errors = []
    def attempt(action):
        try:
            action()
        except Exception as error:
            errors.append(str(error))
    child_mark, log_mark, key_mark, sink_mark = markers
    for sink in sinks[sink_mark:]:
        attempt(sink.close)
    for child in children[child_mark:]:
        attempt(lambda child=child: stop(child))
    for log in logs[log_mark:]:
        attempt(log.close)
    for key in keys[key_mark:]:
        attempt(lambda key=key: key.unlink(missing_ok=True))
    children[child_mark:] = [child for child in children[child_mark:] if child.poll() is None]
    if not errors:
        del sinks[sink_mark:]
        del logs[log_mark:]
    keys[key_mark:] = [key for key in keys[key_mark:] if key.exists()]
    if failed:
        # A crashed class may leave grants, services or fault hooks in the app
        # process. Its next class starts a fresh process and shared fixture reset.
        attempt(restore)
        for name in (package, package + '.test'):
            attempt(lambda name=name: run([*adb, 'shell', 'am', 'force-stop', name], timeout=15))
        attempt(cold_cleanup)
    if errors:
        raise RuntimeError('; '.join(errors))


def run_suites(selected, base_scratch):
    """Continue after each independent case and each suite; never mask failure."""
    global scratch, current_suite, current_case
    suite_results = []
    for current_suite in selected:
        current_case = None
        scratch = base_scratch / current_suite
        scratch.mkdir(exist_ok=True)
        before = len(results)
        errors = []
        print('SUITE ' + current_suite, flush=True)
        try:
            install()
            for current_case in SUITES[current_suite]:
                print('CASE ' + current_case.name, flush=True)
                current_case.executed = set()
                markers = case_resources()
                failed = False
                try:
                    current_case.run()
                    missing = set(current_case.tests) - current_case.executed
                    assert not missing, 'Declared selectors were not executed: ' + ', '.join(sorted(missing))
                except Exception as error:
                    failed = True
                    errors.append(current_case.name + ': ' + str(error))
                    traceback.print_exc()
                finally:
                    try:
                        cleanup_case(markers, failed)
                    except Exception as error:
                        errors.append(current_case.name + ' cleanup: ' + str(error))
                        traceback.print_exc()
            current_case = None
        except Exception as error:
            errors.append(str(error))
            traceback.print_exc()
        finally:
            current_case = None
            try:
                cleanup()
            except Exception as error:
                errors.append('Cleanup: ' + str(error))
                traceback.print_exc()
        summary = dict(suite=current_suite, status='failed' if errors else 'passed',
                       executions=len(results) - before,
                       tests=sum(r['tests'] for r in results[before:]))
        if errors:
            summary['errors'] = errors
        suite_results.append(summary)
        print(json.dumps(summary), flush=True)
        (base_scratch / 'results.json').write_text(json.dumps(dict(suites=suite_results, executions=results), indent=2) + '\n')
    print('Suite summary:', flush=True)
    for summary in suite_results:
        print(f"{summary['status'].upper():6} {summary['suite']}: {summary['executions']} successful instrumentation runs, {summary['tests']} passed tests" +
              ('; ' + '; '.join(summary['errors']) if summary.get('errors') else ''), flush=True)
    print('Evidence: ' + str(base_scratch), flush=True)
    return all(summary['status'] == 'passed' for summary in suite_results)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('suites', nargs='*', help='Suite names (default: all): ' + ', '.join(SUITES))
    parser.add_argument('--scratch', type=Path, default=Path.home() / '.cache/agent-scratch/pneu' / ('device-' + time.strftime('%Y%m%d-%H%M%S')))
    parser.add_argument('--serial', default='emulator-5554')
    parser.add_argument('--daemon', type=Path, default=Path.home() / '.cache/agent-scratch/pneu/android10-target/debug/pneud')
    args = parser.parse_args()
    selected = list(dict.fromkeys(args.suites or SUITES))
    unknown = set(selected) - (SUITES.keys() | {'all'})
    if unknown:
        parser.error('Unknown suites: ' + ', '.join(sorted(unknown)))
    if 'all' in selected:
        selected = list(SUITES)
    os.environ['PYTHONDONTWRITEBYTECODE'] = '1'
    adb = [str(Path(os.environ.get('ANDROID_HOME', str(Path.home() / 'Android/Sdk'))) / 'platform-tools/adb'), '-s', args.serial]
    base_scratch = args.scratch.resolve()
    base_scratch.mkdir(parents=True, exist_ok=True)
    try:
        success = run_suites(selected, base_scratch)
    finally:
        # Includes interruption: normal per-suite cleanup runs in run_suites too.
        cleanup()
    raise SystemExit(0 if success else 1)
