"""Real Docker capture, isolated compilation and host scoring; no model calls."""
from pathlib import Path
import subprocess
import tempfile
import uuid

from artifact_capture import bounded_command
from polyglot_evaluate import evaluate

SOURCE = r'''/* /*
*/
#include <cstdio>
#include <cstdlib>
int main(int argc, char** argv) {
  int n = std::atoi(argv[1]); unsigned long long a=1,b=1;
  for(int i=0;i<n;++i){auto c=a+b;a=b;b=c;}
  std::printf("%llu\n",a); return 0;
}
const char* ignored=R"RUST(
*/
fn main() {
 let n: usize=std::env::args().nth(1).unwrap().parse().unwrap();
 let (mut a,mut b)=(1u64,1u64);
 for _ in 0..n { let c=a+b;a=b;b=c; }
 println!("{}",a);
}
// )RUST";
'''


def check():
    with tempfile.TemporaryDirectory(prefix='rigcoder-polyglot-canary-') as tmp:
        root = Path(tmp)
        (root / 'Dockerfile').write_text('FROM ubuntu:24.04\nWORKDIR /app\n'
                                        'RUN apt-get update && apt-get install -y rustc g++\n')
        subprocess.run(['docker', 'build', '--iidfile', root / 'image.id', root],
                       check=True, timeout=600)
        image = (root / 'image.id').read_text().strip()
        for label in ('correct', 'extra_binary', 'wrong', 'compile_failure', 'link'):
            case = root / label
            case.mkdir()
            submission = case / 'polyglot'
            submission.mkdir()
            source = submission / 'main.rs'
            source.write_text(SOURCE if label != 'compile_failure' else 'synthetic invalid code')
            if label == 'extra_binary':
                (submission / 'main').write_bytes(b'synthetic leftover executable')
            elif label == 'wrong':
                source.write_text(SOURCE.replace('a=1,b=1', 'a=2,b=2').replace('(1u64,1u64)', '(2u64,2u64)'))
            elif label == 'link':
                source.unlink()
                source.symlink_to('/private/hidden-canary')
            evidence = case / 'evidence'
            evidence.mkdir()
            name = 'rigcoder-polyglot-canary-' + uuid.uuid4().hex
            try:
                bounded_command(['docker', 'create', '--name', name, '--network', 'none',
                                 '--cap-drop', 'ALL', '--entrypoint', 'sleep', image, '300'], 4096, 30)
                bounded_command(['docker', 'start', name], 4096, 30)
                bounded_command(['docker', 'cp', str(submission), name + ':/app/polyglot'], 4096, 30)
                reward = evaluate(name, image, evidence, 180)
                assert reward == (1.0 if label == 'correct' else 0.0), (label, reward)
            finally:
                bounded_command(['docker', 'rm', '-f', name], 4096, 30)
    print('PASS: real Docker polyglot capture/compilers/programs and host decisions; zero API calls')


if __name__ == '__main__':
    check()
