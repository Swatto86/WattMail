import { foldersForSidebar } from "../src/folder-sidebar.ts";

function f(id, unreadCount, depth) {
  return { id, unreadCount, depth };
}

function ids(list, pinned = []) {
  const pin = new Set(pinned);
  return foldersForSidebar(list, (id) => pin.has(id)).map((x) => x.id);
}

function indents(list, pinned = []) {
  const pin = new Set(pinned);
  return foldersForSidebar(list, (id) => pin.has(id)).map((x) => `${x.id}:${x.indent}`);
}

function assertEq(got, want, label) {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) throw new Error(`${label}: got ${g}, want ${w}`);
}

const tree = [
  f("inbox", 0, 0),
  f("projects", 0, 0),
  f("alpha", 2, 1),
  f("beta", 0, 1),
  f("archive", 0, 0),
  f("sent", 0, 0),
];

assertEq(
  ids(tree.map((x) => ({ ...x, unreadCount: 0 }))),
  ["inbox", "projects", "alpha", "beta", "archive", "sent"],
  "tree order when nothing is pinned or unread",
);
assertEq(ids(tree), ["alpha", "inbox", "projects", "beta", "archive", "sent"], "unread nested folder lifts above empty roots");
assertEq(
  ids(tree, ["archive"]),
  ["archive", "alpha", "inbox", "projects", "beta", "sent"],
  "pin first, then unread block (alpha), then rest in tree order",
);
assertEq(
  ids(tree, ["archive", "inbox"]),
  ["inbox", "archive", "alpha", "projects", "beta", "sent"],
  "pins keep relative tree order; unread sits under pins",
);

const cleared = tree.map((x) => (x.id === "alpha" ? { ...x, unreadCount: 0 } : x));
assertEq(
  ids(cleared, ["archive"]),
  ["archive", "inbox", "projects", "alpha", "beta", "sent"],
  "zero unread drops alpha back into tree order under pins",
);

assertEq(ids([f("fav", 3, 0), f("inbox", 1, 0), f("sent", 0, 0)], ["fav"]), ["fav", "inbox", "sent"], "pinned unread stays in the pin block");

const nestedPin = [f("parent", 0, 0), f("child", 0, 1), f("grand", 0, 2), f("other", 1, 0)];
assertEq(ids(nestedPin, ["child"]), ["child", "grand", "other", "parent"], "nested pin lifts subtree; unread other follows");
assertEq(indents(nestedPin, ["child"]), ["child:0", "grand:1", "other:0", "parent:0"], "nested pin resets indent");

assertEq(
  ids([f("work", 4, 0), f("client", 0, 1), f("inbox", 0, 0)]),
  ["work", "client", "inbox"],
  "unread parent lifts its subtree as one block",
);
assertEq(
  ids([f("work", 0, 0), f("client", 0, 1), f("inbox", 0, 0)]),
  ["work", "client", "inbox"],
  "clearing unread parent restores tree order",
);

assertEq(
  ids([f("work", 2, 0), f("starred", 0, 1), f("leaf", 0, 2), f("inbox", 1, 0)], ["starred"]),
  ["starred", "leaf", "work", "inbox"],
  "nested pin still wins; unread parent is not re-lifted with the pinned child",
);

console.log("folder-sidebar order checks OK");
