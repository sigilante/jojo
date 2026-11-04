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

The prompt is `jojo> ` unless `--prompt STR` says otherwise, and a
kernel moves it with a `[%prompt cord]` effect: the prompt it names
shows from the next line on. Atalante's kernel (`atalante.jam`, see
below) uses this for its apple prompts. An empty answer prints no
line.

Jojo boots any trap jam: pass the path as the first argument
(`cargo run -- /path/to/some.jam`; default `jojo.jam`), sealed
kernels from jock's `kern.sh --sealed` included. Each jam gets
its own NockApp instance (named by the file stem), so different
jams never adopt each other's checkpointed state.

The prompt is a real line editor (rustyline). While an entry's
brackets stay open, Enter inserts a newline instead of
submitting, so a multi-line entry is written and edited as one
buffer; it submits when the brackets balance. Pasting a block
works the same way — the paste lands whole, trailing blank
lines and all, and you can fix it up before pressing Enter.
Up/Down recall previous entries (a multi-line entry comes back
whole, ready to edit and resubmit); Ctrl-C abandons the current
buffer; Ctrl-D or `exit` quits. History persists in
`.jojo_history` (one file per jam instance). Brackets inside
string and char literals and behind `//` comments don't count.

### Atalante (ATL/APL)

`atalante.jam` is the REPL kernel of [Atalante](https://github.com/sigilante/atalante),
an APL written in Jock and compiled to Nock. Boot it with the red apple
as the opening prompt; `)TOGGLE` switches to the green prompt, which
prints a line's tree instead of evaluating it:

```
$ cargo run --release -- --prompt '🍎> ' atalante.jam
🍎> (~R ∊ R ∘.× R) / R ← 1↓⍳R
🍎> {⍵≤1: 1 ⋄ ⍵×∇ ⍵-1} 5
120
🍎> X ← 'sing' 'goddess' 'sing' 'of' 'the' 'rage'
🍎> X[⍋X]
'goddess' 'of' 'rage' 'sing' 'sing' 'the'
```

A dfn that spans lines is typed as jojo's own continuation: an open
`{` keeps reading until the braces close, and the kernel receives the
lines as one entry (Atalante's D-69).

`atalante.manifest` names the kernel's sha256 and every module source the
unsealed jam embeds: Atalante's own five, and its two Hoon packages. The
Jock compiler rides as compiled Nock, as in the other jams here.

Under `)ASCII ON` every glyph has a word (`2 3 rho iota 6`), and `⎕TYPE←'UINT8 REAL32'` sets the width each kind is made at.

### One-shot checks (the agent lane)

`jojo <kernel.jam> --check file.jock` boots the kernel, pokes its
`%check` cause with the file's text, prints one JSON object (the
compiler's `+chkj` schema, pinned in the jock corpus), and exits
0 if the program typechecks, 1 on a refusal:

```
$ jojo jojo.jam --check prog.jock
{"status":"refused","tag":"mill-nest","line":3,"col":4,"report":[...]}
```

This is the *warm* check lane: the kernel checkpoint carries the
runtime's cold state, so the check runs jetted and answers in
under a second — against ~5 s per call for jock's batch
`tools/check.sh`, which must rebuild per call (a shipped
pre-evaluated checker would run jetless). Checks never touch the
session history.

**One session per jam instance.** Two concurrent `jojo` processes
on the same jam boot and run without an error, but they race the
shared checkpoint: measured live, a third session after two
coincident ones found NEITHER session's history — the torn
checkpoint fell back to empty state. Run one session per
instance (`--check` one-shots against an idle instance are fine);
copies of a jam under different file stems get separate instances.

### Install & Run

1. Install the Rust tool stack, such as `cargo`.

2. Install `hoonc` from [Nockchain](https://github.com/zorp-corp/nockchain).

3. Run `make` to build and run Jojo.

4. Type `exit` (or press Ctrl+D) to end the session.

### The runtime and its jets

The NockVM and NockApp crates are vendored from
[nockchain](https://github.com/nockchain/nockchain) at `b9dd5af`
(2026-08-30), the revision numerics' NockApp jet harness pins, with
jojo's scry roof applied in `crates/nockapp/src/kernel/form.rs`. Jojo
boots every kernel with a hot table holding NockVM's own jets, plus
the jets from numerics' `lagoon/nockapp` harness. `crates/hoon-float-jets`
jets the add, sub, mul, div, sqt and comparison arms of hoon-138's
`rh rs rd rq` doors. `crates/lagoon-jets` jets 28 `/lib/lagoon` arms on
`%i754` rays. Both are bit-exact over
[sdfloat/sdblas](https://github.com/sigilante/RustFloat). A kernel's
floating point therefore runs jetted. Atalante's float `+/` over 1e5
elements drops from 53 s to 29 s.

- `HOON_FLOAT_JET_DISABLE=1` leaves the float doors to the Nock, for a comparison run.
- `NOCK_TEST_JETS=k.138/one/two/tri/rd/add,...` runs jet and Nock side by side and bails on any mismatch.
- `HOON_FLOAT_JET_TRACE=1` and `LAGOON_JET_TRACE=1` print one line per jet call.

The toolchain is the nightly in `rust-toolchain.toml`. A checkpoint
written by the old runtime is untested under this one, so boot a new
kernel jam under a fresh stem.

### Examples

```
jojo> 1 + 41
42

jojo> let a = 5; a + 37
42

jojo> func fact(n: @) -> @ {
  if n == 0 { 1 }
  else { n * fact(n - 1) }
};
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

Pass no `--data-dir`:  the committed jam is the module-free build, and
that is checkable rather than assumed --- the module-bearing variant is
~82 KB larger, which is the size of `lib/*.jock` (~81 KB) riding along
as source.

**Clear the instance state when the prelude's surface changes
incompatibly.** Kernel state is the accumulated statement history, as
TEXT, and the new compiler recompiles it on adoption --- so history
written against an older prelude can stop compiling and take every
later expression down with it.  The `trip()` to `bytes()` rename (jock
2026-09-25) is exactly that kind of change, and so is sealing a library
type: `rational.RatState` and `interval.IvState` became `private` at
jock v0.1.0-beta-rc, so stored history that names or constructs either
one no longer compiles.  Instances are keyed by the
jam's FILE STEM, so refreshing `jojo.jam` in place keeps the old
`.data.jojo`:

    rm -rf .data.jojo      # or: make clean

**`jojo-chain.jam` is the same kernel with the `chain` package
compiled in**, so `import chain;` works at the prompt. Jojo has no
runtime import path; a package reaches the REPL only by being built
into the kernel.

    cargo run -- jojo-chain.jam
    jojo> import chain;
    ok
    jojo> chain.tob58(chain.hashnoun(1))
    "6cpvkSJWCMfHpH4KpEJ6ReAXNb7HWMWjte5AbaXdhb4YuJkmwiYGsn2"

It is shareable where a `lib/*.jock`-bearing build is not. A package
is the compiled construction formula (jock `tools/pkg.sh`), not
source: the kernel is 3,678,920 bytes larger than `jojo.jam`, and
the package jam alone is 3,677,816 --- the 1,104-byte remainder is the
lsrc wrapper, and that the two track each other is the check worth
re-running after a refresh. The formula runs at boot, so
the chain batteries register with `%fast` and run jetted; boot
takes a few seconds longer. It keeps its own instance state in
`.data.jojo-chain`.

`jojo-chain.manifest` pins the package and kernel sha256s. **Rebuild
from a clean jock master checkout.** A dirty tree gives a different
package, and jock's rule is that a jam which cannot be rebuilt
bit-identically does not ship:

    ( mkdir -p /tmp/chainpkg && cd ~/jock && zsh tools/pkg.sh pkg/chain-core.hoon /tmp/chainpkg/chain.jam &&
      zsh tools/repl.sh --data-dir /tmp/chainpkg --manifest /tmp/chain.man /tmp/repl-chain.jam )
    cp /tmp/repl-chain.jam jojo-chain.jam
    shasum -a 256 /tmp/chainpkg/chain.jam jojo-chain.jam   # compare to the manifest

The data-dir must hold `chain.jam` and nothing else. Any `*.jock` in
it would ride along as source. Do not reuse
`~/jock/.chain-pkg/chain.jam` either: it is a local build that goes
stale when `pkg/chain-core.hoon` changes. Refresh both jams together
so they stay on the same compiler.

NOTE: the bundled `hoon/lib/jock.hoon` + `hoon/apps/jojo.hoon` + the
`make`/`hoonc` path are the **old single-file `jockt`-era compiler** and
are stale (no `peekContext`); they are kept only for reference. Do not
iterate there — rebuild `jojo.jam` from the jock repo as above.
