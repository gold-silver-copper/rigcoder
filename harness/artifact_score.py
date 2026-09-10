"""Trusted output-only scoring; never extract archives or execute submissions.

For line-membership tasks only. The controller must capture from a stopped
candidate and keep the expected value and this scorer outside candidate access.
This module is not yet connected to the benchmark runner.
"""
import tarfile

MAX_ARCHIVE = 1_048_576
MAX_ARTIFACT = 65_536


def score_line(archive, filename, expected):
    """Return a binary reward for one regular UTF-8 artifact containing a line.

    Malformed/unsupported archives raise ValueError, so transport corruption
    cannot silently become an ordinary task failure. Missing/wrong output is
    represented by a valid archive with no file or by nonmatching file content.
    No tar member is ever written to the filesystem.
    """
    if (not isinstance(filename, str) or not filename or filename in (".", "..")
            or "/" in filename or "\\" in filename or "\x00" in filename):
        raise ValueError("expected a single artifact filename")
    if not isinstance(expected, str) or not expected or "\n" in expected or "\r" in expected:
        raise ValueError("expected value must be a nonempty single line")
    if (not isinstance(archive, bytes) or not 1024 <= len(archive) <= MAX_ARCHIVE
            or len(archive) % 512):
        raise ValueError("invalid archive size")
    if archive[:512] == bytes(512):
        if any(archive):
            raise ValueError("data follows archive end")
        return 0.0
    try:
        # Decode one physical header only: the streaming tar reader silently
        # consumes extension headers and tolerates missing end markers.
        first = tarfile.TarInfo.frombuf(archive[:512], "utf-8", "strict")
    except (tarfile.TarError, UnicodeError, ValueError) as error:
        raise ValueError("invalid artifact header") from error
    if (first.name != filename or first.type not in (tarfile.REGTYPE, tarfile.AREGTYPE)
            or not 0 <= first.size <= MAX_ARTIFACT):
        raise ValueError("unsupported artifact entry")
    end = 512 + first.size
    padded_end = 512 + ((first.size + 511) // 512) * 512
    # Require body padding, two zero end blocks, and only zero record padding.
    # Any second member, concatenated archive or extension header is rejected.
    if len(archive) < padded_end + 1024 or any(archive[end:]):
        raise ValueError("incomplete or multiple artifacts")
    body = archive[512:end]
    try:
        # Match the development verifier's read_text().strip().split("\n")
        # semantics, including universal newline handling.
        text = body.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
    except UnicodeDecodeError:
        return 0.0
    return float(expected in text.strip().split("\n"))
