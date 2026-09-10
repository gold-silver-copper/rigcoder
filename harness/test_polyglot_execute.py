import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from polyglot_execute import CASES, copy_binary, evaluate_source, execute

IMAGE = 'sha256:' + 'a' * 64


class PolyglotExecuteTests(unittest.TestCase):
    def test_engine_exit_is_required_and_cleanup_always_runs(self):
        for mismatched in (False, True):
            commands = []

            def docker(args, _limit, _timeout, **kwargs):
                commands.append(args)
                if args[1] == 'wait':
                    return b'1\n'
                if args[1] == 'inspect':
                    return json.dumps({'Status': 'exited', 'Running': False, 'Error': '',
                                       'ExitCode': 0 if mismatched else 1, 'OOMKilled': False}).encode()
                if args[1] == 'logs':
                    self.assertTrue(kwargs['include_stderr'])
                    return b'candidate diagnostic', b'compiler stderr'
                return b''

            with patch('polyglot_execute.bounded_command', docker):
                if mismatched:
                    with self.assertRaises(ValueError):
                        execute(IMAGE, ['rustc', '/input/main.rs'], [], 10)
                else:
                    result = execute(IMAGE, ['rustc', '/input/main.rs'], [], 10)
                    self.assertEqual(result['exit_code'], 1)
            self.assertEqual(commands[-1][1:3], ['rm', '-f'])
            command = commands[0]
            self.assertIn('--read-only', command)
            self.assertEqual(command[command.index('--network') + 1], 'none')
            self.assertEqual(command[command.index('--user') + 1], '65534:65534')

    def test_functional_outputs_scored_on_host_with_fresh_runs(self):
        for mode in ('correct', 'unicode_space', 'wrong', 'invalid_utf8'):
            wrong = mode in ('wrong', 'invalid_utf8')
            calls = []

            def run(image, args, mounts, timeout):
                calls.append((args, mounts))
                self.assertEqual(image, IMAGE)
                self.assertGreater(timeout, 0)
                if args[0] in ('rustc', 'g++'):
                    self.assertEqual([m[1] for m in mounts], ['/input/main.rs', '/output'])
                    (mounts[1][0] / 'program').write_bytes(b'synthetic executable')
                    output = b''
                else:
                    self.assertEqual(len(mounts), 1)
                    self.assertEqual(mounts[0][1:], ('/submission', True))
                    output = b'wrong' if wrong else dict(CASES)[int(args[1])]
                    if mode == 'unicode_space':
                        output = '\u00a0'.encode() + output + '\u00a0\r\n'.encode()
                    elif mode == 'invalid_utf8':
                        output = b'\xff'
                return {'exit_code': 0, 'oom_killed': False, 'stdout': output}

            with patch('polyglot_execute.execute', run):
                reward, evidence = evaluate_source(b'synthetic source', IMAGE)
            self.assertEqual(reward, 0.0 if wrong else 1.0)
            self.assertEqual(len(calls), 2 if wrong else 12)
            self.assertEqual(len(evidence), len(calls))

    def test_compilation_failure_does_not_execute_a_binary(self):
        result = {'exit_code': 1, 'oom_killed': False, 'stdout': b'compile error'}
        with patch('polyglot_execute.execute', return_value=result) as run:
            reward, _ = evaluate_source(b'synthetic invalid source', IMAGE)
        self.assertEqual(reward, 0.0)
        self.assertEqual(run.call_count, 1)

    def test_output_links_and_fifos_cannot_become_copied_binaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            canary = root / 'canary'
            canary.write_bytes(b'host canary')
            source = root / 'program'
            source.symlink_to(canary)
            with self.assertRaises(OSError):
                copy_binary(source, root / 'copied')
            source.unlink()
            os.link(canary, source)
            with self.assertRaises(ValueError):
                copy_binary(source, root / 'copied')
            source.unlink()
            os.mkfifo(source)
            with self.assertRaises(ValueError):
                copy_binary(source, root / 'copied')
            self.assertEqual(canary.read_bytes(), b'host canary')
            self.assertFalse((root / 'copied').exists())


if __name__ == '__main__':
    unittest.main()
