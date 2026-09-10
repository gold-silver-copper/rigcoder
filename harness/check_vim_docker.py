"""Real isolated Vim checks using synthetic content; no task answers or API calls."""
import argparse
import hashlib
import json
import subprocess
import tempfile
import uuid
from pathlib import Path
import sys

from vim_execute import evaluate_macros
from vim_evaluate import evaluate

SOURCE = b'''call setreg('a', "A1\\<Esc>")
call setreg('b', "A2\\<Esc>")
call setreg('c', "A3\\<Esc>")
:%normal! @a
:%normal! @b
:%normal! @c
:wq
'''


def check(image):
    for name, source, expected, reward in [
            ('correct', SOURCE, b'x123\n', 1),
            ('wrong', SOURCE, b'wrong\n', 0),
            ('duplicate', SOURCE.replace(b'A2', b'A1'), b'x113\n', 0),
            ('too_many', SOURCE.replace(b'A1', b'A'+b'1'*200), b'x123\n', 0),
            ('injection', SOURCE.replace(b'call setreg', b'!id | call setreg', 1), b'x123\n', 0)]:
        events = []
        actual = evaluate_macros(source, b'x\n', hashlib.sha256(expected).hexdigest(),
                                 image, 30, record=events.append)
        assert actual == reward, (name, actual, events)
        if name in ('correct', 'wrong'):
            assert [x['stage'] for x in events] == ['registers', 'transform']
        elif name in ('duplicate', 'too_many'):
            assert [x['stage'] for x in events] == ['registers']
        else:
            assert events == []
        print('PASS:', name)


def check_capture(image):
    for expected, reward in [(b'x123\n', 1), (b'wrong\n', 0)]:
        name = 'rigcoder-vim-capture-' + uuid.uuid4().hex
        with tempfile.TemporaryDirectory(prefix='rigcoder-vim-capture-') as directory:
            root = Path(directory)
            (root/'apply_macros.vim').write_bytes(SOURCE)
            (root/'input.csv').write_bytes(b'x\n')
            evidence = root/'evidence'
            evidence.mkdir()
            try:
                subprocess.run(['docker','run','-d','--name',name,'--network','none',
                                '--cap-drop','ALL','--security-opt','no-new-privileges',
                                image,'sleep','infinity'],check=True,stdout=subprocess.DEVNULL,timeout=30)
                for artifact in ('apply_macros.vim','input.csv'):
                    subprocess.run(['docker','cp',str(root/artifact),name+':/app/'+artifact],check=True,timeout=30)
                actual = evaluate(name,image,{'expected_sha256':hashlib.sha256(expected).hexdigest()},evidence,30)
                assert actual == reward
                result = json.loads((evidence/'vim-result.json').read_text())
                assert all(result['archive_sha256'].values())
                assert result['image'] == image
                assert len((evidence/'vim-executions.jsonl').read_text().splitlines()) == 2
            finally:
                subprocess.run(['docker','rm','-f',name],check=True,stdout=subprocess.DEVNULL,timeout=30)
        print('PASS: real capture and Vim score',reward)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('image', nargs='?')
    args = parser.parse_args()
    image = args.image
    if image is None:
        image = subprocess.check_output(
            ['docker', 'build', '-q', '-'],
            input=b'FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y vim && rm -rf /var/lib/apt/lists/*\nWORKDIR /app\n',
            timeout=600).decode().strip()
    check(image)
    check_capture(image)
