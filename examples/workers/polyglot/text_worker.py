"""Python half of examples/polyglot.orbit: text.normalize."""
import re

from orbit_sdk import Worker

worker = Worker("text-python")


@worker.task("text.normalize", version="1.0.0")
def normalize(input, ctx):
    text = re.sub(r"[^\w\s']", " ", input["text"].lower())
    return {"text": re.sub(r"\s+", " ", text).strip()}


if __name__ == "__main__":
    worker.run()
