import json, sys

if __name__ == "__main__":
    data = json.load(sys.stdin)
    a = data.get("a", 0)
    b = data.get("b", 0)
    print(a + b)