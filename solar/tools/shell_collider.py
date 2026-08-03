#!/usr/bin/env python3
"""SHELL COLLIDER — turn a building model into a collider you can walk into.

A single box around a building makes it a solid block: the base's sheds looked
like places and behaved like boulders. This reads the .glb, voxelises its
triangles, merges the solid cells into as few axis-aligned boxes as possible,
and prints them as prefab child nodes — so walls are walls, roofs are roofs, and
the openings the model actually has stay open.

    # look first: is it enterable, and where are the doors?
    python3 solar/tools/shell_collider.py models/space-kit/hangar_largeA.glb --check

    # then emit the prefab children (paste under the mesh root)
    python3 solar/tools/shell_collider.py models/space-kit/hangar_largeA.glb --cells 22

Notes that matter if you change this:

* Cells are marked by SAMPLING each triangle's surface. Marking a triangle's
  bounding box instead seals the hole in any wall whose doorway is cut from one
  quad — which closed every door on the first attempt.
* The floor slab is dropped (`--floor-cut`): a threshold at every doorway is
  something a capsule has to climb, and the ground under the building is already
  there to stand on.
* Boxes are emitted in CENTRED MODEL units, because the glTF importer recentres
  a mesh on its AABB and the prefab's own scale then applies to the collider
  boxes exactly as it does to the mesh. Don't pre-multiply the scale in.
* `--check` flood-fills from outside the building at chest height: the number it
  prints is how much of the interior you can actually reach. 100% = a walk-in
  building; 0% = a sealed prop, and no collider tuning will change that (pick a
  different model).
"""

import argparse, json, math, struct, sys
from collections import deque


# ── glTF ────────────────────────────────────────────────────────────────────

CT = {5120: ('b', 1), 5121: ('B', 1), 5122: ('h', 2),
      5123: ('H', 2), 5125: ('I', 4), 5126: ('f', 4)}
NC = {'SCALAR': 1, 'VEC2': 2, 'VEC3': 3, 'VEC4': 4, 'MAT4': 16}


def load(path):
    d = open(path, 'rb').read()
    assert d[:4] == b'glTF', f"{path}: not a binary glTF"
    off, js, bins = 12, None, b''
    while off < len(d):
        ln, ty = struct.unpack_from('<II', d, off)
        chunk = d[off + 8:off + 8 + ln]
        if ty == 0x4E4F534A:
            js = json.loads(chunk)
        elif ty == 0x004E4942:
            bins = chunk
        off += 8 + ln
    return js, bins


def accessor(js, bins, i):
    a = js['accessors'][i]
    bv = js['bufferViews'][a['bufferView']]
    fmt, sz = CT[a['componentType']]
    n = NC[a['type']]
    base = bv.get('byteOffset', 0) + a.get('byteOffset', 0)
    stride = bv.get('byteStride') or sz * n
    return [struct.unpack_from('<' + fmt * n, bins, base + k * stride) for k in range(a['count'])]


def triangles(js, bins):
    """Every triangle in the file, in world (scene) space."""
    tris = []

    def matrix_of(nd):
        if 'matrix' in nd:
            m = nd['matrix']
            return [m[0:4], m[4:8], m[8:12], m[12:16]]
        t = nd.get('translation', [0, 0, 0])
        r = nd.get('rotation', [0, 0, 0, 1])
        s = nd.get('scale', [1, 1, 1])
        x, y, z, w = r
        R = [[1 - 2 * (y * y + z * z), 2 * (x * y + z * w), 2 * (x * z - y * w)],
             [2 * (x * y - z * w), 1 - 2 * (x * x + z * z), 2 * (y * z + x * w)],
             [2 * (x * z + y * w), 2 * (y * z - x * w), 1 - 2 * (x * x + y * y)]]
        M = [[R[i][j] * s[i] for j in range(3)] + [0] for i in range(3)]
        M.append([t[0], t[1], t[2], 1])
        return M

    def mul(a, b):
        return [[sum(a[i][k] * b[k][j] for k in range(4)) for j in range(4)] for i in range(4)]

    def xf(M, p):
        return (p[0] * M[0][0] + p[1] * M[1][0] + p[2] * M[2][0] + M[3][0],
                p[0] * M[0][1] + p[1] * M[1][1] + p[2] * M[2][1] + M[3][1],
                p[0] * M[0][2] + p[1] * M[1][2] + p[2] * M[2][2] + M[3][2])

    def walk(idx, M):
        nd = js['nodes'][idx]
        M2 = mul(matrix_of(nd), M)
        if 'mesh' in nd:
            for pr in js['meshes'][nd['mesh']]['primitives']:
                pos = accessor(js, bins, pr['attributes']['POSITION'])
                idxs = ([i[0] for i in accessor(js, bins, pr['indices'])]
                        if 'indices' in pr else list(range(len(pos))))
                for k in range(0, len(idxs) - 2, 3):
                    tris.append(tuple(xf(M2, pos[idxs[k + j]]) for j in range(3)))
        for c in nd.get('children', []):
            walk(c, M2)

    I = [[1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]]
    scene = js.get('scenes', [{}])[js.get('scene', 0)]
    for r in scene.get('nodes', range(len(js.get('nodes', [])))):
        walk(r, I)
    return tris


# ── voxelise + merge ────────────────────────────────────────────────────────

def shell(path, cells=22, floor_cut=0.15):
    """→ ([(centre, half_extents)], cell_size, model_size), centred model units."""
    js, bins = load(path)
    tris = triangles(js, bins)
    xs = [p[0] for t in tris for p in t]
    ys = [p[1] for t in tris for p in t]
    zs = [p[2] for t in tris for p in t]
    mn = (min(xs), min(ys), min(zs))
    mx = (max(xs), max(ys), max(zs))
    ctr = [(mn[i] + mx[i]) / 2 for i in range(3)]
    size = [mx[i] - mn[i] for i in range(3)]
    cs = max(size) / cells
    N = [max(1, int(math.ceil(size[i] / cs))) for i in range(3)]
    occ = [[[False] * N[2] for _ in range(N[1])] for _ in range(N[0])]

    for t in tris:
        e1 = [t[1][i] - t[0][i] for i in range(3)]
        e2 = [t[2][i] - t[0][i] for i in range(3)]
        n1 = max(1, int(math.sqrt(sum(q * q for q in e1)) / (cs * 0.4)) + 1)
        n2 = max(1, int(math.sqrt(sum(q * q for q in e2)) / (cs * 0.4)) + 1)
        for a in range(n1 + 1):
            for b in range(n2 + 1):
                u, v = a / n1, b / n2
                if u + v > 1.0:
                    continue
                pt = [t[0][i] + e1[i] * u + e2[i] * v for i in range(3)]
                idx = [max(0, min(N[i] - 1, int((pt[i] - mn[i]) / cs))) for i in range(3)]
                occ[idx[0]][idx[1]][idx[2]] = True

    used = [[[False] * N[2] for _ in range(N[1])] for _ in range(N[0])]
    out = []
    for i in range(N[0]):
        for j in range(N[1]):
            for k in range(N[2]):
                if not occ[i][j][k] or used[i][j][k]:
                    continue
                i2 = i
                while i2 + 1 < N[0] and occ[i2 + 1][j][k] and not used[i2 + 1][j][k]:
                    i2 += 1
                k2 = k
                while k2 + 1 < N[2] and all(occ[ii][j][k2 + 1] and not used[ii][j][k2 + 1]
                                            for ii in range(i, i2 + 1)):
                    k2 += 1
                j2 = j
                while j2 + 1 < N[1] and all(occ[ii][j2 + 1][kk] and not used[ii][j2 + 1][kk]
                                            for ii in range(i, i2 + 1) for kk in range(k, k2 + 1)):
                    j2 += 1
                for ii in range(i, i2 + 1):
                    for jj in range(j, j2 + 1):
                        for kk in range(k, k2 + 1):
                            used[ii][jj][kk] = True
                lo = [mn[q] + (i, j, k)[q] * cs for q in range(3)]
                hi = [mn[q] + ((i2, j2, k2)[q] + 1) * cs for q in range(3)]
                c = [(lo[q] + hi[q]) / 2 - ctr[q] for q in range(3)]
                h = [(hi[q] - lo[q]) / 2 for q in range(3)]
                if c[1] + h[1] <= -size[1] / 2 + floor_cut:
                    continue          # floor slab — see the module docstring
                out.append((c, h))
    return out, cs, size


# ── the walk-in check ───────────────────────────────────────────────────────

def check(boxes, size, scale, world_h=1.2, N=48, pad=6, draw=True):
    """Flood-fill from outside at `world_h` metres up. → fraction reachable."""
    y = -size[1] / 2 + world_h / scale
    W = N + 2 * pad
    g = [['.'] * W for _ in range(W)]
    for gz in range(W):
        for gx in range(W):
            x = -size[0] / 2 + size[0] * (gx - pad + 0.5) / N
            z = -size[2] / 2 + size[2] * (gz - pad + 0.5) / N
            for (cx, cy, cz), (hx, hy, hz) in boxes:
                if cy - hy <= y <= cy + hy and cx - hx <= x <= cx + hx and cz - hz <= z <= cz + hz:
                    g[gz][gx] = '#'
                    break
    q, seen = deque([(0, 0)]), {(0, 0)}
    while q:
        gx, gz = q.popleft()
        for dx, dz in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            nx, nz = gx + dx, gz + dz
            if 0 <= nx < W and 0 <= nz < W and (nx, nz) not in seen and g[nz][nx] != '#':
                seen.add((nx, nz))
                q.append((nx, nz))
    tot = rch = 0
    for gz in range(pad, pad + N):
        for gx in range(pad, pad + N):
            if g[gz][gx] == '#':
                continue
            tot += 1
            if (gx, gz) in seen:
                rch += 1
                g[gz][gx] = '+'
            else:
                g[gz][gx] = 'x'
    if draw:
        print(f"  plan at {world_h} m — '+' you can walk to, 'x' sealed off, '#' solid")
        for row in g:
            print("    " + "".join(row))
    return rch / max(1, tot)


# ── prefab output ───────────────────────────────────────────────────────────

def emit(boxes):
    for n, (c, h) in enumerate(boxes):
        print(f'''    (
        name: "Shell {n + 1}",
        transform: (
            translation: ({c[0]:.3f}, {c[1]:.3f}, {c[2]:.3f}),
            rotation: (0.0, 0.0, 0.0, 1.0),
            scale: (1.0, 1.0, 1.0),
        ),
        matter: Empty,
        scripts: [],
        parent: Some(0),
        rigidbody: Some((
            boxed: true,
            mode: Static,
            radius: 0.5,
            height: 1.0,
            half_extents: ({h[0]:.3f}, {h[1]:.3f}, {h[2]:.3f}),
            restitution: 0.0,
            friction: 0.9,
            gravity: false,
            lock_pos: (false, false, false),
            lock_rot: (false, false, false),
        )),
    ),''')


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("model")
    ap.add_argument("--cells", type=int, default=22,
                    help="voxel resolution across the model's longest axis (default 22)")
    ap.add_argument("--scale", type=float, default=1.0,
                    help="the prefab's node scale — only used to report world sizes / check heights")
    ap.add_argument("--floor-cut", type=float, default=0.15,
                    help="drop boxes whose top is below this many MODEL units above the base")
    ap.add_argument("--check", action="store_true",
                    help="print the walk-in report instead of the prefab nodes")
    a = ap.parse_args()

    boxes, cs, size = shell(a.model, a.cells, a.floor_cut)
    if a.check:
        print(f"{a.model}: {len(boxes)} boxes, cell {cs:.3f} model units")
        print(f"  model {size[0]:.2f} x {size[1]:.2f} x {size[2]:.2f}"
              f"  →  world {size[0]*a.scale:.1f} x {size[1]*a.scale:.1f} x {size[2]*a.scale:.1f}"
              f"   (seat {size[1]/2*a.scale:.3f})")
        for h in (0.4, 1.2, 1.9):
            f = check(boxes, size, a.scale, h, draw=(h == 1.2))
            print(f"  reachable from outside at {h} m: {f*100:.1f}%")
        return
    print(f"// {a.model}: {len(boxes)} shell boxes, voxel cell {cs:.3f} model units")
    emit(boxes)


if __name__ == "__main__":
    main()
