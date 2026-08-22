/**
 * The key-cap row under the terminal (design screen 10, on the web): the
 * keys a soft keyboard has not got. Thin on purpose — `keybar-model.ts`
 * decides what each cap means and how the sticky modifiers latch; this
 * component only draws the row and reports taps.
 *
 * The one rule that makes it work on a tablet: a cap must **never take
 * focus**. The terminal's hidden textarea is what keeps the soft keyboard
 * up, and a button that focused itself on tap would dismiss the keyboard
 * with every Esc. `pointerdown` is cancelled (which stops the focus move on
 * touch as well as mouse — `mousedown` is synthesised too late on touch to
 * do it) and the tap acts on `click`.
 */

import { component } from 'sigx';

import { CAPS, type CapId, type LatchState } from '../keybar-model.ts';

export const KeyBar = component<{
  latch: LatchState;
  onCap: (id: CapId) => void;
}>((ctx) => {
  return () => {
    const { latch } = ctx.props;
    return (
      <div class="key-bar" role="toolbar" aria-label="terminal keys">
        {CAPS.map((cap) => {
          const state = cap.id === 'ctrl' ? latch.ctrl : cap.id === 'alt' ? latch.alt : 'off';
          return (
            <button
              key={cap.id}
              class={`key-cap${state === 'off' ? '' : ` ${state}`}${cap.id === 'kbd' ? ' kbd' : ''}`}
              title={cap.title}
              aria-label={cap.title}
              aria-pressed={cap.id === 'ctrl' || cap.id === 'alt' ? state !== 'off' : undefined}
              onPointerDown={(e: PointerEvent) => e.preventDefault()}
              onClick={() => ctx.props.onCap(cap.id)}
            >
              {cap.label}
            </button>
          );
        })}
      </div>
    );
  };
});
