import * as React from "react";

import { relayClient } from "@/shared/api/relayClient";
import { reconcileInboundPersonaEvent } from "@/shared/api/tauriPersonas";
import type { RelayEvent } from "@/shared/api/types";
import {
  KIND_DELETION,
  KIND_MANAGED_AGENT,
  KIND_PERSONA,
  KIND_TEAM,
  KIND_TEAM_CATALOG,
} from "@/shared/constants/kinds";

// Persona/team/managed-agent projections (upserts), the owner's own team
// catalog heads (30178), plus kind:5 NIP-09 deletions, so a tombstone published
// by another device also removes the local record here.
//
// The 30178 head has no local record — it is retained only as this device's
// publication witness, so a second device's boot reconcile and interactive
// refresh have a row to supersede or retract. Without it, device B never learns
// device A published, and B's later edit or delete cannot update A's
// discoverable catalog entry.
const PERSONA_SYNC_KINDS = [
  KIND_PERSONA,
  KIND_TEAM,
  KIND_MANAGED_AGENT,
  KIND_TEAM_CATALOG,
  KIND_DELETION,
];

// One history page. The relay clamps a REQ `limit` to its advertised
// `max_limit` (1000; `crates/buzz-db` `DEFAULT_MAX_PAGE_LIMIT`), so a single
// query can never return an owner's complete history once it exceeds the page —
// `startPersonaSync` pages to exhaustion (see `fetchOwnerHistoryToExhaustion`).
const PERSONA_HISTORY_PAGE_LIMIT = 500;

function eventDTag(event: RelayEvent): string | null {
  return event.tags.find((tag) => tag[0] === "d")?.[1] ?? null;
}

function eventIsNewer(candidate: RelayEvent, current: RelayEvent): boolean {
  return (
    candidate.created_at > current.created_at ||
    (candidate.created_at === current.created_at && candidate.id < current.id)
  );
}

/**
 * Keep only the NIP-33 head for each managed-agent coordinate in a startup
 * backfill. Applying historical policy revisions one by one can stop and start
 * the same runtime for every revision; the retained store only needs the final
 * head. Other event kinds stay in relay order because persona/team projections
 * do not trigger runtime policy transitions and deletion ordering is separate.
 */
export function coalesceManagedAgentBackfill(
  events: readonly RelayEvent[],
): RelayEvent[] {
  const heads = new Map<string, RelayEvent>();

  for (const event of events) {
    if (event.kind !== KIND_MANAGED_AGENT) continue;
    const dTag = eventDTag(event);
    if (!dTag) continue;
    const coordinate = `${event.pubkey.toLowerCase()}:${dTag.toLowerCase()}`;
    const current = heads.get(coordinate);
    if (!current || eventIsNewer(event, current)) heads.set(coordinate, event);
  }

  return events.filter((event) => {
    if (event.kind !== KIND_MANAGED_AGENT) return true;
    const dTag = eventDTag(event);
    if (!dTag) return true;
    return (
      heads.get(`${event.pubkey.toLowerCase()}:${dTag.toLowerCase()}`) === event
    );
  });
}

/**
 * Dispatch owner catalog heads (30178) AFTER their constituents within the
 * complete hydration batch. A freshly shared 30178 head is typically newer than
 * the 30176 team and 30175 personas it projects, so relay newest-first order
 * places it first. Reconciling in that raw order lets the inbound team refresh
 * run while device B's personas have not hydrated: member resolution fails, and
 * the resolution-failure arm purges the just-retained witness and queues a
 * dominating false tombstone — deleting the owner's valid catalog entry on
 * ordinary first sync.
 *
 * Deferring only the 30178 heads to the end of the batch preserves newest-wins
 * within every other coordinate (order among non-catalog events is untouched)
 * while guaranteeing the constituents are all applied before any catalog
 * refresh could fire. A stable partition keeps relay order within each group.
 * This is only sound over a COMPLETE batch — `startPersonaSync` pages history to
 * exhaustion and buffers concurrent live events so every constituent is present
 * before the partition runs.
 */
export function orderCatalogHeadsLast(
  events: readonly RelayEvent[],
): RelayEvent[] {
  const constituents = events.filter(
    (event) => event.kind !== KIND_TEAM_CATALOG,
  );
  const catalogHeads = events.filter(
    (event) => event.kind === KIND_TEAM_CATALOG,
  );
  return [...constituents, ...catalogHeads];
}

// Fetch the owner's complete persona/team/agent/deletion history, paging past
// the relay's per-query `max_limit` clamp. The relay serves each REQ newest-first
// (`created_at DESC, id ASC`) and clamps `limit` to its advertised ceiling, so a
// single 500-event query cannot return a large owner's full history: a newer
// 30178/30176 could land in-page while an older required 30175 constituent falls
// outside it, and the ordered-last partition can only reorder what came back.
// Paging with the `until` time cursor (the only cursor the WS REQ filter exposes —
// the DB `before_id` keyset is REST-only) walks the full window to exhaustion.
//
// `until` is inclusive (`created_at <= until`), so each page re-returns the rows
// at the boundary second; `seen` dedupes them. A page shorter than the limit means
// the window is exhausted. A full page that adds nothing new (the whole page sits
// at one already-consumed second) also terminates — the pathological >500-heads-
// in-one-second case is unreachable for NIP-33 replaceable coordinates.
async function fetchOwnerHistoryToExhaustion(
  pubkey: string,
): Promise<RelayEvent[]> {
  const collected: RelayEvent[] = [];
  const seen = new Set<string>();
  let until: number | undefined;

  for (;;) {
    const page = await relayClient.fetchEvents({
      kinds: PERSONA_SYNC_KINDS,
      authors: [pubkey],
      limit: PERSONA_HISTORY_PAGE_LIMIT,
      ...(until === undefined ? {} : { until }),
    });

    let added = 0;
    let oldest = until ?? Number.POSITIVE_INFINITY;
    for (const event of page) {
      if (event.created_at < oldest) oldest = event.created_at;
      if (seen.has(event.id)) continue;
      seen.add(event.id);
      collected.push(event);
      added += 1;
    }

    if (page.length < PERSONA_HISTORY_PAGE_LIMIT || added === 0) break;
    until = oldest;
  }

  return collected;
}

// Start the persona/team/agent/deletion sync for `pubkey` on `relayUrl`:
// exhaustive backfill of existing heads + tombstones, then a live subscription.
// Returns a disposer that closes the live subscription. Extracted from the hook
// so the wiring is unit-testable without a React renderer (see
// `usePersonaSync.test.mjs`).
//
// `relayUrl` is the community this subscription is bound to, and every reconcile
// carries it as the event's arrival relay. Capturing it here — rather than
// letting the backend read whichever workspace is active when the reconcile runs
// — is what keeps an in-flight event out of the next community's scoped store.
//
// HYDRATION BOUNDARY. The history fetch and the live subscription start
// concurrently and feed one reconcile chain. A live/replayed 30178 that arrives
// before the backfill has reconciled its 30175/30176 constituents reproduces the
// false-tombstone purge (member resolution fails against an unhydrated store).
// Live events are therefore BUFFERED until the complete, dependency-ordered
// backfill has been dispatched, then drained in arrival order. Setting `hydrated`
// and draining the buffer are synchronous and uninterrupted, so no live event can
// slip past the boundary. Steady-state live events (after hydration) reconcile
// immediately.
export function startPersonaSync(
  pubkey: string,
  relayUrl: string,
  onCancelled: () => boolean,
): () => Promise<void> {
  // Reconcile in dispatch order. Managed-agent reconciliation can await a remote
  // provider deployment after releasing the local store lock; firing commands
  // independently lets an older broad policy finish after a newer restrictive
  // one. One chain per owner/relay subscription makes the newest event the last
  // deployment without serializing unrelated identities or communities.
  let reconcileChain = Promise.resolve();
  const reconcile = (event: RelayEvent) => {
    if (event.pubkey !== pubkey) return;
    reconcileChain = reconcileChain
      .then(() => reconcileInboundPersonaEvent(JSON.stringify(event), relayUrl))
      .catch((error) => {
        console.warn("[usePersonaSync] reconcile failed:", error);
      });
  };

  // Live events that arrive before the backfill finishes hydrating are held
  // here and drained once the constituents are reconciled.
  let hydrated = false;
  const liveBuffer: RelayEvent[] = [];
  const onLiveEvent = (event: RelayEvent) => {
    if (event.pubkey !== pubkey) return;
    if (hydrated) {
      reconcile(event);
    } else {
      liveBuffer.push(event);
    }
  };

  // Exhaustive one-shot backfill (closes the fresh-start gap that live-only
  // subscription + reconnect-replay cannot recover). Coalesce managed-agent
  // revisions, defer 30178 catalog heads past their constituents, dispatch the
  // ordered batch, THEN open the hydration boundary and drain buffered live
  // events — otherwise a fresh device retracts the owner's valid shared head.
  void fetchOwnerHistoryToExhaustion(pubkey)
    .then((events) => {
      if (onCancelled()) return;
      for (const event of orderCatalogHeadsLast(
        coalesceManagedAgentBackfill(events),
      ))
        reconcile(event);
      hydrated = true;
      for (const event of liveBuffer) reconcile(event);
      liveBuffer.length = 0;
    })
    .catch((error) => {
      console.warn("[usePersonaSync] backfill failed:", error);
    });

  let unsub: (() => Promise<void>) | null = null;
  void relayClient
    .subscribeLive(
      { kinds: PERSONA_SYNC_KINDS, authors: [pubkey], limit: 0 },
      onLiveEvent,
    )
    .then((dispose) => {
      if (onCancelled()) {
        void dispose();
      } else {
        unsub = dispose;
      }
    });

  return async () => {
    if (unsub) await unsub();
  };
}

// Subscribes to this device's own persona/team/agent projection + deletion
// events and patches each into the local store. The subscription is keyed on
// the active pubkey and relay: an identity or community switch re-runs the
// effect, whose cleanup closes the old subscription before a new one opens on
// the new filter — so no stale-coordinate subscription survives, and every
// reconcile is attributed to the community it was subscribed to.
//
// A fresh device that comes online AFTER another already published gets no
// history from a live-only subscription: relayClient's replayLiveSubscriptions
// only replays from a since-cursor that is undefined until the first live
// event arrives. So `startPersonaSync` does an explicit one-shot history fetch
// up front and feeds each event through the same reconcile path.
export function usePersonaSync(
  pubkey: string | undefined,
  relayUrl: string | undefined,
): void {
  React.useEffect(() => {
    if (!pubkey || !relayUrl) return;
    let cancelled = false;
    const dispose = startPersonaSync(pubkey, relayUrl, () => cancelled);
    return () => {
      cancelled = true;
      void dispose();
    };
  }, [pubkey, relayUrl]);
}
