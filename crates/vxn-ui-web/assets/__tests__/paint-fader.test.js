import { describe, it, expect, beforeEach } from 'vitest';
import { paintFader } from '../panels.js';

// jsdom doesn't compute layout, so the fader/thumb dimensions are stubbed
// directly via Object.defineProperty. The helper only reads
// `fader.clientHeight` and `thumb.offsetHeight`.

const FADER_H = 100;
const THUMB_H = 20;
const HALF_THUMB = THUMB_H / 2;
const TRAVEL = FADER_H - THUMB_H; // 80

function makePair(faderH = FADER_H, thumbH = THUMB_H) {
  const fader = document.createElement('div');
  const thumb = document.createElement('div');
  fader.appendChild(thumb);
  document.body.appendChild(fader);
  Object.defineProperty(fader, 'clientHeight', { value: faderH, configurable: true });
  Object.defineProperty(thumb, 'offsetHeight', { value: thumbH, configurable: true });
  return { fader, thumb };
}

describe('paintFader', () => {
  let fader, thumb;

  beforeEach(() => {
    document.body.innerHTML = '';
    ({ fader, thumb } = makePair());
  });

  it('norm = 0 puts the thumb at the bottom (centre at faderH - halfThumb)', () => {
    paintFader(fader, thumb, 0);
    // thumb.style.top is the thumb's *top edge*; centre = top + halfThumb
    // = (halfThumb + travel) + halfThumb = faderH - halfThumb. So
    // thumb.style.top = halfThumb + travel = 10 + 80 = 90.
    expect(thumb.style.top).toBe(`${HALF_THUMB + TRAVEL}px`);
  });

  it('norm = 1 puts the thumb at the top (centre at halfThumb)', () => {
    paintFader(fader, thumb, 1);
    expect(thumb.style.top).toBe(`${HALF_THUMB}px`);
  });

  it('norm = 0.5 puts the thumb at the midpoint', () => {
    paintFader(fader, thumb, 0.5);
    expect(thumb.style.top).toBe(`${HALF_THUMB + 0.5 * TRAVEL}px`);
  });

  it('clamps norm below 0 to the bottom', () => {
    paintFader(fader, thumb, -0.5);
    expect(thumb.style.top).toBe(`${HALF_THUMB + TRAVEL}px`);
    expect(fader.style.getPropertyValue('--fader-norm')).toBe('0');
  });

  it('clamps norm above 1 to the top', () => {
    paintFader(fader, thumb, 1.5);
    expect(thumb.style.top).toBe(`${HALF_THUMB}px`);
    expect(fader.style.getPropertyValue('--fader-norm')).toBe('1');
  });

  it('sets --fader-norm to the clamped norm', () => {
    paintFader(fader, thumb, 0.25);
    expect(fader.style.getPropertyValue('--fader-norm')).toBe('0.25');
  });
});
