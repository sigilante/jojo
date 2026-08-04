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

(Jojo does not support multi-line input. Since Jock is
whitespace-agnostic, fold programs onto one line.)

### Install & Run

1. Install the Rust tool stack, such as `cargo`.

2. Install `hoonc` from [Nockchain](https://github.com/zorp-corp/nockchain).

3. Run `make` to build and run Jojo.

4. Type `exit` (or press Ctrl+D) to end the session.

### Examples

As mentioned above, Jojo does not support multi-line input yet,
so simply turn newlines into spaces or tabs.

```
jojo> 1 + 41
42

jojo> let a = 5; a + 37
42

jojo> func fib(n:Uint) -> Uint { if n == 0 { 1 } else if n == 1 { 1 } else { $(n - 1) + $(n - 2) } }; ( fib(0) fib(1) fib(2) fib(3) fib(4) fib(5) fib(6) fib(7) fib(8) fib(9) fib(10) )
[1 1 2 3 5 8 13 21 34 55 89]
```

### Development

If you are hacking on Jock, then you should only need to update
`/lib/jock.hoon` and rebuild using `make` to iterate.
