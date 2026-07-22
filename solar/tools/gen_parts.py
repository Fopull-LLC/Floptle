#!/usr/bin/env python3
"""Generate the ship builder's low-poly glTF .glb part meshes:
  - fin.glb        : a swept, tapered aero fin (a real fin, not a rectangle)
  - leg_hinge.glb  : the landing-leg mount housing (bolts to the hull, static)
  - leg_upper.glb  : the leg's UPPER strut ("thigh"): a strut + hydraulic piston,
                     designed hanging straight DOWN from a hip pivot at the origin
  - leg_lower.glb  : the leg's LOWER strut + foot ("shin"), hanging down from a
                     knee pivot at the origin

IMPORTANT — the engine's mesh importer RECENTERS every mesh on its own AABB
centre (gltf_import::recenter_and_measure), so a mesh's authored origin is NOT
its runtime pivot. To rotate a segment about a real JOINT we therefore parent
each mesh under an EMPTY pivot node and offset the mesh by its AABB centre (so
the authored origin lands back on the pivot). This script prints each segment's
AABB centre — those are the mesh-node translations used in PartLegs.prefab.ron.

Flat-shaded (per-face normals), single mesh/node, one baseColor material.
No external deps — writes spec-compliant glTF 2.0 binary by hand.
"""
import struct, json, math, os

def rotz(p, a):
    c, s = math.cos(a), math.sin(a)
    return (p[0]*c - p[1]*s, p[0]*s + p[1]*c, p[2])

def add(p, q):
    return (p[0]+q[0], p[1]+q[1], p[2]+q[2])

def _norm3(a, b, c):
    ux = (b[0]-a[0], b[1]-a[1], b[2]-a[2])
    vx = (c[0]-a[0], c[1]-a[1], c[2]-a[2])
    return (ux[1]*vx[2]-ux[2]*vx[1], ux[2]*vx[0]-ux[0]*vx[2], ux[0]*vx[1]-ux[1]*vx[0])

class Mesh:
    def __init__(self):
        self.tris = []  # list of (v0,v1,v2)
    def tri(self, a, b, c):
        self.tris.append((a, b, c))
    def quad(self, a, b, c, d):  # CCW
        self.tri(a, b, c); self.tri(a, c, d)
    def _orient(self, start, ref):
        # Flip any tri (added since `start`) whose normal points INWARD (toward
        # ref = the piece's centre), so every face is outward-facing / CCW.
        for i in range(start, len(self.tris)):
            a, b, c = self.tris[i]
            n = _norm3(a, b, c)
            ctr = ((a[0]+b[0]+c[0])/3, (a[1]+b[1]+c[1])/3, (a[2]+b[2]+c[2])/3)
            out = (ctr[0]-ref[0], ctr[1]-ref[1], ctr[2]-ref[2])
            if n[0]*out[0] + n[1]*out[1] + n[2]*out[2] < 0:
                self.tris[i] = (a, c, b)
    def box(self, center, size, rot=0.0):
        # size = full extents; rot = rotation about Z (radians). CCW-outward.
        start = len(self.tris)
        hx, hy, hz = size[0]/2, size[1]/2, size[2]/2
        cs = [(-hx,-hy,-hz),( hx,-hy,-hz),( hx, hy,-hz),(-hx, hy,-hz),
              (-hx,-hy, hz),( hx,-hy, hz),( hx, hy, hz),(-hx, hy, hz)]
        cs = [add(rotz(c, rot), center) for c in cs]
        self.quad(cs[0], cs[3], cs[2], cs[1])  # -Z
        self.quad(cs[4], cs[5], cs[6], cs[7])  # +Z
        self.quad(cs[0], cs[1], cs[5], cs[4])  # -Y
        self.quad(cs[3], cs[7], cs[6], cs[2])  # +Y
        self.quad(cs[1], cs[2], cs[6], cs[5])  # +X
        self.quad(cs[0], cs[4], cs[7], cs[3])  # -X
        self._orient(start, center)
    def prism(self, outline_xy, thickness):
        # Extrude a polygon (in XY) by +/- thickness/2 along Z.
        start = len(self.tris)
        t = thickness/2
        cx = sum(x for x, _ in outline_xy)/len(outline_xy)
        cy = sum(y for _, y in outline_xy)/len(outline_xy)
        front = [(x, y,  t) for (x, y) in outline_xy]
        back  = [(x, y, -t) for (x, y) in outline_xy]
        n = len(outline_xy)
        for i in range(1, n-1):
            self.tri(front[0], front[i], front[i+1])
            self.tri(back[0], back[i+1], back[i])
        for i in range(n):
            j = (i+1) % n
            self.quad(front[i], back[i], back[j], front[j])
        self._orient(start, (cx, cy, 0.0))
    def aabb_center(self):
        xs = [v[0] for t in self.tris for v in t]
        ys = [v[1] for t in self.tris for v in t]
        zs = [v[2] for t in self.tris for v in t]
        return ((min(xs)+max(xs))/2, (min(ys)+max(ys))/2, (min(zs)+max(zs))/2)

def write_glb(mesh, path, base_color=(0.55,0.57,0.62,1.0)):
    pos, nrm, idx = [], [], []
    for (a, b, c) in mesh.tris:
        ux = (b[0]-a[0], b[1]-a[1], b[2]-a[2])
        vx = (c[0]-a[0], c[1]-a[1], c[2]-a[2])
        nx = (ux[1]*vx[2]-ux[2]*vx[1], ux[2]*vx[0]-ux[0]*vx[2], ux[0]*vx[1]-ux[1]*vx[0])
        L = math.sqrt(nx[0]**2+nx[1]**2+nx[2]**2) or 1.0
        nx = (nx[0]/L, nx[1]/L, nx[2]/L)
        for v in (a, b, c):
            i = len(pos)//3
            pos += [v[0], v[1], v[2]]
            nrm += [nx[0], nx[1], nx[2]]
            idx.append(i)
    pmin = [min(pos[i::3]) for i in range(3)]
    pmax = [max(pos[i::3]) for i in range(3)]
    pos_b = struct.pack('<%df' % len(pos), *pos)
    nrm_b = struct.pack('<%df' % len(nrm), *nrm)
    idx_b = struct.pack('<%dI' % len(idx), *idx)
    def pad4(b): return b + b'\x00' * ((4 - len(b) % 4) % 4)
    pos_b, nrm_b, idx_b = pad4(pos_b), pad4(nrm_b), pad4(idx_b)
    bin_blob = pos_b + nrm_b + idx_b
    off_pos, off_nrm, off_idx = 0, len(pos_b), len(pos_b)+len(nrm_b)
    gltf = {
        "asset": {"version": "2.0", "generator": "floptle gen_parts"},
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": os.path.basename(path)[:-4]}],
        "meshes": [{"primitives": [{
            "attributes": {"POSITION": 0, "NORMAL": 1}, "indices": 2, "material": 0}]}],
        "materials": [{"pbrMetallicRoughness": {
            "baseColorFactor": list(base_color), "metallicFactor": 0.2, "roughnessFactor": 0.7}}],
        "buffers": [{"byteLength": len(bin_blob)}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": off_pos, "byteLength": len(pos_b), "target": 34962},
            {"buffer": 0, "byteOffset": off_nrm, "byteLength": len(nrm_b), "target": 34962},
            {"buffer": 0, "byteOffset": off_idx, "byteLength": len(idx_b), "target": 34963},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": len(pos)//3, "type": "VEC3",
             "min": pmin, "max": pmax},
            {"bufferView": 1, "componentType": 5126, "count": len(nrm)//3, "type": "VEC3"},
            {"bufferView": 2, "componentType": 5125, "count": len(idx), "type": "SCALAR"},
        ],
    }
    json_blob = json.dumps(gltf).encode('utf-8')
    json_blob += b' ' * ((4 - len(json_blob) % 4) % 4)
    total = 12 + 8 + len(json_blob) + 8 + len(bin_blob)
    with open(path, 'wb') as f:
        f.write(b'glTF'); f.write(struct.pack('<II', 2, total))
        f.write(struct.pack('<I', len(json_blob))); f.write(b'JSON'); f.write(json_blob)
        f.write(struct.pack('<I', len(bin_blob))); f.write(b'BIN\x00'); f.write(bin_blob)
    c = mesh.aabb_center()
    print("wrote %-38s (%d tris)  AABB centre = (%.4f, %.4f, %.4f)"
          % (path, len(mesh.tris), c[0], c[1], c[2]))

os.makedirs("solar/models/parts", exist_ok=True)

# ---- FIN: swept/tapered blade, root edge on the hull (x=0), reaching out +X ----
fin = Mesh()
outline = [(0.0, 0.5), (0.52, 0.12), (0.72, -0.52), (0.0, -0.5)]  # swept trapezoid
fin.prism(outline, 0.06)
fin.box((0.03, 0.0, 0.0), (0.1, 0.9, 0.14))  # a root fairing so it's not a card
write_glb(fin, "solar/models/parts/fin.glb", (0.55,0.57,0.62,1.0))

# ---- LEG HINGE: the mount housing (static; bolts flush to the hull side) -------
hinge = Mesh()
hinge.box((0.0, 0.0, 0.0), (0.24, 0.26, 0.34))
hinge.box((0.0, 0.14, 0.0), (0.16, 0.12, 0.22))  # a raised knuckle for the pin
write_glb(hinge, "solar/models/parts/leg_hinge.glb", (0.42,0.45,0.5,1.0))

# ---- LEG UPPER ("thigh"): strut + piston, hanging DOWN from a hip at origin ----
upper = Mesh()
upper.box((0.0,  -0.42, 0.0), (0.15, 0.84, 0.15))   # main strut
upper.box((0.10, -0.34, 0.0), (0.06, 0.64, 0.06))   # hydraulic piston rod
upper.box((0.0,  -0.84, 0.0), (0.18, 0.16, 0.20))   # knee joint block
write_glb(upper, "solar/models/parts/leg_upper.glb", (0.5,0.53,0.58,1.0))

# ---- LEG LOWER ("shin" + foot): hanging DOWN from a knee at origin -------------
lower = Mesh()
lower.box((0.0,  -0.32, 0.0), (0.12, 0.64, 0.12))   # shin
lower.box((0.0,  -0.62, 0.0), (0.13, 0.13, 0.13))   # ankle nub
lower.box((0.12, -0.66, 0.0), (0.54, 0.10, 0.40))   # foot pad (reaches out +X)
write_glb(lower, "solar/models/parts/leg_lower.glb", (0.5,0.53,0.58,1.0))

# The old monolithic leg is replaced by the articulated segments above.
_old = "solar/models/parts/landing_leg.glb"
if os.path.exists(_old):
    os.remove(_old); print("removed %s (superseded by hinge/upper/lower)" % _old)
