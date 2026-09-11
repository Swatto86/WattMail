/** Folder row the sidebar sorter needs. Extra fields pass through unchanged. */
export type SidebarFolder = { id: string; unreadCount: number; depth: number };

/**
 * Pinned blocks first (folder + descendants, indent reset), then unpinned
 * folders with unread — same block lift — then the rest in original tree order.
 * Nested pin/unread roots that sit under an already-lifted ancestor stay there.
 */
export function foldersForSidebar<T extends SidebarFolder>(
  list: T[],
  isPinned: (id: string) => boolean,
): Array<T & { indent: number }> {
  const taken = new Set<number>();
  const pinned = liftBlocks(list, taken, (f) => isPinned(f.id));
  const unread = liftBlocks(list, taken, (f) => f.unreadCount > 0);
  const rest: Array<T & { indent: number }> = [];
  for (let i = 0; i < list.length; i++) {
    if (!taken.has(i)) rest.push({ ...list[i], indent: list[i].depth });
  }
  return pinned.concat(unread, rest);
}

function liftBlocks<T extends SidebarFolder>(
  list: T[],
  taken: Set<number>,
  want: (f: T) => boolean,
): Array<T & { indent: number }> {
  const rootAt: boolean[] = new Array(list.length).fill(false);
  const ancestor: boolean[] = new Array(list.length).fill(false);
  const stack: number[] = [];
  for (let i = 0; i < list.length; i++) {
    while (stack.length && list[stack[stack.length - 1]].depth >= list[i].depth) {
      stack.pop();
    }
    const parent = stack.length ? stack[stack.length - 1] : -1;
    const parentLifted = parent >= 0 && (rootAt[parent] || ancestor[parent]);
    ancestor[i] = parentLifted;
    rootAt[i] = !taken.has(i) && want(list[i]) && !parentLifted;
    stack.push(i);
  }
  const out: Array<T & { indent: number }> = [];
  for (let i = 0; i < list.length; i++) {
    if (!rootAt[i]) continue;
    const depth = list[i].depth;
    let end = i + 1;
    while (end < list.length && list[end].depth > depth) end++;
    for (let j = i; j < end; j++) {
      if (taken.has(j)) continue;
      taken.add(j);
      out.push({ ...list[j], indent: list[j].depth - depth });
    }
  }
  return out;
}
