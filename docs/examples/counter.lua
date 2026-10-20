a.fold{
    name = "count",
    initial = 0,
    apply = function(state, change) return state + change end,
}

a.tool{
    name = "count",
    audience = "user",
    description = "Count calls in this session.",
    input = {type = "object", properties = {args = {type = "string"}}},
    run = function(input, cx)
        cx.update("count", 1)
        return {count = cx.state("count")}
    end,
}

a.on("input", function(text)
    if text == "count please" then return {replace = "/count"} end
end)

a.text{name = "counter", order = 50, text = "The user can run /count to count calls in this session."}
