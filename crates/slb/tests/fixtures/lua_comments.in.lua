-- Loads the configuration and returns it; the caller handles the missing file case on its own.
-- The table below is aligned into columns, so it must be left exactly as it is written here.
local defaults = {
    width = 120,
    fix = false,
}

--[[ A long comment in brackets is not a line comment, so it stays as it is. ]]

-- Long bracket strings with or without equals signs may hold double-dash lines; they are
-- string literals rather than Lua comments and must never be modified.
local template = [[
-- inside a Lua long bracket string; must not be rewritten
-- wrapped line stays verbatim
]]

local nested_template = [=[
-- inside an equals-delimited bracket string; stays verbatim
local raw = [[inner]]
]=]

--[[
-- Closing a multi-line Lua block comment on a line starting with dashes
-- must not turn the code after it into a formatted line comment.
]] local kept = 1; local second = 2

-- Merges the user table into the defaults table and returns a new table; neither
-- argument is mutated, so the caller can safely reuse both after the call returns.
local function merge(user, defaults)
    local result = {}
    for key, value in pairs(defaults) do
        result[key] = value -- start from the default
    end
    for key, value in pairs(user) do
        result[key] = value -- override with the user value
    end
    return result
end

return { defaults = defaults, template = template, nested = nested_template, merge = merge, kept = kept, second = second }
