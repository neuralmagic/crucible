import { describe, expect, it } from 'vitest';
import { parsePickerPrefs, PICKER_PREFS_DEFAULTS } from './pickerPrefs';

describe('parsePickerPrefs', () => {
  it('falls back to the defaults for a missing or foreign blob', () => {
    expect(parsePickerPrefs(undefined)).toEqual(PICKER_PREFS_DEFAULTS);
    expect(parsePickerPrefs({ somebodyElses: 1 })).toEqual(PICKER_PREFS_DEFAULTS);
  });

  it('reads what it stored', () => {
    expect(
      parsePickerPrefs({ ownerFavorites: ['group:/groups/team-x'], ownerFilter: 'team' }),
    ).toEqual({ ownerFavorites: ['group:/groups/team-x'], ownerFilter: 'team' });
  });

  it('drops a favorite that is not a string and a filter that is not one', () => {
    const prefs = parsePickerPrefs({ ownerFavorites: ['a', 3, null, 'b'], ownerFilter: 7 });
    expect(prefs.ownerFavorites).toEqual(['a', 'b']);
    expect(prefs.ownerFilter).toBe('');
  });
});
