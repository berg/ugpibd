# scpi → HiSLIP client

## Goal

Make the `scpi` CLI (`src/bin/scpi.rs`) speak **HiSLIP** instead of the
Prologix line protocol. HiSLIP replaces Prologix in this client entirely —
the `++mode`/`++addr`/`++auto` startup and raw line-forwarding are removed.
The `gpibd` daemon keeps serving both front-ends; only the `scpi` client
changes.

## Decisions

- **Transport:** replace Prologix. `scpi` always connects to the HiSLIP
  server (default port **4880**, `--host` retained).
- **Scope:** full two-channel client — synchronous channel for data/trigger
  plus asynchronous channel for device-clear / REN / status.
- **Approach:** reuse the existing async message codec
  (`gpibd::hislip::messages`) via a thin lib client; the binary keeps a
  blocking rustyline REPL and drives the client with a current-thread tokio
  runtime through `block_on`. No wire-format duplication, no background
  reader thread (the server does not forward SRQ, so every reply is
  solicited).

## Components

### `src/hislip/client.rs` (new)

`HislipClient` owns both TCP channels and a `message_id` counter.

```
HislipClient::connect(host, port, subaddress, vendor_id) -> Result<Self>
    sync:  Initialize{proto=2.0, vendor, payload=subaddress}
           -> InitializeResponse (capture session_id)
    async: AsyncInitialize{session_id}
           -> AsyncInitializeResponse
           AsyncMaximumMessageSize{our_max} -> AsyncMaximumMessageSizeResponse

query(cmd: &[u8]) -> Result<Vec<u8>>     // send DataEnd, read Data*..DataEnd
write(cmd: &[u8]) -> Result<()>          // send DataEnd, no read
trigger() -> Result<()>                  // sync Trigger, no read
clear()   -> Result<()>                  // async AsyncDeviceClear -> Ack
remote(on: bool) -> Result<()>           // async AsyncRemoteLocalControl -> Resp
status()  -> Result<u8>                  // async AsyncStatusQuery -> Resp.control_code
```

- `message_id` starts at `0xffff_ff00`, `+2` (wrapping) per data
  transaction; the server echoes it in `message_parameter` and the client
  verifies the echo.
- A non-fatal `Error` reply on the sync channel is surfaced as `Err`.
- REN control codes: `on` → `3` (set_remote true), `off` → `0`
  (set_remote false), matching the server's mapping.

### `src/bin/scpi.rs` (rewrite)

- CLI: `--host` (default `localhost`), `--port` (default `4880`),
  `--addr N` (optional). `--addr N` → subaddress `hislip<N>`; absent →
  `hislip0` (daemon-default PAD).
- Create a current-thread tokio runtime; `block_on(HislipClient::connect(..))`.
- REPL (interactive rustyline + history, and non-TTY batch) — per line:
  - `++clr` → `clear()`, `++trg` → `trigger()`, `++ren 0|1` → `remote()`,
    `++status` → `status()` (prints the byte).
  - else if line contains `?` → `query()` and print the response.
  - else → `write()`.
  - errors print `[error: …]`; if the socket is dead, exit cleanly so the
    terminal is restored (panic hook + `stty sane` fallback retained).
- Mid-session address switching (`++addr`) is out of scope: the address is
  fixed at `Initialize`.

### `src/hislip/mod.rs` — add `pub mod client;`

## Testing

`tests/hislip_client.rs` — spin up the real server with `EchoDevice` (as
`tests/hislip_integration.rs` does) and assert each `HislipClient` method
round-trips: connect handshake, `query("*IDN?")`, `write`, `trigger`,
`clear`, `remote`, `status`, and message-id echo/increment.

## Docs

Update `README.md` `scpi` usage to reflect HiSLIP transport and the `++`
meta-command set.
