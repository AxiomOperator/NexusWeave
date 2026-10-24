"""Minimal, dependency-free reader for SCIP indexes (https://github.com/sourcegraph/scip).

Only the fields the accuracy benchmark needs are decoded:

    Index.documents (2)            -> Document
    Document.relative_path (1), occurrences (2), symbols (3), position_encoding (6)
    Occurrence.range (1, packed int32), symbol (2), symbol_roles (3),
               single-line range message (8: {1 line, 2 start, 3 end}),
               multi-line range message  (9: {1 start line, 2 start char, 3 end line, 4 end char})
    (newer SCIP writers use the typed range messages; proto3 omits zero fields)
    SymbolInformation.symbol (1), relationships (4)
    Relationship.symbol (1), is_implementation (3)
"""

from __future__ import annotations

from dataclasses import dataclass, field

DEFINITION = 0x1


@dataclass
class Occurrence:
    line: int          # 0-based
    start: int         # 0-based start character
    end: int           # 0-based end character (same line) or -1 when multi-line
    symbol: str
    roles: int


@dataclass
class Document:
    path: str
    occurrences: list[Occurrence] = field(default_factory=list)
    # symbol -> symbols it implements (Relationship.is_implementation)
    implements: dict[str, list[str]] = field(default_factory=dict)


def _varint(b: bytes, i: int) -> tuple[int, int]:
    shift = result = 0
    while True:
        c = b[i]
        i += 1
        result |= (c & 0x7F) << shift
        if c < 0x80:
            return result, i
        shift += 7


def _fields(b: bytes):
    """Yield (field_number, wire_type, value) for one message; value is int or bytes."""
    i, n = 0, len(b)
    while i < n:
        key, i = _varint(b, i)
        fno, wt = key >> 3, key & 7
        if wt == 0:
            v, i = _varint(b, i)
        elif wt == 2:
            ln, i = _varint(b, i)
            v = b[i:i + ln]
            i += ln
        elif wt == 1:
            v = b[i:i + 8]
            i += 8
        elif wt == 5:
            v = b[i:i + 4]
            i += 4
        else:
            raise ValueError(f"unsupported wire type {wt}")
        yield fno, wt, v


def _packed_int32(v) -> list[int]:
    if isinstance(v, int):
        return [v]
    out, i = [], 0
    while i < len(v):
        x, i = _varint(v, i)
        out.append(x)
    return out


def _occurrence(b: bytes) -> Occurrence | None:
    rng: list[int] = []
    symbol, roles = "", 0
    for fno, _, v in _fields(b):
        if fno == 1:
            rng.extend(_packed_int32(v))
        elif fno == 2:
            symbol = v.decode("utf-8", "replace")
        elif fno == 3:
            roles = v
        elif fno in (8, 9) and isinstance(v, bytes):
            sub = {f: x for f, _, x in _fields(v)}
            if fno == 8:
                rng = [sub.get(1, 0), sub.get(2, 0), sub.get(3, 0)]
            else:
                rng = [sub.get(1, 0), sub.get(2, 0), sub.get(3, 0), sub.get(4, 0)]
    if len(rng) == 3:
        return Occurrence(rng[0], rng[1], rng[2], symbol, roles)
    if len(rng) == 4:
        return Occurrence(rng[0], rng[1], rng[3] if rng[0] == rng[2] else -1, symbol, roles)
    return None


def _symbol_info(b: bytes) -> tuple[str, list[str]]:
    sym, impls = "", []
    for fno, _, v in _fields(b):
        if fno == 1:
            sym = v.decode("utf-8", "replace")
        elif fno == 4:
            target, is_impl = "", False
            for f2, _, v2 in _fields(v):
                if f2 == 1:
                    target = v2.decode("utf-8", "replace")
                elif f2 == 3:
                    is_impl = bool(v2)
            if is_impl and target:
                impls.append(target)
    return sym, impls


def read_index(path: str) -> list[Document]:
    with open(path, "rb") as fh:
        data = fh.read()
    docs: list[Document] = []
    for fno, _, v in _fields(data):
        if fno != 2:
            continue
        doc = Document(path="")
        for f2, _, v2 in _fields(v):
            if f2 == 1:
                doc.path = v2.decode("utf-8", "replace")
            elif f2 == 2:
                occ = _occurrence(v2)
                if occ:
                    doc.occurrences.append(occ)
            elif f2 == 3:
                sym, impls = _symbol_info(v2)
                if impls:
                    doc.implements[sym] = impls
        # generated files outside the checkout (e.g. scip-go's go-build test mains) aren't project code
        if not doc.path.startswith(("../", "/")):
            docs.append(doc)
    # overlapping projects (several tsconfigs) emit the same document more than once: merge by path
    merged: dict[str, Document] = {}
    for d in docs:
        m = merged.setdefault(d.path, Document(path=d.path))
        m.occurrences.extend(d.occurrences)
        for k, v in d.implements.items():
            m.implements.setdefault(k, [])
            m.implements[k].extend(x for x in v if x not in m.implements[k])
    for m in merged.values():
        seen, uniq = set(), []
        for o in m.occurrences:
            key = (o.line, o.start, o.end, o.symbol, o.roles)
            if key not in seen:
                seen.add(key)
                uniq.append(o)
        m.occurrences = uniq
    return list(merged.values())


def is_local(symbol: str) -> bool:
    return symbol.startswith("local ")


def _last_descriptor(symbol: str) -> str:
    """The last descriptor of a global symbol, respecting `backtick` escapes and [..] groups."""
    s = symbol.rstrip()
    if s.startswith("local "):
        return s
    # skip `scheme manager package version ` (4 space-separated fields; `.` is an empty field)
    parts = s.split(" ", 4)
    s = parts[4] if len(parts) == 5 else s
    last, i, depth, tick = 0, 0, 0, False
    while i < len(s):
        c = s[i]
        if c == "`":
            tick = not tick
        elif not tick:
            if c == "[":
                depth += 1
            elif c == "]":
                depth -= 1
            elif depth == 0 and c in "#/.:" and i + 1 < len(s):
                last = i + 1  # (a method's `().` also ends its descriptor: `outer().Inner#`)
        i += 1
    return s[last:]


def descriptor_kind(symbol: str) -> str:
    """Classify a global SCIP symbol by its last descriptor: method, accessor, type, term, macro, other."""
    s = symbol.rstrip()
    if s.endswith(")."):
        last = _last_descriptor(s)
        return "accessor" if "<get>" in last or "<set>" in last else "method"
    if s.endswith("#"):
        return "type"
    if s.endswith("!"):
        return "macro"
    if s.endswith("."):
        return "term"
    return "other"


def descriptor_name(symbol: str) -> str:
    """Name in the last descriptor: `pkg/Foo#bar().` -> `bar`, `pkg/Foo#` -> `Foo`,
    `impl#[`Result<T, E>`][Try]branch().` -> `branch`, `Foo#`<get>type`().` -> `type`."""
    d = _last_descriptor(symbol)
    for suf in ("().", "#", ".", "!", "/"):
        if d.endswith(suf):
            d = d[: -len(suf)]
            break
    if d.endswith(")") and "(" in d:  # disambiguator: bar(+1)
        d = d[: d.rfind("(")]
    # leading [Self][Trait] groups of rust-analyzer impl members
    while d.startswith("["):
        depth, tick, j = 0, False, 0
        for j, c in enumerate(d):
            if c == "`":
                tick = not tick
            elif not tick and c == "[":
                depth += 1
            elif not tick and c == "]":
                depth -= 1
                if depth == 0:
                    break
        d = d[j + 1:]
    d = d.strip("`")
    for pre in ("<get>", "<set>"):
        d = d.removeprefix(pre)
    return d
