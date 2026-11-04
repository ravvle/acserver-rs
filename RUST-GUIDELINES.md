# Rust Coding Guidelines

## Goal

Write Rust that a developer who knows Go, Java, or TypeScript (but not Rust) can read and follow. The code should still be idiomatic, efficient, and performant, but **readability wins over cleverness**. Use the simplest Rust feature that solves the problem well. Reach for advanced features only when they are strictly needed, and explain why when you do.

Let the compiler do as much work as possible: memory safety, type inference, exhaustiveness checks, and resource cleanup should come from the language, not from hand-written logic.

## Core principles

1. **Simple over clever.** If a plain `for` loop is clearer than a long iterator chain, use the loop.
2. **Ownership should be easy to follow.** Prefer owned data and clear borrowing over complex lifetime relationships.
3. **The compiler is your safety net.** Write code so that mistakes become compile errors, not runtime bugs.
4. **Performance where it matters.** Avoid obvious waste, but don't sacrifice readability for micro-optimizations without evidence (profiling or benchmarks).
5. **Complexity must be justified and contained.** When an advanced feature is genuinely required, isolate it and document it (see "Escape hatch" below).

## Ownership and borrowing

- Structs should own their data (`String`, `Vec<T>`, `HashMap<K, V>`) rather than hold references. Avoid structs with lifetime parameters (`struct Foo<'a>`) unless there is a clear, measured performance need.
- Function parameters: borrow when you only read (`&str`, `&[T]`, `&MyStruct`), take ownership when you store or consume the value. This is idiomatic and easy to understand.
- Return owned values from functions (`String`, `Vec<T>`) rather than references, unless returning a reference is obvious and simple (e.g. a getter returning `&str`).
- Clone small values (short strings, small structs, IDs) freely when it makes the code simpler. Do not fight the borrow checker with complex restructuring just to avoid a cheap clone.
- Never clone collections (`Vec`, `HashMap`, etc.) or large structs inside loops or per-request/hot paths without a comment explaining why. Borrow or move them instead.
- Do not write explicit lifetime annotations unless the compiler requires them. Rely on lifetime elision.

## Types and data modeling

- Use `enum` to represent a fixed set of states or variants. Prefer enums over strings, integers, or boolean flags for state.
- Use `struct` with named fields. Avoid tuple structs except for simple newtype wrappers (e.g. `struct UserId(u64)`).
- Use `Option<T>` instead of sentinel values (empty strings, `-1`, etc.).
- Derive common traits instead of implementing them by hand: `#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]` as needed, plus `serde` derives where relevant.
- When matching on your own enums, list the variants explicitly instead of using a `_ =>` catch-all, so the compiler flags every `match` that needs updating when a variant is added.

## Generics and traits

- Simple generics are fine: `fn print_all<T: Display>(items: &[T])` or `fn process(input: impl AsRef<str>)`.
- Prefer `impl Trait` in argument position over named generic parameters when the type is used only once.
- Keep trait bounds short. If a `where` clause has more than two or three bounds, reconsider the design.
- Use traits for real polymorphism (multiple implementations that are swapped). For a closed set of variants, use an `enum` instead.
- Only when you actually need runtime polymorphism (choosing an implementation at runtime, or storing mixed implementations in one collection), use `Box<dyn Trait>` / `Arc<dyn Trait>` rather than heavy generics; it is easier to read and the cost is usually negligible. If a simple generic or an `enum` does the job, use that instead.
- Avoid: higher-ranked trait bounds (`for<'a>`), generic associated types, complex associated-type chains, blanket implementations on foreign traits, and type-level programming (typestate, `PhantomData` tricks) unless strictly needed.

## Error handling

- Use `Result<T, E>` and the `?` operator for all recoverable errors.
- Libraries/modules with a public API: define error enums with `thiserror`.
- Application code (binaries, top-level handlers): use `anyhow::Result` and add context with `.context("what was being attempted")`.
- Do not use `.unwrap()` in non-test code. If a value is guaranteed by an invariant, use `.expect("reason this cannot fail")` with a clear message.
- Do not use `panic!` for expected failure cases.

## Control flow and iterators

- `match`, `if let`, and `let ... else` are all encouraged; they are readable and checked by the compiler.
- Avoid deeply nested patterns. If a `match` arm destructures more than two levels, split it up.
- Short iterator chains (`iter().filter().map().collect()`) are fine. If a chain exceeds roughly four adapters or contains complex closures, use a `for` loop or break it into named steps.
- Add type annotations when inference makes the type unclear to a reader, e.g. `let names: Vec<String> = ...` instead of relying on turbofish deep in a chain.

## Shared state and concurrency

- Prefer passing data by ownership or message passing (channels) over shared mutable state.
- When shared mutable state is required across threads, use `Arc<Mutex<T>>` or `Arc<RwLock<T>>`. Keep lock scopes short.
- Avoid `Rc<RefCell<T>>` in normal code. If you need it, it usually signals a design that should be restructured (e.g. using IDs/indices into a collection instead of references).
- For async code, use `tokio` and plain `async fn`. Do not implement `Future` manually, and avoid `Pin`, custom executors, or hand-written poll logic.

## Things to avoid unless strictly needed

- `unsafe` code of any kind
- Custom macros (`macro_rules!` or procedural macros). Using standard and well-known derive macros is fine.
- Explicit lifetimes beyond what the compiler requires
- `Pin`, manual `Future` implementations
- Const generics, GATs, HRTBs, specialization-style tricks
- `Deref` implementations to fake inheritance
- `Cow`, arena allocators, and zero-copy parsing, unless profiling shows a real need
- Overusing `impl` blocks on generic types with complex bounds

## Performance guidelines

- Accept `&str` and `&[T]` in function parameters instead of `&String` or `&Vec<T>`.
- Pre-allocate with `Vec::with_capacity` / `String::with_capacity` when the size is known.
- Avoid cloning large data structures in loops or hot paths.
- Prefer stack values and plain structs over boxing unless boxing is needed.
- Do not optimize speculatively. If an optimization makes code harder to read, it needs a benchmark or profiling result to justify it, noted in a comment.

## Escape hatch: when complexity is necessary

If an advanced feature is genuinely required (performance, safety, API constraints):

1. Keep it in the smallest possible scope (a single function or module).
2. Expose a simple interface so callers don't need to understand the internals.
3. Add a comment starting with `// COMPLEXITY:` explaining what the feature does in plain terms and why the simpler alternative wasn't sufficient.
4. Any `unsafe` block must also have a `// SAFETY:` comment explaining why it is sound.

## Readability

- Use descriptive names. Single-letter names are fine only for short closures and loop indices.
- Keep functions short and focused; one function, one job.
- Comment the *why*, especially for Rust-specific choices a non-Rust developer might not expect (e.g. why something is cloned, why a value is wrapped in `Arc`).
- Write doc comments (`///`) on public functions, structs, and enums.

## Tooling

- All code must pass `cargo fmt` and `cargo clippy` with no warnings (`cargo clippy -- -D warnings`).
- Do not silence clippy lints with `#[allow(...)]` without a comment explaining why.
- Prefer the standard library and well-established crates (`serde`, `tokio`, `anyhow`, `thiserror`, `tracing`). Avoid adding niche dependencies for small conveniences.
