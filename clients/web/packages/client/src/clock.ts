/**
 * Time, injectable.
 *
 * The ack cadence and the redial backoff are behaviours worth testing at
 * exact instants, and a test that sleeps 16 real milliseconds per assertion
 * is a suite nobody runs. Production passes `systemClock`; tests pass a
 * manual one and advance it.
 */

export interface Clock {
  /** Milliseconds, monotonic-enough. */
  now(): number;
  schedule(fn: () => void, ms: number): TimerHandle;
  cancel(handle: TimerHandle): void;
}

export type TimerHandle = unknown;

export const systemClock: Clock = {
  now: () => Date.now(),
  schedule: (fn, ms) => setTimeout(fn, ms),
  cancel: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};
