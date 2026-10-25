#!/usr/bin/env python3
"""Writes crates/core/assets/probe.vc1 to stdout: one flat grey 320x180 VC-1 Advanced Profile I frame,
built here from the bitstream syntax (no third-party content), the hardware decode probe sample.
Usage: tools/gen-vc1-probe-sample.py > crates/core/assets/probe.vc1"""
import sys
W,H=320,180
bits=[]
def put(v,n):
    for i in range(n-1,-1,-1): bits.append((v>>i)&1)
def flush():
    bits.append(1)
    while len(bits)%8: bits.append(0)
    b=bytes(int(''.join(map(str,bits[i:i+8])),2) for i in range(0,len(bits),8)); bits.clear(); return b
out=b''
# sequence header
put(3,2);put(2,3);put(1,2);put(0,3);put(0,5);put(0,1);put(W//2-1,12);put(H//2-1,12)
put(0,1);put(0,1);put(0,1);put(0,1);put(1,1);put(0,1);put(0,1);put(0,1)
out+=b'\0\0\1\x0f'+flush()
# entry point
put(0,1);put(1,1);put(0,1);put(0,1);put(0,1);put(0,1);put(0,1);put(0,2);put(0,1);put(0,1);put(0,2)
put(0,1);put(0,1);put(0,1)
out+=b'\0\0\1\x0e'+flush()
# frame
put(0b110,3);put(0,1);put(10,5)
put(0,1);put(0,4)       # ACPRED bitplane: no invert, raw
put(0,1);put(0,1);put(0,1)  # c_ac, y_ac, dc table
mbs=((W+15)//16)*((H+15)//16)
for _ in range(mbs):
    put(1,1)   # cbp 0
    put(0,1)   # acpred
    for k in range(6):
        put(1,1) if k<4 else put(0,2)  # dc diff 0
out+=b'\0\0\1\x0d'+flush()
sys.stdout.buffer.write(out)
