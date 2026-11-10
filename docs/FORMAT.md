# The MCPK format

This document describes what an `.mcp` file contains and how `unmcpk` reverses
it. Everything here was derived from the shipped containers themselves
(`vanilla.mcp`, `vanilla_old.mcp`), by probing them and confirming every claim
against real decoded output.

There are five layers, outside in. Each one is independent, which is why the
decoder can probe them instead of hard-coding a single variant.

```
[MCPK container]              header + directory table + file index + bodies
  └─ [rotor cipher]           per-file byte permutation, keyed by a fixed seed
       └─ [zlib stream]       standard zlib, no encryption
            └─ [reversal]     reverse the buffer, xor the first 130 bytes
                 └─ [marshal] NetEase-mutated CPython 2.7 marshal
                      └─ [opcodes] permuted byte values, plus decoy objects
```

## 1. Container

All integers are little endian.

| offset | type   | meaning                                     |
| -----: | ------ | ------------------------------------------- |
| 0x00   | char4  | `MCPK`                                      |
| 0x0c   | u32    | directory table start                       |
| 0x10   | u32    | file index start                            |
| 0x14   | u32    | data start                                  |

The directory table is a run of 12-byte records:

| type | meaning                                       |
| ---- | --------------------------------------------- |
| u32  | directory id                                  |
| u32  | offset of this directory's file records, relative to the file index start |
| u32  | number of file records                        |

A file record is 16 bytes: `{ file id, data offset, size, unknown }`. The data
offset is relative to the data start.

**Names are not stored.** `file id` is `StringID(name)`, a one-way hash, so the
container cannot tell you what a file is called. The name has to be recovered
from the payload itself: every Python code object carries `co_filename`, which
is the path the file had when it was compiled (`lib/SunshineClient/Extension/
EditorExtension.py`, …). `unmcpk` rebuilds the tree from that and falls back to
`<dirid>_<fileid>` when a payload has no usable filename.

`StringID` is also used to special-case one file: `StringID("redirect.mcs") ==
0x8d7bfdc9`, whose body is obfuscated differently (see below).

## 2. Rotor cipher

The body is passed through a rotor machine — a chain of six 256-byte
substitution tables whose positions advance per byte. The tables are permuted at
startup from three 16-bit seeds produced by an LCG whose seed is derived from a
key string by five mixing equations.

The key string for all builds seen so far is

```
seed*4 + (a + seed + b)*5 + "$@" + a*7 + b*2 + "&]`"
  seed = "j2h56ogodh3sk"      (13 bytes)
  a    = "=dziaq;"            (7 bytes)
  b    = "|`o=5v7!\"-276"     (13 bytes)
```

which expands to 297 bytes. State is reinitialised on every region call, so the
transform is a pure function of `(key, data)` and can be run in parallel.

One caveat on the key schedule: the reference is written in Go, where `|` shares
precedence with `+`, `-` and `^`. So

```go
k1 = ((k1 << 3) | (k1 >> 13) + ki) & 65535
```

means `(rot(k1)) + ki`, **not** `(k1 << 3) | ((k1 >> 13) + ki)`. Getting this
wrong yields a key that decodes nothing at all — silently, since every layer
below simply fails.

## 3. zlib

Plain, standard zlib. Nothing to say.

## 4. Reversal

The inflated buffer is reversed, then the first 130 bytes are xored with `0x9c`:

```python
def reverse_data(buf):
    for i in range(min(130, len(buf))):
        buf[i] ^= 0x9C
    buf.reverse()
```

Note the order: xor first, *then* reverse. The function is therefore not an
involution — encoding is "reverse, then xor the head".

Exception: the `redirect.mcs` body has its first four bytes xored with `MCPK`
and is neither reversed nor truncated.

## 5. Payload layout

After un-reversal the payload is

```
[ marshal stream ][ 130 bytes of filler ][ FF FE ]
```

The filler is dead weight (its first two bytes are random); the trailing `FF FE`
marker is a constant. Both were true for 4354/4354 files in `vanilla.mcp` and
3904/3904 in `vanilla_old.mcp`. `unmcpk` does not hard-code 132: it parses the
marshal stream, and reports whatever is left plus whether it ends with the
marker. Files whose payload is empty after the marshal header (assets) are
reported separately rather than treated as failures.

## 6. NetEase marshal

A CPython 2.7 marshal stream with three kinds of modification.

### 6.1 New type tags

Strings and code objects get alternative tags, and there are three "versions" of
each, differing only in how the string bytes are encrypted:

| kind       | v0     | v1     | v2     |
| ---------- | ------ | ------ | ------ |
| string     | `0x08` | `0x17` | `0x6d` |
| interned   | `0x0f` | `0x1a` | `0x62` |
| unicode    | `0x0e` | `0x1d` | `0x31` |
| code       | `0x61` | `0x6f` | `0x4d` |

- v0 strings are xored byte-wise with `0x8d`.
- v1 strings are RC4 encrypted with key `A7 0D 37 7A`.
- v2 strings are RC4 encrypted with key `8D 06 E8 C8 B7 D7 B7 28 46 51 AE 04`.

Each string is encrypted with a fresh cipher, so nothing is chained. Only the
`interned` tags register in the interned table; a plain `R` (`0x52`) is a 32-bit
index into it. `unmcpk` expands those references into inline strings when
emitting a pyc, which makes the output self-contained.

Plain `s`, `u` and `t` values are *not* transformed.

### 6.2 Shuffled code objects

Three alternative field orders are used, all with the same members. Only the
four integers (`argcount`, `nlocals`, `stacksize`, `flags`, `firstlineno`) are
raw int32; **every other field is a full tagged marshal object**, exactly like
the standard layout:

| version | read order |
| ------- | ---------- |
| 0 (`0x61`) | lnotab, varnames, flags, freevars, cellvars, filename, stacksize, firstlineno, consts, argcount, code, nlocals, name, names, opcode type |
| 1 (`0x6f`) | nlocals, flags, consts, stacksize, varnames, argcount, freevars, names, cellvars, name, code, firstlineno, lnotab, opcode type, filename |
| 2 (`0x4d`) | argcount, lnotab, cellvars, firstlineno, varnames, consts, name, stacksize, freevars, names, code, flags, filename, nlocals, opcode type |

`vanilla.mcp` uses version 2 for every file; the others exist for other builds.

The `opcode type` is a 32-bit id naming the permutation applied to the bytecode
(see 6.3). Three ids are known: `0xca49b20f`, `0x00000000`, `0xbc58dbd5`. An
unknown id does not abort the decode — the file is reported and its bytecode is
left alone, so new builds fail loudly but locally.

### 6.3 Permuted opcodes

The byte values of CPython 2.7 opcodes are remapped per code object according to
the table named by the opcode type. Operands are untouched — they stay
little-endian, so they need no swapping. Only the opcode byte moves.

Decoding is a linear scan: translate the byte, and step 3 bytes if the
*translated* opcode has an argument (`>= 90` in 2.7) or 1 otherwise. That
predicate survives permutation, and it is what makes the scan unambiguous.

Translation tables live in `src/nemarshal/tables.rs`: each entry maps a NetEase
byte to the opcode name it stands for and to the CPython 2.7 byte that name
compiles to, so adding a new table is data, not code.

### 6.4 Decoy code objects

Almost every module carries one extra code object — names like
`update_version_01dVersi0n`, `encrypt_function_new_Version2`,
`foo_furtureVersion` — whose bytecode is random noise: unknown opcode bytes, no
terminator, operands pointing outside the constant pool. It is an anti-tamper
decoy; a decompiler that touches it fails on the whole module.

`unmcpk` validates each translated code object (opcode validity, operand bounds,
`LOAD_CONST`/name/local index bounds, ends with `RETURN_VALUE`) and replaces
failing ones with `return None`, keeping the name so cross references still
resolve. The manifest counts them (`decoys`, `decoy_code_objects`), and
`--keep-decoys` disables the replacement.

This is also the hack the old tool performed bluntly: it deleted the last code
object in the module's constant pool and shortened the module bytecode by nine
bytes, which corrupts the module's own code. `unmcpk` keeps that behaviour only
behind `--old-compat`, for reproducing old output.

### 6.5 Output

`unmcpk` writes plain CPython 2.7 pyc files (magic `03 F3 0D 0A`, 8-byte header)
containing standard marshal with the original structure: the same code object
tree, `co_filename` paths, `lnotab`, closure vars.

## 7. What is *not* known

- Whether `MCPK` itself has other versions with a different header; the two
  containers available share the layout exactly.
- The rotor key and opcode tables in builds older than `vanilla_old.mcp`; both
  containers use the same key and tables, so the probing path for new ones is
  untested against a real sample.
- The meaning of the fourth file record field.
