# SQLite over SSH

## Intent

The user keeps SQLite databases on their servers (apps running there write to them) and wants to open them in
DevCat like a local SQLite file: browse tables, run queries, edit rows. Today a SQLite connection can only open a
file on this computer (`db/sqlite.rs`), and SQLite isn't in the dialog's `SSH_KINDS`.

## Goals

- A SQLite connection gets the SSH tab every other database has. With SSH on, "Database file" is a path **on the
  SSH host**, and DevCat works on that file **live, on the host**, through the host's own `sqlite3` command.
- Everything a local SQLite connection does (tree, table data, structure, editing rows, DDL, schema snapshot,
  query tab, export) works the same way, through the same `Driver` trait.
- When the host has no usable `sqlite3`, DevCat still opens the database: it downloads a copy and opens that
  **read-only**, with a way to refresh it, and says clearly that it's a snapshot and why.
- A mistyped path never quietly creates an empty database on the server (see "Missing file").

## Non-goals

- Writing the downloaded copy back to the host. Never: an app on the host may be writing to the file, and
  uploading over it loses its changes or corrupts the database.
- A Python or other helper on the host (considered, not chosen: option A, `sqlite3` only).
- Windows SSH hosts. Linux / macOS / BSD hosts (`uname` works, POSIX `sh`). A Windows host gets a clear error.
- Choosing an SSH *session* from the sidebar as the login. The connection's own SSH tab is the login, as for
  Postgres or JDBC over SSH.
- A remote file browser for picking the path (can come later); the path is typed.

## Anti-goals (would make it a failure even if it works)

- A query box that runs shell commands on the server. `sqlite3` reads dot-commands (`.shell`, `.system`, `.open`,
  `.output`, `.import`, …) from its input, and `readfile()` / `writefile()` / `load_extension()` / `ATTACH` reach
  other files. A user's or an agent's query must never get to any of them (see Security).
- A "read-only" connection or a read-access agent that can still write to the remote file.
- Silently showing stale data: the snapshot mode must always look like a snapshot.

## Approaches considered

1. **Download over SFTP, work locally, upload back.** Simplest, but writes are unsafe against a live app on the
   host, and big files are slow. Kept only as the read-only fallback.
2. **SQLite reads the remote file page by page over SFTP (custom VFS).** No install needed, but sqlx has no VFS
   hook, SFTP has no locking (so effectively read-only), and every page is a round trip.
3. **Run SQLite on the host, send queries over SSH.** Live data and safe writes (the host's SQLite does the
   locking). **Chosen**, with the host's `sqlite3` command (not a Python helper), falling back to 1 (read-only) when
   there is no `sqlite3` on the host at all.

## Design

### Settings and dialog

- `SSH_KINDS` gains `sqlite`. The SSH tab's checkbox reads "Open the file on another computer over SSH", with a
  hint: DevCat runs `sqlite3` there; without it, a read-only copy is downloaded.
- With SSH on, the General tab's "Database file" is a text field for the path on the host (`~` allowed, expanded
  on the host), without Browse. "Create new" becomes a checkbox, "Create the file if it doesn't exist", which
  sets `options.create = "true"` (the key the local driver already reads).
- The settings are the existing ones: `file_path` (now possibly a host path) and `ssh: SshConfig`; no new
  fields. `ConnectionConfig.file_path` doc comment updated.
- Callers that treat a SQLite `file_path` as a local file must check `ssh.enabled` first. Known ones:
  `mcp_sync.rs` (matching connections by file, ~line 625) and `db/transfer.rs` (~line 2041). The builder greps
  for the rest.
- Agents can't make one: `devcat_connection_add` has no SSH input for any kind, and this work doesn't add one
  (an agent naming SSH hosts, passwords and key files on this computer needs its own rules first).

### Driver

`db::connect` sends a SQLite connection with SSH on to a new driver (`db/sqlite_ssh.rs`; name is the builder's
call). It:

1. Logs in with `db::ssh::connect_session` (the same as JDBC over SSH; `ssh_run` / `detect_shell` in `jdbc.rs`
   can move to `db/ssh.rs` and be shared).
2. Checks the host: `uname -s` (refuse Windows), `command -v sqlite3`, `sqlite3 -version`. Any `sqlite3` →
   **live mode**; none → **snapshot mode**. Then checks the file (see "Missing file").
3. **Live mode:** starts one long-lived `sqlite3` on an exec channel, like the local driver's single shared
   connection (so temp tables and explicit transactions behave like one session):
   `sqlite3 -batch [-readonly] [-safe] -- '<path>'` with the path shell-quoted. It feeds statements on
   stdin and reads results from stdout and errors from stderr, using a marker (`.print <random token>`,
   or `SELECT '<token>'`) after each statement so it knows where each statement's output ends.
   **Output format: builder's call, aiming for exact values on as old a `sqlite3` as practical.** `.mode quote`
   (exact integers, BLOBs as `X'…'`, strings told apart from numbers; works on old versions) is preferred over
   `-json` (3.33+, loses BLOBs and big integers). Column names of a result with no rows still have to come out
   (e.g. `.headers on` plus a fallback); the builder finds a way and tests it. Statements are
   split with `sql_common::split_script(Dialect::Sqlite, …)`. `PRAGMA busy_timeout = 5000` at start, as locally.
4. **Snapshot mode:** downloads the file (plus `<file>-wal` if present) with `cat` over the session into a
   private folder in DevCat's data folder. A copy taken while an app writes can be torn: after downloading,
   run `PRAGMA quick_check` on it locally and retry a couple of times before reporting that no clean copy could
   be taken. It then opens it with the existing local `SqliteDriver` read-only, and deletes
   the folder when the connection closes. Refresh = download again and reopen.
5. Implements the whole `Driver` trait on top. Introspection (`children`, `table_structure`, `object_ddl`,
   `schema_snapshot`) reuses the SQL the local driver runs (`sqlite_master`, `pragma_table_info`, …), so as much
   of `sqlite.rs` as possible should be shared rather than copied.
6. `cancel`: the builder's call. Closing the channel and starting a fresh `sqlite3` is acceptable (a transaction
   in progress is lost, and the user is told so); a pty with Ctrl-C is the alternative.

Values: the builder documents what the chosen output format gives for each storage class and maps it to the
same `Value`s the local driver returns. Any loss versus a local file is listed in the driver's doc comment and
shown to the user (e.g. a BLOB that can't be edited).

### Missing file

Locally a missing file is an error unless `options.create == "true"`. On the host, `sqlite3` would create any
path silently, so a typo on a production server would leave an empty database there. So the driver checks
`test -f '<path>'` first and refuses with "No file <path> on <host>" unless `create` is set. Read-only and
snapshot connections never create.

### Security (live mode)

- **Nothing the user or an agent types reaches `sqlite3` as a dot-command.** Each statement from
  `split_script` is sent only if it doesn't start with `.` (after whitespace); otherwise refuse it with an error.
  Statements are written so a newline inside them can't start a new line with `.` that `sqlite3` would read as a
  command (the builder checks how `sqlite3` reads input mid-statement and tests it, e.g. a string literal
  containing `"\n.shell id"`).
- **`-safe`** when the host's `sqlite3` has it (3.37+): blocks `.shell` and friends, `ATTACH`, `readfile`,
  `writefile`, `load_extension`. On older versions (no `-safe`): refuse statements containing `ATTACH`,
  `load_extension`, `readfile`, `writefile`, `edit(` (word-checked like the existing read checks), and say in the
  connection's status that the host's `sqlite3` is old.
- **Read-only** (`cfg.read_only`, and `St::read_only_driver` for agents' and read access over MCP): start with
  `-readonly`, so the host's SQLite refuses writes whatever the query says.
- The path is always shell-quoted; never interpolated unquoted into a command.

### Status and UI

- The connection shows which mode it's in: live ("on <host>") or snapshot ("read-only copy, <time>: <host> has no
  sqlite3"), with **Refresh** in snapshot mode (in the tree's context menu and wherever the connection's
  status already shows). Exact placement is the builder's call, following how JDBC shows "where it runs".
- Errors name the host ("sqlite3 on db1 said: …").

### Where it's available

Like JDBC over SSH and the other databases with SSH tunnels: it's a database connection with its own SSH login,
not the SSH workbench, so agents (by `McpAccess`) and the phone get it the same way they get those. Snapshot
files stay on this computer and are never offered to the phone as files.

## Left to the builder

- Module layout and how much of `sqlite.rs` gets factored out for sharing.
- The exact output-framing protocol with `sqlite3` and the cancel strategy (within the rules above).
- A size warning before downloading a big snapshot (suggest: ask above ~200 MB).
- Tests: unit tests for quoting, the dot-command guard and the `-json` parsing; an integration test against a
  local `sshd` + `sqlite3` if the test setup has one (see how JDBC over SSH is tested).

## As built (decisions made while building)

- **Live needs `sqlite3` 3.26+**, not just any `sqlite3`: the shared structure queries use
  `pragma_table_xinfo` (3.26). Older → snapshot. `-safe` from 3.37.
- **Output:** `.mode quote` with `.headers on`, `2>&1` so errors land in order with the rows, a random
  marker line (`.print`) after each statement and after a `SELECT changes()`. Parsed values: integers
  exact (past 2^53 as text, like the local driver), BLOBs from `X'…'`, text from `'…'`, `unistr('…')`
  (3.50+) and `replace(…,char(n))` (older).
- **Cut short on the host:** a side-effect-free `SELECT`/`WITH`/`VALUES` runs as
  `SELECT * FROM (…) LIMIT max+1`; one with no rows gets its column names from a `LEFT JOIN` onto a
  one-row select.
- **Checks** (`sqlite_ssh::check_statement`, every statement, every access level): no line starting
  with `.`, no `ATTACH`, `VACUUM INTO`, `readfile`, `writefile`, `edit`, `fsdir`, `zipfile`,
  `load_extension`, … (outside string literals); the statement must be complete per
  `sqlite3_complete`. Parameters go in as literals whose control characters become `char(n)`.
- **Catalog code shared:** `sqlite.rs` now reads the catalog through a small `Rows` trait, implemented
  by the local sqlx connection and by the remote `sqlite3`.
- **Cancel** kills the remote `sqlite3`; the next call starts a new one (and says temp tables and an
  open transaction are gone). A dropped SSH session is logged into again on the next call.
- **Snapshot:** refused above 1 GB (instead of asking); checked with `PRAGMA quick_check`, retried 3
  times; its WAL folded in locally; the tree's `main` shows `host:path (copy, HH:MM)` and the version
  line says it's a read-only copy and why. **Refresh = "Reconnect"** in the connection's menu (shown for
  SQLite over SSH).
- **Tested** with unit tests and `sqlite_ssh::tests::over_ssh` against throwaway sshd containers:
  Alpine (sqlite3 3.45, `-safe`), Debian 10 (3.27, no `-safe`), and Alpine without sqlite3 (snapshot of a
  WAL database).
