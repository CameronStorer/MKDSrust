"""Readable-source (T3) progress, from a `--timing native` build's generated source.

  direct: the function has a direct variant (real register parameters).
  T3a:    direct, and the body never uses the simulated CPU (`c`).
  T3b:    T3a, and the body never uses the simulated memory bus (`m`).

In scope (owner decision 2026-11-10): everything except ARM7 code (replaced by native services) and the
online area (`code_net`, Nintendo's servers are gone).

Usage: python t3_metric.py <build>/source"""
import collections, csv, os, re, sys

src = sys.argv[1]
part_of = {}
for t in csv.DictReader(open(os.path.join(src, 'tiers.csv'), encoding='utf-8')):
    p = t['part']
    part_of[t['name']] = p.rsplit('_', 1)[0] if p[-1].isdigit() else p
total = len(part_of)

text = open(os.path.join(src, 'direct', 'src', 'lib.rs'), encoding='utf-8').read()
fn_re = re.compile(r'^pub fn (\w+)\(', re.M)
starts = [m.start() for m in fn_re.finditer(text)] + [len(text)]
# lines every direct function has (imports, macro definitions); they don't count as uses
boiler = re.compile(r'^\s*(#\[allow|macro_rules!)')
uses_c = re.compile(r'(?<![\w.])c(\.|\s*,|\s*\))')
uses_m = re.compile(r'(?<![\w.])m(\.|\s*,|\s*\))')

direct = t3a = t3b = 0
area = collections.defaultdict(lambda: [0, 0, 0, 0])  # total, direct, t3a, t3b
for name, p in part_of.items():
    area[p][0] += 1
for a, b in zip(starts, starts[1:]):
    chunk = text[a:b]
    name = fn_re.match(chunk).group(1)
    body = chunk.split('{', 1)[1] if '{' in chunk else ''
    body = '\n'.join(l for l in body.split('\n') if not boiler.match(l))
    a_ok = not uses_c.search(body)
    b_ok = a_ok and not uses_m.search(body)
    direct += 1; t3a += a_ok; t3b += b_ok
    p = part_of.get(name, '?')
    area[p][1] += 1; area[p][2] += a_ok; area[p][3] += b_ok

print(f'functions {total}: direct {direct} ({direct/total:.1%}), T3a no CPU {t3a} ({t3a/total:.1%}), '
      f'T3b no simulated memory {t3b} ({t3b/total:.1%})')
OUT = ('code_arm7', 'code_net')
sn = sum(v[0] for k, v in area.items() if k not in OUT)
sa = sum(v[2] for k, v in area.items() if k not in OUT); sb = sum(v[3] for k, v in area.items() if k not in OUT)
print(f'in scope (no ARM7, no online) {sn}: T3a {sa} ({sa/sn:.1%}), T3b {sb} ({sb/sn:.1%})')
for k, (n, d, x, y) in sorted(area.items(), key=lambda kv: -kv[1][0]):
    if n:
        print(f'  {k:18} direct {d:5}  T3a {x:5}  T3b {y:5}  / {n}')
