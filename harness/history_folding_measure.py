"""Offline turn-folding estimate, only the named dev2 job; never scans other jobs.

Reads actual request histories (already result-shaped). Token reductions use
run-3's chars/4 estimate, not provider tokenization. Counts one hit per result,
with each triggering request retained. Quote checks see prior history only.
"""
import csv
import json
import re
from pathlib import Path

JOB = Path('harness/runs/dev2-run-dev-1789286562939723000-9523')
WORD = re.compile(r'[A-Za-z_][A-Za-z0-9_./-]{5,}')
EXIT = re.compile(r'^\[exit code: .*\]$', re.M)
NS = (6, 10, 16)
OUTPUT = JOB / 'history-folding-measure.json'
TABLE = Path('harness/history_folding_dev2.csv')

def ident(value):
    return json.dumps(value, sort_keys=True)

def text_content(content):
    return '\n'.join(p['text'] for p in content if p.get('type') == 'text')

def compact(text):
    lines = text.splitlines(keepends=True)
    if len(lines) <= 2:
        return text
    head, tail = lines[0], lines[-1]
    statuses = [s for s in EXIT.findall(text) if s not in head + tail]
    kept = head + '\n'.join(statuses) + ('\n' if statuses else '')
    elided = len(text) - len(head) - len(tail) - sum(map(len, statuses))
    result = kept + f'[... {elided} chars elided; call the tool again to see it ...]\n' + tail
    return result if len(result) < len(text) else text

def measure(path):
    result = json.loads(path.read_text())
    records = json.loads((path.parent / 'agent/effects.json').read_text())['records']
    completions = [r for r in records if r['kind']['effect'] == 'completion']
    first = {}
    saved = {}
    originals = {}
    rows = {n: {'chars_removed': 0, 'quote_checked_chars_removed': 0,
                'hits': {}, 'quote_checked_hits': {}} for n in NS}
    for turn, record in enumerate(completions):
        history = record['kind']['request']['chat_history']
        args = []
        results = []
        seen_results = set()
        for message in history:
            content = message['content']
            if not isinstance(content, list):
                continue
            for part in content:
                # Dev2 has no assistant prose/reasoning to remove from a
                # completed exchange. Keep native calls, including signatures,
                # IDs, names and arguments; only their result bodies shrink.
                if message['role'] == 'assistant':
                    assert part['type'] == 'toolcall'
                if part['type'] == 'toolcall':
                    args.append(json.dumps(part['function']['arguments'], ensure_ascii=False))
                elif part['type'] == 'toolresult':
                    key = ident(part['call'])
                    assert key not in seen_results, (path, turn, key)
                    seen_results.add(key)
                    assert all(p['type'] == 'text' for p in part['content'])
                    full = text_content(part['content'])
                    assert full == originals.setdefault(key, full)
                    first.setdefault(key, turn)
                    if key not in saved:
                        shortened = compact(full)
                        all_words = set(WORD.findall(full))
                        cut_words = all_words - set(WORD.findall(shortened))
                        saved[key] = (len(full) - len(shortened), all_words, cut_words, part['name'])
                    results.append((key, len(args)))
        choice = record.get('outcome', {}).get('Ok', {}).get('choice', [])
        uses = []
        for part in choice:
            if part['type'] == 'toolcall':
                uses.append(('arguments', json.dumps(part['function']['arguments'], ensure_ascii=False)))
            elif part['type'] == 'text' and turn == len(completions) - 1:
                uses.append(('answer', part['text']))
        for key, after in results:
            reduction, words, lost, tool = saved[key]
            if not reduction:
                continue
            age = turn - first[key] + 1
            eligible = [n for n in NS if age > n]
            if not eligible:
                continue
            later_args = '\n'.join(args[after:])
            protected = any(w in later_args for w in words)
            hit = next(((kind, w) for kind, value in uses for w in sorted(lost) if w in value), None)
            for n in eligible:
                row = rows[n]
                row['chars_removed'] += reduction
                if not protected:
                    row['quote_checked_chars_removed'] += reduction
                if hit:
                    detail = {'request': turn + 1, 'age': age, 'kind': hit[0], 'token': hit[1], 'tool': tool}
                    row['hits'].setdefault(key, []).append(detail)
                    if not protected:
                        row['quote_checked_hits'].setdefault(key, []).append(detail)
    return {'trial': path.parent.name, 'task': result['task'], 'reward': result['reward'],
            'input_tokens': result['input_tokens'], 'model_calls': len(completions),
            'tool_calls': result['tool_calls'], 'settings': rows}

if __name__ == '__main__':
    assert JOB.name.startswith('dev2-') and 'holdout' not in str(JOB)
    paths = sorted(JOB.glob('*/result.json'))
    assert len(paths) == 60
    rows = []
    for path in paths:
        rows.append(measure(path))
        print('measured', path.parent.name, flush=True)
    long_names = {r['task'] for r in rows if r['tool_calls'] >= 110}
    output = {'job': str(JOB), 'estimator': 'unicode characters removed / 4',
              'long_tasks': sorted(long_names), 'trials': rows}
    OUTPUT.write_text(json.dumps(output, indent=2) + '\n')
    with TABLE.open('w', newline='') as file:
        writer = csv.writer(file, lineterminator='\n')
        writer.writerow(['trial', 'task', 'reward', 'model_calls', 'tool_calls',
                         'input_tokens', 'N', 'estimated_tokens_removed',
                         'quote_checked_estimated_tokens_removed',
                         'result_hits', 'quote_checked_result_hits'])
        for row in rows:
            for n in NS:
                setting = row['settings'][n]
                writer.writerow([row['trial'], row['task'], row['reward'],
                                 row['model_calls'], row['tool_calls'],
                                 row['input_tokens'], n, setting['chars_removed'] / 4,
                                 setting['quote_checked_chars_removed'] / 4,
                                 len(setting['hits']), len(setting['quote_checked_hits'])])
    for n in NS:
        selected = [r for r in rows if r['task'] in long_names]
        total_input = sum(r['input_tokens'] for r in rows)
        long_input = sum(r['input_tokens'] for r in selected)
        def removed(rs, field):
            return sum(r['settings'][n][field] for r in rs) / 4
        def hits(field):
            return sum(len(r['settings'][n][field]) for r in rows if r['reward'] == 1)
        print(n, 'all input', total_input, 'long input', long_input,
              'raw', removed(rows, 'chars_removed'),
              'checked', removed(rows, 'quote_checked_chars_removed'),
              'long raw', removed(selected, 'chars_removed'),
              'long checked', removed(selected, 'quote_checked_chars_removed'),
              'hits', hits('hits'), 'checked hits', hits('quote_checked_hits'))
