local M = {}

function M.find(id)
  return { id = id }
end

function M:save(user)
  return true
end

return M
