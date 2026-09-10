"""Isolated compiler/program execution for the polyglot development task.

Call only with a trusted, already-built immutable task image and validated source
bytes. This module never executes a submission on the host. Integration with
archive capture and the benchmark controller is still pending.
"""
import json
import os
from pathlib import Path
import re
import stat
import tempfile
import time
import uuid

from artifact_capture import bounded_command
from polyglot_artifact import MAX_SOURCE

MAX_BINARY = 32 * 1024 * 1024
CASES = ((0, b"1"), (1, b"1"), (2, b"2"), (10, b"89"), (42, b"433494437"))


def execute(image, arguments, mounts, timeout, *, file_size_limit=MAX_BINARY, memory_mib=512):
    """Return Engine-confirmed exit status and bounded stdout of one fresh run."""
    if type(memory_mib) is not int or not 128 <= memory_mib <= 4096:
        raise ValueError("invalid isolated process memory limit")
    if type(file_size_limit) is not int or not 1 <= file_size_limit <= 128 * 1024 * 1024:
        raise ValueError("invalid isolated process file size limit")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        raise ValueError("functional scorer requires an immutable image ID")
    if not arguments or not all(isinstance(arg, str) and '\x00' not in arg for arg in arguments):
        raise ValueError("invalid functional scorer command")
    name = 'rigcoder-polyglot-score-' + uuid.uuid4().hex
    deadline = time.monotonic() + timeout

    def run(args, limit=65_536, *, include_stderr=False):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValueError("functional scorer deadline exceeded")
        if include_stderr:
            return bounded_command(['docker', *args], limit, remaining, include_stderr=True)
        return bounded_command(['docker', *args], limit, remaining)

    command = ['create', '--pull', 'never', '--name', name, '--network', 'none',
               '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
               '--user', '65534:65534', '--pids-limit', '128', '--memory', f'{memory_mib}m',
               '--cpus', '1', '--ulimit', f'fsize={file_size_limit}:{file_size_limit}',
               '--tmpfs', '/tmp:rw,nosuid,nodev,size=64m,mode=1777', '-w', '/tmp']
    for source, target, readonly in mounts:
        # Mount only host-created private inputs/outputs, never a repository.
        # Docker's --mount is comma-delimited; reject ambiguous host paths.
        source = str(Path(source).resolve(strict=True))
        if ',' in source or '\x00' in source or not target.startswith('/') or ',' in target:
            raise ValueError("invalid functional scorer mount")
        command += ['--mount', f'type=bind,source={source},target={target}' +
                    (',readonly' if readonly else '')]
    command += ['--entrypoint', arguments[0], image, *arguments[1:]]
    try:
        run(command, 4096)
        run(['start', name], 4096)
        waited = run(['wait', name], 4096).strip()
        state = json.loads(run(['inspect', '--format', '{{json .State}}', name]))
        code = state.get('ExitCode')
        if (state.get('Status') != 'exited' or state.get('Running') is not False
                or state.get('Error') != '' or type(code) is not int or not 0 <= code <= 255
                or waited != str(code).encode('ascii')):
            raise ValueError("functional scorer process state is incomplete")
        output, errors = run(['logs', name], include_stderr=True)
        return {'exit_code': code, 'oom_killed': state.get('OOMKilled'),
                'stdout': output, 'stderr': errors}
    finally:
        # Cleanup has its own small allowance after the execution deadline.
        # A failed cleanup invalidates this run instead of leaving an accepted score.
        bounded_command(['docker', 'rm', '-f', name], 4096, 30)


def copy_binary(source, destination):
    """Copy only a bounded regular binary; candidate links never reach host execution."""
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or not 0 < before.st_size <= MAX_BINARY:
            raise ValueError("invalid compiler output")
        with open(destination, 'xb') as output:
            remaining = before.st_size
            while remaining:
                data = os.read(fd, min(remaining, 65_536))
                if not data:
                    raise ValueError("compiler output was truncated")
                output.write(data)
                remaining -= len(data)
            if os.read(fd, 1):
                raise ValueError("compiler output grew during copy")
            after = os.fstat(fd)
            if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
                    after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                raise ValueError("compiler output changed during copy")
            os.fchmod(output.fileno(), 0o555)
    finally:
        os.close(fd)


def evaluate_source(source, image, timeout=300, *, record=None):
    if not isinstance(source, bytes) or len(source) > MAX_SOURCE:
        raise ValueError("invalid source bytes")
    deadline = time.monotonic() + timeout
    evidence = []
    with tempfile.TemporaryDirectory(prefix='rigcoder-polyglot-eval-') as directory:
        root = Path(directory)
        source_path = root / 'main.rs'
        source_path.write_bytes(source)
        source_path.chmod(0o444)
        for compiler, arguments in (
                ('rustc', ['rustc', '/input/main.rs', '-o', '/output/program']),
                ('g++', ['g++', '-x', 'c++', '/input/main.rs', '-o', '/output/program'])):
            output = root / compiler
            output.mkdir(mode=0o777)
            output.chmod(0o777)
            compilation = execute(image, arguments,
                                  [(source_path, '/input/main.rs', True), (output, '/output', False)],
                                  deadline - time.monotonic())
            evidence.append({'compiler': compiler, 'stage': 'compile', **compilation})
            if record is not None:
                record(evidence[-1])
            if compilation['exit_code'] != 0:
                return 0.0, evidence
            # Compiler container and descendants were removed before reading output.
            binary = root / (compiler + '.bin')
            copy_binary(output / 'program', binary)
            for argument, expected in CASES:
                result = execute(image, ['/submission', str(argument)],
                                 [(binary, '/submission', True)], deadline - time.monotonic())
                evidence.append({'compiler': compiler, 'stage': 'run', 'argument': argument, **result})
                if record is not None:
                    record(evidence[-1])
                try:
                    text = result['stdout'].decode('utf-8').replace('\r\n', '\n').replace('\r', '\n')
                except UnicodeDecodeError:
                    return 0.0, evidence
                if result['exit_code'] != 0 or text.strip() != expected.decode('ascii'):
                    return 0.0, evidence
    return 1.0, evidence
