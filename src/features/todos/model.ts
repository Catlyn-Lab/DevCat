// To do list helpers without React or Tauri (`pnpm check:todos` runs them under plain Node).
import type { TodoGroup, TodoItem } from "../../lib/types";

export const GROUPS: { id: TodoGroup; label: string }[] = [
  { id: "running", label: "Running" },
  { id: "next", label: "Next" },
  { id: "waiting_on_others", label: "Waiting on others" },
  { id: "waiting_on_you", label: "Waiting on you" },
  { id: "done", label: "Done" },
];

export function groupLabel(g: TodoGroup): string {
  return GROUPS.find((x) => x.id === g)?.label ?? g;
}

/** Items per group, in the list's order; done ones newest first. */
export function byGroup(items: TodoItem[]): Record<TodoGroup, TodoItem[]> {
  const out = Object.fromEntries(GROUPS.map((g) => [g.id, [] as TodoItem[]])) as Record<TodoGroup, TodoItem[]>;
  for (const i of items) (out[i.group] ?? out.next).push(i);
  out.done.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
  return out;
}

export function isWebLink(link: string): boolean {
  return /^https?:\/\//i.test(link.trim());
}

/** A short name for a link: `PR #12`, `issue #250`, a file's name, or the address without its scheme. */
export function linkLabel(link: string): string {
  const l = link.trim();
  const gh = /^https?:\/\/(?:www\.)?github\.com\/[^/]+\/([^/]+)\/(pull|issues|discussions)\/(\d+)/i.exec(l);
  if (gh) return `${gh[2] === "pull" ? "PR" : gh[2] === "issues" ? "issue" : "discussion"} #${gh[3]}`;
  const gl = /^https?:\/\/[^/]+\/.+\/-\/(merge_requests|issues)\/(\d+)/i.exec(l);
  if (gl) return gl[1] === "merge_requests" ? `MR !${gl[2]}` : `issue #${gl[2]}`;
  if (isWebLink(l)) {
    const s = l.replace(/^https?:\/\/(www\.)?/i, "").replace(/\/$/, "");
    return s.length > 40 ? `${s.slice(0, 39)}…` : s;
  }
  const name = l.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? l;
  return name || l;
}
