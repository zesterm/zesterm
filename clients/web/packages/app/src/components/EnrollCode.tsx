/**
 * A freshly minted enrolment code, shown where it was minted.
 *
 * The code renders in the mono face and is grouped by **letter-spacing only —
 * never by inserted separator characters**. The enrolment signature covers the
 * code bytes verbatim, so a display reading `ZK7M-2Q9T` teaches people to type
 * the dash, and the dash makes the signature fail with nothing naming the
 * cause. The same rule shapes the copy button: what lands on the clipboard is
 * exactly what the server minted (or, for a machine, the whole one-liner that
 * carries it).
 *
 * The countdown ticks against the wall clock rather than decrementing: a
 * laptop that slept through five minutes must wake to `expired`, not to a
 * confident `7:12` the server stopped honouring while the lid was shut.
 */

import { component, onUnmounted, signal } from 'sigx';

import { codeCountdown, copyOutcome } from '../fleet-model.ts';

export const EnrollCode = component<{
  kind: 'host' | 'device';
  code: string;
  expiresAt: number;
  /** True while the owner is already minting — the remint button says so. */
  busy: boolean;
  onRemint: () => void;
  onClose: () => void;
}>((ctx) => {
  const state = signal<{ now: number; flash: 'copied' | 'copy failed' | null }>({
    now: Date.now(),
    flash: null,
  });

  const ticker = setInterval(() => {
    state.now = Date.now();
  }, 1000);
  let copyFlash: ReturnType<typeof setTimeout> | null = null;
  onUnmounted(() => {
    clearInterval(ticker);
    if (copyFlash !== null) clearTimeout(copyFlash);
  });

  const copy = (text: string): void => {
    // `copyOutcome` never rejects: on plain http `navigator.clipboard` is
    // undefined and the unguarded call throws SYNCHRONOUSLY, so a `.catch`
    // on its result never runs and the handler errors. Every failure shape
    // lands here as `copy failed` — the code stays selectable on screen, and
    // the button flash is the only surface there is to say the copy didn't
    // take. `copied` is said when the write resolved, not when the click did.
    void copyOutcome(navigator.clipboard, text).then((outcome) => {
      state.flash = outcome;
      if (copyFlash !== null) clearTimeout(copyFlash);
      copyFlash = setTimeout(() => {
        state.flash = null;
      }, 1500);
    });
  };

  return () => {
    const { kind, code, expiresAt } = ctx.props;
    const left = codeCountdown(expiresAt, state.now);
    const command = `zest-daemon --enroll ${code}`;
    return (
      <div class="enroll-panel">
        <div class="enroll-head">
          <span class="enroll-title">{kind === 'host' ? 'Add a machine' : 'Add a device'}</span>
          <span class="grow" />
          <button class="button subtle" onClick={() => ctx.props.onClose()}>
            close
          </button>
        </div>
        {left === 'expired' ? (
          <div class="enroll-line">
            <span class="muted">This code has expired.</span>
            <button
              class="button subtle"
              disabled={ctx.props.busy}
              onClick={() => ctx.props.onRemint()}
            >
              {ctx.props.busy ? 'minting…' : 'mint another'}
            </button>
          </div>
        ) : (
          <>
            {kind === 'host' ? (
              <>
                <p class="fineprint">Run this on the machine you are adding:</p>
                <div class="enroll-line">
                  <code class="enroll-cmd">
                    zest-daemon --enroll <span class="enroll-code">{code}</span>
                  </code>
                  <button class="button subtle" onClick={() => copy(command)}>
                    {state.flash ?? 'copy'}
                  </button>
                </div>
              </>
            ) : (
              <>
                <p class="fineprint">Type this code into the app's sign-in screen.</p>
                <div class="enroll-line">
                  <span class="enroll-code">{code}</span>
                  <button class="button subtle" onClick={() => copy(code)}>
                    {state.flash ?? 'copy'}
                  </button>
                </div>
              </>
            )}
            <p class="enroll-expiry fineprint">expires in {left}</p>
          </>
        )}
      </div>
    );
  };
});
