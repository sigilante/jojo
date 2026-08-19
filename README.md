## Jojo

Jojo is a very simple Jock REPL written on the NockApp stack.
Jojo is based on [Nojo by ixv](https://github.com/ixv/nojo).

Jojo now hosts the NEW Jock compiler's REPL kernel (jojo.jam is
built by that project's `tools/repl.sh`). Statements (lines
ending `;`) accumulate in kernel state; expressions evaluate
against the accumulated history; `:nock <expr>` reveals the
expression's compiled Nock formula; `exit` (or `:exit` / `:q` /
Ctrl+D) ends the session. Logging defaults to INFO (set RUST_LOG
to override).

Jojo boots any trap jam: pass the path as the first argument
(`cargo run -- /path/to/some.jam`; default `jojo.jam`), sealed
kernels from jock's `kern.sh --sealed` included. Each jam gets
its own NockApp instance (named by the file stem), so different
jams never adopt each other's checkpointed state.

Multi-line input works: while an entry's brackets stay open the
prompt continues (`  ... `) and lines accumulate, submitting as
one entry when they balance. A blank line at the continuation
prompt cancels the pending entry. (Brackets inside string and
char literals and behind `//` comments don't count.)

### Install & Run

1. Install the Rust tool stack, such as `cargo`.

2. Install `hoonc` from [Nockchain](https://github.com/zorp-corp/nockchain).

3. Run `make` to build and run Jojo.

4. Type `exit` (or press Ctrl+D) to end the session.

### Examples

```
jojo> 1 + 41
42

jojo> let a = 5; a + 37
42

jojo> func fact(n: @) -> @ {
  ...   if n == 0 { 1 }
  ...   else { n * fact(n - 1) }
  ... };
ok
jojo> fact(5)
120

jojo> func fib(n:Uint) -> Uint { if n == 0 { 1 } else if n == 1 { 1 } else { $(n - 1) + $(n - 2) } }; ( fib(0) fib(1) fib(2) fib(3) fib(4) fib(5) fib(6) fib(7) fib(8) fib(9) fib(10) )
[1 1 2 3 5 8 13 21 34 55 89]
```

### Development

**The committed `jojo.jam` is the source of truth for the kernel.** It
is the Jock REPL kernel built by that project's `tools/repl.sh` (the
current 9-file honk-codex compiler:
`jock`/`mint`/`parse`/`lex`/`sugar`/`mold`/`check`/`expand`/`nockasm`),
and it is checked in (un-ignored) so a fresh clone runs as-is.

**It is the module-free build, deliberately** — verified to contain no
source text at all (the compiler rides as compiled Nock; `import hoon`
is the pinned boot library). A REPL built with `--data-dir` modules
carries those modules' *sources* in the jam (runtime `import`
recompiles from source), so a module-bearing jam is for private use,
not for sharing. `import parser` / `import urbit` are therefore not
available in the committed jam.

To refresh it after a compiler change, rebuild from a jock checkout and
copy it in:

    ( cd ~/jock && zsh tools/repl.sh /tmp/repl.jam )
    cp /tmp/repl.jam jojo.jam

NOTE: the bundled `hoon/lib/jock.hoon` + `hoon/apps/jojo.hoon` + the
`make`/`hoonc` path are the **old single-file `jockt`-era compiler** and
are stale (no `peekContext`); they are kept only for reference. Do not
iterate there — rebuild `jojo.jam` from the jock repo as above.
