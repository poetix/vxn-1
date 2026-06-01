import { describe, it, expect, beforeEach } from 'vitest';
import * as dispatch from '../dispatch.js';

const { paramIdByName, paramIdByNameAtLayer, _resetParamIndex } = dispatch;

const PATCH_COUNT = 100;

beforeEach(() => {
  globalThis.window = globalThis;
  window.vxn = {
    patchCount: PATCH_COUNT,
    params: {
      // Per-patch param: Upper id = 0, Lower id = 0 + patchCount.
      0:                  { name: 'cutoff', variants: [] },
      [PATCH_COUNT]:      { name: 'cutoff', variants: [] },
      // Per-patch enum.
      1:                  { name: 'mode', variants: ['A', 'B'] },
      [PATCH_COUNT + 1]:  { name: 'mode', variants: ['A', 'B'] },
      // Global param (id ≥ 2·patchCount, layer-independent).
      [2 * PATCH_COUNT]:  { name: 'master_gain', variants: [] },
    },
  };
  _resetParamIndex();
});

describe('paramIdByName', () => {
  it('maps every name to its lowest (Upper) id', () => {
    expect(paramIdByName('cutoff')).toBe(0);
    expect(paramIdByName('mode')).toBe(1);
    expect(paramIdByName('master_gain')).toBe(2 * PATCH_COUNT);
  });

  it('returns null for an unknown name', () => {
    expect(paramIdByName('does_not_exist')).toBeNull();
  });

  it('builds the index exactly once across many lookups (cache hit on every call after the first)', () => {
    // Pre-flight: cache is empty (beforeEach reset both _paramIdByName and
    // the build counter).
    expect(dispatch._paramIndexBuilds).toBe(0);
    paramIdByName('cutoff');
    paramIdByName('mode');
    paramIdByName('master_gain');
    paramIdByName('cutoff'); // repeats fine
    paramIdByName('does_not_exist');
    expect(dispatch._paramIndexBuilds).toBe(1);
  });

  it('a cache reset reflects a fresh window.vxn.params snapshot', () => {
    // Cold cache resolves against the beforeEach fixture.
    expect(paramIdByName('cutoff')).toBe(0);
    // Mutate the params table without invalidating; cache wins.
    window.vxn.params = {
      99: { name: 'cutoff', variants: [] },
    };
    expect(paramIdByName('cutoff')).toBe(0);
    // After reset the next call rebuilds against the new table.
    _resetParamIndex();
    expect(paramIdByName('cutoff')).toBe(99);
  });
});

describe('paramIdByNameAtLayer', () => {
  it('translates Upper → Lower (+patchCount) for per-patch ids on the lower layer', () => {
    expect(paramIdByNameAtLayer('cutoff', 'upper')).toBe(0);
    expect(paramIdByNameAtLayer('cutoff', 'lower')).toBe(PATCH_COUNT);
    expect(paramIdByNameAtLayer('mode', 'lower')).toBe(PATCH_COUNT + 1);
  });

  it('passes globals through unchanged on either layer', () => {
    expect(paramIdByNameAtLayer('master_gain', 'upper')).toBe(2 * PATCH_COUNT);
    expect(paramIdByNameAtLayer('master_gain', 'lower')).toBe(2 * PATCH_COUNT);
  });

  it('returns null for an unknown name on either layer', () => {
    expect(paramIdByNameAtLayer('nope', 'upper')).toBeNull();
    expect(paramIdByNameAtLayer('nope', 'lower')).toBeNull();
  });
});
