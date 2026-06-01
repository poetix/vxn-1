import { describe, it, expect, beforeEach, vi } from 'vitest';
import { wireDrag } from '../panels.js';

// `wireFaderDrag` (a thin wrapper around `wireDrag`) gets its own focused
// coverage in wire-fader-drag.test.js. This suite exercises the
// generalisation: the delta-based `pointerToValue` path that the wave knob
// relies on, and the hover-during-drag suppression contract shared by both.

function makeEl() {
  const el = document.createElement('div');
  document.body.appendChild(el);
  el.setPointerCapture = vi.fn();
  el.releasePointerCapture = vi.fn();
  return el;
}

function pointerEvt(type, { clientY = 0, pointerId = 11 } = {}) {
  const ev = new MouseEvent(type, { bubbles: true, cancelable: true });
  Object.defineProperty(ev, 'pointerId', { value: pointerId });
  Object.defineProperty(ev, 'clientY', { value: clientY });
  return ev;
}

describe('wireDrag — delta-based map (wave-knob style)', () => {
  let el, onDown, onMove, onUp, downContext, pointerToValue, drag;

  beforeEach(() => {
    document.body.innerHTML = '';
    el = makeEl();
    onDown = vi.fn();
    onMove = vi.fn();
    onUp   = vi.fn();
    // Wave knob's actual shape: ctx captures start state; pointerToValue
    // reads ctx every move. PIXELS_PER_DETENT = 30 elsewhere — use 10 here
    // for shorter test arithmetic.
    downContext = vi.fn((ev) => ({ y0: ev.clientY, v0: 2 }));
    pointerToValue = vi.fn((ev, ctx) => ctx.v0 + (ctx.y0 - ev.clientY) / 10);
    drag = wireDrag(el, { downContext, pointerToValue },
      { onDown, onMove, onUp });
  });

  it('downContext fires once on pointerdown; the returned ctx is passed to pointerToValue', () => {
    el.dispatchEvent(pointerEvt('pointerdown', { clientY: 50 }));
    expect(downContext).toHaveBeenCalledTimes(1);
    expect(pointerToValue).toHaveBeenCalledTimes(1);
    expect(pointerToValue.mock.calls[0][1]).toEqual({ y0: 50, v0: 2 });
    // Initial pointerToValue at down-time: dy = 0 → unchanged.
    expect(onDown.mock.calls[0][1]).toBe(2);
  });

  it('pointermove uses the captured ctx, not a fresh one', () => {
    el.dispatchEvent(pointerEvt('pointerdown', { clientY: 50 }));
    downContext.mockClear();
    el.dispatchEvent(pointerEvt('pointermove', { clientY: 20 })); // dy=30 → +3
    el.dispatchEvent(pointerEvt('pointermove', { clientY: 80 })); // dy=-30 → -3
    expect(downContext).not.toHaveBeenCalled();
    expect(onMove).toHaveBeenCalledTimes(2);
    expect(onMove.mock.calls[0][1]).toBe(5);
    expect(onMove.mock.calls[1][1]).toBe(-1);
  });

  it('passes pointerToValue\'s return value through verbatim — wireDrag does not interpret it', () => {
    // Make pointerToValue return a sentinel object; the helper should not
    // mutate or unwrap it.
    const sentinel = { tag: 'opaque' };
    pointerToValue.mockReturnValue(sentinel);
    el.dispatchEvent(pointerEvt('pointerdown', { clientY: 0 }));
    expect(onDown.mock.calls[0][1]).toBe(sentinel);
    el.dispatchEvent(pointerEvt('pointermove', { clientY: 5 }));
    expect(onMove.mock.calls[0][1]).toBe(sentinel);
  });

  it('downContext is optional — drags with no start-state work too', () => {
    document.body.innerHTML = '';
    const el2 = makeEl();
    const ptv = vi.fn(() => 7);
    const onDown2 = vi.fn();
    wireDrag(el2, { pointerToValue: ptv }, { onDown: onDown2 });
    el2.dispatchEvent(pointerEvt('pointerdown', { clientY: 0 }));
    expect(onDown2).toHaveBeenCalledWith(expect.anything(), 7);
    // ctx arg is null when no downContext is provided.
    expect(ptv.mock.calls[0][1]).toBeNull();
  });

  it('pointer capture + dragging class lifecycle is identical to the fader contract', () => {
    el.dispatchEvent(pointerEvt('pointerdown', { clientY: 0, pointerId: 11 }));
    expect(el.setPointerCapture).toHaveBeenCalledWith(11);
    expect(el.classList.contains('dragging')).toBe(true);
    el.dispatchEvent(pointerEvt('pointerup', { pointerId: 11 }));
    expect(el.releasePointerCapture).toHaveBeenCalledWith(11);
    expect(el.classList.contains('dragging')).toBe(false);
    expect(onUp).toHaveBeenCalledTimes(1);
  });

  it('pointercancel ends the drag like pointerup', () => {
    el.dispatchEvent(pointerEvt('pointerdown', { clientY: 0 }));
    el.dispatchEvent(pointerEvt('pointercancel'));
    expect(onUp).toHaveBeenCalledTimes(1);
    expect(drag.isDragging()).toBe(false);
  });
});

describe('wireDrag — hover-during-drag suppression', () => {
  let el, onEnter, onLeave;

  beforeEach(() => {
    document.body.innerHTML = '';
    el = makeEl();
    onEnter = vi.fn();
    onLeave = vi.fn();
    wireDrag(el, { pointerToValue: () => 0 }, { onEnter, onLeave });
  });

  it('onEnter fires when not dragging', () => {
    el.dispatchEvent(pointerEvt('pointerenter'));
    expect(onEnter).toHaveBeenCalledTimes(1);
  });

  it('onEnter is suppressed while dragging', () => {
    el.dispatchEvent(pointerEvt('pointerdown'));
    onEnter.mockClear();
    el.dispatchEvent(pointerEvt('pointerenter'));
    expect(onEnter).not.toHaveBeenCalled();
  });

  it('onLeave is deferred until drag-end when pointer leaves mid-drag', () => {
    el.dispatchEvent(pointerEvt('pointerenter'));
    el.dispatchEvent(pointerEvt('pointerdown'));
    el.dispatchEvent(pointerEvt('pointerleave'));
    expect(onLeave).not.toHaveBeenCalled();
    el.dispatchEvent(pointerEvt('pointerup'));
    expect(onLeave).toHaveBeenCalledTimes(1);
  });

  it('onLeave does not double-fire when still hovering at drag-end', () => {
    el.dispatchEvent(pointerEvt('pointerenter'));
    el.dispatchEvent(pointerEvt('pointerdown'));
    el.dispatchEvent(pointerEvt('pointerup'));
    expect(onLeave).not.toHaveBeenCalled();
  });
});
