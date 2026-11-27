// To do lists, one per project (or repository in no project). The backend keeps them in
// <data>/todos/<key>.json and sends "devcat://todos" after every change (an agent's todo_write, or
// this panel), so every window reloads. With DevCat closed while an agent wrote, the list is read
// again when the window gets focus. Agents keep the lists only while the user has it on (`enabled`,
// off at first): the switch is in the panel.
import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";
import { api, errMsg } from "../../lib/api";
import type { TodoChange, TodoList, TodoSummary } from "../../lib/types";

interface TodosState {
  lists: TodoSummary[];
  /** The list shown (none yet: the project has no list, or nothing is picked). */
  list: TodoList | null;
  /** The list picked when no project is focused. */
  picked: string | null;
  loaded: boolean;
  error: string | null;
  /** Agents keep the lists (they get the To do tools). Off until the user turns it on. */
  enabled: boolean;
  setEnabled: (on: boolean) => Promise<void>;
  /** Load the picker and the list for `focus` (a project id), else the picked / newest list. */
  load: (focus: string | null) => Promise<void>;
  pick: (key: string) => void;
  edit: (changes: TodoChange[], remove?: string[]) => Promise<void>;
}

let lastFocus: string | null = null;
/** Counts loads and changes of the switch: an answer asked for before a later one is dropped. */
let seq = 0;

export const useTodos = create<TodosState>((set, get) => ({
  lists: [],
  list: null,
  picked: null,
  loaded: false,
  error: null,
  enabled: false,
  setEnabled: async (on) => {
    const was = get().enabled;
    seq++; // a load asked for before this would put the old value back
    set({ enabled: on });
    try {
      await api.todosSetEnabled(on); // the backend's event reloads every window
    } catch (e) {
      set({ enabled: was, error: errMsg(e) });
    }
  },
  load: async (focus) => {
    lastFocus = focus;
    const mine = ++seq;
    try {
      const [lists, enabled] = await Promise.all([api.todosLists(), api.todosEnabled()]);
      // the switch first: it doesn't depend on the list, which may fail to load
      if (mine === seq) set({ enabled: !!enabled });
      let list: TodoList | null = null;
      if (focus) list = await api.todosGet({ projectId: focus });
      else {
        const key = lists.find((l) => l.key === get().picked)?.key ?? lists[0]?.key;
        if (key) list = await api.todosGet({ key });
      }
      if (mine !== seq) return; // a newer load (the focus changed) or the switch came meanwhile
      set({ lists, list, loaded: true, error: null });
    } catch (e) {
      if (mine === seq) set({ loaded: true, error: errMsg(e) });
    }
  },
  pick: (key) => {
    set({ picked: key });
    void get().load(null);
  },
  edit: async (changes, remove = []) => {
    const cur = get().list;
    if (!cur) return;
    // show it right away; the stored list follows
    const gone = new Set(remove);
    set({
      list: {
        ...cur,
        items: cur.items.filter((i) => !gone.has(i.id)).map((i) => {
          const c = changes.find((x) => x.id === i.id);
          return c?.group ? { ...i, group: c.group } : i;
        }),
      },
    });
    try {
      const list = await api.todosEdit(cur.key, changes, remove);
      if (get().list?.key === list.key) set({ list, error: null });
    } catch (e) {
      set({ list: cur, error: errMsg(e) });
    }
  },
}));

/** Reload when an agent (or another window) changed a list. */
export function reloadTodos() {
  void useTodos.getState().load(lastFocus);
}

void listen("devcat://todos", () => reloadTodos()).catch(() => {});
window.addEventListener("focus", () => {
  if (useTodos.getState().loaded) reloadTodos();
});
