params = {
    "mode": {"type": "string", "required": True, "doc": "broken fails the second task"},
}

first = command(
    name = "first",
    run = "echo made-by-first > first.out && echo captured > FIRST.txt && echo '{\"ok\": true}'",
    emits_files = ["FIRST.txt"],
)
second = command(
    name = "second",
    run = "sh second.sh",
    depends_on = [first],
)

workflow(type = "playbook", tasks = [first, second], result = second)
