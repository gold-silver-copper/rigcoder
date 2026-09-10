from pathlib import Path
import os
import tempfile
import unittest

from prompt_workspace import MAX_PROMPT, PROMPT, PromptWorkspace


class WorkspaceTests(unittest.TestCase):
    def test_only_explicit_inputs_are_copied_and_only_prompt_and_note_collected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "hidden").write_bytes(b"SEALED")
            workspace = PromptWorkspace.create(root / "workspace", b"baseline", b"development evidence")
            files = {p.relative_to(workspace.path) for p in workspace.path.rglob("*") if p.is_file()}
            self.assertEqual(files, {PROMPT, Path("development.md")})
            (workspace.path / PROMPT).write_bytes(b"candidate")
            (workspace.path / "improvement.md").write_bytes(b"hypothesis")
            (workspace.path / "unapproved").write_bytes(b"not copied back")
            self.assertEqual(workspace.collect(), {"prompt": b"candidate", "note": b"hypothesis"})
            self.assertEqual((root / "hidden").read_bytes(), b"SEALED")
            with self.assertRaises(FileExistsError):
                PromptWorkspace.create(workspace.path, b"new", b"new")
            self.assertEqual((workspace.path / PROMPT).read_bytes(), b"candidate")

    def test_linked_special_and_oversized_proposals_are_rejected(self):
        for kind in ("symlink", "hardlink", "fifo", "oversized", "parent"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                workspace = PromptWorkspace.create(root / "workspace", b"baseline", b"report")
                target = workspace.path / PROMPT
                target.unlink()
                hidden = root / "hidden"
                hidden.write_bytes(b"SEALED")
                if kind == "symlink":
                    target.symlink_to(hidden)
                elif kind == "hardlink":
                    os.link(hidden, target)
                elif kind == "fifo":
                    os.mkfifo(target)
                elif kind == "oversized":
                    target.write_bytes(b"x" * (MAX_PROMPT + 1))
                else:
                    target.parent.rmdir()
                    target.parent.symlink_to(root, target_is_directory=True)
                    (root / "prompt.md").write_bytes(b"SEALED")
                with self.assertRaises((OSError, ValueError)):
                    workspace.collect()


if __name__ == "__main__":
    unittest.main()
