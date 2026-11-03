"""Block 4 finish line: a function is T2 when its name is confirmed (no _maybe, not sub_) and every
pointer argument it reads through is typed (no `arg` lines in the MKDS_TYPE_DUMP for its entry)."""
import csv, sys, collections
tiers, dump = sys.argv[1], sys.argv[2]
untyped = collections.defaultdict(set)
for line in open(dump, encoding='utf-8'):
    p = line.strip().split(',')
    if p[0] == 'arg':
        untyped[int(p[1], 16) & ~1].add(p[2])
tot = named = typed = both = 0
area = collections.defaultdict(lambda: [0, 0])
for t in csv.DictReader(open(tiers, encoding='utf-8')):
    e = int(t['entry'], 16) & ~1
    n = t['name']
    ok_name = 'maybe' not in n and not n.startswith('sub_')
    ok_type = e not in untyped
    tot += 1; named += ok_name; typed += ok_type; both += ok_name and ok_type
    a = area[t['part'].rsplit('_', 1)[0] if t['part'][-1].isdigit() else t['part']]
    a[0] += 1; a[1] += ok_name and ok_type
print(f'functions {tot}: confirmed name {named} ({named/tot:.1%}), pointer args typed {typed} ({typed/tot:.1%}), T2 {both} ({both/tot:.1%})')
print('functions with untyped pointer args:', len(untyped))
for k, (n, d) in sorted(area.items(), key=lambda x: -x[1][0]):
    print(f'  {k:18} {d:5}/{n:<5} {d/n:.0%}')
