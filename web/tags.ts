/** User tags: name -> shortcut key, and track (relative path) -> tag names. Pure; the app PUTs the whole doc. */
export type Tags = { keys: Record<string, string>; tracks: Record<string, string[]> };

export const RESERVED = " jkhl?D";   // keys already taken by <mlm-player>
export const empty: Tags = { keys: {}, tracks: {} };

export const has = (t: Tags, rel: string, name: string) => (t.tracks[rel] ?? []).includes(name);

/** Add `name` to the track if missing, else remove it. Tracks with no tags are dropped from the doc. */
export function toggle(t: Tags, rel: string, name: string): Tags {
  const cur = t.tracks[rel] ?? [];
  const next = has(t, rel, name) ? cur.filter(n => n !== name) : [...cur, name];
  const tracks = { ...t.tracks, [rel]: next };
  if (!next.length) delete tracks[rel];
  return { ...t, tracks };
}

/** New tag definition, or an error message the form can show. */
export function add(t: Tags, name: string, key: string): Tags | string {
  name = name.trim();
  if (!name) return "name required";
  if (name in t.keys) return `"${name}" exists`;
  if ([...key].length !== 1) return "one key";
  if (RESERVED.includes(key)) return `${key === " " ? "space" : key} is a player key`;
  const used = Object.entries(t.keys).find(([, k]) => k === key);
  if (used) return `${key} is "${used[0]}"`;
  return { ...t, keys: { ...t.keys, [name]: key } };
}

/** Delete the definition and strip the name from every track. */
export function remove(t: Tags, name: string): Tags {
  const { [name]: _, ...keys } = t.keys;
  const tracks: Tags["tracks"] = {};
  for (const [rel, names] of Object.entries(t.tracks)) {
    const rest = names.filter(n => n !== name);
    if (rest.length) tracks[rel] = rest;
  }
  return { keys, tracks };
}
