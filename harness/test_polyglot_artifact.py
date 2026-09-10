import unittest
import tarfile

from polyglot_artifact import MAX_SOURCE, source_from_archive
from test_artifact_score import archive


def directory(*extra):
    return archive([("polyglot", b"", tarfile.DIRTYPE),
                    ("polyglot/main.rs", b"synthetic source", tarfile.REGTYPE), *extra])


class PolyglotArtifactTests(unittest.TestCase):
    def test_source_is_data_and_extras_fail(self):
        self.assertEqual(source_from_archive(directory()), b"synthetic source")
        self.assertIsNone(source_from_archive(None))
        self.assertIsNone(source_from_archive(archive([])))
        self.assertIsNone(source_from_archive(archive([("polyglot", b"", tarfile.DIRTYPE)])))
        for extra in [("polyglot/main", b"compiled binary", tarfile.REGTYPE),
                      ("polyglot/cmain", b"compiled binary", tarfile.REGTYPE),
                      ("polyglot/extra", b"", tarfile.DIRTYPE),
                      ("../../host-canary", b"untrusted", tarfile.REGTYPE)]:
            self.assertIsNone(source_from_archive(directory(extra)))

    def test_links_and_special_files_are_never_resolved(self):
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE,
                     tarfile.CHRTYPE, tarfile.BLKTYPE, tarfile.DIRTYPE):
            data = archive([("polyglot", b"", tarfile.DIRTYPE),
                            ("polyglot/main.rs", b"", kind)])
            self.assertIsNone(source_from_archive(data))
        data = archive([("polyglot", b"", tarfile.SYMTYPE)])
        self.assertIsNone(source_from_archive(data))

    def test_malformed_framing_invalidates_instead_of_scoring(self):
        valid = directory()
        for data in (valid[:1024], valid[:-1], valid + valid, bytes(1024) + valid,
                     valid[:512] + bytes(512) + valid[1024:]):
            with self.assertRaises(ValueError):
                source_from_archive(data)
        for kind in (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.GNUTYPE_LONGNAME,
                     tarfile.GNUTYPE_LONGLINK, tarfile.GNUTYPE_SPARSE):
            with self.assertRaises(ValueError):
                source_from_archive(archive([("polyglot", b"", kind)]))
        with self.assertRaises(ValueError):
            source_from_archive(directory(("polyglot/main.rs", b"duplicate", tarfile.REGTYPE)))

    def test_source_limit_and_missing_directory(self):
        for entries in [[("polyglot/main.rs", b"source", tarfile.REGTYPE)],
                        [("polyglot", b"", tarfile.DIRTYPE),
                         ("polyglot/main.rs", b"x" * (MAX_SOURCE + 1), tarfile.REGTYPE)]]:
            self.assertIsNone(source_from_archive(archive(entries)))


if __name__ == "__main__":
    unittest.main()
