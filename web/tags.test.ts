import { expect, test } from "bun:test";
import { add, empty, has, remove, toggle } from "./tags";

test("add / toggle / remove", () => {
  let t = add(empty, "fav", "f");
  expect(typeof t).toBe("object");
  t = add(t as any, "chill", "c") as any;
  expect(add(t as any, " ", "x")).toBe("name required");
  expect(add(t as any, "fav", "x")).toBe('"fav" exists');
  expect(add(t as any, "loud", "j")).toMatch(/player key/);
  expect(add(t as any, "loud", " ")).toMatch(/space is a player key/);
  expect(add(t as any, "loud", "f")).toBe('f is "fav"');
  expect(add(t as any, "loud", "ab")).toBe("one key");

  let d = t as any;
  d = toggle(d, "a/1.aiff", "fav");
  d = toggle(d, "a/1.aiff", "chill");
  d = toggle(d, "b/2.mp3", "fav");
  expect(d.tracks).toEqual({ "a/1.aiff": ["fav", "chill"], "b/2.mp3": ["fav"] });
  d = toggle(d, "b/2.mp3", "fav");
  expect(has(d, "b/2.mp3", "fav")).toBe(false);
  expect("b/2.mp3" in d.tracks).toBe(false);   // empty entries dropped

  d = remove(d, "fav");
  expect(d.keys).toEqual({ chill: "c" });
  expect(d.tracks).toEqual({ "a/1.aiff": ["chill"] });
  expect(remove(d, "chill").tracks).toEqual({});
});
