# Keyless mode (agent broker client)

Run the `buzz` CLI without a signing key. In keyless mode the CLI holds no nsec
and opens no relay connection — every operation is sent to a **broker host** that
performs it on the agent's behalf. This is the client side of the agent-broker
contract ([#6790](https://github.com/block/buzz/pull/6790)).

The backend is selected by `--agent-mode` (env `BUZZ_AGENT_MODE`), default
`local`:

- `local` — hold the key, sign locally, talk to the relay (the unchanged default).
- `broker` — keyless; route every operation through a broker host.

Broker mode currently covers: `messages get` (with `--mentions-only`, the wake
path), `messages send` / reply, `reactions add`, `users set-profile`, and
`mem address <slug>` as an addressing-only bridge for encrypted memory. Every
other command still needs the local backend.

The same provisioning variables now select the prototype keyless `buzz-acp`
runtime. It derives its public identity through `storage.address`, polls
configured channels through `channel.read`, and passes broker provisioning to
the CLI used by the spawned agent. See
[`../buzz-acp/README.md`](../buzz-acp/README.md#keyless-broker-mode-prototype)
for the runtime command and its deliberate housekeeping limits.

## Build

```sh
source bin/activate-hermit
cargo build -p buzz-cli --bin buzz --example mock_broker
```

## Try it against the bundled mock host

The repo ships a throwaway mock host so you can exercise the round trip without a
real broker. It signs nothing and returns canned outcomes — it exists only to
prove the client wiring end to end.

Terminal 1 — the mock host:

```sh
cargo run -p buzz-cli --example mock_broker
# listening on http://127.0.0.1:8787
```

Terminal 2 — the keyless client (note: no key in the environment):

```sh
export BUZZ_AGENT_MODE=broker
export BUZZ_BROKER_URL=http://127.0.0.1:8787
export BUZZ_BROKER_CREDENTIAL=dev-token
unset BUZZ_PRIVATE_KEY

CH=<channel-uuid>
buzz messages send --channel "$CH" --content "hello from a keyless client"
buzz messages send --channel "$CH" --reply-to <event-id> --content "on it"
buzz messages get  --channel "$CH" --mentions-only --limit 10
buzz reactions add --channel "$CH" --event <event-id> --emoji "👍"
buzz users set-profile --name "Ada" --about "a keyless agent"
buzz mem address core
```

Terminal 1 logs each action, the bearer credential, and the args it received.

## Point it at your own broker

Swap the two broker vars for your host's endpoint and a credential it issued:

```sh
export BUZZ_BROKER_URL=https://your-broker.example
export BUZZ_BROKER_CREDENTIAL=<token your broker accepts>
```

Your host must accept `POST /v1/action` with `Authorization: Bearer <credential>`
and return a broker-result envelope per the contract. `examples/mock_broker.rs`
is a minimal reference for the wire shape.

## Notes

- Keyless mode **fails closed** if a private key is present (`--private-key` /
  `BUZZ_PRIVATE_KEY`): supplying a key in broker mode is a provisioning error,
  not silently ignored.
- Credential issuance, authorization, and custody are the host's concern; the
  client only needs an endpoint and a token to present.
- Broker HTTP actions have a 30-second end-to-end transport timeout. A runtime
  credential rejected as `unauthenticated` is terminal instead of being polled
  forever.
- Runtime cursors and event deduplication are currently in memory only. The
  first cursorless read after restart follows the host's default-window
  semantics, which must be pinned at the real-host integration checkpoint to
  prevent missed or replayed mentions.
- This CLI slice exposes only the first `messages get --limit` window; broker
  cursor pagination and thread reads are not yet command-line options. The ACP
  runtime therefore prompts from the triggering event without relay-fetched
  thread history.
- `buzz mem address <slug>` prints the broker's `{authorPubkey, kind, dTag}`
  outcome as JSON. This temporary bridge proves secret-dependent address
  derivation without giving the client a key or relay route. It does not yet
  make `mem get/set/patch/rm` keyless; fetching, decrypting, encrypting, and
  publishing memory records belong to the later end-to-end runtime storage
  work.
