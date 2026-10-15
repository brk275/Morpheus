"""Pure/mock private package-log reduction; never executes an installed app."""
import io
import json
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

from scripts import package_smoke as M


def missing(name):
    return (b'/private/profile-value/morpheus-app: error while loading shared libraries: '
            + name + b': cannot open shared object file: No such file or directory\n')


class StartupDiagnosticTests(unittest.TestCase):
    def facts(self, raw, status=127):
        return M.startup_failure_diagnostics(io.BytesIO(raw), status)

    def test_only_literal_library_names_can_leave_the_private_log(self):
        for name in M.STARTUP_LIBRARY_NAMES:
            result = self.facts(missing(name.encode()))
            self.assertEqual(result, {'category': 'missing_shared_library', 'exit_status': 127,
                                      'missing_library': name})
        result = self.facts(missing(b'libprivate_payload.so'))
        self.assertEqual(result, {'category': 'missing_shared_library', 'exit_status': 127,
                                  'missing_library': None})
        self.assertNotIn('private', json.dumps(result))

    def test_status127_without_a_complete_loader_line_is_not_a_diagnosis(self):
        for raw in (b'', b'private data', b'error while loading shared libraries: libXi.so.6',
                    missing(b'libXi.so.6').replace(b'No such file or directory', b'Permission denied'),
                    missing(b'libXi.so.6') + b'private payload without another loader message'):
            result = self.facts(raw)
            if raw.startswith(b'/private/') and b'No such file or directory\n' in raw:
                self.assertEqual(result['category'], 'missing_shared_library')
            else:
                self.assertEqual(result['category'], 'unclassified')
            self.assertNotIn('payload', json.dumps(result))

    def test_other_statuses_do_not_claim_loader_failure_or_read_the_log(self):
        for status in (0, 1, -11, None, True, 2**32, 'private-status'):
            log = Mock()
            result = M.startup_failure_diagnostics(log, status)
            log.seek.assert_not_called()
            log.read.assert_not_called()
            self.assertEqual(result['category'], 'unclassified')
            self.assertEqual(result['exit_status'], status if type(status) is int and -128 <= status <= 255 else None)

    def test_multiple_loader_lines_are_ambiguous_even_for_allowed_names(self):
        result = self.facts(missing(b'libXi.so.6') + missing(b'libasound.so.2'))
        self.assertEqual(result['category'], 'ambiguous_loader_diagnostics')
        self.assertIsNone(result['missing_library'])

    def test_symbol_and_version_content_never_leaves_the_reducer(self):
        cases = ((b'/private/app: symbol lookup error: /private/module: undefined symbol: private_symbol\n',
                  'undefined_loader_symbol'),
                 (b"/private/app: /private/module: version `private_version' not found (required by /private/consumer)\n",
                  'missing_library_version'))
        for raw, category in cases:
            result = self.facts(raw)
            self.assertEqual(result['category'], category)
            self.assertIsNone(result['missing_library'])
            self.assertNotIn('private', json.dumps(result))

    def test_crlf_is_accepted_but_non_ascii_library_name_is_not(self):
        self.assertEqual(self.facts(missing(b'libXi.so.6').replace(b'\n', b'\r\n'))['missing_library'], 'libXi.so.6')
        self.assertEqual(self.facts(missing(b'lib\xff.so'))['category'], 'unclassified')

    def test_log_read_is_bounded_and_oversize_withholds_even_valid_prefix(self):
        log = Mock()
        log.read.return_value = missing(b'libXi.so.6') + b'x' * M.STARTUP_LOG_BOUND
        result = M.startup_failure_diagnostics(log, 127)
        log.read.assert_called_once_with(M.STARTUP_LOG_BOUND + 1)
        self.assertEqual(result['category'], 'private_log_exceeded_bound')
        self.assertIsNone(result['missing_library'])

    def test_unavailable_log_cannot_replace_the_original_failure(self):
        log = Mock()
        log.seek.side_effect = OSError('private path')
        self.assertEqual(M.startup_failure_diagnostics(log, 127),
                         {'category': 'private_log_unavailable', 'exit_status': 127, 'missing_library': None})

    def test_startup_reduction_preserves_the_original_failure_and_cleanup(self):
        process = Mock(pid=1234)
        def spawn(*args, **kwargs):
            self.assertTrue(kwargs['start_new_session'])
            self.assertEqual(args[0], ['/private/candidate.AppImage'])
            kwargs['stdout'].write(missing(b'libasound.so.2'))
            kwargs['stdout'].flush()
            return process
        with patch.object(M.sys, 'platform', 'linux'), patch.object(M, 'require_nonreaping_wait'), \
                patch.object(M.subprocess, 'Popen', side_effect=spawn) as start, \
                patch.object(M, 'process_status', return_value=127), \
                patch.object(M, 'stop_process') as stop, patch('builtins.print') as output:
            with self.assertRaisesRegex(RuntimeError, r'Installed app exited during startup \(status 127\)'):
                M.launch_smoke(Path('/private/candidate.AppImage'), 'x11')
        start.assert_called_once()
        stop.assert_called_once_with(process)
        record = json.loads(output.call_args.args[0])
        self.assertEqual(record, {'check': 'installed_startup', 'status': 'failed', 'app_log_public': False,
                                 'diagnostic': {'category': 'missing_shared_library', 'exit_status': 127,
                                                'missing_library': 'libasound.so.2'}})
        self.assertNotIn('/private/', output.call_args.args[0])

    def test_non_linux_failure_keeps_the_previous_public_schema(self):
        with patch.object(M.sys, 'platform', 'darwin'), patch.object(M, 'require_nonreaping_wait'), \
                patch.object(M.subprocess, 'Popen', return_value=Mock()), \
                patch.object(M, 'process_status', return_value=127), patch.object(M, 'stop_process'), \
                patch.object(M, 'startup_failure_diagnostics') as reduction, patch('builtins.print') as output:
            with self.assertRaises(RuntimeError):
                M.launch_smoke(Path('/private/app'))
        reduction.assert_not_called()
        self.assertEqual(json.loads(output.call_args.args[0]),
                         {'check': 'installed_startup', 'status': 'failed', 'app_log_public': False})


if __name__ == '__main__':
    unittest.main()
