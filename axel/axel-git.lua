-- axel-git.lua: give every Axel thread window a rex-git side pane, opened on
-- the thread's workspace. Install it with axel/install.sh, which also starts
-- it at login on macOS, or run it by hand:
--
--   rex do axel-git.lua --all
--
-- A window is handled once: close its git pane and it stays closed.

local HOME = os.getenv("HOME") or ""

-- A program's full path, since a LaunchAgent's PATH is short.
local function find(name)
  for _, dir in ipairs({ HOME .. "/.cargo/bin", HOME .. "/.local/bin", "/opt/homebrew/bin", "/usr/local/bin" }) do
    local f = io.open(dir .. "/" .. name, "r")
    if f then
      f:close()
      return dir .. "/" .. name
    end
  end
  return name
end

local REX_GIT = os.getenv("REX_GIT_BIN") or find("rex-git")
local AXEL = os.getenv("AXEL_BIN") or find("axel")
local SHELL = "com.superlogical.terminal.shell"
local RATIO = 0.72 -- share of the window the conversation keeps

local seen = {}    -- window_id -> true once handled
local pending = {} -- session_id -> debounce timer
local retries = {} -- window_id -> lookups tried before Axel had recorded it

-- window_id -> { dir, thread } for every live Axel session.
local function workspaces()
  local r = rex.exec({ AXEL, "thread", "ls", "--json" }, { timeout = 10 })
  if not r or not r.ok then return {} end
  local by_window = {}
  for _, t in ipairs(rex.json.decode(r.stdout) or {}) do
    local p = t.projects and t.projects[1]
    local dir = p and ((p.workspace and p.workspace.worktree_path) or p.path_hint)
    for _, s in ipairs(t.sessions_live or {}) do
      if dir and s.rex_window_id then
        by_window[s.rex_window_id] = { dir = dir, thread = t.thread and t.thread.id }
      end
    end
  end
  return by_window
end

local function is_repo(dir)
  local r = rex.exec({ "git", "-C", dir, "rev-parse", "--is-inside-work-tree" }, { timeout = 5 })
  return r and r.ok
end

local function root_of(view, window_id)
  for _, w in ipairs(view.windows or {}) do
    if w.window_id == window_id then
      for _, layer in ipairs(w.layers or {}) do
        if layer.kind == "tiled" and layer.layout then
          local node = layer.layout
          return (node.split and node.split.split_id) or node.block_id
        end
      end
    end
  end
end

local scan

local function add_pane(sid, view, wid, ws, left_block)
  local anchor = root_of(view, wid)
  if not anchor then return end
  local res, err = rex.session.new_split{
    session_id = sid,
    anchor_id = anchor,
    direction = "horizontal",
    side = "after",
    ratio = RATIO,
    focus = false,
    layout = { block = {
      flavor = SHELL,
      label = "git",
      options = { command = { REX_GIT, ws.dir, "--thread", ws.thread or "" }, cwd = ws.dir },
    } },
  }
  if err then
    rex.log("warn", "axel-git: " .. tostring(err))
  elseif res and res.block_ids and res.block_ids[1] then
    local git_block = res.block_ids[1]
    rex.session.set_block_label{ session_id = sid, block_id = git_block, label = "git" }
    -- The new pane starts with half the window, and that settles only after
    -- the split returns, so size it a moment later.
    rex.after(0.5, function()
      local _, rerr = rex.session.resize_pane{
        session_id = sid,
        before_block_id = left_block,
        after_block_id = git_block,
        direction = "horizontal",
        ratio = RATIO,
      }
      if rerr then rex.log("warn", "axel-git: resize: " .. tostring(rerr)) end
    end)
  end
end

scan = function(sid)
  local list, err = rex.session.list_blocks{ session_id = sid }
  if not list then
    rex.log("warn", "axel-git: " .. tostring(err))
    return
  end
  -- Axel thread windows are the ones with a "compose" pane.
  local windows = {}
  for _, b in ipairs(list.blocks or {}) do
    if b.window_id and b.placement == "placed" then
      windows[b.window_id] = windows[b.window_id] or {}
      local label = b.label or ""
      -- rex-git retitles its pane "git · <branch>", so match the prefix.
      if label:match("^git") then label = "git" end
      windows[b.window_id][label] = b.block_id
    end
  end
  local todo = {}
  for wid, labels in pairs(windows) do
    if labels.compose and not seen[wid] then
      if labels.git then seen[wid] = true else todo[#todo + 1] = wid end
    end
  end
  if #todo == 0 then return end

  local dirs = workspaces()
  local view = rex.session.view{ session_id = sid }
  if not view then return end
  for _, wid in ipairs(todo) do
    local ws = dirs[wid]
    if ws then
      seen[wid] = true
      if is_repo(ws.dir) then add_pane(sid, view, wid, ws, windows[wid].compose) end
    elseif (retries[wid] or 0) < 5 then
      -- Axel records the window just after it creates it; look again shortly.
      retries[wid] = (retries[wid] or 0) + 1
      rex.after(2, function() scan(sid) end)
    else
      seen[wid] = true
    end
  end
end

local function soon(sid)
  if pending[sid] then pending[sid]:cancel() end
  pending[sid] = rex.after(0.5, function()
    pending[sid] = nil
    scan(sid)
  end)
end

rex.on("session_view_changed", function(ctx) soon(ctx.session_id) end)
rex.on("session_created", function(ctx, ev)
  local sid = ev.session and ev.session.session_id
  if sid then soon(sid) end
end)

-- Windows that already exist when the script starts. Session calls only work
-- once the script is listening, so wait for that.
rex.after(0.2, function()
  local sessions = rex.call("session.list")
  for _, s in ipairs((sessions and sessions.sessions) or {}) do
    scan(s.session_id)
  end
end)
