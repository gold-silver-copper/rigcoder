import io
import tarfile
import unittest

from artifact_score import MAX_ARCHIVE, MAX_ARTIFACT, score_line


def archive(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, body, kind in entries:
            info = tarfile.TarInfo(name)
            info.type = kind
            info.size = len(body)
            if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
                info.linkname = "/private/hidden-canary"
            tar.addfile(info, io.BytesIO(body))
    return output.getvalue()


class ArtifactTests(unittest.TestCase):
    def test_line_membership_is_scored_as_data(self):
        for body, expected in [(b"wrong\nSYNTHETIC\n", 1.0),
                               (b" SYNTHETIC \r\n", 1.0),
                               (b"SYNTHETIC-extra", 0.0), (b"\xff", 0.0),
                               (b"", 0.0)]:
            self.assertEqual(score_line(archive([("answer.txt", body, tarfile.REGTYPE)]),
                                        "answer.txt", "SYNTHETIC"), expected)
        self.assertEqual(score_line(archive([]), "answer.txt", "SYNTHETIC"), 0.0)

    def test_physical_framing_and_extension_headers_are_rejected(self):
        valid = archive([("answer.txt", b"SYNTHETIC", tarfile.REGTYPE)])
        for invalid in (valid[:1024], valid + valid, valid[:-1],
                        bytes(1024) + valid):
            with self.assertRaises(ValueError):
                score_line(invalid, "answer.txt", "SYNTHETIC")
        for kind in (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.GNUTYPE_LONGNAME,
                     tarfile.GNUTYPE_LONGLINK, tarfile.GNUTYPE_SPARSE):
            with self.assertRaises(ValueError):
                score_line(archive([("answer.txt", b"", kind)]) + valid,
                           "answer.txt", "SYNTHETIC")

    def test_unsafe_entries_and_resource_excess_are_rejected(self):
        for name, body, kind in [("../answer.txt", b"SYNTHETIC", tarfile.REGTYPE),
                                  ("answer.txt", b"", tarfile.SYMTYPE),
                                  ("answer.txt", b"", tarfile.LNKTYPE),
                                  ("answer.txt", b"", tarfile.FIFOTYPE),
                                  ("answer.txt", b"x" * (MAX_ARTIFACT + 1), tarfile.REGTYPE)]:
            with self.assertRaises(ValueError):
                score_line(archive([(name, body, kind)]), "answer.txt", "SYNTHETIC")
        with self.assertRaises(ValueError):
            score_line(b"x" * (MAX_ARCHIVE + 1), "answer.txt", "SYNTHETIC")
        with self.assertRaises(ValueError):
            score_line(archive([("answer.txt", b"SYNTHETIC", tarfile.REGTYPE)] * 2),
                       "answer.txt", "SYNTHETIC")
        with self.assertRaises(ValueError):
            score_line(b"corrupt", "answer.txt", "SYNTHETIC")


if __name__ == "__main__":
    unittest.main()
