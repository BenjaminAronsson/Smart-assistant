/**
 * Wire-event classification shared by every socket consumer (docs/05 §3).
 *
 * Transient events are broadcast directly and **never replayed**; domain events
 * come off the outbox and carry the durable `seq` a client resyncs against. A
 * consumer must therefore not let a transient event advance `lastSeq`, and must
 * not read one as a gap in the durable sequence — doing either turns every
 * occurrence into a needless timeline reload.
 *
 * This list lived in two components as byte-identical copies until S3 had to
 * widen both in lockstep. It is one exported constant now for the reason ADR-034
 * gives: the failure mode when duplicated definitions drift is silent.
 *
 * `voice.transcript` and `voice.error` are transient too and are deliberately
 * **not** here: both ride `Channel::Voice`, and every consumer of this set gates
 * on `channel === 'session'` first, so they can never reach the sequence logic.
 * Adding them would suggest the gate does not exist. (A contract review flagged
 * their absence; this note is the answer.)
 */
export const TRANSIENT_WS_TYPES: ReadonlySet<string> = new Set([
  'text.delta',
  'media.state',
  'hud.canvas',
  'degraded.queued',
  // S3/ADR-033 §4: how a run's answer may be spoken. Transient like the deltas
  // it labels — see the event's doc in `jarvis-contracts`.
  'run.speech_sensitive',
]);
