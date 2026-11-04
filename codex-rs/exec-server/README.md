# codex-exec-server

`codex-exec-server` is the library backing `codex exec-server` and the standalone
`codex-exec-server` executable, a small
JSON-RPC server for spawning and controlling subprocesses through
`codex-utils-pty`.

It provides:

- CLI entrypoints: `codex exec-server` and `codex-exec-server`
- a Rust client: `ExecServerClient`
- a small protocol module with shared request/response types

This crate owns the transport, protocol, and filesystem/process handlers. The
`codex-exec-server-cli` crate shares startup between the two executables. Both
use `codex-arg0` for hidden filesystem, process, and platform sandbox dispatch.
The standalone executable accepts the same executor arguments and `-c` config
overrides, including `--listen stdio`. Build it with
`cargo build -p codex-exec-server-cli --bin codex-exec-server`.
Canonical packages place it in `bin/` beside the entrypoint, with platform
sandbox resources in `codex-resources/`.

## Command metadata

Launches expose `metadata.threadId` as `CODEX_THREAD_ID` and `metadata.toolCallId`
as `CODEX_TOOL_CALL_ID` in the initial child environment. `exec_command` supplies
the originating execution call ID, unchanged by later input. Code-mode uses the
nested execution call ID, not the outer cell ID.

These reserved labels override inherited values, policy settings, overlays, and
cached snapshots. Present metadata clears omitted fields; absent metadata preserves
thread-only context and unsets the call ID. Snapshot capture and validation helpers
keep available thread context but omit the call ID; newly captured snapshots exclude
both ID exports.
Call IDs are copied unchanged, including empty values; NUL-containing IDs are
omitted. Platform process-environment size limits apply.

Labels support correlation, not authorization. Shell startup, commands, and
descendants can change or remove them; consumers must handle missing or unusable
labels. These guarantees require an updated executor: older executors may ignore
metadata or restore stale snapshot values.

## Transport

The server speaks the exec-specific `codex-exec-server-protocol` message
envelope on the wire.

The CLI entrypoint supports:

- `ws://IP:PORT` (default)
- `stdio` and `stdio://`

## Authentication

Direct WebSocket listeners support app-server's opt-in auth flags, including
`--ws-auth capability-token --ws-token-sha256 HEX`, token files, and signed
bearer tokens; clients send `Authorization: Bearer TOKEN` on each connection.
Authentication is checked at connection time and these flags do not apply to
stdio.

App-server clients can supply the raw token through `environment/add.authBearerToken` or `auth_bearer_token` in an `environments.toml` URL entry; omission preserves unauthenticated behavior.
Tokens require `wss://` or a loopback destination, are redacted in diagnostics, and are reused on reconnect without automatic refresh.

## Lifecycle

Each connection follows this sequence:

1. Send `initialize`.
2. Wait for the `initialize` response.
3. Send `initialized`.
4. Call process or filesystem RPCs.

Requests run sequentially by default. Pass `--concurrent-requests <COUNT>` to
enable concurrent processing.

If the server receives any notification other than `initialized`, it replies
with an error using request id `-1`.

If the websocket connection closes, the server terminates any remaining managed
processes for that client connection.

## API

### `initialize`

Initial handshake request.

Request params:

```json
{
  "clientName": "my-client"
}
```

Response:

```json
{
  "sessionId": "00000000-0000-4000-8000-000000000001",
  "environmentInfo": {
    "shell": { "name": "bash", "path": "/bin/bash" },
    "executorVersion": "1.2.3-alpha.4",
    "providerId": "sha256:fb4f62da3e84f6864dcec8ede7bc66f1c96ecaeaf55f8a786b85df994057c8ac",
    "cwd": "file:///workspace"
  }
}
```

`environmentInfo` contains the same executor metadata returned by
`environment/info`, so clients can use it without a second request.

`executorVersion` is the executor's package release version, or `0.0.0` when unknown.

The executor caches optional `providerId` at startup using
`codex_build_info::build_id(commit, target)`, which CI can also call for an
explicit build target. This opaque compatibility key excludes package version
and requires no manifest. It identifies a standard build configuration, not exact
executable bytes. Unstamped and legacy executors may omit it.

Rust clients cache this metadata for the client's lifetime, including session
resumption. If initialization omits it, the first metadata request fetches and
caches `environment/info`.

### `initialized`

Handshake acknowledgement notification sent by the client after a successful
`initialize` response.

Params are currently ignored. Sending any other notification method is treated
as an invalid request.

### `process/start`

Starts a new managed process.

Request params:

```json
{
  "processId": "proc-1",
  "argv": ["bash", "-lc", "printf 'hello\\n'"],
  "cwd": "file:///absolute/working/directory",
  "env": {
    "PATH": "/usr/bin:/bin"
  },
  "tty": true,
  "pipeStdin": false,
  "arg0": null
}
```

Field definitions:

- `processId`: caller-chosen stable id for this process within the connection.
- `argv`: command vector. It must be non-empty.
- `cwd`: `file:` URI for the child process working directory.
- `env`: environment variables passed to the child process.
- `tty`: when `true`, spawn a PTY-backed interactive process.
- `pipeStdin`: when `true`, keep non-PTY stdin writable via `process/write`.
- `arg0`: optional argv0 override forwarded to `codex-utils-pty`.

Response:

```json
{
  "processId": "proc-1"
}
```

Behavior notes:

- Reusing an existing `processId` is rejected.
- PTY-backed processes accept later writes through `process/write`.
- Non-PTY processes reject writes unless `pipeStdin` is `true`.
- Output is streamed asynchronously via `process/output`.
- Exit is reported asynchronously via `process/exited`.

### `process/read`

Reads buffered output and terminal state for a managed process.

Request params:

```json
{
  "processId": "proc-1",
  "afterSeq": null,
  "maxBytes": 65536,
  "waitMs": 1000
}
```

Field definitions:

- `processId`: managed process id returned by `process/start`.
- `afterSeq`: optional sequence number cursor; when present, only newer chunks
  are returned.
- `maxBytes`: optional response byte budget.
- `waitMs`: optional long-poll timeout in milliseconds.

Response:

```json
{
  "chunks": [],
  "nextSeq": 1,
  "exited": false,
  "exitCode": null,
  "closed": false,
  "failure": null
}
```

### `process/write`

Writes raw bytes to a running process stdin.

Request params:

```json
{
  "processId": "proc-1",
  "chunk": "aGVsbG8K"
}
```

`chunk` is base64-encoded raw bytes. In the example above it is `hello\n`.

Response:

```json
{
  "status": "accepted"
}
```

Behavior notes:

- Writes to an unknown `processId` are rejected.
- Writes to a non-PTY process are rejected unless it started with `pipeStdin`.

### `process/terminate`

Terminates a running managed process.

Request params:

```json
{
  "processId": "proc-1"
}
```

Response:

```json
{
  "running": true
}
```

If the process is already unknown or already removed, the server responds with:

```json
{
  "running": false
}
```

## Notifications

### `process/output`

Streaming output chunk from a running process.

Params:

```json
{
  "processId": "proc-1",
  "seq": 1,
  "stream": "stdout",
  "chunk": "aGVsbG8K"
}
```

Fields:

- `processId`: process identifier
- `seq`: per-process output sequence number
- `stream`: `"stdout"`, `"stderr"`, or `"pty"`
- `chunk`: base64-encoded output bytes

### `process/exited`

Final process exit notification.

Params:

```json
{
  "processId": "proc-1",
  "seq": 2,
  "exitCode": 0,
  "sandboxDenied": false
}
```

`sandboxDenied` lets streaming clients preserve executor-side sandbox denial
detection without issuing a final `process/read` request. Clients recover it
with `process/read` when an older server omits the field.

### `process/closed`

Notification emitted after process output is closed and the process handle is
removed.

Params:

```json
{
  "processId": "proc-1",
  "seq": 3
}
```

## Filesystem RPCs

Filesystem methods require valid `file:` URI strings and return JSON-RPC errors
for invalid or unavailable paths. Native absolute path strings are rejected;
callers must convert them to `file:` URIs before sending requests:

- `fs/readFile`
- `fs/open`, `fs/readBlock`, `fs/writeBlock`, and `fs/close` (file handles;
  reads also back `ExecutorFileSystem::read_file_stream`)
- `fs/writeFile`
- `fs/createDirectory`
- `fs/getMetadata`
- `fs/canonicalize`
- `fs/readDirectory`
- `fs/remove`
- `fs/copy`

`fs/open.mode` is `read` (default) or `replace` (create or truncate for writing).
Writable handles require `fileWriteStreaming`; `fs/writeBlock` writes a nonempty
base64 `chunk` of up to 1 MiB at an explicit `offset`.

Path-based filesystem requests accept an optional `sandbox` object. When `sandbox`
contains a `ReadOnly` or `WorkspaceWrite` policy, the operation runs in a
hidden helper process launched from the top-level `codex` executable and
prepared through the shared sandbox transform path. Helper requests and
responses are passed over stdin/stdout.

## Errors

The server returns JSON-RPC errors with these codes:

- `-32600`: invalid request
- `-32602`: invalid params
- `-32603`: internal error

Typical error cases:

- unknown method
- malformed params
- empty `argv`
- duplicate `processId`
- writes to unknown processes
- writes to non-PTY processes
- sandbox-denied filesystem operations

## Rust surface

The crate exports:

- `ExecServerClient`
- `ExecServerError`
- `ExecServerClientConnectOptions`
- protocol request/response structs for process and filesystem RPCs
- `DEFAULT_LISTEN_URL` and `ExecServerListenUrlParseError`
- `ExecServerRuntimeOptions`
- `run_main()` for embedding the websocket server

Callers must pass `ExecServerRuntimeOptions` and an explicitly configured
`HttpClientFactory` to `run_main()`. The top-level `codex exec-server` command
builds the runtime options from the `codex` arg0 dispatch state and startup flags,
and resolves its HTTP client factory from the effective Codex configuration.
`--proxy-private-ips-via-upstream` (or
`CODEX_EXEC_SERVER_PROXY_PRIVATE_IPS_VIA_UPSTREAM=true`) allows permitted private IP
destinations to use an inherited upstream proxy. If no valid proxy configuration
applies to the request protocol, routing falls back to a direct connection. This
includes unset, malformed, or unsupported proxy settings. Setting
`allow_upstream_proxy=false` also keeps routing direct; loopback always stays local.
Destination policy still applies to every route. Enforcing mandatory upstream routing
would require a separate fail-closed mode. A connection failure after selecting an
upstream proxy is returned as an error, without retrying directly.

## Example session

Initialize:

```json
{"id":1,"method":"initialize","params":{"clientName":"example-client"}}
{"id":1,"result":{"sessionId":"00000000-0000-4000-8000-000000000001","environmentInfo":{"shell":{"name":"bash","path":"/bin/bash"},"cwd":"file:///tmp"}}}
{"method":"initialized","params":{}}
```

Start a process:

```json
{"id":2,"method":"process/start","params":{"processId":"proc-1","argv":["bash","-lc","printf 'ready\\n'; while IFS= read -r line; do printf 'echo:%s\\n' \"$line\"; done"],"cwd":"file:///tmp","env":{"PATH":"/usr/bin:/bin"},"tty":true,"pipeStdin":false,"arg0":null}}
{"id":2,"result":{"processId":"proc-1"}}
{"method":"process/output","params":{"processId":"proc-1","seq":1,"stream":"stdout","chunk":"cmVhZHkK"}}
```

Write to the process:

```json
{"id":3,"method":"process/write","params":{"processId":"proc-1","chunk":"aGVsbG8K"}}
{"id":3,"result":{"status":"accepted"}}
{"method":"process/output","params":{"processId":"proc-1","seq":2,"stream":"stdout","chunk":"ZWNobzpoZWxsbwo="}}
```

Terminate it:

```json
{"id":4,"method":"process/terminate","params":{"processId":"proc-1"}}
{"id":4,"result":{"running":true}}
{"method":"process/exited","params":{"processId":"proc-1","seq":3,"exitCode":0,"sandboxDenied":false}}
{"method":"process/closed","params":{"processId":"proc-1","seq":4}}
```
