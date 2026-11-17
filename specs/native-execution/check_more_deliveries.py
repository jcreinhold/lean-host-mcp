#!/usr/bin/env python3
"""Larger bounded safety checks, still only two accepted stages."""
import json
from pathlib import Path
from check_stages import Policy,explore

results=[]
for controls in (False,True):
    policy=Policy("two extra deliveries "+("including controls" if controls else "GO/START"),
                  controls=controls,max_redeliveries=2)
    result=explore(policy,limit=300000)
    results.append(result)
    print(result["name"],result["status"],result["states"],flush=True)
Path("stage-two-delivery-results.json").write_text(json.dumps(results,indent=2)+"\n")
if any(r["status"]!="bounded safety passed" for r in results):
    raise SystemExit("Incomplete or failed bounded exploration: inspect output")
