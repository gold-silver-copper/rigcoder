import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from polyglot_evaluate import evaluate
from test_polyglot_artifact import directory

IMAGE = 'sha256:' + 'a' * 64


class PolyglotEvaluateTests(unittest.TestCase):
    def test_capture_precedes_execution_and_partial_evidence_survives(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)

            def failed(source, image, timeout, *, record):
                self.assertTrue((root / 'polyglot-artifact.tar').exists())
                self.assertEqual(source, b'synthetic source')
                self.assertEqual(image, IMAGE)
                record({'stage': 'compile', 'stdout': b'\xff', 'exit_code': 0})
                raise ValueError('synthetic runtime infrastructure failure')

            with patch('polyglot_evaluate.capture', return_value=directory()), \
                    patch('polyglot_evaluate.evaluate_source', side_effect=failed):
                with self.assertRaises(ValueError):
                    evaluate('candidate', IMAGE, root, 20)
            event = json.loads((root / 'polyglot-executions.jsonl').read_text())
            self.assertEqual(event['stdout'], '/w==')
            self.assertFalse((root / 'polyglot-result.json').exists())

    def test_missing_artifact_scores_zero_without_execution(self):
        with tempfile.TemporaryDirectory() as tmp:
            with patch('polyglot_evaluate.capture', return_value=None), \
                    patch('polyglot_evaluate.evaluate_source') as run:
                self.assertEqual(evaluate('candidate', IMAGE, tmp, 20), 0.0)
                run.assert_not_called()
            result = json.loads((Path(tmp) / 'polyglot-result.json').read_text())
            self.assertTrue(result['artifact_missing'])
            self.assertIsNone(result['archive_sha256'])


if __name__ == '__main__':
    unittest.main()
