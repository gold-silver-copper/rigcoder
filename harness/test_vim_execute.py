import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from vim_execute import evaluate_macros, output_digest
from check_vim_docker import SOURCE


class VimOutputTests(unittest.TestCase):
    def test_scorer_oom_invalidates_instead_of_publishing_a_zero(self):
        events = []
        with patch('vim_execute.execute', side_effect=[
                {'exit_code': 0, 'oom_killed': False, 'stdout': b'', 'stderr': b''},
                {'exit_code': 137, 'oom_killed': True, 'stdout': b'', 'stderr': b''}]), \
             patch('vim_execute.read_regular', return_value=b'{"registers":["a","b","c"],"counts":[1,2,3]}'):
            with self.assertRaisesRegex(ValueError, 'memory'):
                evaluate_macros(SOURCE, b'x', 'a'*64, 'sha256:'+'b'*64, 30, record=events.append)
        self.assertEqual(len(events), 2)
        self.assertTrue(events[-1]['oom_killed'])

    def test_output_must_be_a_regular_single_link_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root/'output'
            self.assertIsNone(output_digest(output))
            output.write_bytes(b'synthetic')
            self.assertEqual(output_digest(output), hashlib.sha256(b'synthetic').hexdigest())
            link = root/'link'
            link.symlink_to(output)
            self.assertIsNone(output_digest(link))
            link.unlink()
            link.hardlink_to(output)
            self.assertIsNone(output_digest(output))
            self.assertIsNone(output_digest(link))
            self.assertIsNone(output_digest(root))


if __name__ == '__main__':
    unittest.main()
