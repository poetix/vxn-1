import { describe, it, expect, beforeEach } from 'vitest';
import { tgRow } from '../panels.js';

beforeEach(() => {
  document.body.innerHTML = '';
});

describe('tgRow — standalone form', () => {
  it('returns a fresh .ctl-tg-row with box + label children', () => {
    const row = tgRow('legato');
    expect(row.classList.contains('ctl-tg-row')).toBe(true);
    expect(row.querySelector('.ctl-tg-box')).not.toBeNull();
    expect(row.querySelector('.ctl-tg-lbl')).not.toBeNull();
  });

  it('uppercases the supplied label', () => {
    const row = tgRow('legato');
    expect(row.querySelector('.ctl-tg-lbl').textContent).toBe('LEGATO');
  });

  it('two separate calls produce independent DOM nodes', () => {
    const a = tgRow('one');
    const b = tgRow('two');
    expect(a).not.toBe(b);
    expect(a.querySelector('.ctl-tg-lbl').textContent).toBe('ONE');
    expect(b.querySelector('.ctl-tg-lbl').textContent).toBe('TWO');
    // Mutating one row's contents must not leak into the other.
    a.querySelector('.ctl-tg-lbl').textContent = 'PATCHED';
    expect(b.querySelector('.ctl-tg-lbl').textContent).toBe('TWO');
  });
});

describe('tgRow — mount form', () => {
  it('returns the supplied target and fills it in place', () => {
    const target = document.createElement('div');
    target.className = 'ctl-detune-legato ctl-tg-row';
    document.body.appendChild(target);
    const result = tgRow('legato', { mount: target });
    expect(result).toBe(target);
    expect(target.querySelector('.ctl-tg-box')).not.toBeNull();
    expect(target.querySelector('.ctl-tg-lbl').textContent).toBe('LEGATO');
  });

  it('does not overwrite the target\'s class (caller\'s classes apply)', () => {
    const target = document.createElement('div');
    target.className = 'ctl-detune-legato ctl-tg-row';
    tgRow('legato', { mount: target });
    expect(target.className).toBe('ctl-detune-legato ctl-tg-row');
  });
});
