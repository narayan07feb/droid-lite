#!/usr/bin/env python3
"""Writes the expected lists of the accuracy test (tests/fixtures/accuracy/*.json) from a real
project, so Droid Lite's Hilt links, usages and go-to-declaration can be compared with the truth.

    scripts/accuracy_expected.py snapshot <checkout> <dir>      # sources at HEAD + the Dagger graph
    scripts/accuracy_expected.py hilt   <dir> [--sample 50] > tests/fixtures/accuracy/hilt.json
    scripts/accuracy_expected.py usages <dir>               > tests/fixtures/accuracy/usages.json
    DROID_LITE_ACCURACY_PROJECT=<dir> cargo test --release accuracy_on_a_real_project -- --ignored --nocapture

snapshot: the checkout's committed Kotlin/Java sources (`git archive HEAD`, so edits in progress do
not move the expected lines) and its debug Dagger component, in <dir>. The fixtures were made from
BranchAndroid at the commit in their "commit" field.

hilt: the truth is the Dagger component Hilt generated for the app's debug build
(app/build/generated/hilt/component_sources/debug/**/Dagger*.java; build the app once first). Every
`new Foo(...)`, `XModule_ProvideBarFactory.provideBar(...)` and `Foo_MembersInjector.injectBaz(...)`
in it says which binding feeds each constructor parameter, `@Provides` parameter and `@Inject`
field. A binding is a `@Provides` function (its factory), an `@Inject` class (`new Foo(...)`), or a
`@Binds` function (the parameter's declared type differs from the class built; found by its
parameter and return types). Multibindings (`ImmutableSet.of(...)`) give every contribution. A call
whose argument count no longer matches the source (changed since the build) is skipped, and its
class is left out of "graph_owners". The sample takes the same share of each kind, every n-th, plus
the most injected binding, a multibinding contribution and a qualified binding.

usages: no compiler oracle runs here (kotlin-lsp needs a Gradle import of the whole project, which
writes to ~/.gradle). For each symbol in scripts/accuracy_sample.json, every whole-word place of its
name in code (comments, strings and backtick names skipped) is a candidate; the places that mean
that declaration were read by hand and are kept there (`keep`), with `jump` for the ones whose
go-to-declaration goes to an override.
"""

import json
import os
import re
import sys

SKIP_DIRS = {"build", ".gradle", ".git", ".idea", "node_modules", ".cxx"}


def sources(root):
    for d, dirs, files in os.walk(root):
        dirs[:] = [x for x in dirs if x not in SKIP_DIRS]
        for f in files:
            if f.endswith((".kt", ".java")):
                yield os.path.join(d, f)


def main_set(rel):
    """Only main and debug source sets feed the debug graph."""
    parts = rel.split("/")
    if "src" not in parts:
        return True
    s = parts[parts.index("src") + 1]
    return s in ("main", "debug")


# ---------------------------------------------------------------- source map


class Source:
    """Where classes and functions are declared, by package and simple name."""

    def __init__(self, root):
        self.root = root
        self.types = {}  # fqn -> (rel, line)
        self.texts = {}
        decl = re.compile(r"\b(?:class|interface|object)\s+([A-Za-z_]\w*)")
        for p in sources(root):
            rel = os.path.relpath(p, root)
            if not main_set(rel):
                continue
            try:
                text = open(p, encoding="utf-8").read()
            except (OSError, UnicodeDecodeError):
                continue
            self.texts[rel] = text
            m = re.search(r"^\s*package\s+([\w.]+)", text, re.M)
            pkg = m.group(1) if m else ""
            for m in decl.finditer(code_only(text)):
                fqn = f"{pkg}.{m.group(1)}" if pkg else m.group(1)
                line = text.count("\n", 0, m.start(1)) + 1
                self.types.setdefault(fqn, (rel, line))

    def type(self, fqn):
        fqn = fqn.replace("$", ".")
        if fqn in self.types:
            return self.types[fqn]
        # a nested class: `pkg.Outer.Inner` is found as `pkg.Inner` in the same package
        parts = fqn.split(".")
        for i in range(len(parts) - 1, 0, -1):
            pkg = ".".join(parts[:i])
            cand = pkg + "." + parts[-1]
            if cand in self.types and all(not p[:1].isupper() for p in parts[:i]):
                return self.types[cand]
        # moved since the build: the one class of that name
        if not hasattr(self, "simple"):
            self.simple = {}
            for k, v in self.types.items():
                self.simple.setdefault(k.rsplit(".", 1)[-1], []).append(v)
        same = self.simple.get(parts[-1], [])
        return same[0] if len(same) == 1 else None

    def param_types(self, rel, line, name):
        """Simple type names of the parameters of the constructor or function `name` declared on
        `line` of `rel` (`Lazy<T>` / `Provider<T>` are `T`), or None when not found."""
        text = self.texts.get(rel)
        if text is None or not line:
            return None
        code = code_only(text)
        start = sum(len(l) + 1 for l in text.split("\n")[: line - 1])
        java = rel.endswith(".java")
        m = re.compile(r"\b" + re.escape(name) + r"\b").search(code, start)
        if not m:
            return None
        if java and name[:1].isupper():
            m = re.compile(r"@Inject\b[^;{]*?\b" + re.escape(name) + r"\s*\(").search(code, m.end())
            if not m:
                return None
            open_at = m.end() - 1
        else:
            k = re.compile(r"\bconstructor\s*\(|\(").search(code, m.end())
            if not k:
                return None
            open_at = k.end() - 1
        args, _ = call_args(code, open_at)
        out = []
        for a in split_args(args):
            a = re.sub(r"@\w+(\([^)]*\))?(:\w+)?", " ", a).strip()
            if java:
                ty = a.rsplit(None, 1)[0] if " " in a else a
            else:
                ty = a.split(":", 1)[1] if ":" in a else ""
                ty = ty.split("=", 1)[0]
            ty = re.sub(r"\b(final|val|var|private|internal|public|protected)\b", "", ty).strip().rstrip("?")
            m2 = re.match(r"^(?:dagger\.)?(?:Lazy|Provider|javax\.inject\.Provider)<(.*)>$", ty)
            if m2:
                ty = m2.group(1)
            simple = ty.split("<")[0].split(".")[-1].strip()
            alias = re.search(r"^import\s+([\w.]+)\s+as\s+" + re.escape(simple) + r"\s*$", text, re.M) if simple else None
            out.append(alias.group(1).split(".")[-1] if alias else simple)
        return out

    def fun(self, rel, name, near=None):
        """Line of `fun name(` (or a Java method) in the file, the one after `near` first."""
        text = self.texts.get(rel, "")
        best = None
        for m in re.finditer(r"\b" + re.escape(name) + r"\s*\(", code_only(text)):
            head = text[max(0, m.start() - 200) : m.start()]
            if not re.search(r"(fun\s+(<[^>]*>\s*)?|[\w>\]]\s+)$", head):
                continue
            line = text.count("\n", 0, m.start()) + 1
            if near is None or line >= near:
                return line
            best = best or line
        return best


def code_only(text):
    """The text with comments and strings blanked (same offsets)."""
    out = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            j = n if j < 0 else j + 2
            out.append(re.sub(r"[^\n]", " ", text[i:j]))
            i = j
        elif text.startswith('"""', i):
            j = text.find('"""', i + 3)
            j = n if j < 0 else j + 3
            out.append(re.sub(r"[^\n]", " ", text[i:j]))
            i = j
        elif c == "`":
            # a backtick name (`given x - when y`) is one name, not code that uses its words
            j = text.find("`", i + 1)
            j = i + 1 if j < 0 or "\n" in text[i:j] else j + 1
            out.append("`" + "_" * (j - i - 2) + "`" if j - i >= 2 else " ")
            i = j
        elif c in "\"'":
            j = i + 1
            while j < n and text[j] != c and text[j] != "\n":
                j += 2 if text[j] == "\\" else 1
            j = min(j + 1, n)
            out.append(" " * (j - i))
            i = j
        else:
            out.append(c)
            i += 1
    return "".join(out)


# ---------------------------------------------------------------- Dagger component


def split_args(s):
    """`a(b, c), d` -> ['a(b, c)', 'd']"""
    out, depth, cur = [], 0, []
    for ch in s:
        if ch in "(<[{":
            depth += 1
        elif ch in ")>]}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    if "".join(cur).strip():
        out.append("".join(cur).strip())
    return out


def call_args(expr, start):
    """The text between the `(` at `start` and its `)`, and the index after it."""
    depth = 0
    for i in range(start, len(expr)):
        if expr[i] == "(":
            depth += 1
        elif expr[i] == ")":
            depth -= 1
            if depth == 0:
                return expr[start + 1 : i], i + 1
    return expr[start + 1 :], len(expr)


def lower_camel(name):
    return name[:1].lower() + name[1:]


class Graph:
    def __init__(self, path, src):
        self.src = src
        text = open(path, encoding="utf-8").read()
        self.imports = {}
        for m in re.finditer(r"^import ([\w.]+)\.(\w+);", text, re.M):
            self.imports[m.group(2)] = m.group(1) + "." + m.group(2)
        m = re.search(r"^package ([\w.]+);", text, re.M)
        self.pkg = m.group(1)
        self.fields = {}  # name -> (type, init expr)
        self.cases = {}  # (component, n) -> (comment, expr)
        self.methods = {}  # name -> (type, expr)
        self.injects = []  # (class simple, field, expr)
        self.entry = []  # (type, name, expr)
        comp = None
        for m in re.finditer(
            r"^  private static final class (\w+)|"
            r"^    private static final class SwitchingProvider|"
            r"^          case (\d+): // (.*)\n\s+return \(T\) (.*);$|"
            r"^    (?:private )?(?:Provider|javax\.inject\.Provider)<(.+)> ([\w$]+);$|"
            r"^      (?:this\.([\w$]+) = |DelegateFactory\.setDelegate\(([\w$]+), )(.*?)\)?;$|"
            r"^    (?:@Override\n    )?(public )?([\w.<>, ?]+) ([\w$]+)\(\) \{\n      return (.*);\n    \}$|"
            r"^      (\w+)_MembersInjector\.inject(\w+)\(instance\d*, (.*)\);$",
            text,
            re.M,
        ):
            g = m.groups()
            if g[0]:
                comp = g[0]
            elif g[1]:
                self.cases[(comp, g[1])] = (g[2], g[3])
            elif g[4]:
                self.fields[g[5]] = (g[4], None, comp)
            elif g[6] or g[7]:
                name = g[6] or g[7]
                f = self.fields.get(name)
                if f and (g[7] or "DelegateFactory" not in g[8]):
                    self.fields[name] = (f[0], g[8] if g[7] else g[8] + (")" if g[8].count("(") > g[8].count(")") else ""), f[2])
            elif g[11]:
                self.methods[g[11]] = (g[10], g[12])
                if g[9]:
                    self.entry.append((g[10], g[11], g[12]))
            elif g[13]:
                self.injects.append((g[13], lower_camel(g[14]), g[15]))
        self.memo = {}
        self.busy = set()
        self.cycles = 0
        self.stale = 0
        self.stale_owners = set()
        self.walked = set()
        self.all_binds = None
        self.sites = {}  # provider id -> set of sites
        self.providers = {}  # provider id -> description

    def fqn(self, simple):
        simple = simple.split("<")[0].strip()
        if "." in simple and simple[0].islower():
            return simple
        head = simple.split(".")[0]
        if head in self.imports:
            return self.imports[head] + simple[len(head) :]
        return self.pkg + "." + simple

    # a binding reference -> list of provider ids (several for a multibinding), or [] for builtins
    def resolve(self, expr, want=None, depth=0):
        key = (expr.strip(), want)
        if key in self.busy:
            self.cycles += 1  # a cycle answers nothing, and what depends on it is not kept
            return []
        if key in self.memo:
            return self.memo[key]
        before = self.cycles
        self.busy.add(key)
        r = self.resolve_once(expr, want, depth)
        self.busy.discard(key)
        if self.cycles == before:
            self.memo[key] = r
        return r

    def resolve_once(self, expr, want=None, depth=0):
        expr = expr.strip()
        if depth > 2000:
            sys.exit("binding chain too deep")
        m = re.match(r"^\(([\w.<>, ?]+)\) \(\(\(Provider\) \((.*)\)\)\.get\(\)\)$", expr)
        if m:
            return self.resolve(m.group(2), m.group(1), depth + 1)
        for wrap in ("DoubleCheck.lazy(", "ProviderOfLazy.create(", "Optional.of(", "Providers.of("):
            if expr.startswith(wrap) and expr.endswith(")"):
                return self.resolve(expr[len(wrap) : -1], want, depth + 1)
        if expr.endswith(".get()"):
            return self.resolve(expr[: -len(".get()")], want, depth + 1)
        m = re.match(r"^(?:\w+CImpl\.)?([\w$]+)$", expr)
        if m and m.group(1) in self.fields:
            ty, init, comp = self.fields[m.group(1)]
            name = m.group(1)[: -len("Provider")] if m.group(1).endswith("Provider") else m.group(1)
            c = re.search(r"SwitchingProvider<[^>]*>\((?:[^()]*), (\d+)\)", init or "")
            if not c:
                c2 = re.match(r"^(?:\w+\.)?provider\((?:\(Provider\) )?\(?(.*?)\)?\)$", init or "")
                return self.resolve(c2.group(1), want or ty, depth + 1) if c2 else []
            case = self.cases.get((comp, c.group(1)))
            if not case:
                return []
            return self.binding(name, want or ty, case[1], case[0], depth)
        m = re.match(r"^(?:\w+CImpl\.)?([\w$]+)\(\)$", expr)
        if m and m.group(1) in self.methods:
            ty, body = self.methods[m.group(1)]
            if (want or "").split("<")[0] in ("Set", "Map", "List"):
                want = None  # the method's own type keeps the element type
            return self.binding(m.group(1), want or ty, body, None, depth)
        return self.binding(None, want, expr, None, depth)

    def binding(self, name, want, expr, comment, depth):
        """The provider of `expr` (a case body or a method body), built for the type `want`."""
        name = (name or "").split("$")[0]
        if comment and "#" in comment:
            # a multibinding contribution: `java.util.Set<...> pkg.Module#method`
            mod, meth = comment.split()[-1].split("#")
            return [self.provides(mod, meth)]
        m = re.match(r"^(?:ImmutableSet\.<.*>of|ImmutableSet\.of|SetBuilder\.<.*>newSetBuilder\(\d+\)(?:\.add\(.*)?)\((.*)\)$", expr)
        if expr.startswith(("ImmutableSet.", "SetBuilder.")):
            out = []
            el = re.match(r"^(?:java\.util\.)?Set<(.*)>$", want or "")
            el = el.group(1) if el else None
            for part in re.findall(r"(?:of|add|addAll)\((.*?)\)(?=\.|$)", expr):
                for a in split_args(part):
                    out += self.resolve(a, el, depth + 1)
            return out
        # internal functions carry their module's name: `provideX$BranchAndroid_lib_debug`
        plain = re.sub(r"\$BranchAndroid_\w+?_(?:debug|release)", "", expr.split("(", 1)[0]) + (
            "(" + expr.split("(", 1)[1] if "(" in expr else ""
        )
        m = re.match(r"^(\w+(?:\.\w+)*)_(\w+)Factory\.([\w$]+)\((.*)\)$", plain)
        if m and not m.group(1).endswith("_Factory") and m.group(3) not in ("newInstance", "create", "get"):
            meth = m.group(3).split("$")[0]
            mod = m.group(1)
            factory = self.fqn(f"{m.group(1)}_{m.group(2)}Factory")
            if mod.endswith("_Companion"):
                mod = mod[: -len("_Companion")]
            if mod.startswith("ApplicationContextModule") or mod.startswith("Hilt"):
                return []
            pid = self.provides(factory.rsplit(".", 1)[0] + "." + mod.split(".")[-1], meth)
            self.sites_of_call(pid, expr, depth)
            return [pid]
        m = re.match(r"^new ([\w.]+)(?:<.*?>)?\(", expr)
        n = re.match(r"^([\w.]+)_Factory\.newInstance\(", expr)
        if m or n:
            cls = self.fqn((m or n).group(1))
            pid = self.inject(cls)
            self.sites_of_call(pid, expr, depth)
            if want and self.fqn(want).split(".")[-1] != cls.split(".")[-1] and not want.startswith(("Lazy", "Provider")):
                return [self.binds(name, want, cls)]
            return [pid]
        return []

    def provides(self, mod, meth):
        pid = f"provides {mod}#{meth}"
        if pid not in self.providers:
            t = self.src.type(mod)
            if t:
                self.providers[pid] = {"kind": "provides", "file": t[0], "line": self.src.fun(t[0], meth, t[1]), "name": meth}
            else:
                self.providers[pid] = None
        return pid

    def inject(self, cls):
        pid = f"inject {cls}"
        if pid not in self.providers:
            t = self.src.type(cls)
            self.providers[pid] = {"kind": "inject", "file": t[0], "line": t[1], "name": cls.split(".")[-1]} if t else None
        return pid

    def binds(self, name, want, impl):
        """The `@Binds` function that turns `impl` into `want`: by its name when Dagger named the
        field after it, else the one whose parameter and return types match."""
        want_s = want.split("<")[0].split(".")[-1]
        impl_s = impl.split(".")[-1]
        pid = f"binds {want_s}<-{impl_s}"
        if pid in self.providers:
            return pid
        if self.all_binds is None:
            self.all_binds = []
            for rel, text in self.src.texts.items():
                if "@Binds" not in text:
                    continue
                code = code_only(text)
                for m in re.finditer(r"@Binds\b[\s\S]{0,400}?\bfun\s+(\w+)\s*\(\s*\w+\s*:\s*([\w.]+)[^)]*\)\s*:\s*([\w.]+)", code):
                    line = text.count("\n", 0, m.start(1)) + 1
                    self.all_binds.append((m.group(1), m.group(2).split(".")[-1], m.group(3).split(".")[-1], rel, line))
        found = None
        for fn, p, r, rel, line in self.all_binds:
            if p == impl_s and r == want_s and not (name and name.startswith("bind") and fn != name):
                found = {"kind": "binds", "file": rel, "line": line, "name": fn}
                break
        self.providers[pid] = found
        if found:
            # the @Binds function's parameter is where the class is injected
            self.add_site(self.inject(impl), {"file": found["file"], "line": found["line"], "owner": found["name"], "param": 0})
        return pid

    def add_site(self, pid, site):
        self.sites.setdefault(pid, [])
        if site not in self.sites[pid]:
            self.sites[pid].append(site)

    def sites_of_call(self, pid, expr, depth):
        """Each argument of a constructor or `@Provides` call is a site of the provider it names."""
        info = self.providers.get(pid)
        if not info or (pid, expr) in self.walked:
            return
        self.walked.add((pid, expr))
        args, _ = call_args(expr, expr.index("("))
        args = [a for a in split_args(args) if not re.match(r"^\w+CImpl\.\w*[mM]odule$|^\w*Module$", a)]
        types = self.src.param_types(info["file"], info["line"], info["name"])
        if types is None or len(types) != len(args):
            self.stale += 1  # the source changed since the build: its sites are not known
            self.stale_owners.add((info["file"], info["line"]))
            return
        for i, a in enumerate(args):
            if i >= len(types):
                break  # assisted parameters come first in a factory's `create`
            for p in self.resolve(a, types[i] or None, depth + 1):
                self.add_site(p, {"file": info["file"], "line": info["line"], "owner": info["name"], "param": i})

    def walk(self):
        for (comp, n), (comment, expr) in self.cases.items():
            self.resolve_body(expr)
        for name, (ty, body) in self.methods.items():
            self.resolve_body(body)
        for cls, field, expr in self.injects:
            owner = self.fqn(cls)
            t = self.src.type(owner)
            if not t:
                continue
            line = field_line(self.src.texts[t[0]], field)
            for p in self.resolve(expr, field_type(self.src.texts[t[0]], field)):
                self.add_site(p, {"file": t[0], "line": line, "owner": field, "param": None})

    def resolve_body(self, expr):
        # every nested `new X(` / factory call in a body defines sites; resolving does it
        for m in re.finditer(r"new [\w.]+(?:<[^>]*>)?\(|\b\w+_\w+Factory\.[\w$]+\(|\b\w+_Factory\.newInstance\(", expr):
            start = m.start()
            args, end = call_args(expr, m.end() - 1)
            self.binding(None, None, expr[start:end], None, 0)


def field_type(text, field):
    m = re.search(r"\b(?:var|val)\s+" + re.escape(field) + r"\s*:\s*([\w.]+)|\b([\w.]+)(?:<[^;]*>)?\s+" + re.escape(field) + r"\s*;", code_only(text))
    if not m:
        return None
    return (m.group(1) or m.group(2)).split(".")[-1]


def field_line(text, field):
    m = re.search(r"\b(?:var|val)\s+" + re.escape(field) + r"\b|\b\w+\s+" + re.escape(field) + r"\s*;", code_only(text))
    return text.count("\n", 0, m.start()) + 1 if m else None


def hilt(root, sample):
    src = Source(root)
    comp = None
    for d, _, files in os.walk(os.path.join(root, "app/build/generated/hilt/component_sources/debug")):
        for f in files:
            if f.startswith("Dagger") and f.endswith(".java"):
                comp = os.path.join(d, f)
    g = Graph(comp, src)
    # a second pass: answers cut short by a cycle in the first are complete now
    for _ in range(2):
        g.walked.clear()
        g.walk()
    keep = sorted(p for p, info in g.providers.items() if info and g.sites.get(p))
    # the same share of each kind (binds, provides, inject), each n-th of it
    picked = []
    for kind, share in (("binds", 0.3), ("provides", 0.3), ("inject", 0.4)):
        of = [p for p in keep if p.startswith(kind)]
        n = round(sample * share)
        picked += of[:: max(1, len(of) // n)][:n]
    # every provider of each site (several for a multibinding)
    of_site = {}
    for pid in keep:
        for st in g.sites[pid]:
            of_site.setdefault((st["file"], st["line"], st["param"]), set()).add(pid)
    # the hard cases go in too: the most injected provider, a multibinding contribution and a
    # qualified binding (`@Named` or an annotation marked `@Qualifier`)
    qualifiers = [m.group(1) for t in src.texts.values() for m in re.finditer(r"@Qualifier[\s\S]{0,200}?annotation\s+class\s+(\w+)", t)]
    def qualified(pid):
        info = g.providers[pid]
        lines = src.texts[info["file"]].split("\n")[max(0, info["line"] - 4) : info["line"]]
        return any(re.search(r"@(Named|" + "|".join(qualifiers or ["-"]) + r")\b", l) for l in lines)
    hard = [
        max(keep, key=lambda p: len(g.sites[p])),
        next((p for p in keep if any(len(of_site[(st["file"], st["line"], st["param"])]) > 1 for st in g.sites[p])), None),
        next((p for p in keep if qualified(p)), None),
    ]
    hard = [h for h in hard if h and h not in picked]
    picked = picked[: len(picked) - len(hard)] + hard
    of_site = {}
    owners = set()
    for pid in keep:
        info = g.providers[pid]
        owners.add((info["file"], info["line"]))
        for st in g.sites[pid]:
            key = (st["file"], st["line"], st["param"])
            owners.add((st["file"], st["line"]))
            of_site.setdefault(key, set()).add((info["file"], info["line"]))
    out = []
    for pid in picked:
        sites = sorted(g.sites[pid], key=lambda s: (s["file"], s["line"] or 0, str(s["param"])))
        for st in sites:
            st["providers"] = sorted([list(x) for x in of_site[(st["file"], st["line"], st["param"])]])
        out.append({"id": pid, **g.providers[pid], "sites": sites})
    return {
        "providers_in_graph": len(keep),
        "calls_skipped_as_changed_since_build": g.stale,
        "graph_owners": sorted([list(o) for o in owners if o[1] and o not in g.stale_owners]),
        "sample": out,
    }


# ---------------------------------------------------------------- usages

# Symbols checked by hand (scripts/accuracy_sample.json): `name`, and `file` and `line` of the
# declaration. `keep`: the candidate places that mean this declaration, as `File.kt:line:n` (the n-th
# place of the name on that line; a longer path suffix when two files share a name). Every other
# candidate place means another declaration (or is a local of the same name). `jump`: a kept place
# whose go-to-declaration goes to another declaration (`File.kt:line` of it), e.g. an override.
SAMPLE_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "accuracy_sample.json")


def occurrences(text, name):
    code = code_only(text)
    for m in re.finditer(r"(?<![\w$])" + re.escape(name) + r"(?![\w$])", code):
        yield m.start()


def candidates(root, files, name):
    """Each whole-word place of `name` in code: (rel, line, n)."""
    for rel in files:
        try:
            text = open(os.path.join(root, rel), encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            continue
        if name not in text:
            continue
        per_line = {}
        for o in occurrences(text, name):
            line = text.count("\n", 0, o) + 1
            per_line[line] = per_line.get(line, 0) + 1
            yield rel, line, per_line[line]


def usages(root):
    files = sorted(os.path.relpath(p, root) for p in sources(root))
    out = []
    for s in json.load(open(SAMPLE_FILE)):
        cands = list(candidates(root, files, s["name"]))
        found, jumps = [], {}
        for key in s["keep"]:
            k, n = key.rsplit(":", 1)[0], key
            hits = [c for c in cands if f"/{c[0]}:{c[1]}:{c[2]}".endswith("/" + key)]
            if len(hits) != 1:
                sys.exit(f"{s['name']}: {key} matches {len(hits)} places")
            rel, line, nth = hits[0]
            found.append(f"{rel}:{line}:{nth}")
        for key, to in s.get("jump", {}).items():
            hit = [f for f in found if ("/" + f).endswith("/" + key)]
            target = [rel for rel in files if ("/" + rel).endswith("/" + to.rsplit(":", 1)[0])]
            if len(hit) != 1 or len(target) != 1:
                sys.exit(f"{s['name']}: jump {key} -> {to} is not one place")
            jumps[hit[0]] = f"{target[0]}:{to.rsplit(':', 1)[1]}"
        out.append({"name": s["name"], "file": s["file"], "line": s["line"], "usages": found, "jump": jumps})
    return {"symbols": out}


def snapshot(checkout, out):
    import shutil
    import subprocess

    os.makedirs(out, exist_ok=True)
    sha = subprocess.run(["git", "-C", checkout, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
    archive = subprocess.Popen(["git", "-C", checkout, "archive", sha], stdout=subprocess.PIPE)
    subprocess.run(["tar", "-x", "-C", out, "--include", "*.kt", "--include", "*.java", "--include", "*.kts"], stdin=archive.stdout, check=True)
    archive.wait()
    gen = "app/build/generated/hilt/component_sources/debug"
    for d, _, files in os.walk(os.path.join(checkout, gen)):
        for f in files:
            if f.startswith("Dagger") and f.endswith(".java"):
                to = os.path.join(out, os.path.relpath(d, checkout))
                os.makedirs(to, exist_ok=True)
                shutil.copy2(os.path.join(d, f), to)
    with open(os.path.join(out, ".accuracy-commit"), "w") as f:
        f.write(sha + "\n")


def commit_of(root):
    try:
        return open(os.path.join(root, ".accuracy-commit")).read().strip()
    except OSError:
        return None


if __name__ == "__main__":
    what, root = sys.argv[1], os.path.abspath(sys.argv[2])
    if what == "snapshot":
        snapshot(root, os.path.abspath(sys.argv[3]))
        sys.exit(0)
    n = int(sys.argv[sys.argv.index("--sample") + 1]) if "--sample" in sys.argv else 50
    # binding chains recurse deeply: a big stack and recursion limit
    import threading

    sys.setrecursionlimit(200000)
    threading.stack_size(1 << 29)
    out = {}
    t = threading.Thread(target=lambda: out.update(r=hilt(root, n) if what == "hilt" else usages(root)))
    t.start()
    t.join()
    json.dump({"commit": commit_of(root), **out["r"]}, sys.stdout, indent=1)
    print()
