import argparse
import json

PRIORITIES = ("low", "medium", "high")


def load_tasks(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def effective_priority(task):
    return task.get("priority") or "medium"


def format_task(task):
    mark = "x" if task.get("done") else " "
    due = task.get("due") or "-"
    return f"{task['id']}. [{mark}] {task['title']} (priority: {effective_priority(task)}, due: {due})"


def build_parser():
    parser = argparse.ArgumentParser(prog="todo")
    commands = parser.add_subparsers(dest="command", required=True)
    list_parser = commands.add_parser("list", help="list tasks")
    list_parser.add_argument("--file", required=True, help="path to the tasks JSON file")
    list_parser.add_argument("--all", action="store_true", help="include completed tasks")
    list_parser.add_argument("--priority", choices=PRIORITIES, help="only show this priority")
    list_parser.add_argument("--sort", choices=("id", "due"), default="id", help="sort order")
    return parser


def sort_key(order):
    if order == "due":
        return lambda task: (not task.get("due"), task.get("due") or "", task["id"])
    return lambda task: task["id"]


def main(argv=None):
    args = build_parser().parse_args(argv)
    tasks = load_tasks(args.file)
    if not args.all:
        tasks = [task for task in tasks if not task.get("done")]
    if args.priority:
        tasks = [task for task in tasks if effective_priority(task) == args.priority]
    tasks.sort(key=sort_key(args.sort))
    if not tasks:
        print("No tasks.")
        return 0
    for task in tasks:
        print(format_task(task))
    return 0
