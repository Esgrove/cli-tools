-- Loads the configuration and returns it. The caller handles the missing file case on its own.
-- The table below is aligned into columns, so it must be left exactly as it is written here.
local defaults = {
    width = 120,
    fix = false,
}

--[[ A long comment in brackets is not a line comment, so it stays as it is. ]]
return defaults

-- Merges the user table into the defaults table and returns a new table.
-- Neither argument is mutated, so the caller can safely reuse both after the call returns.
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
