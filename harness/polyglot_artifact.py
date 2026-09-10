"""Read a stopped-container polyglot directory as data; never extract tar entries.

This is a prerequisite for trusted functional scoring, not a complete scorer.
None denotes a valid archive with an invalid single-source deliverable. Malformed
or unsupported archive framing raises ValueError and invalidates the trial.
"""
import tarfile

MAX_DIRECTORY_ARCHIVE = 64 * 1024 * 1024
MAX_SOURCE = 65_536
MAX_ENTRIES = 1024


def source_from_archive(archive):
    if archive is None:
        return None
    if (not isinstance(archive, bytes) or not 1024 <= len(archive) <= MAX_DIRECTORY_ARCHIVE
            or len(archive) % 512):
        raise ValueError("invalid polyglot archive size")
    offset = 0
    entries = 0
    names = set()
    source = None
    valid = True
    root_seen = False
    while offset + 512 <= len(archive):
        header = archive[offset:offset + 512]
        if header == bytes(512):
            if len(archive) - offset < 1024 or any(archive[offset:]):
                raise ValueError("invalid polyglot archive end")
            return source if valid and root_seen else None
        entries += 1
        if entries > MAX_ENTRIES:
            raise ValueError("polyglot archive has too many entries")
        try:
            info = tarfile.TarInfo.frombuf(header, "utf-8", "strict")
        except (tarfile.TarError, UnicodeError, ValueError) as error:
            raise ValueError("invalid polyglot archive header") from error
        # No PAX/GNU overrides or sparse interpretation. Inspect each physical
        # header and its bounded body instead of tarfile's extension processing.
        if info.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE,
                             tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE,
                             tarfile.CHRTYPE, tarfile.BLKTYPE):
            raise ValueError("unsupported polyglot archive entry")
        if info.size < 0 or info.size > MAX_DIRECTORY_ARCHIVE:
            raise ValueError("invalid polyglot archive member size")
        if info.type not in (tarfile.REGTYPE, tarfile.AREGTYPE) and info.size:
            raise ValueError("non-file archive member has data")
        begin = offset + 512
        end = begin + info.size
        next_offset = begin + ((info.size + 511) // 512) * 512
        if next_offset + 1024 > len(archive) or any(archive[end:next_offset]):
            raise ValueError("truncated polyglot archive member")
        name = info.name.rstrip("/") if info.type == tarfile.DIRTYPE else info.name
        if name in names:
            raise ValueError("duplicate polyglot archive member")
        names.add(name)
        if name == "polyglot" and info.type == tarfile.DIRTYPE:
            root_seen = True
        elif name == "polyglot/main.rs" and info.type in (tarfile.REGTYPE, tarfile.AREGTYPE):
            if info.size > MAX_SOURCE:
                valid = False
            else:
                source = archive[begin:end]
        else:
            # Extra compiled binaries, directories, links and special files
            # fail the deliverable contract; no path is resolved on the host.
            valid = False
        offset = next_offset
    raise ValueError("polyglot archive lacks end markers")
