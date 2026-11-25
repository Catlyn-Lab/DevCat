// Sidebar: the To do list agents keep for the focused project (todo_write), by group: Running,
// Next, Waiting on others, Waiting on you, Done. Without a focused project, a picker chooses the
// list (newest first). Tick an item done, or move it from its menu.
import { openUrl } from "@tauri-apps/plugin-opener";
import clsx from "clsx";
import { ArrowRightLeft, CheckCircle2, ChevronDown, ChevronRight, Circle, ExternalLink, FileText, ListChecks, Trash2 } from "lucide-react";
import { useState } from "react";
import { Select, Spinner } from "../../components/ui";
import type { TodoGroup, TodoItem } from "../../lib/types";
import { useConnections } from "../../store/connections";
import { useContextMenu, type MenuItem } from "../connections/ContextMenu";
import { openFilePreview } from "../files/format";
import { relativeTime } from "../history/historyActions";
import { useNow } from "../history/HistoryPanel";
import { byGroup, GROUPS, isWebLink, linkLabel } from "./model";
import { useTodos } from "./store";

function openLink(link: string) {
  if (isWebLink(link)) void openUrl(link.trim()).catch(() => {});
  else openFilePreview(link.trim());
}

export default function TodoPanel() {
  const focus = useConnections((s) => s.focusProjectId);
  const { lists, list, loaded, error, picked } = useTodos();
  const [doneOpen, setDoneOpen] = useState(false);
  const menu = useContextMenu();
  useNow(60_000);


  const groups = byGroup(list?.items ?? []);
  const total = list?.items.length ?? 0;
  const edit = useTodos.getState().edit;
  const move = (i: TodoItem, group: TodoGroup) => void edit([{ id: i.id, group }]);

  const itemMenu = (i: TodoItem): MenuItem[] => [
    {
      label: "Move to", icon: <ArrowRightLeft className="size-3.5" />, submenu: GROUPS.map((g): MenuItem => ({
        label: g.label, disabled: i.group === g.id, onClick: () => move(i, g.id),
      })),
    },
    ...(i.links?.length ? ["separator" as const, ...i.links.map((l): MenuItem => ({ label: `Open ${linkLabel(l)}`, icon: <ExternalLink className="size-3.5" />, onClick: () => openLink(l) }))] : []),
    "separator",
    { label: "Remove", icon: <Trash2 className="size-3.5" />, danger: true, onClick: () => void edit([], [i.id]) },
  ];

  return (
    <div className="flex h-full flex-col">
      {!focus && list && (
        <div className="flex h-9 shrink-0 items-center border-b border-line px-2">
          {lists.length > 1 ? (
            <Select value={list.key ?? picked ?? ""} onChange={(e) => useTodos.getState().pick(e.target.value)} className="h-6 w-full text-xs" title="Which project's list">
              {lists.map((l) => <option key={l.key} value={l.key}>{l.label}{l.open ? ` (${l.open})` : ""}</option>)}
            </Select>
          ) : (
            <span className="min-w-0 truncate px-1 text-xs text-muted" title={list.label}>{list.label}</span>
          )}
        </div>
      )}
      {error && <div className="selectable border-b border-line bg-danger/10 px-3 py-2 text-xs text-danger">{error}</div>}
      <div className="min-h-0 flex-1 overflow-y-auto">
        {!loaded ? (
          <div className="flex justify-center p-4"><Spinner /></div>
        ) : total === 0 ? (
          <div className="flex flex-col items-center gap-2 px-6 py-8 text-center text-xs text-faint">
            <ListChecks className="size-6" />
            {list ? <>No tasks for {list.label} yet.</> : <>No To do lists yet.</>}
            <span>Agents keep this list as they work: what's running, what's next, and what waits on someone (the todo_write tool).</span>
          </div>
        ) : GROUPS.map((g) => {
          const items = groups[g.id];
          const isDone = g.id === "done";
          const open = !isDone || doneOpen;
          return (
            <div key={g.id}>
              <button
                onClick={isDone ? () => setDoneOpen(!doneOpen) : undefined}
                className={clsx("sticky top-0 z-10 flex h-7 w-full items-center gap-1.5 border-b border-line bg-panel px-3 text-left text-[11px] font-semibold tracking-wider text-muted uppercase", !isDone && "cursor-default")}
              >
                {isDone && (doneOpen ? <ChevronDown className="size-3 shrink-0" /> : <ChevronRight className="size-3 shrink-0" />)}
                <span className="min-w-0 flex-1 truncate">{g.label}</span>
                <span className="font-normal tabular-nums text-faint">{items.length}</span>
              </button>
              {open && (items.length === 0 ? (
                <div className="px-3 py-1.5 text-xs text-faint">Nothing here.</div>
              ) : items.map((i) => (
                <div
                  key={i.id}
                  onContextMenu={(e) => menu.show(e, itemMenu(i))}
                  className="group flex gap-1.5 border-b border-line/40 px-3 py-1.5 hover:bg-hover"
                >
                  <button
                    title={isDone ? "Not done: back to Next" : "Done"}
                    onClick={() => move(i, isDone ? "next" : "done")}
                    className="mt-px shrink-0 self-start text-faint hover:text-accent"
                  >
                    {isDone ? <CheckCircle2 className="size-3.5 text-accent" /> : <Circle className="size-3.5" />}
                  </button>
                  <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                    <span className={clsx("selectable text-[13px] break-words", isDone && "text-muted line-through")}>{i.text}</span>
                    {i.waitingOn && <span className="text-[11px] text-muted">Waiting on {i.waitingOn}</span>}
                    {!!i.links?.length && (
                      <span className="flex flex-wrap gap-x-2 gap-y-0.5">
                        {i.links.map((l) => (
                          <button key={l} title={l} onClick={() => openLink(l)} className="flex max-w-full items-center gap-0.5 text-[11px] text-accent hover:underline">
                            {isWebLink(l) ? <ExternalLink className="size-3 shrink-0" /> : <FileText className="size-3 shrink-0" />}
                            <span className="truncate">{linkLabel(l)}</span>
                          </button>
                        ))}
                      </span>
                    )}
                    <span className="flex items-center gap-1.5 text-[11px] text-faint">
                      {i.owner && <span className="truncate">{i.owner}</span>}
                      <span className="shrink-0">{relativeTime(i.updatedAt)}</span>
                    </span>
                  </div>
                  <button
                    title="Move or remove"
                    onClick={(e) => menu.show(e, itemMenu(i))}
                    className="shrink-0 self-start rounded p-0.5 text-faint opacity-0 group-hover:opacity-100 hover:bg-hover hover:text-fg"
                  >
                    <ArrowRightLeft className="size-3.5" />
                  </button>
                </div>
              )))}
            </div>
          );
        })}
      </div>
      {menu.element}
    </div>
  );
}
