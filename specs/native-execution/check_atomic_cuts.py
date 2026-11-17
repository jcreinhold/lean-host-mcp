#!/usr/bin/env python3
"""Reduced stage-model checks before expanding redelivery interleavings."""
from dataclasses import replace
from pathlib import Path
import json
from check_stages import State,Frame,Policy,explore

go=Frame("go",0,0,0,0,0)
key=(0,0,0)
bound=State(created=1,ready=1,scope_owner=0,bindings=((0,0),None),
            authorization_epochs=(0,-1),issued=frozenset((key,)),idle=False)
cases=[
    ("durable binding, before first GO enqueue",bound),
    ("crash after binding before first GO enqueue",replace(bound,service=False,crashed=True)),
    ("crash after first GO enqueue before receipt",replace(bound,service=False,crashed=True,
        sent=frozenset((key,)),messages=(go,))),
    ("crash after gate consumption before OS spawn",replace(bound,service=False,crashed=True,
        sent=frozenset((key,)),consumed=frozenset((key,)),pending=(go,None))),
]
policy=Policy("reduced atomic cuts",max_redeliveries=0)
out=[]
for name,state in cases:
    # The model explorer's optional initial state keeps this a reduced case;
    # no duplicate network product is needed to expose this crash boundary.
    result=explore(policy,initial=state,limit=50000)
    out.append(dict(result,case=name))
    print(name,result["status"],result["states"],flush=True)
    assert result["status"]=="bounded safety passed"
Path("atomic-cut-results.json").write_text(json.dumps(out,indent=2)+"\n")
