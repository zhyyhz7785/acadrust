# HY3D enhanced CadDocument — source and compatibility record

Base: official acadrust v0.6.8 commit afffe4697b09f31f24080cc7a2a44928caeb3640. This branch is a product-targeted, bounded fork, separate from the earlier upstream native-edit research branch. Original acadrust MPL-2.0 source and notices remain in place. No file-format codec redesign.

## Shared implementation

`src/document/hy3d_persist.rs` is ported from HY3D `core/src/persist.rs`, commit bb811a0cace2dd1af6ec2b47ae1d19f42c617a12 (https://github.com/zhyyhz7785/hy3d/blob/bb811a0cace2dd1af6ec2b47ae1d19f42c617a12/core/src/persist.rs). The HY3D workspace declares MIT OR Apache-2.0; the port retains that license declaration and source attribution. The original source history identifies its contributors. The following MIT permission notice applies to the imported code:

MIT License

Copyright (c) HY3D contributors (see original source history).

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

The surrounding modified acadrust files remain covered by MPL-2.0. Distributions must preserve the applicable license/notice and provide the required MPL-covered source. HY3D integration should reference this shared implementation rather than retain a divergent copy.

## Shared planar geometry

`src/document/hy3d_geometry.rs` adapts the planar LINE/ARC/CIRCLE declarations from HY3D `core/src/model.rs`, commit 310320e665c89e24b8271b8764758d785247cfa8 (https://github.com/zhyyhz7785/hy3d/blob/310320e665c89e24b8271b8764758d785247cfa8/core/src/model.rs). The imported declarations remain attributed to HY3D contributors under MIT OR Apache-2.0; the MIT permission notice above applies. The surrounding native record operations are part of the enhanced fork. HY3D Geometry uses these payloads directly with its existing Point2 math type; native compare/build/patch share the same methods. This does not yet remove the separate original native record held in the I/O baseline. No change to native EntityType layout, file codec, OCS/WCS convention or persisted format.

## Native storage changes

- CadDocument owns one native ordered entity store, EntitySlots(CowSeq<EntityType>), and a derived CowMap<Handle, usize> index. Leaves contain the original Arc<EntityType>, not Arc<Arc<EntityType>> and not a converted HY3D geometry. Original slot order, swap_remove, handle lookup, shared before/after image API and native membership logic are preserved. No original/private side-channel fields moved or discarded.
- Port keeps existing ordered persistent map/sequence, root identity and fold memo behavior. Serde implementations remain feature-gated. New shared-slot helpers expose native Arc references; first large batch adopts Arcs and builds balanced leaf/branch levels directly, index sorted by handle with last duplicate slot winning, matching existing map insertion semantics. Both entity and index roots clone in O(1); **full CadDocument::clone is not O(1)** because other fields still clone.
- Existing DWG loader EED decode and ownership passes retain parallel processing, borrowing disjoint mutable Arc slots through the original parallel helpers. Existing native writer and reader APIs retained. No re-encoding of source records solely because of tree adoption.
- Trade-off: hash lookup and flat contiguous array become tree traversals; scalar index entries use shared allocations. Full scans, import, editing, writer and memory costs are not measured. This is a concrete implementation candidate, not an assertion that it beats the Vec/HashMap baseline. Do not equate root sharing with complete document snapshot performance.
- HY3D-specific command/history/index integration, native entity authority migration, full raw/opaque preservation during changed-neighbor transactions, and HYD/HYC codec migration are not completed. Earlier acadrust research remains archived, not merged into this release-based fork. No unsafe clearing of raw records or guards.

## Delivery boundary

No compiler or registry is available in this ChatGPT environment; compile, tests, real drawing read/write, performance, external AutoCAD/ODA and product acceptance NOT RUN. The original repo main is not changed. Before adoption verify compile of all actual dependent crates (including serde/default-features=false), preserve existing functionality and verify equivalent data. Do not adopt until defects in the candidate are resolved. Existing HY3D acceptance/testing timing rules apply; no new gates or product tests added. 
