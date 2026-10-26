import argparse
import json


def load_tasks(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def format_task(task):
    mark = "x" if task.get("done") else " "
    priority = task.get("priority") or "medium"
    due = task.get("due") or "-"
    return f"{task['id']}. [{mark}] {task['title']} (priority: {priority}, due: {due})"


def build_parser():
    parser = argparse.ArgumentParser(prog="todo")
    commands = parser.add_subparsers(dest="command", required=True)
    list_parser = commands.add_parser("list", help="list tasks")
    list_parser.add_argument("--file", required=True, help="path to the tasks JSON file")
    list_parser.add_argument("--all", action="store_true", help="include completed tasks")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    tasks = load_tasks(args.file)
    if not args.all:
        tasks = [task for task in tasks if not task.get("done")]
    tasks.sort(key=lambda task: task["id"])
    if not tasks:
        print("No tasks.")
        return 0
    for task in tasks:
        print(format_task(task))
    return 0
