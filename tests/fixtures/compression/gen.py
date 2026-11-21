# Regenerates app.v1.js and app.v2.js: two releases of one JavaScript bundle,
# deterministic (seeded), with varied identifiers and strings so it
# compresses like a real bundle, and v2 changing about 3% of v1's lines.
# Usage: python3 tests/fixtures/compression/gen.py
import random, os
here = os.path.dirname(os.path.abspath(__file__))
words = "user cart item price order total render state props view list query fetch cache token route theme modal button input select option value label error status loading data result config session".split()
def ident(r):
    return "".join(w.capitalize() if i else w for i, w in enumerate(r.sample(words, r.randint(1, 3))))
def module(r, i, v):
    name = ident(r) + str(i)
    a, b, c = ident(r), ident(r), ident(r)
    n = r.randint(1, 999)
    s = r.choice(words) + "-" + r.choice(words)
    lines = [
        f"export function {name}({a}, {b}) {{",
        f"  const {c} = useMemo(() => {a}.filter((x) => x.{b} > {n}), [{a}]);",
        f"  if (!{c}.length) return h(\"div\", {{ className: \"{s}\" }}, t(\"{s}.empty\"));",
        f"  return h(\"ul\", {{ className: \"{s}-list\" }}, {c}.map((x, i) => h(Item, {{ key: x.id ?? i, {b}: x.{b} }})));",
        "}",
    ]
    return lines
r = random.Random(9842)
modules = [module(r, i, 1) for i in range(420)]
v1 = "\n".join(l for m in modules for l in m) + "\n"
r2 = random.Random(2)
for i in range(len(modules)):
    if r2.random() < 0.03:
        modules[i] = module(r2, i, 2)
v2 = "\n".join(l for m in modules for l in m) + "\n"
open(os.path.join(here, "app.v1.js"), "w").write(v1)
open(os.path.join(here, "app.v2.js"), "w").write(v2)
print(len(v1), len(v2))
