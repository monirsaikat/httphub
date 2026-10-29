# httphub

A local-first HTTP client. Your requests are files you own, in a format you can
read, under a tool that works with no account and no network.

Same engine behind a CLI and a GUI. Not two products.

## Why

Postman and Insomnia both converted a developer utility into an account-gated
cloud platform, and both build on the browser's network stack. That combination
caps how powerful either can be:

| Their problem | What httphub does instead |
| --- | --- |
| Login required, collections sync to a vendor by default | No account, ever, for core function. Files on disk are the truth. |
| Collections are opaque JSON that cannot be diffed or merged | Plain-text `.http` files; `git diff` reads usefully |
| Chromium's net stack: no TLS control, no raw bytes, no real timings | Native `hyper` engine: ALPN control, per-phase timings, exact wire bytes |
| Secrets stored and synced in environment files | Secrets referenced, never stored. Every file is safe to commit. |
| GUI-first; CI goes through a separate, lagging tool | One core crate; CLI and GUI are equal clients of it |
| Large responses buffered into the renderer | Bodies stream to disk and are handed back as a path |

## Architecture

```
crates/httphub-core   engine, .http format, variable resolution
                      no terminal, no Tauri, no printing -- structured types only
crates/httphub-cli    clap, terminal rendering, exit codes   (binary: httphub)
crates/httphub-gui    Tauri; commands are thin wrappers over core   (not yet)
ui/                   frontend; talks only to those commands        (not yet)
```

Two rules hold the design together:

**Sends are event streams, not return values.** `engine::send` reports
`RequestEvent`s as they happen -- `Resolved`, `Connected`, `TlsEstablished`,
`RequestSent`, `FirstByte`, `BodyChunk`, `Complete`. The CLI renders that
stream as a text waterfall; the GUI will forward the identical stream to the
webview and draw one. Adding a face never means adding a second way to observe
a request.

**Timings and byte counts are measured, not estimated.** An instrumented
socket wrapper (`transport.rs`) sits between hyper and the network, so the
`send` phase is the real flush of the request rather than a stamp taken before
an await, and output separates plaintext bytes from wire bytes:

```
-> 404 Not Found  HTTP/2.0  in 66ms
  TLSv1.3 h2
  dns 40ms  tcp 2.8ms  tls 12ms  send 2.0ms  wait 9.2ms  download 777us
  sent 118 B  received 775 B  (wire 542 B up / 5.6 KB down)
```

**Response bodies never cross the API boundary.** They stream to
`.httphub/responses/` and come back as a path plus metadata, so a 2 GB response
costs a file handle instead of a heap allocation. The GUI reads byte ranges off
disk; the CLI reads and prints.

Everything under `.httphub/` is derived and disposable. Deleting it loses
nothing.

## Try it

```sh
cargo build
./target/debug/httphub parse examples/demo/users/list.http -e local
./target/debug/httphub send  examples/demo/users/list.http -e local --trace
./target/debug/httphub send  examples/demo/users/list.http -r create_user -e local
./target/debug/httphub send  examples/demo/public/get.http -e public --http 1.1
./target/debug/httphub send  examples/demo/users/list.http -e local --json | jq .timings_ms

# TLS: staging boxes, internal CAs, mutual auth, reaching a gateway by address
./target/debug/httphub send req.http --insecure
./target/debug/httphub send req.http --ca internal-root.pem
./target/debug/httphub send req.http --cert client.pem --key client.key
./target/debug/httphub send req.http --sni internal.example

# Redirects, with the trail shown
./target/debug/httphub send req.http -L --max-redirects 5

# Collection runs -- the CI story, same binary
./target/debug/httphub run examples/demo/users -e local
./target/debug/httphub run . -e ci --reporter junit > results.xml
./target/debug/httphub run . --bail
```

`examples/demo/users/list.http`:

```http
### List users
# @name list_users
GET {{base_url}}/get?page=1
Accept: application/json
```

Environments live in `httphub.toml`, found by walking up from the request file:

```toml
[env.local]
base_url = "http://127.0.0.1:8787"
```

### Assertions live in the request file

Postman's tests sit in a GUI textarea as sandboxed JavaScript, so they cannot
be reviewed, diffed, or run without the GUI's runtime. Here they are lines in
the same file, and `httphub run` checks them:

```http
### Create a user
# @name create_user
# @expect status 201
# @expect header content-type contains application/json
# @expect json /name == ada
# @expect duration < 500
# @capture id = json /id
POST {{base_url}}/users
Content-Type: application/json

{"name": "ada"}
```

`@capture` feeds later requests in the same run, so a login-then-use chain
needs no scripting:

```http
### Log in
# @capture token = json /access_token
POST {{base_url}}/login

### Use it
GET {{base_url}}/me
Authorization: Bearer {{token}}
```

Assertions are deliberately declarative -- `status`, `header`, `body`, `json`
(JSON Pointer, also accepting `a.b` and `$.a.b`) and `duration`. Anything that
needs real logic belongs in the scripting layer, not in a half-language grown
out of comments.

```
examples/demo/users/list.http
  ok    list_users                 200     5.0ms  4 checks
          captured first_url=/get?page=1
  FAIL  create_user                500      17ms  2/3 checks failed
          status 201 -- got 500
          json /name == ada -- no value at /name

2 requests, 1 passed, 1 failed  |  7 checks, 2 failed  in 28ms
```

### Exit codes

CI depends on these, so they are part of the contract:

| Code | Meaning |
| --- | --- |
| 0 | sent, and with `--fail` the status was not an error |
| 1 | assertion or `--fail` check did not pass |
| 2 | transport failure: dns, connect, tls, timeout |
| 3 | configuration or parse failure; nothing was sent |

`--json` writes machine output to stdout only, so pipes work.

## Status

Working: `.http` parsing (superset of the VS Code REST Client / JetBrains
format, so the same files open in editors people already use), variable
resolution with `--var` and `httphub.toml` environments, the send engine over
HTTP/1.1 and HTTP/2 with TLS 1.2/1.3, ALPN negotiation and per-request protocol
pinning, measured per-phase timings and byte counts via an instrumented
socket, full TLS control (`--insecure`, extra CAs, client certificates for
mutual TLS, SNI override), redirect following with a per-hop trail, bodies
streamed to disk, collection runs with in-file assertions and value capture
for chaining, `send`, `run` and `parse` subcommands, human / JSON / JUnit
output. 46 unit tests.

### Decisions already made, deliberately

- **Headers are an ordered list, not a map.** Duplicates are meaningful; a
  client that collapses them cannot test a server that cares.
- **`Host` is supplied only when the request omits it.** Overriding it by hand
  is how you test virtual hosts and routing.
- **Unknown `{{variables}}` are an error, not an empty string.** Silently
  sending `/users/` when you meant `/users/42` costs an afternoon.
- **Origin-form request targets on HTTP/1.1**, full uri on HTTP/2.
- **`--insecure` waives certificate trust, not signature checking.** A broken
  handshake should still look broken; it is trust people want to skip against
  a staging box.
- **A missing CA or key file exits 3, not 2.** Nothing was sent, so it is a
  configuration error, not a transport one.
- **Redirects are not followed unless asked (`-L`).** Seeing the 301 is often
  the reason you sent the request.
- **303 always demotes to GET, and 301/302 do for non-GET/HEAD.** Browsers and
  curl all do this; matching them surprises people less than being right in
  isolation.
- **`Authorization` and `Cookie` are dropped when the origin changes.** A
  client that forwards credentials across hosts is one bad redirect away from
  handing a token to someone else's server.
- **A request that cannot be sent is recorded, and the run continues.** A
  report covering every request beats stopping at the first network blip;
  `--bail` opts into the other behaviour.
- **A failed `@capture` is a failed check, not an empty variable.** Otherwise
  the next request fails for a reason that looks unrelated.
- **Human output goes to stderr, machine output to stdout.** `--reporter json`
  pipes cleanly; so does a body.
- **Each hop times itself.** A waterfall should describe one exchange, not
  smear a chain together; one timeout still covers the whole chain.

## Next

Roughly in the order that keeps the tool honest:

1. **Importers**: curl, Postman, Insomnia, OpenAPI, HAR. The migration path is
   the onboarding path, so import must be lossless enough to actually leave.
2. **Secrets resolver**: `{{secret:name}}` against the OS keychain, `.env`, and
   shell-outs to `op` / `vault` / `aws`. Never written into a workspace file.
3. **Scripting**: embedded JS with a typed SDK, plus a JSON-over-stdio
   subprocess hook so a pre-request step can be any executable in any language.
4. **Tauri GUI** over the same core, consuming the same event stream.
5. **Capture proxy** -- `httphub proxy --capture` turning real traffic into
   requests. The highest-value feature on this list and the one nobody does
   well.

Non-core protocols (gRPC, WebSocket, SSE, MQTT) land as plugins. If a protocol
cannot be a plugin, the plugin API is wrong -- that is the discipline that keeps
"feature rich" from becoming what Postman became.

## Local toolchain note

This machine has a Chocolatey-installed Rust 1.81 in `C:\ProgramData\chocolatey\bin`
that shadows the rustup shims, and current dependencies need edition 2024. Put
rustup first on `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # or remove the choco rust: choco uninstall rust
```

`rust-toolchain.toml` pins the project to rustup's stable, which only takes
effect once the shims win.
