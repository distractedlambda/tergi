"""Random texts for Racket's plain reader.

They mix data of every kind with comments and atoms that are hard to
tell apart, such as numbers and symbols that look like them.
"""

import random

R=random.random; C=random.choice
SYMS=["abc","a|b c|d","a\\ b","|x|","...","->","a#b","#%app","λ","x1","1+","+","-","a.b","|;|","\\;x","|#|","e","inf","+inf.0x","1/","#%","a\\","||","x|(|y","é\u0301"]
ESC=["\\n","\\x41","\\x4","\\u03bb","\\u3bb","\\U1F600","\\101","\\7","\\\\","\\\"","\\'","\\a","\\e","\\uD83D\\uDE00","\\\n","\\\r\n",";","#|","|","(","x","中"," ","\\q"]
BESC=["\\n","\\x41","\\101","\\\\","\\\"","\\a","\\\n",";","(","x"," "]
CHARS=["#\\a","#\\space","#\\newline","#\\(","#\\;","#\\\"","#\\101","#\\u3BB","#\\U1F600","#\\λ","#\\x","#\\ ","#\\nul","#\\uq","#\\Space","#\\|","#\\#","#\\\\","#\\u","#\\0","#\\1a","#\\tab"]
def num():
    return C(["1","-2","1/2","3.5","+i","1+2i","-inf.0","+nan.f","1#.#","1e10","#x1F","#e1.5","#b101","#i3","1t0","1@2",".5","#x#e1f","#E#X10","1/3+.5i","-.5e-3"])
def atom():
    k=random.randint(0,9)
    if k==0: return num()
    if k==1: return C(SYMS)
    if k==2: return '"'+"".join(C(ESC) for _ in range(random.randint(0,4)))+'"'
    if k==3: return '#"'+"".join(C(BESC) for _ in range(random.randint(0,3)))+'"'
    if k==4: return C(['#rx"a+"','#px#"\\\\d"','#rx#"x"','#px"(a|b)"'])
    if k==5: t=C(["END","x y","E"]); return "#<<"+t+"\n"+C(["","line\n","a\nEND2\n x\n",t+" \n"])+t+"\n"
    if k==6: return C(CHARS)
    if k==7: return C(["#t","#f","#true","#false","#T","#F"])
    if k==8: return C(["#:a","#:|a b|","#:1","#:","#:a\\ b","#:#t"])
    return num()
def comment():
    k=random.randint(0,3)
    if k==0: return ";"+C([""," x","\"","#|","(",";"])+"\n"
    if k==1: return "#|"+C([""," x ","#| y |#","|","#","\n","#||#","\"","(;"])+"|#"
    if k==2: return "#;"+sep()+datum(2)
    return "#!"+C([" x"," a\\\nb","/bin/sh"," \\\\\nq"])+"\n"
def sep():
    s=""
    for _ in range(random.randint(0,2)):
        r=R()
        if r<0.6: s+=C([" ","\n","\t","\r\n"," "])
        elif r<0.85: s+=comment()+C([" ","\n",""])
        else: s+=""
    return s
def datum(d):
    if d<=0 or R()<0.5: return atom()
    k=random.randint(0,9)
    elems=lambda: sep().join(datum(d-1) for _ in range(random.randint(0,4)))
    if k<=2:
        o,c=C(["()","[]","{}"]); return o+sep()+elems()+sep()+c
    if k==3:
        o,c=C(["()","[]","{}"]); return o+datum(d-1)+" "+sep()+"."+" "+datum(d-1)+c
    if k==4: return C(["#(","#2(","#["])+elems()+C([")",")","]"])[0:0]+(")" )
    if k==5: return "#&"+sep()+datum(d-1)
    if k==6: return C(["#hash","#hasheq","#hasheqv","#hashalw","#HASH"])+"(("+num()+" . "+datum(d-1)+")"+sep()+")"
    if k==7: return C(["'","`",",",",@","#'","#`","#,","#,@"])+sep()+datum(d-1)
    if k==8: return C(["#cs","#ci","#CS"])+C([""," "])+datum(d-1)
    return datum(d-1)


def generate(seed, n):
    """Returns n texts made from the seed."""
    random.seed(seed)
    return [sep().join(datum(3) for _ in range(random.randint(1, 4))) for _ in range(n)]
