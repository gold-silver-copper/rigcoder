"""Validate the declared Vim-macro command grammar before isolated execution.

This is not a sandbox. Macro keystrokes remain untrusted and must run in a fresh
restricted container with no oracle, verifier, credentials or host repository.
"""
import re

MAX_SCRIPT = 65_536
DEFINITION = re.compile(r'''^:?call setreg\((['"])([abc])\1,\s*(['"])([^'"]*)\3\)$''')
EXECUTION = re.compile(r'^:%normal! @([abc])$')


def valid_keystrokes(value):
    forbidden = [
        fr':(\d*|[\$%\+\-\.]|(\d+,\d*)|(\d*,\$))*{cmd}\b'
        for cmd in ('r', 'read', 'source', 'so', 'e', 'edit', 'sp', 'split',
                    'vs', 'vsplit', 'tabe', 'tabedit', 'new', 'vnew')
    ] + [r'system\(', r'glob\(', r'expand\(', r'executable\(', r'filereadable\(',
         r'function\s*!', r'let\s+', r'eval\s*\(', r':py\b', r':lua\b',
         r':perl\b', r':ruby\b', r':!\w']
    return not any(re.search(pattern, value, re.IGNORECASE) for pattern in forbidden)


def parse_script(source):
    """Return validated definition lines, or None for a failed task contract."""
    if not isinstance(source, bytes) or len(source) > MAX_SCRIPT:
        raise ValueError('invalid macro script size')
    # Match the original verifier's errors=ignore and universal-newline handling.
    text = source.decode('utf-8', errors='ignore').replace('\r\n', '\n').replace('\r', '\n')
    lines = [line.strip() for line in text.split('\n')
             if line.strip() and not line.strip().startswith('"')]
    definitions = []
    registers = set()
    executions = set()
    exits = False
    for line in lines:
        definition = DEFINITION.fullmatch(line)
        execution = EXECUTION.fullmatch(line)
        if definition:
            if not valid_keystrokes(definition[4]):
                return None
            registers.add(definition[2])
            definitions.append(line)
        elif execution:
            executions.add(execution[1])
        elif line in (':wq', ':x'):
            exits = True
        else:
            return None
    if registers != set('abc') or executions != set('abc') or not exits:
        return None
    return definitions


def inspection_script(definitions):
    """Load literal registers without executing macros; report native Vim counts.

    Only previously validated literal setreg calls are accepted. No submitted
    Ex commands run after the trusted register inspection code.
    """
    if not definitions or any(DEFINITION.fullmatch(line) is None for line in definitions):
        raise ValueError('invalid register definitions')
    return ('\n'.join(definitions) + r'''
function! RigCount(reg) abort
  let s = keytrans(getreg(a:reg))
  let s = substitute(s, '<[^>]\+>', "\x01", 'g')
  return strlen(s)
endfunction
let regs = [keytrans(getreg('a')), keytrans(getreg('b')), keytrans(getreg('c'))]
let counts = [RigCount('a'), RigCount('b'), RigCount('c')]
call writefile([json_encode({'registers': regs, 'counts': counts})], '/output/registers.json')
qa!
''').encode('utf-8')


def score_registers(record):
    if not isinstance(record, dict) or set(record) != {'registers', 'counts'}:
        raise ValueError('invalid Vim register evidence')
    registers, counts = record['registers'], record['counts']
    if (not isinstance(registers, list) or len(registers) != 3
            or not all(isinstance(x, str) for x in registers)
            or not isinstance(counts, list) or len(counts) != 3
            or not all(type(x) is int and x >= 0 for x in counts)):
        raise ValueError('invalid Vim register evidence')
    # Original verifier compares stripped keytrans representations.
    return float(all(x > 0 for x in counts) and sum(counts) < 200
                 and len({x.strip() for x in registers}) == 3)
