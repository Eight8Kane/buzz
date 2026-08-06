import assert from "node:assert/strict";
import test, { mock } from "node:test";

import { relayClient } from "@/shared/api/relayClient";
import {
  KIND_DELETION,
  KIND_MANAGED_AGENT,
  KIND_PERSONA,
  KIND_TEAM,
  KIND_TEAM_CATALOG,
} from "@/shared/constants/kinds";
import {
  coalesceManagedAgentBackfill,
  orderCatalogHeadsLast,
  startPersonaSync,
} from "./usePersonaSync.ts";

const EXPECTED_KINDS = [
  KIND_PERSONA,
  KIND_TEAM,
  KIND_MANAGED_AGENT,
  KIND_TEAM_CATALOG,
  KIND_DELETION,
];

function event({
  id,
  kind = KIND_MANAGED_AGENT,
  createdAt,
  pubkey = "owner-pubkey",
  dTag = "agent-pubkey",
}) {
  return {
    id,
    pubkey,
    created_at: createdAt,
    kind,
    tags: dTag ? [["d", dTag]] : [],
    content: "{}",
    sig: "sig",
  };
}

test("startup backfill keeps only the newest managed-agent head per coordinate", () => {
  const persona = event({
    id: "persona",
    kind: KIND_PERSONA,
    createdAt: 1,
    dTag: "persona-id",
  });
  const otherAgent = event({
    id: "other-agent",
    createdAt: 2,
    dTag: "other-agent",
  });
  const oldest = event({ id: "oldest", createdAt: 1 });
  const sameSecondLoser = event({ id: "f", createdAt: 3 });
  const newest = event({ id: "a", createdAt: 3 });

  assert.deepEqual(
    coalesceManagedAgentBackfill([
      oldest,
      persona,
      newest,
      otherAgent,
      sameSecondLoser,
    ]).map(({ id }) => id),
    ["persona", "a", "other-agent"],
    "NIP-33 uses newest created_at and lowest id on a tie",
  );
});

// Regression guard for the fresh-device backfill ordering defect (Carl r10 P1,
// finding 2): the relay serves history newest-first, so a freshly shared 30178
// head arrives before the 30176 team and 30175 personas it projects. Dispatched
// in that order, the inbound team refresh runs before device B's personas
// hydrate — member resolution fails and the owner's valid shared head is purged
// plus falsely tombstoned. `orderCatalogHeadsLast` MUST defer every catalog head
// past its constituents while preserving relay order within each group.
test("orderCatalogHeadsLast defers catalog heads past all constituents", () => {
  const catalog = event({ id: "cat", kind: KIND_TEAM_CATALOG, createdAt: 30 });
  const team = event({ id: "team", kind: KIND_TEAM, createdAt: 20 });
  const persona = event({ id: "p1", kind: KIND_PERSONA, createdAt: 10 });
  const deletion = event({ id: "del", kind: KIND_DELETION, createdAt: 5 });

  assert.deepEqual(
    // Relay newest-first order: catalog head lands before its constituents.
    orderCatalogHeadsLast([catalog, team, persona, deletion]).map(
      ({ id }) => id,
    ),
    ["team", "p1", "del", "cat"],
    "constituents dispatch first; the catalog head is deferred to the end",
  );
});

test("orderCatalogHeadsLast preserves relay order within each group", () => {
  const catA = event({ id: "cat-a", kind: KIND_TEAM_CATALOG, createdAt: 40 });
  const catB = event({ id: "cat-b", kind: KIND_TEAM_CATALOG, createdAt: 30 });
  const teamA = event({ id: "team-a", kind: KIND_TEAM, createdAt: 20 });
  const teamB = event({ id: "team-b", kind: KIND_TEAM, createdAt: 10 });

  assert.deepEqual(
    orderCatalogHeadsLast([catA, teamA, catB, teamB]).map(({ id }) => id),
    ["team-a", "team-b", "cat-a", "cat-b"],
    "a stable partition keeps newest-wins order inside constituents and heads",
  );
});

// Regression guard for the fresh-start backfill gap (F3): a device that comes
// online AFTER another published gets zero history from a live-only `limit: 0`
// subscription, because reconnect-replay's since-cursor is undefined until the
// first live event. `startPersonaSync` MUST do a one-shot history fetch up
// front, and both the backfill and the live sub MUST carry the deletion kind
// so tombstones catch up too.
test("startPersonaSync backfills history including the deletion kind", () => {
  const fetchCalls = [];
  const liveCalls = [];
  mock.method(relayClient, "fetchEvents", (filter) => {
    fetchCalls.push(filter);
    return Promise.resolve([]);
  });
  mock.method(relayClient, "subscribeLive", (filter) => {
    liveCalls.push(filter);
    return Promise.resolve(() => Promise.resolve());
  });

  startPersonaSync("owner-pubkey", "wss://relay.example", () => false);

  assert.equal(fetchCalls.length, 1, "empty first page exhausts in one fetch");
  assert.deepEqual(
    fetchCalls[0].kinds,
    EXPECTED_KINDS,
    "backfill must cover persona/team/agent + deletion",
  );
  assert.ok(
    fetchCalls[0].limit > 0,
    "backfill must request a positive limit — limit:0 returns no history",
  );
  assert.deepEqual(fetchCalls[0].authors, ["owner-pubkey"]);
  assert.equal(fetchCalls[0].until, undefined, "first page carries no cursor");

  assert.equal(liveCalls.length, 1);
  assert.deepEqual(
    liveCalls[0].kinds,
    EXPECTED_KINDS,
    "live sub must also carry the deletion kind",
  );

  mock.reset();
});

// Regression guard for Thufir r10 P2 finding 2 (hydration boundary). The history
// fetch and the live subscription start concurrently into ONE reconcile chain. A
// live/replayed 30178 catalog head that arrives BEFORE the backfill has
// reconciled its 30175/30176 constituents drives the inbound team refresh against
// an unhydrated persona store — member resolution fails and the owner's valid
// witness is purged plus falsely tombstoned. `startPersonaSync` MUST buffer live
// events until the ordered backfill is dispatched, then drain them. Removing the
// buffer dispatches the live head first and turns this RED.
test("startPersonaSync buffers live catalog heads until the backfill hydrates constituents", async () => {
  const invokedIds = [];
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke: (_cmd, args) => {
        invokedIds.push(JSON.parse(args.eventJson).id);
        return Promise.resolve();
      },
    },
  };

  let resolveBackfill;
  const backfill = new Promise((resolve) => {
    resolveBackfill = resolve;
  });
  mock.method(relayClient, "fetchEvents", () => backfill);
  let onEvent;
  mock.method(relayClient, "subscribeLive", (_filter, listener) => {
    onEvent = listener;
    return Promise.resolve(() => Promise.resolve());
  });

  startPersonaSync("owner-pubkey", "wss://relay.example", () => false);
  await new Promise((resolve) => setImmediate(resolve));

  // A freshly shared catalog head arrives live before the history resolves.
  onEvent(event({ id: "cat-live", kind: KIND_TEAM_CATALOG, createdAt: 100 }));
  // The delayed backfill returns the constituents (relay newest-first).
  resolveBackfill([
    event({ id: "team", kind: KIND_TEAM, createdAt: 90 }),
    event({ id: "persona", kind: KIND_PERSONA, createdAt: 80 }),
  ]);
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(
    invokedIds,
    ["team", "persona", "cat-live"],
    "constituents hydrate first; the buffered live catalog head drains last",
  );

  mock.reset();
  delete globalThis.window;
});

// Regression guard for Thufir r10 P2 finding 3 (capped page). The relay clamps a
// REQ to `max_limit` and serves newest-first, so a large owner's full history
// exceeds one 500-event page: a newer 30178/30176 can land in-page while an older
// required 30175 constituent falls beyond it. `startPersonaSync` MUST page to
// exhaustion via the `until` cursor so every constituent hydrates before the
// catalog head. Removing pagination leaves the required persona unfetched.
test("startPersonaSync pages history to exhaustion so an out-of-page constituent hydrates before the catalog head", async () => {
  const invokedIds = [];
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke: (_cmd, args) => {
        invokedIds.push(JSON.parse(args.eventJson).id);
        return Promise.resolve();
      },
    },
  };

  // Page 1 (newest-first): the catalog head, the team, and 498 filler personas —
  // a full page whose oldest event is created_at 501.
  const page1 = [
    event({ id: "cat", kind: KIND_TEAM_CATALOG, createdAt: 1000 }),
    event({ id: "team", kind: KIND_TEAM, createdAt: 999 }),
  ];
  for (let i = 0; i < 498; i += 1) {
    page1.push(
      event({
        id: `filler-${i}`,
        kind: KIND_PERSONA,
        createdAt: 998 - i,
        dTag: `filler-${i}`,
      }),
    );
  }
  // Page 2: the boundary event re-returned by the inclusive `until`, plus the
  // required older persona that fell outside page 1.
  const boundary = page1[page1.length - 1];
  const page2 = [
    boundary,
    event({
      id: "req-persona",
      kind: KIND_PERSONA,
      createdAt: 100,
      dTag: "req-persona",
    }),
  ];

  const fetchCalls = [];
  mock.method(relayClient, "fetchEvents", (filter) => {
    fetchCalls.push(filter);
    return Promise.resolve(filter.until === undefined ? page1 : page2);
  });
  mock.method(relayClient, "subscribeLive", () =>
    Promise.resolve(() => Promise.resolve()),
  );

  startPersonaSync("owner-pubkey", "wss://relay.example", () => false);
  for (let i = 0; i < 5; i += 1)
    await new Promise((resolve) => setImmediate(resolve));

  assert.equal(
    fetchCalls.length,
    2,
    "a full first page triggers a second page",
  );
  assert.equal(
    fetchCalls[1].until,
    501,
    "the second page is cursored on the oldest event",
  );
  assert.ok(
    invokedIds.includes("req-persona"),
    "the out-of-page constituent must be fetched and reconciled",
  );
  assert.ok(
    invokedIds.indexOf("req-persona") < invokedIds.indexOf("cat"),
    "the required persona hydrates before the catalog head",
  );
  assert.equal(
    invokedIds[invokedIds.length - 1],
    "cat",
    "the catalog head reconciles last, after every constituent",
  );
  assert.equal(
    invokedIds.filter((id) => id === boundary.id).length,
    1,
    "the inclusive-cursor boundary event is deduped, not reconciled twice",
  );

  mock.reset();
  delete globalThis.window;
});

// Regression guard for the arrival-scope fix (F6): the reconcile must carry the
// relay this subscription was opened on, NOT whichever community happens to be
// active when the reconcile runs. Without the forwarded URL the backend falls
// back to the active workspace and an in-flight event lands in the wrong
// community's scoped retention store on a mid-flight switch.
test("startPersonaSync forwards its own relay as the event arrival relay", async () => {
  const invokes = [];
  // @tauri-apps/api/core reads `window.__TAURI_INTERNALS__.invoke`.
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke: (cmd, args) => {
        invokes.push({ cmd, args });
        return Promise.resolve();
      },
    },
  };

  const ownEvent = { id: "e1", pubkey: "owner-pubkey", kind: KIND_PERSONA };
  const foreignEvent = { id: "e2", pubkey: "someone-else", kind: KIND_PERSONA };

  mock.method(relayClient, "fetchEvents", () =>
    Promise.resolve([ownEvent, foreignEvent]),
  );
  mock.method(relayClient, "subscribeLive", () =>
    Promise.resolve(() => Promise.resolve()),
  );

  startPersonaSync("owner-pubkey", "wss://community-a.example", () => false);
  // Let the backfill promise chain and the reconcile invoke settle.
  await new Promise((resolve) => setImmediate(resolve));

  const reconciles = invokes.filter(
    (call) => call.cmd === "reconcile_inbound_persona_event",
  );
  assert.equal(
    reconciles.length,
    1,
    "only the subscribed author's event reconciles",
  );
  assert.equal(
    reconciles[0].args.arrivalRelayUrl,
    "wss://community-a.example",
    "reconcile must carry the subscription's relay as the arrival relay",
  );
  assert.equal(JSON.parse(reconciles[0].args.eventJson).id, "e1");

  mock.reset();
  delete globalThis.window;
});

test("startPersonaSync serializes inbound reconciliation in relay order", async () => {
  const resolvers = [];
  const invokedIds = [];
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke: (_cmd, args) => {
        invokedIds.push(JSON.parse(args.eventJson).id);
        return new Promise((resolve) => resolvers.push(resolve));
      },
    },
  };

  let onEvent;
  mock.method(relayClient, "fetchEvents", () => Promise.resolve([]));
  mock.method(relayClient, "subscribeLive", (_filter, listener) => {
    onEvent = listener;
    return Promise.resolve(() => Promise.resolve());
  });

  startPersonaSync("owner-pubkey", "wss://community.example", () => false);
  await new Promise((resolve) => setImmediate(resolve));
  onEvent({ id: "broad", pubkey: "owner-pubkey", kind: KIND_MANAGED_AGENT });
  onEvent({
    id: "restricted",
    pubkey: "owner-pubkey",
    kind: KIND_MANAGED_AGENT,
  });
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(
    invokedIds,
    ["broad"],
    "newer event waits for prior deployment",
  );
  resolvers.shift()();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(invokedIds, ["broad", "restricted"]);
  resolvers.shift()();

  mock.reset();
  delete globalThis.window;
});
