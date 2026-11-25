// `pnpm check:todos`: the To do panel's grouping and link names (features/todos/model.ts).
// Runs under plain Node (type stripping): no node:* imports, no @types/node.
import { byGroup, groupLabel, isWebLink, linkLabel } from "./model.ts";

function eq(actual: unknown, expected: unknown, what: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${what}\n  expected ${e}\n  actual   ${a}`);
}

const item = (id: string, group: string, updatedAt = "2026-11-25T01:00:00Z") => ({ id, text: id, group, updatedAt }) as never;

const g = byGroup([item("a", "running"), item("b", "next"), item("c", "done", "2026-11-25T01:00:00Z"), item("d", "done", "2026-11-25T03:00:00Z"), item("e", "waiting_on_you")]);
eq(g.running.map((i) => i.id), ["a"], "running");
eq(g.next.map((i) => i.id), ["b"], "next");
eq(g.waiting_on_others, [], "empty group is there");
eq(g.waiting_on_you.map((i) => i.id), ["e"], "waiting on you");
eq(g.done.map((i) => i.id), ["d", "c"], "done newest first");
eq(byGroup([item("x", "someday")]).next.map((i) => i.id), ["x"], "an unknown group shows under Next");
eq(groupLabel("waiting_on_others"), "Waiting on others", "label");

eq(linkLabel("https://github.com/Catlyn-Lab/DevCat/pull/251"), "PR #251", "GitHub PR");
eq(linkLabel("https://github.com/Catlyn-Lab/DevCat/issues/250#issuecomment-1"), "issue #250", "GitHub issue");
eq(linkLabel("https://gitlab.com/g/p/-/merge_requests/9"), "MR !9", "GitLab MR");
eq(linkLabel("https://example.com/docs/"), "example.com/docs", "other URL");
eq(linkLabel("C:\\Users\\me\\repo\\src\\main.rs"), "main.rs", "Windows path");
eq(linkLabel("/home/me/repo/README.md"), "README.md", "Unix path");
eq(isWebLink(" https://x.y"), true, "web link");
eq(isWebLink("src/main.rs"), false, "path");

console.log("todos: ok");
