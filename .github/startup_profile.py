"""An Oak folder for timing startup (client.yml's "Time startup"): `count`
conversations of 5 to `turns` rounds each (a reply, a command, its long
output), signed in with a key that is never used for anything.

    python startup_profile.py <folder> <count> <turns>
"""
import json, os, random, sys

folder, count, turns_max = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
conversations = os.path.join(folder, "conversations")
os.makedirs(conversations, exist_ok=True)
with open(os.path.join(folder, "auth.json"), "w") as f:
    f.write('{"type":"api_key","key":"not-a-real-key"}')
random.seed(1)
output = "".join(f"test parser::case_{l} ... ok\n" for l in range(400))
total = 0
for n in range(count):
    messages = []
    for i in range(random.randint(5, turns_max)):
        call = f"call{i}"
        messages += [
            {"role": "user", "content": [{"type": "text", "text": f"Step {i}: look at the parser and fix what fails."}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": f"## Step {i}\n\nThe parser **fails** on `nested` input.\n\n```rust\nfn parse() {{}}\n```"},
                {"type": "tool_call", "id": call, "name": "EXECUTE", "input": {"command": "cargo test", "timeout": 60, "summary": "Run the tests"}}]},
            {"role": "user", "content": [{"type": "tool_result", "call_id": call, "content": output}]},
        ]
    id = str(1_700_000_000_000 + n)
    text = json.dumps({"id": id, "title": f"Conversation {id}", "folder": folder, "model": "m", "effort": "low",
                       "messages": messages, "marks": [], "last_message_at": 0, "next_task_id": 0, "format": 2})
    total += len(text)
    with open(os.path.join(conversations, f"x - Conversation {id} - {id}.json"), "w") as f:
        f.write(text)
print(f"{count} conversations, {total / 1e6:.1f} MB")
