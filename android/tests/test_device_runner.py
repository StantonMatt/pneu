"""Focused host-only checks; run with PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests."""
import contextlib
import importlib.util
import io
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('device_runner', Path(__file__).resolve().parents[1] / 'tools/run_device_tests.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class DeviceRunnerTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.scratch = Path(self.directory.name)
        runner.adb = ['adb']
        runner.scratch = self.scratch
        runner.current_suite = "count-check"
        runner.current_case = None
        self.original_case_cleanup = runner.cleanup_case
        cleanup_case = patch.object(runner, 'cleanup_case')
        self.case_cleanup = cleanup_case.start()
        self.addCleanup(cleanup_case.stop)
        runner.results.clear()
        runner.children.clear()
        runner.sinks.clear()
        runner.logs.clear()
        runner.keys.clear()

    def test_cold_offline_text_waits_for_rejection_without_requiring_a_queue_row(self):
        response = subprocess.CompletedProcess([], 0, stdout='')
        marker = 'cold-text-test-marker'
        def command(cmd, **kwargs):
            if cmd[-1] == 'files/cold-share.json':
                return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps({'text': 'cold text'}))
            if 'logcat' in cmd:
                return subprocess.CompletedProcess(cmd, 0, stdout=marker + '\nshare_rejected: no_recipient\n')
            return response
        with patch.object(runner, 'restore'), patch.object(runner, 'instrument') as instrument, \
             patch.object(runner, 'run', side_effect=command), \
             patch.object(runner.subprocess, 'run', return_value=response), \
             patch.object(runner, 'snapshot_rows', return_value=[]), \
             patch('uuid.uuid4', return_value=Mock(hex='test-marker')), \
             patch.object(runner.time, 'sleep'), \
             patch.object(runner.time, 'monotonic', side_effect=[0, 0, 13]):
            runner.cold_share('text', 'prepare', 'verify')
        self.assertEqual(instrument.call_count, 2)
        evidence = json.loads((self.scratch / 'cold-text-evidence.json').read_text())
        self.assertEqual(evidence['rejection'], 'no_recipient')
        self.assertEqual(evidence['witnessed_rows_before_instrumentation'], [])

    def test_empty_cold_text_queue_without_this_attempts_completion_is_not_success(self):
        response = subprocess.CompletedProcess([], 0, stdout='')
        def command(cmd, **kwargs):
            if cmd[-1] == 'files/cold-share.json':
                return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps({'text': 'cold text'}))
            if 'logcat' in cmd:
                return subprocess.CompletedProcess(cmd, 0, stdout='old-marker\nshare_rejected: no_recipient\n')
            return response
        with patch.object(runner, 'restore'), patch.object(runner, 'instrument') as instrument, \
             patch.object(runner, 'run', side_effect=command), \
             patch.object(runner.subprocess, 'run', return_value=response), \
             patch.object(runner, 'snapshot_rows', return_value=[]), \
             patch('uuid.uuid4', return_value=Mock(hex='test-marker')), \
             patch.object(runner.time, 'sleep'), \
             patch.object(runner.time, 'monotonic', side_effect=[0, 0, 13]):
            with self.assertRaisesRegex(AssertionError, 'did not finish with no_recipient'):
                runner.cold_share('text', 'prepare', 'verify')
        instrument.assert_called_once_with('cold-text-prepare', 'prepare', ['-e', 'mode', 'text'])

    def test_cold_file_witnesses_still_require_the_expected_delivery_count(self):
        for mode, count in (('single', 1), ('multiple', 2)):
            with self.subTest(mode=mode):
                response = subprocess.CompletedProcess([], 0, stdout='')
                data = json.dumps({'uri': 'content://fixture', 'text': 'unused'})
                def command(cmd, **kwargs):
                    return subprocess.CompletedProcess(cmd, 0, stdout=data if cmd[-1] == 'files/cold-share.json' else '')
                with patch.object(runner, 'restore'), patch.object(runner, 'instrument') as instrument, \
                     patch.object(runner, 'run', side_effect=command), \
                     patch.object(runner.subprocess, 'run', return_value=response), \
                     patch.object(runner, 'snapshot_rows', return_value=[['file', 'waiting', 1, 'hash']] * count):
                    runner.cold_share(mode, 'prepare', 'verify')
                self.assertEqual(instrument.call_count, 2)
                evidence = json.loads((self.scratch / ('cold-' + mode + '-evidence.json')).read_text())
                self.assertEqual(count, len(evidence['witnessed_rows_before_instrumentation']))

    def test_failure_continues_and_summary_preserves_successful_runs(self):
        order = []
        def failing():
            order.append('first')
            runner.results.append(dict(suite='first', scenario='passed-method', tests=2))
            raise AssertionError('broken-method')
        def passing():
            order.append('second')
            runner.results.append(dict(suite='second', scenario='whole-class', tests=3))
        output = io.StringIO()
        with patch.dict(runner.SUITES, {'first': [runner.DeviceCase('broken', (), failing)], 'second': [runner.DeviceCase('good', (), passing)]}, clear=True), \
             patch.object(runner, 'install'), patch.object(runner, 'cleanup') as cleanup, \
             contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(runner.run_suites(['first', 'second'], self.scratch))
        self.assertEqual(order, ['first', 'second'])
        self.assertEqual(cleanup.call_count, 2)
        data = json.loads((self.scratch / 'results.json').read_text())
        self.assertEqual([s['status'] for s in data['suites']], ['failed', 'passed'])
        self.assertEqual([s['tests'] for s in data['suites']], [2, 3])
        self.assertIn('FAILED first', output.getvalue())
        self.assertIn('PASSED second', output.getvalue())

    def test_failed_class_does_not_stop_later_class_in_the_same_suite(self):
        runner.adb = ['adb']
        cases = [runner.DeviceCase('broken-class', 'FirstTest', lambda test: runner.instrument('broken-class', test)),
                 runner.DeviceCase('later-class', 'SecondTest', lambda test: runner.instrument('later-class', test))]
        responses = [subprocess.CompletedProcess([], 0, 'FAILURES!!!\n', ''),
                     subprocess.CompletedProcess([], 0, 'OK (1 test)\n', '')]
        with patch.dict(runner.SUITES, {'same-suite': cases}, clear=True), \
             patch.object(runner, 'install'), patch.object(runner, 'cleanup'), \
             patch.object(runner.subprocess, 'run', side_effect=responses) as commands, \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(runner.run_suites(['same-suite'], self.scratch))
        self.assertEqual(commands.call_count, 2)
        self.assertEqual(self.case_cleanup.call_args_list[0].args[1], True)
        self.assertEqual(self.case_cleanup.call_args_list[1].args[1], False)
        data = json.loads((self.scratch / 'results.json').read_text())
        self.assertEqual(data['suites'][0]['status'], 'failed')
        self.assertEqual(data['suites'][0]['tests'], 1)
        self.assertEqual(data['executions'][0]['scenario'], 'later-class')
        self.assertIn('broken-class', data['suites'][0]['errors'][0])

    def check_instrument_count(self, selector, count, accepted):
        case = runner.DeviceCase('count-check', selector, lambda test: None)
        case.executed = set()
        runner.current_case = case
        response = subprocess.CompletedProcess([], 0, f'OK ({count} tests)\n', '')
        runner.results.clear()
        with patch.object(runner, 'install'), \
             patch.object(runner.subprocess, 'run', return_value=response) as commands, \
             contextlib.redirect_stdout(io.StringIO()):
            if accepted:
                runner.instrument('count-check', selector)
                self.assertEqual(runner.results[0]['tests'], count)
                self.assertEqual(case.executed, {selector})
            else:
                with self.assertRaisesRegex(AssertionError, f'executed {count} tests; expected'):
                    runner.instrument('count-check', selector)
                self.assertEqual(runner.results, [])
                self.assertEqual(case.executed, set())
        self.assertEqual(commands.call_count, 1, 'Count mismatch must not retry as automation interference')

    def test_instrumentation_requires_positive_class_count(self):
        for count in (0, 1, 3):
            with self.subTest(count=count):
                self.check_instrument_count('CountTest', count, count > 0)

    def test_method_filter_requires_exactly_one_test(self):
        for count in (0, 1, 2):
            with self.subTest(count=count):
                self.check_instrument_count('CountTest#oneMethod', count, count == 1)

    def test_zero_count_failure_continues_to_later_case(self):
        cases = [runner.DeviceCase('zero', 'EmptyTest', lambda test: runner.instrument('zero', test)),
                 runner.DeviceCase('later', 'CountTest#oneMethod', lambda test: runner.instrument('later', test))]
        responses = [subprocess.CompletedProcess([], 0, 'OK (0 tests)\n', ''),
                     subprocess.CompletedProcess([], 0, 'OK (1 test)\n', '')]
        with patch.dict(runner.SUITES, {'counts': cases}, clear=True), \
             patch.object(runner, 'install'), patch.object(runner, 'cleanup'), \
             patch.object(runner.subprocess, 'run', side_effect=responses), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(runner.run_suites(['counts'], self.scratch))
        data = json.loads((self.scratch / 'results.json').read_text())
        self.assertEqual(data['suites'][0]['tests'], 1)
        self.assertEqual([r['scenario'] for r in data['executions']], ['later'])
        self.assertIn('expected a positive count', data['suites'][0]['errors'][0])

    def test_missing_declared_selector_and_cleanup_failure_do_not_stop_later_cases(self):
        passed = Mock()
        cases = [runner.DeviceCase('unused-selector', 'NeverExecutedTest', lambda test: None),
                 runner.DeviceCase('cleanup-fault', (), lambda: None),
                 runner.DeviceCase('later', (), passed)]
        self.case_cleanup.side_effect = [None, RuntimeError('fixture cleanup failed'), None]
        with patch.dict(runner.SUITES, {'suite': cases}, clear=True), \
             patch.object(runner, 'install'), patch.object(runner, 'cleanup'), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(runner.run_suites(['suite'], self.scratch))
        passed.assert_called_once()
        errors = json.loads((self.scratch / 'results.json').read_text())['suites'][0]['errors']
        self.assertIn('Declared selectors were not executed', errors[0])
        self.assertIn('fixture cleanup failed', errors[1])

    def test_case_cleanup_preserves_other_case_resources_and_stops_failed_process(self):
        key = self.scratch / 'private.key'
        key.write_text('fixture key')
        original = Mock()
        original.poll.return_value = None
        child = subprocess.Popen(['sleep', '30'])
        self.addCleanup(lambda: runner.stop(child))
        runner.children[:] = [original, child]
        log, sink = Mock(), Mock()
        runner.logs[:] = [log]
        runner.sinks[:] = [sink]
        runner.keys[:] = [key]
        # Use the real helper for this check despite the coordinator-test mock.
        with patch.object(runner, 'restore'), patch.object(runner, 'cold_cleanup'), \
             patch.object(runner, 'run') as commands:
            self.original_case_cleanup((1, 0, 0, 0), True)
        self.assertIsNotNone(child.poll())
        self.assertEqual(runner.children, [original])
        original.terminate.assert_not_called()
        self.assertFalse(key.exists())
        sink.close.assert_called_once();log.close.assert_called_once()
        self.assertEqual(commands.call_count, 2)

    def test_cleanup_failure_is_reported_and_next_suite_runs(self):
        next_suite = Mock()
        with patch.dict(runner.SUITES, {'first': [runner.DeviceCase('first-case', (), lambda: None)], 'second': [runner.DeviceCase('second-case', (), next_suite)]}, clear=True), \
             patch.object(runner, 'install'), patch.object(runner, 'cleanup', side_effect=[RuntimeError('close failed'), None]), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(runner.run_suites(['first', 'second'], self.scratch))
        next_suite.assert_called_once()
        data = json.loads((self.scratch / 'results.json').read_text())
        self.assertEqual(data['suites'][0]['errors'], ['Cleanup: close failed'])
        self.assertEqual(data['suites'][1]['status'], 'passed')

    def test_all_passing_suites_return_success(self):
        with patch.dict(runner.SUITES, {name: [runner.DeviceCase(name, (), lambda: None)] for name in ('a', 'b')}, clear=True), \
             patch.object(runner, 'install'), patch.object(runner, 'cleanup'), contextlib.redirect_stdout(io.StringIO()):
            self.assertTrue(runner.run_suites(['a', 'b'], self.scratch))

    def test_install_failure_still_attempts_each_suite_and_cleanup(self):
        with patch.dict(runner.SUITES, {name: [runner.DeviceCase(name, (), lambda: None)] for name in ('a', 'b')}, clear=True), \
             patch.object(runner, 'install', side_effect=RuntimeError('device offline')) as install, \
             patch.object(runner, 'cleanup') as cleanup, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(runner.run_suites(['a', 'b'], self.scratch))
        self.assertEqual(install.call_count, 2)
        self.assertEqual(cleanup.call_count, 2)
        data = json.loads((self.scratch / 'results.json').read_text())
        self.assertTrue(all(s['status'] == 'failed' for s in data['suites']))

    def test_permission_denial_failure_archives_ui_and_restores_permissions(self):
        for archive_error in (None, RuntimeError('archive unavailable')):
            with self.subTest(archive_error=archive_error), \
                 patch.object(runner, 'install'), patch.object(runner, 'run'), \
                 patch.object(runner, 'instrument', side_effect=AssertionError('UI failed')), \
                 patch.object(runner, 'screenshots', side_effect=archive_error) as archive, \
                 patch.object(runner, 'restore') as restore:
                expected = 'archive unavailable' if archive_error else 'UI failed'
                with self.assertRaisesRegex((AssertionError, RuntimeError), expected):
                    runner.permission_denials('FourthReviewUiDeviceTest')
                archive.assert_called_once_with('files/fourth-review-screens', 'permission-denial-screens')
                restore.assert_called_once()

    def test_cleanup_error_does_not_leave_a_child_or_private_key(self):
        runner.scratch = self.scratch
        runner.adb = ['adb']
        key = self.scratch / 'private.key'
        key.write_text('synthetic fixture key')
        runner.keys.append(key)
        sink = Mock()
        sink.close.side_effect = RuntimeError('listener fault')
        runner.sinks.append(sink)
        child = subprocess.Popen(['sleep', '30'])
        self.addCleanup(lambda: runner.stop(child))
        runner.children.append(child)
        log = Mock()
        runner.logs.append(log)
        with patch.object(runner, 'restore'), patch.object(runner.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1, b'', b'')):
            with self.assertRaisesRegex(RuntimeError, 'listener fault'):
                runner.cleanup()
        self.assertIsNotNone(child.poll())
        self.assertFalse(key.exists())
        log.close.assert_called_once()
        self.assertEqual(runner.children, [])

    def test_cli_reports_all_requested_suites_and_exits_nonzero(self):
        # A fake SDK fails installation: exercise real main/exit behavior without
        # starting fixtures, installing APKs or contacting a device.
        sdk = self.scratch / 'sdk'
        adb = sdk / 'platform-tools/adb'
        adb.parent.mkdir(parents=True)
        adb.write_text('#!/bin/sh\nif [ "$3" = emu ]; then echo pneu-test; exit 0; fi\nexit 1\n')
        adb.chmod(0o700)
        evidence = self.scratch / 'evidence'
        env = dict(os.environ, ANDROID_HOME=str(sdk), PYTHONDONTWRITEBYTECODE='1')
        result = subprocess.run([sys.executable, str(Path(runner.__file__)), 'local', 'cold',
                                 '--scratch', str(evidence)], env=env, timeout=15,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn('Suite summary:', result.stdout)
        self.assertIn('FAILED local', result.stdout)
        self.assertIn('FAILED cold', result.stdout)
        data = json.loads((evidence / 'results.json').read_text())
        self.assertEqual([s['status'] for s in data['suites']], ['failed', 'failed'])


class DeviceCoverageTest(unittest.TestCase):
    @staticmethod
    def methods(directory):
        discovered = {}
        for source in directory.rglob('*.kt'):
            text = source.read_text()
            # Tests are ordinary top-level Kotlin classes with JUnit annotations.
            # Ignore comments so examples cannot appear as executable tests.
            text = re.sub(r'/\*.*?\*/|//[^\n]*', '', text, flags=re.S)
            classes = list(re.finditer(r'^\s*(?:(?:internal|open|abstract)\s+)*class\s+(\w+)', text, re.M))
            seen = 0
            for index, match in enumerate(classes):
                end = classes[index + 1].start() if index + 1 < len(classes) else len(text)
                methods = set(re.findall(r'@(?:org\.junit\.)?Test\b(?:\([^)]*\))?\s*fun\s+(\w+)', text[match.end():end]))
                if methods:
                    discovered[match[1]] = methods
                    seen += len(methods)
            annotations = len(re.findall(r'@(?:org\.junit\.)?Test\b', text))
            if seen != annotations:
                raise AssertionError(f'Cannot inventory every @Test in {source}: {seen}/{annotations}')
        return discovered

    def assert_coverage(self, methods, selectors):
        selected = set()
        for selector in selectors:
            cls, separator, method = selector.partition('#')
            self.assertIn(cls, methods, 'Unknown test class selected: ' + selector)
            if separator:
                self.assertIn(method, methods[cls], 'Unknown test method selected: ' + selector)
                selected.add((cls, method))
            else:
                selected.update((cls, method) for method in methods[cls])
        missing = {(cls, method) for cls, names in methods.items() for method in names} - selected
        self.assertFalse(missing, 'Full runner omits: ' + ', '.join(cls + '#' + method for cls, method in sorted(missing)))

    def test_full_runner_selects_every_android_test_class_and_method(self):
        methods = self.methods(Path(runner.root) / 'app/src/androidTest')
        self.assertTrue(methods)
        self.assert_coverage(methods, runner.selected_device_tests())

    def test_coverage_rejects_new_unselected_classes_and_methods(self):
        methods = self.methods(Path(runner.root) / 'app/src/androidTest')
        methods['UnselectedNewTest'] = {'newScenario'}
        with self.assertRaisesRegex(AssertionError, 'UnselectedNewTest#newScenario'):
            self.assert_coverage(methods, runner.selected_device_tests())
        del methods['UnselectedNewTest']
        methods['ConnectionFaultDeviceTest'].add('unselectedNewPeerScenario')
        with self.assertRaisesRegex(AssertionError, 'ConnectionFaultDeviceTest#unselectedNewPeerScenario'):
            self.assert_coverage(methods, runner.selected_device_tests())


class EmulatorAuthorityTest(unittest.TestCase):
    def test_every_adb_call_requires_the_verified_serial_and_environment(self):
        serial = 'emulator-5580'
        def suite(*_):
            subprocess.run(['/sdk/adb', '-s', serial, 'shell', 'true'])
            for command in (['/sdk/adb', 'shell', 'true'], ['/sdk/adb', '-s', 'real-phone', 'shell', 'true'], ['/sdk/adb', '-s', 'emulator-5554', 'shell', 'true']):
                with self.assertRaisesRegex(RuntimeError, 'verified emulator'):
                    subprocess.run(command)
            os.environ['ANDROID_SERIAL'] = 'real-phone'
            with self.assertRaisesRegex(RuntimeError, 'ANDROID_SERIAL changed'):
                subprocess.run(['/sdk/adb', '-s', serial, 'shell', 'true'])
            os.environ['ANDROID_SERIAL'] = serial
            return True
        with tempfile.TemporaryDirectory() as scratch, patch.dict(os.environ), \
             patch.object(sys, 'argv', ['runner', 'local', '--serial', serial, '--scratch', scratch]), \
             patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='pneu-test\nOK\n')) as command, \
             patch.object(runner, 'run_suites', side_effect=suite), patch.object(runner, 'cleanup'):
            self.assertEqual(0, runner.main())
            self.assertEqual(2, command.call_count)

    def test_wrong_avd_is_rejected_before_install_or_cleanup(self):
        with patch.dict(os.environ), patch.object(sys, 'argv', ['runner', '--serial', 'emulator-5580']), \
             patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='other-avd\nOK\n')) as command, \
             patch.object(runner, 'run_suites') as suites, patch.object(runner, 'cleanup') as cleanup:
            with self.assertRaises(SystemExit) as result:
                runner.main()
            self.assertEqual(2, result.exception.code)
            self.assertEqual(1, command.call_count)
            suites.assert_not_called();cleanup.assert_not_called()

    def test_default_invocation_runs_all_suites_and_named_reruns_preserve_inventory(self):
        original = {name: list(cases) for name, cases in runner.SUITES.items()}
        for arguments in ([], ['local', '--cases', 'MultiComputerReviewDeviceTest']):
            with self.subTest(arguments=arguments), tempfile.TemporaryDirectory() as scratch, \
                 patch.dict(os.environ), patch.object(sys, 'argv', ['runner', '--serial', 'emulator-5580', '--scratch', scratch, *arguments]), \
                 patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='pneu-test\nOK\n')), \
                 patch.object(runner, 'run_suites', return_value=True) as suites, patch.object(runner, 'cleanup'):
                self.assertEqual(0, runner.main())
                selected, _, cases = suites.call_args.args
                if arguments:
                    self.assertEqual(['local'], selected)
                    self.assertEqual(['MultiComputerReviewDeviceTest'], [case.name for case in cases['local']])
                else:
                    self.assertEqual(list(original), selected)
                    self.assertIsNone(cases)
                self.assertEqual(original, runner.SUITES)

    def test_real_phone_serial_is_rejected_before_any_command(self):
        with patch.object(sys, 'argv', ['runner', 'local', '--serial', 'real-phone']), patch.object(subprocess, 'run') as command:
            with self.assertRaises(SystemExit) as result:
                runner.main()
            self.assertEqual(2, result.exception.code)
            command.assert_not_called()
