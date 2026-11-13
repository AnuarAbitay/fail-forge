"""Regression guards for the real SDK gate barrier's machine capture evidence."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
SOURCE = Path(__file__).with_name('sdk_smoke_runner.py')
spec = importlib.util.spec_from_file_location('sdk_smoke_runner', SOURCE)
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)


class AllAdmittedCaptureTests(unittest.TestCase):
    def setUp(self):
        self.marker = {
            'kind': 'failforge_gate_barrier_observed',
            'nonce': 'f00dbabe' * 4,
            'family': 'classic',
            'runId': 'isolated-campaign-run',
            'sequence': 15,
            'killed': 0,
            'survived': 1,
            'remaining': 1,
            'minScore': 80,
            'maxPossiblePercent': 50,
        }
        self.events = [
            {'protocolVersion': '0.1.1', 'runnerVersion': '1.33.0',
             'pid': 700, 'type': 'start', 'time': 0},
            {'suite': {'id': 0, 'platform': 'vm', 'path': 'test/value_test.dart'},
             'type': 'suite', 'time': 0},
            {'test': {'id': 1, 'name': 'loading test/value_test.dart',
                      'suiteID': 0, 'groupIDs': [],
                      'metadata': {'skip': False, 'skipReason': None}},
             'type': 'testStart', 'time': 1},
            {'testID': 1, 'result': 'success', 'skipped': False,
             'hidden': True, 'type': 'testDone', 'time': 10},
            {'test': {'id': 3, 'name': 'enabled', 'suiteID': 0,
                      'groupIDs': [2],
                      'metadata': {'skip': False, 'skipReason': None}},
             'type': 'testStart', 'time': 11},
            {'testID': 3, 'message': json.dumps(self.marker),
             'type': 'print', 'time': 20},
            {'testID': 3, 'error': 'Expected: true\n  Actual: <false>\n',
             'stackTrace': 'package:matcher expect\ntest/value_test.dart main.<fn>\n',
             'isFailure': True, 'type': 'error', 'time': 21},
            {'testID': 3, 'result': 'failure', 'skipped': False,
             'hidden': False, 'type': 'testDone', 'time': 22},
            {'success': False, 'type': 'done', 'time': 23},
        ]

    def stdout(self, events=None, separator='\n'):
        return (separator.join(json.dumps(row, ensure_ascii=False)
                               for row in (self.events if events is None else events))
                + separator).encode('utf-8')

    def validate(self, events=None, stderr=b'', marker=None):
        return module.validate_all_admitted_capture(
            self.stdout(events), stderr, self.marker if marker is None else marker)

    def assert_rejected(self, events=None, stderr=b'', marker=None):
        with self.assertRaises(RuntimeError):
            self.validate(events, stderr, marker)

    def test_accepts_expected_assertion_after_same_test_observes_exact_proof(self):
        result = self.validate()
        self.assertEqual(result, {
            'barrierObserved': True,
            'assertionFailureConfirmed': True,
            'testId': 3,
            'barrierMarker': self.marker,
        })

    def test_accepts_native_windows_crlf_machine_records(self):
        result = module.validate_all_admitted_capture(
            self.stdout(separator='\r\n'), b'', self.marker)
        self.assertTrue(result['assertionFailureConfirmed'])
        self.assertEqual(result['barrierMarker'], self.marker)

    def test_rejects_historical_windows_journal_sharing_failure(self):
        # The original P2 had this exact type/isFailure/testDone combination.
        # The private host path and localized OS text are intentionally removed.
        historical = copy.deepcopy(self.events)
        del historical[5]
        historical[5].update({
            'error': "PathAccessException: Cannot open file, path = 'events.jsonl' "
                     '(OS Error: The process cannot access the file because it is '
                     'being used by another process, errno = 32)',
            'stackTrace': 'dart:io _File.readAsStringSync\n'
                          'test\\value_test.dart 13:39 main.<fn>\n',
            'isFailure': False,
        })
        historical[6]['result'] = 'error'
        self.assert_rejected(historical)

    def test_rejects_sharing_error_even_when_a_valid_marker_was_printed(self):
        self.events[6]['error'] = 'PathAccessException: events.jsonl (errno = 32)'
        self.events[6]['isFailure'] = False
        self.events[7]['result'] = 'error'
        self.assert_rejected()

    def test_rejects_barrier_timeout_even_when_a_valid_marker_was_printed(self):
        self.events[6]['error'] = 'Bad state: Expected gate proof was not published'
        self.events[6]['isFailure'] = False
        self.events[7]['result'] = 'error'
        self.assert_rejected()

    def test_rejects_expected_assertion_without_a_barrier_marker(self):
        del self.events[5]
        self.assert_rejected()

    def test_rejects_marker_for_another_run_nonce(self):
        wrong = dict(self.marker, nonce='bad0cafe' * 4)
        self.events[5]['message'] = json.dumps(wrong)
        self.assert_rejected()

    def test_rejects_marker_with_different_journal_identity_or_gate_proof(self):
        for field, value in [('runId', 'another-run'), ('sequence', 16),
                             ('family', 'other'), ('killed', 1), ('survived', 0),
                             ('remaining', 0), ('minScore', 0),
                             ('maxPossiblePercent', 100)]:
            with self.subTest(field=field):
                changed = copy.deepcopy(self.events)
                changed[5]['message'] = json.dumps(dict(self.marker, **{field: value}))
                self.assert_rejected(changed)

    def test_rejects_assertion_before_barrier_observation(self):
        self.events[5], self.events[6] = self.events[6], self.events[5]
        self.assert_rejected()

    def test_rejects_marker_after_test_completion(self):
        self.events.insert(8, self.events.pop(5))
        self.assert_rejected()

    def test_rejects_marker_attributed_to_another_test(self):
        self.events[5]['testID'] = 1
        self.assert_rejected()

    def test_rejects_expected_failure_attributed_to_another_test(self):
        self.events[6]['testID'] = 1
        self.assert_rejected()

    def test_rejects_duplicate_barrier_markers(self):
        self.events.insert(6, copy.deepcopy(self.events[5]))
        self.assert_rejected()

    def test_rejects_unexpected_error_in_another_test(self):
        self.events.insert(7, {
            'testID': 1, 'error': 'PathAccessException: unrelated setup failed',
            'isFailure': False, 'type': 'error', 'time': 21,
        })
        self.assert_rejected()

    def test_rejects_wrong_assertion_despite_failure_classification(self):
        self.events[6]['error'] = 'Expected: true\n  Actual: <null>\n'
        self.assert_rejected()

    def test_rejects_runtime_error_with_the_expected_assertion_text(self):
        self.events[6]['isFailure'] = False
        self.events[7]['result'] = 'error'
        self.assert_rejected()

    def test_rejects_failed_skipped_or_hidden_completion(self):
        for changed in [{'result': 'error'}, {'result': 'success'},
                        {'skipped': True}, {'hidden': True}]:
            with self.subTest(changed=changed):
                events = copy.deepcopy(self.events)
                events[7].update(changed)
                self.assert_rejected(events)

    def test_rejects_missing_or_successful_terminal_result(self):
        self.assert_rejected(self.events[:-1])
        self.events[-1]['success'] = True
        self.assert_rejected()

    def test_rejects_enabled_test_that_was_not_started(self):
        del self.events[4]
        self.assert_rejected()

    def test_rejects_another_test_name_reusing_the_enabled_test_id(self):
        self.events[4]['test']['name'] = 'another assertion'
        self.assert_rejected()

    def test_rejects_stderr_instead_of_silently_accepting_extra_failures(self):
        self.assert_rejected(stderr=b'Unhandled exception: access denied\n')

    def test_rejects_empty_truncated_or_malformed_machine_capture(self):
        for raw in [b'', self.stdout()[:-5], b'{not-json}\n']:
            with self.subTest(raw=raw[:30]):
                with self.assertRaises(RuntimeError):
                    module.validate_all_admitted_capture(raw, b'', self.marker)


class AllAdmittedReportTests(unittest.TestCase):
    def setUp(self):
        AllAdmittedCaptureTests.setUp(self)
        temporary = tempfile.TemporaryDirectory(prefix='failforge-sdk-gate-evidence-')
        self.addCleanup(temporary.cleanup)
        self.report = Path(temporary.name) / 'report'
        self.mutant_id = 'a' * 64
        self.capture = self.report / 'mutants' / self.mutant_id / 'capture'
        self.capture.mkdir(parents=True)
        stdout = AllAdmittedCaptureTests.stdout(self)
        (self.capture / 'stdout.bin').write_bytes(stdout)
        (self.capture / 'stderr.bin').write_bytes(b'')
        self.summary = {
            'runId': self.marker['runId'],
            'records': [
                {'mutantId': self.mutant_id, 'outcome': 'KILLED',
                 'stdoutSha256': hashlib.sha256(stdout).hexdigest()},
                {'mutantId': 'b' * 64, 'outcome': 'SURVIVED'},
            ],
            'qualityGate': {
                'status': 'failed', 'minScore': 80, 'maxPossiblePercent': 50,
                'proof': {'killed': 0, 'survived': 1, 'remaining': 1,
                          'minScore': 80, 'maxPossiblePercent': 50},
            },
        }
        self.journal_event = {
            key: value for key, value in self.marker.items()
            if key not in ('kind', 'nonce')
        }
        self.journal_event.update(kind='quality_gate_stopped', version=1)
        self.write_journal([self.journal_event])

    def write_journal(self, events):
        (self.report / 'events.jsonl').write_text(
            ''.join(json.dumps(event) + '\n' for event in events), encoding='utf-8')

    def validate(self):
        return module.validate_all_admitted_report(
            self.summary, self.report, self.marker['nonce'])

    def assert_rejected(self):
        with self.assertRaises(RuntimeError):
            self.validate()

    def test_accepts_one_sha_bound_killed_capture_and_matching_journal_proof(self):
        proof = self.validate()
        self.assertTrue(proof['barrierObserved'])
        self.assertTrue(proof['assertionFailureConfirmed'])
        self.assertEqual(proof['mutantId'], self.mutant_id)
        self.assertEqual(proof['journalSequence'], 15)
        self.assertEqual(proof['barrierMarker'], self.marker)

    def test_rejects_report_without_a_killed_mutant(self):
        self.summary['records'][0]['outcome'] = 'SURVIVED'
        self.assert_rejected()

    def test_rejects_report_with_two_killed_mutants(self):
        self.summary['records'][1]['outcome'] = 'KILLED'
        self.assert_rejected()

    def test_rejects_incorrect_record_count(self):
        self.summary['records'].pop()
        self.assert_rejected()

    def test_rejects_capture_hash_mismatch(self):
        self.summary['records'][0]['stdoutSha256'] = '0' * 64
        self.assert_rejected()

    def test_rejects_mutant_id_that_could_escape_capture_directory(self):
        self.summary['records'][0]['mutantId'] = '../../foreign-capture'
        self.assert_rejected()

    def test_rejects_a_journal_for_another_run(self):
        self.journal_event['runId'] = 'another-campaign'
        self.write_journal([self.journal_event])
        self.assert_rejected()

    def test_rejects_a_summary_for_another_run(self):
        self.summary['runId'] = 'another-campaign'
        self.assert_rejected()

    def test_rejects_missing_or_duplicate_stop_proof(self):
        for events in [[], [self.journal_event, self.journal_event]]:
            with self.subTest(events=events):
                self.write_journal(events)
                self.assert_rejected()

    def test_rejects_forged_summary_proof_despite_correct_capture_and_journal(self):
        for field, value in [('killed', 1), ('survived', 0), ('remaining', 0),
                             ('minScore', 0), ('maxPossiblePercent', 100)]:
            with self.subTest(field=field):
                original = copy.deepcopy(self.summary['qualityGate']['proof'])
                self.summary['qualityGate']['proof'][field] = value
                self.assert_rejected()
                self.summary['qualityGate']['proof'] = original

    def test_rejects_nonpositive_or_noninteger_journal_sequence(self):
        for sequence in [0, -1, True, 1.5, '15']:
            with self.subTest(sequence=sequence):
                self.journal_event['sequence'] = sequence
                self.write_journal([self.journal_event])
                self.assert_rejected()

    def test_rejects_journal_proof_with_wrong_family_or_expected_counts(self):
        for field, value in [('family', 'other'), ('killed', 1), ('survived', 0),
                             ('remaining', 0), ('minScore', 0),
                             ('maxPossiblePercent', 100)]:
            with self.subTest(field=field):
                original = self.journal_event[field]
                self.journal_event[field] = value
                self.write_journal([self.journal_event])
                self.assert_rejected()
                self.journal_event[field] = original

    def test_rejects_capture_marker_with_stale_nonce(self):
        self.marker['nonce'] = 'bad0cafe' * 4
        self.assert_rejected()

    def test_rejects_malformed_journal_instead_of_dropping_unknown_records(self):
        (self.report / 'events.jsonl').write_bytes(b'{incomplete\n')
        self.assert_rejected()

    def test_orchestration_rejects_historical_error_before_html_or_green_result(self):
        installation = self.report.parent / 'installation'
        adapter = installation / 'adapters/dart'
        adapter.mkdir(parents=True)
        (adapter / 'pubspec.yaml').write_text('name: failforge_dart_adapter\n')
        (adapter / 'pubspec.lock').write_text('# Frozen test-only lockfile.\n')
        output = self.report.parent / 'output'
        output.mkdir()
        temporary = output / 'temporary'
        temporary.mkdir()
        summary = copy.deepcopy(self.summary)
        summary.update({
            'status': 'completed', 'complete': True,
            'expectedMutants': 2, 'completedMutants': 2,
            'mutationScore': {'percent': 50.0, 'denominator': 2},
            'outcomeCounts': {'KILLED': 1, 'SURVIVED': 1},
        })
        summary['qualityGate'].update({
            'processedMutants': 2, 'notRunMutants': 0,
            'reason': 'threshold_unreachable',
        })
        broken = copy.deepcopy(self.events)
        del broken[5]
        broken[5].update({
            'error': 'PathAccessException: events.jsonl (errno = 32)',
            'stackTrace': 'dart:io _File.readAsStringSync\n'
                          'test\\value_test.dart 13:39 main.<fn>\n',
            'isFailure': False,
        })
        broken[6]['result'] = 'error'
        raw = AllAdmittedCaptureTests.stdout(self, broken)
        summary['records'][0]['stdoutSha256'] = hashlib.sha256(raw).hexdigest()
        journal_event = self.journal_event
        mutant_id = self.mutant_id

        class Harness:
            def __init__(self):
                self.calls = []

            def run(self, name, command, cwd, **options):
                self.calls.append(name)
                if name.endswith('-offline-pub-get'):
                    return
                if name == 'all-admitted-completed':
                    report = output / 'all-admitted-completed-report'
                    capture = report / 'mutants' / mutant_id / 'capture'
                    capture.mkdir(parents=True)
                    (capture / 'stdout.bin').write_bytes(raw)
                    (capture / 'stderr.bin').write_bytes(b'')
                    (report / 'summary.json').write_text(json.dumps(summary))
                    (report / 'events.jsonl').write_text(json.dumps(journal_event) + '\n')
                    return
                raise AssertionError('Invalid capture reached downstream step: ' + name)

        harness = Harness()
        # Cleanup has its own checks. This regression isolates whether actual
        # orchestration invokes capture validation before exporting success.
        with patch.object(module, 'cleanup_evidence') as cleanup:
            with self.assertRaisesRegex(RuntimeError, 'barrier|assertion|runtime'):
                module.real_gate_cases(
                    harness, installation, self.report.parent / 'sdk',
                    output, temporary, only_case='all-admitted-completed')
            cleanup.assert_called_once()
        self.assertEqual(harness.calls, [
            'two-admitted-mutants-offline-pub-get', 'all-admitted-completed'])
        self.assertFalse((output / 'all-admitted-completed.html').exists())
        self.assertFalse((output / 'real-sdk-gates.json').exists())


if __name__ == '__main__':
    unittest.main()
