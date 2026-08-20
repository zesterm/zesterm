/**
 * The registry's audit log (#373): every act that changes what an account
 * trusts, recorded by the handler that performed it.
 *
 * Written AFTER the mutation it describes and awaited, not fire-and-forget: a
 * request that changed the registry but could not say so should fail loudly,
 * because a log with silent holes teaches people to distrust the whole log —
 * and D1 gives no transaction across two `prepare` calls, so "mutation
 * happened, event lost" is already the worst case either way; keeping the
 * write adjacent and awaited makes it as narrow as this store allows.
 */

import type { Db, EnrollKind } from './types.ts';

/** The kind of authority behind an act — see the migration's comment. */
export type EventActor = 'owner' | 'device' | 'machine';

export type EventAction = 'revoke' | 'restore' | 'approve' | 'enroll' | 'register' | 'claim';

export interface RegistryEvent {
  readonly action: EventAction;
  readonly actor: EventActor;
  readonly subjectKind: EnrollKind;
  readonly subjectId: string;
  readonly subjectLabel: string;
  readonly at: number;
}

export async function recordRegistryEvent(
  db: Db,
  userId: string,
  e: RegistryEvent,
): Promise<void> {
  await db
    .prepare(
      `INSERT INTO registry_events (user_id, actor, action, subject_kind, subject_id, subject_label, at)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(userId, e.actor, e.action, e.subjectKind, e.subjectId, e.subjectLabel, e.at)
    .run();
}

/**
 * The account's recent history, newest first. Bounded because this backs a
 * screen section, not an export — and the bound is the server's, so a client
 * cannot ask an account's whole life onto one response.
 */
export async function listRegistryEvents(
  db: Db,
  userId: string,
  limit = 50,
): Promise<RegistryEvent[]> {
  const { results } = await db
    .prepare(
      `SELECT actor, action, subject_kind, subject_id, subject_label, at
         FROM registry_events WHERE user_id = ?
        ORDER BY at DESC, id DESC LIMIT ?`,
    )
    .bind(userId, Math.min(Math.max(1, limit), 200))
    .all<{
      actor: EventActor;
      action: EventAction;
      subject_kind: EnrollKind;
      subject_id: string;
      subject_label: string;
      at: number;
    }>();
  // Field by field, `publicUser`'s rule: the table may grow columns, and a
  // spread would ship each new one to the browser by default.
  return results.map((r) => ({
    action: r.action,
    actor: r.actor,
    subjectKind: r.subject_kind,
    subjectId: r.subject_id,
    subjectLabel: r.subject_label,
    at: r.at,
  }));
}
