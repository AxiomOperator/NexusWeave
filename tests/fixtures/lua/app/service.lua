local repo = require("app.repo")
local util = require("app.util")

local function log(msg)
  print(msg)
end

function handle(id)
  local u = repo.find(util.helper(id))
  repo:save(u)
  log("done")
  return u
end
