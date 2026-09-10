"""macOS isolation for a prompt-only improver (runner integration pending).

Only a disposable workspace, runtime files, and one loopback gateway are
accessible. No process forks are permitted; Rust-edit/compile lanes need a
separate boundary. The launcher must clear the environment and own the workspace.
"""
import json
from pathlib import Path
import sys


def command(executable, workspace, gateway_port):
    if sys.platform != "darwin":
        raise ValueError("this improver sandbox requires macOS")
    executable = Path(executable).resolve(strict=True)
    workspace = Path(workspace).resolve(strict=True)
    if not executable.is_file() or not workspace.is_dir():
        raise ValueError("invalid sandbox inputs")
    if type(gateway_port) is not int or not 1 <= gateway_port <= 65535:
        raise ValueError("invalid gateway port")
    quote = lambda path: json.dumps(str(path))
    profile = '''(version 1)
(deny default)
(allow file-read-metadata)
(allow file-read* (literal "/") (literal "/dev/null") (literal "/dev/random") (literal "/dev/urandom") (literal "/private/etc/ssl/openssl.cnf"))
(allow process-info* (target same-sandbox))
(allow signal (target same-sandbox))
(allow sysctl-read (sysctl-name-prefix "hw.") (sysctl-name-prefix "kern.os") (sysctl-name "kern.usrstack64") (sysctl-name "kern.argmax") (sysctl-name "kern.maxfilesperproc") (sysctl-name "kern.hostname") (sysctl-name "kern.version") (sysctl-name "sysctl.proc_cputype"))
'''
    for path in ("/System/Library", "/usr/bin", "/usr/lib", "/usr/share", "/bin", workspace):
        profile += f"(allow file-read* (subpath {quote(path)}))\n"
    profile += f"(allow file-read* (literal {quote(executable)}))\n"
    profile += f"(allow file-write* (subpath {quote(workspace)}) (literal \"/dev/null\"))\n"
    profile += f"(allow process-exec (literal {quote(executable)}))\n"
    profile += f'(allow network-outbound (remote ip "localhost:{gateway_port}"))\n'
    return ["/usr/bin/sandbox-exec", "-p", profile, str(executable)]
