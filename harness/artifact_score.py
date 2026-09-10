"""Trusted output-only scoring; never extract archives or execute submissions.

Supports line membership and exact stripped-text comparison. The controller must
capture from a stopped candidate and keep the expected value and this scorer
outside candidate access.
The benchmark embeds this module in the trusted host scorer.
"""
import tarfile

MAX_ARCHIVE = 1_048_576
MAX_ARTIFACT = 65_536


def read_artifact(archive, filename, *, max_artifact=MAX_ARTIFACT, max_archive=MAX_ARCHIVE):
    """Read one bounded physical tar member as bytes, without extraction."""
    if (not isinstance(filename, str) or not filename or filename in (".", "..")
            or "/" in filename or "\\" in filename or "\x00" in filename):
        raise ValueError("expected a single artifact filename")
    if (not isinstance(archive, bytes) or not 1024 <= len(archive) <= max_archive
            or len(archive) % 512):
        raise ValueError("invalid archive size")
    if archive[:512] == bytes(512):
        if any(archive):
            raise ValueError("data follows archive end")
        return None
    try:
        # Decode one physical header only: the streaming tar reader silently
        # consumes extension headers and tolerates missing end markers.
        first = tarfile.TarInfo.frombuf(archive[:512], "utf-8", "strict")
    except (tarfile.TarError, UnicodeError, ValueError) as error:
        raise ValueError("invalid artifact header") from error
    if (first.name != filename or first.type not in (tarfile.REGTYPE, tarfile.AREGTYPE)
            or not 0 <= first.size <= max_artifact):
        raise ValueError("unsupported artifact entry")
    end = 512 + first.size
    padded_end = 512 + ((first.size + 511) // 512) * 512
    # Require body padding, two zero end blocks, and only zero record padding.
    # Any second member, concatenated archive or extension header is rejected.
    if len(archive) < padded_end + 1024 or any(archive[end:]):
        raise ValueError("incomplete or multiple artifacts")
    return archive[512:end]


def score_line(archive, filename, expected, comparison="line_membership"):
    """Score captured UTF-8 text; malformed archives remain infrastructure errors."""
    if comparison not in ("line_membership", "exact_stripped_text"):
        raise ValueError("unsupported output comparison")
    if not isinstance(expected, str) or not expected or "\n" in expected or "\r" in expected:
        raise ValueError("expected value must be a nonempty single line")
    body = read_artifact(archive, filename)
    if body is None:
        return 0.0
    try:
        # Match the development verifier's read_text().strip().split("\n")
        # semantics, including universal newline handling.
        text = body.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
    except UnicodeDecodeError:
        return 0.0
    if comparison == "exact_stripped_text":
        return float(expected == text.strip())
    return float(expected in text.strip().split("\n"))
