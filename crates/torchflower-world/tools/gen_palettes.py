#!/usr/bin/env python3
"""Generate the embedded block palettes in data/palettes/ from pmmp/BedrockData.

BedrockData is CC0-1.0: https://github.com/pmmp/BedrockData

Every block in canonical_block_states.nbt enumerates its states as a full
cartesian product of its property values, using a per-block property
significance order. That lets the whole palette be stored as block names,
property value lists and that order, which shrinks 2+ MB of NBT to ~14 KB.

The loader is `torchflower_world::palette_data`; the format is documented
there. Identical palettes across versions are written once.

Usage (network access required):
    python3 tools/gen_palettes.py
"""
import collections
import hashlib
import itertools
import json
import struct
import urllib.parse
import urllib.request
import zlib
from pathlib import Path

# (BedrockData tag, first protocol version that uses this palette).
#
# Protocol numbers are those of the game releases (they match gophertunnel's
# `CurrentProtocol` for each version). BedrockData's own protocol_info.json is
# only cross-checked, not trusted: in the bedrock-1.21.60 and bedrock-1.21.70
# tags it was not updated and still names the previous release, and 1.21.100
# is only published under a semver tag. 1.21.130 is listed with 897, the
# build BedrockData was generated from; the 898 release uses the same palette.
RELEASES = [
    ("bedrock-1.21.50", 766),
    ("bedrock-1.21.60", 776),
    ("bedrock-1.21.70", 786),
    ("bedrock-1.21.80", 800),
    ("bedrock-1.21.90", 818),
    ("bedrock-1.21.93", 819),
    ("6.0.0+bedrock-1.21.100", 827),
    ("bedrock-1.21.111", 844),
    ("bedrock-1.21.120", 859),
    ("bedrock-1.21.130", 897),
    ("bedrock-1.26.0", 924),
    ("bedrock-1.26.10", 944),
    ("bedrock-1.26.20", 975),
    ("bedrock-1.26.30", 1001),
]
BASE = "https://raw.githubusercontent.com/pmmp/BedrockData/{tag}/{file}"
OUT = Path(__file__).resolve().parent.parent / "data" / "palettes"


def fetch(tag, name):
    url = BASE.format(tag=urllib.parse.quote(tag, safe=""), file=name)
    with urllib.request.urlopen(url) as r:
        return r.read()


# --- network NBT reader -------------------------------------------------------

def varu(b, i):
    v = s = 0
    while True:
        c = b[i]
        i += 1
        v |= (c & 0x7F) << s
        s += 7
        if c < 0x80:
            return v, i


def vari(b, i):
    v, i = varu(b, i)
    return (v >> 1) ^ -(v & 1), i


def nstring(b, i):
    n, i = varu(b, i)
    return b[i:i + n].decode(), i + n


def payload(b, i, t):
    if t == 1:
        return b[i], i + 1
    if t == 2:
        return struct.unpack_from("<h", b, i)[0], i + 2
    if t in (3, 4):
        return vari(b, i)
    if t == 5:
        return struct.unpack_from("<f", b, i)[0], i + 4
    if t == 8:
        return nstring(b, i)
    if t == 10:
        entries = []
        while True:
            tt = b[i]
            i += 1
            if tt == 0:
                return entries, i
            k, i = nstring(b, i)
            v, i = payload(b, i, tt)
            entries.append((k, tt, v))
    raise ValueError(f"unsupported NBT tag {t}")


def parse_states(raw):
    i = 0
    states = []
    while i < len(raw):
        t = raw[i]
        i += 1
        _, i = nstring(raw, i)
        v, i = payload(raw, i, t)
        d = {k: (tt, val) for k, tt, val in v}
        states.append((d["name"][1], d.get("states", (10, []))[1], d.get("version", (3, 0))[1]))
    return states


# --- compact encoder ----------------------------------------------------------

def put_varu(out, v):
    while v >= 0x80:
        out.append((v & 0x7F) | 0x80)
        v >>= 7
    out.append(v)


def encode(states):
    groups = collections.OrderedDict()
    tags = {}
    versions = set()
    for name, st, version in states:
        groups.setdefault(name, []).append(st)
        versions.add(version)
        for k, tt, _ in st:
            tags[k] = tt
    if len(versions) != 1:
        raise ValueError(f"expected one state version, got {versions}")

    blocks = []
    for name, lst in groups.items():
        keys = [k for k, _, _ in lst[0]]
        vals = {k: [] for k in keys}
        for st in lst:
            if [k for k, _, _ in st] != keys:
                raise ValueError(f"{name}: inconsistent property keys")
            for k, _, v in st:
                if v not in vals[k]:
                    vals[k].append(v)
        got = [tuple(v for _, _, v in st) for st in lst]
        order = None
        for perm in itertools.permutations(range(len(keys))):
            expected = []
            for combo in itertools.product(*[vals[keys[p]] for p in perm]):
                t = [None] * len(keys)
                for p, v in zip(perm, combo):
                    t[p] = v
                expected.append(tuple(t))
            if expected == got:
                order = perm
                break
        if order is None:
            raise ValueError(f"{name}: states are not a product enumeration")
        blocks.append((name, keys, vals, order))

    strings = collections.OrderedDict()
    keytab = collections.OrderedDict()

    def sid(s):
        return strings.setdefault(s, len(strings))

    for name, keys, vals, _ in blocks:
        sid(name)
        for k in keys:
            sid(k)
            keytab.setdefault(k, len(keytab))
            if tags[k] == 8:
                for v in vals[k]:
                    sid(v)

    out = bytearray(b"TFBP")
    out.append(1)
    put_varu(out, next(iter(versions)))
    put_varu(out, len(strings))
    for s in strings:
        b = s.encode()
        put_varu(out, len(b))
        out += b
    put_varu(out, len(keytab))
    for k in keytab:
        put_varu(out, strings[k])
        out.append(tags[k])
    put_varu(out, len(blocks))
    for name, keys, vals, order in blocks:
        put_varu(out, strings[name])
        put_varu(out, len(keys))
        for k in keys:
            put_varu(out, keytab[k])
            put_varu(out, len(vals[k]))
            for v in vals[k]:
                if tags[k] == 1:
                    out.append(v & 0xFF)
                elif tags[k] == 3:
                    put_varu(out, ((v << 1) ^ (v >> 31)) & 0xFFFFFFFF)
                else:
                    put_varu(out, strings[v])
        out += bytes(order)
    return bytes(out), len(states), hashlib.sha256(bytes(out)).hexdigest()[:16]


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.glob("*.bin"):
        old.unlink()
    protocols = [p for _, p in RELEASES]
    if protocols != sorted(set(protocols)):
        raise ValueError("RELEASES must be sorted by protocol without duplicates")
    index = []
    seen = {}
    for tag, protocol in RELEASES:
        label = tag.split("+")[-1]
        info = json.loads(fetch(tag, "protocol_info.json"))["version"]
        stated = info["protocol_version"]
        if stated != protocol:
            print(
                f"note: {tag} protocol_info.json says {info['major']}.{info['minor']}."
                f"{info['patch']} / {stated}; using {protocol}"
            )
        raw, count, digest = encode(parse_states(fetch(tag, "canonical_block_states.nbt")))
        if digest not in seen:
            path = OUT / f"{label}.bin"
            path.write_bytes(zlib.compress(raw, 9))
            seen[digest] = path.name
        index.append((protocol, tag, seen[digest], count))
        print(f"{tag}: protocol {protocol}, {count} states -> {seen[digest]}")
    consts = {}
    for _, _, file, _ in index:
        if file not in consts:
            consts[file] = "P_" + file.removesuffix(".bin").upper().replace("-", "_").replace(".", "_")
    lines = [
        "// @generated by tools/gen_palettes.py from pmmp/BedrockData (CC0-1.0).",
        "// Do not edit by hand.",
        "",
    ]
    for file, const in consts.items():
        lines.append(f'static {const}: &[u8] = include_bytes!("../data/palettes/{file}");')
    lines += [
        "",
        "/// `(first protocol, BedrockData tag, zlib-compressed palette, state count)`,",
        "/// sorted by protocol.",
        "#[rustfmt::skip]",
        "pub(crate) static PALETTES: &[(i32, &str, &[u8], usize)] = &[",
    ]
    for protocol, tag, file, count in index:
        lines.append(f'    ({protocol}, "{tag}", {consts[file]}, {count}),')
    lines.append("];")
    (OUT.parent.parent / "src" / "palette_index.rs").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
