# 3. Rust essentials, as this codebase uses them

This chapter is not a Rust course. It covers the concepts you will meet in this repo, each with a
short real excerpt and what it means here. For depth, *The Rust Programming Language* ("the book",
free online) is the standard reference; chapters 4 (ownership), 6 (enums), 9 (errors), 10 (traits,
lifetimes) and 16 (concurrency) match the sections below.

Contents:

- [Reading Rust syntax quickly](#reading-rust-syntax-quickly)
- [Strings, vectors, slices and other everyday syntax](#strings-vectors-slices-and-other-everyday-syntax)
- [Ownership, borrowing, `Clone` and `Arc`](#ownership-borrowing-clone-and-arc)
- [`Result`, `?` and the error types](#result--and-the-error-types)
- [`Option`](#option)
- [Enums and `match`](#enums-and-match)
- [Structs and `impl`](#structs-and-impl)
- [Traits, generics and `dyn`](#traits-generics-and-dyn)
- [Modules and visibility](#modules-and-visibility)
- [`async`/`.await` with tokio](#asyncawait-with-tokio)
- [Locks: `Mutex`, and why they are held briefly](#locks-mutex-and-why-they-are-held-briefly)
- [serde and exact JSON](#serde-and-exact-json)
- [Lifetimes, as far as this code needs them](#lifetimes-as-far-as-this-code-needs-them)
- [Tests and `#[cfg(test)]`](#tests-and-cfgtest)
- [Macros you will see](#macros-you-will-see)
- [`unsafe`: where and why](#unsafe-where-and-why)

## Reading Rust syntax quickly

| You see | It means |
|---|---|
| `let x = …;` / `let mut x = …;` | A binding. Bindings are immutable unless marked `mut`. |
| `fn f(a: &str) -> i32 { … }` | A function. The last expression without `;` is the return value. |
| `a::b::C` | A path: module `a`, submodule `b`, item `C`. `crate::` is this crate's root, `super::` the parent module. |
| `x.method()` / `Type::function()` | A method call on a value, or an associated function (like a static method). `Type::new()` is the usual constructor. |
| `&x`, `&mut x` | A shared borrow and an exclusive borrow of `x` (see below). |
| `'a`, `'static` | Lifetime names (see [Lifetimes](#lifetimes-as-far-as-this-code-needs-them)). |
| `#[…]` above an item, `#![…]` at the top of a file | Attributes: compiler instructions such as `#[derive(Clone)]`, `#[test]`, `#[allow(…)]`. |
| `name!(…)` | A macro call, for example `format!`, `info!`, `tokio::select!`. |
| `\|x\| x + 1`, `move \|\| …` | A closure (an anonymous function). `move` makes it take ownership of what it uses. |
| `///` and `//!` | Doc comments for the next item, and for the enclosing module. They are rendered by `cargo doc`. |

## Strings, vectors, slices and other everyday syntax

A few basics appear on almost every page of this repo.

**`String` and `&str`.** `String` is an owned, growable piece of text. `&str` is a borrowed view of
text that someone else owns, for example a string literal or part of a `String`. Functions usually
take `&str` (they only need to read) and return `String` (the caller gets to keep it):

```rust
// crates/xorgate-agent/src/cli.rs:65
pub fn usage(version: &str) -> String {
```

`.to_string()` (or `.to_owned()`, `String::from`) copies a `&str` into a new `String`, as in
`request_id: request_id.to_string()` (`crates/xorgate-core/src/commands.rs:262`). `.as_str()` goes
the other way. Struct fields that must outlive their input hold `String`.

**`Vec<T>` and slices.** `Vec<T>` is an owned, growable array. `&[T]` is a borrowed **slice**: a
view of some elements of a `Vec`, an array or another slice. A function that only reads a list takes
a slice, so it works with any of them. `&[u8]` is a slice of raw bytes, which is how MQTT payloads
arrive (`parse_command(payload: &[u8])`, `commands.rs:247`). Ranges cut slices out of slices:

```rust
// crates/xorgate-agent/src/cli.rs:41-55 (trimmed)
pub fn split_command(av: &[String]) -> (Option<String>, Vec<String>) {
    // …
        let mut rest = av[..i].to_vec();
        rest.extend_from_slice(&av[i + 1..]);
        return (Some(a.to_string()), rest);
```

`av[..i]` is "elements before `i`", `av[i + 1..]` "everything after `i`", and `.to_vec()` copies a
slice into a new `Vec`. Indexing out of range panics, so the code checks lengths first. The function
also returns a **tuple**, `(Option<String>, Vec<String>)`, which the caller unpacks with
`let (cmd, rest) = split_command(argv);`.

**`impl Trait` in argument position** means "any type that implements this trait". It saves the
caller a conversion:

```rust
// crates/xorgate-agent/src/state.rs:143
    pub fn note(&self, level: ActivityLevel, msg: impl Into<String>, now: i64) {
```

`note` accepts a `&str` literal or a `String` from `format!`, and calls `msg.into()` once inside.
`std_command(program: impl AsRef<OsStr>)` (`crates/xorgate-hal/src/child.rs:194`) accepts anything
that can be viewed as an OS string: a `&str`, a `String`, a `Path`.

**`as` casts** convert between number types explicitly. Rust never converts numbers implicitly.

```rust
// crates/xorgate-agent/src/lib.rs:35
        .map(|d| d.as_millis() as i64)
```

`as` never fails: converting to a smaller integer cuts off the high bits, and converting a float
to an integer drops the fraction and saturates at the type's limits. The code uses it only where the
value is known to fit (`m.qos as u8` in `mqtt_plane.rs:299`, `clamp_round(…) as u32` in
`crates/xorgate-core/src/settings.rs:515` after clamping). Where a value might not fit, use
`u32::try_from(x)`, which returns a `Result`.

**`..` in patterns** means "the rest, which I do not need": `MqttEvent::Connected { .. }`
(`mqtt_plane.rs:302`) matches the variant whatever its fields hold. In a struct literal,
`..other` fills the remaining fields from another value (see
[Structs and `impl`](#structs-and-impl)), and in a range (`..i`, `i..`) it is one open end.

**The turbofish `::<T>`** names a generic type when the compiler cannot infer it from context:

```rust
// crates/xorgate-core/tests/cameras_fixtures.rs:22
    let f = common::fixture::<Case>("cameras/enumeration.json");
```

Other examples: `.collect::<Vec<_>>()` (`crates/xorgate-agent/src/worker.rs:156`; `_` lets the
compiler fill in the element type) and `mpsc::unbounded_channel::<Vec<u8>>()`
(`crates/xorgate-agent/src/config_plane/mod.rs:194`).

**Raw and byte strings.** `r#"…"#` is a raw string: backslashes and double quotes inside are taken
literally, which is why the tests write JSON this way (`worker.rs:1050`). `b"…"` is a byte string, a
`&[u8; N]` instead of a `&str`, and `br#"…"#` combines both, used to fake raw MQTT or HTTP bodies
in tests (`br#"{}"#` in `crates/xorgate-cloud/src/iot_creds.rs:476`).

## Ownership, borrowing, `Clone` and `Arc`

Every value in Rust has exactly one **owner**, the variable or field that holds it. When the owner
goes out of scope the value is dropped (freed, files closed, children killed, whatever its `Drop`
does). Passing a value by value **moves** it: the old variable can no longer be used. To let code
use a value without taking it, you **borrow** it:

- `&T` is a shared borrow: read-only, and any number may exist at once;
- `&mut T` is an exclusive borrow: read-write, and only one may exist, with no shared borrows at
  the same time.

The compiler checks these rules at compile time (the "borrow checker"). That is how Rust rules out
data races and use-after-free without a garbage collector.

Most functions in this repo borrow their inputs. `ensure_home` only needs to look at the path, so it
takes `&Path`:

```rust
// crates/xorgate-agent/src/worker.rs:131-137
fn ensure_home(home: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .with_context(|| format!("create XORGATE_HOME {}", home.display()))
}
```

Methods declare how they use their object. In `StateStore`, a reader takes `&self` and a writer
takes `&mut self`, so the compiler knows which calls can change the state:

```rust
// crates/xorgate-agent/src/state.rs:42-52
    pub fn snapshot(&self) -> &StateSnapshot {
        &self.snap
    }

    /// `setConnection` (`state.ts:85-90`): `connectionSince` moves only on a change.
    pub fn set_connection(&mut self, connection: ConnectionState, now: i64) {
        if self.snap.connection != connection {
            self.snap.connection = connection;
            self.snap.connection_since = now;
        }
    }
```

**`Clone` and `Copy`.** `.clone()` makes an explicit copy. Types opt in with `#[derive(Clone)]`.
Small plain values such as `RunMode`, `ConnectionState` or `bool` also derive `Copy`, which means
they are copied implicitly instead of moved. If you see `.clone()` on a `String` or `Vec`, that is a
real copy of the data.

**`Arc` (atomically reference-counted pointer).** Many parts of the agent need the same object at
once: the state file, the clock, the MQTT handle. `Arc<T>` puts one `T` on the heap with a counter.
Cloning an `Arc` copies the pointer and bumps the counter. It does not copy `T`. The value is
dropped when the last `Arc` goes away. `Arc` is safe to share between threads, which matters because
tokio runs tasks on several threads.

That is why `AgentCtx`, the context every subsystem receives, is documented as "cheap to clone":
all its fields are `Arc`s or handles that wrap one (`crates/xorgate-agent/src/ctx.rs:87-113`).

You will often see this pattern: clone what a closure needs, then `move` the clones into it.

```rust
// crates/xorgate-agent/src/worker.rs:239-244
    let (state, clock) = (ctx.state.clone(), ctx.clock.clone());
    let on_status = Arc::new(move |s: &xorgate_core::telemetry::RecordingStatus| {
        // `store.setRecording(status); store.flush(Date.now())`.
        state.set_recording(s.clone());
        state.flush(clock.wall_ms(), false);
    });
```

The callback lives longer than the function that creates it, so it cannot borrow `ctx`. It owns its
own handles instead. Shadowing (`let (state, clock) = …` again on the next lines) is normal Rust
style for "a fresh copy for the next closure".

## `Result`, `?` and the error types

Rust has no exceptions. A function that can fail returns `Result<T, E>`, which is either `Ok(value)`
or `Err(error)`. The caller must handle both; ignoring a `Result` is a compiler warning (and so an
error under `clippy -D warnings`).

The `?` operator is the shorthand for "if this is an error, return it from my function now; else
give me the value". The worker's boot is a chain of them:

```rust
// crates/xorgate-agent/src/worker.rs:508-520
async fn run(args: &[String]) -> Result<i32> {
    let log_sink = logging::init();
    let build = build_info::load();
    // `const config = loadConfig();` (`worker.ts:79`) — throws → exit 1.
    let env = env::load(args)?;
    if env.config.mode == RunMode::Replay {
        bail!("MODE=replay is not supported by the Rust agent (plan Q6) — run the JS agent for replay containers");
    }
    let creds = if env.config.offline {
        None
    } else {
        Some(env::read_creds(&env.config)?)
    };
```

The repo uses three styles of error, each for a reason:

- **`anyhow::Result`** in the agent's top-level code (`worker.rs`, `env.rs`). `anyhow::Error` can
  hold any error and carry context. `.with_context(|| …)` adds a message (see `ensure_home` above),
  `bail!(…)` returns an error built from a message, and `{e:#}` prints the whole chain. It suits
  application code where the error is only logged and turned into an exit code (`worker.rs:99-106`).
- **`thiserror` enums** in the libraries, where callers may want to tell errors apart. Each variant
  gets its message from an attribute:

  ```rust
  // crates/xorgate-cloud/src/tls.rs:23-32 (trimmed)
  #[derive(Debug, thiserror::Error)]
  pub enum TlsError {
      #[error("no certificate found in CA PEM")]
      EmptyCa,
      #[error("no certificate found in client cert PEM")]
      EmptyClientCert,
      #[error("client key PEM: {0}")]
      ClientKey(rustls_pki_types::pem::Error),
      #[error("CA PEM: {0}")]
      CaPem(rustls_pki_types::pem::Error),
      // … ClientCertPem, RootStore, Rustls
  }
  ```

  `.map_err(TlsError::CaPem)?` (`tls.rs:51`) wraps a lower-level error into one of these variants,
  then propagates it.
- **`Result<_, String>`** in many internal traits (for example `TelemetrySource::read`). Here the
  error text itself is part of the contract: it ends up in `state.json` or a log line, worded
  exactly as the reference agent wrote it. `AtomicWriteError::node_message()` in
  `crates/xorgate-hal/src/atomic_write.rs` exists for the same reason.

Two conventions follow from the agent's job (keep running in a vehicle no matter what):

- **Readers fail open.** A missing sensor file or tool is not an error. HAL readers return `None`
  and the caller leaves the field out (`crates/xorgate-hal/src/lib.rs:5-7`).
- **Panics are bugs.** `unwrap()` and `expect()` crash the current task when they meet an error.
  The agent code uses them only where an error is impossible by construction (and says so in the
  `expect` message). Tests use them freely. If a spawned task does panic, `AgentCtx::spawn` catches
  it, logs it and records it in `state.json` instead of letting it vanish (`ctx.rs:129-153`).

## `Option`

`Option<T>` is either `Some(value)` or `None`. Rust has no null; "maybe absent" is always an
`Option`, so the compiler makes you handle the absent case.

`let … else` unpacks an `Option` or leaves early:

```rust
// crates/xorgate-agent/src/worker.rs:465-474
fn connect(env: &AgentEnv, creds: Option<Creds>) -> Result<Link> {
    let cfg = &env.config;
    let Some(creds) = creds else {
        let (t, events) = OfflineTransport::new();
        return Ok(Link {
            mqtt: MqttHandle::new(t),
            events,
            shutdown: None,
        });
    };
```

Option combinators keep "absent stays absent" logic compact. This builds the `lte` telemetry group
from the modem daemon's file: `?` on an `Option` returns `None` from the function (no LTE group at
all when the signal is unknown), and each optional field is mapped only if present:

```rust
// crates/xorgate-core/src/modem.rs:124-132
pub fn lte_group(t: &ModemTelemetry) -> Option<LteGroup> {
    let csq = t.csq.filter(|&c| c != 99.0)?;
    Some(LteGroup {
        quality: MetricLeaf::with_unit(csq, "asu"),
        rssi: t.rssi_dbm.map(|v| MetricLeaf::with_unit(v, "dBm")),
        modem_temp: t
            .modem_temp
            .filter(|v| v.is_finite())
            .map(|v| MetricLeaf::with_unit(v, "°C")),
```

`Option` also decides JSON shape: a field marked `skip_serializing_if = "Option::is_none"` is left
out of the JSON when `None`, while a plain `Option` field prints `null`. See
[serde](#serde-and-exact-json).

## Enums and `match`

A Rust `enum` is a type that is exactly one of several variants. Variants can be bare names:

```rust
// crates/xorgate-core/src/commands.rs:43-52 (trimmed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCommandName {
    Reboot,
    PowerOff,
    Refresh,
    Update,
    // … ReadSms, ClearSms, ObdScan, ObdForget
}
```

or carry data, like the events the MQTT client emits:

```rust
// crates/xorgate-cloud/src/mqtt.rs:102-116 (trimmed)
#[derive(Debug, Clone)]
pub enum MqttEvent {
    /// CONNACK received.
    Connected {
        session_present: bool,
    },
    Disconnected {
        reason: String,
        emits_error: bool,
    },
    // … Reconnecting, ServerDisconnect, Message(Message), SubscribeFailed
}
```

`match` takes a value apart by variant. It must be **exhaustive**: every variant handled, or a `_`
catch-all. That is the main reason enums are everywhere here. Add a variant and the compiler lists
every `match` that has to decide what to do with it.

```rust
// crates/xorgate-agent/src/mqtt_plane.rs:294-302
    pub fn on_event(&mut self, ev: MqttEvent) {
        match ev {
            MqttEvent::Message(m) => {
                self.ctx
                    .tape
                    .mqtt_in(&m.topic, &m.payload, m.retain, m.qos as u8);
                self.on_message(&m.topic, m.payload);
            }
            MqttEvent::Connected { .. } => self.on_connect(),
```

Other patterns you will meet:

- `Some(c @ ("manage" | "monitor")) => …` binds `c` while matching either string
  (`crates/xorgate-agent/src/cli.rs:136`);
- `matches!(x, A | B)` is a `bool` test against a pattern (`commands.rs:101-110`);
- `if let Some(x) = …` and `while let Some(e) = queue.pop_front()` run code for one variant only.

Enums are also how this repo writes state machines and effect lists: `Effect` in
`crates/xorgate-core/src/command_flow.rs:273` is the list of things the command plane may do, and
`ClientEnd` in `crates/xorgate-agent/src/shutdown.rs:87-94` says how to end the MQTT session.

## Structs and `impl`

A `struct` groups named fields. Its methods live in an `impl` block. There are no classes and no
inheritance. `Self` means "this type", and `new` is a naming convention, not a keyword:

```rust
// crates/xorgate-agent/src/state.rs:26-40
/// `StateStore` (`state.ts:69-128`).
pub struct StateStore {
    snap: StateSnapshot,
    last_flush_at: i64,
    path: PathBuf,
}

impl StateStore {
    pub fn new(initial: StateSnapshot, path: PathBuf) -> Self {
        Self {
            snap: initial,
            last_flush_at: 0,
            path,
        }
    }
```

Fields are private unless marked `pub`, so outside code goes through the methods. `path,` alone is
shorthand for `path: path`.

A **tuple struct** with one field is a "newtype": a new type that wraps an existing one to give it
different behaviour. `pub struct JsNumber(pub f64);` (`crates/xorgate-core/src/telemetry.rs:42`) is
an `f64` that serialises the way the reference agent printed numbers. The wrapped value is `.0`.

`#[derive(...)]` generates standard implementations: `Debug` (printable with `{:?}`), `Clone`,
`PartialEq` (`==`), `Default` (an all-default value), `Serialize`/`Deserialize` (serde).
`..Default::default()` or `..b` in a struct literal means "every other field from this value"
(`crates/xorgate-hal/src/child.rs:53-59`, `crates/xorgate-agent/src/worker.rs:1058-1062`).

## Traits, generics and `dyn`

A **trait** is an interface: a set of methods a type promises to provide. Methods may have default
bodies. This is how the worker stays independent of where telemetry comes from:

```rust
// crates/xorgate-agent/src/ctx.rs:217-236 (trimmed)
pub trait TelemetrySource: Send + Sync + 'static {
    /// One reading per tick (`source.read()`): non-empty groups only, imu →
    /// gps → lte → system; `media` is the idle block when no supervisor runs.
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>>;
    fn sample_groups(&self) -> BoxFut<'_, IndexMap<String, GroupSample>>;
    fn sample_power(&self) -> BoxFut<'_, Option<PowerSampleReading>>;
    fn gps_clock(&self) -> Option<GpsClockReading>;
    fn gnss_view(&self) -> Option<GnssView>;
    fn stop(&self);
    fn obd(&self) -> Option<Arc<dyn ObdControl>> {
        None
    }
    // …
}
```

It is implemented by the real collector registry (`collectors/mod.rs:188`), by `SimSource` in
`worker.rs:180`, and by fakes in the tests (`crates/xorgate-agent/tests/common/mod.rs:278`).

**`dyn Trait`** means "some type that implements this trait, decided at run time". It is always
behind a pointer, usually `Arc<dyn Trait>` here. The worker picks the source at run time:

```rust
// crates/xorgate-agent/src/worker.rs:651-654
    let source: Arc<dyn TelemetrySource> = match cfg.mode {
        RunMode::Real => collectors::start_with_media(&ctx, media.clone()).await,
        _ => Arc::new(SimSource),
    };
```

**Generics** are the compile-time alternative: one copy of the function is generated for each
concrete type it is used with. `AgentCtx::spawn` accepts any future:

```rust
// crates/xorgate-agent/src/ctx.rs:129-133
    pub fn spawn<F>(&self, name: &'static str, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let inner = tokio::spawn(fut);
```

The `where` clause lists **trait bounds**: what `F` must support. The bounds you will see most:

- `Send`: safe to move to another thread. tokio may run a task on any worker thread, so anything
  spawned must be `Send`.
- `Sync`: safe to share between threads by reference. Needed for anything inside an `Arc` that
  several tasks use.
- `'static`: holds no borrowed references that could expire. A spawned task may outlive the
  function that spawned it, so it must own everything it uses.

**Why `BoxFut`?** Rust allows `async fn` in traits, but a trait used as `dyn Trait` needs its
methods to return one concrete type. So the traits here return a boxed future:

```rust
// crates/xorgate-agent/src/ctx.rs:45-46
/// A boxed `Send` future (object-safe async trait methods without a crate).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

and implementations write `Box::pin(async move { … })` (see `collectors/lte.rs:69-83`). `Pin`
promises the future will not be moved in memory once polled, which async code requires. You rarely
need to think about it beyond copying the pattern.

Standard traits are implemented the same way: `impl std::fmt::Display for DeviceCommandName`
(`commands.rs:121-125`) makes it printable with `{}`, `impl From<f64> for JsNumber` enables
`.into()`, and `impl Drop for ChildHandle` (`crates/xorgate-hal/src/child.rs:159-167`) runs cleanup
when a handle goes away: dropping it kills a still-running child process.

## Modules and visibility

`pub mod foo;` in a file declares a submodule, whose code lives in `foo.rs` or `foo/mod.rs` next to
it. `crates/xorgate-agent/src/cli.rs` declares `pub mod led;` and so on, and their files are in
`crates/xorgate-agent/src/cli/`. Each crate's `lib.rs` lists its top-level modules. `use` brings
names into scope.

Everything is **private by default**. Visibility levels you will see:

| Marker | Visible to |
|---|---|
| (none) | the module it is in, and its children |
| `pub(crate)` | the whole crate, not other crates (for example `MqttHandle::subscribe`, `mqtt_plane.rs:136`) |
| `pub` | everyone who can reach the module |

This repo uses privacy to make some rules impossible to break. The agent must never run a
privileged command the cloud did not ask for. So the only type the sudo runner accepts has a private
field:

```rust
// crates/xorgate-hal/src/privileged.rs:37-42
/// The exact argv handed to the OS. Private field: built only from a core
/// [`PrivilegedCall`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedArgv {
    argv: Vec<&'static str>,
}
```

and the only way to get a `PrivilegedCall` is `xorgate_core::commands::plan` over a parsed cloud
command (see the module doc at `crates/xorgate-core/src/commands.rs:22-28`). `ClearClaim` in
`command_flow.rs` uses the same trick so that only the command decider can produce an "erase all
SMS" effect.

## `async`/`.await` with tokio

An `async fn` returns a **future**, a value that describes work. Nothing runs until something polls
it, usually by `.await`ing it inside another async function. At an `.await` the task can pause and
let others run. A **runtime** drives the futures. This agent uses tokio's multi-threaded runtime,
built once in `worker::main`:

```rust
// crates/xorgate-agent/src/worker.rs:88-99 (trimmed)
pub fn main(args: &[String]) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => { /* … */ return 1; }
    };
    match rt.block_on(run(args)) {
```

**Tasks.** `tokio::spawn(fut)` runs a future concurrently, as an independent task. The agent spawns
its long-lived subsystems through `ctx.spawn(name, fut)`, which also registers them with a
`TaskTracker` (so shutdown can wait for them) and reports panics.

**`select!`** waits on several futures at once and runs the branch of whichever finishes first. The
worker's main loop is one big `select!`:

```rust
// crates/xorgate-agent/src/worker.rs:837-856 (trimmed)
    let (sig, exit_code) = loop {
        tokio::select! {
            _ = sigterm.recv() => break ("SIGTERM", 0),
            _ = sigint.recv() => break ("SIGINT", 0),
            _ = sighup.recv() => crate::shutdown::on_sighup(&ctx),
            // … the recording manager's fatal channel
            ev = events.recv() => match ev {
                Some(ev) => plane.on_event(ev),
                // … the MQTT loop died: shut down, exit 1
            },
            _ = heartbeat.tick() => { /* watchdog ping, LED tick */ }
```

`loop { … break value }` makes the loop an expression that yields `(signal, exit code)`. Every other
long-lived loop uses the same shape with a cancellation branch, for example the 1 Hz publish loop:

```rust
// crates/xorgate-agent/src/mqtt_plane.rs:456-465
        self.ctx.spawn("publish_loop", async move {
            // setInterval: re-armed from each fire (P-WP6-1).
            let mut iv = SetInterval::new(period);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = iv.tick() => tick.run().await,
                }
            }
        });
```

`select!` normally picks randomly among ready branches. The command and config tasks write
`biased;` as the first line to check branches in order, so cancellation always wins
(`crates/xorgate-agent/src/command_plane.rs:154-166`). A branch's future is dropped when another
branch wins, so futures used in `select!` must be **cancel-safe**: losing the race must not lose
data. `SetInterval::tick` documents that it is (`crates/xorgate-agent/src/timer.rs:76-79`).

**Channels** connect tasks:

| Channel | Shape | Used for |
|---|---|---|
| `tokio::sync::mpsc` (unbounded) | many senders, one receiver, a queue | inbound config and command messages, report requests (`ReportRequester`, `ctx.rs:357-373`). Unbounded on purpose: a retained config or a command must never be dropped because a consumer lagged. |
| `tokio::sync::watch` | one value, latest wins, readers see changes | the hardware block in force (`ctx.hardware`: the config plane calls `send_replace`, everyone else `borrow()`), the MQTT link state, the recording snapshot. |
| `tokio::sync::oneshot` | one value, once | completion of a single publish (`crates/xorgate-cloud/src/mqtt.rs:619`). |

**Timers.** `tokio::time::Instant` is a monotonic clock and is used for every interval; the wall
clock is used only to stamp and name things, because the device's wall clock can jump when GPS, the
modem or NTP corrects it. `SetInterval` (`crates/xorgate-agent/src/timer.rs`) replaces tokio's own
`Interval`. tokio's interval is fixed-rate, while the reference agent re-armed each timer from the
moment it fired, and that small difference changed which consumer refreshed shared cached readings.
`tokio::time::timeout(d, fut)` bounds a wait (`worker.rs:934`).

**Cancellation.** `ctx.cancel` is a `CancellationToken`. Shutdown calls `cancel()` once, and every
loop's `cancel.cancelled()` branch fires. `ctx.shutting_down()` checks it synchronously. Then the
worker closes the `TaskTracker` and waits at most 500 ms for tasks to finish (`worker.rs:928-936`).

**Don't block the runtime.** A runtime thread that sits in a blocking call (a long `std::fs` read of
a slow device, `std::thread::sleep`) stalls every task scheduled on it. Async code uses `tokio::fs`
and `tokio::process`, and genuinely blocking reads, such as the NMEA serial port, run on their own
thread and feed a channel (`crates/xorgate-hal/src/nmea_tty.rs:121-125`).

## Locks: `Mutex`, and why they are held briefly

Shared mutable state lives behind a lock. Two kinds are used, on purpose:

- **`std::sync::Mutex`** for state touched in short, synchronous bursts. `StateHandle` wraps the one
  `StateStore` in `Arc<Mutex<…>>` and every method locks, does one thing and unlocks:

  ```rust
  // crates/xorgate-agent/src/state.rs:138-147
      fn lock(&self) -> MutexGuard<'_, StateStore> {
          self.inner.lock().unwrap_or_else(|p| p.into_inner())
      }

      /// `store.event(level, msg, now); store.flush(now, true)`.
      pub fn note(&self, level: ActivityLevel, msg: impl Into<String>, now: i64) {
          let mut s = self.lock();
          s.event(level, msg.into(), now);
          s.flush(now, true);
      }
  ```

  `lock()` returns a **guard**. The lock is released when the guard is dropped, here at the end of
  `note`. A `Mutex` becomes "poisoned" if a thread panicked while holding it. `unwrap_or_else(|p|
  p.into_inner())` takes the data anyway, because a state file is never worth crashing the agent.
- **`tokio::sync::Mutex`** where the lock must be held across an `.await`, for example the system
  collector holds its sampler while the sampler awaits a `vcgencmd` run
  (`crates/xorgate-agent/src/collectors/system.rs:56-58`). Its `lock()` is itself awaited.

Why locks are held briefly, and never across an `.await` for a `std` mutex:

- **Deadlocks.** The original agent was single-threaded: a block of synchronous code could never be
  interrupted. The Rust agent keeps that atomicity by doing each such block under one lock, but a
  lock held while calling into another component can deadlock if that component calls back. The
  recording manager writes down its lock order and the rules for callbacks
  (`crates/xorgate-agent/src/recording/manager.rs:15-36`). The capture supervisor collects callbacks
  while holding its lock and runs them after releasing it (`recording/capture/mod.rs:26-36`).
- **Stalls.** Holding a `std` guard across `.await` would block other threads waiting for the lock
  while the task sleeps, and the compiler refuses to `spawn` such a future anyway, because the guard
  is not `Send`.

There is no `RwLock` in the codebase. Where many readers need a value that one writer changes, the
code publishes snapshots through a `watch` channel instead (the recording status, the hardware
block). For single counters and flags it uses atomics: `AtomicU64` for the telemetry `seq`,
`AtomicBool` for the LTE collector's warn-once flag (`collectors/lte.rs:74`).

## serde and exact JSON

serde is the serialisation framework. `#[derive(Serialize, Deserialize)]` generates the code that
turns a struct into JSON and back, and attributes adjust the result. For this agent the details
matter, because the JSON must match the reference byte for byte (chapter 6).

```rust
// crates/xorgate-core/src/commands.rs:155-168
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCommand {
    pub request_id: String,
    pub command: DeviceCommandName,
    /// Epoch ms the cloud issued the command.
    #[serde(with = "js_num")]
    pub ts: f64,
    /// Epoch ms after which the device must refuse it. Already TTL-clamped.
    #[serde(with = "js_num")]
    pub expires_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
}
```

What each piece does:

| Attribute | Effect |
|---|---|
| `rename_all = "camelCase"` | Rust fields are `snake_case`; the wire uses `requestId`, `expiresAt`. Enums use `"snake_case"`, `"lowercase"` or `"kebab-case"` the same way (`RunMode` → `"real"`, `telemetry.rs:433-440`). |
| `#[serde(rename = "…")]` | One exact name, for example `RejectReason::RateLimited` → `"rate-limited"` (`commands.rs:174-188`). |
| `skip_serializing_if = "Option::is_none"` | `None` leaves the key **out**. Without it, `None` prints `null`. Both happen on the wire, so each field picks deliberately. In `RecordingStatus`, `session_id` prints `null` while `boot_defer` is omitted (`telemetry.rs:324-339`). |
| `default` | A missing key deserialises to the default (`None`) instead of failing. |
| `with = "js_num"` | Use custom functions for this field (here: print the number like the reference did). |
| `flatten` | Merge a map's entries into the parent object, used to keep unknown keys through a load/save round trip (`commands.rs:298-300`). |

Three workspace-level choices make the output exact (`Cargo.toml`, the `serde_json` line):

- **`preserve_order`**: JSON objects keep insertion order. A struct serialises in field order, so
  **field order in the struct is the key order on the wire**. Dynamic maps use `IndexMap`, an
  insertion-ordered map, never `HashMap`.
- **`float_roundtrip`**: parse every number to the exact same `f64` the reference had.
- **`raw_value`**: lets `JsNumber` emit pre-formatted number text verbatim.

`JsNumber` is a hand-written `Serialize` (`crates/xorgate-core/src/telemetry.rs:50-64`). By default
serde_json prints `3000.0` for an integral float and `5e-6` for some small fractions. The reference
printed `3000` and `0.000005`. So integral values go out as integers and everything else as the exact
text the reference's number formatter produced (`xorgate_core::js::number_to_string`, tested against
5,141 numbers dumped from the reference in `fixtures/js/number-format.json`).

Rounding is the same story: `xorgate_core::round::round` rounds halves toward positive infinity, as
the reference did, while Rust's `f64::round` rounds halves away from zero
(`crates/xorgate-core/src/round.rs:15-28`).

## Lifetimes, as far as this code needs them

A **lifetime** is the compiler's name for "how long a borrow is valid". Most of the time it is
inferred and invisible. You meet explicit lifetimes in three places here.

A struct that holds borrows names their lifetime, which proves it cannot outlive what it borrows.
The shutdown sequence borrows the worker's pieces for the duration of one call:

```rust
// crates/xorgate-agent/src/shutdown.rs:96-107 (trimmed)
/// What `shutdown` touches, borrowed from the worker.
pub struct Shutdown<'a> {
    pub ctx: &'a AgentCtx,
    pub mqtt: &'a MqttHandle,
    pub plane: &'a mut MqttPlane,
    pub config: &'a dyn ConfigPlane,
    pub source: &'a dyn TelemetrySource,
    pub link_closes: bool,
}
```

A function that returns something tied to its inputs says so. Here the returned future may use
`self` and `call`, so it must not outlive either:

```rust
// crates/xorgate-hal/src/privileged.rs:136-141
pub trait PrivilegedRunner: Send + Sync + 'static {
    fn run<'a>(&'a self, call: &'a PrivilegedArgv) -> RunFut<'a>;
    /// One sudo helper run (I5): the clock stepper's `xorgate-setclock` and
    /// the camera-mount reconciler's `xorgate-camrotate`.
    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a>;
}
```

And `'static` is the special lifetime "valid for the whole program": `&'static str` is a string
literal, and `T: 'static` means `T` borrows nothing temporary. `'_` is "the obvious lifetime, I am
not naming it" (`BoxFut<'_, …>` in trait methods: the future borrows `self`).
`impl<'de> Deserialize<'de>` in serde code (`telemetry.rs:66`) is serde's own lifetime for borrowing
from the input; copy the pattern when you hand-write a deserialiser.

If the compiler complains about lifetimes in code you are writing, the fix in this codebase is
almost always to **own instead of borrow**: clone the `Arc` or the `String` and `move` it in.

## Tests and `#[cfg(test)]`

Unit tests live in the same file as the code, in a module that is only compiled for tests:

```rust
// crates/xorgate-core/src/round.rs:37-49 (trimmed)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_ties_go_up() {
        assert_eq!(js_math_round(2.5), 3.0);
        assert_eq!(js_math_round(-2.5), -2.0);
        assert_eq!(js_math_round(-2.6), -3.0);
        // …
    }
```

`#[cfg(test)]` is conditional compilation: the module does not exist in a release build. `use
super::*;` imports everything from the file under test, private items included.

**Integration tests** live in `crates/<crate>/tests/*.rs`. Each file is compiled as a separate crate
that sees only the public API. Shared helpers sit in `tests/common/mod.rs` and each test file pulls
them in with `mod common;`. Most fixture tests (chapter 5) are integration tests.

**Async tests** use `#[tokio::test]`. Adding `start_paused = true` gives the test a virtual clock:
time only advances when every task is idle, and then it jumps straight to the next timer. A test of
a 500 ms bound runs instantly and deterministically:

```rust
// crates/xorgate-agent/src/worker.rs:944-949 (trimmed)
    #[tokio::test(start_paused = true)]
    async fn the_exit_cleanup_runs_after_the_tracker_wait() {
        let at = |tracked_ms: u64| async move {
            let tracker = tokio_util::task::TaskTracker::new();
            tracker.spawn(tokio::time::sleep(Duration::from_millis(tracked_ms)));
            // …
```

That is why the agent's clock is injected: `AgentClock::following_tokio` makes the wall clock follow
the paused tokio clock in tests (`crates/xorgate-agent/src/ctx.rs:178-184`).

Some tests run the real binary: `env!("CARGO_BIN_EXE_xorgate-rs")` is the path cargo built it to
(`crates/xorgate-agent/tests/watchdog.rs:22`).

Test-only code that other crates' tests need is behind a **feature**: `xorgate-hal` exposes
`test_support` only under `#[cfg(any(test, feature = "test-support"))]`
(`crates/xorgate-hal/src/lib.rs:45-47`), and the agent enables that feature in its
`[dev-dependencies]` only, so it never reaches a release build.

## Macros you will see

A call ending in `!` is a macro: code that generates code at compile time. The common ones:

| Macro | Does |
|---|---|
| `format!`, `println!`, `eprintln!` | String formatting. `{x}` inlines a variable, `{x:?}` uses `Debug`, `{e:#}` is anyhow's full chain. |
| `info!`, `warn!`, `error!`, `debug!` | `tracing` log events. Under systemd they go to the journal, else to stderr (`crates/xorgate-agent/src/logging.rs`). The message text often matches the reference's log line exactly, because operators grep for it. |
| `tokio::select!` | Wait for the first of several futures. |
| `matches!` | Pattern test returning `bool`. |
| `assert!`, `assert_eq!` | Test assertions; they panic with both values on failure. |
| `vec![…]`, `serde_json::json!({…})` | Build a `Vec`, build a JSON value from JSON-like syntax. |
| `bail!`, `anyhow!` | Return or build an `anyhow` error. |
| `include_bytes!("…")` | Embed a file in the binary at compile time: the Amazon root CA (`crates/xorgate-cloud/src/tls.rs:21`). |
| `env!("…")`, `concat!` | Read a compile-time environment variable. `crates/xorgate-agent/build.rs` sets `XORGATE_GIT_SHA` with `cargo:rustc-env`, and `crates/xorgate-agent/src/build_info.rs:15` reads it as the fallback build identity. |

`#[derive(…)]` is a macro too (a derive macro), and so is `#[tokio::test]` (an attribute macro).

## `unsafe`: where and why

Safe Rust guarantees memory safety. `unsafe { … }` is a block where you, not the compiler, promise
it, typically around a call into C. This repo keeps `unsafe` rare and visible:

- `xorgate-core` has `#![forbid(unsafe_code)]` (`crates/xorgate-core/src/lib.rs:22`): no `unsafe`
  at all, and `tests/purity.rs` also checks the crate's tests for it.
- `xorgate-hal` and `xorgate-agent` have `#![deny(unsafe_code)]` (`crates/xorgate-hal/src/lib.rs:9`,
  `crates/xorgate-agent/src/lib.rs:8`). Each function that needs it opts in with
  `#[allow(unsafe_code)]` and justifies each block in a `// SAFETY:` comment.

The cases are direct calls into libc (the C library) where no safe wrapper exists, or where the code
must make exactly one system call:

```rust
// crates/xorgate-agent/src/alloc.rs:29-36 (trimmed)
#[allow(unsafe_code)]
pub fn tune() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt only updates glibc's malloc parameters; it takes
        // the arena locks itself and has no memory-safety preconditions.
        unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD_BYTES) == 1 }
    }
```

Others: `libc::kill` to signal a child the supervisor has proven is still unreaped
(`crates/xorgate-hal/src/child.rs:270-271`), `sysinfo`, `clock_gettime`, `geteuid`, and the inotify
calls of the file watcher (`crates/xorgate-hal/src/fs_watch.rs`). The `#[cfg(...)]` in the excerpt
also shows how platform-specific code is compiled only where it applies.

If you need `unsafe`: put it in `xorgate-hal`, keep the block to the one call, write the `SAFETY:`
comment, and expect a reviewer to ask whether a safe wrapper (for example from the `nix` crate) would
do.
