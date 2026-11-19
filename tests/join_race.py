#!/usr/bin/env python3
"""Checks what imgbuild's join of per-CPU images relies on: once a manifest PUT returns, a GET of that tag finds it.
Two threads at once each PUT their part tag, then GET the other's, as two CI jobs do; a round where neither finds
the other would leave the image without its multi-platform tag. Fails if any round does.

  REG_USER=… REG_PASS=… join_race.py registry.example.com/test/scratch registry.example.com/test/multiarch:latest [rounds]

The scratch repo gets 2 tags per round (Harbor tags are immutable, so each round uses new ones).
"""
import base64, collections, json, os, sys, threading, time, urllib.error, urllib.request

dst, src, rounds = sys.argv[1], sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 100
host, dst_repo = dst.split("/", 1)
src_repo, src_tag = src.split("/", 1)[1].rsplit(":", 1)
auth = "Basic " + base64.b64encode(f"{os.environ['REG_USER']}:{os.environ['REG_PASS']}".encode()).decode()
LIST = "application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.index.v1+json"


def req(method, path, data=None, headers={}):
    for _ in range(5):
        r = urllib.request.Request(f"https://{host}/v2/{path}", data=data, method=method, headers={"Authorization": auth, **headers})
        try:
            with urllib.request.urlopen(r) as x:
                return x.status, x.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except urllib.error.URLError:
            time.sleep(0.5)
    raise SystemExit(f"network: {method} {path}")


index = json.loads(req("GET", f"{src_repo}/manifests/{src_tag}", headers={"Accept": LIST})[1])
parts = {}
for m in index["manifests"][:2]:
    body = req("GET", f"{src_repo}/manifests/{m['digest']}", headers={"Accept": m["mediaType"]})[1]
    for blob in [json.loads(body)["config"], *json.loads(body)["layers"]]:
        req("POST", f"{dst_repo}/blobs/uploads/?mount={blob['digest']}&from={src_repo}")
    parts[m["platform"]["architecture"]] = (m["mediaType"], body)
a, b = parts
run, joiners = f"race{int(time.time())}", []
for n in range(rounds):
    barrier, seen = threading.Barrier(2), {}

    def job(me, other):
        barrier.wait()
        mt, body = parts[me]
        status, err = req("PUT", f"{dst_repo}/manifests/{run}-{n}-{me}", data=body, headers={"Content-Type": mt})
        assert status == 201, (status, err[:200])
        seen[me] = req("GET", f"{dst_repo}/manifests/{run}-{n}-{other}", headers={"Accept": parts[other][0]})[0] == 200

    threads = [threading.Thread(target=job, args=x) for x in ((a, b), (b, a))]
    [t.start() for t in threads]
    [t.join() for t in threads]
    assert len(seen) == 2, f"round {n}: a job failed"
    joiners.append(sum(seen.values()))
print(f"rounds={rounds} jobs that would join per round: {dict(collections.Counter(joiners))}")
sys.exit(1 if 0 in joiners else 0)
