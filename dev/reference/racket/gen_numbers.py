"""Random atoms that may or may not be numbers.

Whether an atom is a number or a symbol depends on Racket's long number
syntax. `scrambled` strings together characters and pieces that numbers
are made of. `mutated` builds numbers from their grammar and then often
changes a character or two. Neither makes atoms that start with `#`.
"""

import random


def scrambled(seed, n):
    """Returns the different atoms out of n tries, made from the seed."""
    random.seed(seed)
    alpha = "0123456789+-./#@eEiIfFtTsSlLdDnNaAx"
    pieces = ["inf.0","nan.0","inf.f","inf.t","nan.t","+","-","i","1","12","#",".","/","e5","@","t0","f0","s2","e-3","0"]
    out=set()
    for _ in range(n):
        if random.random()<0.5:
            s="".join(random.choice(alpha) for _ in range(random.randint(1,7)))
        else:
            s="".join(random.choice(pieces) for _ in range(random.randint(1,5)))
        if s.startswith("#") or s==".": continue
        out.add(s)
    return sorted(out)


R=random.random; C=random.choice
def digits(): return "".join(C("0123456789") for _ in range(random.randint(1,3)))
def hashes(): return "#"*random.randint(0,2)
def sign(): return C(["+","-"])
def ureal():
    k=random.randint(0,6)
    if k==0: return digits()
    if k==1: return digits()+"/"+digits()
    if k==2: return digits()+hashes()+C(["",".",])+("#"*random.randint(0,2))
    if k==3: return C(["",digits()])+"."+digits()+hashes()
    if k==4: return digits()+hashes()+"/"+digits()+hashes()
    if k==5: return ureal()+C("eEdDlLfFsStT")+C(["","+","-"])+digits()
    return C(["inf.0","nan.0","inf.f","nan.f","inf.t","nan.t","InF.0","NaN.T"])
def real():
    u=ureal()
    if u[0] in "iInN": return sign()+u
    return C(["",sign()])+u
def num():
    k=random.randint(0,4)
    if k==0: return real()
    if k==1: return C(["",real()])+sign()+C(["",ureal()])+C("iI")
    if k==2: return real()+"@"+real()
    return real()


def mutated(seed, n):
    """Returns the different atoms out of n tries, made from the seed."""
    random.seed(seed)
    out=set()
    alpha="0123456789+-./#@eEiIfFtTnNa"
    for _ in range(n):
        s=num()
        if R()<0.6:
            s=list(s)
            for _ in range(random.randint(1,2)):
                op=random.randint(0,2); p=random.randint(0,len(s))
                if op==0: s.insert(p,C(alpha))
                elif op==1 and p<len(s): del s[p]
                elif p<len(s): s[p]=C(alpha)
            s="".join(s)
        if not s or s.startswith("#") or s==".": continue
        out.add(s)
    return sorted(out)
