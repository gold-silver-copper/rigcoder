"""Run validated macro scripts in fresh containers; all comparisons occur on host."""
import errno
import hashlib
import json
import os
from pathlib import Path
import stat
import tempfile
import time

from polyglot_execute import execute
from prompt_workspace import read_regular
from vim_script import inspection_script, parse_script, score_registers

MAX_CSV = 64 * 1024 * 1024


def output_digest(path):
    """Hash only a bounded regular output after its container has been removed."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except (FileNotFoundError, IsADirectoryError):
        return None
    except OSError as error:
        if error.errno == errno.ELOOP:
            return None
        raise
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > MAX_CSV:
            return None
        digest = hashlib.sha256()
        length = 0
        while chunk := os.read(fd, min(1024 * 1024, MAX_CSV + 1 - length)):
            length += len(chunk)
            if length > MAX_CSV:
                return None
            digest.update(chunk)
        after = os.fstat(fd)
        if (length != before.st_size or before.st_mtime_ns != after.st_mtime_ns
                or before.st_ctime_ns != after.st_ctime_ns):
            raise ValueError('Vim output changed after container removal')
        return digest.hexdigest()
    finally:
        os.close(fd)


def evaluate_macros(source, input_csv, expected_sha256, image, timeout, *, record=None):
    if not isinstance(input_csv, bytes) or len(input_csv) > MAX_CSV:
        raise ValueError('invalid captured CSV size')
    import re
    if not isinstance(expected_sha256, str) or not re.fullmatch('[0-9a-f]{64}', expected_sha256):
        raise ValueError('invalid expected CSV digest')
    definitions = parse_script(source)
    if definitions is None:
        return 0.0
    deadline = time.monotonic() + timeout
    with tempfile.TemporaryDirectory(prefix='rigcoder-vim-score-') as directory:
        root = Path(directory)
        inspect = root / 'inspect.vim'
        inspect.write_bytes(inspection_script(definitions))
        inspect.chmod(0o444)
        output = root / 'registers'
        output.mkdir()
        output.chmod(0o777)
        # No task data or expected output enters the register-counting process.
        result = execute(image, ['vim', '-Nu', 'NONE', '-n', '-Es', '-S', '/inspect.vim'],
                         [(inspect, '/inspect.vim', True), (output, '/output', False)],
                         deadline-time.monotonic())
        if record:
            record({'stage':'registers', **result})
        if result['oom_killed'] is not False:
            raise ValueError('Vim scorer memory status is incomplete or exhausted')
        if result['exit_code'] != 0:
            return 0.0
        registers = json.loads(read_regular(output, Path('registers.json'), 65_536))
        if score_registers(registers) == 0:
            return 0.0
        app = root / 'app'
        app.mkdir()
        app.chmod(0o777)
        (app/'input.csv').write_bytes(input_csv)
        (app/'input.csv').chmod(0o666)
        submission = root / 'apply_macros.vim'
        submission.write_bytes(source)
        submission.chmod(0o444)
        # The /app mount hides the image's public expected.csv. There are no
        # trusted scoring files in this container and no host evaluation mounts.
        result = execute(image, ['vim', '-Nu', 'NONE', '-n', '-Es', '/app/input.csv',
                                 '-S', '/app/apply_macros.vim'],
                         [(app, '/app', False), (submission, '/app/apply_macros.vim', True)],
                         deadline-time.monotonic(), file_size_limit=MAX_CSV, memory_mib=2048)
        if record:
            record({'stage':'transform', **result})
        if result['oom_killed'] is not False:
            raise ValueError('Vim scorer memory status is incomplete or exhausted')
        if result['exit_code'] != 0:
            return 0.0
        return float(output_digest(app/'input.csv') == expected_sha256)
