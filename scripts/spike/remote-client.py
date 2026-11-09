"""Spike client: drives oleafly-remoted over stdio (or any command, e.g. ssh)."""
import json, os, subprocess, sys, tempfile, threading, queue, time

cmd = sys.argv[1:] or ["target/debug/oleafly-remoted"]
data = os.environ.get("SPIKE_DATA") or tempfile.mkdtemp(prefix="oleafly-spike-")
proj = os.path.join(data, "projects", "spk1")
os.makedirs(proj, exist_ok=True)
env = dict(os.environ, OLEAFLY_DATA_DIR=data)
p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr,
                     text=True, env=env, bufsize=1)
inbox = queue.Queue()
def _read():
    for l in p.stdout:
        try: inbox.put(json.loads(l))
        except ValueError: print("NON-JSON on stdout:", repr(l[:120]))
threading.Thread(target=_read, daemon=True).start()

chan = []          # channel messages
replies = {}
def pump(timeout=0.1):
    try:
        m = inbox.get(timeout=timeout)
    except queue.Empty:
        return False
    if "channel" in m: chan.append(m)
    elif "id" in m: replies[m["id"]] = m
    elif m.get("event") == "acp:event" and m["payload"].get("kind") == "permission" and os.environ.get("SPIKE_APPROVE"):
        e = m["payload"]; req = e["data"]
        opts = req.get("options", [])
        print("  PERMISSION asked:", req.get("title"), "| kind:", req.get("kind"), "| locations:", req.get("locations"))
        print("   options:", [(o["optionId"], o["kind"]) for o in opts])
        pick = next((o for o in opts if o["kind"] == "allow_once"), None) or next((o for o in opts if o["kind"].startswith("allow")), None)
        print("   client answers:", pick and pick["optionId"])
        if pick:
            p.stdin.write(json.dumps({"id": 900 + len(replies), "cmd": "acp_permission", "args": {
                "projectId": e["projectId"], "sessionId": e["sessionId"],
                "permissionId": req["id"], "optionId": pick["optionId"]}, "channels": {}}) + "\n")
    elif m.get("event") == "acp:event":
        e = m["payload"]; print("  acp:event", e.get("kind") or e.get("type"), json.dumps(e)[:(900 if (e.get("kind") or e.get("type")) == "agent_message_chunk" else 160)])
    else: print("event:", m)
    return True

def call(i, name, args=None, channels=None, wait=20):
    p.stdin.write(json.dumps({"id": i, "cmd": name, "args": args or {}, "channels": channels or {}}) + "\n")
    end = time.time() + wait
    while i not in replies and time.time() < end: pump()
    r = replies.get(i, {"err": "TIMEOUT"})
    print(f"[{name}] ->", json.dumps(r)[:300])
    return r

def wait_for(text, wait=15):
    end = time.time() + wait
    while time.time() < end:
        out = "".join(m["message"].get("data", "") for m in chan if isinstance(m["message"], dict))
        if text in out: return out
        pump()
    return None

end = time.time() + 120
ready = None
while time.time() < end and not ready:
    try: m = inbox.get(timeout=1)
    except queue.Empty: continue
    if m.get("ready"): ready = m; print("daemon ready:", m)
assert ready, "daemon never became ready"

if os.environ.get("SPIKE_ATTACH"):
    call(20, "remote_attach", {"projectId": "spk1", "path": os.environ["SPIKE_ATTACH"]})

if os.environ.get("SPIKE_SCENARIO") == "acp":
    if os.environ.get("SPIKE_INSTALL"):
        call(10, "acp_install", {"agentId": os.environ["SPIKE_INSTALL"]}, wait=300)
    cat = call(1, "acp_catalog", {"probe": False})
    for a in (cat.get("ok") or []):
        print("  agent:", a.get("definition", {}).get("id"), "installed =", a.get("installed"))
    agent = os.environ.get("SPIKE_AGENT")
    if agent:
        started = call(2, "acp_start", {"projectId": "spk1", "agentId": agent}, wait=90)
        sid = (started.get("ok") or {}).get("session", {}).get("id")
        print("session started:", bool(sid))
        if sid and os.environ.get("SPIKE_PROMPT"):
            call(3, "acp_prompt", {"projectId": "spk1", "sessionId": sid,
                 "input": {"text": os.environ["SPIKE_PROMPT"], "images": [], "files": [], "selections": []}}, wait=120)
        end = time.time() + 5
        while time.time() < end: pump(0.2)
        if sid: call(4, "acp_disconnect", {"projectId": "spk1", "sessionId": sid})
    p.stdin.close(); p.terminate(); sys.exit(0)

call(1, "backend_protocol_info")
call(2, "term_shells")
opened = call(3, "term_open", {"projectId": "spk1", "cols": 80, "rows": 24}, {"channel": 7})
tid = opened.get("ok", {}).get("id")
if tid:
    call(4, "term_write", {"projectId": "spk1", "id": tid, "data": "echo REMOTE_OK_$((6*7)); pwd\n"})
    out = wait_for("REMOTE_OK_42")
    print("terminal output reached the client:", bool(out))
    print("---- terminal stream (tail) ----\n" + (out or "")[-400:])
    call(5, "term_resize", {"projectId": "spk1", "id": tid, "cols": 100, "rows": 30})
    call(6, "term_kill", {"projectId": "spk1", "id": tid})
    time.sleep(0.5)
    while pump(0.2): pass
    print("exit event seen:", any(isinstance(m["message"], dict) and m["message"].get("event") == "exit" for m in chan))
p.stdin.close(); p.terminate()
